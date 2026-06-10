//! Daemon-wide registry of named sessions.

use crate::paste_buffers::PasteBufferStore;
use crate::{error::DaemonError, session::Session};
use plexy_glass_mux::BufferEntry;
use plexy_glass_protocol::{ProtocolError, PtySize, SessionEntry, SpawnSpec};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Maximum retained paste buffers (tmux-style; oldest evicted past this).
const PASTE_BUFFER_CAP: usize = 50;

pub struct SessionRegistry {
    inner: Mutex<HashMap<String, Arc<Session>>>,
    /// Daemon-global paste buffers.
    ///
    /// Independent of `inner` and never locked while `inner` is held (the
    /// delegates touch only this lock).
    paste_buffers: Mutex<PasteBufferStore>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            paste_buffers: Mutex::new(PasteBufferStore::new(PASTE_BUFFER_CAP)),
        }
    }

    /// Push a new newest paste buffer (copy-mode yank).
    pub async fn push_paste_buffer(&self, content: Vec<u8>) {
        self.paste_buffers.lock().await.push(content);
    }

    /// Clone out the most-recent buffer's content, if any.
    pub async fn paste_buffer_top(&self) -> Option<Vec<u8>> {
        self.paste_buffers.lock().await.top().map(|b| b.content.clone())
    }

    /// Clone out a named buffer's content, if present.
    pub async fn paste_buffer_get(&self, name: &str) -> Option<Vec<u8>> {
        self.paste_buffers.lock().await.get(name).map(|b| b.content.clone())
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
        map.retain(|_, s| !s.closing.load(std::sync::atomic::Ordering::SeqCst));
        // `list_entry` takes blocking locks, so defer to spawn_blocking-style.
        let mut out: Vec<SessionEntry> = map
            .values()
            .map(|s| {
                let s = Arc::clone(s);
                tokio::task::block_in_place(|| s.list_entry())
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub async fn get(&self, name: &str) -> Option<Arc<Session>> {
        let mut map = self.inner.lock().await;
        if let Some(s) = map.get(name) {
            if s.closing.load(std::sync::atomic::Ordering::SeqCst) {
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
        validate_name(&name)?;
        let mut map = self.inner.lock().await;
        if map.contains_key(&name) {
            return Err(DaemonError::Protocol(ProtocolError::SessionAlreadyExists { name }));
        }
        let session = Session::new(name.clone(), cmd, size, config)?;
        map.insert(name, Arc::clone(&session));
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
        validate_name(&template.name)?;
        let mut map = self.inner.lock().await;
        if let Some(s) = map.get(&template.name)
            && !s.closing.load(std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(Arc::clone(s));
        }
        let session = Session::build_from_template(template, size, config).await?;
        map.insert(template.name.clone(), Arc::clone(&session));
        Ok(session)
    }

    /// Attach-or-create with restore.
    ///
    /// An existing in-memory session wins; else try the saved file; else fresh
    /// `create`. Failures on the saved-file path fall back to fresh and log at
    /// warn.
    pub async fn attach_or_create(
        &self,
        name: String,
        cmd: SpawnSpec,
        size: PtySize,
        config: Arc<plexy_glass_config::Config>,
    ) -> Result<Arc<Session>, DaemonError> {
        validate_name(&name)?;
        // Fast path: already running.
        {
            let map = self.inner.lock().await;
            if let Some(s) = map.get(&name)
                && !s.closing.load(std::sync::atomic::Ordering::SeqCst)
            {
                return Ok(Arc::clone(s));
            }
        }
        // Config wins: a declared session name is (re)built from its template,
        // never restored from disk. (The client `cmd` is intentionally unused
        // here: declared panes come from the template + the daemon default
        // shell, so a session is identical whether built at boot or on attach.)
        if let Some(template) = config.sessions.iter().find(|t| t.name == name) {
            return self.create_declared(template, Arc::clone(&config), size).await;
        }
        // Try restore.
        match crate::persist::load_session(&name) {
            Ok(Some(saved)) => {
                match Session::restore_from(saved, cmd.clone(), size, Arc::clone(&config)).await {
                    Ok(session) => {
                        let mut map = self.inner.lock().await;
                        map.insert(name, Arc::clone(&session));
                        return Ok(session);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, %name, "session restore failed; falling back to fresh");
                    }
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, %name, "saved session load failed; falling back to fresh");
            }
        }
        self.create(name, cmd, size, config).await
    }

    pub async fn kill(&self, name: &str) -> Result<(), DaemonError> {
        let session = {
            let mut map = self.inner.lock().await;
            map.remove(name).ok_or_else(|| {
                DaemonError::Protocol(ProtocolError::SessionNotFound { name: name.to_string() })
            })?
        };
        // 1. Set closing + abort the Arc-pinning tasks (death/tick), signal the
        //    coordinator to emit a final frame and exit (tearing down attached
        //    clients).
        session.begin_close();
        // 2. Stop the persist task AND await its termination, so no in-flight
        //    `save_session` can re-create the file after we delete it below.
        session.stop_persist().await;
        // 3. Terminate pane children. Dropping panes alone does not SIGHUP
        //    them (the reader thread holds the PTY master open).
        session.terminate_panes().await;
        // 4. Delete the saved file. Safe now: the persist task is fully stopped
        //    (awaited above) and guards on `closing`, so it cannot resurrect
        //    this file. `NotFound` is fine, a session never marked dirty has no
        //    on-disk file.
        if let Err(e) = crate::persist::delete_session(name) {
            tracing::debug!(error = %e, %name, "delete saved session file (non-fatal)");
        }
        Ok(())
    }

    pub async fn prune_empty(&self) {
        let mut map = self.inner.lock().await;
        map.retain(|_, s| !s.closing.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// Re-read config from disk and apply to every session.
    ///
    /// The KDL loader (`plexy_glass_config::load_or_default`) returns
    /// `(Config, Option<ConfigError>)`: even on a parse error we still get
    /// the built-in default. This method propagates that default to every
    /// live session (so the daemon prefers a known-good config to running
    /// on stale state), then returns the parse error to the caller.
    ///
    /// Never panics mid-reload, each `Session::swap_config` is independent.
    pub async fn reload_config(&self) -> Result<(), DaemonError> {
        let (new_config, err) = plexy_glass_config::load_or_default();
        if let Some(e) = &err {
            tracing::warn!(error = %e, "reload: parse error; using fallback");
        }
        let new_config = Arc::new(new_config);
        let map = self.inner.lock().await;
        for session in map.values() {
            session.swap_config(Arc::clone(&new_config)).await;
        }
        drop(map);
        match err {
            None => Ok(()),
            Some(e) => Err(DaemonError::from(e)),
        }
    }
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn validate_name(name: &str) -> Result<(), DaemonError> {
    if name.is_empty() || name.len() > 64 {
        return Err(DaemonError::Protocol(ProtocolError::EmptyName));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(DaemonError::Protocol(ProtocolError::InvalidName { name: name.to_string() }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> SpawnSpec {
        SpawnSpec {
            program: "/bin/sh".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        }
    }

    fn size() -> PtySize {
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }
    }

    fn cfg() -> Arc<plexy_glass_config::Config> {
        Arc::new(plexy_glass_config::built_in_default())
    }

    #[tokio::test]
    async fn create_then_get() {
        let _g = crate::test_env::isolate();
        let r = SessionRegistry::new();
        let s = r.create("main".into(), spec(), size(), cfg()).await.unwrap();
        assert_eq!(s.name, "main");
        let got = r.get("main").await.unwrap();
        assert_eq!(got.name, "main");
    }

    #[tokio::test]
    async fn duplicate_create_fails() {
        let _g = crate::test_env::isolate();
        let r = SessionRegistry::new();
        r.create("main".into(), spec(), size(), cfg()).await.unwrap();
        let err =
            r.create("main".into(), spec(), size(), cfg()).await.map(|_| ()).unwrap_err();
        assert!(matches!(err, DaemonError::Protocol(ProtocolError::SessionAlreadyExists { .. })));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_returns_sorted_entries() {
        let _g = crate::test_env::isolate();
        let r = SessionRegistry::new();
        r.create("zeta".into(), spec(), size(), cfg()).await.unwrap();
        r.create("alpha".into(), spec(), size(), cfg()).await.unwrap();
        let entries = r.list().await;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "alpha");
        assert_eq!(entries[1].name, "zeta");
    }

    #[tokio::test]
    async fn kill_unknown_returns_session_not_found() {
        let _g = crate::test_env::isolate();
        let r = SessionRegistry::new();
        let err = r.kill("ghost").await.unwrap_err();
        assert!(matches!(err, DaemonError::Protocol(ProtocolError::SessionNotFound { .. })));
    }

    #[tokio::test]
    async fn name_validation_rejects_empty() {
        let _g = crate::test_env::isolate();
        let r = SessionRegistry::new();
        let err = r.create("".into(), spec(), size(), cfg()).await.map(|_| ()).unwrap_err();
        assert!(matches!(err, DaemonError::Protocol(ProtocolError::EmptyName)));
    }

    #[tokio::test]
    async fn name_validation_rejects_invalid_chars() {
        let _g = crate::test_env::isolate();
        let r = SessionRegistry::new();
        let err = r
            .create("has space".into(), spec(), size(), cfg())
            .await
            .map(|_| ())
            .unwrap_err();
        assert!(matches!(err, DaemonError::Protocol(ProtocolError::InvalidName { .. })));
    }

    #[tokio::test]
    async fn closing_sessions_are_pruned_on_get() {
        let _g = crate::test_env::isolate();
        let r = SessionRegistry::new();
        let s = r.create("dead".into(), spec(), size(), cfg()).await.unwrap();
        s.closing.store(true, std::sync::atomic::Ordering::SeqCst);
        let got = r.get("dead").await;
        assert!(got.is_none(), "closing session should be pruned on get");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reload_config_swaps_session_config() {
        let _g = crate::test_env::isolate();
        let r = SessionRegistry::new();
        let s = r.create("test".into(), spec(), size(), cfg()).await.unwrap();
        let cfg_before = s.config_snapshot();
        // Reload (will re-read from real XDG path; in tests this just returns
        // the built-in default).
        r.reload_config().await.unwrap();
        let cfg_after = s.config_snapshot();
        // Both should be `Arc::clone`s of a default `Config`, so pointer
        // equality won't hold but structural equality should.
        assert_eq!(cfg_before.status.left.len(), cfg_after.status.left.len());
        assert_eq!(cfg_before.status.right.len(), cfg_after.status.right.len());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kill_with_pinned_session_keeps_file_deleted() {
        let _g = crate::test_env::isolate();
        let r = SessionRegistry::new();
        // Hold the strong Arc across the kill to simulate an attached client /
        // running coordinator that pins the Session past the kill (the exact
        // condition under which the bug resurrected the file).
        let s = r.create("pinned".into(), spec(), size(), cfg()).await.unwrap();
        s.mark_dirty();
        tokio::time::sleep(std::time::Duration::from_millis(1800)).await;
        assert!(crate::persist::load_session("pinned").unwrap().is_some());

        r.kill("pinned").await.unwrap();
        assert!(r.get("pinned").await.is_none(), "session still in registry after kill");
        // Try to make the (now-aborted) persist task resurrect the file.
        s.mark_dirty();
        s.persist_notify.notify_one();
        tokio::time::sleep(std::time::Duration::from_millis(1800)).await;
        assert!(
            crate::persist::load_session("pinned").unwrap().is_none(),
            "file resurrected after kill while session Arc was held"
        );
        drop(s);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kill_deletes_saved_file() {
        let _g = crate::test_env::isolate();
        let r = SessionRegistry::new();
        let s = r.create("kill-me".into(), spec(), size(), cfg()).await.unwrap();
        s.mark_dirty();
        tokio::time::sleep(std::time::Duration::from_millis(1800)).await;
        assert!(crate::persist::load_session("kill-me").unwrap().is_some());
        r.kill("kill-me").await.unwrap();
        assert!(crate::persist::load_session("kill-me").unwrap().is_none());
    }

    fn cfg_with_session(kdl: &str) -> Arc<plexy_glass_config::Config> {
        Arc::new(plexy_glass_config::parse_config(kdl).expect("declared-session config"))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_declared_builds_template() {
        let _g = crate::test_env::isolate();
        let r = SessionRegistry::new();
        let cfg = cfg_with_session(r##"session "dev" { window "w" { pane } }"##);
        let s = r.create_declared(&cfg.sessions[0], Arc::clone(&cfg), size()).await.unwrap();
        assert_eq!(s.name, "dev");
        assert!(r.get("dev").await.is_some());
        s.terminate_panes().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn attach_or_create_routes_declared_name_to_template() {
        let _g = crate::test_env::isolate();
        let r = SessionRegistry::new();
        let cfg = cfg_with_session(r##"session "dev" { window "w" { split vertical { pane; pane } } }"##);
        // No saved file, no live session: attach must build the 2-pane template,
        // not a 1-pane fresh `create`.
        let s = r.attach_or_create("dev".into(), spec(), size(), Arc::clone(&cfg)).await.unwrap();
        {
            let wm = s.window_manager.lock().await;
            assert_eq!(wm.windows()[0].layout().panes().len(), 2);
        }
        s.terminate_panes().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn declared_session_wins_over_saved_disk_state() {
        let _g = crate::test_env::isolate();
        // A saved file for "dev" with a DIFFERENT (1-pane, name "stale") shape
        // than the declared template, so the test distinguishes "config wins"
        // from "no file present".
        let saved = crate::persist::SessionStateV1 {
            schema: crate::persist::SCHEMA_VERSION,
            name: "dev".into(),
            created: chrono::Utc::now(),
            active_window: 0,
            windows: vec![crate::persist::WindowStateV1 {
                name: "stale".into(),
                sync_input: false,
                home_cwd: None,
                active_pane: 0,
                panes: vec![crate::persist::PaneStateV1 { cwd: None, name: None }],
                layout: crate::persist::LayoutStateV1::Leaf(0),
            }],
        };
        crate::persist::save_session(&saved).unwrap();
        assert!(crate::persist::load_session("dev").unwrap().is_some(), "precondition: saved file exists");

        // Config declares "dev" as a 2-pane split; it must win over the file.
        let cfg = cfg_with_session(r##"session "dev" { window "w" { split vertical { pane; pane } } }"##);
        let r = SessionRegistry::new();
        let s = r.attach_or_create("dev".into(), spec(), size(), Arc::clone(&cfg)).await.unwrap();
        {
            let wm = s.window_manager.lock().await;
            assert_eq!(
                wm.windows()[0].layout().panes().len(),
                2,
                "config template (2 panes) must win over the 1-pane saved file"
            );
            assert_eq!(wm.windows()[0].name, "w", "window name comes from the template, not the saved 'stale'");
        }
        s.terminate_panes().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn non_declared_name_unaffected_by_routing() {
        let _g = crate::test_env::isolate();
        let r = SessionRegistry::new();
        let cfg = cfg_with_session(r##"session "dev" { window "w" { pane } }"##);
        // "other" isn't declared, so this is a normal fresh create (1 pane from
        // `spec()`).
        let s = r.attach_or_create("other".into(), spec(), size(), Arc::clone(&cfg)).await.unwrap();
        {
            let wm = s.window_manager.lock().await;
            assert_eq!(wm.windows().len(), 1);
            assert_eq!(wm.windows()[0].layout().panes().len(), 1);
        }
        s.terminate_panes().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn closing_sessions_are_pruned_on_list() {
        let _g = crate::test_env::isolate();
        let r = SessionRegistry::new();
        let alive = r.create("alive".into(), spec(), size(), cfg()).await.unwrap();
        let dead = r.create("dead".into(), spec(), size(), cfg()).await.unwrap();
        dead.closing.store(true, std::sync::atomic::Ordering::SeqCst);
        let entries = r.list().await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "alive");
        // touch `alive` so the borrow checker doesn't complain about unused
        let _ = alive.name.clone();
    }
}
