use crate::pipeline::Verdict;
use crate::portfolio::Position;
use crate::state::AppState;
use crate::strategy::StrategyConfig;
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

pub struct ScannerConfig {
    pub interval: Duration,
    /// Caps how many tickers are being scanned at once (quote + news
    /// fetch). This is separate from - and typically larger than - the
    /// pipeline's internal LLM concurrency cap: a scan involves network
    /// I/O (parallelizes well) that only funnels down to the narrow LLM
    /// gate for the one expensive step.
    pub max_concurrent_scans: usize,
}

impl Default for ScannerConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(15 * 60),
            max_concurrent_scans: 8,
        }
    }
}

/// Runs forever: every `interval`, re-reads the current ticker list and
/// scans all of it in parallel. Because the list is re-read each cycle
/// (not captured once at startup), adding or removing a position or
/// watchlist symbol via the API takes effect on the very next tick -
/// no restart needed.
pub async fn run_scanner_loop(state: Arc<AppState>, cfg: ScannerConfig) {
    let mut ticker = tokio::time::interval(cfg.interval);
    // Fire the first scan immediately rather than waiting a full interval.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let started = std::time::Instant::now();
        match scan_once(&state, cfg.max_concurrent_scans).await {
            Ok(count) => tracing::info!("scan cycle done: {count} tickers in {:?}", started.elapsed()),
            Err(e) => tracing::error!("scan cycle failed: {e:?}"),
        }
    }
}

/// One full pass over every position and every watchlist symbol, fanned
/// out concurrently and capped by a semaphore. Returns how many tickers
/// were scanned.
pub async fn scan_once(state: &Arc<AppState>, max_concurrent: usize) -> Result<usize> {
    // Dynamic list, read fresh every cycle:
    //   - held positions come from the live in-memory cache (source of truth
    //     while the server is running)
    //   - watchlist candidates come from sqlite, since they have no other
    //     in-memory representation
    let position_symbols: Vec<String> = state.positions.iter().map(|e| e.key().clone()).collect();
    let watchlist_symbols: Vec<String> = state.db.list_watchlist().await?;
    let total = position_symbols.len() + watchlist_symbols.len();

    let scan_semaphore = Arc::new(Semaphore::new(max_concurrent.max(1)));
    let mut tasks: JoinSet<()> = JoinSet::new();

    for symbol in position_symbols {
        let state = Arc::clone(state);
        let sem = Arc::clone(&scan_semaphore);
        tasks.spawn(async move {
            // Held for the whole task, released on drop when the task ends -
            // this is what actually bounds concurrency, not the spawn itself.
            let _permit = match sem.acquire_owned().await {
                Ok(p) => p,
                Err(_) => return,
            };
            if let Err(e) = scan_position(&state, &symbol).await {
                tracing::warn!(symbol, "position scan failed: {e:?}");
            }
        });
    }

    for symbol in watchlist_symbols {
        let state = Arc::clone(state);
        let sem = Arc::clone(&scan_semaphore);
        tasks.spawn(async move {
            let _permit = match sem.acquire_owned().await {
                Ok(p) => p,
                Err(_) => return,
            };
            if let Err(e) = scan_candidate(&state, &symbol).await {
                tracing::warn!(symbol, "candidate scan failed: {e:?}");
            }
        });
    }

    // Drain results as they finish (not in spawn order) so one slow ticker
    // (e.g. a name with unusually chatty news coverage) never blocks the
    // whole cycle from reporting the tickers that finished early. A panic
    // inside one task surfaces here as an `Err` instead of taking down the
    // whole scanner.
    while let Some(res) = tasks.join_next().await {
        if let Err(e) = res {
            tracing::error!("scan task panicked: {e:?}");
        }
    }

    Ok(total)
}

async fn scan_position(state: &Arc<AppState>, symbol: &str) -> Result<()> {
    let quote = state.provider.quote(symbol).await?;
    state.db.record_price(symbol.to_string(), quote.price).await?;

    // Update the in-memory position, then release the lock before the
    // (slow, network-bound) pipeline call - DashMap guards are not meant
    // to be held across an .await.
    let snapshot: Position = {
        let mut entry = match state.positions.get_mut(symbol) {
            Some(e) => e,
            None => return Ok(()), // removed between listing and now - fine, skip
        };
        entry.record_price(quote.price);
        entry.clone()
    };

    state.db.upsert_position(snapshot.clone()).await?;

    let cfg = StrategyConfig::default();
    let result = state.pipeline.run_position(&snapshot, quote.price, &cfg).await;
    state.db.log_evaluation(&result).await?;

    match &result.verdict {
        Verdict::SellAll { reason } => tracing::warn!(symbol, "ACTION sell_all: {reason}"),
        Verdict::TrimProfit { fraction, reason } => {
            tracing::warn!(symbol, "ACTION trim_profit ({:.0}%): {reason}", fraction * 100.0)
        }
        Verdict::Watch { .. } => tracing::info!(symbol, "watch: news risk flag raised"),
        _ => {}
    }
    // TODO: this is the hook point for a real notifier - Telegram, ntfy.sh,
    // email - once you're ready to stop tailing logs for alerts.

    Ok(())
}

async fn scan_candidate(state: &Arc<AppState>, symbol: &str) -> Result<()> {
    let quote = state.provider.quote(symbol).await?;
    state.db.record_price(symbol.to_string(), quote.price).await?;

    let mut history = state.db.recent_prices(symbol.to_string(), 60).await?;
    if history.last().copied() != Some(quote.price) {
        history.push(quote.price);
    }

    let result = state.pipeline.run_candidate(symbol, &history).await;
    state.db.log_evaluation(&result).await?;

    if let Verdict::Buy { confidence } = &result.verdict {
        tracing::info!(symbol, "candidate BUY signal, confidence {confidence:.2}");
    }

    Ok(())
}
