use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

const MAX_HISTORY: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub symbol: String,
    pub entry_price: f64,
    pub quantity: f64,
    /// Highest price observed since entry - the number a trailing stop is measured against.
    pub peak_price: f64,
    /// Fraction of the original quantity already sold off via take-profit rungs (0.0..1.0).
    pub realized_fraction: f64,
    #[serde(skip, default)]
    pub history: VecDeque<f64>,
    pub opened_at: DateTime<Utc>,
}

impl Position {
    pub fn new(symbol: String, entry_price: f64, quantity: f64) -> Self {
        Self {
            symbol,
            entry_price,
            quantity,
            peak_price: entry_price,
            realized_fraction: 0.0,
            history: VecDeque::from([entry_price]),
            opened_at: Utc::now(),
        }
    }

    pub fn record_price(&mut self, price: f64) {
        if price > self.peak_price {
            self.peak_price = price;
        }
        self.history.push_back(price);
        if self.history.len() > MAX_HISTORY {
            self.history.pop_front();
        }
    }

    pub fn history_vec(&self) -> Vec<f64> {
        self.history.iter().cloned().collect()
    }

    pub fn gain_pct(&self, current_price: f64) -> f64 {
        (current_price - self.entry_price) / self.entry_price * 100.0
    }

    pub fn drawdown_from_peak_pct(&self, current_price: f64) -> f64 {
        (current_price - self.peak_price) / self.peak_price * 100.0
    }
}
