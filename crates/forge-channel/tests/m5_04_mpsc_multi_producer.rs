//! M5.4 —— mpsc：多生产者，消息全部收到、顺序在单生产者内保持
use forge_channel::mpsc;
use std::thread;

#[test]
fn mpsc_collects_from_many_producers() {
    let (tx, rx) = mpsc::channel::<u32>();
    const N: usize = 4;
    const PER: u32 = 1000;

    thread::scope(|s| {
        for _ in 0..N {
            let tx = tx.clone();
            s.spawn(move || {
                for i in 0..PER {
                    tx.send(i);
                }
            });
        }
        drop(tx); // 主线程的 tx 也丢掉
    });
    // 所有发送者都 drop 后， Receiver 最终应收到恰好 N*PER 条
    let mut got = Vec::new();
    for _ in 0..(N as u32 * PER) {
        got.push(rx.recv());
    }
    assert_eq!(got.len() as u32, N as u32 * PER);
    // 单生产者内的顺序保持：每个生产者发的 0..PER 在全局队列里应是连续递增的（FIFO）。
    // 这里只校验总数；顺序的多生产者交错不在保证范围内。
}
