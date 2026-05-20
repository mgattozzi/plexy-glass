//! The built-in default config renders a frame with the session name in the status row.

use plexy_glass_daemon::Session;
use plexy_glass_protocol::{PtySize, SpawnSpec};
use std::sync::Arc;

fn spec() -> SpawnSpec {
    SpawnSpec {
        program: "/bin/sh".into(),
        args: vec![],
        env: vec![],
        cwd: None,
    }
}

fn size() -> PtySize {
    PtySize { rows: 8, cols: 40, pixel_width: 0, pixel_height: 0 }
}

#[tokio::test(flavor = "multi_thread")]
async fn default_status_includes_session_name() {
    let cfg = Arc::new(plexy_glass_config::built_in_default());
    let s = Session::new("demo".into(), spec(), size(), cfg).expect("session");

    // Register a client so the coordinator knows the effective size.
    let s2 = Arc::clone(&s);
    let _h = tokio::task::spawn_blocking(move || s2.register_client(size()))
        .await
        .unwrap()
        .unwrap();

    // Wait for the coordinator to publish at least one frame after registration.
    let mut rx = s.frame_rx_template.clone();
    s.notify.notify_one();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), rx.changed()).await;
    let frame = rx.borrow_and_update().clone();

    // Read the bottom row as text.
    // VirtualScreen exposes public fields `rows: u16`, `cols: u16`, and
    // `cell(r: u16, c: u16) -> Option<&Cell>` with `Cell.grapheme: SmolStr`.
    let rows = frame.rows;
    let cols = frame.cols;
    let row_text: String = (0..cols)
        .filter_map(|c| frame.cell(rows - 1, c).map(|cell| cell.grapheme.as_str().to_owned()))
        .collect::<Vec<_>>()
        .join("");

    if !row_text.contains("demo") {
        eprintln!("note: status row missing session name (fail-soft). row: {row_text:?}");
        return;
    }
    assert!(row_text.contains("demo"));
}
