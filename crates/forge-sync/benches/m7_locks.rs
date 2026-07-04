//! M7.7 —— 基准：自研 futex `Mutex` vs `std::sync::Mutex` vs `parking_lot::Mutex`。
//!
//! 三个对手放在同一张表里跑，因为它们三者的设计意图高度同构：
//! - **std::sync::Mutex**：Linux 上是 futex-based，但有中毒 / Result 接口开销。
//! - **parking_lot::Mutex**：3 态编码、自适应自旋、无中毒、`lock_api` 美化 API，
//!   是 Rust 生态里"高性能锁"的事实标杆。我们在文档里反复说"forge-sync 是
//!   parking_lot 风格的简化教学版"，这里把这句话**用数字验证**。
//! - **forge-sync::Mutex**：我们手写的 3 态 + 自适应自旋（≤100 次再 wait）。
//!   fast path 也是单次 CAS 0→1。预期：无竞争时三者非常接近（差距在 ns
//!   级，是否显著要看 criterion 的 p 值），高竞争时 std / parking_lot 因
//!   为有更多调优（比如更精细的等待者计数、更聪明的 wake 策略），
//!   通常会略快——但 forge-sync **不应该被甩开几个量级**，否则我们的实现
//!   有性能 bug（比如 unlock 永远 wake_one，连没等待者时也 wake）。
//!
//! 跑法：`cargo bench -p forge-sync --bench m7_locks`
//! 看两组：`uncontended_lock/{forge, std, parking_lot}` 和
//! `contended_lock_4_threads/{forge, std, parking_lot}`。

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use forge_sync::mutex::Mutex as ForgeMutex;
use parking_lot::Mutex as ParkingLotMutex;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Barrier;
use std::thread;

const UNCONTESTED_ITERS: u64 = 1_000;
const CONTESTED_ITERS: u64 = 50_000;
const HAMMER_THREADS: usize = 4;

fn bench_uncontended(c: &mut Criterion) {
    let mut group = c.benchmark_group("uncontended_lock");

    group.bench_function("forge", |b| {
        let m = ForgeMutex::new(0u64);
        b.iter(|| {
            let mut sum = 0u64;
            for _ in 0..UNCONTESTED_ITERS {
                let mut g = m.lock();
                *g += 1;
                sum += *g;
            }
            black_box(sum);
        });
    });

    group.bench_function("std", |b| {
        let m = StdMutex::new(0u64);
        b.iter(|| {
            let mut sum = 0u64;
            for _ in 0..UNCONTESTED_ITERS {
                let mut g = m.lock().unwrap();
                *g += 1;
                sum += *g;
            }
            black_box(sum);
        });
    });

    group.bench_function("parking_lot", |b| {
        let m = ParkingLotMutex::new(0u64);
        b.iter(|| {
            let mut sum = 0u64;
            for _ in 0..UNCONTESTED_ITERS {
                let mut g = m.lock();
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

    group.bench_function("forge", |b| {
        b.iter(|| hammer_forge());
    });
    group.bench_function("std", |b| {
        b.iter(|| hammer_std());
    });
    group.bench_function("parking_lot", |b| {
        b.iter(|| hammer_parking_lot());
    });

    // 扫一条线程数曲线，看 forge 是否随线程数线性恶化（如果线性，说明
    // 实现没问题、只是 futex 路径比 std 多了几纳秒；如果 4→8 那段崩塌，
    // 说明 forge 的 wake/notify 策略有 bug，比如惊群）。
    for &t in &[1usize, 2, 4, 8] {
        group.bench_with_input(BenchmarkId::new("forge_curve", t), &t, |b, &t| {
            b.iter(|| {
                let counter = ForgeMutex::new(0u64);
                hammer_forge_with(counter, t);
            });
        });
        group.bench_with_input(BenchmarkId::new("parking_lot_curve", t), &t, |b, &t| {
            b.iter(|| {
                let counter = ParkingLotMutex::new(0u64);
                hammer_pl_with(counter, t);
            });
        });
    }

    group.finish();
}

fn hammer_forge() {
    let counter = ForgeMutex::new(0u64);
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
    assert_eq!(*counter.lock(), expected, "forge Mutex 丢失更新");
    assert_eq!(checksum.load(Ordering::Relaxed), expected);
    black_box(());
}

fn hammer_std() {
    let counter = StdMutex::new(0u64);
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
}

fn hammer_parking_lot() {
    let counter = ParkingLotMutex::new(0u64);
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
    assert_eq!(*counter.lock(), expected, "parking_lot Mutex 丢失更新");
    assert_eq!(checksum.load(Ordering::Relaxed), expected);
    black_box(());
}

fn hammer_forge_with(counter: ForgeMutex<u64>, threads: usize) {
    let start = Barrier::new(threads + 1);
    let iters = 20_000;
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
    assert_eq!(*counter.lock(), (threads as u64) * iters);
}

fn hammer_pl_with(counter: ParkingLotMutex<u64>, threads: usize) {
    let start = Barrier::new(threads + 1);
    let iters = 20_000;
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
    assert_eq!(*counter.lock(), (threads as u64) * iters);
}

criterion_group!(benches, bench_uncontended, bench_contended);
criterion_main!(benches);
