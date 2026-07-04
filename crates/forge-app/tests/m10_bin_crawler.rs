//! M10 bin 集成测试：用 MockFetcher 跑 crawler 的 run_with_fetcher 路径。
//!
//! 不联网——和"读者用真实 bin 抓 example.com"在并发决策上完全等价：
//! bin 的 main 只是把 ureq 换成 MockFetcher 之外的另一实现。

use forge_app::crawler::{run_with_fetcher, CliArgs, Fetcher, Page};
use std::collections::HashMap;

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
fn bin_crawler_walks_mock_graph() {
    // 一个 3 节点微型图：seed -> a -> b -> c
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
    let args = CliArgs {
        seed: "https://seed.test/".into(),
        max: 5,
        workers: 3,
        per_domain: 2,
    };
    let crawled = run_with_fetcher(fetcher, args);

    let mut urls: Vec<_> = crawled.iter().map(|p| p.url.clone()).collect();
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
fn bin_crawler_respects_max_pages() {
    let pages: &[(&str, &str)] = &[
        ("https://seed.test/", r#"<a href="https://x.test/1">1</a>"#),
        ("https://x.test/1", r#"<a href="https://x.test/2">2</a>"#),
        ("https://x.test/2", r#"<a href="https://x.test/3">3</a>"#),
        ("https://x.test/3", "leaf"),
    ];
    let fetcher = MockFetcher::new(pages);
    let args = CliArgs {
        seed: "https://seed.test/".into(),
        max: 2,
        workers: 2,
        per_domain: 2,
    };
    let crawled = run_with_fetcher(fetcher, args);
    assert_eq!(crawled.len(), 2);
}

#[test]
fn bin_crawler_dedups_in_a_cycle() {
    // a <-> b 互相指——去重保证不会无限抓。
    let pages: &[(&str, &str)] = &[
        ("https://a.test/", r#"<a href="https://b.test/">b</a>"#),
        ("https://b.test/", r#"<a href="https://a.test/">a</a>"#),
    ];
    let fetcher = MockFetcher::new(pages);
    let args = CliArgs {
        seed: "https://a.test/".into(),
        max: 10,
        workers: 2,
        per_domain: 2,
    };
    let crawled = run_with_fetcher(fetcher, args);
    let unique: std::collections::HashSet<_> = crawled.iter().map(|p| p.url.clone()).collect();
    assert_eq!(unique.len(), 2);
}
