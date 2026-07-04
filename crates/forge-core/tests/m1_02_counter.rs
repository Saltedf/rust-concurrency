//! M1.2 —— 计数器（单线程）
use forge_core::atomics::Counter;
use std::sync::atomic::Ordering;

#[test]
fn counter_increments() {
    let c = Counter::new();
    for _ in 0..1000 {
        c.inc();
    }
    assert_eq!(c.get(), 1000);
}

#[test]
fn counter_add() {
    let c = Counter::new();
    c.add(7);
    c.add(3);
    assert_eq!(c.get(), 10);
    let _ = Ordering::Relaxed; // 仅占位，演示 Ordering 的存在
}
