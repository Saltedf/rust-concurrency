//! M5.3 —— 发了没收的消息会被 Drop（不泄漏）
use forge_channel::oneshot::Channel;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn unread_message_is_dropped() {
    static DROPS: AtomicUsize = AtomicUsize::new(0);
    struct D;
    impl Drop for D {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }

    {
        let mut ch = Channel::<D>::new();
        let (tx, _rx) = ch.split();
        tx.send(D); // 发了，但 _rx 没收
    } // ch drop → Channel::drop 看到 ready=true → drop 未读消息

    assert_eq!(DROPS.load(Ordering::Relaxed), 1);
}
