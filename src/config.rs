use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Result};

/// A passport centre (subdomain of pasport.org.ua).
///
/// `center` is the numeric `ServiceCenterId` used by the site's JSON API.
/// It is `None` for centres we have not learned the id of yet; the monitor
/// then reads it from the page HTML the first time it sees an open form.
#[derive(Clone, Copy, Debug)]
pub struct City {
    pub key: &'static str,
    pub name: &'static str,
    pub url: &'static str,
    pub center: Option<u32>,
}

pub const CITIES: &[City] = &[
    City { key: "warszawa", name: "🇵🇱 Варшава", url: "https://warszawa.pasport.org.ua/solutions/e-queue", center: Some(10) },
    City { key: "krakow", name: "🇵🇱 Краків", url: "https://krakow.pasport.org.ua/solutions/e-queue", center: None },
    City { key: "gdansk", name: "🇵🇱 Ґданськ", url: "https://gdansk.pasport.org.ua/solutions/e-queue", center: None },
    City { key: "wroclaw", name: "🇵🇱 Вроцлав", url: "https://wroclaw.pasport.org.ua/solutions/e-queue", center: None },
    City { key: "berlin", name: "🇩🇪 Берлін", url: "https://berlin.pasport.org.ua/solutions/e-queue", center: Some(2) },
    City { key: "cologne", name: "🇩🇪 Кельн", url: "https://cologne.pasport.org.ua/solutions/e-queue", center: None },
    City { key: "munich", name: "🇩🇪 Мюнхен", url: "https://munich.pasport.org.ua/solutions/e-queue", center: None },
    City { key: "prague", name: "🇨🇿 Прага", url: "https://prague.pasport.org.ua/solutions/e-queue", center: None },
    City { key: "bratislava", name: "🇸🇰 Братислава", url: "https://bratislava.pasport.org.ua/solutions/e-queue", center: Some(9) },
    City { key: "madrid", name: "🇪🇸 Мадрид", url: "https://madrid.pasport.org.ua/solutions/e-queue", center: Some(6) },
    City { key: "valencia", name: "🇪🇸 Валенсія", url: "https://valencia.pasport.org.ua/solutions/e-queue", center: Some(7) },
    City { key: "milan", name: "🇮🇹 Мілан", url: "https://milan.pasport.org.ua/solutions/e-queue", center: None },
];

pub fn city(key: &str) -> Option<City> {
    CITIES.iter().copied().find(|c| c.key == key)
}

#[derive(Clone, Debug)]
pub struct Config {
    pub bot_token: String,
    pub admin_id: Option<i64>,
    pub default_city: String,
    /// Which site service to watch ("4" = biometric passport abroad).
    pub service_id: String,
    /// Normal gap between full check cycles.
    pub poll_secs: u64,
    /// Random +/- added to every sleep.
    pub jitter_secs: u64,
    /// Minimum delay between any two HTTP requests to the site.
    pub request_gap: Duration,
    /// First backoff step after a 429 (doubles up to max).
    pub blocked_secs: u64,
    /// Short pause after a Cloudflare challenge (403) before trying the next proxy.
    pub challenge_secs: u64,
    pub blocked_max_secs: u64,
    /// Re-notify while slots stay open at most this often.
    pub alert_cooldown_secs: u64,
    /// Optional http/socks proxy for the site requests.
    pub proxy: Option<String>,
    /// Optional URL or file with a proxy list (`scheme://host:port` per line).
    pub proxy_list: Option<String>,
    /// How often to reload and re-verify the proxy list (zero = only once).
    pub proxy_refresh: Duration,
    /// Open the booking page in the default browser when slots appear.
    pub open_browser: bool,
    pub data_dir: PathBuf,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let cfg = Config {
            bot_token: env_str("BOT_TOKEN", ""),
            admin_id: env_str("ADMIN_ID", "").parse().ok(),
            default_city: env_str("DEFAULT_BRANCH", "warszawa").to_lowercase(),
            service_id: env_str("SERVICE_ID", "4"),
            poll_secs: env_u64("CHECK_INTERVAL", 300).max(60),
            jitter_secs: env_u64("INTERVAL_JITTER", 20),
            request_gap: Duration::from_millis(env_u64("REQUEST_GAP_MS", 4000).max(1000)),
            blocked_secs: env_u64("BLOCKED_SECS", 600).max(120),
            challenge_secs: env_u64("CHALLENGE_SECS", 30).max(5),
            blocked_max_secs: env_u64("BLOCKED_MAX_SECS", 3600),
            alert_cooldown_secs: env_u64("ALERT_COOLDOWN", 900),
            proxy: {
                let p = env_str("PROXY", "");
                (!p.is_empty()).then_some(p)
            },
            proxy_list: {
                let p = env_str("PROXY_LIST", "");
                (!p.is_empty()).then_some(p)
            },
            proxy_refresh: Duration::from_secs(env_u64("PROXY_REFRESH_HOURS", 6) * 3600),
            open_browser: env_bool("OPEN_BROWSER", false),
            data_dir: PathBuf::from(env_str("DATA_DIR", "data")),
        };
        if city(&cfg.default_city).is_none() {
            bail!("DEFAULT_BRANCH '{}' is not a known city", cfg.default_city);
        }
        Ok(cfg)
    }
}

fn env_str(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn env_u64(key: &str, default: u64) -> u64 {
    env_str(key, "").parse().unwrap_or(default)
}

fn env_bool(key: &str, default: bool) -> bool {
    let v = env_str(key, if default { "true" } else { "false" }).to_lowercase();
    matches!(v.as_str(), "1" | "true" | "yes" | "on")
}
