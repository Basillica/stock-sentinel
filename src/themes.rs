use crate::news::NewsProvider;
use crate::ollama::{OllamaClient, ThemeAnalysis};
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// A tracked macro theme: a name, search keywords, and the tickers you'd
/// consider researching if something big happens in this space. E.g.
/// name="german_defense", keywords=["Germany defense spending", "NATO
/// budget increase", "Bundeswehr"], symbols=["RHM.DE", "RNMBY"].
#[derive(Debug, Clone)]
pub struct Theme {
    pub name: String,
    pub keywords: Vec<String>,
    pub symbols: Vec<String>,
}

pub struct ThemeWatcher {
    news: Arc<dyn NewsProvider>,
    llm: Arc<OllamaClient>,
    llm_semaphore: Arc<Semaphore>,
}

impl ThemeWatcher {
    pub fn new(news: Arc<dyn NewsProvider>, llm: Arc<OllamaClient>, llm_semaphore: Arc<Semaphore>) -> Self {
        Self {
            news,
            llm,
            llm_semaphore,
        }
    }

    /// Searches news for each of the theme's keywords, pools the
    /// headlines, and asks the model whether this looks like a durable,
    /// significant shift worth researching - never a buy signal, always
    /// "here's what's happening, go look at these tickers yourself".
    ///
    /// This is the honest version of "I should have bought Rheinmetal":
    /// there is no way to guarantee catching the next one, and hindsight
    /// makes any single past example look obvious in a way it wasn't at
    /// the time. What this *can* do is make sure a genuinely large,
    /// sustained policy shift in a theme you're already tracking doesn't
    /// go unnoticed while you're not actively reading the news yourself.
    pub async fn check(&self, theme: &Theme) -> Result<ThemeAnalysis> {
        let mut headlines = Vec::new();
        for keyword in &theme.keywords {
            if let Ok(mut found) = self.news.search(keyword).await {
                headlines.append(&mut found);
            }
        }
        headlines.sort();
        headlines.dedup();

        let _permit = self.llm_semaphore.acquire().await?;
        self.llm
            .analyze_theme(&theme.name, &theme.symbols, &headlines)
            .await
    }
}
