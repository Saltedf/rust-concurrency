//! M1.7 —— LazyBox：用 Release/Acquire 安全发布堆对象
//!
//! 多个线程并发 `get()`，竞争构造；所有人最终看到的必须是**赢家那份**同一个值。
use forge_core::atomics::LazyBox;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

#[test]
fn lazy_box_publishes_one_consistent_value() {
    let lazy = Arc::new(LazyBox::<usize>::new());
    let made = Arc::new(AtomicUsize::new(0));

    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let handles: Vec<_> = (0..16)
        .map(|_| {
            let lazy = lazy.clone();
            let made = made.clone();
            let seen = seen.clone();
            thread::spawn(move || {
                let val = lazy.get(|| {
                    // 模拟昂贵的构造；赢家写下的值会被所有人看到。
                    made.fetch_add(1, Ordering::Relaxed)
                });
                seen.lock().unwrap().push(*val);
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    let seen = seen.lock().unwrap();
    let first = seen[0];
    assert!(
        seen.iter().all(|v| *v == first),
        "所有线程必须看到同一个值，实际: {:?}",
        *seen
    );
    assert!(made.load(Ordering::Relaxed) >= 1, "至少构造过一次");
}
