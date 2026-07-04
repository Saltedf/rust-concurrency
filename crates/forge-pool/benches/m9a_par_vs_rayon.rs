//! M9a.8b —— 基准：自研 par_sort vs `rayon`（plan 要求的 rayon 对照）。
//!
//! `m9a_pool` 对照了"工作窃取池 vs std::thread::spawn"。这条补上 plan 的另一半：
//! **和 Rust 生态的事实标杆 rayon 比**——rayon 是工业级工作窃取线程池，
//! 调优了十几年（crossbeam-deque + 梯阶窃取 + 主动睡眠等）。我们的 StealingPool
//! 是教学版（v3_stealing.rs：每 worker 一个 Mutex<VecDeque> + Condvar 唤醒，
//! 没用无锁 Chase-Lev）。
//!
//! 预期（4 核机器）：
//! - **serial**：纯 `slice.sort()`，单核基线。
//! - **小 N（10 万）**：forge/rayon **可能都比 serial 慢**——并行 spawn +
//!   窃取的固定开销 > 单核排序时间。这是正常的：任务粒度太小，并行不划算。
//! - **大 N（50 万 / 100 万）**：forge ~1.5×、rayon ~1.6–1.8× 快于 serial。
//!   forge **应该比 rayon 慢一点，但不会被甩开数量级**——否则我们的窃取调度
//!   有性能 bug。差距主要在：本地队列用 `Mutex<VecDeque>` 而非无锁 Chase-Lev
//!   deque（这正是 M8g 的用武之地——升级到无锁 deque 后差距应缩小）。
//! - Amdahl 上限：4 核理论加速比 ≤ 4×，实测 1.5–1.8× 是因为排序的 partition
//!   串行段 + 窃取开销 + 内存带宽。
//!
//! 跑法：`cargo bench -p forge-pool --bench m9a_par_vs_rayon`

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use forge_pool::par::par_sort;
use forge_pool::StealingPool;
use rand::Rng;
use std::sync::Arc;

const SIZES: &[usize] = &[100_000, 500_000, 1_000_000];

fn random_vec(n: usize) -> Vec<u64> {
    let mut rng = rand::thread_rng();
    (0..n).map(|_| rng.gen()).collect()
}

fn bench_par_sort_vs_rayon(c: &mut Criterion) {
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    // 池在整个 group 内复用：建池有成本（起 worker 线程），不该每条都重建。
    let pool = Arc::new(StealingPool::new(workers));

    let mut group = c.benchmark_group("parallel_sort");
    for &n in SIZES {
        group.bench_with_input(BenchmarkId::new("serial_std_sort", n), &n, |b, &n| {
            b.iter_batched(
                || random_vec(n),
                |mut v| {
                    v.sort();
                    black_box(v);
                },
                criterion::BatchSize::LargeInput,
            );
        });

        group.bench_with_input(BenchmarkId::new("forge_par_sort", n), &n, |b, &n| {
            b.iter_batched(
                || random_vec(n),
                |mut v| {
                    par_sort(&pool, &mut v);
                    black_box(v);
                },
                criterion::BatchSize::LargeInput,
            );
        });

        group.bench_with_input(BenchmarkId::new("rayon_par_sort", n), &n, |b, &n| {
            b.iter_batched(
                || random_vec(n),
                |mut v| {
                    v.par_sort();
                    black_box(v);
                },
                criterion::BatchSize::LargeInput,
            );
        });
    }
    group.finish();
}

use rayon::prelude::*; // 让 v.par_sort() 可用

criterion_group!(benches, bench_par_sort_vs_rayon);
criterion_main!(benches);
