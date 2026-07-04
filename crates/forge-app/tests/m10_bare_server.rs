//! M10 子应用：零依赖 epoll echo 服务器集成测试。
//!
//! 仅在 Linux 上运行——epoll 是 Linux 专属。其他平台此测试整体跳过。

#![cfg(target_os = "linux")]

use forge_app::bare_server::{bind_loopback_random, BareEchoServer};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// 起一个后台线程跑服务器，返回 (服务器 Arc 用于关闭, 端口)。
fn spawn_server(max_events: usize) -> (Arc<BareEchoServer>, u16) {
    let (server, port) = bind_loopback_random().expect("bind failed");
    let server = Arc::new(server);
    let s = Arc::clone(&server);
    thread::spawn(move || {
        // 服务器在子线程里循环，直到被 close / 出错退出
        let _ = s.serve(max_events, Some(Duration::from_secs(2)));
    });
    // 给服务器一点时间进入 epoll_wait
    thread::sleep(Duration::from_millis(50));
    (server, port)
}

fn echo_round(port: u16, payload: &[u8]) -> Vec<u8> {
    let mut conn = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    conn.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    conn.write_all(payload).expect("write");
    // 读回相同字节
    let mut got = Vec::with_capacity(payload.len());
    while got.len() < payload.len() {
        let mut buf = [0u8; 1024];
        match conn.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(e) => panic!("read error: {e}"),
        }
    }
    got
}

#[test]
fn single_connection_echo() {
    let (_server, port) = spawn_server(16);
    let payload = b"hello epoll";
    let got = echo_round(port, payload);
    assert_eq!(got, payload.as_slice());
}

#[test]
fn small_message_roundtrip() {
    let (_server, port) = spawn_server(16);
    let got = echo_round(port, b"forge");
    assert_eq!(&got, b"forge");
}

#[test]
fn multiple_sequential_messages_same_connection() {
    let (_server, port) = spawn_server(16);
    let mut conn = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    conn.set_write_timeout(Some(Duration::from_secs(5))).unwrap();

    for &payload in &[b"a".as_slice(), b"bb", b"ccc"] {
        conn.write_all(payload).unwrap();
        let mut got = vec![0u8; payload.len()];
        conn.read_exact(&mut got).unwrap_or_else(|e| panic!("read: {e}"));
        assert_eq!(got, payload);
    }
}

#[test]
fn multiple_concurrent_connections_all_echoed() {
    // 3 条连接同时上来——epoll_wait 一次或多次返回它们的就绪事件，
    // 每条都被 echo。这就是手算例 3 的代码版。
    let (_server, port) = spawn_server(32);

    let mut threads = Vec::new();
    for i in 0..3 {
        let payload = format!("hello-from-client-{i}").into_bytes();
        threads.push(thread::spawn(move || {
            let got = echo_round(port, &payload);
            assert_eq!(got, payload);
            i
        }));
    }
    for t in threads {
        let _ = t.join().expect("client thread panicked");
    }
}

#[test]
fn large_payload_echoed_in_chunks() {
    // 验证 read 循环能处理超过单次 read 缓冲的大消息。
    let (_server, port) = spawn_server(32);
    let payload: Vec<u8> = (0..50_000).map(|i| (i % 251) as u8).collect();
    let got = echo_round(port, &payload);
    assert_eq!(got.len(), payload.len());
    assert_eq!(got, payload);
}

#[test]
fn many_clients_stress() {
    // 20 个连接 × 各发一条消息，全部 echo 回来。
    let (_server, port) = spawn_server(64);
    let mut threads = Vec::new();
    for i in 0..20 {
        let payload = format!("msg-{i}").into_bytes();
        threads.push(thread::spawn(move || {
            let got = echo_round(port, &payload);
            (i, got)
        }));
    }
    for t in threads {
        let (i, got) = t.join().unwrap();
        assert_eq!(got, format!("msg-{i}").into_bytes());
    }
}

#[test]
fn server_binds_to_specific_loopback_port() {
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
    let server = BareEchoServer::bind(addr).expect("bind");
    let _ = server.epoll_fd();
    let _ = server.listen_fd();
    // 析构时应当干净释放
    drop(server);
}
