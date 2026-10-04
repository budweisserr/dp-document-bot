use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::proxy::{self, Proxies};

const UA_LANG: &str = "uk-UA,uk;q=0.9,en;q=0.8";

/// What a page (or an API reply) tells us.
#[derive(Debug, Clone, PartialEq)]
pub enum PageKind {
    /// "Наразі всі місця зайняті" — the form is not rendered.
    Busy,
    /// The booking form is on the page; we can call the JSON API.
    Open { center: Option<u32>, token: Option<String> },
    /// 429 / Cloudflare challenge — stop hammering.
    Blocked,
    Unknown,
}

/// Result of a POST to the site's `form=...` API.
#[derive(Debug, Clone)]
pub enum Api<T> {
    Value(T),
    /// Non-JSON reply: the request needs a fresh page token / cookies.
    NeedsSession,
    Blocked,
}

impl<T> Api<T> {
    /// Drop the payload type when all we need is the state.
    fn widen<U>(self) -> Api<U> {
        match self {
            Api::Value(_) | Api::NeedsSession => Api::NeedsSession,
            Api::Blocked => Api::Blocked,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Day {
    pub date: String,
    pub allowed: u32,
}

impl Day {
    pub fn short(&self) -> String {
        let d = self.date.split('T').next().unwrap_or(&self.date);
        format!("{d} · {} місць", self.allowed)
    }
}

pub struct Site {
    /// Swapped when the proxy changes, hence the lock.
    client: Mutex<wreq::Client>,
    /// Plain client used only to download the proxy list.
    list_client: wreq::Client,
    proxies: Mutex<Proxies>,
    gap: Duration,
    blocked_base: u64,
    blocked_max: u64,
    challenge_secs: u64,
    last_request: Mutex<Instant>,
    blocked_until: Mutex<Option<Instant>>,
    block_streak: Mutex<u32>,
}

impl Site {
    pub async fn new(cfg: &Config) -> Result<Self> {
        if cfg.proxy.is_some() && cfg.proxy_list.is_some() {
            info!("PROXY is set — PROXY_LIST is ignored");
        }
        let mut proxies = Proxies::new(cfg);
        let list_client = wreq::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(wreq::redirect::Policy::limited(5))
            .build()
            .context("cannot build list client")?;
        if proxies.enabled() {
            if let Some(source) = proxies.source() {
                info!("loading proxy list {source} (verifying candidates, this can take ~30s)");
                let list = proxy::load(&list_client, &source).await;
                proxies.install(list);
            }
        }
        let client = proxy::build_client(proxies.active().as_deref())?;
        match proxies.active() {
            Some(p) if proxies.enabled() => info!("using proxy {}", proxy::short(&p)),
            _ => debug!("no proxy configured"),
        }
        Ok(Site {
            client: Mutex::new(client),
            list_client,
            proxies: Mutex::new(proxies),
            gap: cfg.request_gap,
            blocked_base: cfg.blocked_secs,
            blocked_max: cfg.blocked_max_secs,
            challenge_secs: cfg.challenge_secs,
            last_request: Mutex::new(Instant::now() - cfg.request_gap),
            blocked_until: Mutex::new(None),
            block_streak: Mutex::new(0),
        })
    }

    fn client(&self) -> wreq::Client {
        self.client.lock().unwrap().clone()
    }

    /// Drop the current proxy (it just failed) and rebuild the client with the
    /// next one. A fixed `PROXY=` never rotates.
    fn switch_proxy(&self, why: &str) {
        let next = {
            let mut p = self.proxies.lock().unwrap();
            if !p.enabled() {
                return;
            }
            p.rotate()
        };
        match proxy::build_client(next.as_deref()) {
            Ok(client) => {
                *self.client.lock().unwrap() = client;
                match &next {
                    Some(p) => info!("proxy switched ({why}) → {}", proxy::short(p)),
                    None => warn!("proxy pool exhausted ({why}) — direct until the next list refresh"),
                }
            }
            Err(e) => warn!("cannot switch proxy ({why}): {e}"),
        }
    }

    /// Re-download and re-verify the proxy list when the refresh interval is up
    /// or the pool ran dry. Called from the monitor loop, cheap when not due.
    pub async fn maybe_refresh(&self) {
        let (due, source) = {
            let p = self.proxies.lock().unwrap();
            (p.refresh_due(), p.source())
        };
        if !due {
            return;
        }
        let Some(source) = source else { return };
        info!("refreshing proxy list {source} …");
        let list = proxy::load(&self.list_client, &source).await;
        let active = {
            let mut p = self.proxies.lock().unwrap();
            p.install(list)
        };
        match proxy::build_client(active.as_deref()) {
            Ok(client) => {
                *self.client.lock().unwrap() = client;
                match &active {
                    Some(p) => info!("proxy pool ready ({} verified), active {}", self.proxies.lock().unwrap().len(), proxy::short(p)),
                    None => warn!("proxy list gave no working proxies — going direct"),
                }
            }
            Err(e) => warn!("cannot build client for proxy list: {e}"),
        }
    }

    /// Time left before we are allowed to touch the site again.
    pub fn backoff_left(&self) -> Option<Duration> {
        let until = (*self.blocked_until.lock().unwrap())?;
        let now = Instant::now();
        (until > now).then(|| until - now)
    }

    fn record_blocked(&self) {
        let mut streak = self.block_streak.lock().unwrap();
        *streak = (*streak + 1).min(4);
        let secs = self.blocked_base.saturating_mul(1 << (*streak - 1)).min(self.blocked_max);
        let until = Instant::now() + Duration::from_secs(secs);
        let mut cur = self.blocked_until.lock().unwrap();
        if cur.is_none_or(|c| c < until) {
            *cur = Some(until);
        }
        drop(cur);
        warn!("site rate-limited (streak {streak}); backing off {secs}s");
        self.switch_proxy("rate-limited");
    }

    /// A Cloudflare challenge (not a rate limit): rotate the proxy and pause
    /// only briefly, otherwise one flagged proxy would cost a 10-minute blackout.
    fn record_challenge(&self) {
        self.pause(self.challenge_secs);
        warn!("Cloudflare challenge (403); pausing {}s", self.challenge_secs);
        self.switch_proxy("challenge");
    }

    fn pause(&self, secs: u64) {
        let until = Instant::now() + Duration::from_secs(secs);
        let mut cur = self.blocked_until.lock().unwrap();
        if cur.is_none_or(|c| c < until) {
            *cur = Some(until);
        }
    }

    fn record_ok(&self) {
        *self.block_streak.lock().unwrap() = 0;
        *self.blocked_until.lock().unwrap() = None;
    }

    /// Serialise every request through a minimum gap so we never burst.
    async fn gate(&self) {
        let wait = {
            let last = self.last_request.lock().unwrap();
            self.gap.saturating_sub(last.elapsed())
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        *self.last_request.lock().unwrap() = Instant::now();
    }

    pub async fn get_page(&self, url: &str) -> Result<PageKind> {
        let req = self
            .client()
            .get(url)
            .header("accept-language", UA_LANG)
            .header("referer", "https://pasport.org.ua/");
        self.gate().await;
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                self.switch_proxy("transport error");
                return Err(e).with_context(|| format!("GET {url}"));
            }
        };
        let status = resp.status();
        let challenged = resp.headers().contains_key("cf-mitigated");
        let body = resp.text().await.unwrap_or_default();
        debug!("GET {url} -> {status} ({} bytes)", body.len());

        let low = body.to_ascii_lowercase();
        let rate_limited = status.as_u16() == 429
            || (!status.is_success() && low.contains("too many requests"));
        let is_challenge = challenged
            || status.as_u16() == 403
            || (!status.is_success() && low.contains("just a moment"));
        if rate_limited {
            self.record_blocked();
            return Ok(PageKind::Blocked);
        }
        if is_challenge {
            self.record_challenge();
            return Ok(PageKind::Blocked);
        }
        if !status.is_success() {
            return Ok(PageKind::Unknown);
        }
        let kind = parse_page(&body);
        if kind == PageKind::Blocked {
            self.record_blocked();
        } else {
            self.record_ok();
        }
        if kind == PageKind::Unknown {
            debug!("GET {url} unrecognised body: {}", body.chars().take(200).collect::<String>());
        }
        Ok(kind)
    }

    pub async fn post_form(&self, url: &str, fields: &[(&str, &str)]) -> Result<Api<String>> {
        // The site rejects "AJAX" POSTs that do not look like a browser XHR
        // (`Помилка доступу!`), so fetch-metadata must match a same-origin fetch.
        let req = self
            .client()
            .post(url)
            .header("accept", "application/json, text/plain, */*")
            .header("accept-language", UA_LANG)
            .header("referer", url)
            .header("origin", origin_of(url))
            .header("sec-fetch-dest", "empty")
            .header("sec-fetch-mode", "cors")
            .header("sec-fetch-site", "same-origin")
            .form(fields);
        self.gate().await;
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                self.switch_proxy("transport error");
                return Err(e).with_context(|| format!("POST {url}"));
            }
        };
        let status = resp.status();
        let is_json =
            resp.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("json"));
        let body = resp.text().await.unwrap_or_default();
        debug!("POST {url} -> {status} json={is_json} ({} bytes)", body.len());

        let low = body.to_ascii_lowercase();
        if status.as_u16() == 429 || low.contains("too many requests") {
            self.record_blocked();
            return Ok(Api::Blocked);
        }
        if status.as_u16() == 403 || low.contains("just a moment") {
            self.record_challenge();
            return Ok(Api::Blocked);
        }
        if is_json {
            self.record_ok();
            debug!("POST {url} json: {}", body.chars().take(300).collect::<String>());
            return Ok(Api::Value(body));
        }
        debug!("POST {url} non-JSON: {}", body.chars().take(200).collect::<String>());
        if !status.is_success() {
            self.record_ok();
            return Ok(Api::NeedsSession);
        }
        Ok(Api::NeedsSession)
    }

    pub async fn days(
        &self,
        url: &str,
        center: u32,
        service: &str,
        token: Option<&str>,
    ) -> Result<Api<Vec<Day>>> {
        let center_s = center.to_string();
        let mut fields: Vec<(&str, &str)> =
            vec![("form", "days"), ("ServiceCenterId", &center_s), ("ServiceId", service)];
        if let Some(t) = token {
            fields.push((t, "1"));
        }
        match self.post_form(url, &fields).await? {
            Api::Value(body) => Ok(parse_days(&body).map(Api::Value).unwrap_or(Api::NeedsSession)),
            other => Ok(other.widen()),
        }
    }

}

/// `form=days` normally returns `{"days":[...]}`, but `false`/`null` also
/// occurs (meaning "no days"), so parse defensively.
fn parse_days(body: &str) -> Option<Vec<Day>> {
    let raw: serde_json::Value = serde_json::from_str(body).ok()?;
    Some(match raw.get("days") {
        Some(serde_json::Value::Array(items)) => to_days(
            items
                .iter()
                .filter_map(|v| serde_json::from_value::<DayRaw>(v.clone()).ok())
                .collect(),
        ),
        _ => Vec::new(),
    })
}

fn to_days(raw: Vec<DayRaw>) -> Vec<Day> {
    let mut days: Vec<Day> = raw
        .into_iter()
        .filter(|d| d.is_allowed && d.allowed > 0)
        .map(|d| Day { date: d.date_part.or(d.date).unwrap_or_default(), allowed: d.allowed })
        .filter(|d| !d.date.is_empty())
        .collect();
    days.sort_by(|a, b| a.date.cmp(&b.date));
    days
}

pub fn origin_of(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let host = rest.split('/').next().unwrap_or(rest);
            format!("{scheme}://{host}")
        }
        None => url.to_string(),
    }
}

/// Classify a page without a full HTML parser: the markers we need are stable
/// strings in the SSR markup.
pub fn parse_page(html: &str) -> PageKind {
    let low = html.to_ascii_lowercase();
    // `challenge-platform` alone is NOT a challenge: Cloudflare injects its
    // invisible bot-management script into perfectly normal pages.
    if low.contains("<title>just a moment")
        || low.contains("id=\"challenge-running\"")
        || low.contains("__cf_chl_")
    {
        return PageKind::Blocked;
    }
    if low.contains("qlogickform") || low.contains("queueform") {
        let (center, token) = parse_config(html);
        info!("page has an open booking form (center={center:?}, token={})", token.is_some());
        if center.is_none() || token.is_none() {
            let at = low.find("qlogickform").or_else(|| low.find("queueform")).unwrap_or(0);
            let from = at.saturating_sub(150);
            let to = (at + 350).min(html.len());
            debug!("config context: {}", html[from..to].replace('\n', " "));
        }
        return PageKind::Open { center, token };
    }
    if low.contains("всі місця зайняті") || low.contains("місця зайнято") {
        return PageKind::Busy;
    }
    PageKind::Unknown
}

/// The Alpine config is rendered either as `center: '10', token: 'eyJ...'`
/// (older builds, single quotes) or as HTML-escaped JSON
/// `&quot;center&quot;:&quot;10&quot;,&quot;csrf&quot;:&quot;eed3...&quot;`
/// (current builds, `qlogickFormHaku`). Both are supported.
fn parse_config(html: &str) -> (Option<u32>, Option<String>) {
    let decoded = html.replace("&quot;", "\"").replace("&#39;", "'");
    let center = value_after(&decoded, "\"center\"").or_else(|| value_after(&decoded, "center"));
    let token = value_after(&decoded, "\"csrf\"")
        .or_else(|| value_after(&decoded, "\"token\""))
        .or_else(|| value_after(&decoded, "csrf"))
        .or_else(|| value_after(&decoded, "token"));
    (center.and_then(|v| v.parse().ok()), token)
}

/// `key` is a literal such as `"csrf"` or `center`; returns the quoted value
/// that follows the next `key :` pair.
fn value_after(h: &str, key: &str) -> Option<String> {
    let mut from = 0;
    while let Some(rel) = h[from..].find(key) {
        let at = from + rel + key.len();
        from = at;
        let Some(rest) = h[at..].trim_start().strip_prefix(':') else {
            continue;
        };
        let rest = rest.trim_start();
        for quote in ['"', '\''] {
            if let Some(r) = rest.strip_prefix(quote) {
                if let Some(end) = r.find(quote) {
                    let value = &r[..end];
                    if !value.is_empty() {
                        return Some(value.to_string());
                    }
                }
            }
        }
    }
    None
}

#[derive(Deserialize)]
struct DayRaw {
    #[serde(default)]
    date: Option<String>,
    #[serde(rename = "datePart", default)]
    date_part: Option<String>,
    #[serde(rename = "allowedJobCount", default)]
    allowed: u32,
    #[serde(rename = "isAllowed", default)]
    is_allowed: bool,
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_real_page_markers() {
        let busy = "<html><body>Наразі всі місця зайняті. Будь ласка, спробуйте в інший час.</body></html>";
        assert_eq!(parse_page(busy), PageKind::Busy);

        let open = r#"<div id="queue_form" x-data="queueForm()"></div>
            <form x-data="qlogickForm({ url: 'https://warszawa.pasport.org.ua/solutions/e-queue',
            hcaptcha: 'x', center: '10', token: 'eyJhbGciOi.abc-123_xyz', messageErrorPhone: 'x' })">"#;
        assert_eq!(
            parse_page(open),
            PageKind::Open { center: Some(10), token: Some("eyJhbGciOi.abc-123_xyz".into()) }
        );

        let haku = r#"<form x-data="qlogickFormHaku({&quot;url&quot;:&quot;https:\/\/warszawa.pasport.org.ua\/solutions\/e-queue&quot;,&quot;csrf&quot;:&quot;eed3a9e4dfadd8010cdf894845b14cce&quot;,&quot;center&quot;:&quot;10&quot;,&quot;messageErrorPhone&quot;:&quot;x&quot;,&quot;diia&quot;:&quot;1&quot;})">"#;
        assert_eq!(
            parse_page(haku),
            PageKind::Open { center: Some(10), token: Some("eed3a9e4dfadd8010cdf894845b14cce".into()) }
        );

        assert_eq!(parse_page("<title>Just a moment...</title>"), PageKind::Blocked);
        assert_eq!(parse_page("<html>nonsense</html>"), PageKind::Unknown);
    }

    #[test]
    fn parses_days_payload() {
        let body = r#"{"days":[
            {"date":"2026-10-16T00:00:00","datePart":"2026-10-16","allowedJobCount":2,"isAllowed":true},
            {"date":"2026-10-15T00:00:00","datePart":"2026-10-15","allowedJobCount":0,"isAllowed":false},
            {"date":"2026-10-15T00:00:00","datePart":"2026-10-15","allowedJobCount":5,"isAllowed":true}
        ]}"#;
        let days = parse_days(body).unwrap();
        assert_eq!(days, vec![Day { date: "2026-10-15".into(), allowed: 5 }, Day { date: "2026-10-16".into(), allowed: 2 }]);
    }

    #[test]
    fn extracts_origin() {
        assert_eq!(origin_of("https://warszawa.pasport.org.ua/solutions/e-queue"), "https://warszawa.pasport.org.ua");
    }
}
