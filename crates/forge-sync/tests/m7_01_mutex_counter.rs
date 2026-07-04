//! M7.1 —— 自建 Mutex：10 线程各加 100，最终 1000（验证互斥 + happens-before）
use forge_sync::mutex::Mutex;
use std::thread;

#[test]
fn mutex_protects_counter() {
    let n = Mutex::new(0i64);
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
