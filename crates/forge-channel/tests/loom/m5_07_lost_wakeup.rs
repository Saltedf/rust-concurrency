//! # M5 loom 模型测试：oneshot 通道——ready 用 Relaxed 时消息可能读不到
//!
//! 【在抓什么 bug】
//! oneshot 通道的本质就是 M1.6 的同构骨架："sender 把 message 写到共享槽 → 用 ready
//! 标志发布 → receiver 看到 ready=true 后读 message"。这必须靠 Release/Acquire 拉起
//! happens-before。
//!
//! 如果 sender 用 Relaxed 发布 ready、receiver 用 Relaxed 读 ready，loom 模型下
//! 允许"receiver 看到 ready=true 但 message 仍是 0"的交错——和 M1.6 完全同构。
//!
//! 【关键坑：park/unpark 自带 happens-before】
//! M5 的接收循环朴素写法是 `while !ready { thread::park(); }`。但 loom 的
//! `thread::park` / `unpark` **本身会拉起 happens-before**（和 std 一样：unpark
//! 醒来的 parker 能看到 unpark 之前的一切写）。所以一旦用 park/unpark，ready 这对
//! 就算用 Relaxed 也被 park/unpark 的 HB 兜底了——测不出 bug。
//!
//! 这正是 M5 教程强调"park/unpark 是同步原语"的体现。要测出"纯 ready 标志的
//! ordering bug"，必须**不用 park**——用 yield 自旋等。本测试就采用这种自旋接收。
//!
//! 【怎么抓 / 协议】
//! - sender：`message.store(42, Relaxed); ready.store(true, ord_ready);`
//! - receiver：有限次 yield 自旋，直到 `ready.load(ord_ready)==true`，然后读 message。
//!
//! 【先红后绿】
//! - red：ready 用 Relaxed（不用 park）。loom 找到"ready 可见但 message=0"的交错 → 红。
//! - green：ready 用 Release/Acquire → HB 接通 → message=42 → 绿。
//!
//! 运行：`LOOM_MAX_PREEMPTIONS=3 RUSTFLAGS="--cfg loom" cargo +nightly test \
//!        -p forge-channel --test m5_07_lost_wakeup`

#![cfg(loom)]

use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use loom::sync::Arc;
use loom::thread;

struct Channel {
    message: AtomicUsize,
    ready: AtomicBool,
}

impl Channel {
    fn new() -> Self {
        Self {
            message: AtomicUsize::new(0),
            ready: AtomicBool::new(false),
        }
    }
}

fn run(ord_ready_store: Ordering, ord_ready_load: Ordering) {
    let ors = ord_ready_store;
    let orl = ord_ready_load;
    loom::model::Builder::new().check(move || {
        let ch = Arc::new(Channel::new());
        let ch2 = ch.clone();

        let sender = thread::spawn(move || {
            ch2.message.store(42, Ordering::Relaxed);
            ch2.ready.store(true, ors);
        });

        // receiver：有限次 yield 自旋（不用 park，避免 park/unpark 的 HB 兜底）。
        // 6 次足够在 preemption_bound=3 下覆盖所有交错里"sender 写完 ready"的时刻。
        let mut ready_seen = false;
        for _ in 0..6 {
            if ch.ready.load(orl) {
                ready_seen = true;
                break;
            }
            thread::yield_now();
        }

        if ready_seen {
            let v = ch.message.load(Ordering::Relaxed);
            // 关键断言：看到 ready=true 后，message 必须是 42。
            //   正确版（Release/Acquire）：HB 接通 → 永远成立。
            //   buggy 版（全 Relaxed）：loom 枚举到"ready 可见、message 未可见"的交错 → 红。
            assert_eq!(
                v, 42,
                "看到 ready=true 但 message 未就绪：oneshot 缺少 Release/Acquire 配对"
            );
        }

        sender.join().unwrap();
    });
}

#[test]
fn green_correct_release_acquire() {
    // 正确版：ready.store 用 Release，ready.load 用 Acquire。HB 接通，永不红。
    run(Ordering::Release, Ordering::Acquire);
}

#[test]
#[should_panic]
fn red_relaxed_relaxed() {
    // buggy 版：ready.store/load 都用 Relaxed，没有 happens-before 边。
    // loom 会枚举到"ready 可见、message 未可见"的交错 → 红。
    run(Ordering::Relaxed, Ordering::Relaxed);
}
