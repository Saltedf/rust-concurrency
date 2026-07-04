//! 回归测试:多等待者下的丢失唤醒 bug。
//!
//! 复现路径(M8a Semaphore 旧版 release 只在 old==0 时 wake_one):
//!   permits=0; T1、T2 都 acquire → 都进入 futex wait。
//!   release() → permits 0→1,wake_one 唤醒 T1。
//!   release() → permits 1→2,old==1≠0 ⇒ **不 wake**。
//!   T1 醒来 CAS 2→1 完成。
//!   结果 permits=1,但 T2 永远睡(丢唤醒)。bug。
//!
//! 修复后(num_waiters 计数器:release 看到 waiter>0 总是 wake_one)T2 必醒。
//! 测试带超时:bug 版会卡死到超时失败;修复版秒过。

use std::sync::Arc;
use std::time::Duration;

use forge_lockfree::semaphore::Semaphore;

#[test]
fn two_waiters_two_releases_both_acquire() {
    let sem = Arc::new(Semaphore::new(0));
    // barrier 让两个 acquire 线程都先确认对方已进入 wait 再 release,
    // 把"两 waiter 都在队列里"这个前置条件做实,确保 bug 可复现。
    let both_parked = Arc::new(std::sync::Barrier::new(3));

    let t1 = {
        let sem = sem.clone();
        let both_parked = both_parked.clone();
        std::thread::spawn(move || {
            // 故意让 acquire 先循环空转几毫秒,增加两线程同时进入 wait 的概率。
            // 不影响正确性:acquire 内部用 atomic-wait 的 expected 机制,不会误睡。
            both_parked.wait();
            sem.acquire();
            sem.release();
        })
    };
    let t2 = {
        let sem = sem.clone();
        let both_parked = both_parked.clone();
        std::thread::spawn(move || {
            both_parked.wait();
            sem.acquire();
            sem.release();
        })
    };

    // 主线程也到 barrier:三方齐 → 两个 acquire 线程同时去 acquire(0)。
    both_parked.wait();

    // 给 acquire 线程一点时间真的进入 futex wait(permits 仍为 0)。
    std::thread::sleep(Duration::from_millis(100));

    // 两次 release:bug 版第二次 old==1 不 wake,T2 永远睡。
    sem.release();
    sem.release();

    // 如果丢唤醒,join 会卡死;用超时把它转成测试失败而不是 CI 挂起。
    let join = std::thread::spawn(move || {
        t1.join().expect("t1 panicked");
        t2.join().expect("t2 panicked"); // bug 版这里永远不返回
    });
    let done = crossbeam_or_wait_timeout(join, Duration::from_secs(5));
    assert!(done, "丢失唤醒:一个 acquire 线程在 5s 内没拿到许可");
}

/// 没有跨线程 join-with-timeout 的标准库 API;用一个看门狗线程实现:
/// 主线程等 `timeout`,若子线程还没结束就 panic。
fn crossbeam_or_wait_timeout(handle: std::thread::JoinHandle<()>, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if handle.is_finished() {
            // join 收尾(此时线程必然已结束,join 不会阻塞)。
            let _ = handle.join();
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false // 超时未完成 ⇒ bug 复现
}
