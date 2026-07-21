use crate::notifier::Notifier;
use anyhow::{Context, Result};
use async_trait::async_trait;
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
}

#[async_trait]
impl Notifier for TelegramNotifier {
    async fn send(&self, text: &str) -> Result<()> {
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
