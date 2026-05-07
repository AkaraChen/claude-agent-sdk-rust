use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use claude_agent_sdk::internal::task_compat::spawn_detached;
use tokio::time::{Duration, sleep};

#[tokio::test]
async fn spawn_and_wait() {
    let flag = Arc::new(AtomicBool::new(false));
    let flag_for_task = flag.clone();

    let handle = spawn_detached(async move {
        flag_for_task.store(true, Ordering::SeqCst);
    });

    handle.wait().await.unwrap();
    assert!(flag.load(Ordering::SeqCst));
    assert!(handle.done());
}

#[tokio::test]
async fn cancel() {
    let flag = Arc::new(AtomicBool::new(false));
    let flag_for_task = flag.clone();

    let handle = spawn_detached(async move {
        sleep(Duration::from_secs(60)).await;
        flag_for_task.store(true, Ordering::SeqCst);
    });

    sleep(Duration::from_millis(1)).await;
    handle.cancel();
    handle.wait().await.unwrap();
    assert!(!flag.load(Ordering::SeqCst));
    assert!(handle.done());
}

#[tokio::test]
async fn done_callback() {
    let fired = Arc::new(AtomicUsize::new(0));
    let fired_for_callback = fired.clone();
    let handle = spawn_detached(async {});

    handle.add_done_callback(move |task| {
        if task.done() {
            fired_for_callback.fetch_add(1, Ordering::SeqCst);
        }
    });

    handle.wait().await.unwrap();
    assert_eq!(fired.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn panic_propagates_via_wait() {
    let handle = spawn_detached(async {
        panic!("boom");
    });

    let err = handle.wait().await.unwrap_err();
    assert!(err.contains("panicked"));
}
