use std::sync::Arc;
use tokio::sync::Mutex;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocking_lock_on_worker_thread() {
    let m = Arc::new(Mutex::new(0u32));
    let m2 = Arc::clone(&m);
    let h = tokio::spawn(async move {
        // Mirror `engine.rs`'s `spawn_tick_task`: a synchronous call at the top of the
        // task body that takes a `blocking_lock` on a tokio `Mutex`.
        let g1 = m2.blocking_lock();
        let g2 = m2.blocking_lock(); // (would deadlock if it ever got here twice; first should panic)
        drop(g2);
        drop(g1);
        42u32
    });
    match h.await {
        Ok(v) => println!("REPRO_RESULT: task completed ok value={v}"),
        Err(e) if e.is_panic() => println!("REPRO_RESULT: task PANICKED is_panic=true"),
        Err(e) => println!("REPRO_RESULT: task failed other err={e:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn blocking_lock_on_current_thread() {
    let m = Arc::new(Mutex::new(0u32));
    let m2 = Arc::clone(&m);
    let h = tokio::spawn(async move {
        let g1 = m2.blocking_lock();
        drop(g1);
        7u32
    });
    match h.await {
        Ok(v) => println!("REPRO_CURRENT: task completed ok value={v}"),
        Err(e) if e.is_panic() => println!("REPRO_CURRENT: task PANICKED is_panic=true"),
        Err(e) => println!("REPRO_CURRENT: task failed other err={e:?}"),
    }
}
