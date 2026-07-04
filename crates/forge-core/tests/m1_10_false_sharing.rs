//! M1.10 —— False sharing（伪共享）冒烟测试。
//!
//! 真正的量化对比在 benches/m1_false_sharing.rs；这里只验证两种结构都能正确并发自增。
use forge_core::atomics::{AdjacentCounters, PaddedCounters};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;

#[test]
fn adjacent_counters_work() {
    let c = Arc::new(AdjacentCounters {
        a: std::sync::atomic::AtomicU64::new(0),
        b: std::sync::atomic::AtomicU64::new(0),
    });
    let c2 = c.clone();
    let h = thread::spawn(move || {
        for _ in 0..100_000 {
            c2.a.fetch_add(1, Ordering::Relaxed);
        }
    });
    for _ in 0..100_000 {
        c.b.fetch_add(1, Ordering::Relaxed);
    }
    h.join().unwrap();
    assert_eq!(c.a.load(Ordering::Relaxed), 100_000);
    assert_eq!(c.b.load(Ordering::Relaxed), 100_000);
}

#[test]
fn padded_counters_work() {
    let c = Arc::new(PaddedCounters {
        a: forge_core::atomics::CacheLine::new(std::sync::atomic::AtomicU64::new(0)),
        b: forge_core::atomics::CacheLine::new(std::sync::atomic::AtomicU64::new(0)),
    });
    let c2 = c.clone();
    let h = thread::spawn(move || {
        for _ in 0..100_000 {
            c2.a.0.fetch_add(1, Ordering::Relaxed);
        }
    });
    for _ in 0..100_000 {
        c.b.0.fetch_add(1, Ordering::Relaxed);
    }
    h.join().unwrap();
    assert_eq!(c.a.0.load(Ordering::Relaxed), 100_000);
    assert_eq!(c.b.0.load(Ordering::Relaxed), 100_000);
}
