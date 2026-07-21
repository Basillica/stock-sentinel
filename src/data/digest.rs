use crate::data::alphavantage::AlphaVantageProvider;
use crate::news::NewsProvider;
use crate::ollama::{DigestReport, OllamaClient};
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Deliberately explicit per-region searches rather than one generic
/// "stock market news" query. A single broad query tends to return
/// whatever's dominant in the query's home market (US-heavy for
/// Google.com), which is exactly the "not just Europe" gap being closed
/// here - being systematic about region coverage matters more than
/// being clever about a single query.
const REGION_QUERIES: &[(&str, &str)] = &[
    ("US", "US stock market today major movers"),
    ("Europe", "European stock markets today"),
    ("Asia", "Asian stock markets today Nikkei Hang Seng"),
    ("Emerging markets", "emerging markets stocks today"),
    ("Macro", "global economy central bank interest rates today"),
];

const AV_TOPICS: &[&str] = &[
    "financial_markets",
    "economy_macro",
    "economy_fiscal",
    "economy_monetary",
];

pub struct DigestGenerator {
    news: Arc<dyn NewsProvider>,
    llm: Arc<OllamaClient>,
    llm_semaphore: Arc<Semaphore>,
    alphavantage: Option<Arc<AlphaVantageProvider>>,
}

impl DigestGenerator {
    pub fn new(
        news: Arc<dyn NewsProvider>,
        llm: Arc<OllamaClient>,
        llm_semaphore: Arc<Semaphore>,
        alphavantage: Option<Arc<AlphaVantageProvider>>,
    ) -> Self {
        Self {
            news,
            llm,
            llm_semaphore,
            alphavantage,
        }
    }

    /// Pulls broad, multi-region market news, folds in Alpha Vantage's
    /// sentiment scoring where available, and asks the model to surface
    /// what's worth researching - both strengthening and weakening
    /// narratives - across the whole set, not just whatever's been
    /// explicitly configured as a theme or watchlist symbol. This is the
    /// "wholistic, not just Europe" sweep, run on its own schedule
    /// (default once/day) separate from the per-symbol scanner.
    pub async fn generate(&self) -> Result<DigestReport> {
        let mut entries: Vec<String> = Vec::new();

        // Explicit per-region searches - free, no quota concern.
        for (region, query) in REGION_QUERIES {
            if let Ok(headlines) = self.news.search(query).await {
                for h in headlines.into_iter().take(6) {
                    entries.push(format!("[{region}] {h}"));
                }
            }
        }

        // Alpha Vantage sentiment sweep, if configured - one API call,
        // rate-limited internally against the 25/day free-tier budget.
        if let Some(av) = &self.alphavantage {
            match av.broad_news_sentiment(AV_TOPICS, 50).await {
                Ok(items) => {
                    for item in items.into_iter().take(40) {
                        let tickers: Vec<String> = item
                            .ticker_sentiment
                            .iter()
                            .filter(|t| t.relevance_score >= 0.3)
                            .map(|t| t.ticker.clone())
                            .collect();
                        let ticker_note = if tickers.is_empty() {
                            String::new()
                        } else {
                            format!(" (tickers: {})", tickers.join(", "))
                        };
                        entries.push(format!(
                            "[AlphaVantage, sentiment {:.2} {}] {}{}",
                            item.overall_sentiment_score,
                            item.overall_sentiment_label,
                            item.title,
                            ticker_note
                        ));
                    }
                }
                Err(e) => tracing::warn!(
                    "Alpha Vantage news sweep failed (continuing with region searches only): {e:?}"
                ),
            }
        }

        entries.sort();
        entries.dedup();

        let _permit = self.llm_semaphore.acquire().await?;
        self.llm.analyze_digest(&entries).await
    }
}
