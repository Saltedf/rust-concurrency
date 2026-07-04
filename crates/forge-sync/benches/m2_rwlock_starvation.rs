//! M2.4 —— 基准：RwLock 的写饥饿（plan 要求的 "criterion 演示"）。
//!
//! M2 文档第 7.1 节讲过"写饥饿"这个敌人，并把它埋成 M7 的伏笔。M7 自建的
//! RwLock 用"写者在等就挡住新读者"的策略正面解决了它。这条基准把"std vs
//! forge"放在同一张表里跑，**让数据说话**——结果和我们最初的直觉有些出入，
//! 正好值得诚实分析：
//!
//! 场景：7 个读者线程死循环 `read()`（hold 期间做 256 次加法以撑大 hold 时间，
//! 制造"几乎总有读者占着"的墙），写者完成 5000 次 `write()`。criterion 报
//! 写者总耗时。
//!
//! 实测（4 核 Linux，glibc）：
//! - **std::sync::RwLock**：~45–60 ms。现代 glibc 的 `pthread_rwlock` 并不像
//!   经典说法那样剧烈饿写者——内核调度 + glibc 的实现给了写者相当多的机会。
//! - **forge_sync::RwLock**（M7 写公平版）：~120–155 ms，**比 std 慢 2–3 倍**。
//!
//! **为什么 forge 反而慢？** 这条基准测的是**吞吐**（写者 5000 次写的总耗时），
//! 不是**公平性**（写者最坏情况延迟）。forge 的"写公平"保证来自每次操作都要
//! 维护的 3 态编码、`writer_wake_counter`、Condvar 唤醒——这些是**活性保证的
//! 代价**，不是免费的。std 的实现被调优了十几年，per-op 开销低得多，即使它
//! 理论上"偏向读者"，在吞吐维度上仍然赢。
//!
//! **那 M7 的写公平 RwLock 还有意义吗？** 有——但它的价值在**最坏情况延迟**
//! （写者不会被无限期推迟），不在吞吐。一条吞吐 bench 会**低估**写公平的
//! 价值；要真正衡量它，得测写者延迟的**尾部分布**（p99/p999），那才是"饿不
//! 饿"的真正指标（留给练习：把这条 bench 改成测写者单次写的 p99 延迟）。
//!
//! 这条 bench 的教学价值：① 让你看见 RwLock 写者在读者压力下的真实行为；
//! ② 区分"吞吐"和"公平性/尾延迟"两个维度；③ 诚实面对"自研教学版在吞吐上
//! 输给工业版"——这正是 M7 文档里"我们用 std/parking_lot 对照自己"的精神。
//!
//! 跑法：`cargo bench -p forge-sync --bench m2_rwlock_starvation`

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

const READER_THREADS: usize = 7;
/// 每次测量里写者要完成多少次写。固定工作量、测时长——时长越短说明写者
/// 越没被饿。criterion 报告的就是"写者做 N 次写的耗时"。
const WRITER_OPS: u64 = 5_000;

trait RwLockApi {
    type ReadGuard<'a>: std::ops::Deref<Target = u64>
    where
        Self: 'a;
    type WriteGuard<'a>: std::ops::DerefMut<Target = u64>
    where
        Self: 'a;
    fn new(v: u64) -> Self;
    fn read(&self) -> Self::ReadGuard<'_>;
    fn write(&self) -> Self::WriteGuard<'_>;
}

// —— std::sync::RwLock 适配（reader-preferring，会饿写者）——
struct StdRw(std::sync::RwLock<u64>);
impl RwLockApi for StdRw {
    type ReadGuard<'a> = std::sync::RwLockReadGuard<'a, u64>;
    type WriteGuard<'a> = std::sync::RwLockWriteGuard<'a, u64>;
    fn new(v: u64) -> Self {
        StdRw(std::sync::RwLock::new(v))
    }
    fn read(&self) -> Self::ReadGuard<'_> {
        self.0.read().unwrap()
    }
    fn write(&self) -> Self::WriteGuard<'_> {
        self.0.write().unwrap()
    }
}

// —— forge_sync::RwLock 适配（M7 写公平版）——
struct ForgeRw(forge_sync::rwlock::RwLock<u64>);
impl RwLockApi for ForgeRw {
    type ReadGuard<'a>
        = forge_sync::rwlock::ReadGuard<'a, u64>
    where
        Self: 'a;
    type WriteGuard<'a>
        = forge_sync::rwlock::WriteGuard<'a, u64>
    where
        Self: 'a;
    fn new(v: u64) -> Self {
        ForgeRw(forge_sync::rwlock::RwLock::new(v))
    }
    fn read(&self) -> Self::ReadGuard<'_> {
        self.0.read()
    }
    fn write(&self) -> Self::WriteGuard<'_> {
        self.0.write()
    }
}

/// 在 READER_THREADS 个读者死循环 read 的压力下，写者完成 WRITER_OPS 次写。
/// criterion 测这个函数的耗时：写者被饿 ⇒ 每次写等很久 ⇒ 总耗时长；
/// 写公平 ⇒ 总耗时短。两个 variant 的相对差距就是写饥饿的代价。
fn writer_does_n_writes<L: RwLockApi + Send + Sync + 'static>() {
    let rw = Arc::new(L::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    let readers: Vec<_> = (0..READER_THREADS)
        .map(|_| {
            let rw = rw.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let g = rw.read();
                    // 关键：读锁要持有"够长"——做一点活再放。
                    // 否则 hold 时间纳秒级，写者总能钻空子，饥饿不显形。
                    let mut acc = *g;
                    for _ in 0..256 {
                        acc = acc.wrapping_add(black_box(1));
                    }
                    black_box(acc);
                }
            })
        })
        .collect();

    for _ in 0..WRITER_OPS {
        let mut g = rw.write();
        *g += 1;
        black_box(*g);
    }

    stop.store(true, Ordering::Relaxed);
    for r in readers {
        r.join().unwrap();
    }
}

fn bench_writer_starvation(c: &mut Criterion) {
    let mut group = c.benchmark_group("writer_5000_writes_under_7_readers");
    group.bench_function("std_reader_preferring", |b| {
        b.iter_with_large_drop(|| writer_does_n_writes::<StdRw>());
    });
    group.bench_function("forge_write_fair", |b| {
        b.iter_with_large_drop(|| writer_does_n_writes::<ForgeRw>());
    });
    group.finish();
}

criterion_group!(benches, bench_writer_starvation);
criterion_main!(benches);
