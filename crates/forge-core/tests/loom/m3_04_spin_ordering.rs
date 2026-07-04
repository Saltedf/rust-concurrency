//! # M3 loom 模型测试：SpinLock 用 Relaxed 排序时，T2 拿到锁却读到旧数据
//!
//! 【在抓什么 bug】
//! SpinLock 的契约：A 在临界区里写 T，解锁；B 后续**观测到锁已释放**、并取到锁后，
//! **必须看到** A 的写。这靠 lock/unlock 那对原子操作建立 happens-before：
//!   - unlock 用 `Release`：把本临界区里对 T 的写"发布"出去。
//!   - lock 用 `Acquire`：与 unlock 的 Release 配对，建立 HB 边。
//!
//! 如果 unlock 用 `Relaxed`、lock 也用 `Relaxed`，那么 B 读到"已解锁"状态后去读 T，
//! **并不保证**能看到 A 写的 T——loom 会枚举出"A 解锁、B 读到解锁状态、B 读到旧 T"
//! 的交错。
//!
//! 【协议设计 / 避免自旋的状态爆炸】
//! loom 是有限状态模型检查器，真自旋会让状态空间爆炸。所以这里**不**自旋：
//! - T1 是当前持锁者（`locked` 初始为 true），先写 value=42，再 `unlock`。
//! - T2 用一次 `load(lock_ord)`：若仍 locked（T1 还没 unlock）→ 什么也不读，没问题；
//!   若 unlocked（T1 已 unlock）→ 此时读 value，**必须**看到 42——这正是排序的用武之地。
//!
//! 注意：这个"单次 load"是 SpinLock 的退化场景——它对应"T2 进入 lock() 时第一次
//! swap 就抢到锁"的快速路径，没有自旋。但 happens-before 的建立条件不变：
//! "T2 读到 T1 写的 unlocked 值" 这一对 load/store 的 ordering 决定了 value 的可见性。
//! 这与完整 SpinLock 的核心排序事实完全一致。
//!
//! 同一份协议，用 lock_ord / unlock_ord 决定排序：
//!   buggy 版：全 Relaxed。loom 找到"T2 读到 unlocked 但 value 未传播"的交错 → 红。
//!   正确版：load 用 Acquire、store 用 Release。HB 接通 → 绿。
//!
//! 运行：`LOOM_MAX_PREEMPTIONS=3 RUSTFLAGS="--cfg loom" cargo +nightly test \
//!        -p forge-core --test m3_04_spin_ordering`

#![cfg(loom)]

use loom::cell::UnsafeCell;
use loom::sync::atomic::{AtomicBool, Ordering};
use loom::sync::Arc;
use loom::thread;

/// 一个最小自旋锁（不真自旋，只暴露 lock/unlock 的状态字），便于在测试里改排序。
struct MiniSpin {
    locked: AtomicBool,
    value: UnsafeCell<u64>,
}

unsafe impl Send for MiniSpin {}
unsafe impl Sync for MiniSpin {}

impl MiniSpin {
    fn new_prelocked() -> Self {
        Self {
            locked: AtomicBool::new(true), // T1 一开始就持锁
            value: UnsafeCell::new(0),
        }
    }
}

/// 标准协议：T1（已持锁）写 42、解锁；T2 load 锁状态，若已解锁则读 value 必须看到 42。
/// 用 lock_ord / unlock_ord 决定排序——同一份协议，不同排序就是不同版本。
fn run_protocol(spin: Arc<MiniSpin>, lock_ord: Ordering, unlock_ord: Ordering) {
    let t = spin.clone();
    let h = thread::spawn(move || {
        // T1：已持锁（prelocked）。写 42，再 unlock。
        // 关键：unlock 之前写 value——排序是否能在跨线程拉起 HB，全看 unlock/lock 这对。
        t.value.with_mut(|p| unsafe {
            *p = 42;
        });
        t.locked.store(false, unlock_ord);
    });

    // T2（本线程）：load 锁状态——这是 SpinLock::lock 在 swap 路径上的核心读。
    // 看到 false（已解锁）等价于"swap 抢到了锁"那条路径。
    let unlocked = !spin.locked.load(lock_ord);
    if unlocked {
        // 关键：此时 T2 进入临界区。value 必须是 T1 写的 42。
        let seen = spin.value.with(|p| unsafe { *p });
        //   buggy 版（Relaxed）：loom 找到"T2 读到解锁状态、但 value 写尚未传播"
        //     的交错 → 断言失败 → 红。
        //   正确版（Acquire/Release）：HB 接通 → 绿。
        assert_eq!(seen, 42, "拿到锁却读不到上一个临界区的写：缺少 Acquire/Release");
    }

    h.join().unwrap();
}

#[test]
fn green_correct_acquire_release() {
    let lo = Ordering::Acquire;
    let uo = Ordering::Release;
    loom::model::Builder::new().check(move || {
        run_protocol(Arc::new(MiniSpin::new_prelocked()), lo, uo);
    });
}

#[test]
#[should_panic]
fn red_buggy_relaxed() {
    let lo = Ordering::Relaxed;
    let uo = Ordering::Relaxed;
    loom::model::Builder::new().check(move || {
        run_protocol(Arc::new(MiniSpin::new_prelocked()), lo, uo);
    });
}
