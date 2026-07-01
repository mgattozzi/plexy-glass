//! Daemon-wide registry of named sessions.

use crate::paste_buffers::PasteBufferStore;
use crate::{LockExt, error::DaemonError, session::Session};
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
    /// Set when the config failed to load (boot or reload) and the daemon is
    /// running on built-in defaults. Surfaced on the next attach so the failure
    /// isn't invisible; cleared by a clean reload. A plain sync mutex, since
    /// accesses are brief and await-free.
    config_error: std::sync::Mutex<Option<String>>,
    /// True until the one-time welcome modal has been shown once this daemon
    /// lifetime. Gated additionally by `config.welcome` (the user's on/off knob);
    /// in-memory only, so a fresh daemon shows it once again. No on-disk marker.
    welcome_pending: std::sync::atomic::AtomicBool,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            paste_buffers: Mutex::new(PasteBufferStore::new(PASTE_BUFFER_CAP)),
            config_error: std::sync::Mutex::new(None),
            welcome_pending: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// Consume the one-time welcome slot: returns `true` exactly once per
    /// daemon lifetime (the first caller), `false` thereafter.
    ///
    /// The attach path gates this behind `config.welcome` so a disabled
    /// welcome never consumes it.
    pub fn take_welcome(&self) -> bool {
        self.welcome_pending.swap(false, std::sync::atomic::Ordering::Relaxed)
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
        self.paste_buffers.lock().await.top().map(|b| b.content.clone())
    }

    /// Clone out a named buffer's content, if present.
    pub async fn paste_buffer_get(&self, name: &str) -> Option<Vec<u8>> {
        self.paste_buffers.lock().await.get(name).map(|b| b.content.clone())
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
        map.retain(|_, s| !s.closing.load(std::sync::atomic::Ordering::SeqCst));
        // `list_entry` takes blocking locks, so defer via `block_in_place`. No
        // per-session `Arc::clone` needed, `block_in_place` runs inline and
        // `list_entry` only needs a shared borrow.
        let mut out: Vec<SessionEntry> = map
            .values()
            .map(|s| tokio::task::block_in_place(|| s.list_entry()))
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

    /// Build every config-declared session that is NOT already live, from its
    /// template (Feature B / declarative v2).
    ///
    /// Idempotent: a name already live is skipped by `create_declared`'s
    /// short-circuit, so this never rebuilds a running session and never races
    /// a concurrent attach of the same name. A per-session build failure is
    /// logged and skipped, it never aborts the rest. Shared by daemon boot and
    /// `reload_config`; both pass the 24×80 default `size` (resized when a
    /// client later attaches).
    pub async fn build_declared(
        &self,
        config: &Arc<plexy_glass_config::Config>,
        size: PtySize,
    ) {
        for template in &config.sessions {
            match self.create_declared(template, Arc::clone(config), size).await {
                Ok(_) => tracing::info!(session = %template.name, "built declared session"),
                Err(e) => {
                    tracing::warn!(session = %template.name, error = %e, "skipping declared session")
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
        validate_name(&name)?;
        // Fast path: already running. A still-mapped but *closing* entry must be
        // PRUNED here (mirroring `get`), not merely skipped, otherwise it
        // lingers in the map and the fresh-create fallback below is rejected by
        // `create`'s contains_key check (SessionAlreadyExists). This is the
        // default `plexy-glass` (no args) re-launch after exiting the sole
        // "main" session: it calls attach_or_create directly with no preceding
        // `get` to do the pruning.
        {
            let mut map = self.inner.lock().await;
            let closing = match map.get(&name) {
                Some(s) => {
                    if !s.closing.load(std::sync::atomic::Ordering::SeqCst) {
                        return Ok(Arc::clone(s));
                    }
                    true
                }
                None => false,
            };
            if closing {
                map.remove(&name);
            }
        }
        // A declared session name is (re)built from its template. (The client
        // `cmd` is intentionally unused, declared panes come from the template +
        // the daemon default shell.)
        if let Some(template) = config.sessions.iter().find(|t| t.name == name) {
            return self.create_declared(template, Arc::clone(&config), size).await;
        }
        // The daemon is memory-only, there is no on-disk restore, so build fresh.
        self.create(name, cmd, size, config).await
    }

    /// Rename a live session: re-key the map and update the session's live name
    /// under ONE map-lock hold (so a concurrent `get`/`create` can never observe
    /// the key and the live name disagreeing). Memory-only, nothing on disk.
    pub async fn rename_session(self: &Arc<Self>, old: &str, new: &str) -> Result<(), DaemonError> {
        validate_name(new)?;
        let mut map = self.inner.lock().await;
        if map.contains_key(new) {
            return Err(DaemonError::Protocol(ProtocolError::SessionAlreadyExists {
                name: new.to_string(),
            }));
        }
        let session = map.remove(old).ok_or_else(|| {
            DaemonError::Protocol(ProtocolError::SessionNotFound { name: old.to_string() })
        })?;
        // Key and live name move together, under the same lock hold.
        session.set_name(new.to_string());
        map.insert(new.to_string(), session);
        Ok(())
    }

    pub async fn kill(&self, name: &str) -> Result<(), DaemonError> {
        let session = {
            let mut map = self.inner.lock().await;
            map.remove(name).ok_or_else(|| {
                DaemonError::Protocol(ProtocolError::SessionNotFound { name: name.to_string() })
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
        let reload_size = PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 };
        self.build_declared(&new_config, reload_size).await;
        // A clean reload clears the error state; a failing one refreshes it (the
        // daemon is still on defaults). Keeps the attach notice honest.
        self.set_config_error(err.as_ref().map(|e| e.to_string()));
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
        let r = Arc::new(SessionRegistry::new());
        let s = r.create("main".into(), spec(), size(), cfg()).await.unwrap();
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
        assert!(r.take_welcome(), "first attach to a fresh daemon shows the welcome");
        assert!(!r.take_welcome(), "shown once — subsequent attaches do not");
        assert!(!r.take_welcome(), "stays false for this daemon lifetime");
    }

    #[tokio::test]
    async fn duplicate_create_fails() {
        let _g = crate::test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        r.create("main".into(), spec(), size(), cfg()).await.unwrap();
        let err =
            r.create("main".into(), spec(), size(), cfg()).await.map(|_| ()).unwrap_err();
        assert!(matches!(err, DaemonError::Protocol(ProtocolError::SessionAlreadyExists { .. })));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_returns_sorted_entries() {
        let _g = crate::test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
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
        let r = Arc::new(SessionRegistry::new());
        let err = r.kill("ghost").await.unwrap_err();
        assert!(matches!(err, DaemonError::Protocol(ProtocolError::SessionNotFound { .. })));
    }

    #[tokio::test]
    async fn name_validation_rejects_empty() {
        let _g = crate::test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        let err = r.create("".into(), spec(), size(), cfg()).await.map(|_| ()).unwrap_err();
        assert!(matches!(err, DaemonError::Protocol(ProtocolError::EmptyName)));
    }

    #[tokio::test]
    async fn name_validation_rejects_invalid_chars() {
        let _g = crate::test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
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
        let r = Arc::new(SessionRegistry::new());
        let s = r.create("dead".into(), spec(), size(), cfg()).await.unwrap();
        s.closing.store(true, std::sync::atomic::Ordering::SeqCst);
        let got = r.get("dead").await;
        assert!(got.is_none(), "closing session should be pruned on get");
    }

    #[tokio::test]
    async fn attach_or_create_replaces_a_closing_entry() {
        let _g = crate::test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        let s = r.create("main".into(), spec(), size(), cfg()).await.unwrap();
        // Mark closing DIRECTLY (not via get/list, which would prune it) to
        // reproduce the lingering-entry state after a natural session close.
        s.closing.store(true, std::sync::atomic::Ordering::SeqCst);
        // The default `plexy-glass` (no args) relaunch calls `attach_or_create`
        // with no preceding get, so it must yield a fresh live session rather
        // than failing with `SessionAlreadyExists` on the stale closing entry.
        let fresh = r
            .attach_or_create("main".into(), spec(), size(), cfg())
            .await
            .expect("must replace a closing entry, not error");
        assert!(!fresh.closing.load(std::sync::atomic::Ordering::SeqCst));
        assert!(r.get("main").await.is_some());
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn reload_config_swaps_session_config() {
        let _g = crate::test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        let s = r.create("test".into(), spec(), size(), cfg()).await.unwrap();
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
    #[tokio::test]
    async fn rename_session_rekeys_map_and_live_name() {
        let _g = crate::test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        let s = r.create("before".into(), spec(), size(), cfg()).await.unwrap();
        r.rename_session("before", "after").await.unwrap();
        assert!(r.get("before").await.is_none(), "old key must be gone");
        let got = r.get("after").await.expect("new key resolves");
        assert!(Arc::ptr_eq(&got, &s), "same session Arc under the new key");
        assert_eq!(s.name(), "after", "live name follows the map key");
    }
    #[tokio::test]
    async fn rename_session_live_collision_errors() {
        let _g = crate::test_env::isolate();
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
        let _g = crate::test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        r.create("a".into(), spec(), size(), cfg()).await.unwrap();
        let err = r.rename_session("a", "has space").await.unwrap_err();
        assert!(matches!(err, DaemonError::Protocol(ProtocolError::InvalidName { .. })));
        assert!(r.get("a").await.is_some(), "failed rename leaves the session keyed as before");
    }

    #[tokio::test]
    async fn rename_session_unknown_old_errors() {
        let _g = crate::test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        let err = r.rename_session("ghost", "x").await.unwrap_err();
        assert!(matches!(err, DaemonError::Protocol(ProtocolError::SessionNotFound { .. })));
    }

    #[tokio::test]
    async fn rename_session_rekeys_to_new_name() {
        let _g = crate::test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        r.create("fresh".into(), spec(), size(), cfg()).await.unwrap();
        r.rename_session("fresh", "moved").await.expect("rename succeeds");
        assert!(r.get("moved").await.is_some());
        assert!(r.get("fresh").await.is_none());
    }
    fn cfg_with_session(kdl: &str) -> Arc<plexy_glass_config::Config> {
        Arc::new(plexy_glass_config::parse_config(kdl).expect("declared-session config"))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_declared_builds_template() {
        let _g = crate::test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        let cfg = cfg_with_session(r##"session "dev" { window "w" { pane } }"##);
        let s = r.create_declared(&cfg.sessions[0], Arc::clone(&cfg), size()).await.unwrap();
        assert_eq!(s.name(), "dev");
        assert!(r.get("dev").await.is_some());
        s.terminate_panes().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn build_declared_builds_all_new_names() {
        let _g = crate::test_env::isolate();
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
        let _g = crate::test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
        let cfg_v1 = cfg_with_session(r##"session "dev" { window "w" { split vertical { pane; pane } } }"##);
        let live = r.create_declared(&cfg_v1.sessions[0], Arc::clone(&cfg_v1), size()).await.unwrap();
        let live_ptr = Arc::as_ptr(&live);
        {
            let wm = live.window_manager.lock().await;
            assert_eq!(wm.windows()[0].layout().panes().len(), 2);
        }
        // A reloaded config with a CHANGED (1-pane) "dev" template.
        let cfg_v2 = cfg_with_session(r##"session "dev" { window "w" { pane } }"##);
        r.build_declared(&cfg_v2, size()).await;
        let still = r.get("dev").await.expect("dev still live");
        assert!(std::ptr::eq(Arc::as_ptr(&still), live_ptr), "same session Arc (not rebuilt)");
        {
            let wm = still.window_manager.lock().await;
            assert_eq!(wm.windows()[0].layout().panes().len(), 2, "live shape unchanged by reload");
        }
        live.terminate_panes().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn attach_or_create_routes_declared_name_to_template() {
        let _g = crate::test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
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
    async fn non_declared_name_unaffected_by_routing() {
        let _g = crate::test_env::isolate();
        let r = Arc::new(SessionRegistry::new());
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
        let r = Arc::new(SessionRegistry::new());
        let alive = r.create("alive".into(), spec(), size(), cfg()).await.unwrap();
        let dead = r.create("dead".into(), spec(), size(), cfg()).await.unwrap();
        dead.closing.store(true, std::sync::atomic::Ordering::SeqCst);
        let entries = r.list().await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "alive");
        // touch `alive` so the borrow checker doesn't complain about unused
        let _ = alive.name();
    }
}
