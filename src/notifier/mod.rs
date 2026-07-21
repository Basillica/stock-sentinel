pub mod multi;
pub mod ntfy;
pub mod telegram;

use crate::notifier::multi::MultiNotifier;
use crate::notifier::ntfy::NtfyNotifier;
use crate::notifier::telegram::TelegramNotifier;
use std::{env, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait Notifier: Send + Sync {
    /// Sends a formatted string message to the underlying service.
    async fn send(&self, text: &str) -> Result<()>;
}

pub fn init() -> Option<Arc<dyn Notifier>> {
    let mut active_notifiers: Vec<Arc<dyn Notifier>> = Vec::new();

    // 1. Check Telegram
    if let (Ok(token), Ok(chat_id)) = (env::var("TELEGRAM_BOT_TOKEN"), env::var("TELEGRAM_CHAT_ID"))
    {
        if !token.is_empty() && !chat_id.is_empty() {
            tracing::info!("Telegram notifications enabled");
            active_notifiers.push(Arc::new(TelegramNotifier::new(token, chat_id)));
        }
    }

    // 2. Check ntfy
    if let (Ok(topic), Ok(api_key)) = (env::var("NTFY_TOPIC"), env::var("NTFY_API_KEY")) {
        if !topic.is_empty() && !api_key.is_empty() {
            let server_url = env::var("NTFY_SERVER_URL").ok();
            tracing::info!("ntfy notifications enabled for topic: {topic}");
            active_notifiers.push(Arc::new(NtfyNotifier::new(server_url, topic, api_key)));
        }
    }

    // Return single, multi, or none based on configuration
    match active_notifiers.len() {
        0 => {
            tracing::info!("No notification providers configured - alerts will only go to logs");
            None
        }
        1 => Some(active_notifiers.remove(0)),
        _ => Some(Arc::new(MultiNotifier::new(active_notifiers))),
    }
}

/// Formats a scanner verdict into a short, skimmable alert. Kept as a free
/// function (not a method on Verdict) so telegram.rs stays the only place
/// that knows about Telegram's formatting quirks.
pub fn format_alert(symbol: &str, verdict: &crate::pipeline::Verdict) -> Option<String> {
    use crate::pipeline::Verdict;
    match verdict {
        Verdict::SellAll { reason } => Some(format!("🔴 *{symbol}: SELL ALL*\n{reason}")),
        Verdict::TrimProfit { fraction, reason } => Some(format!(
            "🟡 *{symbol}: trim {:.0}%*\n{reason}",
            fraction * 100.0
        )),
        Verdict::Watch { confidence } => Some(format!(
            "🟠 *{symbol}: news risk flag* (confidence {confidence:.2}) - worth a look."
        )),
        Verdict::Buy { confidence } => Some(format!(
            "🟢 *{symbol}: candidate BUY signal* (confidence {confidence:.2})"
        )),
        Verdict::Hold | Verdict::Avoid { .. } => None, // routine outcomes, don't spam
    }
}
