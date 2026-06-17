//! One PTY-backed pane: child process + emulator + input/output channels.
//!
//! Cloning a `Pane` is cheap (shared `Arc<Inner>`); shared state is protected
//! by Mutex/broadcast/mpsc/watch as before.

use crate::error::DaemonError;
use bytes::Bytes;
use plexy_glass_config::{Config, PaletteConfig};
use plexy_glass_emulator::{ColorQuery, Emulator, Screen};
use plexy_glass_status::Rgb;
use plexy_glass_mux::PaneId;
use plexy_glass_protocol::{ExitStatus, PtySize, SpawnSpec};
use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize as PortablePtySize};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, broadcast, mpsc, watch};
use tracing::{debug, error};

#[derive(Clone)]
pub struct Pane {
    inner: Arc<Inner>,
}

struct Inner {
    id: PaneId,
    input_tx: mpsc::Sender<Bytes>,
    output_tx: broadcast::Sender<Bytes>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    /// Independent kill handle for the child process. Stored because the
    /// child itself is moved into the detached wait thread; killing via this
    /// handle is how session teardown (`kill`) terminates the child.
    /// Dropping the master alone does not, because the reader thread keeps
    /// the PTY open until the child exits.
    child_killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    exit_rx: watch::Receiver<Option<ExitStatus>>,
    emulator: Arc<Mutex<Emulator>>,
    scroll_offset: AtomicU32,
    copy_mode: Mutex<Option<plexy_glass_mux::CopyMode>>,
    /// User-assigned pane name (distinct from the shell-set terminal title on
    /// the emulator screen). `None` until the user renames the pane; shown on
    /// the pane's top border and persisted in the saved-session file.
    name: Mutex<Option<String>>,
    /// Held behind a Mutex so hot reload can swap the Arc.
    /// Wrapped in Arc so the reader thread can clone a handle without
    /// borrowing self.
    config: Arc<Mutex<Arc<Config>>>,
    /// Set by the reader on any output; drained by the daemon for per-window
    /// activity monitoring. `Arc` so the reader thread holds a handle.
    activity: Arc<AtomicBool>,
    /// Set by the reader when the emulator saw a BEL; drained for bell monitoring.
    bell: Arc<AtomicBool>,
    /// The pipe-pane slot (one pipe per pane, see `crate::pipe`). Shared as its
    /// own `Arc` so the drain task and the reader thread hold it WITHOUT
    /// keeping the whole pane (and its broadcast sender) alive.
    pipe: crate::pipe::PipeSlot,
}

impl Pane {
    /// Spawn a PTY-backed pane. `preseed`, when `Some`, is restored scrollback
    /// history (session restore): the rows are pushed into the fresh emulator's
    /// scrollback BEFORE the reader thread starts, so no child byte can advance
    /// the emulator ahead of the seed. Interactive callers (splits, new windows,
    /// popups) pass `None`.
    pub fn spawn(
        id: PaneId,
        spec: SpawnSpec,
        size: PtySize,
        output_notify: Arc<Notify>,
        death_tx: Option<mpsc::Sender<PaneId>>,
        config: Arc<Config>,
        preseed: Option<Vec<plexy_glass_emulator::Row>>,
    ) -> Result<Self, DaemonError> {
        let pty_system = portable_pty::native_pty_system();
        // openpty can transiently fail under load (the OS PTY table is briefly
        // exhausted, observed as "Unknown error: -6" on macOS), so we retry a few
        // times with a short backoff before giving up. This hardens both heavy
        // real-world use and parallel test runs that allocate many PTYs.
        let pair = {
            let mut attempt = 0;
            loop {
                match pty_system.openpty(to_portable(size)) {
                    Ok(p) => break p,
                    Err(_) if attempt < 5 => {
                        attempt += 1;
                        std::thread::sleep(std::time::Duration::from_millis(10 * attempt));
                    }
                    Err(e) => {
                        return Err(DaemonError::Io(std::io::Error::other(format!(
                            "openpty: {e}"
                        ))));
                    }
                }
            }
        };

        let mut cmd = CommandBuilder::new(&spec.program);
        cmd.args(&spec.args);
        if let Some(cwd) = &spec.cwd {
            cmd.cwd(cwd);
        }
        // OVERLAY, do not wipe: declared `env` keys are set ON TOP of the
        // inherited daemon environment (CommandBuilder inherits the parent env
        // unless env_clear is called). A previous version called env_clear()
        // here, which would have dropped `PATH`/`HOME`/`TERM`/`SHELL` (breaking
        // the child) the moment any env was declared. Overlaying preserves the
        // inherited `TERM` (passthrough) and adds only the declared keys.
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        cmd.env("PLEXY_GLASS", "1");
        // We deliberately do NOT force TERM: panes inherit the host terminal's
        // TERM (passthrough), so programs target the real outer terminal (e.g.
        // ghostty's `xterm-ghostty`) and plexy's emulator handles what they
        // emit. Callers who want a different value per multiplexer can set it
        // from their shell using the `PLEXY_GLASS` env var we export above.
        // (Styled underlines etc. are handled correctly by the emulator's SGR
        // decoder regardless of TERM, see emulator::screen::handle_sgr.)

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| DaemonError::Io(std::io::Error::other(format!("spawn: {e}"))))?;
        drop(pair.slave);
        // Capture an independent kill handle before the child is moved into
        // the detached wait thread below.
        let child_killer = child.clone_killer();

        let (input_tx, mut input_rx) = mpsc::channel::<Bytes>(64);
        let (output_tx, _) = broadcast::channel::<Bytes>(256);
        let (exit_tx, exit_rx) = watch::channel::<Option<ExitStatus>>(None);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| DaemonError::Io(std::io::Error::other(format!("clone reader: {e}"))))?;
        let mut writer = pair
            .master
            .take_writer()
            .map_err(|e| DaemonError::Io(std::io::Error::other(format!("take writer: {e}"))))?;
        let master = pair.master;

        // XTGETTCAP `TN` must report the `$TERM` the child actually inherits.
        // The child gets a declared `spec.env` TERM if present; otherwise the
        // env is an overlay (no env_clear), so the daemon's inherited TERM
        // survives (passthrough); otherwise a 256-color xterm default.
        let child_term = spec
            .env
            .iter()
            .find(|(k, _)| k == "TERM")
            .map(|(_, v)| v.clone())
            .or_else(|| std::env::var("TERM").ok())
            .unwrap_or_else(|| "xterm-256color".to_string());
        let mut emu = Emulator::new(size.rows, size.cols);
        emu.screen_mut().set_term(child_term);
        // Apply restored scrollback BEFORE the reader thread (spawned below)
        // can advance the emulator. A post-spawn preseed would race the child's
        // first prompt and could land seeded history below it (scrollback push
        // is FIFO with front-eviction). This ordering is a hard requirement.
        if let Some(rows) = preseed {
            emu.preseed_scrollback(rows);
        }
        let emulator = Arc::new(Mutex::new(emu));
        let config_slot: Arc<Mutex<Arc<Config>>> = Arc::new(Mutex::new(config));

        // Per-pane monitoring signals: set by the reader on output / bell, drained
        // by the daemon's per-frame `update_monitor_flags`.
        let activity = Arc::new(AtomicBool::new(false));
        let bell = Arc::new(AtomicBool::new(false));
        let activity_for_reader = Arc::clone(&activity);
        let bell_for_reader = Arc::clone(&bell);
        // Pipe-pane slot; the reader thread closes any pipe on EOF/Err (the
        // natural child-exit teardown path, which never goes through
        // `kill_child`).
        let pipe: crate::pipe::PipeSlot = Arc::new(Mutex::new(None));
        let pipe_for_reader = Arc::clone(&pipe);

        let output_tx_clone = output_tx.clone();
        let emulator_for_reader = Arc::clone(&emulator);
        let notify_for_reader = Arc::clone(&output_notify);
        // Reader also writes emulator-generated replies (DSR cursor reports,
        // DA, …) back through the input mpsc so the writer thread forwards
        // them to the child. Without this, TUI line editors block on `ESC[6n`.
        // These go out via try_send (drop-on-full), NOT blocking_send: the reader
        // is the SOLE drainer of this pane's PTY OUTPUT, so blocking it on the
        // shared input channel (e.g. a child that stops reading its stdin while
        // still emitting queries) would freeze the pane's output pump. Replies
        // are best-effort responses, so dropping under extreme backpressure is
        // acceptable.
        let reply_tx = input_tx.clone();
        let reader_notify_for_self = Arc::clone(&output_notify);
        let config_for_reader = Arc::clone(&config_slot);

        let (clip_tx, mut clip_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        tokio::spawn(async move {
            while let Some(payload) = clip_rx.recv().await {
                let _ = crate::osc_actions::write_clipboard(&payload).await;
            }
        });

        let reader_handle = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        debug!("pane reader EOF");
                        // No more output can ever flow: close any pipe.
                        crate::pipe::cancel_slot(
                            &pipe_for_reader,
                            crate::pipe::PipeCloseReason::PaneClosed,
                        );
                        // Final notify so the renderer can pick up any
                        // unprocessed bytes before the connection tears down.
                        reader_notify_for_self.notify_one();
                        return;
                    }
                    Ok(n) => {
                        let (replies, clip_writes, color_queries, belled) = {
                            // invariant: emulator mutex held briefly to advance + drain.
                            let mut e = emulator_for_reader
                                .lock()
                                .expect("pane emulator mutex poisoned");
                            e.advance(&buf[..n]);
                            (
                                e.take_replies(),
                                e.take_clipboard_writes(),
                                e.take_color_queries(),
                                e.take_bell(),
                            )
                        };
                        let chunk = Bytes::copy_from_slice(&buf[..n]);
                        let _ = output_tx_clone.send(chunk);
                        // Set the monitoring signals BEFORE notify so the very next
                        // coordinator frame (woken by this notify) drains them. A
                        // one-shot bell would otherwise wait for an unrelated wake.
                        activity_for_reader.store(true, Ordering::Relaxed);
                        if belled {
                            bell_for_reader.store(true, Ordering::Relaxed);
                        }
                        notify_for_reader.notify_one();
                        for reply in replies {
                            match reply_tx.try_send(Bytes::from(reply)) {
                                Ok(()) => {}
                                Err(mpsc::error::TrySendError::Full(_)) => {
                                    debug!("input channel full; dropping emulator reply");
                                }
                                Err(mpsc::error::TrySendError::Closed(_)) => {
                                    debug!("input channel closed; stop forwarding replies");
                                    break;
                                }
                            }
                        }
                        // clip writes are likewise non-blocking (drop on full):
                        // the dedicated drain task may be slow behind pbcopy.
                        for payload in clip_writes {
                            clip_tx.try_send(payload).ok();
                        }
                        if !color_queries.is_empty() {
                            // invariant: config mutex held briefly to clone the palette.
                            let palette = {
                                let cfg = config_for_reader
                                    .lock()
                                    .expect("pane config mutex poisoned");
                                cfg.palette.clone()
                            };
                            for q in color_queries {
                                if let Some(bytes) = format_color_reply(q, &palette) {
                                    match reply_tx.try_send(Bytes::from(bytes)) {
                                        Ok(()) => {}
                                        Err(mpsc::error::TrySendError::Full(_)) => {
                                            debug!("input channel full; dropping color reply");
                                        }
                                        Err(mpsc::error::TrySendError::Closed(_)) => {
                                            debug!("input channel closed; stop color replies");
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        debug!(error = %e, "pane reader closed");
                        crate::pipe::cancel_slot(
                            &pipe_for_reader,
                            crate::pipe::PipeCloseReason::PaneClosed,
                        );
                        return;
                    }
                }
            }
        });

        std::thread::spawn(move || {
            while let Some(chunk) = input_rx.blocking_recv() {
                if let Err(e) = writer.write_all(&chunk) {
                    error!(error = %e, "pane writer error");
                    return;
                }
                if let Err(e) = writer.flush() {
                    error!(error = %e, "pane flush error");
                    return;
                }
            }
        });

        std::thread::spawn(move || {
            let status = wait_child(&mut child);
            let _ = exit_tx.send(Some(status));
            // Wait for the reader thread to drain any remaining PTY bytes
            // (line-editor cleanup, final prompt erase, etc.) into the
            // emulator before signaling death. Otherwise the connection
            // might tear down the renderer while the host TTY is still in a
            // mid-render state, leaving the user's host shell to need a
            // keystroke before redrawing.
            let _ = reader_handle.join();
            if let Some(tx) = death_tx {
                let _ = tx.blocking_send(id);
            }
        });

        Ok(Self {
            inner: Arc::new(Inner {
                id,
                input_tx,
                output_tx,
                master: Mutex::new(master),
                child_killer: Mutex::new(child_killer),
                exit_rx,
                emulator,
                scroll_offset: AtomicU32::new(0),
                copy_mode: Mutex::new(None),
                name: Mutex::new(None),
                config: config_slot,
                activity,
                bell,
                pipe,
            }),
        })
    }

    /// Swap the pane's config in place. Called by hot reload so subsequent
    /// color queries use the new palette.
    pub fn update_config(&self, new: Arc<Config>) {
        // invariant: config mutex briefly held to swap the Arc.
        let mut guard = self
            .inner
            .config
            .lock()
            .expect("pane config mutex poisoned");
        *guard = new;
    }

    /// Terminate the child process. Killing an already-dead child returns an
    /// error which we ignore. Used by session teardown (`kill`): dropping the
    /// pane does NOT reliably terminate the child, because the detached reader
    /// thread holds the PTY master open until the child exits.
    ///
    /// Also closes any pipe-pane explicitly: `kill_child` knows nothing about
    /// the pipe's consumer child, so the pipe must be cancelled here (the
    /// drain task then kills and reaps the consumer). Every production teardown
    /// site flows through this method, both the death-channel paths
    /// (`terminate_panes`, `close_popup`, `kill_window_panes`,
    /// `kill_pane_child`) and the synchronous-close paths (`close_pane` via
    /// Ctrl+a x, `close_active_window` via Ctrl+a &).
    pub fn kill_child(&self) {
        crate::pipe::cancel_slot(&self.inner.pipe, crate::pipe::PipeCloseReason::PaneClosed);
        // invariant: child_killer mutex briefly held; kill never blocks.
        let mut killer = self
            .inner
            .child_killer
            .lock()
            .expect("child_killer mutex poisoned");
        let _ = killer.kill();
    }

    /// The pipe-pane slot (an `Arc` clone, safe for the drain task to hold
    /// without keeping the pane alive).
    pub(crate) fn pipe_slot(&self) -> crate::pipe::PipeSlot {
        Arc::clone(&self.inner.pipe)
    }

    /// Stop a running pipe (cancel + the drain kills/reaps the consumer).
    /// Returns whether a pipe was running.
    pub fn stop_pipe(&self, reason: crate::pipe::PipeCloseReason) -> bool {
        crate::pipe::cancel_slot(&self.inner.pipe, reason)
    }

    /// Whether a pipe is currently installed on this pane.
    pub fn has_pipe(&self) -> bool {
        // invariant: pipe slot mutex held briefly; no await, no nested locks.
        self.inner.pipe.lock().expect("pipe slot poisoned").is_some()
    }

    /// The running pipe consumer's pid, if a pipe is installed. Test
    /// observability for the kill/reap (no-zombie) assertions.
    pub fn pipe_pid(&self) -> Option<u32> {
        // invariant: pipe slot mutex held briefly; no await, no nested locks.
        self.inner.pipe.lock().expect("pipe slot poisoned").as_ref().and_then(|h| h.pid())
    }

    pub fn id(&self) -> PaneId {
        self.inner.id
    }

    /// Read-and-clear whether this pane produced output since the last call.
    pub fn take_activity(&self) -> bool {
        self.inner.activity.swap(false, Ordering::Relaxed)
    }

    /// Read-and-clear whether this pane emitted a BEL since the last call.
    pub fn take_bell(&self) -> bool {
        self.inner.bell.swap(false, Ordering::Relaxed)
    }

    /// The user-assigned pane name, if any (cloned out from under the lock).
    pub fn name(&self) -> Option<String> {
        // invariant: name mutex briefly held to clone the value out.
        self.inner.name.lock().expect("name mutex poisoned").clone()
    }

    /// Set (or clear, with `None`) the user-assigned pane name.
    pub fn set_name(&self, name: Option<String>) {
        // invariant: name mutex briefly held to store the value.
        *self.inner.name.lock().expect("name mutex poisoned") = name;
    }

    pub async fn send_input(&self, bytes: Bytes) -> Result<(), DaemonError> {
        self.inner
            .input_tx
            .send(bytes)
            .await
            .map_err(|_| DaemonError::Io(std::io::Error::other("pane input channel closed")))
    }

    pub fn subscribe_output(&self) -> broadcast::Receiver<Bytes> {
        self.inner.output_tx.subscribe()
    }

    pub fn resize(&self, size: PtySize) -> Result<(), DaemonError> {
        {
            // invariant: contended only by resize calls; brief hold.
            let master = self
                .inner
                .master
                .lock()
                .expect("pane master mutex poisoned");
            master
                .resize(to_portable(size))
                .map_err(|e| DaemonError::Io(std::io::Error::other(format!("resize: {e}"))))?;
        }
        {
            // invariant: emulator mutex contended only briefly.
            let mut emu = self
                .inner
                .emulator
                .lock()
                .expect("pane emulator mutex poisoned");
            emu.resize(size.rows, size.cols);
        }
        Ok(())
    }

    pub async fn wait(&self) -> ExitStatus {
        let mut rx = self.inner.exit_rx.clone();
        // Err only if the sender (the child-exit watch) dropped without a
        // value; the old loop returned Unknown in that case, so match it.
        match rx.wait_for(|s| s.is_some()).await {
            Ok(s) => s.unwrap_or(ExitStatus::Unknown),
            Err(_) => ExitStatus::Unknown,
        }
    }

    pub fn exit_rx(&self) -> watch::Receiver<Option<ExitStatus>> {
        self.inner.exit_rx.clone()
    }

    pub fn with_screen<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Screen) -> R,
    {
        // invariant: emulator mutex contended only briefly.
        let emu = self
            .inner
            .emulator
            .lock()
            .expect("pane emulator mutex poisoned");
        f(emu.screen())
    }

    /// Whether this pane's application turned on bracketed paste (?2004).
    /// The paste paths gate their `\e[200~`/`\e[201~` wrapping on the pane
    /// the bytes actually go to, see `WindowManager::input_target_pane`.
    pub fn wants_bracketed_paste(&self) -> bool {
        self.with_screen(|s| s.modes.contains(plexy_glass_emulator::Modes::BRACKETED_PASTE))
    }

    pub fn with_screen_mut<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut plexy_glass_emulator::Screen) -> R,
    {
        let mut emu = self.inner.emulator.lock().expect("pane emulator mutex poisoned");
        f(emu.screen_mut())
    }

    pub fn scroll_offset(&self) -> u32 {
        self.inner.scroll_offset.load(Ordering::SeqCst)
    }

    /// Adjust the scroll offset by `delta` rows (positive = up into
    /// scrollback, negative = down toward live). Clamps to `[0, max]`.
    pub fn scroll_by(&self, delta: i32, max_offset: u32) {
        let _ = self.inner.scroll_offset.fetch_update(
            Ordering::SeqCst,
            Ordering::SeqCst,
            |current| Some((current as i64 + delta as i64).clamp(0, max_offset as i64) as u32),
        );
    }

    pub fn reset_scroll(&self) {
        self.inner.scroll_offset.store(0, Ordering::SeqCst);
    }

    /// Set the absolute scroll offset (scrollback rows shown above the live
    /// grid), clamped to `[0, max_offset]`. The block-scroll verbs compute a
    /// target offset and need an absolute set; `scroll_by` is relative.
    pub fn set_scroll_offset(&self, offset: u32, max_offset: u32) {
        self.inner.scroll_offset.store(offset.min(max_offset), Ordering::SeqCst);
    }

    pub fn scrollback_len(&self) -> u32 {
        // invariant: emulator mutex briefly held to read len.
        let emu = self.inner.emulator.lock().expect("pane emulator mutex poisoned");
        emu.screen().scrollback.len() as u32
    }

    pub fn enter_copy_mode(
        &self,
        total_lines: u32,
        pane_rows: u16,
        start_line: u32,
        start_col: u16,
    ) {
        // invariant: copy_mode mutex is only contended with the Connection's
        // brief checks; no async holding.
        let mut guard = self
            .inner
            .copy_mode
            .lock()
            .expect("pane copy_mode mutex poisoned");
        *guard = Some(plexy_glass_mux::CopyMode::new(
            total_lines,
            pane_rows,
            start_line,
            start_col,
        ));
    }

    pub fn exit_copy_mode(&self) {
        let mut guard = self
            .inner
            .copy_mode
            .lock()
            .expect("pane copy_mode mutex poisoned");
        *guard = None;
    }

    pub fn is_in_copy_mode(&self) -> bool {
        self.inner
            .copy_mode
            .lock()
            .expect("pane copy_mode mutex poisoned")
            .is_some()
    }

    pub fn with_copy_mode_mut<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&mut plexy_glass_mux::CopyMode) -> R,
    {
        let mut guard = self
            .inner
            .copy_mode
            .lock()
            .expect("pane copy_mode mutex poisoned");
        guard.as_mut().map(f)
    }

    pub fn with_copy_mode<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&plexy_glass_mux::CopyMode) -> R,
    {
        let guard = self
            .inner
            .copy_mode
            .lock()
            .expect("pane copy_mode mutex poisoned");
        guard.as_ref().map(f)
    }

    /// Notify the pane that its cell size has changed (called from
    /// `Window::resize` after the layout recomputes pane rects). Keeps
    /// `CopyMode` state consistent across host resizes.
    pub fn on_size_changed(&self, new_rows: u16) {
        let total_lines = self.with_screen(|s| {
            s.scrollback.len() as u32 + u32::from(s.active.num_rows())
        });
        let _ = self.with_copy_mode_mut(|cm| {
            cm.set_pane_rows(new_rows, total_lines);
        });
    }
}

/// Format an OSC 10/11/12 reply for the given color query using the configured
/// palette. Returns `None` if the palette is missing the relevant entry or
/// holds an unparseable hex value, in which case the daemon stays silent
/// (matches xterm behaviour when no answer is available).
///
/// Palette entries are expected to be hex literals (`#RRGGBB`). Indirect
/// references (e.g. `cursor = "accent"`) are not resolved here, they should
/// be resolved to hex literals by config-load time.
fn format_color_reply(query: ColorQuery, palette: &PaletteConfig) -> Option<Vec<u8>> {
    let (osc_num, key, fallback) = match query {
        ColorQuery::Foreground => ("10", "fg", None),
        ColorQuery::Background => ("11", "bg", None),
        ColorQuery::Cursor => ("12", "cursor", Some("accent")),
    };
    let hex = palette
        .entries
        .get(key)
        .or_else(|| fallback.and_then(|k| palette.entries.get(k)))
        .or_else(|| palette.entries.get("fg"))?
        .clone();
    let Rgb { r, g, b } = Rgb::parse_hex(&hex).or_else(|| {
        tracing::debug!(
            hex = hex.as_str(),
            key,
            "palette entry failed to parse as hex; OSC color reply skipped"
        );
        None
    })?;
    Some(
        format!(
            "\x1b]{osc_num};rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}\x07",
            r, r, g, g, b, b,
        )
        .into_bytes(),
    )
}

fn to_portable(size: PtySize) -> PortablePtySize {
    PortablePtySize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: size.pixel_width,
        pixel_height: size.pixel_height,
    }
}

fn wait_child(child: &mut Box<dyn Child + Send + Sync>) -> ExitStatus {
    match child.wait() {
        Ok(s) if s.success() => ExitStatus::Code(0),
        Ok(s) => ExitStatus::Code(s.exit_code() as i32),
        Err(_) => ExitStatus::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexy_glass_mux::PaneId;
    use tokio::sync::Notify;

    fn size() -> PtySize {
        PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    fn cfg() -> Arc<Config> {
        Arc::new(plexy_glass_config::built_in_default())
    }

    #[tokio::test]
    async fn kill_child_terminates_the_process() {
        // A long-lived child that would never exit on its own.
        let spec = SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };
        let p =
            Pane::spawn(PaneId(0), spec, size(), Arc::new(Notify::new()), None, cfg(), None).expect("spawn");
        let mut exit = p.exit_rx();
        assert!(exit.borrow().is_none(), "child should still be running");
        p.kill_child();
        // exit_rx flips to Some once the wait thread observes the child dying.
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            exit.wait_for(|s| s.is_some()).await
        })
        .await;
        assert!(res.is_ok(), "child did not exit within 5s after kill_child");
    }

    #[tokio::test]
    async fn spawn_sets_screen_term_from_env() {
        // A pane spawned with `TERM` in `spec.env` reports it via `Screen.term` (the
        // value XTGETTCAP `TN` answers) instead of the default.
        let spec = SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![("TERM".into(), "xterm-ghostty".into())],
            cwd: None,
        };
        let p = Pane::spawn(PaneId(0), spec, size(), Arc::new(Notify::new()), None, cfg(), None)
            .expect("spawn");
        assert_eq!(p.with_screen(|s| s.term.clone()), "xterm-ghostty");
        p.kill_child();
    }

    #[tokio::test]
    async fn echo_hello_round_trips() {
        let spec = SpawnSpec {
            program: "/bin/echo".into(),
            args: vec!["hello".into()],
            env: vec![],
            cwd: None,
        };
        let pane = Pane::spawn(PaneId(0), spec, size(), Arc::new(Notify::new()), None, cfg(), None)
            .expect("spawn");

        // Poll the emulator screen rather than the broadcast channel: /bin/echo
        // exits almost immediately, so its output can be broadcast (and lost)
        // before a subscriber attaches. The screen retains the rendered output
        // regardless of subscription timing, so it is the race-free signal.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut found = false;
        while tokio::time::Instant::now() < deadline {
            let row0 = pane.with_screen(|s| {
                (0..s.active.num_cols())
                    .filter_map(|c| {
                        s.active
                            .get_cell(0, c)
                            .map(|cell| cell.grapheme.as_str().to_string())
                    })
                    .collect::<String>()
            });
            if row0.contains("hello") {
                found = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(found, "emulator screen never showed 'hello'");

        let status = tokio::time::timeout(std::time::Duration::from_secs(2), pane.wait())
            .await
            .expect("pane.wait");
        assert!(matches!(status, ExitStatus::Code(0)), "got {status:?}");
    }

    #[tokio::test]
    async fn cat_echoes_input() {
        let spec = SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };
        let session = Pane::spawn(PaneId(0), spec, size(), Arc::new(Notify::new()), None, cfg(), None).expect("spawn");
        let mut rx = session.subscribe_output();

        session
            .send_input(Bytes::from_static(b"ping\n"))
            .await
            .unwrap();

        let mut got = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            if let Ok(Ok(chunk)) =
                tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
            {
                got.extend_from_slice(&chunk);
            }
            if got.windows(4).any(|w| w == b"ping") {
                break;
            }
        }
        assert!(
            got.windows(4).any(|w| w == b"ping"),
            "got: {:?}",
            String::from_utf8_lossy(&got)
        );

        // Send Ctrl-D (EOT) so `cat` exits.
        session
            .send_input(Bytes::from_static(&[0x04]))
            .await
            .unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), session.wait()).await;
    }

    #[tokio::test]
    async fn take_activity_and_bell_report_output() {
        let spec = SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };
        let p = Pane::spawn(PaneId(0), spec, size(), Arc::new(Notify::new()), None, cfg(), None)
            .expect("spawn");
        assert!(!p.take_activity(), "no activity before any output");
        assert!(!p.take_bell(), "no bell before any output");
        // cat echoes its input; the BEL byte makes the emulator flag a bell.
        p.send_input(Bytes::from_static(b"hi\x07\n")).await.unwrap();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut saw_activity = false;
        let mut saw_bell = false;
        while tokio::time::Instant::now() < deadline && !(saw_activity && saw_bell) {
            saw_activity |= p.take_activity();
            saw_bell |= p.take_bell();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(saw_activity, "output set the activity signal");
        assert!(saw_bell, "the echoed BEL set the bell signal");

        p.send_input(Bytes::from_static(&[0x04])).await.unwrap(); // Ctrl-D → exit
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), p.wait()).await;
    }

    #[tokio::test]
    async fn emulator_captures_echo_output() {
        let spec = SpawnSpec {
            program: "/bin/echo".into(),
            args: vec!["hello".into()],
            env: vec![],
            cwd: None,
        };
        let session = Pane::spawn(PaneId(0), spec, size(), Arc::new(Notify::new()), None, cfg(), None).expect("spawn");
        // Wait for the child to exit so the PTY has flushed.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), session.wait()).await;
        // Give the reader thread a beat to drain.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let saw_hello = session.with_screen(|screen| {
            (0..screen.rows()).any(|r| {
                let row_text: String = screen.active.rows[r as usize]
                    .cells
                    .iter()
                    .filter(|c| !c.is_wide_spacer())
                    .map(|c| c.grapheme.as_str())
                    .collect();
                row_text.contains("hello")
            })
        });
        assert!(saw_hello, "emulator did not capture 'hello'");
    }

    #[tokio::test]
    async fn emulator_resizes_with_session() {
        let spec = SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };
        let session = Pane::spawn(PaneId(0), spec, size(), Arc::new(Notify::new()), None, cfg(), None).expect("spawn");

        session
            .resize(PtySize {
                rows: 30,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize");
        let (r, c) = session.with_screen(|s| (s.rows(), s.cols()));
        assert_eq!((r, c), (30, 100));

        // Send EOF so `cat` exits.
        session
            .send_input(Bytes::from_static(&[0x04]))
            .await
            .unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), session.wait()).await;
    }

    #[tokio::test]
    async fn scroll_offset_starts_at_zero() {
        let spec = SpawnSpec {
            program: "/bin/echo".into(),
            args: vec!["hi".into()],
            env: vec![],
            cwd: None,
        };
        let p = Pane::spawn(PaneId(0), spec, size(), Arc::new(Notify::new()), None, cfg(), None).expect("spawn");
        assert_eq!(p.scroll_offset(), 0);
    }

    #[tokio::test]
    async fn scroll_by_clamps_at_zero_and_max() {
        let spec = SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };
        let p = Pane::spawn(PaneId(0), spec, size(), Arc::new(Notify::new()), None, cfg(), None).expect("spawn");
        p.scroll_by(-5, 100);
        assert_eq!(p.scroll_offset(), 0);
        p.scroll_by(3, 10);
        assert_eq!(p.scroll_offset(), 3);
        p.scroll_by(-1, 10);
        assert_eq!(p.scroll_offset(), 2);
        p.scroll_by(100, 10);
        assert_eq!(p.scroll_offset(), 10);
        p.reset_scroll();
        assert_eq!(p.scroll_offset(), 0);
        let _ = p.send_input(bytes::Bytes::from_static(&[0x04])).await;
    }

    #[tokio::test]
    async fn set_scroll_offset_is_absolute_and_clamped() {
        let spec = SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };
        let p = Pane::spawn(PaneId(0), spec, size(), Arc::new(Notify::new()), None, cfg(), None).expect("spawn");
        p.set_scroll_offset(7, 10);
        assert_eq!(p.scroll_offset(), 7);
        // Absolute, not relative: setting 3 lands on 3, not 10.
        p.set_scroll_offset(3, 10);
        assert_eq!(p.scroll_offset(), 3);
        p.set_scroll_offset(99, 10);
        assert_eq!(p.scroll_offset(), 10, "clamped to max_offset");
        p.set_scroll_offset(0, 10);
        assert_eq!(p.scroll_offset(), 0);
        let _ = p.send_input(bytes::Bytes::from_static(&[0x04])).await;
    }

    #[tokio::test]
    async fn emulator_records_sgr_attributes_from_child() {
        let spec = SpawnSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "printf '\\x1b[1mhi\\x1b[0m'".into()],
            env: vec![],
            cwd: None,
        };
        let session = Pane::spawn(PaneId(0), spec, size(), Arc::new(Notify::new()), None, cfg(), None).expect("spawn");
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), session.wait()).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let bold = session.with_screen(|screen| {
            use plexy_glass_emulator::Attrs;
            screen.active.rows[0]
                .cells
                .iter()
                .take(2)
                .all(|c| c.attrs.contains(Attrs::BOLD))
        });
        assert!(bold, "expected first two cells to be BOLD");
    }

    #[tokio::test]
    async fn enter_and_exit_copy_mode() {
        let spec = SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };
        let p = Pane::spawn(PaneId(0), spec, size(), Arc::new(Notify::new()), None, cfg(), None).unwrap();
        assert!(!p.is_in_copy_mode());
        p.enter_copy_mode(100, 24, 99, 0);
        assert!(p.is_in_copy_mode());
        let cursor = p.with_copy_mode(|s| s.cursor).unwrap();
        assert_eq!(cursor.0, 99);
        p.exit_copy_mode();
        assert!(!p.is_in_copy_mode());
        let _ = p.send_input(bytes::Bytes::from_static(&[0x04])).await;
    }

    #[tokio::test]
    async fn on_size_changed_updates_copy_mode_state() {
        let spec = SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };
        let p = Pane::spawn(PaneId(0), spec, size(), Arc::new(Notify::new()), None, cfg(), None).unwrap();
        p.enter_copy_mode(100, 24, 99, 0);
        p.on_size_changed(10);
        let pane_rows = p.with_copy_mode(|cm| cm.pane_rows).unwrap();
        assert_eq!(pane_rows, 10);
        let _ = p.send_input(bytes::Bytes::from_static(&[0x04])).await;
    }

    #[tokio::test]
    async fn color_query_path_does_not_panic() {
        use std::time::Duration;
        let spec = SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };
        let p = Pane::spawn(
            PaneId(0),
            spec,
            size(),
            Arc::new(Notify::new()),
            None,
            cfg(),
            None,
        )
        .unwrap();
        // Send OSC 11 query through cat (which echoes stdin back); the emulator
        // parses it on the way out, the reader thread drains and replies via
        // reply_tx.
        p.send_input(Bytes::copy_from_slice(b"\x1b]11;?\x07"))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        // Drained queries shouldn't remain in the emulator. Mainly asserts no
        // panic + the drain path runs without leaving residue.
        let pending = p
            .inner
            .emulator
            .lock()
            .expect("pane emulator mutex poisoned")
            .take_color_queries();
        assert!(pending.is_empty(), "expected no leftover color queries");
        let _ = p.send_input(Bytes::from_static(&[0x04])).await;
    }

    #[tokio::test]
    async fn spawn_with_preseed_seeds_history_and_child_draws_below() {
        use plexy_glass_emulator::{Row, RowMark};
        // Two restored history rows (one marked) seeded into the fresh pane.
        let mut prompt = Row::blank(80);
        prompt.cells[0].grapheme = "$".into();
        prompt.mark.set(RowMark::PROMPT_START);
        let mut out = Row::blank(80);
        for (i, ch) in "HISTORY".chars().enumerate() {
            out.cells[i].grapheme = ch.to_string().into();
        }
        let preseed = vec![prompt, out];

        let spec = SpawnSpec {
            program: "/bin/cat".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };
        let p = Pane::spawn(
            PaneId(0),
            spec,
            size(),
            Arc::new(Notify::new()),
            None,
            cfg(),
            Some(preseed),
        )
        .expect("spawn");

        // The seeded history is in scrollback (with its mark) immediately, before
        // any child output.
        let (sb_len, has_prompt_mark, hist_text) = p.with_screen(|s| {
            let rows = s.scrollback.rows();
            (
                rows.len(),
                rows.front().map(|r| r.mark.contains(RowMark::PROMPT_START)).unwrap_or(false),
                rows.get(1)
                    .map(|r| r.cells.iter().map(|c| c.grapheme.as_str()).collect::<String>())
                    .unwrap_or_default(),
            )
        });
        assert_eq!(sb_len, 2, "both history rows seeded into scrollback");
        assert!(has_prompt_mark, "the seeded prompt mark rode into scrollback");
        assert!(hist_text.contains("HISTORY"), "seeded history text present: {hist_text:?}");

        // cat echoes input into the LIVE grid below the history.
        p.send_input(Bytes::from_static(b"FRESH\n")).await.unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut saw_fresh = false;
        while tokio::time::Instant::now() < deadline {
            let row0: String = p.with_screen(|s| {
                s.active.rows[0].cells.iter().map(|c| c.grapheme.as_str()).collect()
            });
            if row0.contains("FRESH") {
                saw_fresh = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(saw_fresh, "child output landed in the live grid below the history");
        // History stays in scrollback (the child's output did not displace it).
        assert_eq!(p.with_screen(|s| s.scrollback.len()), 2, "history still in scrollback");

        p.send_input(Bytes::from_static(&[0x04])).await.unwrap(); // Ctrl-D → exit
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), p.wait()).await;
    }

    #[test]
    fn format_color_reply_uses_palette_bg() {
        let palette = plexy_glass_config::kanagawa_dragon_palette();
        let bytes = format_color_reply(ColorQuery::Background, &palette).expect("reply");
        // bg = #1D1C19; expanded RRRR/GGGG/BBBB is 1d1d/1c1c/1919.
        assert_eq!(bytes, b"\x1b]11;rgb:1d1d/1c1c/1919\x07");
    }

    #[test]
    fn format_color_reply_uses_fg() {
        let palette = plexy_glass_config::kanagawa_dragon_palette();
        let bytes = format_color_reply(ColorQuery::Foreground, &palette).expect("reply");
        // fg = #c8c093.
        assert_eq!(bytes, b"\x1b]10;rgb:c8c8/c0c0/9393\x07");
    }

    #[test]
    fn format_color_reply_cursor_falls_back_to_accent() {
        // Default palette has no `cursor` entry, so fall back to `accent`.
        let palette = plexy_glass_config::kanagawa_dragon_palette();
        let bytes = format_color_reply(ColorQuery::Cursor, &palette).expect("reply");
        // accent = #737c73.
        assert_eq!(bytes, b"\x1b]12;rgb:7373/7c7c/7373\x07");
    }
}
