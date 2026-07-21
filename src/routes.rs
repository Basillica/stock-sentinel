use crate::pipeline::PipelineResult;
use crate::portfolio::Position;
use crate::state::AppState;
use crate::strategy::{evaluate, StrategyConfig};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub async fn health() -> &'static str {
    "ok"
}

#[derive(Deserialize)]
pub struct AddPositionRequest {
    pub symbol: String,
    pub entry_price: f64,
    pub quantity: f64,
}

pub async fn add_position(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AddPositionRequest>,
) -> Result<Json<Position>, StatusCode> {
    let symbol = req.symbol.to_uppercase();
    let pos = Position::new(symbol.clone(), req.entry_price, req.quantity);
    state
        .db
        .upsert_position(pos.clone())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    state.positions.insert(symbol, pos.clone());
    Ok(Json(pos))
}

pub async fn add_positions(
    State(state): State<Arc<AppState>>,
    Json(req): Json<Vec<AddPositionRequest>>,
) -> Result<Json<Vec<Position>>, StatusCode> {
    let mut pos = vec![];
    for position in req {
        let symbol = position.symbol.to_uppercase();
        let p = Position::new(symbol.clone(), position.entry_price, position.quantity);
        state
            .db
            .upsert_position(p.clone())
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        state.positions.insert(symbol, p.clone());
        pos.insert(pos.len(), p);
    }

    Ok(Json(pos))
}

pub async fn list_positions(State(state): State<Arc<AppState>>) -> Json<Vec<Position>> {
    Json(state.positions.iter().map(|e| e.value().clone()).collect())
}

pub async fn remove_position(
    State(state): State<Arc<AppState>>,
    Path(symbol): Path<String>,
) -> StatusCode {
    let symbol = symbol.to_uppercase();
    let existed = state.positions.remove(&symbol).is_some();
    let _ = state.db.delete_position(symbol).await;
    if existed {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

/// Fast path: pure technical rules, no network calls beyond the price quote.
/// Good for a tight polling loop.
pub async fn get_signal(
    State(state): State<Arc<AppState>>,
    Path(symbol): Path<String>,
) -> Result<Json<crate::strategy::Evaluation>, StatusCode> {
    let symbol = symbol.to_uppercase();
    let quote = state
        .provider
        .quote(&symbol)
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    let mut entry = state
        .positions
        .get_mut(&symbol)
        .ok_or(StatusCode::NOT_FOUND)?;
    entry.record_price(quote.price);

    let cfg = state.default_strategy.clone();
    let real_atr = state
        .db
        .latest_real_atr(symbol.clone(), 14)
        .await
        .ok()
        .flatten();
    let eval = evaluate(&entry, quote.price, &cfg, real_atr);
    Ok(Json(eval))
}

/// Full path: technical rules + news + local LLM, run through the decision
/// pipeline state machine. Slower (network + model inference) - the
/// background scanner uses this same pipeline on a schedule; this endpoint
/// is for an on-demand check.
pub async fn get_full_signal(
    State(state): State<Arc<AppState>>,
    Path(symbol): Path<String>,
) -> Result<Json<PipelineResult>, StatusCode> {
    let symbol = symbol.to_uppercase();
    let quote = state
        .provider
        .quote(&symbol)
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    let snapshot = {
        let mut entry = state
            .positions
            .get_mut(&symbol)
            .ok_or(StatusCode::NOT_FOUND)?;
        entry.record_price(quote.price);
        entry.clone()
    };

    let cfg = state.default_strategy.clone();
    let real_atr = state
        .db
        .latest_real_atr(symbol.clone(), 14)
        .await
        .ok()
        .flatten();
    let result = state
        .pipeline
        .run_position(&snapshot, quote.price, &cfg, real_atr)
        .await;
    let _ = state.db.log_evaluation(&result).await;
    Ok(Json(result))
}

#[derive(Deserialize)]
pub struct BackfillRequest {
    pub days: Option<usize>,
}

/// Pulls real daily OHLC history from Twelve Data (if configured), stores
/// it, and seeds `price_history` closes from it too - so RSI/SMA/MACD get
/// an immediate head start instead of waiting on live polling to
/// accumulate, and a real ATR becomes available for `atr_stop_multiplier`.
pub async fn backfill(
    State(state): State<Arc<AppState>>,
    Path(symbol): Path<String>,
    Json(req): Json<BackfillRequest>,
) -> Result<Json<usize>, StatusCode> {
    let provider = state
        .twelvedata
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let symbol = symbol.to_uppercase();
    let bars = provider
        .daily_history(&symbol, req.days.unwrap_or(100))
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    if bars.is_empty() {
        return Err(StatusCode::BAD_GATEWAY);
    }
    let closes: Vec<f64> = bars.iter().map(|b| b.close).collect();
    let count = bars.len();
    state
        .db
        .upsert_ohlc_bars(symbol.clone(), bars)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let _ = state.db.import_price_series(symbol, closes).await;
    Ok(Json(count))
}

#[derive(Deserialize)]
pub struct EvaluateCandidatesRequest {
    pub symbols: Vec<String>,
}

/// "What should I buy?" - one-off evaluation of an arbitrary symbol list.
/// The scanner's watchlist scan (below) is the always-on version of this.
pub async fn evaluate_candidates(
    State(state): State<Arc<AppState>>,
    Json(req): Json<EvaluateCandidatesRequest>,
) -> Json<Vec<PipelineResult>> {
    let mut results = Vec::new();
    for symbol in req.symbols {
        let symbol = symbol.to_uppercase();
        let mut history = state
            .db
            .recent_prices(symbol.clone(), 60)
            .await
            .unwrap_or_default();
        if let Ok(q) = state.provider.quote(&symbol).await {
            let _ = state.db.record_price(symbol.clone(), q.price).await;
            history.push(q.price);
        }
        let result = state.pipeline.run_candidate(&symbol, &history).await;
        let tripped = state
            .circuit_breaker
            .load(std::sync::atomic::Ordering::SeqCst);
        results.push(crate::pipeline::apply_circuit_breaker(result, tripped));
    }
    Json(results)
}

#[derive(Deserialize)]
pub struct WatchlistRequest {
    pub symbol: String,
}

// #[derive(Serialize)]
// pub struct WatchlistEntry {
//     pub symbol: String,
// }

/// Add a symbol to the always-scanned watchlist. Picked up by the scanner
/// on its next cycle - no restart needed.
pub async fn add_watchlist(
    State(state): State<Arc<AppState>>,
    Json(req): Json<WatchlistRequest>,
) -> Result<StatusCode, StatusCode> {
    state
        .db
        .add_watchlist_symbol(req.symbol.to_uppercase())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::CREATED)
}

pub async fn list_watchlist(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<String>>, StatusCode> {
    state
        .db
        .list_watchlist()
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

pub async fn remove_watchlist(
    State(state): State<Arc<AppState>>,
    Path(symbol): Path<String>,
) -> StatusCode {
    match state
        .db
        .remove_watchlist_symbol(symbol.to_uppercase())
        .await
    {
        Ok(_) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

pub async fn fundamentals(
    State(state): State<Arc<AppState>>,
    Path(symbol): Path<String>,
) -> Result<Json<crate::finnhub_extra::BasicFinancials>, StatusCode> {
    let extras = state
        .extras
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    extras
        .basic_financials(&symbol.to_uppercase())
        .await
        .map(Json)
        .map_err(|_| StatusCode::BAD_GATEWAY)
}

pub async fn peers(
    State(state): State<Arc<AppState>>,
    Path(symbol): Path<String>,
) -> Result<Json<Vec<String>>, StatusCode> {
    let extras = state
        .extras
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    extras
        .peers(&symbol.to_uppercase())
        .await
        .map(Json)
        .map_err(|_| StatusCode::BAD_GATEWAY)
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    pub limit: Option<usize>,
}

/// Price history for charting / trend-following - whatever's accumulated
/// in sqlite for this symbol, whether it came from live polling or a
/// backtest import.
pub async fn price_history(
    State(state): State<Arc<AppState>>,
    Path(symbol): Path<String>,
    axum::extract::Query(q): axum::extract::Query<HistoryQuery>,
) -> Result<Json<Vec<f64>>, StatusCode> {
    state
        .db
        .recent_prices(symbol.to_uppercase(), q.limit.unwrap_or(200))
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// Fixed-fractional position sizing - how many shares to buy given your
/// account size, risk tolerance, and where your stop actually sits.
pub async fn position_size(
    Json(req): Json<crate::risk::PositionSizeRequest>,
) -> Json<crate::risk::PositionSizeResponse> {
    Json(crate::risk::calculate(&req))
}

#[derive(Deserialize)]
pub struct ImportPricesRequest {
    /// Chronological, oldest first - e.g. pasted daily closes from a CSV.
    pub prices: Vec<f64>,
}

pub async fn import_backtest_prices(
    State(state): State<Arc<AppState>>,
    Path(symbol): Path<String>,
    Json(req): Json<ImportPricesRequest>,
) -> Result<Json<usize>, StatusCode> {
    state
        .db
        .import_price_series(symbol.to_uppercase(), req.prices)
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[derive(Deserialize)]
pub struct BacktestRequest {
    pub entry_price: f64,
    pub trailing_stop_pct: Option<f64>,
    pub take_profit_ladder: Option<Vec<(f64, f64)>>,
    /// How much stored history to replay - defaults to everything available.
    pub limit: Option<usize>,
}

/// Replay the trailing-stop / take-profit rules against whatever price
/// history is stored for this symbol (live-accumulated or imported via
/// `import_backtest_prices`) before you trust those thresholds with money.
pub async fn run_backtest(
    State(state): State<Arc<AppState>>,
    Path(symbol): Path<String>,
    Json(req): Json<BacktestRequest>,
) -> Result<Json<crate::backtest::BacktestResult>, StatusCode> {
    let prices = state
        .db
        .recent_prices(symbol.to_uppercase(), req.limit.unwrap_or(10_000))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if prices.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }
    let mut cfg = crate::strategy::StrategyConfig::default();
    if let Some(pct) = req.trailing_stop_pct {
        cfg.trailing_stop_pct = pct;
    }
    if let Some(ladder) = req.take_profit_ladder {
        cfg.take_profit_ladder = ladder;
    }
    Ok(Json(crate::backtest::run(&prices, req.entry_price, &cfg)))
}

#[derive(Deserialize)]
pub struct SweepRequest {
    pub entry_price: f64,
    pub stop_pcts: Vec<f64>,
    /// Defaults to [no ladder, a standard 25/25/25 ladder] if omitted.
    pub ladders: Option<Vec<Vec<(f64, f64)>>>,
    pub limit: Option<usize>,
}

pub async fn run_backtest_sweep(
    State(state): State<Arc<AppState>>,
    Path(symbol): Path<String>,
    Json(req): Json<SweepRequest>,
) -> Result<Json<Vec<crate::backtest::SweepResult>>, StatusCode> {
    let prices = state
        .db
        .recent_prices(symbol.to_uppercase(), req.limit.unwrap_or(10_000))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if prices.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }
    let ladders = req
        .ladders
        .unwrap_or_else(|| vec![vec![], vec![(30.0, 0.25), (60.0, 0.25), (100.0, 0.25)]]);
    Ok(Json(crate::backtest::sweep(
        &prices,
        req.entry_price,
        &req.stop_pcts,
        &ladders,
    )))
}

#[derive(Serialize)]
pub struct PortfolioStatus {
    pub current_value: f64,
    pub peak_value: f64,
    pub drawdown_pct: f64,
    pub tripped: bool,
    pub limit_pct: f64,
}

pub async fn portfolio_status(
    State(state): State<Arc<AppState>>,
) -> Result<Json<PortfolioStatus>, StatusCode> {
    let drawdown = state
        .db
        .portfolio_drawdown()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let (current_value, peak_value, drawdown_pct) = drawdown.unwrap_or((0.0, 0.0, 0.0));
    Ok(Json(PortfolioStatus {
        current_value,
        peak_value,
        drawdown_pct,
        tripped: state
            .circuit_breaker
            .load(std::sync::atomic::Ordering::SeqCst),
        limit_pct: state.portfolio_drawdown_limit_pct,
    }))
}

#[derive(Deserialize)]
pub struct AddThemeRequest {
    pub name: String,
    pub keywords: Vec<String>,
    pub symbols: Vec<String>,
}

pub async fn add_theme(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AddThemeRequest>,
) -> Result<StatusCode, StatusCode> {
    state
        .db
        .add_theme(req.name, req.keywords, req.symbols)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::CREATED)
}

#[derive(Serialize)]
pub struct ThemeEntry {
    pub name: String,
    pub keywords: Vec<String>,
    pub symbols: Vec<String>,
}

pub async fn list_themes(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<ThemeEntry>>, StatusCode> {
    let themes = state
        .db
        .list_themes()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(
        themes
            .into_iter()
            .map(|(name, keywords, symbols)| ThemeEntry {
                name,
                keywords,
                symbols,
            })
            .collect(),
    ))
}

pub async fn remove_theme(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> StatusCode {
    match state.db.remove_theme(name).await {
        Ok(_) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[derive(Serialize)]
pub struct ThemeEventEntry {
    pub summary: String,
    pub relevance: f64,
    pub symbols: Vec<String>,
    pub recorded_at: String,
}

pub async fn theme_history(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<Vec<ThemeEventEntry>>, StatusCode> {
    let rows = state
        .db
        .recent_theme_events(name, 50)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(
        rows.into_iter()
            .map(
                |(summary, relevance, symbols_csv, recorded_at)| ThemeEventEntry {
                    summary,
                    relevance,
                    symbols: symbols_csv
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect(),
                    recorded_at,
                },
            )
            .collect(),
    ))
}

#[derive(Serialize)]
pub struct EvaluationLogEntry {
    pub verdict: String,
    pub trace: serde_json::Value,
    pub recorded_at: String,
}

/// Audit trail: what did the scanner decide about this symbol recently,
/// and why? This is what makes the system explainable after the fact, not
/// just in the moment.
pub async fn evaluation_history(
    State(state): State<Arc<AppState>>,
    Path(symbol): Path<String>,
) -> Result<Json<Vec<EvaluationLogEntry>>, StatusCode> {
    let rows = state
        .db
        .recent_evaluations(symbol.to_uppercase(), 50)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let entries = rows
        .into_iter()
        .map(|(verdict, trace_json, recorded_at)| EvaluationLogEntry {
            verdict,
            trace: serde_json::from_str(&trace_json).unwrap_or(serde_json::Value::Null),
            recorded_at,
        })
        .collect();
    Ok(Json(entries))
}
