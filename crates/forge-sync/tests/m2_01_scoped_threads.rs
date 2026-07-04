//! M2.1 —— scoped threads：线程借用局部变量
//!
//! `thread::scope` 保证作用域内 spawn 的线程在作用域结束前全部 join，
//! 所以它们可以安全地**借用**非 'static 的局部数据（如这里的 `numbers`）。
//! 两个线程同时只读 `numbers`——这是 `thread::spawn`（要求 'static）做不到的。
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

#[test]
fn scoped_threads_can_borrow_locals() {
    let numbers = vec![1, 2, 3, 4, 5]; // 非 'static 的局部变量
    let sum = Arc::new(AtomicUsize::new(0));

    thread::scope(|s| {
        // 线程 A：借用 numbers 算长度
        s.spawn({
            let numbers = &numbers;
            let sum = sum.clone();
            move || {
                sum.fetch_add(numbers.len(), Ordering::Relaxed);
            }
        });
        // 线程 B：借用 numbers 求和
        s.spawn({
            let numbers = &numbers;
            let sum = sum.clone();
            move || {
                sum.fetch_add(numbers.iter().sum::<usize>(), Ordering::Relaxed);
            }
        });
    });
    // 作用域到此：两个线程已自动 join，numbers 可安全释放。

    assert_eq!(sum.load(Ordering::Relaxed), 5 + 15);
}
