//! M3.1 —— SpinLock 保护计数器：10 线程各加 100，最终恰好 1000
use forge_core::spin::SpinLock;
use std::thread;

#[test]
fn spinlock_protects_counter() {
    let n = SpinLock::new(0i64);
    thread::scope(|s| {
        for _ in 0..10 {
            s.spawn(|| {
                let mut g = n.lock();
                for _ in 0..100 {
                    *g += 1;
                }
            });
        }
    });
    assert_eq!(*n.lock(), 1000);
}
