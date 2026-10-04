mod config;
mod monitor;
mod proxy;
mod site;
mod store;
mod telegram;

use std::sync::Arc;

use anyhow::Result;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let cfg = Arc::new(config::Config::from_env()?);
    let store = Arc::new(store::Store::load(&cfg.data_dir));
    let site = Arc::new(site::Site::new(&cfg).await?);
    let bot = (!cfg.bot_token.is_empty()).then(|| telegram::Bot::new(&cfg.bot_token));
    let monitor = Arc::new(monitor::Monitor::new(cfg.clone(), site, store.clone(), bot.clone()));

    let m = monitor.clone();
    tokio::spawn(async move { m.run().await });

    match bot {
        Some(bot) => {
            let bot_task = tokio::spawn(telegram::run(bot, store, monitor, cfg.clone()));
            tokio::select! {
                _ = tokio::signal::ctrl_c() => info!("shutting down"),
                res = bot_task => if let Err(e) = res { error!("telegram task: {e}"); },
            }
        }
        None => {
            warn!("BOT_TOKEN is empty — monitoring only, alerts are printed to the log");
            tokio::signal::ctrl_c().await.ok();
        }
    }
    Ok(())
}
