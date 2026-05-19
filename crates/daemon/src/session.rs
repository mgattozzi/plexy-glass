//! A named session: a WindowManager + attached clients + broadcasting renderer.

use crate::{error::DaemonError, window_manager::WindowManager};
use plexy_glass_mux::{PaneId, VirtualScreen};
use plexy_glass_protocol::{PtySize, SessionEntry, SpawnSpec};
use std::sync::atomic::{AtomicBool, AtomicU64};
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
    // Task 7 will read `next_client_id` when issuing client IDs.
    #[allow(dead_code)]
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
}
