//! M5.1 —— one-shot 阻塞通道（原书第 5 章最终版的示例）
use forge_channel::oneshot::Channel;
use std::thread;

#[test]
fn blocking_oneshot() {
    let mut channel = Channel::new();
    thread::scope(|s| {
        let (sender, receiver) = channel.split();
        s.spawn(move || {
            sender.send("hello world!");
        });
        assert_eq!(receiver.receive(), "hello world!");
    });
}
