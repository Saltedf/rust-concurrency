//! M10 bin 集成测试：起一个后台线程跑 mini-redis 监听测试端口，
//! 用裸 TcpStream 写一条 RESP `SET k v`，读 `+OK`，再 `GET k` 读
//! `$1\r\nv`。这是读者用 redis-cli 跑 SET/GET 时实际发生的事——
//! 把 redis-cli 拆掉，直接和 RESP 字节对话。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use forge_app::mini_redis::{serve_one, ServerState};

fn spawn_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let state = Arc::new(ServerState::new());
    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            if let Ok(stream) = incoming {
                let st = state.clone();
                std::thread::spawn(move || {
                    let _ = serve_one(stream, st);
                });
            }
        }
    });
    // 给 accept 循环一点时间，避免客户端第一次 connect 撞空。
    std::thread::sleep(Duration::from_millis(30));
    port
}

struct RespClient {
    w: TcpStream,
    r: BufReader<TcpStream>,
}

impl RespClient {
    fn new(port: u16) -> Self {
        let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let w = s.try_clone().unwrap();
        let r = BufReader::new(s);
        Self { w, r }
    }

    /// 写一条 RESP 多块字符串命令，原始字节一比一刻画 redis-cli 实际行为。
    fn send_raw(&mut self, raw: &[u8]) {
        self.w.write_all(raw).unwrap();
        self.w.flush().unwrap();
    }

    fn read_line(&mut self) -> String {
        let mut s = String::new();
        self.r.read_line(&mut s).unwrap();
        s
    }

    fn read_n(&mut self, n: usize) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        self.r.read_exact(&mut buf).unwrap();
        buf
    }
}

#[test]
fn bin_set_then_get_real_resp_roundtrip() {
    let port = spawn_server();
    let mut c = RespClient::new(port);

    // *3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n
    let set_frame: &[u8] = b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n";
    c.send_raw(set_frame);
    assert_eq!(c.read_line(), "+OK\r\n");

    // *2\r\n$3\r\nGET\r\n$1\r\nk\r\n
    let get_frame: &[u8] = b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n";
    c.send_raw(get_frame);
    // 应回 `$1\r\nv\r\n`——逐拍读，断言每个字段。
    assert_eq!(c.read_line(), "$1\r\n");
    assert_eq!(c.read_n(1), b"v");
    assert_eq!(c.read_n(2), b"\r\n");
}

#[test]
fn bin_get_missing_returns_nil_bulk() {
    let port = spawn_server();
    let mut c = RespClient::new(port);

    c.send_raw(b"*2\r\n$3\r\nGET\r\n$5\r\nnopes\r\n");
    assert_eq!(c.read_line(), "$-1\r\n");
}

#[test]
fn bin_ping_returns_pong() {
    // 回归:bin 启动横幅声称支持 PING,但旧版 Command 枚举没有 Ping 变体,
    // PING 落到未知命令分支返回空/ERR。手动跑服务时抓到。这条测试守住它。
    let port = spawn_server();
    let mut c = RespClient::new(port);

    c.send_raw(b"*1\r\n$4\r\nPING\r\n");
    assert_eq!(c.read_line(), "+PONG\r\n");
}

#[test]
fn bin_publish_zero_when_no_subs() {
    let port = spawn_server();
    let mut c = RespClient::new(port);

    c.send_raw(b"*3\r\n$7\r\nPUBLISH\r\n$2\r\nch\r\n$5\r\nhello\r\n");
    assert_eq!(c.read_line(), ":0\r\n");
}

#[test]
fn bin_subscribe_then_publish_fans_out() {
    let port = spawn_server();

    // 第一条连接订阅 ch
    let mut sub = RespClient::new(port);
    sub.send_raw(b"*2\r\n$9\r\nSUBSCRIBE\r\n$2\r\nch\r\n");
    assert_eq!(sub.read_line(), "+subscribed ch\r\n");

    // 第二条连接 PUBLISH
    let mut pub_c = RespClient::new(port);
    pub_c.send_raw(b"*3\r\n$7\r\nPUBLISH\r\n$2\r\nch\r\n$5\r\nhello\r\n");
    // 应扇出给 1 个订阅者
    assert_eq!(pub_c.read_line(), ":1\r\n");
}

#[test]
fn bin_multiple_connections_share_state() {
    // 验证 KV 状态在连接之间共享：A 连接 SET，B 连接 GET。
    let port = spawn_server();

    let mut a = RespClient::new(port);
    a.send_raw(b"*3\r\n$3\r\nSET\r\n$3\r\nfoo\r\n$3\r\nbar\r\n");
    assert_eq!(a.read_line(), "+OK\r\n");

    let mut b = RespClient::new(port);
    b.send_raw(b"*2\r\n$3\r\nGET\r\n$3\r\nfoo\r\n");
    assert_eq!(b.read_line(), "$3\r\n");
    assert_eq!(b.read_n(3), b"bar");
}
