use anyhow::{Context, Result};
use serde_json::json;

pub struct TelegramNotifier {
    token: String,
    chat_id: String,
    client: reqwest::Client,
}

impl TelegramNotifier {
    pub fn new(token: String, chat_id: String) -> Self {
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .build()
            .expect("failed to build HTTPS client - is the rustls-tls feature enabled?");
        Self {
            token,
            chat_id,
            client,
        }
    }

    pub async fn send(&self, text: &str) -> Result<()> {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
        let resp = self
            .client
            .post(&url)
            .json(&json!({
                "chat_id": self.chat_id,
                "text": text,
                "parse_mode": "Markdown",
                "disable_web_page_preview": true,
            }))
            .send()
            .await
            .context("failed to reach Telegram API")?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Telegram API returned an error: {body}");
        }
        Ok(())
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
