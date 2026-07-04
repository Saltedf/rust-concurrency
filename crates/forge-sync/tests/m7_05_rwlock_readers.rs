//! M7.5 —— 自建 RwLock：多读者并行读
use forge_sync::rwlock::RwLock;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::thread;

#[test]
fn rwlock_concurrent_readers() {
    let lock = RwLock::new(vec![1, 2, 3]);
    let sum = Arc::new(AtomicI64::new(0));
    thread::scope(|s| {
        for _ in 0..8 {
            let lock = &lock;
            let sum = sum.clone();
            s.spawn(move || {
                let g = lock.read();
                sum.fetch_add(g.iter().sum::<i32>() as i64, Ordering::Relaxed);
            });
        }
    });
    assert_eq!(sum.load(Ordering::Relaxed), 8 * 6); // 8 × (1+2+3)
}
