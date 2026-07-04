//! M7.3 —— 自建 Condvar：等待被通知（原书测试）
use forge_sync::condvar::Condvar;
use forge_sync::mutex::Mutex;
use std::thread;
use std::time::Duration;

#[test]
fn condvar_waits_then_notified() {
    let mutex = Mutex::new(0);
    let condvar = Condvar::new();
    let mut wakeups = 0usize;

    thread::scope(|s| {
        s.spawn(|| {
            thread::sleep(Duration::from_millis(200));
            *mutex.lock() = 123;
            condvar.notify_one();
        });
        let mut m = mutex.lock();
        while *m < 100 {
            m = condvar.wait(m);
            wakeups += 1;
        }
        assert_eq!(*m, 123);
    });
    // 真的睡了，而非忙等（允许少量假唤醒）
    assert!(wakeups < 10, "应当真正睡眠，wakeups={wakeups}");
}
