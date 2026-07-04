//! # 并发网页爬虫入口（M10 真实 bin）
//!
//! 命令行：
//!
//! ```text
//! cargo run -p forge-app --bin crawler -- https://example.com --max 10 --concurrency 4
//! ```
//!
//! 把抓取能力抽成 `trait Fetcher`（在 lib 里），bin 里写两个实现：
//! - `UreqFetcher`（feature = "real-fetch"）：基于 ureq 的真 HTTP fetch。
//! - 默认（不开 real-fetch）：编译时给一个占位，提示用 `--features real-fetch`。
//!
//! main 把"跑爬虫"逻辑委托给 `forge_app::crawler::run_with_fetcher`——
//! 那是 lib 的可注入入口，集成测试用它注入 MockFetcher 跑完整路径，
//! 不用真的联网。

use forge_app::crawler::{CliArgs, Fetcher, Page};
#[cfg(feature = "real-fetch")]
use forge_app::crawler::run_with_fetcher;
use std::collections::HashMap;
#[cfg(feature = "real-fetch")]
use std::time::Duration;

fn main() {
    let args = parse_args(std::env::args().skip(1));

    #[cfg(feature = "real-fetch")]
    {
        let fetcher = UreqFetcher;
        let crawled = run_with_fetcher(fetcher, args.clone());
        eprintln!("crawler: 完成，共 {} 页", crawled.len());
        for p in &crawled {
            eprintln!(
                "  抓到 {} ({} 字节, 解析出新链接 {})",
                p.url,
                p.body.len(),
                p.new_links
            );
        }
    }

    #[cfg(not(feature = "real-fetch"))]
    {
        let _ = args;
        eprintln!(
            "crawler: 默认构建不联网。\n\
             重新构建以启用真实的 HTTP：\n\
             cargo run -p forge-app --bin crawler --features real-fetch -- https://example.com"
        );
    }
}

// =========================================================================
//  命令行解析
// =========================================================================

fn parse_args<I: Iterator<Item = String>>(mut it: I) -> CliArgs {
    let mut seed: Option<String> = None;
    let mut max: usize = 50;
    let mut workers: usize = 8;
    let mut per_domain: usize = forge_app::crawler::DEFAULT_PER_DOMAIN_CONCURRENCY;

    while let Some(a) = it.next() {
        match a.as_str() {
            "--max" => {
                if let Some(v) = it.next() {
                    max = v.parse().unwrap_or_else(|_| {
                        eprintln!("bad --max: {v}");
                        std::process::exit(2);
                    });
                }
            }
            "--concurrency" | "-c" => {
                if let Some(v) = it.next() {
                    workers = v.parse().unwrap_or_else(|_| {
                        eprintln!("bad --concurrency: {v}");
                        std::process::exit(2);
                    });
                }
            }
            "--domain-limit" | "-d" => {
                if let Some(v) = it.next() {
                    per_domain = v.parse().unwrap_or_else(|_| {
                        eprintln!("bad --domain-limit: {v}");
                        std::process::exit(2);
                    });
                }
            }
            "--help" | "-h" => {
                eprintln!(
                    "用法: crawler <seed-url> [--max N] [--concurrency C] [--domain-limit D]\n\
                     默认 max=50, concurrency=8, domain-limit=2"
                );
                std::process::exit(0);
            }
            s if s.starts_with("--") => {
                eprintln!("未知参数: {s}");
                std::process::exit(2);
            }
            _ => {
                if seed.is_none() {
                    seed = Some(a.to_string());
                } else {
                    eprintln!("多余的位置参数: {a}");
                    std::process::exit(2);
                }
            }
        }
    }

    let seed = match seed {
        Some(s) if s.starts_with("http://") || s.starts_with("https://") => s,
        Some(s) => format!("https://{s}"),
        None => {
            eprintln!("usage: crawler <seed-url> [--max N] [--concurrency C] [--domain-limit D]");
            std::process::exit(2);
        }
    };

    CliArgs {
        seed,
        max,
        workers,
        per_domain,
    }
}

// =========================================================================
//  真实 HTTP fetcher（feature = "real-fetch"）
// =========================================================================

#[cfg(feature = "real-fetch")]
#[derive(Default)]
pub struct UreqFetcher;

#[cfg(feature = "real-fetch")]
impl Fetcher for UreqFetcher {
    fn fetch(&self, url: &str) -> Result<Page, String> {
        // ureq::Agent 是同步 HTTP/1.1 客户端。这里每次新建 agent 简化教学——
        // 生产里把 agent 挪到 Crawler 之上以复用连接池（keep-alive）。
        let agent = ureq::AgentBuilder::new()
            .timeout_read(Duration::from_secs(15))
            .timeout_write(Duration::from_secs(5))
            .build();
        let resp = agent
            .get(url)
            .call()
            .map_err(|e| format!("fetch {url}: {e}"))?;
        let mut reader = resp.into_reader();
        let mut body = String::new();
        std::io::Read::read_to_string(&mut reader, &mut body)
            .map_err(|e| format!("read body {url}: {e}"))?;
        Ok(Page {
            url: url.to_string(),
            body,
        })
    }
}

// =========================================================================
//  测试用 MockFetcher（保持 bin 自包含可读）——集成测试在 tests/ 里另写。
// =========================================================================

#[allow(dead_code)]
struct MockFetcher {
    pages: HashMap<String, String>,
}

#[allow(dead_code)]
impl MockFetcher {
    fn new(pages: &[(&str, &str)]) -> Self {
        Self {
            pages: pages.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
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
