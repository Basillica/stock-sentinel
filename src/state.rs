use crate::data::MarketDataProvider;
use crate::db::Db;
use crate::finnhub_extra::FinnhubExtras;
use crate::pipeline::Pipeline;
use crate::portfolio::Position;
use crate::telegram::TelegramNotifier;
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
    pub notifier: Option<Arc<TelegramNotifier>>,
    pub theme_watcher: ThemeWatcher,
    /// True when aggregate portfolio drawdown has breached
    /// `portfolio_drawdown_limit_pct` - suppresses new Buy verdicts until
    /// it clears. Checked and updated by the scanner every cycle; read
    /// by routes for on-demand candidate evaluation too.
    pub circuit_breaker: Arc<AtomicBool>,
    pub portfolio_drawdown_limit_pct: f64,
}
