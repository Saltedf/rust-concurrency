//! M9b 测试 2：多线程 Runtime + spawn + JoinHandle。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;


use forge_rt::{Reactor, Runtime};

#[test]
fn runtime_spawns_and_returns_results() {
    let reactor = Reactor::new().expect("reactor");
    let rt = Runtime::new(4, reactor).expect("runtime");

    let h1 = rt.spawn(async { 1u32 + 1 });
    let h2 = rt.spawn(async { "hello".to_string() });
    let h3 = rt.spawn(async {
        // 一个稍微长的 future：spawn 出来的 task 可以 yield（返回 Pending）
        // 多次再最终完成。这里用 std::future::ready 风格——直接 Ready。
        let v: u64 = 100;
        v * 2
    });

    assert_eq!(h1.recv(), 2);
    assert_eq!(h2.recv(), "hello");
    assert_eq!(h3.recv(), 200);
}

#[test]
fn runtime_runs_many_tasks_across_workers() {
    let reactor = Reactor::new().expect("reactor");
    let rt = Runtime::new(4, reactor).expect("runtime");

    // 投 1000 个任务，每个返回自己 id。
    let counter = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for i in 0..1000u32 {
        let c = counter.clone();
        handles.push(rt.spawn(async move {
            c.fetch_add(1, Ordering::Relaxed);
            i
        }));
    }

    let mut sum: u64 = 0;
    for h in handles {
        sum += h.recv() as u64;
    }
    // counter 应该被每个 task 自增一次。
    assert_eq!(counter.load(Ordering::Relaxed), 1000);
    // sum 应该是 0 + 1 + ... + 999。
    assert_eq!(sum, (0..1000u64).sum::<u64>());
}

#[test]
fn runtime_block_on_returns_value() {
    let reactor = Reactor::new().expect("reactor");
    let rt = Runtime::new(2, reactor).expect("runtime");

    let v: u32 = rt.block_on(async { 7 });
    assert_eq!(v, 7);
}

#[test]
fn runtime_handles_panicking_task_without_dying() {
    // 一个 task panic 不应该让 worker 退出（catch_unwind 兜底）。
    let reactor = Reactor::new().expect("reactor");
    let rt = Runtime::new(2, reactor).expect("runtime");

    let _panic_h = rt.spawn(async {
        panic!("boom inside task");
    });
    let good_h = rt.spawn(async { "ok" });
    // panic_h 的 recv 会因为 sender drop without send 而触发 oneshot panic，
    // 但 worker 不死，good_h 仍然能拿到结果。
    // 我们用 catch_unwind 包住 panic_h.recv。
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = _panic_h.recv();
    }));
    assert_eq!(good_h.recv(), "ok");
}
