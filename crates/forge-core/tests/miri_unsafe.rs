//! M11 miri 目标：把 `forge-core` 的全部 unsafe 路径集中跑给 miri 看。
//!
//! 这个文件**只在 miri 下编译运行**——`#![cfg(miri)]` 让它在普通 `cargo test`
//! 里完全消失（空 crate）。`miri` 是 cargo 在 `cargo +nightly miri test` 时
//! 自动设置的 cfg，不需要在 Cargo.toml 里声明 feature。
//!
//! 跑法：
//! ```bash
//! cargo +nightly miri test -p forge-core --test miri_unsafe
//! ```
//!
//! 为什么不直接跑 `cargo +nightly miri test -p forge-core`？
//! - `m4_04_stress` 在 miri 下要跑几分钟（miri 慢 100~1000 倍），
//!   这个文件刻意只跑"刚够触发每条 unsafe 路径"的小循环（≤ 200 次），
//!   把 miri 单次跑时间压在秒级。
//! - 这个文件是"快速回归"——CI 上每 PR 跑一次；想跑全套 miri 就跑各模块
//!   已有的 stress 测试（m4_04_stress 等）。
//!
//! 每个测试都要做到：**真的进入 unsafe 块**，而不是只构造一个安全值就返回。
//! miri 抓的是 unsafe 内部的 UB（数据竞争、use-after-free、别名违规），
//! 如果 unsafe 路径没被触发，miri 等于白跑。

#![cfg(miri)]

use forge_core::arc::Arc;
use forge_core::spin::SpinLock;
use std::thread;

/// M4 Arc：clone/drop/upgrade/downgrade 的并发混跑。
///
/// 这是 forge-core 里最微妙的 unsafe——两个原子计数器 + `ManuallyDrop`
/// + `Box::from_raw`。任何一个内存序用错（比如 `Clone::clone` 的
/// `fetch_add` 不是 Relaxed 而是被某种重排拖累），miri 都会报"数据竞争：
/// Weak::drop 的 Release-store 和 Arc::clone 的 Relaxed-load 没有
/// happens-before 关系"。
///
/// miri 不开真线程——它用"虚拟调度器 + 1% 抢占率"模拟多线程。所以这里
/// 把循环数压在 50 次（miri 下 50 次 × 4 线程 ≈ 几秒），既够触发各种交错，
/// 又不会跑太久。
#[test]
fn arc_clone_drop_stress_under_miri() {
    const THREADS: usize = 4;
    const ITERS: usize = 50;

    // 关键：把强 Arc 在 scope 外立刻 drop 掉，只留 weak——
    // 这样 worker 线程们才能检验"在没有任何 Arc 的状态下，upgrade 应当失败"。
    let weak = {
        let arc = Arc::new(42usize);
        let weak = Arc::downgrade(&arc);
        // 先在 arc 还活着时让多线程跑一圈——这一段覆盖"有 Arc 的正常路径"。
        thread::scope(|s| {
            for _ in 0..THREADS {
                let arc = arc.clone();
                let weak = weak.clone();
                s.spawn(move || {
                    for _ in 0..ITERS {
                        // 三种操作混跑：clone Arc 再 drop、clone Weak 再 drop、
                        // upgrade 成 Arc 再 drop。这覆盖了 ArcData<T> 上的全部
                        // 读写路径——data_ref_count / alloc_ref_count 的全部
                        // fetch_add / fetch_sub / CAS。
                        let _a = arc.clone();
                        let _a2 = weak.upgrade();
                        let _w = weak.clone();
                    }
                });
            }
        });
        // arc 在这里出 scope 时仍然活着（被外层持有），所以这一段
        // 测试的是"数据一直存在、所有操作都不应该让它提前 drop"。
        weak
    };
    // 现在 arc 已 drop，所有 worker clone 的 Arc / Weak 也都已 drop。
    // 剩下唯一活着的引用是这个 weak。它的 upgrade 应当返回 None——
    // 因为 data_ref_count 已经到 0、数据已被释放。
    assert!(weak.upgrade().is_none(), "Arc 数据未被释放");
}

/// M4 `Arc::get_mut`：唯一性检查的 unsafe 路径。
///
/// `get_mut` 内部走 `compare_exchange(1, usize::MAX, Acquire, Relaxed)`
/// 把 alloc_ref_count "锁住"，读 data_ref_count，再解锁，最后 fence(Acquire)
/// + `&mut *self.data().data.get()`。这一步的裸指针解引用如果时序错（比如
/// fence 漏了），miri 会立刻报"与并发的 Clone::clone 的 fetch_add 没有
/// happens-before"。
#[test]
fn arc_get_mut_under_miri() {
    // 先正常 clone/drop 几次，让 ArcData 状态走起来。
    let mut arc = Arc::new(String::from("hello"));
    {
        let a2 = arc.clone();
        let a3 = arc.clone();
        drop(a2);
        drop(a3);
    }
    // 此时 data_ref_count 回到 1、alloc_ref_count 也是 1，get_mut 应当成功。
    if let Some(s) = Arc::get_mut(&mut arc) {
        s.push_str(" miri");
    }
    assert_eq!(&*arc, "hello miri");
}

/// M3 SpinLock：lock / DerefMut / Drop(unlock) 的完整临界区。
///
/// 这条路径上的 unsafe：Guard::deref_mut 解引用 `UnsafeCell::get()`，
/// Drop::drop 做 Release-store。如果 unlock 用 Relaxed（M3 文档里讲过的
/// 经典 bug），miri 会立刻报"下个 lock 的 Acquire-load 和上个临界区的写
/// 没有同步关系"——也就是数据竞争。
#[test]
fn spinlock_critical_section_under_miri() {
    const THREADS: usize = 4;
    const ITERS: usize = 50;

    let lock = SpinLock::new(0usize);
    thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                for _ in 0..ITERS {
                    let mut g = lock.lock();
                    // 这里 deref_mut 进 unsafe 路径——读旧值、+1、写回。
                    *g += 1;
                    // 显式早 drop，让 unlock 的 Release-store 在循环里独立发生。
                    drop(g);
                }
            });
        }
    });

    assert_eq!(*lock.lock(), THREADS * ITERS, "SpinLock 丢失更新");
}

/// M3 SpinLock：验证 Guard 的 Deref（不可变）也能正确同步。
///
/// deref 和 deref_mut 走的是同一个 unsafe 解引用，但 deref 不写——
/// 如果某条 unsafe 路径只在"读"时触发 bug，单跑 deref_mut 看不到。
#[test]
fn spinlock_read_only_critical_section_under_miri() {
    let lock = SpinLock::new(vec![1u32, 2, 3, 4, 5]);

    // 主线程写一次。
    {
        let mut g = lock.lock();
        g.push(6);
    }

    // 多个线程只读，验证读路径不发生别名违规。
    thread::scope(|s| {
        for _ in 0..3 {
            s.spawn(|| {
                let g = lock.lock();
                // 黑盒读，防止优化掉。
                let _ = black_box_read(&*g);
            });
        }
    });

    fn black_box_read(v: &[u32]) -> u32 {
        // 触发对内部数据的读取——这一步在 miri 下会校验
        // "这条 &T 的借用在 lock 释放前一直合法"。
        let mut sum = 0u32;
        for &x in v {
            sum = sum.wrapping_add(x);
        }
        sum
    }

    assert_eq!(lock.lock().len(), 6);
}

/// 多线程同时 hammer 一把锁 + 同时通过 Arc 共享所有权，覆盖两条 unsafe
/// 路径交织的最坏情况。这条是上面两个测试的"组合拳"——miri 在这条里
/// 抓到 bug 的概率最高（因为交错更多）。
#[test]
fn arc_and_spinlock_together_under_miri() {
    const THREADS: usize = 3;
    const ITERS: usize = 30;

    // Arc<SpinLock<usize>>：共享所有权 + 共享可变状态。
    let arc = Arc::new(SpinLock::new(0usize));

    thread::scope(|s| {
        for _ in 0..THREADS {
            let arc = arc.clone();
            s.spawn(move || {
                for _ in 0..ITERS {
                    // 每次循环都 clone + drop Arc，外加 lock + 写 + unlock。
                    // 这把"计数器同步"和"临界区同步"两条 unsafe 链
                    // 全部塞进同一个线程交错。
                    let _a = arc.clone();
                    let mut g = arc.lock();
                    *g += 1;
                    drop(g);
                    drop(_a);
                }
            });
        }
    });

    assert_eq!(*arc.lock(), THREADS * ITERS);
}
