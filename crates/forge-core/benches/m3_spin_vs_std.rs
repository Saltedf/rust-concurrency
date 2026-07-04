//! M3.7 —— 基准：自研 `SpinLock` vs `std::sync::Mutex`。
//!
//! 两条对照路径：
//! - **无竞争**：单线程反复 lock/unlock，临界区里只做一次 `+= 1`。
//!   这里 SpinLock 应当明显赢——`std::sync::Mutex` 的 fast path 虽然也是
//!   一次 CAS，但 std 会预留中毒错误路径、guard 是 `Result`，编译器很难
//!   把 `unwrap` 那条分支完全消掉；我们的 SpinLock 是裸 `AtomicBool::swap`
//!   + 直接返回 `Guard`，更接近"两个 mov"。差距通常在 2~5ns。
//! - **高竞争**：4 线程 hammer 同一个锁，每个线程抢 N 次、临界区里 `+= 1`。
//!   这里 std::sync::Mutex 应当赢（或至少不输）——它在抢不到时会 park
//!   （让出核），让持锁者能独占临界区跑完；我们的 SpinLock 在抢不到时
//!   **烧 CPU 自旋**，4 个线程互相把对方挤掉核，吞吐崩塌。这就是 M3 文档
//!   里"自旋锁在过订阅/高争用下是灾难"的实证。
//!
//! 跑法：`cargo bench -p forge-core --bench m3_spin_vs_std`
//! 看报告：`spin_uncontended` vs `std_uncontended`、`spin_contended_4t` vs
//! `std_contended_4t`。前者差几 ns（统计显著性看 p 值），后者差几倍甚至
//! 几十倍（差几个 σ 一目了然）。读法详见 `docs/modules/M11-testing.md`
//! 第四节"criterion 的统计模型"。

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use forge_core::spin::SpinLock;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Barrier;
use std::thread;

/// 单线程下每个 bench iter 要做的 lock/unload 次数。
/// 选 1000 是为了让单次 iter 落在 criterion 舒适的 ~ms 量级——太少会被
/// 计时器精度吃掉，太多会让 warmup 慢。
const UNCONTESTED_ITERS: u64 = 1_000;
/// 高竞争下每线程的 lock/unlock 次数。比无竞争少一些，因为要乘以 4 线程。
const CONTESTED_ITERS: u64 = 50_000;
const HAMMER_THREADS: usize = 4;

fn bench_uncontended(c: &mut Criterion) {
    let mut group = c.benchmark_group("uncontended_lock");

    group.bench_function("spin", |b| {
        let lock = SpinLock::new(0u64);
        b.iter(|| {
            let mut sum: u64 = 0;
            for _ in 0..UNCONTESTED_ITERS {
                let mut g = lock.lock();
                *g += 1;
                sum += *g;
            }
            black_box(sum);
        });
    });

    group.bench_function("std", |b| {
        let lock = Mutex::new(0u64);
        b.iter(|| {
            let mut sum: u64 = 0;
            for _ in 0..UNCONTESTED_ITERS {
                let mut g = lock.lock().unwrap();
                *g += 1;
                sum += *g;
            }
            black_box(sum);
        });
    });

    group.finish();
}

fn bench_contended(c: &mut Criterion) {
    let mut group = c.benchmark_group("contended_lock_4_threads");

    // 用同一组线程模型跑两个子项。每 iter 起一次 4 线程 hammer。
    group.bench_function("spin", |b| {
        b.iter(|| {
            let counter = SpinLock::new(0u64);
            // 用 AtomicU64 做校验和——每线程把自己累加值汇报上来，
            // 最后和 lock 内的总值核对。这一步逼着编译器不能把
            // `*g += 1` 优化掉（虽然 black_box 已经够，但多一道保险）。
            let checksum = AtomicU64::new(0);
            let start = Barrier::new(HAMMER_THREADS + 1);
            thread::scope(|s| {
                for _ in 0..HAMMER_THREADS {
                    s.spawn(|| {
                        let _ = start.wait();
                        let mut local = 0u64;
                        for _ in 0..CONTESTED_ITERS {
                            let mut g = counter.lock();
                            *g += 1;
                            local += 1;
                        }
                        checksum.fetch_add(local, Ordering::Relaxed);
                    });
                }
                let _ = start.wait();
            });
            let expected = (HAMMER_THREADS as u64) * CONTESTED_ITERS;
            assert_eq!(*counter.lock(), expected, "SpinLock 丢失更新");
            assert_eq!(checksum.load(Ordering::Relaxed), expected, "局部校验失败");
            black_box(());
        });
    });

    group.bench_function("std", |b| {
        b.iter(|| {
            let counter = Mutex::new(0u64);
            let checksum = AtomicU64::new(0);
            let start = Barrier::new(HAMMER_THREADS + 1);
            thread::scope(|s| {
                for _ in 0..HAMMER_THREADS {
                    s.spawn(|| {
                        let _ = start.wait();
                        let mut local = 0u64;
                        for _ in 0..CONTESTED_ITERS {
                            let mut g = counter.lock().unwrap();
                            *g += 1;
                            local += 1;
                        }
                        checksum.fetch_add(local, Ordering::Relaxed);
                    });
                }
                let _ = start.wait();
            });
            let expected = (HAMMER_THREADS as u64) * CONTESTED_ITERS;
            assert_eq!(*counter.lock().unwrap(), expected, "std Mutex 丢失更新");
            assert_eq!(checksum.load(Ordering::Relaxed), expected);
            black_box(());
        });
    });

    // 额外画一条"竞争度曲线"：固定 iters，扫描 1/2/4/8 线程。
    // 这条曲线最能讲清"自旋锁在 worker > 物理核时崩塌"——它在 4→8
    // 那段会有明显的拐点（前提是你机器是 4~8 核）。
    for &t in &[1usize, 2, 4, 8] {
        group.bench_with_input(BenchmarkId::new("spin_curve", t), &t, |b, &t| {
            b.iter(|| hammer_spin(t));
        });
        group.bench_with_input(BenchmarkId::new("std_curve", t), &t, |b, &t| {
            b.iter(|| hammer_std(t));
        });
    }

    group.finish();
}

fn hammer_spin(threads: usize) {
    let counter = SpinLock::new(0u64);
    let start = Barrier::new(threads + 1);
    let iters = 20_000; // 曲线扫描用小一点，不然跑太久
    thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let _ = start.wait();
                for _ in 0..iters {
                    let mut g = counter.lock();
                    *g += 1;
                }
            });
        }
        let _ = start.wait();
    });
    let expected = (threads as u64) * iters;
    assert_eq!(*counter.lock(), expected);
    black_box(());
}

fn hammer_std(threads: usize) {
    let counter = Mutex::new(0u64);
    let start = Barrier::new(threads + 1);
    let iters = 20_000;
    thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let _ = start.wait();
                for _ in 0..iters {
                    let mut g = counter.lock().unwrap();
                    *g += 1;
                }
            });
        }
        let _ = start.wait();
    });
    let expected = (threads as u64) * iters;
    assert_eq!(*counter.lock().unwrap(), expected);
    black_box(());
}

criterion_group!(benches, bench_uncontended, bench_contended);
criterion_main!(benches);
