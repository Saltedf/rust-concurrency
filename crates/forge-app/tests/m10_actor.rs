//! M10 子应用：Actor 模型集成测试。
//!
//! 注意：所有测试在 shutdown / drop Actor 之前都先 drop Handle（以及任何
//! clone 出来的 Handle）。这是因为 actor 的退出条件是"所有 sender drop"——
//! 只要还有 Handle 持有 sender，inbox 的 recv 就不会返回 None，actor 线程
//! 不会退出，shutdown 会永远阻塞。这条前提在 Actor::shutdown 的文档里。

use forge_app::actor::{
    reply_channel, spawn, spawn_broadcast, spawn_counter, spawn_kv, Actor, CounterMsg, Handle,
    KvMsg,
};
use forge_app::event_bus::EventBus;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

// 手算例 2 的代码对应：两个 Handle 同时发 Inc，actor 内部串行处理。
//
// 初始值 5
// 队列: [Inc(1), Inc(1)]  ← 两条消息顺序进入 inbox
// 第 1 拍 handler: state.value 5 → 6
// 第 2 拍 handler: state.value 6 → 7
// 之后 Get 查询得到 7。
#[test]
fn counter_inc_then_get() {
    let (actor, handle) = spawn_counter(0);
    handle.send(CounterMsg::Inc(5));
    handle.send(CounterMsg::Inc(2));

    let (reply, rx) = reply_channel::<i64>();
    handle.send(CounterMsg::Get(reply));
    assert_eq!(rx.await_reply(), 7);

    // Get 又一次——确认状态持续存在
    let (reply2, rx2) = reply_channel::<i64>();
    handle.send(CounterMsg::Get(reply2));
    assert_eq!(rx2.await_reply(), 7);

    drop(handle);
    actor.shutdown().unwrap();
}

#[test]
fn counter_starts_from_initial() {
    let (actor, handle) = spawn_counter(100);
    let (reply, rx) = reply_channel::<i64>();
    handle.send(CounterMsg::Get(reply));
    assert_eq!(rx.await_reply(), 100);
    drop(handle);
    actor.shutdown().unwrap();
}

#[test]
fn counter_concurrent_inc_serialized() {
    // 两个 Handle 同时发 Inc(1)，各 1000 次——actor 串行处理，无丢失、无竞态。
    // 最终值精确等于 2000，证明"用消息传递代替共享状态"避免数据竞争。
    let (actor, handle) = spawn_counter(0);
    let h1 = handle.clone();
    let h2 = handle.clone();

    let t1 = thread::spawn(move || {
        for _ in 0..1000 {
            h1.send(CounterMsg::Inc(1));
        }
    });
    let t2 = thread::spawn(move || {
        for _ in 0..1000 {
            h2.send(CounterMsg::Inc(1));
        }
    });
    t1.join().unwrap();
    t2.join().unwrap();

    // 原始 handle 保留在这里：用它在所有 Inc 之后发 Get。
    // send 是无界 FIFO，Get 排在 2000 条 Inc 之后，
    // actor 串行处理完所有 Inc 才轮到 Get，回复的值一定 = 2000。
    let (reply, rx) = reply_channel::<i64>();
    handle.send(CounterMsg::Get(reply));
    assert_eq!(rx.await_reply(), 2000);
    drop(handle);
    actor.shutdown().unwrap();
}

#[test]
fn kv_actor_set_get_del() {
    let (actor, handle) = spawn_kv();
    handle.send(KvMsg::Set("name".into(), "forge".into()));

    let (reply, rx) = reply_channel::<Option<String>>();
    handle.send(KvMsg::Get("name".into(), reply));
    assert_eq!(rx.await_reply().as_deref(), Some("forge"));

    // 不存在的 key
    let (reply2, rx2) = reply_channel::<Option<String>>();
    handle.send(KvMsg::Get("nope".into(), reply2));
    assert_eq!(rx2.await_reply(), None);

    // Del
    handle.send(KvMsg::Del("name".into()));
    let (reply3, rx3) = reply_channel::<Option<String>>();
    handle.send(KvMsg::Get("name".into(), reply3));
    assert_eq!(rx3.await_reply(), None);

    drop(handle);
    actor.shutdown().unwrap();
}

#[test]
fn kv_actor_concurrent_writes_no_lost_updates() {
    // 多线程并发 Set 不同 key——actor 串行处理，无丢失。
    let (actor, handle) = spawn_kv();
    let mut threads = Vec::new();
    for i in 0..10 {
        let h = handle.clone();
        threads.push(thread::spawn(move || {
            let key = format!("k{i}");
            let val = format!("v{i}");
            h.send(KvMsg::Set(key, val));
        }));
    }
    for t in threads {
        t.join().unwrap();
    }

    // 等所有 Set 处理完后查每一个
    for i in 0..10 {
        let (reply, rx) = reply_channel::<Option<String>>();
        handle.send(KvMsg::Get(format!("k{i}"), reply));
        assert_eq!(rx.await_reply().as_deref(), Some(format!("v{i}").as_str()));
    }
    drop(handle);
    actor.shutdown().unwrap();
}

#[test]
fn actor_shutdown_terminates_thread_cleanly() {
    let (actor, handle) = spawn_counter(0);
    handle.send(CounterMsg::Inc(1));
    drop(handle);
    let result = actor.shutdown();
    assert!(result.is_ok());
}

#[test]
fn actor_state_dropped_on_shutdown() {
    // 验证 actor 内部 state 在 shutdown 时被正确 drop（不泄漏）。
    static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

    struct DropTracker;
    impl Drop for DropTracker {
        fn drop(&mut self) {
            DROP_COUNT.fetch_add(1, Ordering::SeqCst);
        }
    }

    DROP_COUNT.store(0, Ordering::SeqCst);
    let initial = DropTracker;
    let (actor, handle) = spawn::<u32, _, _>(initial, |state: &mut DropTracker, _msg: u32| {
        let _ = state;
    });
    drop(handle);
    actor.shutdown().unwrap();
    assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 1);
}

#[test]
fn broadcast_actor_forwards_to_event_bus() {
    // 把 actor 和事件总线连起来：往 BroadcastState 的 Handle 投消息 =
    // 向总线所有订阅者广播。
    let bus: EventBus<String> = EventBus::new();
    let r1 = bus.subscribe();
    let r2 = bus.subscribe();
    let (actor, handle) = spawn_broadcast(bus.clone());

    handle.send("hello-bus".to_string());
    assert_eq!(r1.recv(), "hello-bus");
    assert_eq!(r2.recv(), "hello-bus");

    drop(handle);
    actor.shutdown().unwrap();
}

#[test]
fn reply_channel_single_use() {
    let (reply, rx) = reply_channel::<i32>();
    reply.send(42);
    assert_eq!(rx.await_reply(), 42);
}

#[test]
fn reply_channel_blocks_until_sent() {
    let (reply, rx) = reply_channel::<String>();
    let t = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        reply.send("delayed".to_string());
    });
    assert_eq!(rx.await_reply(), "delayed");
    t.join().unwrap();
}

#[test]
fn custom_actor_with_external_side_effect() {
    // 直接用 spawn 测试自定义状态——用 Arc<AtomicUsize> 作为副作用出口
    // （它不是 state，是闭包捕获的，所以安全）。
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_for_closure = Arc::clone(&counter);

    let (actor, handle): (Actor<u32>, Handle<u32>) =
        spawn(0u32, move |_state: &mut u32, msg: u32| {
            counter_for_closure.fetch_add(msg as usize, Ordering::SeqCst);
        });

    for n in &[1u32, 2, 3, 4] {
        handle.send(*n);
    }
    // shutdown 会先关闭 inbox，handler 处理完队列里的剩余消息后 recv 返回 None，线程退出。
    drop(handle);
    actor.shutdown().unwrap();

    assert_eq!(counter.load(Ordering::SeqCst), 10); // 1+2+3+4
}

#[test]
fn actor_can_be_dropped_without_explicit_shutdown() {
    // Drop Actor 应当等价于 shutdown（Drop 实现里做了同样的清理）。
    // 注意：drop handle 必须在 drop actor 之前——否则 actor 线程不退出。
    let (actor, handle) = spawn_counter(0);
    handle.send(CounterMsg::Inc(1));
    drop(handle);
    drop(actor); // 不应当 hang
}

#[test]
fn many_concurrent_gets_all_return_consistent_value() {
    // 多线程同时 Get——actor 串行回复，每个 ReplyRx 都能拿到自己的答案。
    let (actor, handle) = spawn_counter(42);
    let mut handles = Vec::new();
    for _ in 0..8 {
        let h = handle.clone();
        handles.push(thread::spawn(move || {
            let (reply, rx) = reply_channel::<i64>();
            h.send(CounterMsg::Get(reply));
            rx.await_reply()
        }));
    }
    for t in handles {
        assert_eq!(t.join().unwrap(), 42);
    }
    drop(handle);
    actor.shutdown().unwrap();
}
