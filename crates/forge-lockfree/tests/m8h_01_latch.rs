//! M8h Latch 测试：倒计数门闩。

use forge_lockfree::latch::Latch;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

#[test]
fn latch_zero_is_open() {
    let l = Latch::new(0);
    assert!(l.is_open());
    l.wait(); // 立刻返回
}

#[test]
fn latch_basic_wait_and_countdown() {
    let l = Arc::new(Latch::new(4));
    let done = Arc::new(AtomicUsize::new(0));

    // 4 个 worker：每个干一点活，count_down。
    let mut handles = vec![];
    for _ in 0..4 {
        let l = l.clone();
        let done = done.clone();
        handles.push(std::thread::spawn(move || {
            // 模拟工作
            done.fetch_add(1, Ordering::Relaxed);
            l.count_down();
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    // 主线程：wait 应该立刻返回（因为 4 个都 count_down 了）。
    l.wait();
    assert_eq!(done.load(Ordering::Relaxed), 4);
    assert!(l.is_open());
    assert_eq!(l.count(), 0);
}

#[test]
fn latch_blocks_until_zero() {
    // 验证：count_down 不足时，wait 会真的阻塞。
    // 我们用 channel 把"waiter 已返回"的消息发回主线程，
    // 主线程在限定时间内 recv_timeout 应当超时——证明 waiter 还卡在 wait。
    let (tx, rx) = std::sync::mpsc::channel();
    let l = Arc::new(Latch::new(3));
    let l2 = l.clone();

    let _waiter = std::thread::spawn(move || {
        l2.wait();
        // 这一行只有被 wake 才会执行。
        let _ = tx.send("released");
    });

    // 只 count_down 2 次，wait 应该还在睡。
    l.count_down();
    l.count_down();

    // 在 100ms 内等消息——应该超时（Err）。
    let res = rx.recv_timeout(std::time::Duration::from_millis(100));
    assert!(
        res.is_err(),
        "count_down 不足时 waiter 不应返回，但收到了: {:?}",
        res
    );

    // 减第三次，count -> 0，wake_all。
    l.count_down();
    // 现在应该很快收到。
    let res2 = rx.recv_timeout(std::time::Duration::from_secs(2));
    assert_eq!(res2.unwrap(), "released");
}

#[test]
fn latch_release_wakes_blocked_waiter() {
    let l = Arc::new(Latch::new(2));
    let l2 = l.clone();

    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let started2 = started.clone();

    let waiter = std::thread::spawn(move || {
        started2.store(true, Ordering::SeqCst);
        l2.wait();
    });

    // 等 waiter 进入 wait。
    while !started.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    std::thread::sleep(std::time::Duration::from_millis(20));

    // 还剩 2，waiter 应在睡。
    l.count_down(); // -> 1
    std::thread::sleep(std::time::Duration::from_millis(20));
    l.count_down(); // -> 0, wake_all
    waiter.join().unwrap();
}

#[test]
fn latch_stress_many_threads() {
    // 8 worker + 1 waiter，反复 count_down。
    let l = Arc::new(Latch::new(8));
    let l2 = l.clone();
    let mut hs = vec![];
    for _ in 0..8 {
        let l = l.clone();
        hs.push(std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(5));
            l.count_down();
        }));
    }
    let waiter = std::thread::spawn(move || {
        l2.wait();
    });
    for h in hs {
        h.join().unwrap();
    }
    waiter.join().unwrap();
}
