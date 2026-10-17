mod board;
mod bot;
mod db;
mod llm;
mod signal;
mod web;

use anyhow::{Context, Result, bail};
use std::sync::Arc;

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let db_path = env("DB_PATH").unwrap_or_else(|| "buzziwork.db".into());
    let signal_cli = env("SIGNAL_CLI").unwrap_or_else(|| "signal-cli".into());
    let account = env("SIGNAL_ACCOUNT"); // optional when signal-cli has exactly one account
    let config_dir = env("SIGNAL_CONFIG_DIR");
    let group_id = env("GROUP_ID");
    let web_url = env("BOARD_URL");
    let board_hour: u32 = env("BOARD_HOUR").map(|h| h.parse()).transpose()?.unwrap_or(7);
    let llm = llm::OpenRouter::new(
        env("OPENROUTER_BASE_URL").unwrap_or_else(|| "https://openrouter.ai/api/v1".into()),
        env("OPENROUTER_API_KEY").context("OPENROUTER_API_KEY is required")?,
        env("OPENROUTER_MODEL").unwrap_or_else(|| "anthropic/claude-haiku-5.5".into()),
    );

    let db = Arc::new(db::Db::open(&db_path).with_context(|| format!("opening {db_path}"))?);

    if let Some(addr) = env("BOARD_LISTEN") {
        let token = env("BOARD_TOKEN").context("BOARD_TOKEN is required when BOARD_LISTEN is set")?;
        let title = env("BOARD_TITLE").unwrap_or_else(|| "Klussen".into());
        let db = db.clone();
        tokio::spawn(async move {
            if let Err(e) = web::serve(&addr, db, token, title).await {
                log::error!("web board stopped: {e:#}");
            }
        });
    }

    let (sig, mut rx) = signal::Signal::spawn(&signal_cli, account.as_deref(), config_dir.as_deref())?;
    let bot = Arc::new(bot::Bot::new(db, sig, llm, group_id, web_url));
    tokio::spawn(bot.clone().daily_loop(board_hour));

    log::info!("buzziwork running");
    while let Some(params) = rx.recv().await {
        log::debug!("receive: {params}");
        if let Some(inc) = signal::parse_incoming(&params)
            && let Err(e) = bot.handle(inc).await
        {
            log::error!("handling message: {e:#}");
        }
    }
    bail!("signal-cli exited; stopping (let systemd restart us)")
}
