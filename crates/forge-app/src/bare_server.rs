//! # 零依赖异步服务器（M10 子应用之三，呼应 Async Rust 第 10 章）
//!
//! 这一节回答第三个问题：**一台机器怎么同时管几千条 TCP 连接？**
//!
//! M10 主干里的 mini-Redis 给的答案是"thread-per-connection"——每来一条
//! 连接，起一个线程。1000 连接 = 1000 线程。这在教学上清楚，但每个线程
//! 占 8MB 栈，1000 个就是 8GB 内存，操作系统线程调度也扛不住。
//!
//! 这里的答案叫 **I/O 多路复用（I/O multiplexing）**：一个线程就能同时
//! 监视几千条 socket，哪条上有数据可读操作系统叫醒它。在 Linux 上这个
//! 系统调用叫 **epoll**。我们这一节**直接调 libc 的 syscall**——不依赖
//! mio、不依赖 tokio——把"一台机器扛 10 万连接"的运行时内核从底层扒开
//! 给你看。
//!
//! 注意：本模块**只在 Linux 上编译**。epoll 是 Linux 专属。其他平台有
//! 等价物（BSD/macOS 的 kqueue、Windows 的 IOCP），但 syscall 接口完全
//! 不同，我们留给练习。本模块所有代码用 `#[cfg(target_os = "linux")]`
//! 门控——非 Linux 平台上 `cargo build` 不报错，只是这个模块为空。
//!
//! ## epoll 三步走
//!
//! 1. **建一棵 epoll 实例**：`epoll_create1(0)` 返回一个 fd。这棵"实例"
//!    是一个内核对象，你往里登记"我想监视哪些 fd"。
//! 2. **登记关心的 fd**：`epoll_ctl(epfd, EPOLL_CTL_ADD, fd, &event)`。
//!    `event` 里写"这条 fd 上发生什么事件我才被叫醒"——比如 `EPOLLIN`
//!    （可读）。
//! 3. **等事件发生**：`epoll_wait(epfd, &mut events, timeout_ms)` 阻塞
//!    直到至少有一个事件就绪。返回值告诉你"这次叫醒你有几条 fd 就绪了"，
//!    你遍历这些 fd 做对应的 read/write。
//!
//! ## edge-triggered vs level-triggered
//!
//! epoll 有两种触发模式：
//!
//! - **level-triggered（默认）**：只要 fd 上**还有数据可读**，每次
//!   `epoll_wait` 都会把它返回。你只读了一半？下次 wait 它还在。
//! - **edge-triggered（`EPOLLET` 标志）**：只在 fd **状态变化时**通知
//!   一次——从"无数据"变成"有数据"的那一刻通知一次。如果你只读了一半
//!   就回去 wait，**不会再被叫醒**——内核认为"已经通知过你了"。
//!
//! edge-triggered 听上去坑更大，为什么还要用？因为它**系统调用次数更少**
//! ——level-triggered 每次都重复通知，浪费 CPU。代价是 edge-triggered
//! **必须读到 `EAGAIN`**（也就是 read 返回 "WouldBlock"），把 fd 里的
//! 数据彻底抽干，否则剩下的数据会被饿死。这就是手算例 3 要讲的坑。
//!
//! ## 为什么必须配合非阻塞 socket？
//!
//! 想象 edge-triggered 模式下，一个客户端发了 100 字节，你被叫醒，调
//! `read` 读到 50 字节——但客户端**还在慢慢发**。如果你的 socket 是
//! **阻塞**的，下一次 `read` 会卡住整个事件循环，等剩下的 50 字节到——
//! 这期间其它几千条连接全被晾着。这就是为什么 I/O 多路复用必须配
//! **非阻塞 socket**：read 没数据时立即返回 `EAGAIN`，让你回去 `wait`
//! 等下一条 fd。

#![cfg(target_os = "linux")]

use libc::{
    accept4, c_int, c_void, close, epoll_create1, epoll_ctl, epoll_event, epoll_wait, setsockopt,
    sockaddr_in, socklen_t, AF_INET, EPOLLERR, EPOLLET, EPOLLHUP, EPOLLIN, EPOLL_CLOEXEC,
    EPOLL_CTL_ADD, EPOLL_CTL_DEL, SOCK_NONBLOCK, SOCK_STREAM, SOL_SOCKET, SO_REUSEADDR,
};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::unix::io::RawFd;
use std::time::Duration;

// 我们直接用 RawFd（裸 fd）操作，不包 std::net::TcpListener——因为
// std::net 的 set_nonblocking 是方法，会和 libc 直接 accept 出来的
// 裸 fd 对不上。教学上"零依赖"就该零抽象：所有东西都是 syscall。

/// 一个跑在 epoll 之上的 echo 服务器。构造完调 `serve`，它会一直循环到
/// 被外部 close 掉。
pub struct BareEchoServer {
    listen_fd: RawFd,
    epfd: RawFd,
}

impl BareEchoServer {
    /// 绑定一个地址、设非阻塞、登记到 epoll 实例上。
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        // —— Step 1: epoll_create1 —— 建一棵 epoll 实例
        let epfd = unsafe { epoll_create1(EPOLL_CLOEXEC) };
        if epfd < 0 {
            return Err(io::Error::last_os_error());
        }

        // —— Step 2: 建 listening socket —— SOCK_NONBLOCK 一上来就非阻塞
        let listen_fd = unsafe { libc::socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0) };
        if listen_fd < 0 {
            let e = io::Error::last_os_error();
            unsafe { close(epfd) };
            return Err(e);
        }

        // SO_REUSEADDR：避免 TIME_WAIT 状态的端口挡住重启。教学版里
        // 测试反复 bind 同一端口时这一条特别有用。
        let one: c_int = 1;
        unsafe {
            setsockopt(
                listen_fd,
                SOL_SOCKET,
                SO_REUSEADDR,
                &one as *const _ as *const c_void,
                std::mem::size_of::<c_int>() as socklen_t,
            );
        }

        // bind + listen
        let v4 = match addr {
            SocketAddr::V4(v4) => v4,
            SocketAddr::V6(_) => {
                let e = io::Error::new(io::ErrorKind::InvalidInput, "IPv6 unsupported (teaching)");
                unsafe {
                    close(listen_fd);
                    close(epfd)
                };
                return Err(e);
            }
        };
        let mut sin: sockaddr_in = unsafe { std::mem::zeroed() };
        sin.sin_family = AF_INET as u16;
        sin.sin_port = v4.port().to_be();
        sin.sin_addr.s_addr = u32::from_be_bytes(v4.ip().octets()).to_be();
        let ret = unsafe {
            libc::bind(
                listen_fd,
                &sin as *const _ as *const libc::sockaddr,
                std::mem::size_of::<sockaddr_in>() as socklen_t,
            )
        };
        if ret < 0 {
            let e = io::Error::last_os_error();
            unsafe {
                close(listen_fd);
                close(epfd)
            };
            return Err(e);
        }

        let ret = unsafe { libc::listen(listen_fd, 128) };
        if ret < 0 {
            let e = io::Error::last_os_error();
            unsafe {
                close(listen_fd);
                close(epfd)
            };
            return Err(e);
        }

        // —— Step 3: 把 listening socket 登记到 epoll ——
        // EPOLLIN: 有连接可 accept
        // EPOLLET: edge-triggered——只在"从无到有新连接"那一瞬间通知
        let mut ev = epoll_event {
            events: (EPOLLIN | EPOLLET) as u32,
            // u64 数据位：用来标识"这个就绪 fd 是谁"。我们把 fd 本身塞进去。
            u64: listen_fd as u64,
        };
        let ret = unsafe { epoll_ctl(epfd, EPOLL_CTL_ADD, listen_fd, &mut ev) };
        if ret < 0 {
            let e = io::Error::last_os_error();
            unsafe {
                close(listen_fd);
                close(epfd)
            };
            return Err(e);
        }

        Ok(Self { listen_fd, epfd })
    }

    /// 当前 epoll 实例的 fd。给测试用：测试可以 close 它强制中断 wait。
    pub fn epoll_fd(&self) -> RawFd {
        self.epfd
    }

    pub fn listen_fd(&self) -> RawFd {
        self.listen_fd
    }

    /// 跑事件循环。`max_events_per_poll` 是一次 epoll_wait 最多取几条
    /// 事件（决定"积压很多时一次处理多少"）。`timeout` 是每次 wait 的
    /// 超时——None 表示无限等。
    ///
    /// 这就是手算例 3 那张"3 连接同时就绪"逐拍图的本质。每次循环：
    /// 1. `epoll_wait` 返回 n 条就绪 fd
    /// 2. 对每条 fd：如果是 listening fd → accept 新连接并登记；
    ///    如果是已建立的连接 fd → read 直到 EAGAIN，把读到的写回去
    pub fn serve(&self, max_events_per_poll: usize, timeout: Option<Duration>) -> io::Result<()> {
        let mut events: Vec<epoll_event> =
            vec![epoll_event { events: 0, u64: 0 }; max_events_per_poll];
        let timeout_ms = timeout.map(|d| d.as_millis() as c_int).unwrap_or(-1);

        loop {
            let n = unsafe {
                epoll_wait(
                    self.epfd,
                    events.as_mut_ptr(),
                    max_events_per_poll as c_int,
                    timeout_ms,
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue; // EINTR：被信号打断，再来一次
                }
                return Err(e);
            }

            for i in 0..(n as usize) {
                let ev = &events[i];
                let fd = ev.u64 as RawFd;
                let flags = ev.events;

                if fd == self.listen_fd {
                    // listening fd 就绪：accept 所有排队的新连接。
                    // edge-triggered 必须"读到 EAGAIN"——accept 也不例外，
                    // 否则 accept 一次就回去 wait，剩余排队连接永远等不到通知。
                    loop {
                        let new_fd = unsafe {
                            accept4(
                                self.listen_fd,
                                std::ptr::null_mut(),
                                std::ptr::null_mut(),
                                SOCK_NONBLOCK, // 新连接也设非阻塞
                            )
                        };
                        if new_fd < 0 {
                            let e = io::Error::last_os_error();
                            if e.kind() == io::ErrorKind::WouldBlock {
                                break; // 接收队列空了，EAGAIN——退出循环
                            }
                            break; // 其它错误：忽略，别让一条 accept 拖垮循环
                        }
                        self.add_conn(new_fd)?;
                    }
                } else {
                    // 已建立连接就绪：read 直到 EAGAIN，把读到的写回（echo）。
                    // 对端关闭（EPOLLHUP）或出错（EPOLLERR）也要清理。
                    if (flags & (EPOLLERR as u32 | EPOLLHUP as u32)) != 0 {
                        self.remove_conn(fd);
                        continue;
                    }
                    self.handle_conn(fd)?;
                }
            }
        }
    }

    /// 把一个新连接的 fd 登记到 epoll 实例上。
    fn add_conn(&self, fd: RawFd) -> io::Result<()> {
        let mut ev = epoll_event {
            events: (EPOLLIN | EPOLLET) as u32,
            u64: fd as u64,
        };
        let ret = unsafe { epoll_ctl(self.epfd, EPOLL_CTL_ADD, fd, &mut ev) };
        if ret < 0 {
            let e = io::Error::last_os_error();
            unsafe { close(fd) };
            return Err(e);
        }
        Ok(())
    }

    fn remove_conn(&self, fd: RawFd) {
        // 从 epoll 摘除并 close。EPOLL_CTL_DEL 后 fd 自动不再被监视。
        unsafe {
            epoll_ctl(self.epfd, EPOLL_CTL_DEL, fd, std::ptr::null_mut());
            close(fd);
        }
    }

    /// 处理一条已建立的连接：循环 read 直到 EAGAIN，把读到的写回去。
    /// 这就是"edge-triggered 必须抽干"那条规则在代码里的样子。
    fn handle_conn(&self, fd: RawFd) -> io::Result<()> {
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::WouldBlock {
                    // EAGAIN / EWOULDBLOCK：fd 暂时没数据了。
                    // edge-triggered 下这至关重要——停下来回去 wait。
                    return Ok(());
                }
                // 其它错误：关连接
                self.remove_conn(fd);
                return Ok(());
            }
            if n == 0 {
                // 对端正常关闭（EOF）。摘除并 close。
                self.remove_conn(fd);
                return Ok(());
            }
            // 写回——简化：不处理 partial write 和 EAGAIN on write 的完整
            // 状态机（真实服务器要再注册 EPOLLOUT 等可写时续写）。echo
            // 数据少，影响有限，留给练习。
            let mut written = 0usize;
            while written < n as usize {
                let w = unsafe {
                    libc::write(
                        fd,
                        buf[written..n as usize].as_ptr() as *const c_void,
                        (n as usize) - written,
                    )
                };
                if w < 0 {
                    let e = io::Error::last_os_error();
                    if e.kind() == io::ErrorKind::WouldBlock {
                        break; // 写缓冲满了：放弃剩余（教学简化）
                    }
                    self.remove_conn(fd);
                    return Ok(());
                }
                written += w as usize;
            }
        }
    }
}

impl Drop for BareEchoServer {
    fn drop(&mut self) {
        unsafe {
            close(self.listen_fd);
            close(self.epfd);
        }
    }
}

/// 一条便利函数：在 `127.0.0.1:0`（操作系统随机分配端口）上起一个
/// BareEchoServer，返回 `(服务器, 端口)`。给测试用。
pub fn bind_loopback_random() -> io::Result<(BareEchoServer, u16)> {
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
    let server = BareEchoServer::bind(addr)?;
    // 取出实际分配的端口：getsockname
    let mut sin: sockaddr_in = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<sockaddr_in>() as socklen_t;
    let ret = unsafe {
        libc::getsockname(
            server.listen_fd,
            &mut sin as *mut _ as *mut libc::sockaddr,
            &mut len,
        )
    };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    let port = u16::from_be(sin.sin_port);
    Ok((server, port))
}
