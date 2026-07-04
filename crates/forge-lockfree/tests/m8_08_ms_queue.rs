//! M8.08 —— Michael-Scott 无锁队列：MPMC enqueue/dequeue、顺序保持、空队列语义
use forge_lockfree::queue::Queue;
use std::sync::Arc;
use std::thread;

#[test]
fn queue_empty_returns_none() {
    let q: Queue<i32> = Queue::new();
    assert!(q.dequeue().is_none());
}

#[test]
fn queue_fifo_single_thread() {
    let q = Queue::new();
    q.enqueue(1);
    q.enqueue(2);
    q.enqueue(3);
    assert_eq!(q.dequeue(), Some(1));
    assert_eq!(q.dequeue(), Some(2));
    assert_eq!(q.dequeue(), Some(3));
    assert_eq!(q.dequeue(), None);
}

/// 单生产者单消费者：顺序保持。一条线程 enqueue 0..N，另一条 dequeue 收 N 个，
/// 收到的必须按入队顺序严格递增。
#[test]
fn queue_spsc_preserves_order() {
    let q = Arc::new(Queue::new());
    const N: u32 = 10_000;
    let qp = q.clone();
    let producer = thread::spawn(move || {
        for i in 0..N {
            qp.enqueue(i);
        }
    });
    let qp = q.clone();
    let consumer = thread::spawn(move || {
        let mut got = Vec::with_capacity(N as usize);
        while got.len() < N as usize {
            if let Some(v) = qp.dequeue() {
                got.push(v);
            }
        }
        got
    });
    producer.join().unwrap();
    let got = consumer.join().unwrap();
    for (i, v) in got.iter().enumerate() {
        assert_eq!(*v, i as u32, "顺序破坏：位置 {} 期望 {} 实际 {}", i, i, v);
    }
}

/// 多生产者多消费者：所有元素不丢不重。这里只测"集合相等"——MPMC 下
/// 单条流内的相对顺序不保证，但每个元素必须恰好出现一次。
#[test]
fn queue_mpmc_no_loss_no_dup() {
    use std::sync::atomic::{AtomicU32, Ordering};
    let q = Arc::new(Queue::new());
    const PRODUCERS: usize = 4;
    const PER_PRODUCER: u32 = 5000;
    const TOTAL: u32 = (PRODUCERS as u32) * PER_PRODUCER;

    // 全局已消费计数器：所有消费者共享，达到 TOTAL 就停。
    let consumed = Arc::new(AtomicU32::new(0));

    let all: Vec<(u32, u32)> = thread::scope(|s| {
        // 生产者：每个推 (pid, k)，k = 0..PER_PRODUCER。
        for pid in 0..PRODUCERS as u32 {
            let q = q.clone();
            s.spawn(move || {
                for k in 0..PER_PRODUCER {
                    q.enqueue((pid, k));
                }
            });
        }
        // 消费者：4 个，循环 dequeue 直到全局 consumed == TOTAL。
        let consumers: Vec<_> = (0..4)
            .map(|_| {
                let q = q.clone();
                let consumed = consumed.clone();
                s.spawn(move || {
                    let mut local = Vec::new();
                    while consumed.load(Ordering::Relaxed) < TOTAL {
                        if let Some(v) = q.dequeue() {
                            local.push(v);
                            consumed.fetch_add(1, Ordering::Relaxed);
                        } else {
                            std::thread::yield_now();
                        }
                    }
                    local
                })
            })
            .collect();
        let mut all = Vec::with_capacity(TOTAL as usize);
        for c in consumers {
            all.extend(c.join().unwrap());
        }
        all
    });

    // 不变量：每个 (pid, k) 恰好出现一次。
    assert_eq!(all.len() as u32, TOTAL, "总元素数必须等于生产总数");
    let mut seen: Vec<Vec<bool>> = (0..PRODUCERS)
        .map(|_| vec![false; PER_PRODUCER as usize])
        .collect();
    for (pid, k) in all {
        assert!(!seen[pid as usize][k as usize], "重复出现: ({},{})", pid, k);
        seen[pid as usize][k as usize] = true;
    }
    for (pid, row) in seen.iter().enumerate() {
        for (k, &v) in row.iter().enumerate() {
            assert!(v, "丢失: ({},{})", pid, k);
        }
    }
}

/// 简化的"并发 enqueue/dequeue 混跑"——确保不死锁、不丢消息。
#[test]
fn queue_mixed_concurrent() {
    let q = Arc::new(Queue::new());
    const N: usize = 20_000;
    thread::scope(|s| {
        for _ in 0..4 {
            let q = q.clone();
            s.spawn(move || {
                for i in 0..N {
                    q.enqueue(i as u32);
                    // 偶尔 dequeue
                    if i % 4 == 0 {
                        let _ = q.dequeue();
                    }
                }
            });
        }
    });
    // 把残余的元素全部 dequeue 出来，确保队列还能正常工作。
    let mut drained = 0;
    while q.dequeue().is_some() {
        drained += 1;
    }
    assert!(drained > 0, "残余元素必须能被全部 dequeue");
}
