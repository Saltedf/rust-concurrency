//! M1.6 / M1.7 —— 内存序：Release 发布 / Acquire 订阅
//!
//! 这是全章的核心课。演示"通过原子标志发布数据"为什么**必须**用 Release/Acquire。
//!
//! 教程会先展示一个用 Relaxed 的**错误**版本（可能读到"指针/标志已就绪，但数据未写入"），
//! 然后修复为 Release/Acquire。本测试验证**修复后**的正确性：在大量并发迭代下，
//! 消费者每次都能看到完整的 42。
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

#[test]
fn release_acquire_publishes_data() {
    for _ in 0..2000 {
        let data = Arc::new(AtomicU64::new(0));
        let ready = Arc::new(AtomicBool::new(false));

        let (d2, r2) = (data.clone(), ready.clone());
        let h = thread::spawn(move || {
            // 生产者：先写数据，再用 Release 立标志。
            d2.store(42, Ordering::Relaxed);
            r2.store(true, Ordering::Release);
        });

        // 消费者：忙等到标志（Acquire），然后读数据。
        while !ready.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
        assert_eq!(data.load(Ordering::Relaxed), 42, "Acquire 之后必须看到 Release 之前写的数据");
        h.join().unwrap();
    }
}
