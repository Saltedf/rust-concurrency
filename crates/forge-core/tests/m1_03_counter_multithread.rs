//! M1.3 —— 多线程计数器：为什么 `+=` 会丢更新，`fetch_add` 不会
//!
//! `fetch_add_correct` 是绿色（正确）的版本；
//! `broken_read_modify_write` 是**故意错误**的版本，用 `#[ignore]` 标记——
//! 取消 ignore 跑它，你会看见计数远小于预期（丢失更新）。
use forge_core::atomics::Counter;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

const THREADS: usize = 8;
const PER_THREAD: u64 = 100_000;

#[test]
fn fetch_add_correct() {
    let c = Arc::new(Counter::new());
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let c = c.clone();
            thread::spawn(move || {
                for _ in 0..PER_THREAD {
                    c.inc();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(c.get(), (THREADS as u64) * PER_THREAD);
}

/// 故意错误：把"读-改-写"拆成三步，多线程下会丢更新。
/// 取消下面的 `#[ignore]` 再跑，会看见断言失败。
#[test]
#[ignore = "M1.3 练习：这是错误示范，跑它会失败（丢失更新）"]
fn broken_read_modify_write() {
    let v = Arc::new(AtomicU64::new(0));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let v = v.clone();
            thread::spawn(move || {
                for _ in 0..PER_THREAD {
                    // 读-改-写三步，非原子：两个线程可能同时读到同一个旧值。
                    let cur = v.load(Ordering::Relaxed);
                    v.store(cur + 1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    // 这个断言几乎必然失败（除非线程被罕见地串行调度）。
    assert_eq!(v.load(Ordering::Relaxed), (THREADS as u64) * PER_THREAD);
}
