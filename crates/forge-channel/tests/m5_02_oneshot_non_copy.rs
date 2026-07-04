//! M5.2 —— one-shot 传送非 Copy 数据（Vec 被移动，不被复制）
use forge_channel::oneshot::Channel;
use std::thread;

#[test]
fn sends_non_copy_data() {
    let mut ch = Channel::new();
    thread::scope(|s| {
        let (tx, rx) = ch.split();
        s.spawn(move || {
            tx.send(vec![1, 2, 3]);
        });
        assert_eq!(rx.receive(), vec![1, 2, 3]);
    });
}
