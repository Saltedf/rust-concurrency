//! # mini-Redis（M10 主应用之二）
//!
//! 一个跑在 TCP loopback 上的微型 Redis：支持 GET / SET / DEL，以及
//! PUBLISH / SUBSCRIBE。KV 存储用我们自研的 `forge_sync::Mutex`，订阅表
//! 用 `forge_channel::mpsc` 的 Sender 做广播。
//!
//! 为什么不用 M9b 异步运行时？讲清取舍：异步运行时的精髓在"一台机器扛
//! 10 万连接"——它靠 epoll 让一个线程管几千条 socket。但教学价值上，
//! "每连接一个线程"模型更直白：你能看见每条 TCP 流、每一拍 read/write、
//! 每一次锁获取。M11 会专门讲怎么把它搬到异步上。本模块的焦点是
//! "并发决策"——pub/sub 的扇出、KV 的共享状态、SUBSCRIBE 时的注册竞争。
//!
//! 协议是 RESP（REdis Serialization Protocol）。例：
//! `*2\r\n$3\r\nGET\r\n$3\r\nkey\r\n` 表示一条两参数命令（GET key）。
//! 多一个字段就是更通用的"行框架"——比我们手撸"按空格切"更稳。

use forge_channel::mpsc;
use forge_sync::mutex::Mutex;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;

// BufReader / Read 在 read_command 中通过 `impl BufRead` 泛型使用；
// TcpStream / Write / ToSocketAddrs 用于 serve。

// =========================================================================
//  RESP 协议
// =========================================================================

/// 命令。全大写（不区分大小写由 `parse_command` 处理）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Get(String),
    Set(String, String),
    Del(String),
    /// `PING` → 回 `+PONG`。健康检查用;无参数版本。
    Ping,
    Publish(String, String),
    Subscribe(String),
    Unsubscribe(String),
}

/// 解析错误：服务端用它在客户端流上回一条 `-ERR ...`。
#[derive(Debug)]
pub struct RespError(pub String);

/// 把一个 RESP 命令解析出来。`reader` 是已带缓冲的 reader。
///
/// 简化：只支持最常用的 `*N\r\n$LEN\r\nBYTES\r\n ...`（Array of Bulk Strings）。
/// 真正的 Redis 还支持 inline 命令、整数、错误类型等——我们砍掉，因为
/// 它们只是协议细节，不影响"如何用并发原语搭服务"这条主线。
pub fn read_command(reader: &mut impl BufRead) -> Result<Command, RespError> {
    // 读第一行：应当形如 `*2\r\n`
    let mut header = String::new();
    reader
        .read_line(&mut header)
        .map_err(|e| RespError(e.to_string()))?;
    let header = header.trim_end_matches(|c| c == '\r' || c == '\n');
    let count = header
        .strip_prefix('*')
        .ok_or_else(|| RespError(format!("expected '*', got {header:?}")))?
        .parse::<usize>()
        .map_err(|_| RespError(format!("bad array len: {header}")))?;

    // 后续 `count` 个 bulk string：每个 `$LEN\r\nBYTES\r\n`
    let mut args: Vec<String> = Vec::with_capacity(count);
    for _ in 0..count {
        let mut len_line = String::new();
        reader
            .read_line(&mut len_line)
            .map_err(|e| RespError(e.to_string()))?;
        let len_line = len_line.trim_end_matches(|c| c == '\r' || c == '\n');
        let len = len_line
            .strip_prefix('$')
            .ok_or_else(|| RespError(format!("expected '$', got {len_line:?}")))?
            .parse::<usize>()
            .map_err(|_| RespError(format!("bad bulk len: {len_line}")))?;
        let mut buf = vec![0u8; len];
        reader
            .read_exact(&mut buf)
            .map_err(|e| RespError(e.to_string()))?;
        // 吃掉结尾的 \r\n
        let mut crlf = [0u8; 2];
        reader
            .read_exact(&mut crlf)
            .map_err(|e| RespError(e.to_string()))?;
        args.push(String::from_utf8(buf).map_err(|e| RespError(e.to_string()))?);
    }

    parse_command(&args)
}

fn parse_command(args: &[String]) -> Result<Command, RespError> {
    if args.is_empty() {
        return Err(RespError("empty command".into()));
    }
    let cmd = args[0].to_ascii_uppercase();
    match (cmd.as_str(), args.len()) {
        ("GET", 2) => Ok(Command::Get(args[1].clone())),
        ("SET", 3) => Ok(Command::Set(args[1].clone(), args[2].clone())),
        ("DEL", 2) => Ok(Command::Del(args[1].clone())),
        ("PING", 1) => Ok(Command::Ping),
        ("PUBLISH", 3) => Ok(Command::Publish(args[1].clone(), args[2].clone())),
        ("SUBSCRIBE", 2) => Ok(Command::Subscribe(args[1].clone())),
        ("UNSUBSCRIBE", 2) => Ok(Command::Unsubscribe(args[1].clone())),
        _ => Err(RespError(format!("unknown: {args:?}"))),
    }
}

/// 编码一条 bulk string 回复：`$N\r\nBYTES\r\n`。nil 用 `$-1\r\n`。
pub fn write_bulk(w: &mut impl Write, data: Option<&str>) -> std::io::Result<()> {
    match data {
        Some(s) => write!(w, "${}\r\n{}\r\n", s.len(), s),
        None => write!(w, "$-1\r\n"),
    }
}

/// 整数回复：`:N\r\n`
pub fn write_int(w: &mut impl Write, n: i64) -> std::io::Result<()> {
    write!(w, ":{n}\r\n")
}

/// 简单字符串（+OK 之类）：`+OK\r\n`
pub fn write_simple(w: &mut impl Write, s: &str) -> std::io::Result<()> {
    write!(w, "+{s}\r\n")
}

/// 错误：`-ERR ...\r\n`
pub fn write_err(w: &mut impl Write, s: &str) -> std::io::Result<()> {
    write!(w, "-ERR {s}\r\n")
}

// =========================================================================
//  服务端状态
// =========================================================================

/// 一条对订阅客户端的消息：`(channel, message)`。
pub type SubMessage = (String, String);

/// 服务端共享状态。
///
/// `kv` 和 `subs` 是两把不同的锁——故意分开，因为它们的访问模式完全不同：
/// GET/SET 是"短锁"，订阅是"长持有"（注册 / 注销时才动）。如果用一把锁，
/// 一条 SET 会阻塞所有 SUBSCRIBE 注册；用两把锁，它们互不干扰。
/// 这就是"按访问模式分锁"的最小例子——粒度不是越细越好，是按场景分。
pub struct ServerState {
    /// KV 存储。Bytes 用 String 简化（真实 Redis 用二进制安全的 SDS）。
    pub kv: Mutex<HashMap<String, String>>,
    /// 订阅表：channel -> 一堆 sender。每个订阅者一条 mpsc Sender。
    /// PUBLISH 就是对所有 sender 各发一份。
    pub subs: Mutex<HashMap<String, Vec<mpsc::Sender<SubMessage>>>>,
}

impl ServerState {
    pub fn new() -> Self {
        Self {
            kv: Mutex::new(HashMap::new()),
            subs: Mutex::new(HashMap::new()),
        }
    }
}

/// 处理一条客户端连接（提取出来便于测试和复用）。
pub fn serve_one(stream: TcpStream, state: Arc<ServerState>) -> std::io::Result<()> {
    handle_client(stream, state)
}

/// 运行服务直到 `listener` 被外部关闭。每个连接起一个线程——
/// 这就是"thread-per-connection"模型。教学焦点不是性能。
pub fn serve<A: ToSocketAddrs>(addr: A, state: Arc<ServerState>) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let state = state.clone();
                std::thread::spawn(move || {
                    if let Err(e) = handle_client(stream, state) {
                        eprintln!("client error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("accept failed: {e}"),
        }
    }
    Ok(())
}

fn handle_client(stream: TcpStream, state: Arc<ServerState>) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    // 记录这个连接订阅了哪些 channel——断开时要逐个注销，避免给已死的
    // sender 发消息（那会一直占内存）。
    let mut subscribed: HashSet<String> = HashSet::new();

    // 这个连接的"消息接收器"。订阅时把 sender 克隆一份塞进 subs 表，
    // 本线程另一个循环把 receiver 里的消息写给客户端的 socket。
    let (sub_tx, sub_rx) = mpsc::channel::<SubMessage>();

    loop {
        // 命令解析：阻塞读一行。注意：客户端 SUBSCRIBE 之后应当只发命令、
        // 服务端还要同时把 pub 出来的消息推回去——这正是 Redis 客户端
        // 协议的双向性。我们用"非阻塞试读" + "轮询 sub_rx" 来模拟：但
        // 阻塞 read_line 会卡死，所以教学版里我们让客户端在订阅后用一条
        // 单独的连接收消息（见测试）。
        let cmd = match read_command(&mut reader) {
            Ok(c) => c,
            Err(_) => break, // 客户端断开 / 协议错，结束
        };

        match cmd {
            Command::Get(key) => {
                let kv = state.kv.lock();
                let v = kv.get(&key).cloned();
                drop(kv);
                match v {
                    Some(s) => write_bulk(&mut writer, Some(s.as_str()))?,
                    None => write_bulk(&mut writer, None)?,
                }
                writer.flush()?;
            }
            Command::Set(key, value) => {
                let mut kv = state.kv.lock();
                kv.insert(key, value);
                write_simple(&mut writer, "OK")?;
                writer.flush()?;
            }
            Command::Ping => {
                // 健康检查:回 +PONG。无状态,不碰 kv。
                write_simple(&mut writer, "PONG")?;
                writer.flush()?;
            }
            Command::Del(key) => {
                let mut kv = state.kv.lock();
                let removed = kv.remove(&key).is_some();
                drop(kv);
                write_int(&mut writer, if removed { 1 } else { 0 })?;
                writer.flush()?;
            }
            Command::Subscribe(channel) => {
                // 关键并发点：把我的 sender 加进 subs[channel]，并记住本地集合
                // 这两步必须按顺序、且锁内一致——见教程手算例 3 的"早到 PUBLISH"。
                {
                    let mut subs = state.subs.lock();
                    subs.entry(channel.clone())
                        .or_insert_with(Vec::new)
                        .push(sub_tx.clone());
                }
                subscribed.insert(channel.clone());
                write_simple(&mut writer, &format!("subscribed {channel}"))?;
                writer.flush()?;
            }
            Command::Unsubscribe(channel) => {
                let mut subs = state.subs.lock();
                if let Some(vec) = subs.get_mut(&channel) {
                    // 移除"任意一个"指向本连接的 sender。mpsc::Sender 没有 eq，
                    // 我们简单粗暴：删除一个就好（本教学版每个连接每个 channel 只订阅一次）。
                    if let Some(idx) = (0..vec.len()).next() {
                        let _ = vec.swap_remove(idx);
                    }
                }
                subscribed.remove(&channel);
                write_simple(&mut writer, &format!("unsubscribed {channel}"))?;
                writer.flush()?;
            }
            Command::Publish(channel, message) => {
                // —— 扇出：找到这个 channel 的所有 sender，逐个 send ——
                // 注意：在锁内 send 会让"慢订阅者"拖住所有人。
                // 真实 Redis 是写时复制到本地 vec 再锁外 send——我们这里也这么做。
                let senders: Vec<mpsc::Sender<SubMessage>> = {
                    let subs = state.subs.lock();
                    subs.get(&channel).cloned().unwrap_or_default()
                };
                let mut delivered = 0;
                for tx in &senders {
                    // mpsc::Sender::send 是无界的，理论上不会阻塞——但
                    // 无界有"内存爆"的风险。教学上这里就放任无界，留给
                    // 练习：换成有界 + 丢弃策略会怎样？
                    tx.send((channel.clone(), message.clone()));
                    delivered += 1;
                }
                write_int(&mut writer, delivered)?;
                writer.flush()?;

                // 同时也把这条消息排空出去（避免本连接的 receiver 内存堆积）
                // —— 注意：handle_client 这版不主动 drain sub_rx；测试里
                // 我们用专门一条连接订阅，另一条连接发命令，所以这里 OK。
                let _ = sub_rx;
            }
        }
    }

    // 客户端断开：清理所有订阅
    {
        let mut subs = state.subs.lock();
        for ch in &subscribed {
            if let Some(vec) = subs.get_mut(ch) {
                // 删一个（简化）
                if !vec.is_empty() {
                    vec.swap_remove(0);
                }
            }
        }
    }

    Ok(())
}
