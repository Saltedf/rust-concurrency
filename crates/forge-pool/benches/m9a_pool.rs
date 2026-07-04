//! M9a.8 —— 基准：工作窃取池 vs `std::thread::spawn` per-task。
//!
//! 这条基准对照的对象很特殊：不是另一把锁，而是"每任务一个 OS 线程"。
//! 后者是写并发代码的默认方式（`for _ in 0..N { thread::spawn(|| ...); join }`），
//! 也是工作窃取池要**打败**的对象——线程池的全部意义就是"省掉反复
//! spawn OS 线程的开销"。
//!
//! 预期看到的（在 4 核机器上，跑 5000 个 `1+1` 这种极短任务）：
//! - **std::thread::spawn**：每个任务 ≈ 10~50 μs（spawn + join 一次 OS 线程
//!   的全部代价，包括 `clone` 系统调用、栈分配、调度器入队）。
//! - **StealingPool**：每个任务 ≈ 几百 ns 到几 μs（任务闭包装箱 + push 到
//!   本地队列 + worker 偷过来 + 跑）。差距通常 10~100 倍。
//! - **SharedQueuePool**（V1，所有 worker 抢同一把 `Mutex<VecDeque>`）：
//!   比 StealingPool 慢，但**不会**比 std::thread::spawn 慢——因为 OS
//!   线程 spawn 的代价远大于 Mutex 抢锁。这条对照的目的是让你看到
//!   "为什么 V3 比 V1 快"——因为锁争用从"全局一把锁"变成了"每 worker
//!   一把本地锁 + 偶尔偷"。
//!
//! **本基准故意只测短任务**（每个闭包 `move || black_box(1u64 + 1)`）。
//! 短任务最能放大 spawn 开销。任务变长（> 1ms）后，"spawn 开销"在
//! 总时间里占比变小，三者的差距收窄——这是正常的，详见 M9a 文档
//! "任务长度 vs spawn 开销"那条曲线。
//!
//! 跑法：`cargo bench -p forge-pool --bench m9a_pool`

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use forge_pool::{SharedQueuePool, StealingPool};

const TASKS_LIST: &[usize] = &[100, 1_000, 5_000];

/// 极短任务：每个闭包做一次加法。这把 spawn 的开销暴露到最大。
fn short_task() -> u64 {
    // black_box 阻止编译器把整个闭包常量折叠掉。
    let mut acc = 0u64;
    acc = acc.wrapping_add(black_box(1u64));
    acc = acc.wrapping_add(black_box(1u64));
    acc
}

fn bench_stealing_pool(c: &mut Criterion) {
    let mut group = c.benchmark_group("spawn_short_task");

    // —— Baseline：std::thread::spawn per-task ——
    // 每个任务起一个 OS 线程跑 short_task，主线程 join。
    group.bench_function("std_thread_spawn", |b| {
        b.iter_with_large_drop(|| {
            // 用 iter_with_large_drop 而不是 iter——每次 iter 都要建/拆
            // 大量 JoinHandle，iter 的小开销会被淹没。
            let mut handles = Vec::new();
            for _ in 0..TASKS_LIST[0] {
                handles.push(std::thread::spawn(short_task));
            }
            let mut sum = 0u64;
            for h in handles {
                sum = sum.wrapping_add(h.join().unwrap());
            }
            black_box(sum);
        });
    });

    // —— 工作窃取池：V3 ——
    // 4 worker（典型桌面机核数）。每次 iter 复用同一个池——重新建池很贵，
    // 我们要测的是"稳态下 spawn 一个任务"的开销，不是"建池"开销。
    let pool = StealingPool::new(4);
    group.bench_function("stealing_pool_v3", |b| {
        b.iter(|| {
            let mut handles = Vec::new();
            for _ in 0..TASKS_LIST[0] {
                handles.push(pool.spawn(short_task));
            }
            let mut sum = 0u64;
            for h in handles {
                sum = sum.wrapping_add(h.recv());
            }
            black_box(sum);
        });
    });

    // —— V1：共享队列 ——
    let pool_v1 = SharedQueuePool::new(4);
    group.bench_function("shared_queue_pool_v1", |b| {
        b.iter(|| {
            let mut handles = Vec::new();
            for _ in 0..TASKS_LIST[0] {
                handles.push(pool_v1.spawn(short_task));
            }
            let mut sum = 0u64;
            for h in handles {
                sum = sum.wrapping_add(h.recv());
            }
            black_box(sum);
        });
    });

    // —— 扫任务数曲线（只 V3 vs std）——
    // 这条曲线讲清"为什么任务量大了，pool 的优势变小"。在 100 任务时
    // pool 可能比 std 快 50 倍；在 5000 任务时差距可能掉到 10 倍——
    // 因为 std 的 spawn 也走 OS 调度器缓存，热起来后单次 spawn 稍快。
    for &n in TASKS_LIST {
        group.bench_with_input(BenchmarkId::new("stealing_curve", n), &n, |b, &n| {
            b.iter(|| {
                let mut handles = Vec::with_capacity(n);
                for _ in 0..n {
                    handles.push(pool.spawn(short_task));
                }
                let mut sum = 0u64;
                for h in handles {
                    sum = sum.wrapping_add(h.recv());
                }
                black_box(sum);
            });
        });
        group.bench_with_input(BenchmarkId::new("std_thread_curve", n), &n, |b, &n| {
            b.iter(|| {
                let mut handles = Vec::with_capacity(n);
                for _ in 0..n {
                    handles.push(std::thread::spawn(short_task));
                }
                let mut sum = 0u64;
                for h in handles {
                    sum = sum.wrapping_add(h.join().unwrap());
                }
                black_box(sum);
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_stealing_pool);
criterion_main!(benches);
