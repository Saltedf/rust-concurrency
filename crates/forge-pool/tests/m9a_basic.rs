//! M9a 基础测试：oneshot 通道 + 两种池的最小可用性 + 关停。

use forge_pool::{SharedQueuePool, StealingPool};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

// ---------- oneshot ----------

#[test]
fn oneshot_send_then_recv_returns_value() {
    let (s, r) = forge_pool::oneshot::channel();
    s.send(42).unwrap();
    assert_eq!(r.recv(), 42);
}

#[test]
fn oneshot_recv_blocks_until_send() {
    let (s, r) = forge_pool::oneshot::channel::<u32>();
    let handle = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        s.send(7).unwrap();
    });
    // 主线程立刻 recv——会阻塞约 30ms。
    assert_eq!(r.recv(), 7);
    handle.join().unwrap();
}

#[test]
fn oneshot_try_recv_returns_none_before_send() {
    let (_s, r) = forge_pool::oneshot::channel::<u32>();
    assert!(r.try_recv().is_none());
}

#[test]
fn oneshot_try_recv_returns_some_after_send() {
    let (s, r) = forge_pool::oneshot::channel();
    s.send("hello").unwrap();
    assert_eq!(r.try_recv(), Some("hello"));
}

#[test]
#[should_panic(expected = "sender dropped")]
fn oneshot_recv_panics_on_sender_drop_without_send() {
    let (s, r) = forge_pool::oneshot::channel::<u32>();
    drop(s);
    let _ = r.recv();
}

// ---------- V1 共享队列池 ----------

#[test]
fn v1_pool_runs_a_task_and_returns_value() {
    let pool = SharedQueuePool::new(4);
    let h = pool.spawn(|| 1 + 2);
    assert_eq!(h.recv(), 3);
}

#[test]
fn v1_pool_runs_many_tasks() {
    let pool = SharedQueuePool::new(4);
    let handles: Vec<_> = (0..100).map(|i| pool.spawn(move || i * i)).collect();
    let sum: i64 = handles.into_iter().map(|h| h.recv() as i64).sum();
    let expected: i64 = (0..100).map(|i| (i * i) as i64).sum();
    assert_eq!(sum, expected);
}

#[test]
fn v1_pool_drop_waits_for_workers() {
    let counter = Arc::new(AtomicUsize::new(0));
    {
        let pool = SharedQueuePool::new(4);
        for _ in 0..50 {
            let c = counter.clone();
            pool.spawn(move || {
                std::thread::sleep(Duration::from_millis(2));
                c.fetch_add(1, Ordering::Relaxed);
            });
        }
        // 离开作用域 → drop(pool) → 所有 worker 干完手头任务后退出。
    }
    assert_eq!(counter.load(Ordering::Relaxed), 50);
}

// ---------- V3 工作窃取池 ----------

#[test]
fn v3_pool_runs_a_task_and_returns_value() {
    let pool = StealingPool::new(4);
    let h = pool.spawn(|| 10 * 10);
    assert_eq!(h.recv(), 100);
}

#[test]
fn v3_pool_runs_many_tasks_from_external_thread() {
    let pool = StealingPool::new(4);
    let n = 500;
    let handles: Vec<_> = (0..n).map(|i| pool.spawn(move || i * 2)).collect();
    let sum: i64 = handles.into_iter().map(|h| h.recv() as i64).sum();
    let expected: i64 = (0..n).map(|i| (i * 2) as i64).sum();
    assert_eq!(sum, expected);
}

#[test]
fn v3_pool_drop_waits_for_workers() {
    let counter = Arc::new(AtomicUsize::new(0));
    {
        let pool = StealingPool::new(4);
        for _ in 0..50 {
            let c = counter.clone();
            pool.spawn(move || {
                std::thread::sleep(Duration::from_millis(2));
                c.fetch_add(1, Ordering::Relaxed);
            });
        }
    }
    assert_eq!(counter.load(Ordering::Relaxed), 50);
}
