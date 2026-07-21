use crate::notifier::Notifier;
use anyhow::{Context, Result};
use async_trait::async_trait;

pub struct NtfyNotifier {
    server_url: String, // Defaults to "https://ntfy.sh"
    topic: String,
    api_key: String,
    client: reqwest::Client,
}

impl NtfyNotifier {
    pub fn new(server_url: Option<String>, topic: String, api_key: String) -> Self {
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .build()
            .expect("failed to build HTTPS client");

        Self {
            server_url: server_url.unwrap_or_else(|| "https://ntfy.sh".into()),
            topic,
            client,
            api_key,
        }
    }
}

#[async_trait]
impl Notifier for NtfyNotifier {
    async fn send(&self, text: &str) -> Result<()> {
        let url = format!("{}/{}", self.server_url.trim_end_matches('/'), self.topic);

        let resp = self
            .client
            .post(&url)
            .header("Markdown", "yes") // Enables bold/italic markdown parsing
            .header("Authorization", format!("Bearer {}", &self.api_key))
            .body(text.to_owned())
            .send()
            .await
            .context("failed to reach ntfy server")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("ntfy API returned error (status {}): {body}", status);
        }

        Ok(())
    }
}
