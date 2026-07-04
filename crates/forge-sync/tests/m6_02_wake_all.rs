//! M6.2 —— wake_all 唤醒所有等待者
use forge_sync::atomic_wait::{wait, wake_all};
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[test]
fn wake_all_wakes_every_waiter() {
    let a = AtomicU32::new(0);
    let done = Arc::new(AtomicU32::new(0));

    thread::scope(|s| {
        for _ in 0..3 {
            let done = done.clone();
            let a = &a;
            s.spawn(move || {
                while a.load(Relaxed) == 0 {
                    wait(a, 0);
                }
                done.fetch_add(1, Relaxed);
            });
        }
        // 给三个线程一点时间进入等待
        thread::sleep(Duration::from_millis(100));
        a.store(1, Relaxed);
        wake_all(&a);
    });

    assert_eq!(done.load(Relaxed), 3, "三个等待者都应被唤醒");
}
