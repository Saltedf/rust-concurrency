//! M2.6 —— Condvar 驱动的 TaskQueue：多工作者消费（调度器种子）
//!
//! 这是 M2 的核心产物。多个工作者睡在 Condvar 上，生产者 push 后 notify_one。
//! Condvar 解决了 park 在"多消费者"下的短板：生产者无需知道哪个消费者在等。
use forge_sync::std_locks::TaskQueue;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

#[test]
fn many_workers_drain_the_queue() {
    let q = Arc::new(TaskQueue::<usize>::new());
    let done = Arc::new(AtomicUsize::new(0));

    const N_WORKERS: usize = 4;
    const N_TASKS: usize = 1000;

    thread::scope(|s| {
        // 4 个工作者，各自循环 pop_blocking，直到收到"毒丸"哨兵（usize::MAX）
        for _ in 0..N_WORKERS {
            let q = q.clone();
            let done = done.clone();
            s.spawn(move || {
                loop {
                    let task = q.pop_blocking();
                    if task == usize::MAX {
                        break; // 毒丸：收摊
                    }
                    done.fetch_add(1, Ordering::Relaxed);
                }
            });
        }

        // 生产者：投 1000 个任务，再投 N_WORKERS 个毒丸叫停
        for i in 0..N_TASKS {
            q.push(i);
        }
        for _ in 0..N_WORKERS {
            q.push(usize::MAX);
        }
    });

    assert_eq!(done.load(Ordering::Relaxed), N_TASKS, "1000 个任务必须全部被处理");
}
