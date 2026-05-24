//! Daemon-wide registry of named sessions.

use crate::{error::DaemonError, session::Session};
use plexy_glass_protocol::{ProtocolError, PtySize, SessionEntry, SpawnSpec};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct SessionRegistry {
    inner: Mutex<HashMap<String, Arc<Session>>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self { inner: Mutex::new(HashMap::new()) }
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
        let mut map = self.inner.lock().await;
        let session = map.remove(name).ok_or_else(|| {
            DaemonError::Protocol(ProtocolError::SessionNotFound { name: name.to_string() })
        })?;
        session.closing.store(true, std::sync::atomic::Ordering::SeqCst);
        session.notify.notify_one();
        Ok(())
    }

    pub async fn prune_empty(&self) {
        let mut map = self.inner.lock().await;
        map.retain(|_, s| !s.closing.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// Re-read config from disk and apply to every session.
    ///
    /// The TOML loader (`plexy_glass_config::load_or_default`) returns
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
        let r = SessionRegistry::new();
        let s = r.create("main".into(), spec(), size(), cfg()).await.unwrap();
        assert_eq!(s.name, "main");
        let got = r.get("main").await.unwrap();
        assert_eq!(got.name, "main");
    }

    #[tokio::test]
    async fn duplicate_create_fails() {
        let r = SessionRegistry::new();
        r.create("main".into(), spec(), size(), cfg()).await.unwrap();
        let err =
            r.create("main".into(), spec(), size(), cfg()).await.map(|_| ()).unwrap_err();
        assert!(matches!(err, DaemonError::Protocol(ProtocolError::SessionAlreadyExists { .. })));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_returns_sorted_entries() {
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
        let r = SessionRegistry::new();
        let err = r.kill("ghost").await.unwrap_err();
        assert!(matches!(err, DaemonError::Protocol(ProtocolError::SessionNotFound { .. })));
    }

    #[tokio::test]
    async fn name_validation_rejects_empty() {
        let r = SessionRegistry::new();
        let err = r.create("".into(), spec(), size(), cfg()).await.map(|_| ()).unwrap_err();
        assert!(matches!(err, DaemonError::Protocol(ProtocolError::EmptyName)));
    }

    #[tokio::test]
    async fn name_validation_rejects_invalid_chars() {
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
        let r = SessionRegistry::new();
        let s = r.create("dead".into(), spec(), size(), cfg()).await.unwrap();
        s.closing.store(true, std::sync::atomic::Ordering::SeqCst);
        let got = r.get("dead").await;
        assert!(got.is_none(), "closing session should be pruned on get");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reload_config_swaps_session_config() {
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
    async fn closing_sessions_are_pruned_on_list() {
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
