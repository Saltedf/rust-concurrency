//! M9b 测试 3：Delay + 多个并发 timer 的 reactor 协作。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_rt::{block_on, Delay, Reactor};

#[test]
fn many_concurrent_delays_under_block_on() {
    // 主 future 里 spawn（在 block_on 内部我们不能真 spawn，但可以连续 await
    // 多个 Delay——验证 reactor 同时维护多个 timer 槽）。
    let reactor = Reactor::new().expect("reactor");
    let r2 = reactor.clone();

    let start = Instant::now();
    let out = block_on(
        async move {
            // 我们手工写一个"两 delay 串行 await"的最简 future。
            // 真正的 join! 需要执行器，这里我们仅作串行 await 验证 reactor
            // 能正确处理多次 register/unregister。
            Delay::new(r2.clone(), Duration::from_millis(40)).await;
            Delay::new(r2, Duration::from_millis(40)).await;
            "two"
        },
        &reactor,
    );
    assert_eq!(out, "two");
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(70),
        "should wait roughly 80ms, got {:?}",
        elapsed
    );
}

#[test]
fn runtime_drives_many_concurrent_delays_via_workstealing() {
    // 用 Runtime.spawn 多个 task，每个内部 await Delay；执行器多 worker 并发
    // 推进——总耗时约等于最长那个，而非 N 倍。
    use forge_rt::Runtime;
    let reactor = Reactor::new().expect("reactor");
    let rt = Runtime::new(4, reactor.clone()).expect("runtime");

    let counter = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    let start = Instant::now();
    for _ in 0..8 {
        let c = counter.clone();
        let r = reactor.clone();
        handles.push(rt.spawn(async move {
            Delay::new(r, Duration::from_millis(60)).await;
            c.fetch_add(1, Ordering::Relaxed);
        }));
    }
    for h in handles {
        h.recv();
    }
    let elapsed = start.elapsed();
    assert_eq!(counter.load(Ordering::Relaxed), 8);
    // 8 个并发 60ms delay，4 worker 并发——总耗时应远小于 8*60=480ms。
    // 我们宽松一点：必须 < 400ms（验证并发，不是串行）。
    assert!(
        elapsed < Duration::from_millis(400),
        "expected concurrent execution, got {:?}",
        elapsed
    );
}
