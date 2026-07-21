use crate::data::fetcher::StockData;
use crate::macros::themes::Theme;
use reqwest::Client;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::error::Error;
use ta::indicators::{
    ChandelierExit, ExponentialMovingAverage, MoneyFlowIndex, RelativeStrengthIndex,
};
use ta::{DataItem, Next};
use tokio::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum Signal {
    StrongBuy,
    Buy,
    Hold,
    Sell,
    StrongSell,
}

#[derive(Debug)]
pub struct AnalysisResult {
    pub signal: Signal,
    pub confidence: f64,
    pub reason: String,
    pub themes: Vec<Theme>,
}

pub struct Candle {
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

struct EngineMetrics {
    ticker: String,
    current_price: f64,
    rsi: f64,
    mfi: f64,
    ema_50: f64,
    ema_200: f64,
    ce_long_stop: f64,
    sentiment: f64,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct TickerConfig {
    pub symbol: String,
    pub is_owned: bool,
    pub entry_price: f64,
    pub highest_tracked_price: f64,
    pub current_price: f64,
    pub rsi: f64,
    pub sentiment: f64,
    pub latest_signal: String,
    pub shares: f64,             // e.g., 100.0
    pub half_profit_taken: bool, // false initially
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaSentimentResponse {
    sentiment_score: f64,
}

pub async fn run_orchestrator(mut ticker: TickerConfig) {
    println!("Fetching data for {}...", &ticker.symbol);

    let market_data = match fetch_yahoo_market_data(&ticker.symbol, 250).await {
        Ok(data) => data,
        Err(e) => {
            eprintln!("Ingestion failed for {}: {}", &ticker.symbol, e);
            return; // Abort this run if we can't get data
        }
    };

    if market_data.is_empty() {
        eprintln!("No data returned for {}", &ticker.symbol);
        return;
    }

    // 2. Initialize the Alpha Matrix Indicators
    let mut rsi_ind = RelativeStrengthIndex::new(14).unwrap();
    let mut mfi_ind = MoneyFlowIndex::new(14).unwrap();
    let mut ce_ind = ChandelierExit::new(22, 3.0).unwrap(); // 22 periods, 3.0 ATR multiplier
    let mut ema_50_ind = ExponentialMovingAverage::new(50).unwrap();
    let mut ema_200_ind = ExponentialMovingAverage::new(200).unwrap();

    let mut metrics = EngineMetrics {
        ticker: ticker.symbol.clone(),
        current_price: 0.0,
        rsi: 0.0,
        mfi: 0.0,
        ema_50: 0.0,
        ema_200: 0.0,
        ce_long_stop: 0.0,
        sentiment: 0.0,
    };

    // 3. Process the data pipeline
    for candle in &market_data {
        // The ta crate requires a DataItem for complex OHLCV indicators
        let item = DataItem::builder()
            .open(candle.open)
            .high(candle.high)
            .low(candle.low)
            .close(candle.close)
            .volume(candle.volume)
            .build()
            .unwrap();

        metrics.rsi = rsi_ind.next(candle.close);
        metrics.mfi = mfi_ind.next(&item);

        let ce_out = ce_ind.next(&item);
        metrics.ce_long_stop = ce_out.long; // Extracts the trailing stop line

        metrics.ema_50 = ema_50_ind.next(candle.close);
        metrics.ema_200 = ema_200_ind.next(candle.close);
        metrics.current_price = candle.close;
    }

    // 4. Sentiment Analysis
    metrics.sentiment = analyze_news_sentiment_locally().await;

    // 5. Evaluate Matrix Rules
    let final_decision = evaluate_trading_rules(&metrics, &mut ticker);
    println!("the final decision: {final_decision}");

    // ... [Alerting and Database saving logic remains the same] ...
}

pub struct ArticleData {
    pub title: String,
    pub body_snippet: String,
}

pub async fn fetch_ticker_news(ticker: &str) -> Vec<ArticleData> {
    let url = format!(
        "https://query2.finance.yahoo.com/v1/finance/search?q={}",
        ticker
    );
    let client = match reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
        .build()
    {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let Ok(response) = client.get(&url).send().await else {
        return vec![];
    };
    
    let Ok(json): Result<serde_json::Value, _> = response.json().await else {
        return vec![];
    };

    let mut news_items = Vec::new();

    if let Some(news_array) = json["news"].as_array() {
        for item in news_array.iter().take(10) {
            let title = item["title"].as_str().unwrap_or_default();
            let link = item["link"].as_str().unwrap_or_default();
            
            let json_summary = item["summary"].as_str().unwrap_or_default();

            // Compute the final summary String cleanly without mutation
            let summary: String = if !link.is_empty() {
                let scraped_text = scrape_article_body(&client, link).await.unwrap_or_default();
                if !scraped_text.is_empty() {
                    scraped_text
                } else {
                    json_summary.to_string()
                }
            } else {
                json_summary.to_string()
            };

            if !title.is_empty() {
                news_items.push(ArticleData {
                    title: title.to_string(),
                    body_snippet: summary.to_string(),
                });
            }
        }
    }

    news_items
}

async fn scrape_article_body(client: &reqwest::Client, url: &str) -> Option<String> {
    let Ok(resp) = client.get(url).send().await else {
        return None;
    };
    let Ok(html_text) = resp.text().await else {
        return None;
    };

    let document = Html::parse_document(&html_text);
    let p_selector = Selector::parse("p").unwrap();

    let mut full_text = String::new();

    // Extract paragraph text up to 800 chars to avoid exceeding LLM context limits
    for element in document.select(&p_selector) {
        let paragraph = element.text().collect::<Vec<_>>().join(" ");
        full_text.push_str(&paragraph);
        full_text.push(' ');

        if full_text.len() > 800 {
            break;
        }
    }

    Some(full_text.trim().to_string())
}

pub async fn _fetch_ticker_headlines(
    ticker: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    // This endpoint returns both ticker quotes and recent news articles
    let url = format!(
        "https://query2.finance.yahoo.com/v1/finance/search?q={}",
        ticker
    );

    // We use the same browser-spoofing client to bypass basic bot-blocking
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36")
        .build()?;

    let response = client.get(&url).send().await?;

    if !response.status().is_success() {
        return Err(format!("Failed to fetch news: HTTP {}", response.status()).into());
    }

    let json: serde_json::Value = response.json().await?;
    let mut headlines = Vec::new();

    // Safely extract the "news" array. If it doesn't exist, this cleanly skips.
    if let Some(news_array) = json["news"].as_array() {
        for article in news_array {
            if let Some(title) = article["title"].as_str() {
                headlines.push(title.to_string());
            }
        }
    }

    Ok(headlines)
}

pub async fn fetch_yahoo_market_data(
    ticker: &str,
    days: u16,
) -> Result<Vec<Candle>, Box<dyn Error>> {
    // We use the 1-day interval and specify how many days of history we need.
    // E.g., range=250d gives us roughly one year of trading days.
    let url = format!(
        "https://query1.finance.yahoo.com/v8/finance/chart/{}?interval=1d&range={}d",
        ticker, days
    );

    // Yahoo Finance blocks default HTTP clients. We must spoof a standard browser.
    let client = Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36")
        .build()?;

    let response = client.get(&url).send().await?;

    if !response.status().is_success() {
        return Err(format!(
            "Failed to fetch data for {}: HTTP {}",
            ticker,
            response.status()
        )
        .into());
    }

    let json: Value = response.json().await?;
    let result = &json["chart"]["result"][0];

    let timestamps = result["timestamp"]
        .as_array()
        .ok_or("No timestamp data found")?;
    let quote = &result["indicators"]["quote"][0];

    let mut candles = Vec::new();

    // Iterate through the arrays and build our Candle structs
    for i in 0..timestamps.len() {
        // We use pattern matching to safely extract f64 values.
        // If Yahoo returns 'null' for a halted trading day, this cleanly skips that day
        // rather than crashing your entire trading engine.
        if let (Some(open), Some(high), Some(low), Some(close), Some(volume)) = (
            quote["open"][i].as_f64(),
            quote["high"][i].as_f64(),
            quote["low"][i].as_f64(),
            quote["close"][i].as_f64(),
            quote["volume"][i].as_f64(),
        ) {
            candles.push(Candle {
                open,
                high,
                low,
                close,
                volume,
            });
        }
    }

    Ok(candles)
}

async fn analyze_news_sentiment_locally() -> f64 {
    let client = reqwest::Client::new();
    // let articles = fetch_ticker_news_with_content("AMAT", 3).await;
    let articles = fetch_ticker_news("AMAT").await;

    let mut formatted_news = String::new();
    for (i, article) in articles.iter().enumerate() {
        formatted_news.push_str(&format!(
            "--- Article {} ---\nTitle: {}\nSummary: {}\n\n",
            i + 1,
            article.title,
            if article.body_snippet.is_empty() {
                "No body content available"
            } else {
                &article.body_snippet
            }
        ));
    }

    let prompt = format!(
        r#"Analyze the sentiment for Applied Materials (AMAT) based on these full news excerpts.
        Respond ONLY in JSON matching this schema:
        {{
        "reasoning": "Explanation of sentiment findings",
        "sentiment_score": float between -1.0 and 1.0
        }}

        News Data:
        {formatted_news}"#
    );

    let payload = serde_json::json!({
        "model": "gemma4:e4b", //"llama3.2:3b",
        "prompt": prompt,
        "format": "json",
        "stream": false
    });

    println!("the frigging payload: {:?}", payload);

    match client
        .post("http://localhost:11434/api/generate")
        .json(&payload)
        .timeout(Duration::from_secs(60))
        .send()
        .await
    {
        Ok(res) => {
            if let Ok(json) = res.json::<serde_json::Value>().await {
                if let Some(text) = json.get("response") {
                    if let Ok(parsed) = serde_json::from_str::<OllamaSentimentResponse>(
                        text.as_str().unwrap_or("{}"),
                    ) {
                        println!("the frigging value: {:?}", parsed);
                        return parsed.sentiment_score;
                    }
                }
            }
            0.0
        }
        Err(_) => 0.0,
    }
}

fn evaluate_trading_rules(metrics: &EngineMetrics, config: &mut TickerConfig) -> String {
    // We define the macro trend for both states
    let is_macro_uptrend =
        metrics.current_price > metrics.ema_50 && metrics.ema_50 > metrics.ema_200;

    // ==========================================
    // STATE 1: WE OWN THE STOCK (Look for Exits)
    // ==========================================
    if config.is_owned {
        // Calculate how the stock has grown or dropped since we bought it
        let roi_percentage =
            ((metrics.current_price - config.entry_price) / config.entry_price) * 100.0;

        let performance_string = format!(
            "Current ROI: {:.2}% (Entry: ${:.2} | Current: ${:.2}) | Shares Held: {:.1}",
            roi_percentage, config.entry_price, metrics.current_price, config.shares
        );

        // --- NEW: SCALED PROFIT-TAKING GATE (+30% Target Example) ---
        let profit_target_roi = 5.0; // 5% gain target
        if !config.half_profit_taken && roi_percentage >= profit_target_roi {
            let initial_shares = config.shares;
            let sell_shares = config.shares / 2.0;
            config.shares -= sell_shares;
            config.half_profit_taken = true;

            return format!(
                "\n--- 📈 AMAT EXECUTION REPORT: SCALE OUT ---\n\
                 • Action Required : Sell {:.1} shares (50% of position)\n\
                 • Target Trigger  : +{:.1}% ROI Target Met (Current: {:.2}%)\n\
                 • Price Details   : Entry @ ${:.2} | Current @ ${:.2}\n\
                 • Shares Status   : {} -> {} remaining\n\
                 • Strategy        : Securing partial profits to de-risk; leaving runner to Chandelier Exit.\n\
                 --------------------------------------------",
                sell_shares, profit_target_roi, roi_percentage, 
                config.entry_price, metrics.current_price, 
                initial_shares, config.shares
            );

            // return format!(
            //     "SCALE OUT: +5% Target Hit! Sold {:.1} shares at ${:.2}. {}\nAction: Secure partial profits, leave runner.",
            //     sell_shares, metrics.current_price, performance_string
            // );
        }

        // SELL GATE 1: The Chandelier Exit (Dynamic Trailing Stop)
        if metrics.current_price < metrics.ce_long_stop {
            // Full position close, reset ownership state
            let shares_to_close = config.shares;
            config.is_owned = false;
            config.shares = 0.0;
            config.half_profit_taken = false;

            return format!(
                "\n--- 🚨 AMAT EXECUTION REPORT: CRITICAL SELL ---\n\
                 • Action Required : Close Entire Position ({:.1} shares)\n\
                 • Trigger Reason  : Price (${:.2}) dropped below Chandelier Exit floor (${:.2})\n\
                 • Final Performance: {:.2}% ROI (Entry @ ${:.2})\n\
                 • Strategy        : Mathematical uptrend broken. Capital protected.\n\
                 -------------------------------------------------",
                shares_to_close, metrics.current_price, metrics.ce_long_stop, 
                roi_percentage, config.entry_price
            );
        }

        // SELL GATE 2: Bearish Divergence (RSI vs MFI)
        if metrics.rsi > 70.0 && metrics.mfi < 50.0 {
            return format!(
                "STRATEGIC SELL: Artificial price pump on weak volume. {}\nAction: Lock in gains.",
                performance_string
            );
        }

        return format!(
            "\n--- 🛡️ AMAT STATUS REPORT: HOLD ---\n\
             • Trend Status    : Uptrend Intact\n\
             • Performance     : {:.2}% ROI (Entry @ ${:.2} | Current @ ${:.2})\n\
             • Risk Floor      : Chandelier Exit at ${:.2}\n\
             • Shares Held     : {:.1}\n\
             ------------------------------------",
            roi_percentage, config.entry_price, metrics.current_price, 
            metrics.ce_long_stop, config.shares
        );
    }
    // ==========================================
    // STATE 2: WE DO NOT OWN IT (Look for Entries)
    // ==========================================
    else {
        // BUY GATE 1: Deep Exhaustion Entry
        if is_macro_uptrend && metrics.rsi < 30.0 && metrics.mfi < 20.0 {
            return format!("STRONG BUY: Asymmetrical entry detected at ${:.2}. Deep exhaustion in a confirmed uptrend.", 
                metrics.current_price);
        }

        // BUY GATE 2: Accumulation / Pullback
        if is_macro_uptrend && metrics.rsi < 45.0 && metrics.mfi > 60.0 {
            return format!(
                "ACCUMULATE: Healthy pullback at ${:.2}. Smart money is buying.",
                metrics.current_price
            );
        }

        return format!(
            "WATCHING: No high-probability entry for {}. Price: ${:.2}, RSI: {:.1}",
            config.symbol, metrics.current_price, metrics.rsi
        );
    }
}

pub fn calculate_ema(closes: &[f64], period: usize) -> Vec<f64> {
    if closes.len() < period {
        return vec![];
    }
    let k = 2.0 / (period as f64 + 1.0);
    let mut ema = vec![0.0; closes.len()];
    let sum: f64 = closes[..period].iter().sum();
    ema[period - 1] = sum / period as f64;
    for i in period..closes.len() {
        ema[i] = closes[i] * k + ema[i - 1] * (1.0 - k);
    }
    ema
}

pub fn calculate_rsi(closes: &[f64], period: usize) -> Vec<f64> {
    if closes.len() < period + 1 {
        return vec![];
    }
    let mut rsi = vec![0.0; closes.len()];
    let mut gains = 0.0;
    let mut losses = 0.0;
    for i in 1..=period {
        let change = closes[i] - closes[i - 1];
        if change > 0.0 {
            gains += change;
        } else {
            losses += -change;
        }
    }
    let mut avg_gain = gains / period as f64;
    let mut avg_loss = losses / period as f64;
    if avg_loss == 0.0 {
        rsi[period] = 100.0;
    } else {
        let rs = avg_gain / avg_loss;
        rsi[period] = 100.0 - (100.0 / (1.0 + rs));
    }
    for i in (period + 1)..closes.len() {
        let change = closes[i] - closes[i - 1];
        let gain = if change > 0.0 { change } else { 0.0 };
        let loss = if change < 0.0 { -change } else { 0.0 };
        avg_gain = (avg_gain * (period as f64 - 1.0) + gain) / period as f64;
        avg_loss = (avg_loss * (period as f64 - 1.0) + loss) / period as f64;
        if avg_loss == 0.0 {
            rsi[i] = 100.0;
        } else {
            let rs = avg_gain / avg_loss;
            rsi[i] = 100.0 - (100.0 / (1.0 + rs));
        }
    }
    rsi
}

pub fn calculate_adx(highs: &[f64], lows: &[f64], closes: &[f64], period: usize) -> Vec<f64> {
    if closes.len() < period * 2 {
        return vec![];
    }
    let mut adx = vec![0.0; closes.len()];

    let mut plus_dm = vec![0.0; closes.len()];
    let mut minus_dm = vec![0.0; closes.len()];
    let mut tr = vec![0.0; closes.len()];

    for i in 1..closes.len() {
        let up_move = highs[i] - highs[i - 1];
        let down_move = lows[i - 1] - lows[i];

        plus_dm[i] = if up_move > down_move && up_move > 0.0 {
            up_move
        } else {
            0.0
        };
        minus_dm[i] = if down_move > up_move && down_move > 0.0 {
            down_move
        } else {
            0.0
        };

        let tr1 = highs[i] - lows[i];
        let tr2 = (highs[i] - closes[i - 1]).abs();
        let tr3 = (lows[i] - closes[i - 1]).abs();
        tr[i] = tr1.max(tr2).max(tr3);
    }

    let mut smoothed_plus_dm = vec![0.0; closes.len()];
    let mut smoothed_minus_dm = vec![0.0; closes.len()];
    let mut smoothed_tr = vec![0.0; closes.len()];

    let sum_pdm: f64 = plus_dm[1..=period].iter().sum();
    let sum_mdm: f64 = minus_dm[1..=period].iter().sum();
    let sum_tr: f64 = tr[1..=period].iter().sum();

    smoothed_plus_dm[period] = sum_pdm / period as f64;
    smoothed_minus_dm[period] = sum_mdm / period as f64;
    smoothed_tr[period] = sum_tr / period as f64;

    for i in (period + 1)..closes.len() {
        smoothed_plus_dm[i] =
            (smoothed_plus_dm[i - 1] * (period as f64 - 1.0) + plus_dm[i]) / period as f64;
        smoothed_minus_dm[i] =
            (smoothed_minus_dm[i - 1] * (period as f64 - 1.0) + minus_dm[i]) / period as f64;
        smoothed_tr[i] = (smoothed_tr[i - 1] * (period as f64 - 1.0) + tr[i]) / period as f64;
    }

    let mut plus_di = vec![0.0; closes.len()];
    let mut minus_di = vec![0.0; closes.len()];
    for i in period..closes.len() {
        if smoothed_tr[i] == 0.0 {
            plus_di[i] = 0.0;
            minus_di[i] = 0.0;
        } else {
            plus_di[i] = (smoothed_plus_dm[i] / smoothed_tr[i]) * 100.0;
            minus_di[i] = (smoothed_minus_dm[i] / smoothed_tr[i]) * 100.0;
        }
    }

    let mut dx = vec![0.0; closes.len()];
    for i in period..closes.len() {
        let sum_di = plus_di[i] + minus_di[i];
        if sum_di == 0.0 {
            dx[i] = 0.0;
        } else {
            dx[i] = ((plus_di[i] - minus_di[i]).abs() / sum_di) * 100.0;
        }
    }

    if !dx.is_empty() {
        let last_dx = *dx.last().unwrap();
        adx[closes.len() - 1] = last_dx;
    }

    adx
}

pub fn analyze_stock(
    stock_data: &[StockData],
    macro_context: &crate::macros::themes::MacroContext,
) -> AnalysisResult {
    if stock_data.len() < 50 {
        return AnalysisResult {
            signal: Signal::Hold,
            confidence: 0.0,
            reason: "Insufficient data".to_string(),
            themes: vec![],
        };
    }

    let closes: Vec<f64> = stock_data.iter().map(|s| s.close).collect();
    let highs: Vec<f64> = stock_data.iter().map(|s| s.high).collect();
    let lows: Vec<f64> = stock_data.iter().map(|s| s.low).collect();

    let ema_12 = calculate_ema(&closes, 12);
    let ema_26 = calculate_ema(&closes, 26);
    let rsi = calculate_rsi(&closes, 14);
    let adx = calculate_adx(&highs, &lows, &closes, 14);

    let current_ema_12 = ema_12.last().unwrap_or(&0.0);
    let current_ema_26 = ema_26.last().unwrap_or(&0.0);
    let prev_ema_12 = ema_12.get(ema_12.len() - 2).unwrap_or(&0.0);
    let prev_ema_26 = ema_26.get(ema_26.len() - 2).unwrap_or(&0.0);

    let current_rsi = rsi.last().unwrap_or(&50.0);
    let current_adx = adx.last().unwrap_or(&0.0);

    let mut score = 0.0;
    let mut reasons = Vec::new();

    let golden_cross = *prev_ema_12 <= *prev_ema_26 && *current_ema_12 > *current_ema_26;
    let death_cross = *prev_ema_12 >= *prev_ema_26 && *current_ema_12 < *current_ema_26;

    if golden_cross {
        score += 2.0;
        reasons.push("Golden Cross (Bullish EMA Crossover)");
    } else if death_cross {
        score -= 2.0;
        reasons.push("Death Cross (Bearish EMA Crossover)");
    }

    if *current_rsi < 30.0 {
        score += 1.5;
        reasons.push("Oversold (RSI < 30)");
    } else if *current_rsi > 70.0 {
        score -= 1.5;
        reasons.push("Overbought (RSI > 70)");
    }

    if *current_adx > 25.0 {
        if *current_ema_12 > *current_ema_26 {
            score += 1.0;
            reasons.push("Strong Uptrend (ADX > 25)");
        } else {
            score -= 1.0;
            reasons.push("Strong Downtrend (ADX > 25)");
        }
    }

    if macro_context.is_theme_active(&Theme::Defense) {
        reasons.push("Macro: Defense Sector Active");
    }

    let mut signal = Signal::Hold;
    let mut confidence = 0.5;

    if score >= 3.0 {
        signal = Signal::StrongBuy;
        confidence = 0.9;
    } else if score >= 1.5 {
        signal = Signal::Buy;
        confidence = 0.7;
    } else if score <= -3.0 {
        signal = Signal::StrongSell;
        confidence = 0.9;
    } else if score <= -1.5 {
        signal = Signal::Sell;
        confidence = 0.7;
    }

    let reason = reasons.join(", ");

    AnalysisResult {
        signal,
        confidence,
        reason,
        themes: macro_context.active_themes.clone(),
    }
}

#[derive(Debug, Clone)]
pub struct PositionState {
    pub is_active: bool,
    pub entry_price: f64,
    pub shares: f64,
    pub half_profit_taken: bool, // New flag to track if we've scaled out
}

#[derive(Debug, Clone)]
pub struct TradingConfig {
    pub chandelier_atr_multiplier: f64,
    pub profit_target_roi: f64, // e.g., 0.30 for +30%
}
