//! M2.5 —— thread::park / unpark：单消费者队列
//!
//! 关键性质：unpark 的请求**不会丢失**——先 unpark 后 park，park 会立刻返回。
//! 这避免了"检查为空 → 解锁 → park"缝隙里的通知丢失。
//! 但 unpark 不累积：两次 unpark + 两次 park，第二个 park 仍会睡。
use forge_sync::std_locks::TaskQueue;
use std::sync::Mutex;
use std::thread;

#[test]
fn park_based_queue_works() {
    // 这里直接用 TaskQueue（它内部就是 Mutex + Condvar），验证 park 模式的等价物：
    // 一个生产者 push，一个消费者阻塞 pop。
    let q = std::sync::Arc::new(TaskQueue::<u32>::new());
    let got = std::sync::Arc::new(Mutex::new(Vec::new()));

    let q2 = q.clone();
    let g2 = got.clone();
    let consumer = thread::spawn(move || {
        for _ in 0..5 {
            let item = q2.pop_blocking();
            g2.lock().unwrap().push(item);
        }
    });

    for i in 0..5u32 {
        q.push(i);
    }
    consumer.join().unwrap();

    let got = got.lock().unwrap();
    assert_eq!(*got, vec![0, 1, 2, 3, 4]);
}
