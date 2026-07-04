//! # epoll echo 服务器（M10 加分 bin，Linux 专属真正实现）
//!
//! 用 forge_app::bare_server 的 epoll 循环做最朴素的 echo。
//!
//! ```text
//! cargo run -p forge-app --bin echo-server
//! # 另一个终端：
//! nc 127.0.0.1 7878
//! ```
//!
//! 非 Linux 平台 bare_server.rs 模块体为空，本 bin 在那里只打印一条提示
//! 退出，不参与 epoll 的教学——把"epoll 是 Linux 专属"的事实直白地
//! 摆给读者。

#[cfg(target_os = "linux")]
fn main() {
    use forge_app::bare_server::BareEchoServer;
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    let port: u16 = std::env::var("ECHO_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7878);

    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
    let server = match BareEchoServer::bind(addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("echo-server: 绑定 {addr} 失败: {e}");
            std::process::exit(1);
        }
    };
    println!("echo-server: 监听 127.0.0.1:{port}（epoll，edge-triggered，单线程）");
    println!("用 `nc 127.0.0.1 {port}` 连上打字，服务器逐字节回显。Ctrl-C 退出。");

    if let Err(e) = server.serve(64, None) {
        eprintln!("echo-server: 事件循环退出: {e}");
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!(
        "echo-server: 仅在 Linux 上提供真正实现（epoll）。\n\
         你当前不是 Linux，本 bin 没东西可跑。"
    );
}
