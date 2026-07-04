//! M5.5 —— mpsc：recv 在消息到达前阻塞，到达后被唤醒
use forge_channel::mpsc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[test]
fn recv_blocks_until_message_arrives() {
    let (tx, rx) = mpsc::channel::<&'static str>();
    let blocked_then_received = Arc::new(AtomicBool::new(false));
    let flag = blocked_then_received.clone();

    let h = thread::spawn(move || {
        let msg = rx.recv(); // 应阻塞，直到 send
        flag.store(msg == "hi", Ordering::SeqCst);
    });

    // 给接收者一点时间确实进入阻塞
    thread::sleep(Duration::from_millis(50));
    assert!(
        !blocked_then_received.load(Ordering::SeqCst),
        "此时还没 send，应仍在阻塞"
    );

    tx.send("hi");
    h.join().unwrap();
    assert!(blocked_then_received.load(Ordering::SeqCst));
}
