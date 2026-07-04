//! # M9a loom 模型测试：嵌套 spawn 死锁——worker 在 recv 里 park 而不干活
//!
//! 【在抓什么 bug】
//! M9a 的核心 stress 是嵌套 spawn：任务 A 在 worker W1 上跑，A 内部 spawn 了任务 B
//! 然后调 `h.recv()` 等 B 的结果。如果 `recv` 在 worker 线程上是**纯阻塞 park**
//! （像外部线程那样），那么 W1 就会睡死，B 永远不会被运行（因为只有 worker 能跑它，
//! 而 W1 在睡、没有别的 worker 或没人偷 B）。
//!
//! 这就是 rayon/tokio 等"阻塞调度"运行时必须解决的问题：**recv 在 worker 上不能
//! park，必须继续跑挂起的任务**。我们的 `JoinHandle::recv`（src/lib.rs）正是这样
//! 写的。
//!
//! 【怎么抓 / 协议设计】
//! loom 是单线程模型——主线程"充当唯一的 worker"。它跑 A，A 内 spawn B 入队，A 调 recv。
//! recv 有两个版本：
//!   - buggy：只等结果，**不帮忙**跑队列里的任务。此时只有主线程能跑 B，但它不跑 →
//!            B 永远不被弹 → 永远不被 send → 主线程 stuck。
//!   - correct：循环"检查结果 → 从队列弹一个任务跑 → 再检查"。B 被跑、A 拿到结果。
//!
//! 【避免 loom 死锁 abort】
//! loom 检测到"有线程永远 park"会 panic 并 abort，`#[should_panic]` 抓不住（loom 内部
//! 清理时的二次 panic）。所以 buggy 版**不用真 park**——主线程有限次 yield 重试，
//! 超过则报告"stuck"（没拿到结果）。这就把死锁翻译成可观测的布尔断言。
//!
//! 【先红后绿】
//! - red：buggy recv（只等结果，不帮忙跑队列里的任务）。B 永远不被弹 →
//!        主线程 stuck（None）→ 断言"拿到结果"失败 → 红。
//! - green：correct recv（worker 上帮忙跑队列里的任务）。B 被弹、send、A 拿到结果 →
//!          Some(42) → 绿。
//!
//! 运行：`LOOM_MAX_PREEMPTIONS=4 RUSTFLAGS="--cfg loom" cargo +nightly test \
//!        -p forge-pool --test m9a_06_deadlock`

#![cfg(loom)]

use loom::sync::{Arc, Mutex};
use std::collections::VecDeque;

/// 一个 oneshot 结果槽：sender 写一次，receiver 用"有限次 yield"等结果。
struct Oneshot<T: Clone> {
    inner: Mutex<Option<T>>,
}

impl<T: Clone> Oneshot<T> {
    fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }
    fn send(&self, v: T) {
        *self.inner.lock().unwrap() = Some(v);
    }
    fn try_get(&self) -> Option<T> {
        self.inner.lock().unwrap().clone()
    }
}

struct Pool {
    queue: Mutex<VecDeque<Box<dyn FnOnce() + Send>>>,
}

impl Pool {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(VecDeque::new()),
        })
    }
}

fn pool_push(pool: &Arc<Pool>, f: Box<dyn FnOnce() + Send>) {
    pool.queue.lock().unwrap().push_back(f);
}

fn pool_try_pop(pool: &Arc<Pool>) -> Option<Box<dyn FnOnce() + Send>> {
    pool.queue.lock().unwrap().pop_front()
}

/// buggy recv：只等结果，**不帮忙**跑队列里的任务。
/// 在"唯一 worker 是主线程"的场景下，B 永远不被弹 → 永远拿不到结果 → stuck。
fn recv_buggy<T: Clone>(result: &Oneshot<T>, _pool: &Arc<Pool>) -> Option<T> {
    // 故意忽略 pool：只 yield 等结果，从不弹队列里的任务。
    for _ in 0..6 {
        if let Some(v) = result.try_get() {
            return Some(v);
        }
        loom::thread::yield_now();
    }
    None // stuck
}

/// correct recv：每次 yield 后**先弹一个任务跑**，再检查结果。
/// B 被弹、被跑、send 结果 → 主线程拿到。
fn recv_correct<T: Clone>(result: &Oneshot<T>, pool: &Arc<Pool>) -> Option<T> {
    for _ in 0..6 {
        if let Some(v) = result.try_get() {
            return Some(v);
        }
        // 帮忙：弹队列里的任务跑。第一次迭代就能弹到 B 并跑它。
        if let Some(t) = pool_try_pop(&pool) {
            t();
        }
        loom::thread::yield_now();
    }
    None
}

#[test]
fn green_recv_helps() {
    loom::model::Builder::new().check(|| {
        let pool = Pool::new();
        let result = Arc::new(Oneshot::<u64>::new());

        // spawn B：B 算出 42 后 send。
        let result_for_b = result.clone();
        pool_push(&pool, Box::new(move || {
            result_for_b.send(42);
        }));

        // 主线程充当唯一 worker：correct recv 帮忙跑队列里的 B。
        let v = recv_correct(&result, &pool);
        assert_eq!(
            v,
            Some(42),
            "correct recv 应能跑到子任务并拿到结果"
        );
    });
}

#[test]
#[should_panic]
fn red_recv_deadlocks() {
    loom::model::Builder::new().check(|| {
        let pool = Pool::new();
        let result = Arc::new(Oneshot::<u64>::new());

        let result_for_b = result.clone();
        pool_push(&pool, Box::new(move || {
            result_for_b.send(42);
        }));

        // 主线程充当唯一 worker：buggy recv **不帮忙**跑队列里的 B。
        // B 永远不被弹 → result 永远是 None → recv_buggy 返回 None（stuck）。
        let v = recv_buggy(&result, &pool);
        // 关键断言：必须拿到结果。buggy 版下永远拿不到 → 红。
        assert_eq!(
            v,
            Some(42),
            "嵌套 spawn 死锁：worker 在 recv 里没帮忙跑子任务"
        );
    });
}
