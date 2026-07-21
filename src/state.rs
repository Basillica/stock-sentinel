use crate::data::twelvedata::TwelveDataProvider;
use crate::data::MarketDataProvider;
use crate::db::Db;
use crate::finnhub_extra::FinnhubExtras;
use crate::notifier::Notifier;
use crate::pipeline::Pipeline;
use crate::portfolio::Position;
use crate::strategy::StrategyConfig;
use crate::themes::ThemeWatcher;
use dashmap::DashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

pub struct AppState {
    pub positions: DashMap<String, Position>,
    pub provider: Box<dyn MarketDataProvider>,
    pub pipeline: Pipeline,
    pub db: Db,
    pub extras: Option<Arc<FinnhubExtras>>,
    pub notifier: Option<Arc<dyn Notifier>>,
    pub theme_watcher: ThemeWatcher,
    pub twelvedata: Option<Arc<TwelveDataProvider>>,
    /// True when aggregate portfolio drawdown has breached
    /// `portfolio_drawdown_limit_pct` - suppresses new Buy verdicts until
    /// it clears. Checked and updated by the scanner every cycle; read
    /// by routes for on-demand candidate evaluation too.
    pub circuit_breaker: Arc<AtomicBool>,
    pub portfolio_drawdown_limit_pct: f64,
    /// Built once from env vars at startup rather than hardcoded at every
    /// call site - see `main.rs` for the env var names. Every route and
    /// the scanner use `state.default_strategy.clone()` instead of
    /// `StrategyConfig::default()` so tuning the strategy doesn't require
    /// a recompile.
    pub default_strategy: StrategyConfig,
}
