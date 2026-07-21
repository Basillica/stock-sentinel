use crate::notifier::Notifier;
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

pub struct MultiNotifier {
    notifiers: Vec<Arc<dyn Notifier>>,
    service: String,
}

impl MultiNotifier {
    pub fn new(notifiers: Vec<Arc<dyn Notifier>>) -> Self {
        Self {
            notifiers,
            service: "MULTI".to_string(),
        }
    }
}

#[async_trait]
impl Notifier for MultiNotifier {
    async fn send(&self, text: &str) -> Result<()> {
        tracing::info!("sending for service {}", self.service);
        for notifier in &self.notifiers {
            if let Err(err) = notifier.send(text).await {
                tracing::error!("Notifier failed: {err:#}");
            }
        }
        Ok(())
    }
}
