use crate::ratelimit::RateLimiter;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

/// One daily bar, oldest-to-newest when returned from `daily_history`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OhlcBar {
    pub date: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}

/// Twelve Data's free tier (800 requests/day, ~8/min) includes real daily
/// OHLC history, which Finnhub's free tier does not (`/stock/candle` is
/// premium-only there - confirmed against the swagger export earlier in
/// this project). This is what makes a genuine ATR - not the close-only
/// approximation - possible without a paid plan.
pub struct TwelveDataProvider {
    api_key: String,
    client: reqwest::Client,
    rate_limiter: Arc<RateLimiter>,
}

impl TwelveDataProvider {
    pub fn new(api_key: String, rate_limiter: Arc<RateLimiter>) -> Self {
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build HTTPS client - is the rustls-tls feature enabled?");
        Self {
            api_key,
            client,
            rate_limiter,
        }
    }

    pub async fn daily_history(&self, symbol: &str, outputsize: usize) -> Result<Vec<OhlcBar>> {
        self.rate_limiter.acquire().await;
        let url = format!(
            "https://api.twelvedata.com/time_series?symbol={symbol}&interval=1day&outputsize={outputsize}&apikey={}",
            self.api_key
        );
        let resp: TwelveDataResponse = self
            .client
            .get(&url)
            .send()
            .await
            .context("twelve data request failed")?
            .json()
            .await
            .context("twelve data response was not the expected shape")?;

        if resp.status.as_deref() != Some("ok") {
            anyhow::bail!(
                "twelve data returned an error: {}",
                resp.message.unwrap_or_else(|| "no message".into())
            );
        }

        let mut bars: Vec<OhlcBar> = resp
            .values
            .unwrap_or_default()
            .into_iter()
            .filter_map(|v| {
                Some(OhlcBar {
                    date: v.datetime,
                    open: v.open.parse().ok()?,
                    high: v.high.parse().ok()?,
                    low: v.low.parse().ok()?,
                    close: v.close.parse().ok()?,
                })
            })
            .collect();
        // Twelve Data returns newest-first; every indicator in this
        // project expects oldest-first.
        bars.reverse();
        Ok(bars)
    }
}

#[derive(Deserialize)]
struct TwelveDataResponse {
    status: Option<String>,
    message: Option<String>,
    values: Option<Vec<TwelveDataBar>>,
}

#[derive(Deserialize)]
struct TwelveDataBar {
    datetime: String,
    open: String,
    high: String,
    low: String,
    close: String,
}
