//! A named session: a WindowManager + attached clients + broadcasting renderer.

use crate::{error::DaemonError, window_manager::WindowManager};
use plexy_glass_mux::{PaneId, VirtualScreen};
use plexy_glass_protocol::{ProtocolError, PtySize, SessionEntry, SpawnSpec};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::{mpsc, watch, Mutex, Notify};
use tokio::task::JoinHandle;

pub struct ClientHandle {
    pub client_id: u64,
    pub size: PtySize,
    pub frame_rx: watch::Receiver<Arc<VirtualScreen>>,
}

pub struct Session {
    pub name: String,
    pub created: SystemTime,
    pub window_manager: Mutex<WindowManager>,
    pub clients: Mutex<Vec<ClientHandle>>,
    pub notify: Arc<Notify>,
    pub frame_tx: watch::Sender<Arc<VirtualScreen>>,
    pub death_tx: mpsc::Sender<PaneId>,
    pub closing: AtomicBool,
    next_client_id: AtomicU64,
    // Task 7 will take the coordinator handle; unused skeleton until then.
    #[allow(dead_code)]
    coordinator_handle: Mutex<Option<JoinHandle<()>>>,
    /// Holds the death channel receiver until Task 13 wires up the consumer.
    pub pending_death_rx: Mutex<Option<mpsc::Receiver<PaneId>>>,
}

impl Session {
    pub fn new(
        name: String,
        initial_cmd: SpawnSpec,
        first_size: PtySize,
    ) -> Result<Arc<Self>, DaemonError> {
        let notify = Arc::new(Notify::new());
        let (death_tx, death_rx) = mpsc::channel::<PaneId>(16);
        let window_manager = WindowManager::new(
            initial_cmd,
            first_size,
            Arc::clone(&notify),
            Some(death_tx.clone()),
        )?;
        let initial_frame = Arc::new(VirtualScreen::blank(first_size.rows, first_size.cols));
        let (frame_tx, _) = watch::channel(initial_frame);
        let session = Arc::new(Self {
            name,
            created: SystemTime::now(),
            window_manager: Mutex::new(window_manager),
            clients: Mutex::new(Vec::new()),
            notify,
            frame_tx,
            death_tx,
            closing: AtomicBool::new(false),
            next_client_id: AtomicU64::new(0),
            coordinator_handle: Mutex::new(None),
            pending_death_rx: Mutex::new(Some(death_rx)),
        });
        // Coordinator task is spawned in Task 7. For now, no-op.
        // Death channel handling is wired in Task 13.
        Ok(session)
    }

    pub fn list_entry(&self) -> SessionEntry {
        let m = self.window_manager.blocking_lock();
        let clients = self.clients.blocking_lock().len() as u8;
        let windows = m.windows().len() as u8;
        let panes = m
            .windows()
            .iter()
            .map(|w| w.layout().panes().len() as u8)
            .sum();
        SessionEntry {
            name: self.name.clone(),
            windows,
            panes,
            clients,
            created: self.created,
        }
    }

    pub fn register_client(self: &Arc<Self>, size: PtySize) -> Result<ClientHandle, DaemonError> {
        if self.closing.load(Ordering::SeqCst) {
            return Err(DaemonError::Protocol(ProtocolError::SessionNotFound {
                name: self.name.clone(),
            }));
        }
        let client_id = self.next_client_id.fetch_add(1, Ordering::SeqCst);
        let frame_rx_for_caller = self.frame_tx.subscribe();
        let frame_rx_for_session = self.frame_tx.subscribe();
        {
            let mut clients = self.clients.blocking_lock();
            clients.push(ClientHandle {
                client_id,
                size,
                frame_rx: frame_rx_for_session,
            });
        }
        self.recompute_size_and_notify();
        Ok(ClientHandle {
            client_id,
            size,
            frame_rx: frame_rx_for_caller,
        })
    }

    pub fn deregister_client(&self, client_id: u64) {
        {
            let mut clients = self.clients.blocking_lock();
            clients.retain(|c| c.client_id != client_id);
        }
        self.recompute_size_and_notify();
    }

    pub fn effective_size(&self) -> PtySize {
        let clients = self.clients.blocking_lock();
        if clients.is_empty() {
            let m = self.window_manager.blocking_lock();
            return m.host_size();
        }
        let rows = clients.iter().map(|c| c.size.rows).min().unwrap_or(1);
        let cols = clients.iter().map(|c| c.size.cols).min().unwrap_or(1);
        let pw = clients.iter().map(|c| c.size.pixel_width).min().unwrap_or(0);
        let ph = clients.iter().map(|c| c.size.pixel_height).min().unwrap_or(0);
        PtySize { rows, cols, pixel_width: pw, pixel_height: ph }
    }

    fn recompute_size_and_notify(&self) {
        let new_size = self.effective_size();
        let mut m = self.window_manager.blocking_lock();
        if m.host_size() != new_size {
            let _ = m.on_host_resize(new_size);
        }
        drop(m);
        self.notify.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plexy_glass_protocol::SpawnSpec;
    use std::sync::atomic::Ordering;

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

    #[tokio::test]
    async fn session_construct_succeeds() {
        let s = Session::new("main".into(), spec(), size()).expect("construct session");
        assert_eq!(s.name, "main");
        assert!(!s.closing.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn list_entry_reports_one_window_one_pane_zero_clients() {
        let s = Session::new("main".into(), spec(), size()).unwrap();
        let entry = tokio::task::spawn_blocking(move || s.list_entry()).await.unwrap();
        assert_eq!(entry.name, "main");
        assert_eq!(entry.windows, 1);
        assert_eq!(entry.panes, 1);
        assert_eq!(entry.clients, 0);
    }

    #[tokio::test]
    async fn register_then_effective_size_matches_single_client() {
        let s = Session::new("main".into(), spec(), size()).unwrap();
        let s2 = Arc::clone(&s);
        let h = tokio::task::spawn_blocking(move || {
            s2.register_client(PtySize { rows: 10, cols: 30, pixel_width: 0, pixel_height: 0 })
        })
        .await
        .unwrap()
        .unwrap();
        let s2 = Arc::clone(&s);
        let eff = tokio::task::spawn_blocking(move || s2.effective_size()).await.unwrap();
        assert_eq!((eff.rows, eff.cols), (10, 30));
        let s2 = Arc::clone(&s);
        let cid = h.client_id;
        tokio::task::spawn_blocking(move || s2.deregister_client(cid)).await.unwrap();
    }

    #[tokio::test]
    async fn smallest_client_wins() {
        let s = Session::new("main".into(), spec(), size()).unwrap();
        let s2 = Arc::clone(&s);
        let a = tokio::task::spawn_blocking(move || {
            s2.register_client(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
        })
        .await
        .unwrap()
        .unwrap();
        let s2 = Arc::clone(&s);
        let b = tokio::task::spawn_blocking(move || {
            s2.register_client(PtySize { rows: 10, cols: 30, pixel_width: 0, pixel_height: 0 })
        })
        .await
        .unwrap()
        .unwrap();
        let s2 = Arc::clone(&s);
        let eff = tokio::task::spawn_blocking(move || s2.effective_size()).await.unwrap();
        assert_eq!((eff.rows, eff.cols), (10, 30));
        let s2 = Arc::clone(&s);
        let cid_b = b.client_id;
        tokio::task::spawn_blocking(move || s2.deregister_client(cid_b)).await.unwrap();
        let s2 = Arc::clone(&s);
        let eff2 = tokio::task::spawn_blocking(move || s2.effective_size()).await.unwrap();
        assert_eq!((eff2.rows, eff2.cols), (24, 80));
        let s2 = Arc::clone(&s);
        let cid_a = a.client_id;
        tokio::task::spawn_blocking(move || s2.deregister_client(cid_a)).await.unwrap();
    }

    #[tokio::test]
    async fn closing_session_refuses_register() {
        let s = Session::new("main".into(), spec(), size()).unwrap();
        s.closing.store(true, Ordering::SeqCst);
        let s2 = Arc::clone(&s);
        let result =
            tokio::task::spawn_blocking(move || s2.register_client(size())).await.unwrap();
        assert!(result.is_err());
    }
}
