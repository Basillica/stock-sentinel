use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};

/// Extra Finnhub endpoints beyond /quote. Kept separate from
/// `MarketDataProvider` because these are Finnhub-specific (no equivalent
/// abstraction needed for the Mock provider) and several are gated by
/// subscription tier - every method here degrades to `None`/empty on
/// failure (auth, rate limit, premium-only) rather than erroring out the
/// whole pipeline. Verified against the endpoints in your swagger export;
/// see the comment on each method for its documented free-tier status.
pub struct FinnhubExtras {
    api_key: String,
    client: reqwest::Client,
}

impl FinnhubExtras {
    pub fn new(api_key: String) -> Self {
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .build()
            .expect("failed to build HTTPS client - is the rustls-tls feature enabled?");
        Self { api_key, client }
    }

    fn url(&self, path: &str, query: &str) -> String {
        format!("https://finnhub.io/api/v1{path}?{query}&token={}", self.api_key)
    }

    /// GET /company-news - free tier gets 1 year of history. Structured
    /// (headline + datetime + source), which is why we prefer this over
    /// the RSS scraper whenever a Finnhub key is configured: no dedup
    /// guesswork, and we can bound the window precisely.
    pub async fn company_news(&self, symbol: &str, days_back: i64) -> Result<Vec<CompanyNewsItem>> {
        let to = Utc::now().date_naive();
        let from = to - Duration::days(days_back);
        let url = self.url(
            "/company-news",
            &format!("symbol={symbol}&from={from}&to={to}"),
        );
        let items: Vec<CompanyNewsItem> = self
            .client
            .get(&url)
            .send()
            .await
            .context("company-news request failed")?
            .json()
            .await
            .context("company-news response was not the expected shape")?;
        Ok(items)
    }

    /// GET /stock/metric?metric=all - no premium flag in the swagger, but
    /// marked highUsage; treat failures as "unavailable" rather than fatal.
    pub async fn basic_financials(&self, symbol: &str) -> Result<BasicFinancials> {
        let url = self.url("/stock/metric", &format!("symbol={symbol}&metric=all"));
        let resp: BasicFinancialsResponse = self
            .client
            .get(&url)
            .send()
            .await
            .context("basic financials request failed")?
            .json()
            .await
            .context("basic financials response was not the expected shape")?;
        Ok(resp.metric)
    }

    /// GET /calendar/earnings?symbol=X - free tier gets 1 month forward.
    /// Returns the next upcoming release if one falls within `days_ahead`.
    pub async fn next_earnings(&self, symbol: &str, days_ahead: i64) -> Result<Option<EarningsEvent>> {
        let from = Utc::now().date_naive();
        let to = from + Duration::days(days_ahead);
        let url = self.url(
            "/calendar/earnings",
            &format!("symbol={symbol}&from={from}&to={to}"),
        );
        let resp: EarningsCalendarResponse = self
            .client
            .get(&url)
            .send()
            .await
            .context("earnings calendar request failed")?
            .json()
            .await
            .context("earnings calendar response was not the expected shape")?;
        Ok(resp.earnings_calendar.into_iter().next())
    }

    /// GET /stock/recommendation - no premium flag; latest period's
    /// analyst buy/hold/sell counts.
    pub async fn recommendation_trend(&self, symbol: &str) -> Result<Option<RecommendationTrend>> {
        let url = self.url("/stock/recommendation", &format!("symbol={symbol}"));
        let resp: Vec<RecommendationTrend> = self
            .client
            .get(&url)
            .send()
            .await
            .context("recommendation trend request failed")?
            .json()
            .await
            .context("recommendation trend response was not the expected shape")?;
        Ok(resp.into_iter().next())
    }

    /// GET /stock/peers - no premium flag; useful for building a watchlist
    /// starting from one ticker you already know.
    pub async fn peers(&self, symbol: &str) -> Result<Vec<String>> {
        let url = self.url("/stock/peers", &format!("symbol={symbol}"));
        let resp: Vec<String> = self
            .client
            .get(&url)
            .send()
            .await
            .context("peers request failed")?
            .json()
            .await
            .context("peers response was not the expected shape")?;
        Ok(resp)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CompanyNewsItem {
    pub headline: String,
    pub summary: String,
    pub datetime: i64,
    pub source: String,
}

#[derive(Deserialize)]
struct BasicFinancialsResponse {
    metric: BasicFinancials,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BasicFinancials {
    #[serde(rename = "peBasicExclExtraTTM")]
    pub pe_ttm: Option<f64>,
    #[serde(rename = "52WeekHigh")]
    pub week52_high: Option<f64>,
    #[serde(rename = "52WeekLow")]
    pub week52_low: Option<f64>,
    pub beta: Option<f64>,
}

#[derive(Deserialize)]
struct EarningsCalendarResponse {
    #[serde(rename = "earningsCalendar")]
    earnings_calendar: Vec<EarningsEvent>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EarningsEvent {
    pub date: String,
    /// "bmo" (before market open), "amc" (after market close), or "dmh".
    pub hour: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RecommendationTrend {
    pub buy: i64,
    pub hold: i64,
    pub sell: i64,
    #[serde(rename = "strongBuy")]
    pub strong_buy: i64,
    #[serde(rename = "strongSell")]
    pub strong_sell: i64,
    pub period: String,
}

impl RecommendationTrend {
    /// -1.0 (unanimous sell) .. 1.0 (unanimous buy). None if no analysts.
    pub fn consensus_score(&self) -> Option<f64> {
        let total = self.buy + self.hold + self.sell + self.strong_buy + self.strong_sell;
        if total == 0 {
            return None;
        }
        let weighted = (2 * self.strong_buy + self.buy) as f64
            - (2 * self.strong_sell + self.sell) as f64;
        Some((weighted / (2 * total) as f64).clamp(-1.0, 1.0))
    }
}
