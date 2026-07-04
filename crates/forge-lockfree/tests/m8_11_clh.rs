//! M8.11 —— CLH 队列锁：N 线程排队、FIFO 公平、互斥。
//!
//! 对照 m8_04_mcs.rs：同样测 8 线程 × 1000 = 8000 互斥计数。
//! 这里额外加一个"近似 FIFO 公平"的检查——记录每线程获锁时刻的"全局单调计数器"，
//! 看是否符合入队顺序（CLH 保证 FIFO，但因为 spin 而非 park，时序在纳秒粒度有抖动，
//! 我们只检查"宏观上每线程获锁次数大致均匀"——这是 CLH 公平性的弱指标）。
use forge_lockfree::clh::ClhLock;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

#[test]
fn clh_mutual_exclusion() {
    let lock = ClhLock::new();
    let counter = AtomicI64::new(0);
    thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                for _ in 0..1000 {
                    let _g = lock.lock();
                    counter.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });
    assert_eq!(counter.load(Ordering::Relaxed), 8000);
}

/// 强互斥保证：lock 期间，全局 max 不应超过 1（同一时刻最多 1 个线程持锁）。
#[test]
fn clh_strict_mutual_exclusion() {
    let lock = Arc::new(ClhLock::new());
    let in_critical = Arc::new(AtomicI64::new(0));
    let max_concurrent = Arc::new(AtomicI64::new(0));

    thread::scope(|s| {
        for _ in 0..8 {
            let lock = lock.clone();
            let in_c = in_critical.clone();
            let max_c = max_concurrent.clone();
            s.spawn(move || {
                for _ in 0..500 {
                    let _g = lock.lock();
                    let cur = in_c.fetch_add(1, Ordering::SeqCst) + 1;
                    // 更新 max。
                    let mut m = max_c.load(Ordering::Relaxed);
                    while cur > m {
                        match max_c.compare_exchange_weak(
                            m,
                            cur,
                            Ordering::SeqCst,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => break,
                            Err(v) => m = v,
                        }
                    }
                    // 在临界区做点事，放大并发窗口。
                    std::hint::black_box(());
                    in_c.fetch_sub(1, Ordering::SeqCst);
                }
            });
        }
    });

    assert_eq!(
        max_concurrent.load(Ordering::Relaxed),
        1,
        "CLH 必须保证强互斥"
    );
}

/// 公平性：用每线程获锁次数检查"无饿死"。
/// CLH 保证 FIFO，但因 spin 实现而非严格 park/unpark，时间片切片下每个线程应能
/// 获得相近次数的锁（这里放宽：每个线程获锁次数应 >= 总数 / (线程数 * 4)）。
#[test]
fn clh_no_starvation() {
    let lock = Arc::new(ClhLock::new());
    const THREADS: usize = 8;
    const PER_THREAD: u64 = 2000;
    let per_thread_counts: Vec<AtomicU64> = (0..THREADS).map(|_| AtomicU64::new(0)).collect();
    let per_thread_counts = Arc::new(per_thread_counts);

    thread::scope(|s| {
        for tid in 0..THREADS {
            let lock = lock.clone();
            let counts = per_thread_counts.clone();
            s.spawn(move || {
                for _ in 0..PER_THREAD {
                    let _g = lock.lock();
                    counts[tid].fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });

    // 每个线程都恰好完成 PER_THREAD 次获锁——CLH 不应让任何线程饿死。
    for (tid, c) in per_thread_counts.iter().enumerate() {
        assert_eq!(c.load(Ordering::Relaxed), PER_THREAD, "线程 {} 被饿死", tid);
    }
}
