mod data;
mod db;
mod finnhub_extra;
mod indicators;
mod news;
mod ollama;
mod pipeline;
mod portfolio;
mod routes;
mod scanner;
mod state;
mod strategy;

use axum::routing::{delete, get, post};
use axum::Router;
use data::{FinnhubProvider, MockProvider};
use db::Db;
use dotenv::dotenv;
use news::GoogleNewsRssProvider;
use ollama::OllamaClient;
use pipeline::Pipeline;
use scanner::ScannerConfig;
use state::AppState;
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

#[tokio::main]
async fn main() {
    dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "stock_sentinel=info,tower_http=info".into()),
        )
        .init();

    let provider: Box<dyn data::MarketDataProvider> = match std::env::var("FINNHUB_API_KEY") {
        Ok(key) if !key.is_empty() => {
            tracing::info!("using FinnhubProvider");
            Box::new(FinnhubProvider::new(key))
        }
        _ => {
            tracing::warn!(
                "FINNHUB_API_KEY not set - falling back to MockProvider (fake data, dev only)"
            );
            Box::new(MockProvider)
        }
    };

    let extras: Option<Arc<finnhub_extra::FinnhubExtras>> = match std::env::var("FINNHUB_API_KEY") {
        Ok(key) if !key.is_empty() => {
            tracing::info!(
                "Finnhub extras enabled: structured news, earnings calendar, analyst consensus"
            );
            Some(Arc::new(finnhub_extra::FinnhubExtras::new(key)))
        }
        _ => None,
    };

    let ollama_base =
        std::env::var("OLLAMA_BASE_URL").unwrap_or_else(|_| "http://localhost:11434".into());
    let ollama_model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "llama3.2:3b".into());
    let llm_concurrency: usize = std::env::var("LLM_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    tracing::info!(
        "using Ollama at {ollama_base} with model {ollama_model} (concurrency {llm_concurrency})"
    );
    let llm = Arc::new(OllamaClient::new(ollama_base, ollama_model));
    let news = Arc::new(GoogleNewsRssProvider::new());
    let pipeline = Pipeline::new(news, llm, llm_concurrency, extras.clone());

    let db_path = std::env::var("DATABASE_PATH").unwrap_or_else(|_| "stock-sentinel.db".into());
    let db = Db::open(&db_path).expect("failed to open sqlite database");

    // Hydrate the in-memory cache from disk so positions survive a restart.
    let positions: dashmap::DashMap<String, portfolio::Position> = Default::default();
    match db.load_all_positions().await {
        Ok(loaded) => {
            tracing::info!("loaded {} position(s) from {db_path}", loaded.len());
            for p in loaded {
                positions.insert(p.symbol.clone(), p);
            }
        }
        Err(e) => tracing::error!("failed to load positions from sqlite: {e:?}"),
    }

    let state = Arc::new(AppState {
        positions,
        provider,
        pipeline,
        db,
        extras,
    });

    let scan_interval_secs: u64 = std::env::var("SCAN_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(15 * 60);
    let max_concurrent_scans: usize = std::env::var("MAX_CONCURRENT_SCANS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    let scanner_cfg = ScannerConfig {
        interval: Duration::from_secs(scan_interval_secs),
        max_concurrent_scans,
    };
    tracing::info!(
        "scanner: every {scan_interval_secs}s, up to {max_concurrent_scans} tickers in parallel"
    );
    tokio::spawn(scanner::run_scanner_loop(Arc::clone(&state), scanner_cfg));

    let app = Router::new()
        .route("/health", get(routes::health))
        .route(
            "/positions",
            get(routes::list_positions).post(routes::add_position),
        )
        .route("/positions/{symbol}", delete(routes::remove_position))
        .route("/positions/{symbol}/signal", get(routes::get_signal))
        .route(
            "/positions/{symbol}/full-signal",
            get(routes::get_full_signal),
        )
        .route("/candidates/evaluate", post(routes::evaluate_candidates))
        .route(
            "/watchlist",
            get(routes::list_watchlist).post(routes::add_watchlist),
        )
        .route("/watchlist/{symbol}", delete(routes::remove_watchlist))
        .route("/evaluations/{symbol}", get(routes::evaluation_history))
        .route("/fundamentals/{symbol}", get(routes::fundamentals))
        .route("/peers/{symbol}", get(routes::peers))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
    tracing::info!("listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
