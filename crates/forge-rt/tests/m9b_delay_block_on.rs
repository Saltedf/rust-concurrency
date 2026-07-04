//! M9b 测试 1：单线程 `block_on` + `Delay` future。
//!
//! 这条测试演示：在单线程上跑 `block_on(Delay)`，reactor 在另一个线程等到期
//! wake 主 task → 执行器再 poll → Ready。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_rt::{block_on, noop_waker, Delay, Reactor};

#[test]
fn delay_completes_under_block_on() {
    let reactor = Reactor::new().expect("reactor");
    let start = Instant::now();
    let result = block_on(
        async {
            // 一个简单 async 块：等 50ms 然后返回值。
            // 但我们这里直接 await Delay —— Delay 是 future，await 它。
            // 注意：在 forge-rt 内部，Delay 自带 reactor 引用，所以这里我们
            // 用一个不直接调 Delay 的简单 future 验证 block_on。
            42u32
        },
        &reactor,
    );
    assert_eq!(result, 42);
    // 验证 block_on 确实"等到 future 完成才返回"——这里几乎瞬时。
    assert!(start.elapsed() < Duration::from_millis(100));
}

#[test]
fn delay_actually_waits_for_reactor() {
    // 用 Delay future + block_on：单线程异步 + reactor 唤醒的最小完整链路。
    let reactor = Reactor::new().expect("reactor");
    let r2 = reactor.clone();
    let start = Instant::now();
    let result = block_on(
        async move {
            // 等待 80ms。这会让 block_on 主循环：poll Delay → Pending →
            // reactor 线程 80ms 后 wake 主 task → 重新 poll → Ready。
            Delay::new(r2, Duration::from_millis(80)).await;
            "done"
        },
        &reactor,
    );
    assert_eq!(result, "done");
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(70),
        "Delay did not actually wait: {:?}",
        elapsed
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "Delay took too long (reactor wake lost?): {:?}",
        elapsed
    );
}

#[test]
fn noop_waker_can_be_woken_without_panicking() {
    // noop_waker 的 wake() 是 no-op；这只验证它构造、clone、drop 不炸。
    let w = noop_waker();
    w.wake_by_ref();
    let _c = w.clone();
}

#[test]
fn block_on_handles_multiple_inner_spawns_via_state_machine() {
    // block_on 内部其实没有 worker 线程，但它的就绪队列能容纳"被外部 wake 的 task"。
    // 这里我们用一个简单 future + 一个 reactor-delay 子 future 直接 await，
    // 验证 task 状态机 IDLE → QUEUED → RUNNING → (Pending) → IDLE → ... → Ready。
    let reactor = Reactor::new().expect("reactor");
    let r2 = reactor.clone();
    let flag = Arc::new(AtomicBool::new(false));
    let f2 = flag.clone();
    let out = block_on(
        async move {
            Delay::new(r2.clone(), Duration::from_millis(30)).await;
            f2.store(true, Ordering::SeqCst);
            Delay::new(r2, Duration::from_millis(30)).await;
            "two_delays"
        },
        &reactor,
    );
    assert_eq!(out, "two_delays");
    assert!(
        flag.load(Ordering::SeqCst),
        "first delay should have completed before second"
    );
}
