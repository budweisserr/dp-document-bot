use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::{error, info, warn};

use crate::config::{self, Config};
use crate::site::{Api, Day, PageKind, Site};
use crate::store::Store;
use crate::telegram::{self, Bot};

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Found,
    NoSlots,
    Blocked,
    Error,
}

/// Outcome of one check, also used by `/check` and `/status`.
#[derive(Debug, Clone)]
pub struct Report {
    pub key: String,
    pub name: String,
    pub kind: Kind,
    pub message: String,
    pub days: Vec<Day>,
}

impl Report {
    fn new(key: &str, kind: Kind, message: impl Into<String>) -> Self {
        Report {
            key: key.to_string(),
            name: config::city(key).map(|c| c.name).unwrap_or(key).to_string(),
            kind,
            message: message.into(),
            days: Vec::new(),
        }
    }

    pub fn to_html(&self, checked_at: &str) -> String {
        let mut s = format!("📍 <b>{}</b>\n{}\n⏱ {}", telegram::esc(&self.name), telegram::esc(&self.message), telegram::esc(checked_at));
        if !self.days.is_empty() {
            s.push('\n');
            for d in self.days.iter().take(10) {
                s.push_str(&format!("\n📅 {}", telegram::esc(&d.short())));
            }
        }
        s
    }
}

#[derive(Clone, Default)]
struct Session {
    center: Option<u32>,
    /// CSRF field name rendered into the page; valid while the Joomla session
    /// lives. If the server stops accepting it, we simply read the page again.
    token: Option<String>,
}

#[derive(Default)]
struct Inner {
    sessions: HashMap<String, Session>,
    last: HashMap<String, Report>,
    alert_at: HashMap<String, Instant>,
}

pub struct Monitor {
    cfg: Arc<Config>,
    site: Arc<Site>,
    store: Arc<Store>,
    bot: Option<Bot>,
    inner: Mutex<Inner>,
}

impl Monitor {
    pub fn new(cfg: Arc<Config>, site: Arc<Site>, store: Arc<Store>, bot: Option<Bot>) -> Self {
        Monitor { cfg, site, store, bot, inner: Mutex::new(Inner::default()) }
    }

    pub fn last_report(&self, key: &str) -> Option<Report> {
        self.inner.lock().unwrap().last.get(key).cloned()
    }

    /// Cities we should poll: every city an active subscriber picked,
    /// or the default city (so the admin gets alerts before anyone subscribes).
    fn active_cities(&self) -> Vec<String> {
        let mut keys: Vec<String> = config::CITIES
            .iter()
            .filter(|c| !self.store.active_for(c.key).is_empty())
            .map(|c| c.key.to_string())
            .collect();
        if keys.is_empty() {
            keys.push(self.cfg.default_city.clone());
        }
        keys
    }

    pub async fn run(self: Arc<Self>) {
        loop {
            self.site.maybe_refresh().await;
            let cities = self.active_cities();
            let per_city = (self.cfg.poll_secs / cities.len().max(1) as u64).max(45);
            for key in cities {
                if let Some(wait) = self.site.backoff_left() {
                    info!("rate-limited: sleeping {}s", wait.as_secs());
                    tokio::time::sleep(wait + Duration::from_secs(1)).await;
                }
                let report = self.check_city(&key).await;
                self.publish(report).await;
                let jitter = if self.cfg.jitter_secs > 0 {
                    fastrand::i64(-(self.cfg.jitter_secs as i64)..=(self.cfg.jitter_secs as i64))
                } else {
                    0
                };
                let secs = (per_city as i64 + jitter).max(30) as u64;
                tokio::time::sleep(Duration::from_secs(secs)).await;
            }
        }
    }

    /// One check for one city: use the token from the last page load while it is
    /// fresh (cheap JSON POSTs), otherwise re-read the page (which also refreshes
    /// the token and teaches us the numeric centre id).
    pub async fn check_city(&self, key: &str) -> Report {
        let Some(city) = config::city(key) else {
            return Report::new(key, Kind::Error, format!("Невідоме місто: {key}"));
        };
        let (sess, saved_center) = {
            let inner = self.inner.lock().unwrap();
            (inner.sessions.get(key).cloned().unwrap_or_default(), self.store.center(key))
        };
        let center = sess.center.or(city.center).or(saved_center);
        let service = self.cfg.service_id.as_str();

        if let (Some(center), Some(token)) = (center, sess.token.as_deref()) {
            match self.site.days(city.url, center, service, Some(token)).await {
                Ok(Api::Value(days)) if !days.is_empty() => return self.found(key, days),
                Ok(Api::Value(_)) => {
                    return Report::new(key, Kind::NoSlots, "Вільних місць немає (API)");
                }
                Ok(Api::Blocked) => return self.blocked_report(key),
                Ok(Api::NeedsSession) => {} // stale token — read the page again
                Err(e) => return Report::new(key, Kind::Error, format!("Помилка API: {e}")),
            }
        }

        match self.site.get_page(city.url).await {
            Ok(PageKind::Open { center, token }) => {
                self.save_session(key, center, token);
                let fresh = self.inner.lock().unwrap().sessions.get(key).and_then(|s| s.token.clone());
                if let (Some(center), Some(token)) = (center, fresh) {
                    match self.site.days(city.url, center, service, Some(&token)).await {
                        Ok(Api::Value(days)) if !days.is_empty() => return self.found(key, days),
                        Ok(Api::Value(_)) => {
                            return Report::new(key, Kind::NoSlots, "Форма відкрита, але вільних днів немає (API)");
                        }
                        Ok(Api::Blocked) => return self.blocked_report(key),
                        Err(e) => return Report::new(key, Kind::Error, format!("Помилка days: {e}")),
                        Ok(Api::NeedsSession) => {}
                    }
                }
                // Fallback: the form is open but the API did not answer. Tell the
                // user to look manually — a false alarm beats a missed slot.
                Report::new(key, Kind::Found, "Відкрито форму запису — перевірте сайт!")
            }
            Ok(PageKind::Busy) => Report::new(key, Kind::NoSlots, "Всі місця зайняті (сторінка)"),
            Ok(PageKind::Blocked) => self.blocked_report(key),
            Ok(PageKind::Unknown) => Report::new(key, Kind::Error, "Незрозуміла відповідь сайту"),
            Err(e) => Report::new(key, Kind::Error, format!("Помилка з'єднання: {e}")),
        }
    }

    fn blocked_report(&self, key: &str) -> Report {
        let secs = self.site.backoff_left().map(|d| d.as_secs()).unwrap_or(0);
        let wait = if secs >= 120 {
            format!("~{} хв", secs.div_ceil(60))
        } else {
            format!("{secs} с")
        };
        Report::new(key, Kind::Blocked, format!("Сайт обмежив запити, пауза {wait}"))
    }

    fn save_session(&self, key: &str, center: Option<u32>, token: Option<String>) {
        if let Some(c) = center {
            self.store.set_center(key, c);
        }
        let mut inner = self.inner.lock().unwrap();
        let sess = inner.sessions.entry(key.to_string()).or_default();
        if let Some(c) = center {
            sess.center = Some(c);
        }
        if token.is_some() {
            sess.token = token;
        }
    }

    fn found(&self, key: &str, days: Vec<Day>) -> Report {
        let mut rep = Report::new(key, Kind::Found, "ЗНАЙДЕНО ВІЛЬНІ МІСЦЯ!");
        rep.days = days;
        rep
    }

    /// Store the report and alert subscribers (with a cooldown while slots stay open).
    async fn publish(&self, report: Report) {
        let checked_at = chrono::Local::now().format("%H:%M:%S").to_string();
        {
            let mut inner = self.inner.lock().unwrap();
            inner.last.insert(report.key.clone(), report.clone());
        }
        match report.kind {
            Kind::Found => {
                info!("🟢 {} — {} day(s) with slots", report.key, report.days.len());
                self.alert(&report, &checked_at).await;
            }
            Kind::NoSlots => info!("🔴 {} — no slots", report.key),
            Kind::Blocked => warn!("🛡 {} — {}", report.key, report.message),
            Kind::Error => error!("⚠ {} — {}", report.key, report.message),
        }
    }

    async fn alert(&self, report: &Report, checked_at: &str) {
        let cooldown = Duration::from_secs(self.cfg.alert_cooldown_secs);
        {
            let inner = self.inner.lock().unwrap();
            if inner.alert_at.get(&report.key).is_some_and(|t| t.elapsed() < cooldown) {
                return;
            }
        }
        let Some(bot) = &self.bot else {
            warn!("slots found but BOT_TOKEN is not set: {}", report.to_html(checked_at));
            return;
        };

        let text = format!("🚨 <b>Вільні слоти!</b>\n{}", report.to_html(checked_at));
        let keyboard = telegram::booking_keyboard(
            config::city(&report.key).map(|c| c.url).unwrap_or("https://pasport.org.ua"),
        );

        let subs = self.store.active_for(&report.key);
        let recipients: Vec<i64> = if subs.is_empty() {
            self.cfg.admin_id.into_iter().collect()
        } else {
            subs
        };
        for chat_id in recipients {
            bot.send(chat_id, &text, Some(keyboard.clone())).await;
        }
        self.inner.lock().unwrap().alert_at.insert(report.key.clone(), Instant::now());

        if self.cfg.open_browser {
            if let Some(city) = config::city(&report.key) {
                open_browser(city.url);
            }
        }
    }
}

fn open_browser(url: &str) {
    #[cfg(target_os = "windows")]
    let cmd = ("cmd", vec!["/C", "start", "", url]);
    #[cfg(target_os = "macos")]
    let cmd = ("open", vec![url]);
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = ("xdg-open", vec![url]);

    if let Err(e) = std::process::Command::new(cmd.0).args(cmd.1).spawn() {
        warn!("cannot open browser: {e}");
    }
}
