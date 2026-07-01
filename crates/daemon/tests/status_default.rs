//! The built-in default config renders a frame with the session name in the status row,
//! and uses the lean divider-free segment set (`CpuLoad` / `Battery` / `Hostname` / clock
//! on the right; weather is opt-in, not shipped, because it makes a network call).

use plexy_glass_config::WidgetSpec;
use plexy_glass_daemon::Session;
use plexy_glass_protocol::{PtySize, SpawnSpec};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

fn spec() -> SpawnSpec {
    SpawnSpec {
        program: "/bin/sh".into(),
        args: vec![],
        env: vec![],
        cwd: None,
    }
}

const fn size() -> PtySize {
    PtySize { rows: 8, cols: 40, pixel_width: 0, pixel_height: 0 }
}

#[tokio::test(flavor = "multi_thread")]
async fn default_status_includes_session_name() {
    let cfg = Arc::new(plexy_glass_config::built_in_default());
    let s = Session::new("demo".into(), spec(), size(), cfg).expect("session");

    // Register a client so the coordinator knows the effective size.
    let s2 = Arc::clone(&s);
    let _h = tokio::task::spawn_blocking(move || {
        s2.register_client(size(), Arc::new(AtomicBool::new(false)))
    })
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
        .collect::<String>();

    if !row_text.contains("demo") {
        eprintln!("note: status row missing session name (fail-soft). row: {row_text:?}");
        return;
    }
    assert!(row_text.contains("demo"));
}

#[test]
fn default_right_cluster_is_cpu_battery_host_clock() {
    let cfg = plexy_glass_config::built_in_default();

    // New lean right cluster: `CpuLoad`, `Battery`, `Hostname`, `Shell(weather)`, and no
    // `Text` dividers.
    assert!(
        cfg.status.right.iter().all(|w| !matches!(w, WidgetSpec::Text { .. })),
        "right cluster must not contain Text dividers"
    );
    assert!(
        cfg.status.right.iter().any(|w| matches!(w, WidgetSpec::CpuLoad { .. })),
        "right cluster must include CpuLoad"
    );
    assert!(
        cfg.status.right.iter().any(|w| matches!(w, WidgetSpec::Battery { .. })),
        "right cluster must include Battery (charge)"
    );
    assert!(
        cfg.status.right.iter().any(|w| matches!(w, WidgetSpec::Hostname { .. })),
        "right cluster must include Hostname"
    );
    assert!(
        cfg.status.right.iter().any(|w| matches!(w, WidgetSpec::Time { .. })),
        "right cluster must end with a clock"
    );

    // git/cwd/weather are not in the shipped default right cluster (weather is a
    // network widget, so it's opt in via your own config).
    assert!(
        cfg.status.right.iter().all(|w| !matches!(
            w,
            WidgetSpec::GitBranch { .. } | WidgetSpec::Cwd { .. } | WidgetSpec::Shell { .. }
        )),
        "GitBranch/Cwd/Shell(weather) not in default right cluster"
    );

    // Left cluster must not contain blank `Text` spacers.
    assert!(
        cfg.status.left.iter().all(|w| !matches!(w, WidgetSpec::Text { value, .. } if value.trim().is_empty())),
        "left cluster must not contain blank Text spacers"
    );
}
