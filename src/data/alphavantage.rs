use crate::ratelimit::RateLimiter;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

/// Alpha Vantage's free tier is 25 requests/day, 5/minute - far too tight
/// to use as a quote provider alongside Finnhub. It IS enough for a
/// couple of calls a day, which is exactly the shape of "pull a broad
/// news+sentiment sweep once or twice daily" rather than "poll per
/// symbol per scan cycle". Two rate limiters are enforced here (not just
/// one) because the daily cap and the per-minute cap are independent
/// constraints - burning through 25 calls in the first minute would be
/// within the per-minute limit but would empty the daily budget for
/// nothing.
pub struct AlphaVantageProvider {
    api_key: String,
    client: reqwest::Client,
    per_minute: Arc<RateLimiter>,
    per_day: Arc<RateLimiter>,
}

impl AlphaVantageProvider {
    pub fn new(api_key: String) -> Self {
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("failed to build HTTPS client - is the rustls-tls feature enabled?");
        Self {
            api_key,
            client,
            per_minute: Arc::new(RateLimiter::new(5, Duration::from_secs(60))),
            per_day: Arc::new(RateLimiter::new(25, Duration::from_secs(24 * 60 * 60))),
        }
    }

    /// NEWS_SENTIMENT with no ticker filter and broad macro/market topics -
    /// this is the "what's happening in the world" call, not a per-symbol
    /// one. `topics` follows Alpha Vantage's fixed vocabulary (e.g.
    /// "financial_markets", "economy_macro", "economy_fiscal",
    /// "economy_monetary", "technology", "energy_transportation").
    pub async fn broad_news_sentiment(
        &self,
        topics: &[&str],
        limit: u32,
    ) -> Result<Vec<NewsSentimentItem>> {
        self.per_minute.acquire().await;
        self.per_day.acquire().await;

        let topics_param = topics.join(",");
        let url = format!(
            "https://www.alphavantage.co/query?function=NEWS_SENTIMENT&topics={}&sort=LATEST&limit={}&apikey={}",
            urlencode(&topics_param),
            limit,
            self.api_key
        );

        let resp: AvNewsResponse = self
            .client
            .get(&url)
            .send()
            .await
            .context("failed to reach Alpha Vantage")?
            .json()
            .await
            .context("Alpha Vantage response was not the expected shape")?;

        if let Some(note) = resp.note.or(resp.information) {
            anyhow::bail!("Alpha Vantage declined the request: {note}");
        }

        Ok(resp.feed.unwrap_or_default())
    }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b',' => {
                (b as char).to_string()
            }
            _ => format!("%{:02X}", b),
        })
        .collect()
}

#[derive(Deserialize)]
struct AvNewsResponse {
    feed: Option<Vec<NewsSentimentItem>>,
    // Alpha Vantage returns these as plain-text fields instead of an HTTP
    // error status when a request is malformed or over quota - both must
    // be checked explicitly or a bad request looks like an empty result.
    #[serde(rename = "Note")]
    note: Option<String>,
    #[serde(rename = "Information")]
    information: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsSentimentItem {
    pub title: String,
    pub url: String,
    pub time_published: String,
    pub summary: String,
    pub source: String,
    #[serde(default)]
    pub overall_sentiment_score: f64,
    #[serde(default)]
    pub overall_sentiment_label: String,
    #[serde(default)]
    pub ticker_sentiment: Vec<TickerSentiment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TickerSentiment {
    pub ticker: String,
    #[serde(default, deserialize_with = "str_or_f64")]
    pub relevance_score: f64,
    #[serde(default, deserialize_with = "str_or_f64")]
    pub ticker_sentiment_score: f64,
}

// Alpha Vantage sends numeric fields as JSON strings ("0.532101") rather
// than numbers in this endpoint - deserialize either shape defensively.
fn str_or_f64<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StrOrF64 {
        S(String),
        F(f64),
    }
    match StrOrF64::deserialize(deserializer)? {
        StrOrF64::S(s) => s.parse().map_err(serde::de::Error::custom),
        StrOrF64::F(f) => Ok(f),
    }
}
