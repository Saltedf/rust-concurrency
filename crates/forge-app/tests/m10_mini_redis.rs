//! M10 mini-Redis 集成测试：起一个真实 TCP 服务，用 std::net 客户端跑通。

use forge_app::mini_redis::{serve_one, ServerState};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

fn spawn_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let state = Arc::new(ServerState::new());
    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            match incoming {
                Ok(stream) => {
                    let state = state.clone();
                    std::thread::spawn(move || {
                        let _ = serve_one(stream, state);
                    });
                }
                Err(_) => break,
            }
        }
    });
    std::thread::sleep(Duration::from_millis(50));
    port
}

// 一个测试用的 RESP 客户端：持有一把 BufReader，避免每次 read 都新建
// reader（否则前一次 read 可能预读到下次要用的字节，下一个 reader
// 拿不到，数据就"丢"在销毁的 BufReader 内部缓冲里了）。
struct RespClient {
    writer: TcpStream,
    reader: BufReader<TcpStream>,
}

impl RespClient {
    fn new(stream: TcpStream) -> Self {
        let writer = stream.try_clone().unwrap();
        let reader = BufReader::new(stream);
        Self { writer, reader }
    }

    fn send_cmd(&mut self, args: &[&str]) {
        let mut buf = format!("*{}\r\n", args.len());
        for a in args {
            buf.push_str(&format!("${}\r\n{}\r\n", a.len(), a));
        }
        self.writer.write_all(buf.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }

    fn read_line(&mut self) -> String {
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        line
    }

    fn read_bulk(&mut self) -> Option<String> {
        let first = self.read_line();
        let first = first.trim_end_matches(|c| c == '\r' || c == '\n');
        if first == "$-1" {
            return None;
        }
        let len: usize = first.strip_prefix('$').unwrap().parse().unwrap();
        let mut buf = vec![0u8; len];
        self.reader.read_exact(&mut buf).unwrap();
        let mut crlf = [0u8; 2];
        self.reader.read_exact(&mut crlf).unwrap();
        Some(String::from_utf8(buf).unwrap())
    }
}

fn fresh_client(port: u16) -> RespClient {
    let c = TcpStream::connect(("127.0.0.1", port)).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    c.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    RespClient::new(c)
}

#[test]
fn set_and_get() {
    let port = spawn_server();
    let mut conn = fresh_client(port);

    conn.send_cmd(&["SET", "name", "forge"]);
    assert_eq!(conn.read_line(), "+OK\r\n");

    conn.send_cmd(&["GET", "name"]);
    assert_eq!(conn.read_bulk().as_deref(), Some("forge"));
}

#[test]
fn get_missing_returns_nil() {
    let port = spawn_server();
    let mut conn = fresh_client(port);

    conn.send_cmd(&["GET", "nope"]);
    assert_eq!(conn.read_bulk(), None);
}

#[test]
fn del_removes_key() {
    let port = spawn_server();
    let mut conn = fresh_client(port);

    conn.send_cmd(&["SET", "k", "v"]);
    let _ = conn.read_line();

    conn.send_cmd(&["DEL", "k"]);
    assert_eq!(conn.read_line(), ":1\r\n");

    conn.send_cmd(&["GET", "k"]);
    assert_eq!(conn.read_bulk(), None);

    conn.send_cmd(&["DEL", "k"]);
    assert_eq!(conn.read_line(), ":0\r\n");
}

#[test]
fn publish_to_zero_subscribers() {
    let port = spawn_server();
    let mut conn = fresh_client(port);

    conn.send_cmd(&["PUBLISH", "news", "hello"]);
    assert_eq!(conn.read_line(), ":0\r\n");
}

#[test]
fn publish_fanout_count_three_subscribers() {
    let port = spawn_server();

    // 3 个 SUBSCRIBE 连接
    let mut subs = Vec::new();
    for _ in 0..3 {
        let mut s = fresh_client(port);
        s.send_cmd(&["SUBSCRIBE", "news"]);
        // 等 +subscribed 回执，确认服务端已注册
        let r = s.read_line();
        assert_eq!(r, "+subscribed news\r\n");
        subs.push(s);
    }

    let mut pub_conn = fresh_client(port);
    // 注册已经同步完成（+subscribed 已回），不需要额外 sleep
    pub_conn.send_cmd(&["PUBLISH", "news", "hello"]);
    assert_eq!(pub_conn.read_line(), ":3\r\n");
}

#[test]
fn subscribe_then_publish_in_two_connections() {
    let port = spawn_server();
    let mut sub = fresh_client(port);
    sub.send_cmd(&["SUBSCRIBE", "ch"]);
    assert_eq!(sub.read_line(), "+subscribed ch\r\n");

    let mut pub_conn = fresh_client(port);
    pub_conn.send_cmd(&["PUBLISH", "ch", "hi"]);
    // 应该扇出给 1 个订阅者
    assert_eq!(pub_conn.read_line(), ":1\r\n");
}

#[test]
fn resp_parser_handles_set() {
    let raw = b"*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$5\r\nhello\r\n";
    let mut reader = &raw[..];
    let cmd = forge_app::mini_redis::read_command(&mut reader).unwrap();
    assert_eq!(
        cmd,
        forge_app::mini_redis::Command::Set("key".into(), "hello".into())
    );
}

#[test]
fn resp_parser_handles_get_and_publish() {
    let raw = b"*2\r\n$3\r\nGET\r\n$3\r\nkey\r\n";
    let mut reader = &raw[..];
    let cmd = forge_app::mini_redis::read_command(&mut reader).unwrap();
    assert_eq!(cmd, forge_app::mini_redis::Command::Get("key".into()));

    let raw = b"*3\r\n$7\r\nPUBLISH\r\n$4\r\nnews\r\n$5\r\nhello\r\n";
    let mut reader = &raw[..];
    let cmd = forge_app::mini_redis::read_command(&mut reader).unwrap();
    assert_eq!(
        cmd,
        forge_app::mini_redis::Command::Publish("news".into(), "hello".into())
    );
}

#[test]
fn resp_parser_handles_subscribe() {
    let raw = b"*2\r\n$9\r\nSUBSCRIBE\r\n$5\r\ntopic\r\n";
    let mut reader = &raw[..];
    let cmd = forge_app::mini_redis::read_command(&mut reader).unwrap();
    assert_eq!(cmd, forge_app::mini_redis::Command::Subscribe("topic".into()));
}

#[test]
fn resp_parser_handles_unknown() {
    let raw = b"*1\r\n$4\r\nFOOB\r\n";
    let mut reader = &raw[..];
    let r = forge_app::mini_redis::read_command(&mut reader);
    assert!(r.is_err());
}

#[test]
fn resp_parser_is_case_insensitive_on_command() {
    let raw = b"*2\r\n$3\r\nget\r\n$3\r\nkey\r\n";
    let mut reader = &raw[..];
    let cmd = forge_app::mini_redis::read_command(&mut reader).unwrap();
    assert_eq!(cmd, forge_app::mini_redis::Command::Get("key".into()));
}
