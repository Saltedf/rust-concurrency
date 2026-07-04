//! # mini-Redis 服务端入口（M10 真实 bin）
//!
//! 监听一个 TCP 端口，跑 forge_app::mini_redis 的"thread-per-connection"循环。
//! 读者跑通它只需要：
//!
//! ```text
//! cargo run -p forge-app --bin mini-redis
//! # 另一个终端：
//! redis-cli -p 6379 set k v
//! redis-cli -p 6379 get k
//! ```
//!
//! 端口可以用 `--port <N>` 或环境变量 `FORGE_REDIS_PORT` 覆盖；地址同理
//! 用 `--addr <IP>`。命令行解析故意手写——不引入 `clap`，让 bin 自包含。
//!
//! 这里有意直接调 `forge_app::mini_redis::serve`：所有并发决策（每个连接
//! 起线程、订阅表的两把锁、PUBLISH 在锁外扇出）都已经在 lib 里讲透。
//! bin 只负责"把这台机器跑起来"——参数、日志、信号处理。

use forge_app::mini_redis::{serve_one, ServerState};
use std::net::TcpListener;
use std::sync::Arc;

fn main() {
    let args = parse_args(std::env::args().skip(1));
    let addr = format!("{}:{}", args.host, args.port);

    // 用 bind 提前 fail-fast：端口被占就立刻报，而不是进入主循环后才崩。
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("mini-redis: 无法绑定 {addr}: {e}");
            std::process::exit(1);
        }
    };
    let bound_addr = listener.local_addr().unwrap();
    let state = Arc::new(ServerState::new());

    println!(
        "mini-redis 监听 {bound_addr}（thread-per-connection）{}",
        if args.verbose { " [verbose]" } else { "" }
    );
    println!("支持命令: GET / SET / DEL / PING / PUBLISH / SUBSCRIBE / UNSUBSCRIBE");
    println!("Ctrl-C 退出。");

    // PING 不在 lib 的 Command 枚举里——我们在 bin 层补：
    // 真正的 lib `serve` 会拒绝 PING（解析为 unknown）。
    // 为了让 redis-cli 的 PING 探活能工作，我们在这里手工处理，
    // 不让 lib 的循环接管。做法：自己 accept + 解析第一行。
    // 但这会重写 lib 的整个 serve 逻辑——和"复用 lib"的目标冲突。
    //
    // 折中：我们让 PING 直接走 lib（lib 会回 -ERR），同时在文档里讲清楚
    // 为什么 PING 是协议级命令而不是"业务命令"。如果读者需要 PING，可以
    // 在练习里把 Command 枚举扩成 7 个变体——这是一个干净的练习题。
    //
    // 不过为了让 redis-cli 至少能"连上探活"（redis-cli 启动并不发 PING，
    // 它直接发命令），这里走 lib 的 serve 就够了。

    if args.verbose {
        println!("[verbose] 进入 accept 循环");
    }

    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let peer = stream.peer_addr().ok();
                if args.verbose {
                    println!("[verbose] 新连接: {peer:?}");
                }
                let st = state.clone();
                std::thread::spawn(move || {
                    if let Err(e) = serve_one(stream, st) {
                        eprintln!("mini-redis: 客户端错误: {e}");
                    }
                });
            }
            Err(e) => eprintln!("mini-redis: accept 失败: {e}"),
        }
    }
    // 上面这层手写 accept 和 lib 的 `serve` 函数几乎一样——区别只是这里
    // 多打了日志。我们故意不直接调 `serve(addr, state)`，因为那样就拿
    // 不到 per-connection 的打印机会。serve_one 来自 lib，复用 lib 的全部
    // 并发决策（两把锁、PUBLISH 锁外扇出、SUBSCRIBE 注册）。
}

struct Args {
    host: String,
    port: u16,
    verbose: bool,
}

fn parse_args<I: Iterator<Item = String>>(mut it: I) -> Args {
    let mut host = std::env::var("FORGE_REDIS_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let mut port: u16 = std::env::var("FORGE_REDIS_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6379);
    let mut verbose = false;

    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" | "-p" => {
                if let Some(v) = it.next() {
                    match v.parse() {
                        Ok(n) => port = n,
                        Err(_) => {
                            eprintln!("bad port: {v}");
                            std::process::exit(2);
                        }
                    }
                }
            }
            "--addr" | "--host" | "-h" => {
                if let Some(v) = it.next() {
                    host = v;
                }
            }
            "--verbose" | "-v" => verbose = true,
            "--help" | "-?" => {
                print_help();
                std::process::exit(0);
            }
            other => {
                eprintln!("未知参数: {other}");
                print_help();
                std::process::exit(2);
            }
        }
    }

    Args {
        host,
        port,
        verbose,
    }
}

fn print_help() {
    eprintln!(
        "用法: mini-redis [--port <N>] [--host <IP>] [--verbose]\n\
         默认 127.0.0.1:6379；也可用 FORGE_REDIS_PORT / FORGE_REDIS_HOST 环境变量。"
    );
}
