//! Integration tests for multi-client `Session` behavior.

use plexy_glass_daemon::Session;
use plexy_glass_protocol::{PtySize, SpawnSpec};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

fn spec() -> SpawnSpec {
    SpawnSpec {
        program: "/bin/cat".into(),
        args: vec![],
        env: vec![],
        cwd: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn two_clients_effective_size_is_min() {
    let s = Session::new(
        "test".into(),
        spec(),
        PtySize { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
        Arc::new(plexy_glass_config::built_in_default()),
    )
    .unwrap();

    let s2 = Arc::clone(&s);
    let a = tokio::task::spawn_blocking(move || {
        s2.register_client(
            PtySize { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 },
            Arc::new(AtomicBool::new(false)),
        )
    })
    .await
    .unwrap()
    .unwrap();

    let s2 = Arc::clone(&s);
    let b = tokio::task::spawn_blocking(move || {
        s2.register_client(
            PtySize { rows: 20, cols: 60, pixel_width: 0, pixel_height: 0 },
            Arc::new(AtomicBool::new(false)),
        )
    })
    .await
    .unwrap()
    .unwrap();

    let s2 = Arc::clone(&s);
    let eff = tokio::task::spawn_blocking(move || s2.effective_size()).await.unwrap();
    assert_eq!((eff.rows, eff.cols), (20, 60));

    let s2 = Arc::clone(&s);
    let cid_b = b.client_id;
    tokio::task::spawn_blocking(move || s2.deregister_client(cid_b)).await.unwrap();

    let s2 = Arc::clone(&s);
    let eff = tokio::task::spawn_blocking(move || s2.effective_size()).await.unwrap();
    assert_eq!((eff.rows, eff.cols), (30, 100));

    let s2 = Arc::clone(&s);
    let cid_a = a.client_id;
    tokio::task::spawn_blocking(move || s2.deregister_client(cid_a)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_register_is_safe() {
    let s = Session::new(
        "test".into(),
        spec(),
        PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
        Arc::new(plexy_glass_config::built_in_default()),
    )
    .unwrap();

    let mut handles = Vec::new();
    for i in 0..8u16 {
        let s2 = Arc::clone(&s);
        let h = tokio::task::spawn_blocking(move || {
            s2.register_client(
                PtySize {
                    rows: 10 + i,
                    cols: 30 + i,
                    pixel_width: 0,
                    pixel_height: 0,
                },
                Arc::new(AtomicBool::new(false)),
            )
        });
        handles.push(h);
    }

    let mut client_ids = Vec::new();
    for h in handles {
        let handle = h.await.unwrap().unwrap();
        client_ids.push(handle.client_id);
    }
    assert_eq!(client_ids.len(), 8);

    let s2 = Arc::clone(&s);
    let eff = tokio::task::spawn_blocking(move || s2.effective_size()).await.unwrap();
    // smallest-client-wins: rows=10+0=10, cols=30+0=30
    assert_eq!((eff.rows, eff.cols), (10, 30));

    for id in client_ids {
        let s2 = Arc::clone(&s);
        tokio::task::spawn_blocking(move || s2.deregister_client(id)).await.unwrap();
    }
}
