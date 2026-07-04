//! M10 爬虫集成测试：用 mock fetcher 走完整流程，不联网。

use forge_app::crawler::{Crawler, Fetcher, Page};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex as StdMutex;

/// 一个能数"每个域名被同时抓过几次"的 fetcher。
/// 我们用它来**证明** per-domain Semaphore 真的把并发限住了。
struct ConcurrencyCountingFetcher {
    pages: HashMap<String, String>,
    /// 当前每个域名在途的请求数。
    in_flight: StdMutex<HashMap<String, usize>>,
    /// 历史上每个域名达到过的最大在途数。
    peak: StdMutex<HashMap<String, usize>>,
    fetch_calls: AtomicUsize,
}

impl ConcurrencyCountingFetcher {
    fn new(pages: &[(&str, &str)]) -> Self {
        Self {
            pages: pages.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            in_flight: StdMutex::new(HashMap::new()),
            peak: StdMutex::new(HashMap::new()),
            fetch_calls: AtomicUsize::new(0),
        }
    }
}

impl Fetcher for ConcurrencyCountingFetcher {
    fn fetch(&self, url: &str) -> Result<Page, String> {
        let domain = forge_app::crawler::domain_of(url);
        // 在途 +1，记录峰值
        {
            let mut m = self.in_flight.lock().unwrap();
            let n = m.entry(domain.clone()).or_insert(0);
            *n += 1;
            let mut peak = self.peak.lock().unwrap();
            let p = peak.entry(domain.clone()).or_insert(0);
            if *n > *p {
                *p = *n;
            }
        }
        self.fetch_calls.fetch_add(1, Ordering::SeqCst);

        // 模拟一点延迟，让并发更可能真的发生
        std::thread::sleep(std::time::Duration::from_millis(20));

        {
            let mut m = self.in_flight.lock().unwrap();
            *m.get_mut(&domain).unwrap() -= 1;
        }

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
fn per_domain_limiter_caps_concurrency() {
    use std::sync::Arc;
    let mut owned: Vec<(String, String)> = Vec::new();
    for i in 0..6 {
        owned.push((format!("https://a.test/{i}"), String::new()));
    }
    let pages_ref: Vec<(&str, &str)> = owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let fetcher = Arc::new(ConcurrencyCountingFetcher::new(&pages_ref));
    // 我们需要一个可以"被 move 又能继续观察内部状态"的方案：
    // 包装一层 Fetcher，把内部 Arc 留一份在测试里。
    struct SharedFetcher(Arc<ConcurrencyCountingFetcher>);
    impl Fetcher for SharedFetcher {
        fn fetch(&self, url: &str) -> Result<Page, String> {
            self.0.fetch(url)
        }
    }

    let observer = fetcher.clone();
    let seeds: Vec<String> = (0..6).map(|i| format!("https://a.test/{i}")).collect();
    let _crawled = Crawler::new(SharedFetcher(fetcher))
        .per_domain(2)
        .workers(8)
        .max_pages(6)
        .run(seeds);

    let peak = observer.peak.lock().unwrap();
    let a_peak = peak.get("a.test").copied().unwrap_or(0);
    // 允许 2，绝不允许 3
    assert!(
        a_peak <= 2,
        "per-domain 并发被破坏了：peak = {a_peak}"
    );
    assert!(a_peak >= 1);
}

#[test]
fn crawler_visits_all_unique_urls() {
    let owned: Vec<(String, String)> = vec![
        ("https://seed.test/".into(), "<a href=\"https://a.test/1\">1</a><a href=\"https://b.test/2\">2</a>".into()),
        ("https://a.test/1".into(), "leaf-a".into()),
        ("https://b.test/2".into(), "leaf-b".into()),
    ];
    let pages_ref: Vec<(&str, &str)> = owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let fetcher = MockFetcherStatic::new(pages_ref);

    let crawled = Crawler::new(fetcher)
        .max_pages(3)
        .workers(2)
        .run(vec!["https://seed.test/".into()]);
    assert_eq!(crawled.len(), 3);
}

// 简单 mock（无并发计数）
struct MockFetcherStatic {
    pages: HashMap<String, String>,
}
impl MockFetcherStatic {
    fn new(pages: Vec<(&str, &str)>) -> Self {
        Self {
            pages: pages.into_iter().map(|(k, v)| (k.into(), v.into())).collect(),
        }
    }
}
impl Fetcher for MockFetcherStatic {
    fn fetch(&self, url: &str) -> Result<Page, String> {
        self.pages
            .get(url)
            .map(|b| Page { url: url.into(), body: b.clone() })
            .ok_or_else(|| format!("miss {url}"))
    }
}
