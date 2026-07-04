//! # 并发网页爬虫（M10 主应用之一）
//!
//! 把前面所有原语串起来：用 `forge_lockfree::Semaphore` 给每个域名限速、
//! 用 `forge_sync::Mutex<HashSet>` 做去重、用 `forge_channel::mpsc` 把抓到的
//! 页面汇聚到一个写盘点。worker 池用普通 scoped thread，因为本模块的焦点是
//! "并发决策"而不是运行时——M9a 的池可以在练习里替换进来。
//!
//! 设计取舍见 `docs/modules/M10-applications.md`。

use forge_channel::mpsc;
use forge_sync::mutex::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Condvar;

// 关于"为什么不用 forge_lockfree::Semaphore"：那个 Semaphore 用单个 AtomicU32 +
// atomic-wait 实现，在低并发、少量等待者时工作得很好（见 M8a 测试）。但在
// 大量等待者 + 频繁唤醒的爬虫场景下，我们这里实测出现了 wake 偶发丢失的死锁
// （根因分析留给 M11 的"并发 bug 调试"小节）。生产里这种限速器一般直接用
// `Mutex + Condvar + 计数器`——虽然慢一点点，但行为可预测、不会丢唤醒。
// 这里我们手写一个，让它和 M8a 的信号量在教程里形成对照。

/// 抓回来的一个页面。`url` 是它的最终地址（已经过规范化），
/// `body` 是响应正文（测试里我们只放几行带链接的 HTML 片段）。
#[derive(Debug, Clone)]
pub struct Page {
    pub url: String,
    pub body: String,
}

/// 抓取能力。生产里我们会写一个 `UreqFetcher`（基于 `ureq`，阻塞）；
/// 测试里写一个 `MockFetcher`，从一张静态表里查 body，绝不联网。
///
/// 为什么要 trait？因为我们要在测试里替换它——否则要么得连真服务器
/// （慢、不稳定、CI 不能用），要么得把 fetch 写进生产代码里再 mock（污染）。
/// trait 把"什么算一次抓取"定义成一台机器上的一个洞，洞后面接什么都行。
pub trait Fetcher {
    fn fetch(&self, url: &str) -> Result<Page, String>;
}

/// 限速配额：每个域名同时最多能开几个连接。
///
/// 这个数字不是随便选的。把人家服务器当兄弟：对方一台机器的并发处理能力
/// 是有限的，如果你这一台客户端瞬间开 500 条连接，对方的 accept 队列可能
/// 满到丢包，看起来像 DDoS；正经的爬虫作者会自我设限（ politeness ）。
/// 本教程里默认 2，足够讲清楚限速机制又不至于把例子搞复杂。
pub const DEFAULT_PER_DOMAIN_CONCURRENCY: usize = 2;

/// 链接提取。从一段 HTML 里粗略地把 `<a href="...">` 里的 URL 拽出来。
///
/// 我们故意不引入一个真正的 HTML 解析器（`scraper` / `html5ever`）——
/// 那会让教程的注意力跑到 HTML 文法上去。这里只做一个最小匹配：
/// 找到所有 `href="` 后面跟到下一个 `"` 为止的子串。够 mock 用，也够
/// 抓大多数结构良好的页面。教程里会把这条决策讲清楚。
pub fn extract_links(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let needle = b"href=\"";
    let bytes = html.as_bytes();
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let start = i + needle.len();
            let end = bytes[start..]
                .iter()
                .position(|&b| b == b'"')
                .map(|p| start + p)
                .unwrap_or(bytes.len());
            if end > start {
                let href = &html[start..end];
                if href.starts_with("http://") || href.starts_with("https://") {
                    out.push(href.to_string());
                }
            }
            i = end + 1;
        } else {
            i += 1;
        }
    }
    out
}

/// 从 URL 字符串里粗略取出"域名"：`https://a.com/x` → `a.com`。
/// 这只用于给"每个域名"发一把 Semaphore，不参与正确性。
pub fn domain_of(url: &str) -> String {
    let after_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let host = after_scheme.split('/').next().unwrap_or("");
    // 去掉可能的 :port
    host.split(':').next().unwrap_or("").to_string()
}

/// 一个域名级并发限速器：`acquire` 拿一张许可，`drop` 归还。
///
/// 它就是"信号量"（M8a），但用 `Mutex + Condvar + 计数器` 实现——
/// 比 `forge_lockfree::Semaphore` 慢一点（每次 acquire/release 都过锁），
/// 但保证**绝不会丢唤醒**。理由：爬虫的并发压力主要在"按域名排队"上，
/// 每个域名同时只有几个 worker 在 acquire，锁竞争很小；而"绝对不死锁"
/// 的保证比每秒多打几次 CAS 重要得多。
pub struct DomainLimiter {
    /// 共享的"剩余许可数 + Condvar"。同一把锁把"读计数 + 改计数 + 等待"
    /// 捆绑成一个原子动作——这正是 Condvar + Mutex 模型相对裸 atomic 的优势：
    /// 你不用自己处理"读到 0 但还没 wait 时被别人 release"的窗口。
    inner: std::sync::Mutex<LimInner>,
    cv: Condvar,
}

struct LimInner {
    available: usize,
}

impl DomainLimiter {
    pub fn new(permits: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: std::sync::Mutex::new(LimInner { available: permits }),
            cv: Condvar::new(),
        })
    }

    /// 拿一张许可；不够就在 Condvar 上等。返回一个 RAII 守卫，
    /// drop 时自动归还——这样 worker 不可能忘记 release。
    pub fn acquire(&self) -> Permit<'_> {
        let mut g = self.inner.lock().unwrap();
        while g.available == 0 {
            g = self.cv.wait(g).unwrap();
        }
        g.available -= 1;
        Permit { limiter: self }
    }

    fn release(&self) {
        let mut g = self.inner.lock().unwrap();
        g.available += 1;
        // 把锁释放后再 notify，避免"等锁的唤醒者立刻又被阻塞"的浪费。
        // （std 的 Condvar 这样写是惯用法。）
        drop(g);
        self.cv.notify_one();
    }
}

/// RAII 许可：drop 时归还。这样在 worker 里就能写成
/// `let _p = limiter.acquire(); fetch(); ` —— fetch 出错早返回也安全。
pub struct Permit<'a> {
    limiter: &'a DomainLimiter,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.limiter.release();
    }
}

/// 内部共享状态。
struct CrawlState<F: Fetcher> {
    fetcher: F,
    /// 已访问（或正在访问）的 URL 集合。"检查 + 插入"必须在同一把锁里——
    /// 见教程手算例 2：拆开就会有两个 worker 同时通过 contains 检查、都去抓。
    visited: Mutex<HashSet<String>>,
    /// 按域名发的限速器。第一把锁保护这张表，表里每条 value 才是真正的限速器。
    per_domain: Mutex<HashMap<String, Arc<DomainLimiter>>>,
    per_domain_permits: usize,
}

impl<F: Fetcher> CrawlState<F> {
    /// 给定一个域名，返回它所属域名的限速器。
    /// 关键：表里没这个域名时，我们 **先建好 limiter 再插进去**，整个过程
    /// 在锁里完成，保证一个域名只会有一个 limiter。
    fn limiter_for(&self, domain: &str) -> Arc<DomainLimiter> {
        // 快速路径：先读
        {
            let map = self.per_domain.lock();
            if let Some(lim) = map.get(domain) {
                return lim.clone();
            }
        }
        // 慢速路径：拿了写权限再检查一次（防止两个 worker 同时进入慢速路径）
        let mut map = self.per_domain.lock();
        map.entry(domain.to_string())
            .or_insert_with(|| DomainLimiter::new(self.per_domain_permits))
            .clone()
    }

    /// 原子的"检查 + 标记"。返回 true 表示"这次归我了，去抓吧"，
    /// 返回 false 表示"别人已经在抓 / 抓完了"。
    fn claim(&self, url: &str) -> bool {
        let mut set = self.visited.lock();
        set.insert(url.to_string())
    }
}

/// 爬虫本体。`seed_urls` 是起点；`max_pages` 是停止条件（防止无限抓下去）。
pub struct Crawler<F: Fetcher> {
    fetcher: F,
    per_domain_permits: usize,
    max_pages: usize,
    /// 待抓队列。有界，**背压**：队列满了，生产者阻塞在 send 上，自然慢下来。
    /// 呼应 M5 手算例 2：无界队列会让内存涨爆，有界队列把"快慢不匹配"
    /// 转换成"慢方对快方的反压"。
    queue_bound: usize,
    n_workers: usize,
}

/// 一条抓取结果。workers 把这种东西经 mpsc 送给唯一的写盘线程。
#[derive(Debug)]
pub struct CrawledPage {
    pub url: String,
    pub body: String,
    /// 从这个页面解析出的、已经成功入队的新链接数。
    pub new_links: usize,
}

impl<F: Fetcher + Send + Sync + 'static> Crawler<F> {
    pub fn new(fetcher: F) -> Self {
        Self {
            fetcher,
            per_domain_permits: DEFAULT_PER_DOMAIN_CONCURRENCY,
            max_pages: 50,
            queue_bound: 64,
            n_workers: 8,
        }
    }

    pub fn per_domain(mut self, n: usize) -> Self {
        self.per_domain_permits = n;
        self
    }

    pub fn max_pages(mut self, n: usize) -> Self {
        self.max_pages = n;
        self
    }

    pub fn queue_bound(mut self, n: usize) -> Self {
        self.queue_bound = n;
        self
    }

    pub fn workers(mut self, n: usize) -> Self {
        self.n_workers = n;
        self
    }

    /// 跑一次完整爬取，返回所有抓到的页面（按抓到的顺序）。
    ///
    /// 这个函数里没有 `unsafe`、没有裸线程；并发决策全部写在这里：
    /// 1. 待抓队列是有界的（我们手写一个 `Mutex<VecDeque> + Condvar` 的有界
    ///    版本，因为 `forge_channel::mpsc` 是无界的——背压恰恰来自"有界"）。
    /// 2. 每个 worker 抓一个 URL 前，先 `claim`（原子的检查+插入），
    ///    再 `semaphore_for(domain).acquire()`，再 fetch。
    /// 3. 结果汇聚到主线程，主线程一边收结果一边把新链接塞回待抓队列，
    ///    直到收够 `max_pages` 页；之后关闭队列让 workers 退出。
    ///
    /// **何时算"结束"** 是这个函数最棘手的并发决策。看一个错误版本：
    /// "队列空就结束"——但是队列空时，可能正有一个 worker 在 fetch 一个
    /// 即将吐出 10 条新链接的页面，主线程这时关掉队列就把那 10 条都丢了。
    /// 正确的做法是**显式追踪"未完成工作数"**：入队 +1，一条 URL 被 worker
    /// 完整处理完（无论成败）-1；只有当它归零、队列又空，才是真正没活干。
    pub fn run(self, seed_urls: Vec<String>) -> Vec<CrawledPage> {
        let pending = Arc::new(BoundedQueue::new(self.queue_bound));
        // "未送达结果"计数：worker 决定要发一条结果时 +1，主线程收一条时 -1。
        // 这个计数器的语义是"还会有多少条结果到达 result_rx"。
        // 当它归零时，主线程可以确定不会再有结果来了——可以安全结束。
        // （注意：worker 必须先 inc 再 send，否则主线程可能在 send 之后、
        //  inc 之前看到 0 而提前退出，丢掉这条结果。）
        let in_flight_results = Arc::new(std::sync::atomic::AtomicUsize::new(seed_urls.len()));
        for u in &seed_urls {
            pending.push(u.clone());
        }

        // 结果通道：workers → 主线程。无界（结果总是比待抓项少，不会爆）。
        let (result_tx, result_rx) = mpsc::channel();

        let state = Arc::new(CrawlState {
            fetcher: self.fetcher,
            visited: Mutex::new(HashSet::new()),
            per_domain: Mutex::new(HashMap::new()),
            per_domain_permits: self.per_domain_permits,
        });

        // 预先把种子标记成已 claim（避免 workers 把种子再塞回队列）。
        {
            let mut v = state.visited.lock();
            for u in &seed_urls {
                v.insert(u.clone());
            }
        }

        let n_workers = self.n_workers;
        let max_pages = self.max_pages;

        std::thread::scope(|s| {
            // —— workers ——
            for _ in 0..n_workers {
                let state = state.clone();
                let pending = pending.clone();
                let in_flight = in_flight_results.clone();
                let result_tx = result_tx.clone();
                s.spawn(move || loop {
                    let url = match pending.pop() {
                        Some(u) => u,
                        None => break, // 主线程关了队列
                    };

                    let domain = domain_of(&url);
                    let limiter = state.limiter_for(&domain);
                    let _permit = limiter.acquire(); // drop 时自动归还
                    let page_result = state.fetcher.fetch(&url);
                    // _permit 在这里 drop：归还许可

                    match page_result {
                        Ok(page) => {
                            let mut new_links = 0;
                            for link in extract_links(&page.body) {
                                if state.claim(&link) {
                                    new_links += 1;
                                    // 这条新链接会变成一次未来的 fetch，
                                    // 也就是一条未来的结果：现在就 +1
                                    in_flight.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                    pending.push(link);
                                }
                            }
                            // **先 inc 再 send？不——这里 inc 已经在 push 时
                            // 为"未来结果"做过了；本条结果是种子/早期 +1 对应
                            // 的那次 fetch 完成的产物，所以现在直接 send。**
                            // 不过有一条对应"现在这条 URL"的 +1 是发生在它被
                            // 入队的那一刻（种子由主线程隐式 +n，新链接由这里
                            // +1），所以 send 一条结果等价于消费一次那笔预算。
                            result_tx.send(CrawledPage {
                                url: page.url,
                                body: page.body,
                                new_links,
                            });
                        }
                        Err(_e) => {
                            // 失败：这条 URL 当初占了一个 +1 名额但不会发
                            // 结果。我们必须把名额还回去，否则 in_flight
                            // 永远不归零，主线程会卡在 recv。
                            in_flight.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                });
            }

            // —— 主线程：收结果；够了或没活了就关队列 ——
            let mut collected = Vec::new();
            while collected.len() < max_pages {
                let cur = in_flight_results.load(std::sync::atomic::Ordering::SeqCst);
                if cur == 0 {
                    break;
                }
                let page = result_rx.recv();
                in_flight_results.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                collected.push(page);
            }
            pending.close();
            collected
        })
    }
}

// ----- 一个最小的有界队列（Mutex<VecDeque> + Condvar + 关闭位）-----
//
// 这是 forge_channel::mpsc 的"有界 + 多消费者 + 关闭"版本。教程里会
// 把它和 forge_channel::mpsc 摆在一起对比：背压来自"满了就 wait"。

use std::collections::VecDeque;

struct BoundedQueue {
    inner: std::sync::Mutex<Inner>,
    not_full: Condvar,
    not_empty: Condvar,
}

struct Inner {
    buf: VecDeque<String>,
    cap: usize,
    closed: bool,
}

impl BoundedQueue {
    fn new(cap: usize) -> Self {
        Self {
            inner: std::sync::Mutex::new(Inner {
                buf: VecDeque::with_capacity(cap),
                cap,
                closed: false,
            }),
            not_full: Condvar::new(),
            not_empty: Condvar::new(),
        }
    }

    fn push(&self, url: String) {
        let mut g = self.inner.lock().unwrap();
        while g.buf.len() >= g.cap && !g.closed {
            g = self.not_full.wait(g).unwrap();
        }
        if g.closed {
            return;
        }
        g.buf.push_back(url);
        self.not_empty.notify_one();
    }

    fn pop(&self) -> Option<String> {
        let mut g = self.inner.lock().unwrap();
        loop {
            if let Some(u) = g.buf.pop_front() {
                self.not_full.notify_one();
                return Some(u);
            }
            if g.closed {
                return None;
            }
            g = self.not_empty.wait(g).unwrap();
        }
    }

    fn close(&self) {
        let mut g = self.inner.lock().unwrap();
        g.closed = true;
        // 唤醒所有在等"非空"的 worker，让它们重看 close 标志退出
        self.not_empty.notify_all();
        // 唤醒所有在等"非满"的 push，让它们退出
        self.not_full.notify_all();
    }
}

// =========================================================================
//  bin 入口的可注入包装（m10_bin_crawler 集成测试用）
//
//  bin crate 不能被测试 import，所以我们把"接收一个 fetcher + 一组参数
//  跑完整爬取"的入口放在 lib 里——bin 只是 parse_args + 选 fetcher +
//  调 run。测试注入 MockFetcher 走这条路，不联网、可复现。
// =========================================================================

/// 命令行解析后的参数。bin 把它和具体 fetcher 一起喂给 `run`。
#[derive(Debug, Clone)]
pub struct CliArgs {
    pub seed: String,
    pub max: usize,
    pub workers: usize,
    /// 每个域名同时开多少连接（与 workers 解耦，见 lib 文档）。
    pub per_domain: usize,
}

/// 跑完整爬取。返回抓到的页数。这是 bin main 的"逻辑部分"被抽出来——
/// 测试用 MockFetcher 调它，避免联网。
pub fn run_with_fetcher<F: Fetcher + Send + Sync + 'static>(
    fetcher: F,
    args: CliArgs,
) -> Vec<CrawledPage> {
    Crawler::new(fetcher)
        .max_pages(args.max)
        .workers(args.workers)
        .per_domain(args.per_domain)
        .run(vec![args.seed])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个 mock fetcher：内部就是一张 `HashMap<url, body>`，查到就返回 Page。
    /// 这样测试永远不联网，且完全可复现。
    struct MockFetcher {
        pages: HashMap<String, String>,
    }

    impl MockFetcher {
        fn new(pages: &[(&str, &str)]) -> Self {
            Self {
                pages: pages
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            }
        }
    }

    impl Fetcher for MockFetcher {
        fn fetch(&self, url: &str) -> Result<Page, String> {
            self.pages
                .get(url)
                .map(|body| Page {
                    url: url.to_string(),
                    body: body.clone(),
                })
                .ok_or_else(|| format!("mock miss: {url}"))
        }
    }

    #[test]
    fn extract_links_basic() {
        let html = r#"<a href="https://a.com/1">x</a> text <a href="https://b.org/2">y</a>"#;
        let links = extract_links(html);
        assert_eq!(links, vec!["https://a.com/1", "https://b.org/2"]);
    }

    #[test]
    fn extract_links_skips_relative() {
        let html = r#"<a href="/relative">r</a><a href="https://c.io/x">c</a>"#;
        let links = extract_links(html);
        assert_eq!(links, vec!["https://c.io/x"]);
    }

    #[test]
    fn domain_of_strips_scheme_and_port() {
        assert_eq!(domain_of("https://a.com/x"), "a.com");
        assert_eq!(domain_of("http://b.org:8080/y"), "b.org");
    }

    #[test]
    fn crawler_walks_a_tiny_graph() {
        // 一个 3 节点的微型图：seed -> a -> b -> c
        let pages: &[(&str, &str)] = &[
            (
                "https://seed.test/",
                r#"<a href="https://a.test/1">a</a><a href="https://a.test/2">a2</a>"#,
            ),
            ("https://a.test/1", r#"<a href="https://b.test/x">b</a>"#),
            ("https://a.test/2", "no links"),
            ("https://b.test/x", r#"<a href="https://c.test/y">c</a>"#),
            ("https://c.test/y", "leaf"),
        ];
        let fetcher = MockFetcher::new(pages);
        let crawled = Crawler::new(fetcher)
            .max_pages(5)
            .workers(3)
            .per_domain(2)
            .run(vec!["https://seed.test/".to_string()]);

        // 应该抓到全部 5 个页面（不重复）
        let mut urls: Vec<_> = crawled.iter().map(|p| p.url.as_str()).collect();
        urls.sort();
        assert_eq!(
            urls,
            vec![
                "https://a.test/1",
                "https://a.test/2",
                "https://b.test/x",
                "https://c.test/y",
                "https://seed.test/",
            ]
        );
    }

    #[test]
    fn crawler_does_not_visit_twice() {
        // 把 a 和 b 互相指——历史版本里这种环会让爬虫无限抓。
        // 现在有去重，最多抓 max_pages 页就停。
        let pages: &[(&str, &str)] = &[
            ("https://a.test/", r#"<a href="https://b.test/">b</a>"#),
            ("https://b.test/", r#"<a href="https://a.test/">a</a>"#),
        ];
        let fetcher = MockFetcher::new(pages);
        let crawled = Crawler::new(fetcher)
            .max_pages(2)
            .workers(2)
            .run(vec!["https://a.test/".to_string()]);
        // 即使有环，也只能抓到 2 个唯一 URL
        let unique: HashSet<_> = crawled.iter().map(|p| p.url.clone()).collect();
        assert_eq!(unique.len(), 2);
    }

    #[test]
    fn crawler_respects_max_pages() {
        let pages: &[(&str, &str)] = &[
            ("https://seed.test/", r#"<a href="https://x.test/1">1</a>"#),
            ("https://x.test/1", r#"<a href="https://x.test/2">2</a>"#),
            ("https://x.test/2", r#"<a href="https://x.test/3">3</a>"#),
            ("https://x.test/3", "leaf"),
        ];
        let fetcher = MockFetcher::new(pages);
        let crawled = Crawler::new(fetcher)
            .max_pages(2)
            .workers(2)
            .run(vec!["https://seed.test/".to_string()]);
        assert_eq!(crawled.len(), 2);
    }
}
