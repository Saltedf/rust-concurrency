//! M8.4 —— MCS 队列锁：互斥（8 线程各 1000 → 8000）
use forge_lockfree::mcs::McsLock;
use std::sync::atomic::{AtomicI64, Ordering};
use std::thread;

#[test]
fn mcs_lock_mutual_exclusion() {
    let lock = McsLock::new();
    let counter = AtomicI64::new(0);
    thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                for _ in 0..1000 {
                    let _g = lock.lock();
                    counter.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });
    assert_eq!(counter.load(Ordering::Relaxed), 8000);
}
