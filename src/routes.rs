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

    let cfg = StrategyConfig::default();
    let eval = evaluate(&entry, quote.price, &cfg);
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

    let cfg = StrategyConfig::default();
    let result = state
        .pipeline
        .run_position(&snapshot, quote.price, &cfg)
        .await;
    let _ = state.db.log_evaluation(&result).await;
    Ok(Json(result))
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
        results.push(result);
    }
    Json(results)
}

#[derive(Deserialize)]
pub struct WatchlistRequest {
    pub symbol: String,
}

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
