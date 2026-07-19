use crate::ratelimit::RateLimiter;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct Quote {
    pub price: f64,
}

/// Abstraction over "wherever price data comes from". Swap Finnhub for
/// Twelve Data, Alpha Vantage, IEX, or a local CSV replay without touching
/// the strategy or routing code.
#[async_trait]
pub trait MarketDataProvider: Send + Sync {
    async fn quote(&self, symbol: &str) -> Result<Quote>;
}

pub struct FinnhubProvider {
    api_key: String,
    client: reqwest::Client,
    rate_limiter: Arc<RateLimiter>,
}

impl FinnhubProvider {
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
}

#[derive(Deserialize)]
struct FinnhubQuoteResponse {
    c: f64, // current price
}

#[async_trait]
impl MarketDataProvider for FinnhubProvider {
    async fn quote(&self, symbol: &str) -> Result<Quote> {
        let url = format!(
            "https://finnhub.io/api/v1/quote?symbol={symbol}&token={}",
            self.api_key
        );

        // 3 attempts, exponential backoff. Transient network blips and
        // momentary rate-limit hiccups shouldn't take a symbol out of a
        // whole scan cycle - they should just cost a couple hundred ms.
        let mut last_err = None;
        for attempt in 0..3u32 {
            self.rate_limiter.acquire().await;
            match self.client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    return resp
                        .json::<FinnhubQuoteResponse>()
                        .await
                        .map(|r| Quote { price: r.c })
                        .context("finnhub response was not the expected shape");
                }
                Ok(resp) => {
                    last_err = Some(anyhow::anyhow!("finnhub returned status {}", resp.status()));
                }
                Err(e) => {
                    last_err = Some(anyhow::Error::new(e).context("finnhub request failed"));
                }
            }
            if attempt < 2 {
                tokio::time::sleep(Duration::from_millis(200 * 2u64.pow(attempt))).await;
            }
        }
        Err(last_err
            .unwrap_or_else(|| anyhow::anyhow!("finnhub request failed with no error detail")))
    }
}

/// Deterministic fake provider for local dev / tests, so you can run the
/// whole server and hit the API without burning a real key or rate limit.
pub struct MockProvider;

#[async_trait]
impl MarketDataProvider for MockProvider {
    async fn quote(&self, symbol: &str) -> Result<Quote> {
        // Cheap pseudo-random walk keyed off the symbol so repeated calls
        // for the same ticker still look like a plausible price series.
        let seed: u64 = symbol.bytes().map(|b| b as u64).sum();
        let t = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            / 5) as u64; // changes every 5s
        let noise =
            (((seed.wrapping_mul(2654435761).wrapping_add(t)) % 1000) as f64 - 500.0) / 500.0; // -1.0..1.0
        let base = 100.0 + (seed % 400) as f64;
        Ok(Quote {
            price: (base * (1.0 + noise * 0.01)).max(0.01),
        })
    }
}
