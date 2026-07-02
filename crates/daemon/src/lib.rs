//! plexy-glass daemon.

use std::fs::OpenOptions;
use std::process;
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;

/// Crate-wide serialization for tests that mutate process-global env vars
/// (notably `XDG_STATE_HOME` for the persist/session/registry suites).
///
/// All such tests must lock this single mutex. Per-module locks would not
/// serialize across modules, so concurrent tests could clobber each other's
/// `XDG_STATE_HOME` and read the wrong session directory.
#[cfg(test)]
pub(crate) static STATE_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Per-test `XDG_STATE_HOME` isolation.
///
/// Any test that constructs a `Session`, a `SessionRegistry`, or otherwise
/// reaches the persist layer must take `let _g = crate::test_env::isolate();`
/// as its first line and hold the guard for the test's full duration,
/// otherwise the debounced persist loop (or an `attach_or_create` restore)
/// reads/writes the user's *real* state dir.
#[cfg(test)]
pub(crate) mod test_env {
    use std::env;
    use std::ffi::OsString;
    use std::sync::{MutexGuard, PoisonError};
    use std::time::{Duration, Instant};
    use tokio::time;

    /// Holds the crate-wide env lock, points `XDG_STATE_HOME` at a fresh
    /// tempdir, pins `SHELL` to `/bin/sh`, and restores the previous values
    /// on drop.
    ///
    /// `SHELL` is pinned because everything a guarded test spawns through
    /// `declared::default_shell()` (splits / new windows via `default_spec`,
    /// popups, and declared-template panes without an explicit `command`)
    /// must not depend on the developer's interactive shell existing or
    /// behaving. An interactive login shell sources rc files and its line
    /// editor decides whether ^G even beeps; `/bin/sh` is POSIX-guaranteed
    /// and cheap. Tests that build a `WindowManager` directly (no
    /// Session/registry, no guard) pin via
    /// `WindowManager::set_default_program` instead.
    pub struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        old_xdg: Option<OsString>,
        old_shell: Option<OsString>,
        _tmp: tempfile::TempDir,
    }

    pub fn isolate() -> EnvGuard {
        // A poisoned lock is safe to reuse: the panicking test's guard already
        // restored the env vars during unwind.
        let lock = super::STATE_ENV_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let tmp = tempfile::tempdir().expect("tempdir");
        let old_xdg = env::var_os("XDG_STATE_HOME");
        let old_shell = env::var_os("SHELL");
        // SAFETY: env mutation is guarded by `STATE_ENV_LOCK`, held for the
        // lifetime of the guard.
        unsafe {
            env::set_var("XDG_STATE_HOME", tmp.path());
            env::set_var("SHELL", "/bin/sh");
            // Suppress the one-time welcome modal so it can't intercept
            // overlay/screen assertions in attach-based tests (every test wants
            // it off; the var is test-only and never set in production).
            env::set_var("PLEXY_GLASS_NO_WELCOME", "1");
        }
        EnvGuard { _lock: lock, old_xdg, old_shell, _tmp: tmp }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: `STATE_ENV_LOCK` is held for `self`'s lifetime.
            unsafe {
                match &self.old_xdg {
                    Some(v) => env::set_var("XDG_STATE_HOME", v),
                    None => env::remove_var("XDG_STATE_HOME"),
                }
                match &self.old_shell {
                    Some(v) => env::set_var("SHELL", v),
                    None => env::remove_var("SHELL"),
                }
            }
        }
    }

    /// Poll `cond` every 50 ms until it returns `true` or `deadline` elapses.
    ///
    /// Returns whether the condition was met. Use this to wait out the persist
    /// debounce (and similar async side-effects) without a fixed sleep; tests
    /// exit early on success so the suite is faster than any fixed-sleep bound.
    ///
    /// Note that for *negative* assertions ("X did NOT happen") a poll cannot
    /// prove absence, so keep a short fixed sleep there and mark it with a
    /// comment.
    pub async fn poll_until(
        deadline: Duration,
        mut cond: impl FnMut() -> bool,
    ) -> bool {
        let start = Instant::now();
        loop {
            if cond() {
                return true;
            }
            if start.elapsed() >= deadline {
                return false;
            }
            time::sleep(Duration::from_millis(50)).await;
        }
    }
}

pub mod args;
pub mod connection;
pub mod declared;
pub mod error;
pub mod input_router;
pub mod listener;
mod lock;
pub mod osc_actions;
pub mod pane;
pub mod paste_buffers;
pub mod paths;
pub mod pipe;
pub mod popup;
mod panic_hook;
pub mod registry;
pub mod renderer;
pub mod session;
pub mod window;
pub mod window_manager;

pub use args::DaemonArgs;
pub use error::DaemonError;
pub use input_router::{InputEvent, InputRouter};
pub(crate) use lock::LockExt;
pub use pane::Pane;
pub use paths::RuntimePaths;
pub use registry::SessionRegistry;
pub use session::Session;

use tracing::{error, info};

pub async fn run(args: DaemonArgs) -> Result<(), DaemonError> {
    let paths = RuntimePaths::for_current_user()?;
    paths.create_dirs()?;

    // Logs are already initialized by the top-level binary when foregrounded.
    if !args.foreground {
        use tracing_subscriber::Layer;
        use tracing_subscriber::fmt;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&paths.log_file)?;
        let layer = fmt::layer()
            .with_writer(file)
            .with_ansi(false)
            .with_target(true)
            .with_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            );
        // Best-effort: if a global subscriber is already set (e.g., the top-level
        // binary in tests), keep using it.
        let _ = tracing_subscriber::registry().with(layer).try_init();
    }

    // Bridge panics into the tracing log (daemon stderr is /dev/null).
    panic_hook::install_panic_logging();

    let (config, cfg_err) = plexy_glass_config::load_or_default();
    if let Some(e) = &cfg_err {
        tracing::warn!(error = %e, "config load error; using built-in default");
    }
    let config = Arc::new(config);

    let listener = listener::Listener::bind(paths)?;
    let daemon_pid = process::id();
    let registry = Arc::new(SessionRegistry::new());
    // Surface a boot config error on the next attach (it would otherwise only
    // reach the log). Cleared by the first clean reload.
    registry.set_config_error(cfg_err.as_ref().map(ToString::to_string));

    // Build config-declared default sessions eagerly (Feature B). A failure to
    // build one is logged and skipped, so it never blocks the accept loop. The
    // 24×80 default size is resized when a client attaches, and the reload
    // path (`reload_config`) reuses the same `build_declared` for
    // newly-declared names.
    {
        let boot_size = plexy_glass_protocol::PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
        registry.build_declared(&config, boot_size).await;
    }

    info!(foreground = args.foreground, "daemon ready, entering accept loop");
    loop {
        let (stream, _addr) = match listener.socket.accept().await {
            Ok(p) => p,
            Err(e) => {
                error!(error = %e, "accept failed");
                continue;
            }
        };
        let registry = Arc::clone(&registry);
        let config = Arc::clone(&config);
        tokio::spawn(async move {
            if let Err(e) = connection::serve(stream, daemon_pid, registry, config).await {
                error!(error = %e, "connection ended with error");
            }
        });
    }
}
