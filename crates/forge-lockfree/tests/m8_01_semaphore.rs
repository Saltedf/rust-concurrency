//! M8.1 —— 信号量限制并发数 ≤ 许可数
use forge_lockfree::semaphore::Semaphore;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

#[test]
fn semaphore_limits_concurrency() {
    let sem = Arc::new(Semaphore::new(2));
    let active = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));

    thread::scope(|s| {
        for _ in 0..8 {
            let sem = sem.clone();
            let active = active.clone();
            let max_seen = max_seen.clone();
            s.spawn(move || {
                for _ in 0..200 {
                    sem.acquire();
                    let a = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(a, Ordering::SeqCst);
                    thread::yield_now();
                    active.fetch_sub(1, Ordering::SeqCst);
                    sem.release();
                }
            });
        }
    });
    assert!(
        max_seen.load(Ordering::SeqCst) <= 2,
        "并发不应超过 2 个许可"
    );
}
