use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::config::{self, Config};
use crate::monitor::{Kind, Monitor};
use crate::store::Store;

/// Minimal Telegram Bot API client built on the same HTTP stack as the site
/// client. No framework needed for four commands and a few buttons.
#[derive(Clone)]
pub struct Bot {
    http: wreq::Client,
    base: Arc<str>,
}

impl Bot {
    pub fn new(token: &str) -> Self {
        let http = wreq::Client::builder()
            .timeout(Duration::from_secs(40))
            .build()
            .expect("cannot build telegram client");
        Bot { http, base: format!("https://api.telegram.org/bot{token}").into() }
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let url = format!("{}/{}", self.base, method);
        let resp = self.http.post(&url).json(&params).send().await?;
        let v: Value = resp.json().await?;
        if v["ok"].as_bool() != Some(true) {
            bail!("{}: {}", method, v["description"].as_str().unwrap_or("unknown error"));
        }
        Ok(v)
    }

    pub async fn send(&self, chat_id: i64, text: &str, keyboard: Option<Value>) {
        let mut params = json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "HTML",
            "disable_web_page_preview": true
        });
        if let Some(kb) = keyboard {
            params["reply_markup"] = kb;
        }
        if let Err(e) = self.call("sendMessage", params).await {
            warn!("sendMessage to {chat_id} failed: {e}");
        }
    }

    async fn answer(&self, callback_id: &str) {
        let _ = self.call("answerCallbackQuery", json!({ "callback_query_id": callback_id })).await;
    }

    async fn updates(&self, offset: i64) -> Result<Vec<Value>> {
        let v = self
            .call(
                "getUpdates",
                json!({ "offset": offset, "timeout": 25, "allowed_updates": ["message", "callback_query"] }),
            )
            .await?;
        Ok(v["result"].as_array().cloned().unwrap_or_default())
    }
}

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub async fn run(bot: Bot, store: Arc<Store>, monitor: Arc<Monitor>, cfg: Arc<Config>) -> Result<()> {
    let me = bot.call("getMe", json!({})).await?;
    info!("telegram bot @{} online", me["result"]["username"].as_str().unwrap_or("?"));
    let _ = bot.call("deleteWebhook", json!({ "drop_pending_updates": true })).await;

    let mut offset = 0i64;
    loop {
        match bot.updates(offset).await {
            Ok(updates) => {
                for update in updates {
                    if let Some(id) = update["update_id"].as_i64() {
                        offset = id + 1;
                    }
                    if let Err(e) = handle(&bot, &store, &monitor, &cfg, &update).await {
                        warn!("update handling failed: {e}");
                    }
                }
            }
            Err(e) => {
                warn!("getUpdates failed: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn handle(bot: &Bot, store: &Store, monitor: &Monitor, cfg: &Config, update: &Value) -> Result<()> {
    if let Some(msg) = update.get("message") {
        let chat = msg["chat"]["id"].as_i64().unwrap_or_default();
        let username = msg["from"]["username"].as_str();
        let text = msg["text"].as_str().unwrap_or("");
        let cmd = text.split('@').next().unwrap_or("").trim().to_lowercase();
        match cmd.as_str() {
            "/start" => {
                store.touch(chat, username);
                bot.send(chat, &welcome(store, chat, cfg), Some(main_kb(store, chat))).await;
            }
            "/subscribe" => {
                store.touch(chat, username);
                store.set_active(chat, true);
                bot.send(chat, "🔔 Сповіщення увімкнено.", Some(main_kb(store, chat))).await;
            }
            "/unsubscribe" => {
                store.touch(chat, username);
                store.set_active(chat, false);
                bot.send(chat, "🔕 Сповіщення вимкнено.", Some(main_kb(store, chat))).await;
            }
            "/check" => check_now(bot, store, monitor, chat).await,
            "/status" => status(bot, store, monitor, cfg, chat).await,
            "/cities" => bot.send(chat, "📍 Оберіть місто:", Some(cities_kb())).await,
            "/help" => bot.send(chat, help_text(), Some(main_kb(store, chat))).await,
            _ => {}
        }
        return Ok(());
    }

    let Some(cb) = update.get("callback_query") else {
        return Ok(());
    };
    let chat = cb["message"]["chat"]["id"].as_i64().unwrap_or_default();
    let username = cb["from"]["username"].as_str();
    let data = cb["data"].as_str().unwrap_or("");
    store.touch(chat, username);
    let _ = bot.answer(cb["id"].as_str().unwrap_or("")).await;

    if let Some(key) = data.strip_prefix("city:") {
        if config::city(key).is_some() {
            store.set_branch(chat, key);
        }
    }
    match data {
        "check_now" => check_now(bot, store, monitor, chat).await,
        "status" => status(bot, store, monitor, cfg, chat).await,
        "help" => bot.send(chat, help_text(), Some(main_kb(store, chat))).await,
        "cities" => bot.send(chat, "📍 Оберіть місто:", Some(cities_kb())).await,
        "menu" => bot.send(chat, &welcome(store, chat, cfg), Some(main_kb(store, chat))).await,
        "sub_on" => {
            store.set_active(chat, true);
            bot.send(chat, "🔔 Сповіщення увімкнено.", Some(main_kb(store, chat))).await;
        }
        "sub_off" => {
            store.set_active(chat, false);
            bot.send(chat, "🔕 Сповіщення вимкнено.", Some(main_kb(store, chat))).await;
        }
        _ => {
            if data.starts_with("city:") {
                bot.send(chat, &welcome(store, chat, cfg), Some(main_kb(store, chat))).await;
            }
        }
    }
    Ok(())
}

async fn check_now(bot: &Bot, store: &Store, monitor: &Monitor, chat: i64) {
    let key = store.branch_of(chat);
    bot.send(chat, "⏳ Перевіряю…", None).await;
    let report = monitor.check_city(&key).await;
    let at = chrono::Local::now().format("%H:%M:%S").to_string();
    let head = match report.kind {
        Kind::Found => "🎉 <b>Вільні місця!</b>",
        Kind::NoSlots => "ℹ️ <b>Результат перевірки</b>",
        Kind::Blocked => "🛡 <b>Сайт обмежив запити</b>",
        Kind::Error => "⚠️ <b>Помилка</b>",
    };
    let kb = (report.kind == Kind::Found)
        .then(|| config::city(&key).map(|c| booking_keyboard(c.url)))
        .flatten();
    bot.send(chat, &format!("{head}\n{}", report.to_html(&at)), kb).await;
}

async fn status(bot: &Bot, store: &Store, monitor: &Monitor, cfg: &Config, chat: i64) {
    let key = store.branch_of(chat);
    let last = monitor.last_report(&key);
    let name = config::city(&key).map(|c| c.name).unwrap_or(key.as_str());
    let mut text = format!(
        "📊 <b>Статус</b>\n👥 Підписників: {}\n⏱ Інтервал: ~{} с\n📍 Місто: {}\n",
        store.active_count(),
        cfg.poll_secs,
        esc(name)
    );
    match last {
        Some(r) => text.push_str(&format!("🕐 Остання перевірка: {}", esc(&r.message))),
        None => text.push_str("🕐 Ще не перевірялось"),
    }
    bot.send(chat, &text, Some(main_kb(store, chat))).await;
}

fn welcome(store: &Store, chat: i64, cfg: &Config) -> String {
    let key = store.branch_of(chat);
    let name = config::city(&key).map(|c| c.name).unwrap_or(key.as_str());
    let sub = if store.is_active(chat) { "✅ увімкнено" } else { "❌ вимкнено" };
    format!(
        "👋 <b>Моніторинг е-черги ДП «Документ»</b>\n\n📍 Місто: <b>{}</b>\n🔔 Сповіщення: {}\n⏱ Перевірка кожні ~{} с\n\nЯкщо з'являться слоти — надішлю посилання одразу.",
        esc(name),
        sub,
        cfg.poll_secs
    )
}

fn help_text() -> &'static str {
    "📖 <b>Команди</b>\n• /start — меню\n• /check — перевірити зараз\n• /subscribe, /unsubscribe — сповіщення\n• /cities — змінити місто\n• /status — стан\n\nПісля появи слотів заповніть форму на сайті: капчу та SMS-код підтверджуєте ви."
}

fn main_kb(store: &Store, chat: i64) -> Value {
    let sub = store.is_active(chat);
    let key = store.branch_of(chat);
    let name = config::city(&key).map(|c| c.name).unwrap_or(key.as_str());
    json!({ "inline_keyboard": [
        [{ "text": "🔍 Перевірити зараз", "callback_data": "check_now" }],
        [{ "text": if sub { "🔕 Вимкнути сповіщення" } else { "🔔 Увімкнути сповіщення" },
          "callback_data": if sub { "sub_off" } else { "sub_on" } }],
        [{ "text": format!("📍 {name}"), "callback_data": "cities" }],
        [{ "text": "📊 Статус", "callback_data": "status" }, { "text": "ℹ️ Допомога", "callback_data": "help" }]
    ]})
}

fn cities_kb() -> Value {
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut row: Vec<Value> = Vec::new();
    for c in config::CITIES {
        row.push(json!({ "text": c.name, "callback_data": format!("city:{}", c.key) }));
        if row.len() == 2 {
            rows.push(std::mem::take(&mut row));
        }
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows.push(vec![json!({ "text": "⬅️ Назад", "callback_data": "menu" })]);
    json!({ "inline_keyboard": rows })
}

pub fn booking_keyboard(url: &str) -> Value {
    json!({ "inline_keyboard": [
        [{ "text": "⚡ ПЕРЕЙТИ ДО ЗАПИСУ", "url": url }],
        [{ "text": "🔄 Перевірити знову", "callback_data": "check_now" }]
    ]})
}
