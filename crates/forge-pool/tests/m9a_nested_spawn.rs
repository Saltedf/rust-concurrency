//! M9a 嵌套 spawn：M9a 的核心 stress test。
//!
//! 在 worker 线程上跑的任务 A 调 `pool.spawn(B)` 然后 `b_handle.recv()`。
//! 如果 worker 在 recv 里直接 park（不干活），并且 B 落在了同一个 worker
//! 的本地队列上（其他 worker 都在忙、没人偷），就会**死锁**——A 等 B 的结果、
//! B 永远不会被跑。
//!
//! V3（StealingPool）的 `JoinHandle::recv` 在 worker 上不会 park，而是持续
//! 跑挂起的任务，所以 B 会自己被运行完、oneshot 唤醒 A 的 recv。这个测试
//! 在 V3 上**必须通过**（带超时保险，10 秒没结果就视为死锁失败）。
//!
//! 对照实验：V1 的 JoinHandle 走纯 park 路径，**会死锁**——所以这个测试
//! 只对 V3 跑，不对 V1 跑。教程里我们用文字画出 V1 的死锁。

use forge_pool::StealingPool;
use std::sync::Arc;
use std::time::Duration;

/// 单个任务嵌套：spawn 一个子任务并等它。
#[test]
fn v3_nested_spawn_one_level_does_not_deadlock() {
    let pool = Arc::new(StealingPool::new(1)); // 只有 1 个 worker：最容易触发死锁
    let pool_clone = pool.clone();
    let h = pool.spawn(move || {
        // 当前在外部线程注入的入口任务上跑——不，这个任务被某个 worker 跑。
        // 它内部又 spawn 一个子任务，然后阻塞 recv。子任务大概率落在同一个
        // worker 的本地队列上（只有 1 个 worker）。
        let inner_h = pool_clone.spawn(|| 99);
        inner_h.recv() + 1
    });

    // 超时保险：如果死锁，主线程 10 秒后超时失败。
    let result = wait_timeout(h, Duration::from_secs(10));
    assert_eq!(result, Some(100), "V3 nested spawn must not deadlock");
}

/// 更深：3 层嵌套。验证多层 spawn + recv 都不死锁。
#[test]
fn v3_nested_spawn_three_levels_do_not_deadlock() {
    let pool = Arc::new(StealingPool::new(2));
    let pool_clone = pool.clone();
    let h = pool.spawn(move || {
        // 第 1 层
        let p1 = pool_clone.clone();
        let h1 = pool_clone.spawn(move || {
            // 第 2 层
            let p2 = p1.clone();
            let h2 = p1.spawn(move || {
                // 第 3 层
                let h3 = p2.spawn(|| 1);
                h3.recv() + 1
            });
            h2.recv() + 1
        });
        h1.recv() + 1
    });
    let result = wait_timeout(h, Duration::from_secs(10));
    assert_eq!(result, Some(4));
}

/// 多个 worker 上各自嵌套 spawn，且任务量足够多，逼迫 worker 频繁"边等边跑"。
#[test]
fn v3_many_nested_spawns_under_load() {
    let pool = Arc::new(StealingPool::new(4));
    let mut handles = Vec::new();
    for i in 0..40 {
        let p = pool.clone();
        handles.push(pool.spawn(move || {
            // 每个 outer 任务再 spawn 3 个子任务，等它们全部结果。
            let mut sub = Vec::new();
            for j in 0..3 {
                let p = p.clone();
                sub.push(p.spawn(move || i * 100 + j));
            }
            sub.into_iter().map(|h| h.recv()).collect::<Vec<i32>>()
        }));
    }
    let all: Vec<Vec<i32>> = handles
        .into_iter()
        .map(|h| wait_timeout(h, Duration::from_secs(15)).expect("no deadlock"))
        .collect();
    assert_eq!(all.len(), 40);
    for (i, v) in all.into_iter().enumerate() {
        assert_eq!(
            v,
            vec![(i * 100) as i32, (i * 100 + 1) as i32, (i * 100 + 2) as i32]
        );
    }
}

// ---------- helpers ----------

/// 在专用线程上跑 recv，主线程等到 deadline。超时返回 None。
fn wait_timeout<T: Send + 'static>(h: forge_pool::JoinHandle<T>, dur: Duration) -> Option<T> {
    let (s, r) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let v = h.recv();
        let _ = s.send(v);
    });
    match r.recv_timeout(dur) {
        Ok(v) => Some(v),
        Err(_) => None,
    }
}
