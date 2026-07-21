mod ai;
mod analysis;
mod auth;
mod backtest;
mod data;
mod db;
mod finnhub_extra;
mod indicators;
mod macros;
mod news;
mod notifier;
mod ollama;
mod pipeline;
mod portfolio;
mod ratelimit;
mod risk;
mod routes;
mod scanner;
mod state;
mod strategy;
mod themes;

use crate::data::twelvedata::TwelveDataProvider;
use crate::ratelimit::RateLimiter;
use crate::strategy::StrategyConfig;
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
use std::env;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

fn env_var<T: std::str::FromStr>(name: &str, default: T) -> T {
    env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() {
    dotenv().ok();

    let ticker = analysis::technical::TickerConfig {
        symbol: "AMAT".to_string(),
        is_owned: true,      // Switched to true to demonstrate active trade management
        entry_price: 415.00, // Example of a past successful Alpha Matrix entry
        highest_tracked_price: 739.67, // The actual 52-week high for AMAT to measure the drawdown
        current_price: 525.70, // The current active price
        rsi: 42.5, // Reflects the recent price drop (cooling off, but not yet < 30 oversold)
        sentiment: 0.85, // Highly positive based on strong Wall Street consensus ratings
        latest_signal: "HOLD: No high-probability setup detected.".to_string(),
        shares: 100.0,
        half_profit_taken: false,
    };
    analysis::technical::run_orchestrator(ticker).await;

    tracing_subscriber::fmt()
        .with_env_filter(
            env::var("RUST_LOG").unwrap_or_else(|_| "stock_sentinel=info,tower_http=info".into()),
        )
        .init();

    // Shared across FinnhubProvider and FinnhubExtras so the free-tier
    // 60-calls/min cap is respected across *all* Finnhub traffic, not
    // per-client. Override via FINNHUB_RATE_LIMIT_PER_MIN if you're on a
    // paid tier with a higher cap.
    let finnhub_rate_limit: usize = env_var("FINNHUB_RATE_LIMIT_PER_MIN", 55);
    let finnhub_limiter = Arc::new(ratelimit::RateLimiter::new(
        finnhub_rate_limit,
        Duration::from_secs(60),
    ));

    let finnhub_key = env::var("FINNHUB_API_KEY").ok().filter(|k| !k.is_empty());

    let provider: Box<dyn data::MarketDataProvider> = match &finnhub_key {
        Some(key) => {
            tracing::info!("using FinnhubProvider (rate limit {finnhub_rate_limit}/min)");
            Box::new(FinnhubProvider::new(
                key.clone(),
                Arc::clone(&finnhub_limiter),
            ))
        }
        None => {
            tracing::warn!(
                "FINNHUB_API_KEY not set - falling back to MockProvider (fake data, dev only)"
            );
            Box::new(MockProvider)
        }
    };

    let extras: Option<Arc<finnhub_extra::FinnhubExtras>> = finnhub_key.as_ref().map(|key| {
        tracing::info!(
            "Finnhub extras enabled: structured news, earnings calendar, analyst consensus"
        );
        Arc::new(finnhub_extra::FinnhubExtras::new(
            key.clone(),
            Arc::clone(&finnhub_limiter),
        ))
    });

    // Twelve Data: free-tier alternative for real OHLC history, since
    // Finnhub's /stock/candle is premium-only (confirmed against the
    // swagger export earlier in this project). Optional - everything
    // works without it, just with the close-only ATR approximation.
    let twelvedata_rate_limit: usize = env_var("TWELVEDATA_RATE_LIMIT_PER_MIN", 8);
    let twelvedata: Option<Arc<TwelveDataProvider>> = std::env::var("TWELVEDATA_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
        .map(|key| {
            tracing::info!("Twelve Data backfill enabled (rate limit {twelvedata_rate_limit}/min) - POST /backfill/:symbol for real OHLC + ATR");
            let limiter = Arc::new(RateLimiter::new(twelvedata_rate_limit, Duration::from_secs(60)));
            Arc::new(TwelveDataProvider::new(key, limiter))
        });
    if twelvedata.is_none() {
        tracing::info!("TWELVEDATA_API_KEY not set - ATR stays approximated from closes; /backfill unavailable");
    }

    let notifier = notifier::init();

    let ollama_base =
        env::var("OLLAMA_BASE_URL").unwrap_or_else(|_| "http://localhost:11434".into());
    let ollama_model = env::var("OLLAMA_MODEL").unwrap_or_else(|_| "llama3.2:3b".into());
    let llm_concurrency: usize = env_var("LLM_CONCURRENCY", 2);
    tracing::info!(
        "using Ollama at {ollama_base} with model {ollama_model} (concurrency {llm_concurrency})"
    );
    let llm = Arc::new(OllamaClient::new(ollama_base, ollama_model));
    let news: Arc<dyn news::NewsProvider> = Arc::new(GoogleNewsRssProvider::new());
    // Shared LLM concurrency gate between the per-symbol pipeline and the
    // theme watcher - one local model, one real bottleneck, regardless of
    // which feature is asking it for something.
    let llm_semaphore = Arc::new(Semaphore::new(llm_concurrency.max(1)));
    let pipeline = Pipeline::new(
        Arc::clone(&news),
        Arc::clone(&llm),
        Arc::clone(&llm_semaphore),
        extras.clone(),
    );
    let theme_watcher = themes::ThemeWatcher::new(news, llm, llm_semaphore);

    let db_path = env::var("DATABASE_PATH").unwrap_or_else(|_| "stock-sentinel.db".into());
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

    let portfolio_drawdown_limit_pct: f64 = env_var("MAX_PORTFOLIO_DRAWDOWN_PCT", 15.0);
    tracing::info!(
        "portfolio circuit breaker: trips at {portfolio_drawdown_limit_pct:.1}% aggregate drawdown"
    );

    // Strategy is now genuinely configurable via env, not hardcoded at
    // every call site - tune it without a recompile.
    let default_strategy = StrategyConfig {
        trailing_stop_pct: env_var("TRAILING_STOP_PCT", 15.0),
        take_profit_ladder: vec![(30.0, 0.25), (60.0, 0.25), (100.0, 0.25)],
        rsi_overbought: env_var("RSI_OVERBOUGHT", 75.0),
        rsi_period: env_var("RSI_PERIOD", 14),
        atr_stop_multiplier: std::env::var("ATR_STOP_MULTIPLIER")
            .ok()
            .and_then(|v| v.parse().ok()),
    };
    tracing::info!(
        "default strategy: trailing_stop={:.1}%, rsi_overbought={:.0}, atr_stop_multiplier={:?}",
        default_strategy.trailing_stop_pct,
        default_strategy.rsi_overbought,
        default_strategy.atr_stop_multiplier
    );

    let state = Arc::new(AppState {
        positions,
        provider,
        pipeline,
        db,
        extras,
        notifier,
        theme_watcher,
        twelvedata,
        circuit_breaker: Arc::new(AtomicBool::new(false)),
        portfolio_drawdown_limit_pct,
        default_strategy,
    });

    let scan_interval_secs: u64 = env_var("SCAN_INTERVAL_SECS", 15 * 60);
    let max_concurrent_scans: usize = env_var("MAX_CONCURRENT_SCANS", 8);
    let scanner_cfg = ScannerConfig {
        interval: Duration::from_secs(scan_interval_secs),
        max_concurrent_scans,
    };
    tracing::info!(
        "scanner: every {scan_interval_secs}s, up to {max_concurrent_scans} tickers in parallel"
    );

    // Graceful scanner shutdown: a watch channel rather than abort(), so
    // the scanner task gets to notice the shutdown signal and return
    // cleanly (finishing whatever it was doing gets interrupted at the
    // next `select!` point, but the task itself is properly joined
    // rather than yanked out from under itself).
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let scanner_handle = tokio::spawn(scanner::run_scanner_loop(
        Arc::clone(&state),
        scanner_cfg,
        shutdown_rx,
    ));

    // Auth: bearer token if API_AUTH_TOKEN is set, otherwise wide open.
    // This is going on a public cloud box per the original ask, so an
    // unset token gets a loud, repeated warning rather than a quiet one.
    let auth_token = env::var("API_AUTH_TOKEN").ok().filter(|t| !t.is_empty());
    let auth_state: auth::AuthState = Arc::new(auth_token.clone());
    if auth_token.is_none() {
        tracing::warn!("========================================================");
        tracing::warn!("API_AUTH_TOKEN is not set - this server has NO AUTHENTICATION.");
        tracing::warn!("Anyone who can reach it can add positions, spend your Finnhub");
        tracing::warn!("quota, and trigger Telegram alerts. Set API_AUTH_TOKEN before");
        tracing::warn!("deploying anywhere reachable from the public internet.");
        tracing::warn!("========================================================");
    } else {
        tracing::info!("API auth enabled (bearer token required, except /health)");
    }

    let app = Router::new()
        .route("/health", get(routes::health))
        .route(
            "/position",
            get(routes::list_positions).post(routes::add_position),
        )
        .route("/positions", post(routes::add_positions))
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
        .route("/history/{symbol}", get(routes::price_history))
        .route("/backfill/{symbol}", post(routes::backfill))
        .route("/risk/position-size", post(routes::position_size))
        .route("/backtest/{symbol}", post(routes::run_backtest))
        .route(
            "/backtest/{symbol}/import",
            post(routes::import_backtest_prices),
        )
        .route("/backtest/{symbol}/sweep", post(routes::run_backtest_sweep))
        .route("/portfolio/status", get(routes::portfolio_status))
        .route("/themes", get(routes::list_themes).post(routes::add_theme))
        .route("/themes/{name}", delete(routes::remove_theme))
        .route("/themes/{name}/history", get(routes::theme_history))
        .layer(axum::middleware::from_fn_with_state(
            auth_state.clone(),
            auth::require_bearer_token,
        ))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
    tracing::info!("listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();

    // Signal the scanner and wait for it to actually finish, instead of
    // aborting it mid-cycle.
    let _ = shutdown_tx.send(true);
    if let Err(e) = scanner_handle.await {
        tracing::warn!("scanner task did not shut down cleanly: {e:?}");
    }
}

/// Waits for Ctrl+C or SIGTERM (the signal a cloud platform sends on
/// deploy/restart) so axum can stop accepting new connections and finish
/// in-flight requests instead of dropping them mid-response.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received Ctrl+C, shutting down gracefully"),
        _ = terminate => tracing::info!("received SIGTERM, shutting down gracefully"),
    }
}
