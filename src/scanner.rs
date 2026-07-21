use crate::notifier::format_alert;
use crate::pipeline::{apply_circuit_breaker, Verdict};
use crate::portfolio::Position;
use crate::state::AppState;
use anyhow::Result;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Semaphore};
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

/// Runs until `shutdown` is signalled: every `interval`, re-reads the
/// current ticker list and scans all of it in parallel. Because the list
/// is re-read each cycle (not captured once at startup), adding or
/// removing a position or watchlist symbol via the API takes effect on
/// the very next tick - no restart needed.
///
/// Selects between the interval tick and the shutdown signal so a
/// SIGTERM/Ctrl+C doesn't have to wait out a full scan interval, and so
/// `main.rs` can `.await` this task to completion instead of aborting it
/// mid-cycle.
pub async fn run_scanner_loop(
    state: Arc<AppState>,
    cfg: ScannerConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(cfg.interval);
    // Fire the first scan immediately rather than waiting a full interval.
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let started = std::time::Instant::now();
                match scan_once(&state, cfg.max_concurrent_scans).await {
                    Ok(count) => tracing::info!("scan cycle done: {count} tickers in {:?}", started.elapsed()),
                    Err(e) => tracing::error!("scan cycle failed: {e:?}"),
                }
                if let Err(e) = scan_themes(&state).await {
                    tracing::error!("theme scan failed: {e:?}");
                }
            }
            _ = shutdown.changed() => {
                tracing::info!("scanner loop received shutdown signal, exiting cleanly");
                return;
            }
        }
    }
}

/// One full pass: positions first (their aggregate value feeds the
/// portfolio circuit breaker), then watchlist candidates (which respect
/// whatever the breaker just decided). Returns how many tickers were
/// scanned in total.
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

    // --- Phase 1: positions. Also accumulates total market value for the
    // portfolio-level circuit breaker below. ---
    let portfolio_value = Arc::new(Mutex::new(0.0_f64));
    let mut position_tasks: JoinSet<()> = JoinSet::new();
    for symbol in position_symbols {
        let state = Arc::clone(state);
        let sem = Arc::clone(&scan_semaphore);
        let portfolio_value = Arc::clone(&portfolio_value);
        position_tasks.spawn(async move {
            let _permit = match sem.acquire_owned().await {
                Ok(p) => p,
                Err(_) => return,
            };
            match scan_position(&state, &symbol).await {
                Ok(market_value) => {
                    *portfolio_value.lock().await += market_value;
                }
                Err(e) => tracing::warn!(symbol, "position scan failed: {e:?}"),
            }
        });
    }
    while let Some(res) = position_tasks.join_next().await {
        if let Err(e) = res {
            tracing::error!("position scan task panicked: {e:?}");
        }
    }

    update_circuit_breaker(state, *portfolio_value.lock().await).await;

    // --- Phase 2: watchlist candidates, gated by whatever the breaker
    // decided in phase 1. ---
    let mut candidate_tasks: JoinSet<()> = JoinSet::new();
    for symbol in watchlist_symbols {
        let state = Arc::clone(state);
        let sem = Arc::clone(&scan_semaphore);
        candidate_tasks.spawn(async move {
            let _permit = match sem.acquire_owned().await {
                Ok(p) => p,
                Err(_) => return,
            };
            if let Err(e) = scan_candidate(&state, &symbol).await {
                tracing::warn!(symbol, "candidate scan failed: {e:?}");
            }
        });
    }
    while let Some(res) = candidate_tasks.join_next().await {
        if let Err(e) = res {
            tracing::error!("candidate scan task panicked: {e:?}");
        }
    }

    Ok(total)
}

/// Records this cycle's total position value, recomputes drawdown from
/// the all-time peak, and flips the circuit breaker if the configured
/// limit is breached - alerting once on each transition, not every
/// cycle, so it doesn't spam.
async fn update_circuit_breaker(state: &Arc<AppState>, total_value: f64) {
    if total_value <= 0.0 {
        return; // no positions held - nothing to track yet
    }
    if let Err(e) = state.db.record_portfolio_equity(total_value).await {
        tracing::warn!("failed to record portfolio equity: {e:?}");
        return;
    }
    let drawdown = match state.db.portfolio_drawdown().await {
        Ok(Some(d)) => d,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!("failed to compute portfolio drawdown: {e:?}");
            return;
        }
    };
    let (current, peak, drawdown_pct) = drawdown;
    let should_trip = drawdown_pct <= -state.portfolio_drawdown_limit_pct;
    let was_tripped = state.circuit_breaker.swap(should_trip, Ordering::SeqCst);

    if should_trip && !was_tripped {
        tracing::warn!(
            "PORTFOLIO CIRCUIT BREAKER TRIPPED: {:.1}% drawdown (current {:.2}, peak {:.2}) - new buys suppressed",
            drawdown_pct, current, peak
        );
        if let Some(notifier) = &state.notifier {
            let text = format!(
                "🛑 *Portfolio circuit breaker tripped*\n{:.1}% drawdown from peak ({:.2} vs peak {:.2}). New candidate buys are suppressed until this recovers.",
                drawdown_pct, current, peak
            );
            if let Err(e) = notifier.send(&text).await {
                tracing::error!(
                    "unable to send tripper circuit breaker. error: {:?} | message: {}",
                    e,
                    text,
                )
            }
        }
    } else if !should_trip && was_tripped {
        tracing::info!(
            "portfolio circuit breaker cleared - drawdown now {:.1}%",
            drawdown_pct
        );
        if let Some(notifier) = &state.notifier {
            let text = format!(
                "✅ *Portfolio circuit breaker cleared* - drawdown now {drawdown_pct:.1}%."
            );
            if let Err(e) = notifier.send(&text).await {
                tracing::error!(
                    "unable to send tripper circuit breaker. error: {:?} | message: {}",
                    e,
                    text,
                )
            }
        }
    }
}

/// Returns this position's current market value (quantity * price) so
/// the caller can fold it into the portfolio-level total.
async fn scan_position(state: &Arc<AppState>, symbol: &str) -> Result<f64> {
    let quote = state.provider.quote(symbol).await?;
    state
        .db
        .record_price(symbol.to_string(), quote.price)
        .await?;

    // Update the in-memory position, then release the lock before the
    // (slow, network-bound) pipeline call - DashMap guards are not meant
    // to be held across an .await.
    let snapshot: Position = {
        let mut entry = match state.positions.get_mut(symbol) {
            Some(e) => e,
            None => return Ok(0.0), // removed between listing and now - fine, skip
        };
        entry.record_price(quote.price);
        entry.clone()
    };

    state.db.upsert_position(snapshot.clone()).await?;
    let market_value = snapshot.quantity * quote.price;

    let cfg = state.default_strategy.clone();
    let real_atr = state
        .db
        .latest_real_atr(symbol.to_string(), 14)
        .await
        .ok()
        .flatten();
    let result = state
        .pipeline
        .run_position(&snapshot, quote.price, &cfg, real_atr)
        .await;
    state.db.log_evaluation(&result).await?;

    match &result.verdict {
        Verdict::SellAll { reason } => tracing::warn!(symbol, "ACTION sell_all: {reason}"),
        Verdict::TrimProfit { fraction, reason } => {
            tracing::warn!(
                symbol,
                "ACTION trim_profit ({:.0}%): {reason}",
                fraction * 100.0
            )
        }
        Verdict::Watch { .. } => tracing::info!(symbol, "watch: news risk flag raised"),
        _ => {}
    }

    if let Some(notifier) = &state.notifier {
        if let Some(message) = format_alert(&symbol, &result.verdict) {
            if let Err(e) = notifier.send(&message).await {
                tracing::warn!(symbol, "failed to send alert: {e:?}");
            }
        }
    }

    Ok(market_value)
}

async fn scan_candidate(state: &Arc<AppState>, symbol: &str) -> Result<()> {
    let quote = state.provider.quote(symbol).await?;
    state
        .db
        .record_price(symbol.to_string(), quote.price)
        .await?;

    let mut history = state.db.recent_prices(symbol.to_string(), 60).await?;
    if history.last().copied() != Some(quote.price) {
        history.push(quote.price);
    }

    let result = state.pipeline.run_candidate(symbol, &history).await;
    let tripped = state.circuit_breaker.load(Ordering::SeqCst);
    let result = apply_circuit_breaker(result, tripped);
    state.db.log_evaluation(&result).await?;

    if let Verdict::Buy { confidence } = &result.verdict {
        tracing::info!(symbol, "candidate BUY signal, confidence {confidence:.2}");
    }

    if let Some(notifier) = &state.notifier {
        if let Some(message) = format_alert(&symbol, &result.verdict) {
            if let Err(e) = notifier.send(&message).await {
                tracing::warn!(symbol, "failed to send alert: {e:?}");
            }
        }
    }

    Ok(())
}

/// Checks every tracked macro theme (Rheinmetal-style "something big is
/// happening in the world" monitoring) and logs + alerts on genuinely
/// relevant findings. Sequential rather than parallel - themes are
/// expected to be few, and each one already does several news searches
/// internally, so there's no strong case for adding another concurrency
/// dimension here.
async fn scan_themes(state: &Arc<AppState>) -> Result<()> {
    let themes = state.db.list_themes().await?;
    for (name, keywords, symbols) in themes {
        let theme = crate::themes::Theme {
            name: name.clone(),
            keywords,
            symbols,
        };
        let analysis = match state.theme_watcher.check(&theme).await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(theme = %name, "theme check failed: {e:?}");
                continue;
            }
        };

        state
            .db
            .log_theme_event(
                name.clone(),
                analysis.summary.clone(),
                analysis.relevance,
                analysis.affected_symbols.clone(),
            )
            .await?;

        // Conservative threshold - most cycles should NOT alert. This is
        // explicitly tuned to only surface things the model itself rates
        // as a genuinely major, durable shift.
        if analysis.relevance >= 0.6 {
            tracing::warn!(
                theme = %name,
                "THEME ALERT (relevance {:.2}): {}",
                analysis.relevance, analysis.summary
            );
            if let Some(notifier) = &state.notifier {
                let symbols_str = if analysis.affected_symbols.is_empty() {
                    "none flagged".to_string()
                } else {
                    analysis.affected_symbols.join(", ")
                };
                let text = format!(
                    "🌍 *Theme alert: {name}* (relevance {:.2})\n{}\nConsider researching: {symbols_str}",
                    analysis.relevance, analysis.summary
                );
                if let Err(e) = notifier.send(&text).await {
                    tracing::warn!(theme = %name, "failed to send theme Telegram alert: {e:?}");
                }
            }
        }
    }
    Ok(())
}
