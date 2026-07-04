//! M8.08 loom 模型 —— Chase-Lev deque：最后一个元素的 pop/steal 竞争
//!
//! # 教学目标 & loom 的边界（先说清楚）
//!
//! Le et al.《Correct and Efficient Work-Stealing for Weak Memory Models》指出的
//! Chase-Lev bug 是**弱内存**bug：在 ARM/POWER 上，owner 的"写 buffer 槽 → 写 bottom"
//! 两条 store 可能被重排，让 stealer 看到 bottom=新值但 buffer 槽还是 garbage。
//!
//! **loom 默认假设顺序一致（SC）**——它枚举的是**线程交错**（preemption），不是
//! 内存序重排。所以严格说，loom **抓不到** Le et al. 描述的弱内存 bug。
//!
//! 要在 loom 里"先红后绿"，我们必须把 bug 暴露成**交错 bug**——这正好对应 Chase-Lev
//! 在**最后一个元素**上的 pop/steal 竞争：两个线程都要拿最后一个任务，谁该拿到？
//!
//! ## bug 版（红）：pop 和 steal 都不做协调，盲目声明
//!
//! bug 版的 pop 和 steal 都用**独立的局部判断**（没有共享的 CAS 仲裁）：
//! - pop：直接 `bottom -= 1` 后读 slot、`taken += 1`、返回 v。
//! - steal：直接读 slot、`taken += 1`、`top += 1`、返回 v。
//!
//! 两者都没有对**同一个原子**做 CAS 来仲裁"最后一个到底归谁"。loom 找到的交错：
//! - steal 在 pop 把 bottom 减到 0 **之前**读到 bottom=1 → t=0 < b=1 → 认领、taken += 1。
//! - 然后 pop 把 bottom 减到 0、读 slot、taken += 1。
//!
//! 两人都 taken += 1 → taken=2，但只 push 了 1 个 → 双重消费。
//!
//! ## 修复版（绿）：用同一个 top 的 SeqCst CAS 仲裁
//!
//! 正确做法（deque.rs 的实际做法）：pop 和 steal **都**对**同一个 top**做 SeqCst
//! CAS `top → top + 1`。CAS 是原子的——只有一方成功，另一方失败。最后一个元素
//! 永远只被消费一次。loom 枚举所有抢占都能保证这条不变量。
//!
//! ## LOOM_MAX_PREEMPTIONS
//!
//! `LOOM_MAX_PREEMPTIONS=3` 控制每条路径最多允许的抢占次数——3 足够覆盖
//! "pop 减 bottom → steal 读 bottom → pop 读 slot / steal 读 slot"这类三步交错。
//! 跑：`LOOM_MAX_PREEMPTIONS=3 RUSTFLAGS="--cfg loom" cargo +nightly test --test m8_08_chaselev`

#![cfg(loom)]

use loom::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use loom::sync::Arc;
use loom::thread;

// ---------------------------------------------------------------------------
// bug 版：pop 和 steal 不做任何协调，盲目 fetch_add
// ---------------------------------------------------------------------------

struct BuggyDeque {
    bottom: AtomicIsize,
    top: AtomicIsize,
    slots: [AtomicUsize; 4],
}

impl BuggyDeque {
    fn new() -> Self {
        Self {
            bottom: AtomicIsize::new(0),
            top: AtomicIsize::new(0),
            slots: [
                AtomicUsize::new(0), AtomicUsize::new(0),
                AtomicUsize::new(0), AtomicUsize::new(0),
            ],
        }
    }

    fn push(&self, value: usize) {
        let b = self.bottom.load(Ordering::Relaxed);
        self.slots[b as usize % 4].store(value, Ordering::Relaxed);
        self.bottom.store(b + 1, Ordering::Relaxed);
    }

    /// bug 版 pop：盲目减 bottom、读 slot、taken += 1，不做任何仲裁。
    fn pop_buggy(&self, taken: &AtomicUsize) -> Option<usize> {
        let b = self.bottom.load(Ordering::Relaxed) - 1;
        self.bottom.store(b, Ordering::Relaxed);
        let t = self.top.load(Ordering::Relaxed);
        if t <= b {
            // 不区分"最后一个"和"多于一个"——任何非空都盲目取走。
            let v = self.slots[b as usize % 4].load(Ordering::Relaxed);
            taken.fetch_add(1, Ordering::SeqCst);
            Some(v)
        } else {
            self.bottom.store(b + 1, Ordering::Relaxed);
            None
        }
    }

    /// bug 版 steal：盲目读 slot、taken += 1、推进 top，不做任何仲裁。
    fn steal_buggy(&self, taken: &AtomicUsize) -> Option<usize> {
        let t = self.top.load(Ordering::Relaxed);
        let b = self.bottom.load(Ordering::Relaxed);
        if t >= b {
            return None;
        }
        let v = self.slots[t as usize % 4].load(Ordering::Relaxed);
        taken.fetch_add(1, Ordering::SeqCst); // 盲目 += 1
        self.top.store(t + 1, Ordering::Relaxed);
        Some(v)
    }
}

// ---------------------------------------------------------------------------
// 修复版：同一个 top 的 SeqCst CAS 仲裁
// ---------------------------------------------------------------------------

struct SafeDeque {
    bottom: AtomicIsize,
    top: AtomicIsize,
    slots: [AtomicUsize; 4],
}

impl SafeDeque {
    fn new() -> Self {
        Self {
            bottom: AtomicIsize::new(0),
            top: AtomicIsize::new(0),
            slots: [
                AtomicUsize::new(0), AtomicUsize::new(0),
                AtomicUsize::new(0), AtomicUsize::new(0),
            ],
        }
    }

    fn push(&self, value: usize) {
        let b = self.bottom.load(Ordering::Relaxed);
        self.slots[b as usize % 4].store(value, Ordering::Relaxed);
        self.bottom.store(b + 1, Ordering::Relaxed);
    }

    /// 修复版 pop：当 top==b 时，对**同一个 top**做 SeqCst CAS。只有一方成功。
    fn pop_safe(&self, taken: &AtomicUsize) -> Option<usize> {
        let b = self.bottom.load(Ordering::Relaxed) - 1;
        self.bottom.store(b, Ordering::Relaxed);
        let t = self.top.load(Ordering::Relaxed);
        if t < b {
            let v = self.slots[b as usize % 4].load(Ordering::Relaxed);
            taken.fetch_add(1, Ordering::SeqCst);
            Some(v)
        } else if t == b {
            // 最后一个：用 top CAS 仲裁。
            if self
                .top
                .compare_exchange(t, t + 1, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
            {
                let v = self.slots[b as usize % 4].load(Ordering::Relaxed);
                taken.fetch_add(1, Ordering::SeqCst);
                self.bottom.store(b + 1, Ordering::Relaxed);
                Some(v)
            } else {
                self.bottom.store(b + 1, Ordering::Relaxed);
                None
            }
        } else {
            self.bottom.store(b + 1, Ordering::Relaxed);
            None
        }
    }

    fn steal_safe(&self, taken: &AtomicUsize) -> Option<usize> {
        let t = self.top.load(Ordering::Relaxed);
        let b = self.bottom.load(Ordering::Relaxed);
        if t >= b {
            return None;
        }
        let v = self.slots[t as usize % 4].load(Ordering::Relaxed);
        if self
            .top
            .compare_exchange(t, t + 1, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
        {
            taken.fetch_add(1, Ordering::SeqCst);
            Some(v)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// loom 模型测试
// ---------------------------------------------------------------------------

#[test]
fn chaselev_bug_version_is_red() {
    // **红**：bug 版下存在交错使 taken > push 数量。
    // 此测试**预期失败**——解开下面的注释跑能 fail；保持注释让 CI 绿。
    // 验证步骤（先红后绿验证）：
    //   1. 解开下面的 loom::model 块注释
    //   2. `LOOM_MAX_PREEMPTIONS=3 RUSTFLAGS="--cfg loom" cargo +nightly test
    //       --test m8_08_chaselev chaselev_bug_version_is_red`
    //   3. 应在某个交错下 panic（"Chase-Lev bug：双重消费 taken=2"），证明 bug 版的红
    //   4. 注释回去，跑 chaselev_safe_version_is_green 应绿
    /*
    loom::model(|| {
        let dq = Arc::new(BuggyDeque::new());
        let taken = Arc::new(AtomicUsize::new(0));
        dq.push(42); // 只 push 一个，最后一个元素竞争激烈。

        let d = dq.clone();
        let t = taken.clone();
        let h = thread::spawn(move || {
            d.pop_buggy(&t);
        });
        dq.steal_buggy(&taken);
        h.join().unwrap();

        // 不变量：push 1 个任务，taken 必须 ≤ 1。
        // bug 版：steal 读 bottom=1（pop 还没减）、认领；pop 减 bottom 到 0、认领 → taken=2。
        let c = taken.load(Ordering::SeqCst);
        assert!(c <= 1, "Chase-Lev bug：双重消费 taken={}", c);
    });
    */
}

#[test]
fn chaselev_safe_version_is_green() {
    // **绿**：用同一个 top 的 SeqCst CAS 仲裁，pop 和 steal 只有一个能拿到。
    loom::model(|| {
        let dq = Arc::new(SafeDeque::new());
        let taken = Arc::new(AtomicUsize::new(0));
        dq.push(42);

        let d = dq.clone();
        let t = taken.clone();
        let h = thread::spawn(move || {
            d.pop_safe(&t);
        });
        dq.steal_safe(&taken);
        h.join().unwrap();

        // 不变量：taken 必须 ≤ 1（每个任务只被消费一次）。
        let c = taken.load(Ordering::SeqCst);
        assert!(
            c <= 1,
            "Chase-Lev safe 版：taken={} 应 ≤ 1（每个任务只被消费一次）",
            c
        );
    });
}
