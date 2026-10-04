//! Optional proxy pool fed by a public list (e.g. monosans/proxy-list).
//!
//! Free lists are mostly dead, so the pool is *verified* in parallel at load
//! time and only working entries are kept. On a transport failure or a
//! 403/429/challenge the monitor drops the current proxy and switches to the
//! next one; the global rate-limit backoff still applies, so switching never
//! raises the request rate.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{info, warn};
use wreq_util::Emulation;

use crate::config::Config;

/// Neutral endpoint used to check that a proxy actually works (2xx = alive).
const PROBE_URL: &str = "https://api.ipify.org/";
/// How many random entries from a list to probe per refresh.
const MAX_CANDIDATES: usize = 200;
/// Proxies probed at the same time.
const VERIFY_CONCURRENCY: usize = 64;
/// Per-proxy timeout while probing.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(8);
const SITE_TIMEOUT: Duration = Duration::from_secs(25);

pub fn build_client(proxy: Option<&str>) -> Result<wreq::Client> {
    build_client_with_timeout(proxy, SITE_TIMEOUT)
}

fn build_client_with_timeout(proxy: Option<&str>, timeout: Duration) -> Result<wreq::Client> {
    let mut builder = wreq::Client::builder()
        .emulation(Emulation::Chrome136)
        .cookie_store(true)
        .timeout(timeout)
        .redirect(wreq::redirect::Policy::limited(5));
    if let Some(url) = proxy {
        builder = builder
            .proxy(wreq::Proxy::all(url).with_context(|| format!("bad proxy url: {}", short(url)))?);
    }
    builder.build().context("cannot build HTTP client")
}

/// `http://user:pass@host:port` -> `http://host:port` (safe for logs).
pub fn short(proxy: &str) -> String {
    match proxy.split_once("://") {
        Some((scheme, rest)) => {
            let host = rest.rsplit('@').next().unwrap_or(rest).trim_end_matches('/');
            format!("{scheme}://{host}")
        }
        None => proxy.to_string(),
    }
}

/// Lines must carry an explicit scheme (`all.txt` style). Comments, blanks and
/// bare `host:port` lines (http.txt / socks5.txt style) are ignored — pick the
/// right file or prefix the scheme yourself.
pub fn parse_list(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || !line.contains("://") {
            continue;
        }
        if seen.insert(line.to_string()) {
            out.push(line.to_string());
        }
    }
    out
}

/// Read or download the list, keep a random sample of `MAX_CANDIDATES`, verify
/// them and return the working ones.
pub async fn load(client: &wreq::Client, source: &str) -> Vec<String> {
    let text = if source.starts_with("http://") || source.starts_with("https://") {
        match client.get(source).send().await {
            Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
            Ok(r) => {
                warn!("proxy list {source} -> HTTP {}", r.status());
                return Vec::new();
            }
            Err(e) => {
                warn!("cannot fetch proxy list {source}: {e}");
                return Vec::new();
            }
        }
    } else {
        match std::fs::read_to_string(source) {
            Ok(t) => t,
            Err(e) => {
                warn!("cannot read proxy list {source}: {e}");
                return Vec::new();
            }
        }
    };

    let mut list = parse_list(&text);
    if list.is_empty() {
        warn!("proxy list {source}: no usable entries (need scheme://host:port, e.g. socks5://1.2.3.4:1080)");
        return list;
    }
    fastrand::shuffle(&mut list);
    list.truncate(MAX_CANDIDATES);
    let candidates = list.len();
    let good = verify(list).await;
    info!("proxy pool: {}/{} candidates passed the check", good.len(), candidates);
    good
}

async fn verify(proxies: Vec<String>) -> Vec<String> {
    let mut good = Vec::new();
    for chunk in proxies.chunks(VERIFY_CONCURRENCY) {
        let mut set = tokio::task::JoinSet::new();
        for p in chunk {
            set.spawn(probe(p.clone()));
        }
        while let Some(res) = set.join_next().await {
            if let Ok(Some(p)) = res {
                good.push(p);
            }
        }
    }
    good
}

async fn probe(proxy: String) -> Option<String> {
    let client = build_client_with_timeout(Some(&proxy), VERIFY_TIMEOUT).ok()?;
    match client.get(PROBE_URL).send().await {
        Ok(r) if r.status().is_success() => Some(proxy),
        _ => None,
    }
}

/// List state + rotation. The `Site` owns the actual HTTP client, this only
/// decides *which* proxy should be used next.
#[derive(Debug)]
pub struct Proxies {
    source: Option<String>,
    refresh: Duration,
    list: Vec<String>,
    active: Option<String>,
    loaded_at: Option<Instant>,
}

impl Proxies {
    /// A fixed `PROXY=` disables the pool entirely.
    pub fn new(cfg: &Config) -> Self {
        Proxies {
            source: if cfg.proxy.is_some() { None } else { cfg.proxy_list.clone() },
            refresh: cfg.proxy_refresh,
            list: Vec::new(),
            active: cfg.proxy.clone(),
            loaded_at: None,
        }
    }

    pub fn enabled(&self) -> bool {
        self.source.is_some()
    }

    pub fn source(&self) -> Option<String> {
        self.source.clone()
    }

    pub fn active(&self) -> Option<String> {
        self.active.clone()
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    /// True when the pool is empty or the refresh interval has elapsed.
    pub fn refresh_due(&self) -> bool {
        if self.source.is_none() {
            return false;
        }
        match self.loaded_at {
            None => true,
            Some(t) => !self.refresh.is_zero() && t.elapsed() >= self.refresh,
        }
    }

    /// Replace the pool with a freshly verified list; returns the new active proxy.
    pub fn install(&mut self, list: Vec<String>) -> Option<String> {
        self.loaded_at = Some(Instant::now());
        self.list = list;
        self.active = self.list.first().cloned();
        self.active.clone()
    }

    /// Drop the proxy that just failed and return the next one (or `None` when
    /// the pool is exhausted — then `refresh_due` becomes true again).
    pub fn rotate(&mut self) -> Option<String> {
        if let Some(active) = self.active.take() {
            self.list.retain(|p| p != &active);
        }
        if self.list.is_empty() {
            self.loaded_at = None; // force a re-check on the next cycle
            return None;
        }
        self.active = Some(self.list[0].clone());
        self.active.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_scheme_prefixed_lines() {
        let text = "# comment\n\nsocks5://1.2.3.4:1080\n1.2.3.4:8080\nhttp://5.6.7.8:3128\nsocks5://1.2.3.4:1080\n";
        assert_eq!(parse_list(text), vec!["socks5://1.2.3.4:1080", "http://5.6.7.8:3128"]);
    }

    #[test]
    fn masks_credentials_in_logs() {
        assert_eq!(short("http://user:pass@1.2.3.4:8080"), "http://1.2.3.4:8080");
        assert_eq!(short("socks5://1.2.3.4:1080"), "socks5://1.2.3.4:1080");
    }
}
