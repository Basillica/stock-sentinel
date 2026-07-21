use reqwest;
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Deserialize, Debug)]
pub struct AlphaVantageResponse {
    #[serde(rename = "Time Series (Daily)")]
    time_series: HashMap<String, TimeSeriesData>,
}

#[derive(Deserialize, Debug)]
pub struct TimeSeriesData {
    #[serde(rename = "1. open")]
    pub open: String,
    #[serde(rename = "2. high")]
    pub high: String,
    #[serde(rename = "3. low")]
    pub low: String,
    #[serde(rename = "4. close")]
    pub close: String,
    #[serde(rename = "5. volume")]
    pub volume: String,
}

#[derive(Debug)]
pub struct StockData {
    pub date: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: u64,
}

pub async fn fetch_stock_data(
    symbol: &str,
    stock_api_keys: String,
) -> Result<Vec<StockData>, Box<dyn std::error::Error>> {
    let url = format!(
        "https://www.alphavantage.co/query?function=TIME_SERIES_DAILY&symbol={}&apikey={}&outputsize=compact",
        symbol, stock_api_keys
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10)) // Timeout after 10 seconds
        .build()?;

    let response = client.get(&url).send().await?;

    if !response.status().is_success() {
        return Err(format!("API Error: {}", response.status()).into());
    }

    let json: AlphaVantageResponse = response.json().await?;

    let mut data: Vec<StockData> = json
        .time_series
        .iter()
        .map(|(date, ts)| StockData {
            date: date.clone(),
            open: ts.open.parse().unwrap_or(0.0),
            high: ts.high.parse().unwrap_or(0.0),
            low: ts.low.parse().unwrap_or(0.0),
            close: ts.close.parse().unwrap_or(0.0),
            volume: ts.volume.parse().unwrap_or(0),
        })
        .collect();

    // Alpha Vantage returns data from newest to oldest. We want oldest to newest for indicators.
    data.reverse();

    Ok(data)
}
