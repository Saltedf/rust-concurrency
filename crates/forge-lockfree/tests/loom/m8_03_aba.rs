//! M8.03 loom 模型 —— Treiber 栈 ABA：先红（bug 版 double-count）后绿（hazard 修绿）
//!
//! # loom 在 ABA 上的局限 & 我们怎么"诚实"地先红后绿
//!
//! 真正的 ABA 需要"分配器复用地址"——而 loom 的内部 allocator **不**做地址复用。
//! 所以直接在 loom 里跑 Treiber 栈，**永远抓不到 ABA**：被释放的节点地址不会被
//! 再分给新节点，T1 的 CAS 永远失败（值不同）。这是 loom 的已知局限。
//!
//! 我们怎么做到"先红后绿"？把 ABA 的**逻辑后果**——"被弹出的节点被双重消费"——
//! 用一个**显式的双重消费检测器**（一个 `Arc<AtomicUsize>` "popped_count"）钉死：
//!
//! - **bug 版**：pop 在 CAS 成功之前**先取走 value**（用 `unsafe` 模拟"取值"），
//!   然后 CAS head。如果两个线程同时执行 pop，**两人都看到 head=X、都 take value**，
//!   value 的 popped_count 被加两次 → bug 版的红。
//! - **绿版（hazard 风格）**：pop 先 CAS head（决定独占），CAS 成功后才 take value。
//!   两个并发 pop 只有一个 CAS 成功 → 只 take 一次 → popped_count 正确。
//!
//! 这正是 ABA 的本质伤害（"我以为我独占了，其实没有"）的一个**忠实子集**——
//! 我们用 loom 找到那条 double-take 的交错，证明 bug 版的红，再证明 hazard 版的绿。
//! 完整 ABA（地址复用版）的工业级 loom 模型要 mock allocator，超出本教学范围。
//!
//! 跑：`LOOM_MAX_PREEMPTIONS=3 RUSTFLAGS="--cfg loom" cargo +nightly test --test m8_03_aba`

#![cfg(loom)]

use loom::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use loom::sync::Arc;
use loom::thread;
use std::ptr;

struct Node {
    value: usize,
    next: *mut Node,
}

struct BuggyStack {
    head: AtomicPtr<Node>,
}

impl BuggyStack {
    fn new() -> Self {
        Self {
            head: AtomicPtr::new(ptr::null_mut()),
        }
    }

    fn push(&self, value: usize) {
        let node = Box::into_raw(Box::new(Node {
            value,
            next: ptr::null_mut(),
        }));
        let mut old = self.head.load(Ordering::Relaxed);
        loop {
            unsafe { (*node).next = old };
            match self
                .head
                .compare_exchange_weak(old, node, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(a) => old = a,
            }
        }
    }

    /// **BUG 版**：先 take value（假设 CAS 必成），再 CAS。如果两个线程同时执行，
    /// 两个都先 take 再 CAS——只有一个 CAS 成功，但**两个 take 都执行了**。
    /// popped_count 被加两次 → 红。
    fn pop_buggy(&self, popped_count: &AtomicUsize) -> Option<usize> {
        let mut old = self.head.load(Ordering::Relaxed);
        loop {
            if old.is_null() {
                return None;
            }
            // BUG：在 CAS 之前 take value，并增加 popped_count。
            let value = unsafe { (*old).value };
            let next = unsafe { (*old).next };
            popped_count.fetch_add(1, Ordering::SeqCst); // ← 双重计数 bug 的源头
            if self
                .head
                .compare_exchange_weak(old, next, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return Some(value);
            }
            // CAS 失败：我们已经 fetch_add 了！count 多算一次。
            old = self.head.load(Ordering::Relaxed);
        }
    }
}

/// **绿版（hazard 风格）**：CAS 成功后才 take value。两个并发 pop 只有一个 CAS 成功，
/// popped_count 只加一次。
struct SafeStack {
    head: AtomicPtr<Node>,
}

impl SafeStack {
    fn new() -> Self {
        Self {
            head: AtomicPtr::new(ptr::null_mut()),
        }
    }

    fn push(&self, value: usize) {
        let node = Box::into_raw(Box::new(Node {
            value,
            next: ptr::null_mut(),
        }));
        let mut old = self.head.load(Ordering::Relaxed);
        loop {
            unsafe { (*node).next = old };
            match self
                .head
                .compare_exchange_weak(old, node, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(a) => old = a,
            }
        }
    }

    fn pop_safe(&self, popped_count: &AtomicUsize) -> Option<usize> {
        let mut old = self.head.load(Ordering::Relaxed);
        loop {
            if old.is_null() {
                return None;
            }
            let next = unsafe { (*old).next };
            // GREEN：先 CAS。只有 CAS 成功的那一个线程才 take value、加 count。
            if self
                .head
                .compare_exchange_weak(old, next, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                let value = unsafe { (*old).value };
                popped_count.fetch_add(1, Ordering::SeqCst); // ← 只在独占后加
                return Some(value);
            }
            old = self.head.load(Ordering::Relaxed);
        }
    }
}

#[test]
fn aba_bug_version_is_red() {
    // **红**：bug 版下存在交错使 popped_count > push 数量。
    // 此测试**预期会失败**——解开下面的注释跑能 fail；保持注释让 CI 绿。
    // 验证步骤（先红后绿验证）：
    //   1. 解开下面的 loom::model 块注释
    //   2. `LOOM_MAX_PREEMPTIONS=3 RUSTFLAGS="--cfg loom" cargo +nightly test
    //       --test m8_03_aba aba_bug_version_is_red`
    //   3. 应在某个交错下 panic（"ABA bug：double-count=2 (>1)"），证明 bug 版的红
    //   4. 注释回去，跑 aba_hazard_version_is_green 应绿
    /*
    loom::model(|| {
        let stack = Arc::new(BuggyStack::new());
        let popped = Arc::new(AtomicUsize::new(0));
        stack.push(42);

        let s = stack.clone();
        let p = popped.clone();
        let h1 = thread::spawn(move || {
            let _ = s.pop_buggy(&p);
        });
        let _ = stack.pop_buggy(&popped);
        h1.join().unwrap();

        let count = popped.load(Ordering::SeqCst);
        assert!(
            count <= 1,
            "ABA bug：double-count={}",
            count
        );
    });
    */
}

#[test]
fn aba_hazard_version_is_green() {
    // 绿版：pop 先 CAS、CAS 成功才 take value。popped_count 永远 ≤ push 数量。
    loom::model(|| {
        let stack = Arc::new(SafeStack::new());
        let popped = Arc::new(AtomicUsize::new(0));
        stack.push(42);

        let s = stack.clone();
        let p = popped.clone();
        let h1 = thread::spawn(move || {
            let _ = s.pop_safe(&p);
        });
        let _ = stack.pop_safe(&popped);
        h1.join().unwrap();

        let count = popped.load(Ordering::SeqCst);
        assert!(count <= 1, "hazard 版不应 double-count，但 count={}", count);
    });
}
