//! M7.4 —— 自建 RwLock：写者互斥（4 线程各加 1000 → 4000）
use forge_sync::rwlock::RwLock;
use std::thread;

#[test]
fn rwlock_writers_are_exclusive() {
    let lock = RwLock::new(0i64);
    thread::scope(|s| {
        for _ in 0..4 {
            let lock = &lock;
            s.spawn(move || {
                for _ in 0..1000 {
                    *lock.write() += 1;
                }
            });
        }
    });
    assert_eq!(*lock.read(), 4000);
}
