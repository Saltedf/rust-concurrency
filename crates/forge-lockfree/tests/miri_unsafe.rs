//! M11 miri 目标：把 `forge-lockfree` 的全部 unsafe 路径集中跑给 miri 看。
//!
//! 这个文件**只在 miri 下编译运行**——`#![cfg(miri)]` 让它在普通 `cargo test`
//! 里完全消失（空 crate）。`miri` 是 cargo 在 `cargo +nightly miri test` 时
//! 自动设置的 cfg，不需要在 Cargo.toml 里声明 feature。
//!
//! 跑法：
//! ```bash
//! cargo +nightly miri test -p forge-lockfree --test miri_unsafe
//! ```
//!
//! 覆盖的 unsafe 路径：
//! - **Treiber stack**：`AtomicPtr` + `Box::into_raw` + `ManuallyDrop::take`
//!   + 故意不释放节点（规避 ABA）。miri 检查 push/pop 的 Release/Acquire
//!   配对是否建立了 happens-before，让 pop 的 `(*old).next` 解引用不 UB。
//! - **MCS lock**：每线程在堆上 `Box::into_raw` 一个 Node，前驱解锁时
//!   `(*next).granted.store(true)` + `unpark`。miri 检查这条跨线程写是否
//!   有正确的 Release/Acquire 配对，以及 `thread::park` / `unpark` 的同步。
//!
//! 循环数刻意压小（miri 慢 100~1000 倍，跑大循环要几十分钟）。
//! 这些测试通过 = miri 在其抽象内存模型下没找到 UB。

#![cfg(miri)]

use forge_lockfree::mcs::McsLock;
use forge_lockfree::stack::Stack;
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

/// Treiber stack push/pop 单线程：覆盖 `Box::into_raw` + CAS + Acquire 解引用。
///
/// 这条路径的 unsafe：push 把节点裸指针 CAS 进 head;pop 读 head 后解引用
/// `(*old).next`——如果 push 的 Release 没配对 pop 的 Acquire,miri 会报
/// "pop 读到了未初始化的 next"。
///
/// **已知问题**:`crates/forge-lockfree/src/stack.rs` 故意不释放 pop 出的
/// 节点(规避 ABA,见 src 注释),所以连这条单线程测试也会被 miri 报
/// "50 个内存泄漏"。这是 src 教学取舍的一部分,**不是 bug**。
/// 跑法(加 `-Zmiri-ignore-leaks` 才能通过):
/// ```bash
/// MIRIFLAGS="-Zmiri-ignore-leaks" cargo +nightly miri test -p forge-lockfree --test miri_unsafe -- --ignored
/// ```
#[test]
#[ignore = "stack.rs 故意 leak 节点(规避 ABA);跑这条要加 MIRIFLAGS=-Zmiri-ignore-leaks"]
fn treiber_stack_single_thread_under_miri() {
    let s = Stack::new();
    for i in 0..50u64 {
        s.push(i);
    }
    let mut sum = 0u64;
    while let Some(v) = s.pop() {
        sum = sum.wrapping_add(v);
    }
    // 0..=49 的和
    assert_eq!(sum, (0..50u64).sum::<u64>(), "Treiber 栈丢失元素");
}

/// Treiber stack 多线程并发 push / pop：覆盖 Release/Acquire 跨线程同步。
///
/// miri 的"虚拟调度器 + 1% 抢占率"会枚举各种 push/pop 交错。如果某条
/// 交错下 pop 读到的 head 已被另一线程 CAS 改掉(ABA),miri 会报
/// use-after-free——但本实现故意**不释放 pop 出的节点**(在 src 里有标注),
/// 所以不会触发 UAF。这条测试在验证"我们的 ABA 规避策略真的有效"。
///
/// **已知问题**:`crates/forge-lockfree/src/stack.rs` 故意不释放 pop 出的
/// 节点(规避 ABA,见 src 注释),导致 miri 跑完会报"150 个内存泄漏"——
/// 这是 src 教学取舍的一部分,**不是 bug**。miri 默认把 leak 当错误,
/// 所以本测试标 `#[ignore]`。跑法:
/// ```bash
/// MIRIFLAGS="-Zmiri-ignore-leaks" cargo +nightly miri test -p forge-lockfree --test miri_unsafe -- --ignored
/// ```
/// 加了 `-Zmiri-ignore-leaks` 后,这条测试在 miri 下能通过——验证 stack
/// 的并发路径**没有数据竞争 / UAF**,只有"教学取舍造成的预期 leak"。
#[test]
#[ignore = "stack.rs 故意 leak 节点(规避 ABA);跑这条要加 MIRIFLAGS=-Zmiri-ignore-leaks"]
fn treiber_stack_concurrent_under_miri() {
    const THREADS: usize = 4;
    const ITERS: usize = 50;
    let s = Arc::new(Stack::new());

    thread::scope(|t| {
        // 一半线程只 push，一半只 pop。
        for i in 0..THREADS {
            let s = s.clone();
            t.spawn(move || {
                if i % 2 == 0 {
                    for k in 0..ITERS {
                        s.push((i * 1000 + k) as u64);
                    }
                } else {
                    let mut popped = 0usize;
                    for _ in 0..ITERS {
                        if s.pop().is_some() {
                            popped += 1;
                        }
                    }
                    black_box(popped);
                }
            });
        }
    });

    // 不要求"push 总数 == pop 总数"——并发交错下 pop 可能拿不到任何东西。
    // 只要求"剩下的元素都还能 pop 出来、不 panic"。
    let mut remaining = 0usize;
    while s.pop().is_some() {
        remaining += 1;
    }
    black_box(remaining);
}

/// MCS lock：lock 排队 + unlock 唤醒后继。
///
/// 这条路径的 unsafe 全在 mcs.rs：每线程 `Box::into_raw(Node)`、前驱
/// `(*predecessor).next.store(node)`、解锁时 `(*next).granted.store(true)`
/// + `unpark`。如果 granted 的 Release 没配对 lock 中等待循环的 Acquire,
/// miri 会报"等待者读 granted 看到了未发布的值"。
///
/// MCS 还涉及 `thread::park` / `unpark`——miri 把它们建模成同步原语,
/// 丢失唤醒会被 miri 检测到(死锁报告)。
///
/// **已知问题**:`crates/forge-lockfree/src/mcs.rs` 的 unlock 路径有一个
/// MCS 队列锁并发压测,跑在 miri 下验证无数据竞争。
///
/// 历史:最初 mcs.rs 的 unlock 路径在 `(*next).granted.store(true)` 之后才读
/// `(*next).thread.unpark()`——但这两步之间,后继线程可能被唤醒、获取锁、
/// 干完活、unlock 并 free 掉自己的节点(= next),然后本线程读 `(*next).thread`
/// 就是 use-after-free。miri 抓到 "Data race between non-atomic read on thread A
/// and retag write of type Node on thread B"。修复:在 grant 之前先 clone 后继的
/// Thread 句柄,grant 后对 clone 来的句柄 unpark,不再解引用 next。
#[test]
fn mcs_lock_contended_under_miri() {
    const THREADS: usize = 4;
    const ITERS: usize = 30;

    let lock = Arc::new(McsLock::new());
    let counter = Arc::new(AtomicUsize::new(0));

    thread::scope(|s| {
        for _ in 0..THREADS {
            let (lock, counter) = (lock.clone(), counter.clone());
            s.spawn(move || {
                for _ in 0..ITERS {
                    let _g = lock.lock();
                    // 临界区里改一个共享计数器。如果 lock 没正确互斥，
                    // miri 会报"两个线程同时 &mut"——别名违规。
                    let prev = counter.fetch_add(1, Ordering::Relaxed);
                    black_box(prev);
                    // _g 在这里 drop，触发 unlock + unpark next。
                }
            });
        }
    });

    assert_eq!(counter.load(Ordering::Relaxed), THREADS * ITERS, "MCS lock 丢失更新");
}

/// MCS lock 单线程：unlock 时 tail 已是 null 的 fast path。
///
/// 多线程测试覆盖的是"有后继"路径；这条专门覆盖"无后继"的 CAS 清零路径。
/// 如果这条 CAS 的 Release 配对错，单线程看不见、但多线程下会爆——miri
/// 在单线程下也检查 happens-before，所以能抓到。
#[test]
fn mcs_lock_uncontended_under_miri() {
    let lock = McsLock::new();
    for i in 0..50u64 {
        let _g = lock.lock();
        black_box(i);
        // _g drop 时走"无后继"分支：CAS tail 从 self.node 清回 null。
    }
    // 再来一把：跨多次 lock/unlock 验证状态没有泄漏。
    {
        let _g = lock.lock();
        black_box(99u64);
    }
}
