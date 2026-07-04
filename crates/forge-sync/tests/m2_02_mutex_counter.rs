//! M2.2 —— Mutex：10 线程各加 100，最终恰好 1000
use std::sync::Mutex;
use std::thread;

#[test]
fn mutex_makes_increments_atomic() {
    let n = Mutex::new(0);
    thread::scope(|s| {
        for _ in 0..10 {
            s.spawn(|| {
                let mut guard = n.lock().unwrap();
                for _ in 0..100 {
                    *guard += 1;
                }
                // guard 在此 drop，解锁——保持锁定时间尽可能短
            });
        }
    });
    assert_eq!(n.into_inner().unwrap(), 1000);
}
