//! M2.3 —— MutexGuard 的生命周期陷阱
//!
//! `if let Some(x) = lock().pop() { … }` 里，临时 guard 会活到**整条 if let 结束**，
//! 于是处理 item 时不必要地占着锁。正确做法：把 pop 拆到单独的 let。
//! 本测试演示"正确写法"——处理阶段不持锁，于是别的线程能并行进来。
use std::sync::Mutex;
use std::thread;

#[test]
fn drop_guard_before_processing() {
    let list: Mutex<Vec<i32>> = Mutex::new(vec![1, 2, 3]);
    let processed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // 正确：先在一条语句里 pop（guard 随即 drop），再处理
    let item = list.lock().unwrap().pop();
    if let Some(x) = item {
        // 这里已经不持锁了
        processed.fetch_add(x as usize, std::sync::atomic::Ordering::Relaxed);
    }

    // 另一个线程能立刻拿到锁
    let p = processed.clone();
    thread::scope(|s| {
        s.spawn(|| {
            list.lock().unwrap().push(99);
            p.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
    });

    assert_eq!(processed.load(std::sync::atomic::Ordering::Relaxed), 4); // 弹出 3，再 +1
}
