//! M1.10 —— 基准：伪共享的真实代价。
//!
//! 两个线程各 hammer 一个计数器：
//! - `adjacent`：两个 `AtomicU64` 紧挨着，落在同一条缓存行 → 伪共享 → 慢。
//! - `padded`：各自用 `CacheLine<AtomicU64>` 对齐到 64 字节 → 独立缓存行 → 快。
//!
//! 跑法：`cargo bench -p forge-core`，对比两组时间。
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use forge_core::atomics::{AdjacentCounters, CacheLine, PaddedCounters};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

const N: u64 = 2_000_000;

fn hammer_adjacent(c: &mut Criterion) {
    c.bench_function("adjacent_counters/false_sharing", |b| {
        b.iter(|| {
            let c = Arc::new(AdjacentCounters {
                a: AtomicU64::new(0),
                b: AtomicU64::new(0),
            });
            let c2 = c.clone();
            let h = thread::spawn(move || {
                for _ in 0..N {
                    black_box(c2.a.fetch_add(1, Ordering::Relaxed));
                }
            });
            for _ in 0..N {
                black_box(c.b.fetch_add(1, Ordering::Relaxed));
            }
            h.join().unwrap();
        });
    });
}

fn hammer_padded(c: &mut Criterion) {
    c.bench_function("padded_counters/cache_line_aligned", |b| {
        b.iter(|| {
            let c = Arc::new(PaddedCounters {
                a: CacheLine::new(AtomicU64::new(0)),
                b: CacheLine::new(AtomicU64::new(0)),
            });
            let c2 = c.clone();
            let h = thread::spawn(move || {
                for _ in 0..N {
                    black_box(c2.a.0.fetch_add(1, Ordering::Relaxed));
                }
            });
            for _ in 0..N {
                black_box(c.b.0.fetch_add(1, Ordering::Relaxed));
            }
            h.join().unwrap();
        });
    });
}

criterion_group!(benches, hammer_adjacent, hammer_padded);
criterion_main!(benches);
