//! `forge-app` —— 把前面所有原语，组装成两个**真实应用**。
//!
//! 模块 **M10**：
//! - `crawler`：并发网页爬虫（Semaphore 按域名限速、`Arc<Mutex<HashSet>>` 去重、mpsc 汇聚结果）
//! - `mini_redis`：mini-Redis（自研锁做 KV 存储、PUBLISH/SUBSCRIBE 经自研通道/事件总线）
//!
//! 二进制入口在 `src/bin/`。详见 `docs/modules/M10-apps.md`。

pub mod crawler;
pub mod mini_redis;

// —— M10 三个补缺子应用 ——
// event_bus / actor 跨平台；bare_server 只在 Linux 上编译（epoll 专属）。
pub mod actor;
#[cfg(target_os = "linux")]
pub mod bare_server;
pub mod event_bus;
