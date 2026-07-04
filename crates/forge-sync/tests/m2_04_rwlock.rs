//! M2.4 —— RwLock：读多写少时，多个读者可并行
use std::sync::RwLock;
use std::thread;

#[test]
fn rwlock_allows_concurrent_readers() {
    let config = RwLock::new(vec![1, 2, 3]);
    let sum = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));

    thread::scope(|s| {
        // 8 个读者并行读
        for _ in 0..8 {
            let config = &config; // 借用
            let sum = sum.clone();
            s.spawn(move || {
                let g = config.read().unwrap();
                sum.fetch_add(g.iter().sum::<i32>() as i64, std::sync::atomic::Ordering::Relaxed);
            });
        }
    });
    // 8 × (1+2+3) = 48
    assert_eq!(sum.load(std::sync::atomic::Ordering::Relaxed), 48);

    // 写者独占
    *config.write().unwrap() = vec![10];
    assert_eq!(&*config.read().unwrap(), &[10_i32][..]);
}
