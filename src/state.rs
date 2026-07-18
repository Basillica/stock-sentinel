use crate::data::MarketDataProvider;
use crate::db::Db;
use crate::finnhub_extra::FinnhubExtras;
use crate::pipeline::Pipeline;
use crate::portfolio::Position;
use dashmap::DashMap;
use std::sync::Arc;

pub struct AppState {
    pub positions: DashMap<String, Position>,
    pub provider: Box<dyn MarketDataProvider>,
    pub pipeline: Pipeline,
    pub db: Db,
    pub extras: Option<Arc<FinnhubExtras>>,
}
