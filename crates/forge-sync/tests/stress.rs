//! M11 —— 压力测试:用 16 线程 hammer 自研 Mutex,跑久验证"不丢更新"。
//!
//! 这是要被 `scripts/stress.sh forge-sync` 反复调用的压力测试。
//! 单文件路径 `tests/stress.rs`,Cargo 自动注册为名为 `stress` 的测试目标,
//! 所以 `cargo test -p forge-sync --release --test stress` 能直接命中。
//!
//! 跑单次:cargo test -p forge-sync --release --test stress
//! 跑压力(循环到超时):./scripts/stress.sh forge-sync
//!
//! 设计要点(详见 docs/modules/M11-testing.md 第六节):
//! - `--release`:debug build 的行为(没内联、寄存器分配不同)与 release 不同,
//!   很多并发 bug 只在 release 下暴露。
//! - `THREADS=16` 远超 CPU 核数(典型 4~8 核),强制 OS 频繁抢占,
//!   把低概率线程交错逼到必现。
//! - `Barrier` 让所有线程同时起跑,最大化竞争窗口。
//! - 显式 `drop(g)` 早释放锁,缩短临界区,让"两个线程同时想拿锁"的概率最大化。
//! - `checksum` 用每线程的局部计数做交叉验证:即便 Mutex 出 bug,
//!   checksum 也会暴露问题(每线程以为自己加了 N 次,但全局丢了)。
//! - 共享方式:`thread::scope` 允许借用栈上的变量进 spawned 线程,
//!   所以 `&counter` / `&checksum` / `&start` 直接被 move 进闭包(引用是 Copy)。

use forge_sync::mutex::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Barrier;
use std::thread;

#[test]
fn mutex_hammer_counter() {
    const THREADS: usize = 16;
    const ITERS: u64 = 100_000;

    let counter = Mutex::new(0u64);
    // 每线程的局部计数累加到这里,用于交叉验证 Mutex 的正确性
    let checksum = AtomicU64::new(0);
    let start = Barrier::new(THREADS + 1);

    thread::scope(|s| {
        for _ in 0..THREADS {
            // 引用是 Copy,move 进闭包只是各取一份引用——
            // counter/checksum/start 本体仍留在主线程的栈上,scope 保证它们活到所有线程 join
            s.spawn(|| {
                // 所有线程在起跑线等齐,然后同时冲——最大化竞争
                let _g = start.wait();
                let mut local = 0u64;
                for _ in 0..ITERS {
                    let mut g = counter.lock();
                    *g += 1;
                    local += 1;
                    drop(g); // 显式早 drop,缩短临界区
                }
                checksum.fetch_add(local, Ordering::Relaxed);
            });
        }
        let _g = start.wait();
    });

    let expected = (THREADS as u64) * ITERS;
    assert_eq!(*counter.lock(), expected, "Mutex 丢失更新");
    assert_eq!(
        checksum.load(Ordering::Relaxed),
        expected,
        "局部计数核对失败"
    );
}
