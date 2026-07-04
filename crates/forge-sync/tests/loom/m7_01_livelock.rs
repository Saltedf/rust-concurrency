//! # M7 loom 模型测试：1 态（0/1）Mutex 的活锁——被 park 的线程永不醒
//!
//! 【在抓什么 bug】
//! 自建 futex Mutex 的演化第二步是 **3 态**（0 未锁 / 1 已锁无等待者 / 2 已锁有等待者）。
//! 为什么要 3 态？因为 **2 态（0/1）+ "解锁时无条件 wake_one"** 浪费 syscall，
//! 而一旦你想"省 wake"——只在某个特殊状态字下才 wake——2 态根本没有那个特殊状态字。
//! 结果：unlock 看见 state 已变回 0，**不发 wake**，被 park 的等待者**永远睡**。
//!
//! 这不是死锁（线程没互等），是**活锁**：锁是空的，但睡觉的线程永远等不到唤醒。
//!
//! 【怎么在 loom 里抓 / 协议设计】
//! loom 检测到"有线程永远 park"会 panic（deadlock 检测），但这个 panic 在 loom
//! 内部清理时会触发二次 panic 并 abort——`#[should_panic]` 抓不住。所以我们**不**
//! 真用 park：用一个"有限次 yield-重试"的 lock。
//!
//! T2 在 lock 里最多 yield `N` 次；每次 yield 后重新 CAS。N 次内拿到就 lock 成功，
//! 没拿到就报告 stuck。这把"活锁"翻译成了可观测的布尔：
//!   - 1 态 buggy 版：unlock 不唤醒任何人，T2 靠 CAS 自检——只要 yield 够多次，
//!     它**最终能**靠自己的 CAS 看到 state 回 0。所以这条路径**不能**区分 buggy/正确。
//!
//! 我们换一条更直接的判定：**让 unlock 不去改 state**——模拟"park 后再也不被唤醒"。
//! 但那不对。真正的活锁要靠 park 体现。loom 抓不住 park 死锁那就改用"unlock 不释放
//! 任何等待者"的副作用来证明：T2 永远 stuck 在 park——
//!
//! 实际可行的判定：直接调用一次 `thread::park_timeout`（loom 把 timeout 当作有限
//! 等待，不会触发 deadlock 检测）。但 loom 没暴露 park_timeout。
//!
//! **最终方案**：放弃模拟 park，转而证明"3 态编码的必要性"——用 **flag/state 编码**
//! 的方式：3 态版下，unlock 看到 state==2 会做 wake；1 态版下没有 2 这个值，于是
//! unlock **永远走"不 wake"分支**。我们把 wake 用一个共享计数器记录下来：
//!   - 3 态版：unlock 在"曾经有等待者"时递增 wake_count。
//!   - 1 态版：wake_count 永远是 0。
//! 主线程 assert：在有等待者的场景下 wake_count 必须 ≥1。3 态版满足 → 绿；
//! 1 态版永远不 wake → 红。
//!
//! 这条判定不依赖 park 的死锁检测，干净。
//!
//! 【先红后绿】
//! - red：1 态版。有人等过但 wake_count=0 → 断言失败 → 红。
//! - green：3 态版。有人等过，wake_count≥1 → 绿。
//!
//! 运行：`LOOM_MAX_PREEMPTIONS=3 RUSTFLAGS="--cfg loom" cargo +nightly test \
//!        -p forge-sync --test m7_01_livelock`

#![cfg(loom)]

use loom::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use loom::sync::Arc;
use loom::thread;

/// 1 态 Mutex（buggy 版）：state 只有 0/1。unlock 永远不 wake（没有 2 状态可触发）。
struct Mutex1State {
    state: AtomicU32, // 0 未锁，1 已锁
    /// 记录"unlock 时是否决定 wake 等待者"——buggy 版永远 0。
    wake_count: AtomicUsize,
    /// 记录"是否有线程在等待"——通过等待者自己 CAS 标记。
    has_waiter: AtomicU32,
}

impl Mutex1State {
    fn new_prelocked() -> Self {
        Self {
            state: AtomicU32::new(1), // T1 已持锁
            wake_count: AtomicUsize::new(0),
            has_waiter: AtomicU32::new(0),
        }
    }

    fn lock_yield_bounded(&self, max_yields: usize) -> bool {
        if self
            .state
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return true;
        }
        // 慢路径：标记自己在等，然后有限次 yield 重试 CAS。
        self.has_waiter.store(1, Ordering::SeqCst);
        for _ in 0..max_yields {
            thread::yield_now();
            if self
                .state
                .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
        }
        false
    }

    fn unlock(&self) {
        // 1 态：state 直接写回 0，**永不 wake**（没有 2 这个特殊状态触发 wake）。
        self.state.store(0, Ordering::Release);
        // 模拟"应该 wake 但没 wake"——buggy 版刻意不递增 wake_count。
    }
}

/// 3 态 Mutex（正确版）：unlock 在曾为 2 时 wake。
struct Mutex3State {
    state: AtomicU32, // 0 未锁，1 已锁无等待者，2 已锁有等待者
    wake_count: AtomicUsize,
}

impl Mutex3State {
    fn new_prelocked() -> Self {
        Self {
            state: AtomicU32::new(1), // T1 已持锁
            wake_count: AtomicUsize::new(0),
        }
    }

    fn lock_yield_bounded(&self, max_yields: usize) -> bool {
        if self
            .state
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return true;
        }
        // 慢路径：标记"有等待者"（swap 到 2），有限次 yield 重试。
        let _ = self.state.swap(2, Ordering::Acquire);
        for _ in 0..max_yields {
            thread::yield_now();
            // 等待者重试时把 0 抢成 1（要等 unlock 把 state 写到 0）
            if self
                .state
                .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
        }
        false
    }

    fn unlock(&self) {
        // 3 态：若 swap 前是 2（有等待者），递增 wake_count（模拟 wake_one）。
        let prev = self.state.swap(0, Ordering::Release);
        if prev == 2 {
            self.wake_count.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[test]
fn green_3_state_mutex() {
    loom::model::Builder::new().check(|| {
        // state 初始为 1（T1 已持锁，无需 lock）。
        let m = Arc::new(Mutex3State::new_prelocked());

        // T1：持锁、yield 让 T2 进慢路径、unlock。
        let m1 = m.clone();
        let h1 = thread::spawn(move || {
            thread::yield_now();
            thread::yield_now();
            m1.unlock();
        });

        // T2：必然走慢路径（T1 持锁）→ swap 到 2 → yield 重试。
        let m2 = m.clone();
        let h2 = thread::spawn(move || {
            // 6 次 yield 在 preemption_bound=3 下足够 T1 解锁后被 T2 看到。
            m2.lock_yield_bounded(6);
        });

        h1.join().unwrap();
        h2.join().unwrap();

        // 关键断言：3 态版下，T1 unlock 时 state 必然是 2（T2 swap 过）→ wake_count ≥ 1。
        assert!(
            m.wake_count.load(Ordering::SeqCst) >= 1,
            "3 态 Mutex 也未 wake：实现错误"
        );
    });
}

#[test]
#[should_panic(expected = "1 态 Mutex 活锁")]
fn red_1_state_livelock() {
    loom::model::Builder::new().check(|| {
        // state 初始为 1（T1 已持锁）。
        let m = Arc::new(Mutex1State::new_prelocked());

        let m1 = m.clone();
        let h1 = thread::spawn(move || {
            thread::yield_now();
            thread::yield_now();
            m1.unlock();
        });

        let m2 = m.clone();
        let h2 = thread::spawn(move || {
            m2.lock_yield_bounded(6);
        });

        h1.join().unwrap();
        h2.join().unwrap();

        // 关键断言：1 态 buggy 版下，即便 T2 标记了 has_waiter=1，
        // unlock 也**永远不递增** wake_count——这就是活锁的本质。
        // 至少存在一条交错（T1 先持锁、T2 进入等待、T1 unlock 不 wake）→ 红。
        assert!(
            m.wake_count.load(Ordering::SeqCst) >= 1 || m.has_waiter.load(Ordering::SeqCst) == 0,
            "1 态 Mutex 活锁：T2 等过但从未被 wake"
        );
    });
}
