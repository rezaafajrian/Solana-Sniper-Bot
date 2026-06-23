//! Minimal Telegram alerting. Fire-and-forget push notifications for the events you
//! need to know about while the bot runs unattended: circuit-breaker halt, margin call,
//! a quarantined (unsellable) token, and a position that left the wallet (you sold it
//! manually / from a Telegram trading bot). Configured via TELEGRAM_BOT_TOKEN +
//! TELEGRAM_CHAT_ID; a no-op when unset, so it never affects the bot if you don't use it.

use once_cell::sync::OnceCell;
use reqwest::Client;

static NOTIFIER: OnceCell<Option<Telegram>> = OnceCell::new();

struct Telegram {
    token: String,
    chat_id: String,
    http: Client,
}

impl Telegram {
    fn from_env() -> Option<Telegram> {
        let token = std::env::var("TELEGRAM_BOT_TOKEN").ok().filter(|s| !s.is_empty())?;
        let chat_id = std::env::var("TELEGRAM_CHAT_ID").ok().filter(|s| !s.is_empty())?;
        Some(Telegram { token, chat_id, http: Client::new() })
    }
}

/// Initialize from env once at startup. Safe to call repeatedly (only the first wins).
/// Returns whether Telegram alerting is active.
pub fn init_from_env() -> bool {
    let cfg = Telegram::from_env();
    let active = cfg.is_some();
    let _ = NOTIFIER.set(cfg);
    active
}

/// Fire-and-forget alert. No-op if Telegram isn't configured. Never blocks the caller
/// and never propagates errors — a down Telegram must not affect trading. Must be called
/// from within a tokio runtime (the bot always is).
pub fn notify(msg: impl Into<String>) {
    let msg = msg.into();
    if let Some(Some(tg)) = NOTIFIER.get() {
        let (token, chat_id, http) = (tg.token.clone(), tg.chat_id.clone(), tg.http.clone());
        tokio::spawn(async move {
            let url = format!("https://api.telegram.org/bot{}/sendMessage", token);
            let body = serde_json::json!({
                "chat_id": chat_id,
                "text": msg,
                "disable_web_page_preview": true,
            })
            .to_string();
            let _ = http
                .post(&url)
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await;
        });
    }
}
