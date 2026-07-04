//! # M1.6 loom 模型测试：用 Relaxed 发布数据，能复现"标志可见但数据未就绪"
//!
//! 【在抓什么 bug】
//! 一个线程先写"数据"（`data.store(42)`），再用一个"发布标志"（`ready.store(1)`）
//! 告诉对方"数据写好了，去读吧"。另一个线程读到标志后去读 data。
//!
//! 在内存模型层面，这是 **release/acquire 同构骨架**：发布数据的一方需要把发布
//! 操作本身标记为 `Release`，订阅的一方需要 `Acquire`。只要这样配对，就拉起一根
//! happens-before 因果线——"load 到了那个值，就一定能看到 store 之前的一切写"。
//!
//! 如果用 `Relaxed` 发布，模型**不强制**跨线程的读写顺序：消费者可能看到标志已立、
//! 但 data 的写尚未"传播"过来 → 读到旧值（0）。这正是 M1.6 LazyBox 的同构 bug。
//!
//! 【loom 怎么抓】
//! loom 不模拟硬件 store buffer，但它**模型化**内存模型：对 Relaxed 操作它不强制
//! happens-before 边。在穷举的交错里，loom 允许"T2 读到 ready 的写、却没读到 data
//! 的写"——只要两者之间没有 HB 连接，这就是合法行为。我们的断言把它变成失败。
//!
//! 【先红后绿】
//! - red：`data.store(Relaxed)` + `ready.store(Relaxed)` 发布；消费者 `ready.load(Relaxed)`
//!   + `data.load(Relaxed)`。loom 必然枚举到"标志可见、数据未可见"的交错 → 红。
//! - green：发布用 `Release`、订阅用 `Acquire`。happens-before 接通 → 绿。
//!
//! 运行：`LOOM_MAX_PREEMPTIONS=3 RUSTFLAGS="--cfg loom" cargo +nightly test \
//!        -p forge-core --test m1_06_relaxed_bug`

#![cfg(loom)]

use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::sync::Arc;
use loom::thread;

/// 共享状态：`data` 是要发布的数据，`ready` 是"数据写好了吗"的发布标志。
struct State {
    data: AtomicUsize,
    ready: AtomicUsize, // 0 = 未发布，1 = 已发布
}

fn run(store_ord: Ordering, load_ord: Ordering) {
    // loom::model::Builder 在 new() 时读取 LOOM_MAX_PREEMPTIONS 环境变量设上限。
    // Ordering 不是 Copy，所以这里复制两份拷给两个 closure。
    let so_b = store_ord;
    let lo_b = load_ord;
    loom::model::Builder::new().check(move || {
        let s = Arc::new(State {
            data: AtomicUsize::new(0),
            ready: AtomicUsize::new(0),
        });

        let s2 = s.clone();
        let h = thread::spawn(move || {
            // 生产者：先写数据，再立标志。中间没有同步原语——
            // 是否能"压住"这两步、让消费者在它们之间看不到 data，
            // 完全取决于发布/订阅这对操作各自的 Ordering。
            s2.data.store(42, Ordering::Relaxed);
            s2.ready.store(1, so_b);
        });

        // 消费者：读标志；若为 1，则读 data。
        if s.ready.load(lo_b) == 1 {
            let seen = s.data.load(Ordering::Relaxed);
            // 关键断言：看到了 ready==1，就必须看到 data==42。
            //   Release/Acquire 版：happens-before 接通 → 永远成立。
            //   Relaxed/Relaxed 版：loom 枚举的某些交错允许"ready 可见、data 不可见"
            //     → 断言失败 → 红。
            assert_eq!(
                seen, 42,
                "标志已可见但数据未就绪：缺少 Release/Acquire 配对"
            );
        }

        h.join().unwrap();
    });
}

#[test]
fn green_release_acquire() {
    // 正确版：Release store 配 Acquire load。happens-before 接通，永不红。
    run(Ordering::Release, Ordering::Acquire);
}

#[test]
#[should_panic]
fn red_relaxed_relaxed() {
    // 故意 buggy 版：发布与订阅都 Relaxed，没有 happens-before 边。
    // LOOM_MAX_PREEMPTIONS=3 下 loom 会枚举到"ready 可见、data 未可见"的交错。
    run(Ordering::Relaxed, Ordering::Relaxed);
}
