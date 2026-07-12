//! Daemon-wide registry of named sessions.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use plexy_glass_mux::BufferEntry;
use plexy_glass_protocol::{ProtocolError, PtySize, SessionEntry, SessionName, SpawnSpec};
use tokio::sync::Mutex;
use tokio::task;

use crate::LockExt;
use crate::error::DaemonError;
use crate::paste_buffers::PasteBufferStore;
use crate::session::Session;

/// Maximum retained paste buffers (tmux-style; oldest evicted past this).
const PASTE_BUFFER_CAP: usize = 50;

pub struct SessionRegistry {
    /// Live sessions keyed by their validated [`SessionName`]. Every key got
    /// there through `SessionName::parse` at a construction method below (the one
    /// validation boundary), so an invalid name is unrepresentable in the map.
    /// Read paths (`get`/`kill`) still take `&str` and look up via `Borrow<str>`
    /// — an unparseable name just isn't found, exactly the old behavior.
    inner: Mutex<HashMap<SessionName, Arc<Session>>>,
    /// Daemon-global paste buffers.
    ///
    /// Independent of `inner` and never locked while `inner` is held (the
    /// delegates touch only this lock).
    paste_buffers: Mutex<PasteBufferStore>,
    /// Set when the config failed to load (boot or reload) and the daemon is
    /// running on built-in defaults. Surfaced on the next attach so the failure
    /// isn't invisible; cleared by a clean reload. A plain sync mutex, since
    /// accesses are brief and await-free.
    config_error: StdMutex<Option<String>>,
    /// True until the one-time welcome modal has been shown once this daemon
    /// lifetime. Gated additionally by `config.welcome` (the user's on/off knob);
    /// in-memory only, so a fresh daemon shows it once again. No on-disk marker.
    welcome_pending: AtomicBool,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            paste_buffers: Mutex::new(PasteBufferStore::new(PASTE_BUFFER_CAP)),
            config_error: StdMutex::new(None),
            welcome_pending: AtomicBool::new(true),
        }
    }

    /// Consume the one-time welcome slot: returns `true` exactly once per
    /// daemon lifetime (the first caller), `false` thereafter.
    ///
    /// The attach path gates this behind `config.welcome` so a disabled
    /// welcome never consumes it.
    pub fn take_welcome(&self) -> bool {
        self.welcome_pending.swap(false, Ordering::Relaxed)
    }

    /// Record (or clear) the config-load error state. Recovers a poisoned lock
    /// rather than panicking on the attach path: the protected `Option` is
    /// structurally valid after any panic, so the write still lands.
    pub fn set_config_error(&self, err: Option<String>) {
        *self.config_error.lock_recover() = err;
    }

    /// Whether the running config is the fallback default because a load failed
    /// (boot or last reload). Drives the attach-time "config error" notice.
    pub fn has_config_error(&self) -> bool {
        self.config_error.lock_recover().is_some()
    }

    /// Push a new newest paste buffer (copy-mode yank).
    pub async fn push_paste_buffer(&self, content: Vec<u8>) {
        self.paste_buffers.lock().await.push(content);
    }

    /// Clone out the most-recent buffer's content, if any.
    pub async fn paste_buffer_top(&self) -> Option<Vec<u8>> {
        self.paste_buffers
            .lock()
            .await
            .top()
            .map(|b| b.content.clone())
    }

    /// Clone out a named buffer's content, if present.
    pub async fn paste_buffer_get(&self, name: &str) -> Option<Vec<u8>> {
        self.paste_buffers
            .lock()
            .await
            .get(name)
            .map(|b| b.content.clone())
    }

    /// Clone out the most-recent buffer's `(name, content)`, `save-buffer`'s
    /// default source (its status text names the buffer it wrote).
    pub async fn paste_buffer_top_entry(&self) -> Option<(String, Vec<u8>)> {
        self.paste_buffers
            .lock()
            .await
            .top()
            .map(|b| (b.name.clone(), b.content.clone()))
    }

    /// Delete a named buffer; returns whether one was removed.
    pub async fn delete_paste_buffer(&self, name: &str) -> bool {
        self.paste_buffers.lock().await.delete(name)
    }

    /// Newest-first `(name, preview)` rows for the choose-buffer overlay.
    pub async fn list_paste_buffers(&self) -> Vec<BufferEntry> {
        self.paste_buffers.lock().await.entries()
    }

    pub async fn list(&self) -> Vec<SessionEntry> {
        let mut map = self.inner.lock().await;
        // Lazily prune sessions that have already closed.
        map.retain(|_, s| !s.closing.load(Ordering::SeqCst));
        // `list_entry` takes blocking locks, so defer via `block_in_place`. No
        // per-session `Arc::clone` needed, `block_in_place` runs inline and
        // `list_entry` only needs a shared borrow.
        let mut out: Vec<SessionEntry> = map
            .values()
            .map(|s| task::block_in_place(|| s.list_entry()))
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub async fn get(&self, name: &str) -> Option<Arc<Session>> {
        let mut map = self.inner.lock().await;
        if let Some(s) = map.get(name) {
            if s.closing.load(Ordering::SeqCst) {
                map.remove(name);
                return None;
            }
            return Some(Arc::clone(s));
        }
        None
    }

    pub async fn create(
        &self,
        name: String,
        cmd: SpawnSpec,
        size: PtySize,
        config: Arc<plexy_glass_config::Config>,
    ) -> Result<Arc<Session>, DaemonError> {
        // Parse the wire `String` into a `SessionName` here — the boundary. A bad
        // name returns `DaemonError::Protocol(EmptyName | InvalidName)`, which the
        // caller replies as a graceful `ServerMsg::Error`.
        let key = SessionName::parse(&name)?;
        let mut map = self.inner.lock().await;
        if map.contains_key(&key) {
            return Err(DaemonError::Protocol(ProtocolError::SessionAlreadyExists {
                name,
            }));
        }
        let session = Session::new(name, cmd, size, config)?;
        map.insert(key, Arc::clone(&session));
        Ok(session)
    }

    /// Build a config-declared session from its template and register it.
    ///
    /// Mirrors `create`'s locking (holds `inner` across construction; the
    /// session's own `window_manager` is a different mutex, so no deadlock). If
    /// the name is already live (a concurrent attach), returns the existing one.
    pub async fn create_declared(
        &self,
        template: &plexy_glass_config::SessionTemplate,
        config: Arc<plexy_glass_config::Config>,
        size: PtySize,
    ) -> Result<Arc<Session>, DaemonError> {
        let key = SessionName::parse(&template.name)?;
        let mut map = self.inner.lock().await;
        if let Some(s) = map.get(&key)
            && !s.closing.load(Ordering::SeqCst)
        {
            return Ok(Arc::clone(s));
        }
        let session = Session::build_from_template(template, size, config).await?;
        map.insert(key, Arc::clone(&session));
        Ok(session)
    }

    /// Build every config-declared session that is NOT already live, from its
    /// template (Feature B / declarative v2).
    ///
    /// Idempotent: a name already live is skipped by `create_declared`'s
    /// short-circuit, so this never rebuilds a running session and never races
    /// a concurrent attach of the same name. A per-session build failure is
    /// logged and skipped, it never aborts the rest. Shared by daemon boot and
    /// `reload_config`; both pass the 24×80 default `size` (resized when a
    /// client later attaches).
    pub async fn build_declared(&self, config: &Arc<plexy_glass_config::Config>, size: PtySize) {
        for template in &config.sessions {
            match self
                .create_declared(template, Arc::clone(config), size)
                .await
            {
                Ok(_) => tracing::info!(session = %template.name, "built declared session"),
                Err(e) => {
                    tracing::warn!(session = %template.name, error = %e, "skipping declared session");
                }
            }
        }
    }

    /// Attach to an existing in-memory session if one is live, else build a
    /// declared-template session, else `create` a fresh one.
    ///
    /// Memory-only: there is no on-disk restore.
    pub async fn attach_or_create(
        &self,
        name: String,
        cmd: SpawnSpec,
        size: PtySize,
        config: Arc<plexy_glass_config::Config>,
    ) -> Result<Arc<Session>, DaemonError> {
        // Parse at the boundary; `key` is used for the map ops, `name` (the raw
        // `String`) for the declared-template lookup and the fresh-create
        // fallback (which re-parses — cheap and keeps `create` the sole insert
        // path). A bad name returns a graceful `DaemonError::Protocol` here.
        let key = SessionName::parse(&name)?;
        // Fast path: already running. A still-mapped but *closing* entry must be
        // PRUNED here (mirroring `get`), not merely skipped, otherwise it
        // lingers in the map and the fresh-create fallback below is rejected by
        // `create`'s contains_key check (SessionAlreadyExists). This is the
        // default `plexy-glass` (no args) re-launch after exiting the sole
        // "main" session: it calls attach_or_create directly with no preceding
        // `get` to do the pruning.
        {
            let mut map = self.inner.lock().await;
            let closing = match map.get(&key) {
                Some(s) => {
                    if !s.closing.load(Ordering::SeqCst) {
                        return Ok(Arc::clone(s));
                    }
                    true
                }
                None => false,
            };
            if closing {
                map.remove(&key);
            }
        }
        // A declared session name is (re)built from its template. (The client
        // `cmd` is intentionally unused, declared panes come from the template +
        // the daemon default shell.)
        if let Some(template) = config.sessions.iter().find(|t| t.name == name) {
            return self
                .create_declared(template, Arc::clone(&config), size)
                .await;
        }
        // The daemon is memory-only, there is no on-disk restore, so build fresh.
        self.create(name, cmd, size, config).await
    }

    /// Rename a live session: re-key the map and update the session's live name
    /// under ONE map-lock hold (so a concurrent `get`/`create` can never observe
    /// the key and the live name disagreeing). Memory-only, nothing on disk.
    pub async fn rename_session(self: &Arc<Self>, old: &str, new: &str) -> Result<(), DaemonError> {
        // The new name is parsed at the boundary; `old` is a lookup key (`&str`
        // via `Borrow`), never validated — an unknown/invalid `old` is
        // `SessionNotFound`, as before.
        let new_key = SessionName::parse(new)?;
        let mut map = self.inner.lock().await;
        if map.contains_key(&new_key) {
            return Err(DaemonError::Protocol(ProtocolError::SessionAlreadyExists {
                name: new.to_string(),
            }));
        }
        let session = map.remove(old).ok_or_else(|| {
            DaemonError::Protocol(ProtocolError::SessionNotFound {
                name: old.to_string(),
            })
        })?;
        // Key and live name move together, under the same lock hold.
        session.set_name(new.to_string());
        map.insert(new_key, session);
        Ok(())
    }

    pub async fn kill(&self, name: &str) -> Result<(), DaemonError> {
        let session = {
            let mut map = self.inner.lock().await;
            map.remove(name).ok_or_else(|| {
                DaemonError::Protocol(ProtocolError::SessionNotFound {
                    name: name.to_string(),
                })
            })?
        };
        // Set closing + abort the Arc-pinning tasks (death/tick), signal the
        // coordinator to emit a final frame and exit (tearing down attached
        // clients), then terminate pane children, since dropping panes alone
        // does not SIGHUP them (the reader thread holds the PTY master open).
        session.begin_close();
        session.terminate_panes().await;
        Ok(())
    }

    /// Re-read config from disk and apply to every session.
    ///
    /// The KDL loader (`plexy_glass_config::load_or_default`) returns
    /// `(Config, Option<ConfigError>)`: on a parse/IO error the `Config` half is
    /// the built-in default, which we must NOT apply (see [`Self::apply_reload`]
    /// for the last-known-good retention).
    pub async fn reload_config(&self) -> Result<(), DaemonError> {
        let (new_config, err) = plexy_glass_config::load_or_default();
        self.apply_reload(new_config, err).await
    }

    /// Apply a freshly-loaded config to every live session, but only if it
    /// loaded cleanly.
    ///
    /// On `Some(err)` the `new_config` is the built-in default (the loader's
    /// error fallback), so applying it would silently wipe the running custom
    /// palette/status/keymap. Instead we KEEP the last-known-good config (it
    /// already lives in each session's own config slot, so "retaining" it is
    /// simply not touching it), surface the error on the next attach, and
    /// return `Err`. Only a clean reload swaps and clears the error.
    ///
    /// Never panics mid-reload; each `Session::swap_config` is independent.
    /// Split out so the error-vs-clean branching is unit-testable without
    /// depending on the platform config path.
    async fn apply_reload(
        &self,
        new_config: plexy_glass_config::Config,
        err: Option<plexy_glass_config::ConfigError>,
    ) -> Result<(), DaemonError> {
        if let Some(e) = err {
            tracing::warn!(error = %e, "reload: parse error; keeping last-known-good config");
            self.set_config_error(Some(e.to_string()));
            return Err(DaemonError::from(e));
        }
        let new_config = Arc::new(new_config);
        {
            let map = self.inner.lock().await;
            for session in map.values() {
                session.swap_config(Arc::clone(&new_config)).await;
            }
        }
        // Re-read templates: build any NEWLY-declared session names not yet
        // live, so `:switch new` / `attach -n new` find them (matching boot).
        // Live sessions are NOT rebuilt (`build_declared` skips live names),
        // so a changed template for a running session is deferred to its next
        // build (after kill + reattach), and a removed-but-live name is left
        // alone. The reload build has no client size, so it uses the same
        // 24×80 default as boot (resized on the first attach).
        let reload_size = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };
        self.build_declared(&new_config, reload_size).await;
        // A clean reload clears the error state.
        self.set_config_error(None);
        Ok(())
    }
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::ptr;
    use std::time::Duration;

    use super::*;
    use crate::test_env;

    fn spec() -> SpawnSpec {
        SpawnSpec {
            program: "/bin/sh".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        }
    }

    fn size() -> PtySize {
        PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    fn cfg() -> Arc<plexy_glass_config::Config> {
        Arc::new(plexy_glass_config::built_in_default())
    }

    #[tokio::test]
    async fn create_then_get() {
        let _g = test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        let s = r
            .create("main".into(), spec(), size(), cfg())
            .await
            .unwrap();
        assert_eq!(s.name(), "main");
        let got = r.get("main").await.unwrap();
        assert_eq!(got.name(), "main");
    }

    #[test]
    fn config_error_flag_set_and_cleared() {
        let r = SessionRegistry::new();
        assert!(!r.has_config_error(), "fresh registry has no config error");
        r.set_config_error(Some("line 7:3: boom".into()));
        assert!(r.has_config_error());
        r.set_config_error(None);
        assert!(!r.has_config_error(), "a clean reload clears it");
    }

    #[test]
    fn take_welcome_is_true_exactly_once_per_daemon() {
        let r = SessionRegistry::new();
        assert!(
            r.take_welcome(),
            "first attach to a fresh daemon shows the welcome"
        );
        assert!(!r.take_welcome(), "shown once — subsequent attaches do not");
        assert!(!r.take_welcome(), "stays false for this daemon lifetime");
    }

    #[tokio::test]
    async fn duplicate_create_fails() {
        let _g = test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        r.create("main".into(), spec(), size(), cfg())
            .await
            .unwrap();
        let err = r
            .create("main".into(), spec(), size(), cfg())
            .await
            .map(|_| ())
            .unwrap_err();
        assert!(matches!(
            err,
            DaemonError::Protocol(ProtocolError::SessionAlreadyExists { .. })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_returns_sorted_entries() {
        let _g = test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        r.create("zeta".into(), spec(), size(), cfg())
            .await
            .unwrap();
        r.create("alpha".into(), spec(), size(), cfg())
            .await
            .unwrap();
        let entries = r.list().await;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "alpha");
        assert_eq!(entries[1].name, "zeta");
    }

    #[tokio::test]
    async fn kill_unknown_returns_session_not_found() {
        let _g = test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        let err = r.kill("ghost").await.unwrap_err();
        assert!(matches!(
            err,
            DaemonError::Protocol(ProtocolError::SessionNotFound { .. })
        ));
    }

    #[tokio::test]
    async fn name_validation_rejects_empty() {
        let _g = test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        let err = r
            .create(String::new(), spec(), size(), cfg())
            .await
            .map(|_| ())
            .unwrap_err();
        assert!(matches!(
            err,
            DaemonError::Protocol(ProtocolError::EmptyName)
        ));
    }

    #[tokio::test]
    async fn name_validation_rejects_invalid_chars() {
        let _g = test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        let err = r
            .create("has space".into(), spec(), size(), cfg())
            .await
            .map(|_| ())
            .unwrap_err();
        assert!(matches!(
            err,
            DaemonError::Protocol(ProtocolError::InvalidName { .. })
        ));
    }

    #[tokio::test]
    async fn closing_sessions_are_pruned_on_get() {
        let _g = test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        let s = r
            .create("dead".into(), spec(), size(), cfg())
            .await
            .unwrap();
        s.closing.store(true, Ordering::SeqCst);
        let got = r.get("dead").await;
        assert!(got.is_none(), "closing session should be pruned on get");
    }

    #[tokio::test]
    async fn attach_or_create_replaces_a_closing_entry() {
        let _g = test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        let s = r
            .create("main".into(), spec(), size(), cfg())
            .await
            .unwrap();
        // Mark closing DIRECTLY (not via get/list, which would prune it) to
        // reproduce the lingering-entry state after a natural session close.
        s.closing.store(true, Ordering::SeqCst);
        // The default `plexy-glass` (no args) relaunch calls `attach_or_create`
        // with no preceding get, so it must yield a fresh live session rather
        // than failing with `SessionAlreadyExists` on the stale closing entry.
        let fresh = r
            .attach_or_create("main".into(), spec(), size(), cfg())
            .await
            .expect("must replace a closing entry, not error");
        assert!(!fresh.closing.load(Ordering::SeqCst));
        assert!(r.get("main").await.is_some());
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn reload_config_swaps_session_config() {
        let _g = test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        let s = r
            .create("test".into(), spec(), size(), cfg())
            .await
            .unwrap();
        // Reload re-reads from disk via `load_or_default()` (the platform config
        // dir). `isolate()` only sandboxes the state dir, not the HOME-derived
        // config path, so a developer with a real `config.kdl` (custom status
        // bar) would otherwise fail here. Compare the swapped-in config to a
        // fresh load of the same source, which is what reload actually installs.
        r.reload_config().await.unwrap();
        let cfg_after = s.config_snapshot();
        let (expected, _) = plexy_glass_config::load_or_default();
        assert_eq!(cfg_after.status.left.len(), expected.status.left.len());
        assert_eq!(cfg_after.status.right.len(), expected.status.right.len());
    }
    /// A custom config marked with a recognizable `duration_threshold`, so a
    /// test can tell "kept my config" from "reverted to built-in default".
    fn custom_cfg(threshold_ms: u32) -> Arc<plexy_glass_config::Config> {
        let mut c = plexy_glass_config::built_in_default();
        c.blocks.duration_threshold = Duration::from_millis(threshold_ms.into());
        Arc::new(c)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failed_reload_keeps_last_known_good_config() {
        let _g = test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        // Session running a customized config.
        let s = r
            .create("test".into(), spec(), size(), custom_cfg(999_999))
            .await
            .unwrap();
        // A parse error: the loader hands back the built-in DEFAULT + `Some(err)`.
        // Applying that default would wipe the custom config, which is the bug.
        // The fix keeps the running config untouched.
        let err = Some(plexy_glass_config::ConfigError::Kdl(
            "line 3:1: boom".into(),
        ));
        let result = r
            .apply_reload(plexy_glass_config::built_in_default(), err)
            .await;
        assert!(result.is_err(), "a broken reload reports failure");
        assert_eq!(
            s.config_snapshot().blocks.duration_threshold,
            Duration::from_millis(999_999),
            "custom config must survive a failed reload (not revert to default)"
        );
        assert!(
            r.has_config_error(),
            "the failure is surfaced on the next attach"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn clean_reload_still_swaps_config() {
        let _g = test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        let s = r
            .create("test".into(), spec(), size(), custom_cfg(999_999))
            .await
            .unwrap();
        // A legitimate reload (no error) must still apply the new config.
        let mut good = plexy_glass_config::built_in_default();
        good.blocks.duration_threshold = Duration::from_millis(12_345);
        r.apply_reload(good, None).await.unwrap();
        assert_eq!(
            s.config_snapshot().blocks.duration_threshold,
            Duration::from_millis(12_345),
            "a clean reload swaps in the new config"
        );
        assert!(
            !r.has_config_error(),
            "a clean reload clears any prior error"
        );
    }

    #[tokio::test]
    async fn rename_session_rekeys_map_and_live_name() {
        let _g = test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        let s = r
            .create("before".into(), spec(), size(), cfg())
            .await
            .unwrap();
        r.rename_session("before", "after").await.unwrap();
        assert!(r.get("before").await.is_none(), "old key must be gone");
        let got = r.get("after").await.expect("new key resolves");
        assert!(Arc::ptr_eq(&got, &s), "same session Arc under the new key");
        assert_eq!(s.name(), "after", "live name follows the map key");
    }
    #[tokio::test]
    async fn rename_session_live_collision_errors() {
        let _g = test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        r.create("a".into(), spec(), size(), cfg()).await.unwrap();
        r.create("b".into(), spec(), size(), cfg()).await.unwrap();
        let err = r.rename_session("a", "b").await.unwrap_err();
        assert!(matches!(
            err,
            DaemonError::Protocol(ProtocolError::SessionAlreadyExists { .. })
        ));
        // Both sessions stay reachable under their original names.
        assert!(r.get("a").await.is_some());
        assert!(r.get("b").await.is_some());
    }

    #[tokio::test]
    async fn rename_session_invalid_new_name_errors() {
        let _g = test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        r.create("a".into(), spec(), size(), cfg()).await.unwrap();
        let err = r.rename_session("a", "has space").await.unwrap_err();
        assert!(matches!(
            err,
            DaemonError::Protocol(ProtocolError::InvalidName { .. })
        ));
        assert!(
            r.get("a").await.is_some(),
            "failed rename leaves the session keyed as before"
        );
    }

    #[tokio::test]
    async fn rename_session_unknown_old_errors() {
        let _g = test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        let err = r.rename_session("ghost", "x").await.unwrap_err();
        assert!(matches!(
            err,
            DaemonError::Protocol(ProtocolError::SessionNotFound { .. })
        ));
    }

    #[tokio::test]
    async fn rename_session_rekeys_to_new_name() {
        let _g = test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        r.create("fresh".into(), spec(), size(), cfg())
            .await
            .unwrap();
        r.rename_session("fresh", "moved")
            .await
            .expect("rename succeeds");
        assert!(r.get("moved").await.is_some());
        assert!(r.get("fresh").await.is_none());
    }
    fn cfg_with_session(kdl: &str) -> Arc<plexy_glass_config::Config> {
        Arc::new(plexy_glass_config::parse_config(kdl).expect("declared-session config"))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_declared_builds_template() {
        let _g = test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        let cfg = cfg_with_session(r#"session "dev" { window "w" { pane } }"#);
        let s = r
            .create_declared(&cfg.sessions[0], Arc::clone(&cfg), size())
            .await
            .unwrap();
        assert_eq!(s.name(), "dev");
        assert!(r.get("dev").await.is_some());
        s.terminate_panes().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn build_declared_builds_all_new_names() {
        let _g = test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        let cfg = cfg_with_session(
            "session \"alpha\" { window \"w\" { pane } }\nsession \"beta\" { window \"w\" { pane } }",
        );
        r.build_declared(&cfg, size()).await;
        let alpha = r.get("alpha").await.expect("alpha built");
        let beta = r.get("beta").await.expect("beta built");
        alpha.terminate_panes().await;
        beta.terminate_panes().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn build_declared_skips_live_names_no_rebuild() {
        // A name already live (with a 2-pane shape) is NOT rebuilt even if the
        // template now differs, `build_declared` returns the existing session
        // (the reload "live sessions are never rebuilt" guarantee).
        let _g = test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        let cfg_v1 =
            cfg_with_session(r#"session "dev" { window "w" { split vertical { pane; pane } } }"#);
        let live = r
            .create_declared(&cfg_v1.sessions[0], Arc::clone(&cfg_v1), size())
            .await
            .unwrap();
        let live_ptr = Arc::as_ptr(&live);
        {
            let wm = live.window_manager.lock().await;
            assert_eq!(wm.windows()[0].layout().panes().len(), 2);
        }
        // A reloaded config with a CHANGED (1-pane) "dev" template.
        let cfg_v2 = cfg_with_session(r#"session "dev" { window "w" { pane } }"#);
        r.build_declared(&cfg_v2, size()).await;
        let still = r.get("dev").await.expect("dev still live");
        assert!(
            ptr::eq(Arc::as_ptr(&still), live_ptr),
            "same session Arc (not rebuilt)"
        );
        {
            let wm = still.window_manager.lock().await;
            assert_eq!(
                wm.windows()[0].layout().panes().len(),
                2,
                "live shape unchanged by reload"
            );
        }
        live.terminate_panes().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn attach_or_create_routes_declared_name_to_template() {
        let _g = test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        let cfg =
            cfg_with_session(r#"session "dev" { window "w" { split vertical { pane; pane } } }"#);
        // No saved file, no live session: attach must build the 2-pane template,
        // not a 1-pane fresh `create`.
        let s = r
            .attach_or_create("dev".into(), spec(), size(), Arc::clone(&cfg))
            .await
            .unwrap();
        {
            let wm = s.window_manager.lock().await;
            assert_eq!(wm.windows()[0].layout().panes().len(), 2);
        }
        s.terminate_panes().await;
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn non_declared_name_unaffected_by_routing() {
        let _g = test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        let cfg = cfg_with_session(r#"session "dev" { window "w" { pane } }"#);
        // "other" isn't declared, so this is a normal fresh create (1 pane from
        // `spec()`).
        let s = r
            .attach_or_create("other".into(), spec(), size(), Arc::clone(&cfg))
            .await
            .unwrap();
        {
            let wm = s.window_manager.lock().await;
            assert_eq!(wm.windows().len(), 1);
            assert_eq!(wm.windows()[0].layout().panes().len(), 1);
        }
        s.terminate_panes().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn closing_sessions_are_pruned_on_list() {
        let _g = test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        let alive = r
            .create("alive".into(), spec(), size(), cfg())
            .await
            .unwrap();
        let dead = r
            .create("dead".into(), spec(), size(), cfg())
            .await
            .unwrap();
        dead.closing.store(true, Ordering::SeqCst);
        let entries = r.list().await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "alive");
        // touch `alive` so the borrow checker doesn't complain about unused
        let _ = alive.name();
    }
}
