use crate::indicators::{atr_from_closes, rsi, sma};
use crate::portfolio::Position;
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct StrategyConfig {
    /// Sell everything if price falls this many % below its peak since entry.
    /// Used as-is unless `atr_stop_multiplier` is set AND real ATR data is
    /// available for this evaluation - see that field for why you'd want
    /// both configured rather than one or the other.
    pub trailing_stop_pct: f64,
    /// (gain_threshold_pct, fraction_to_sell) rungs, evaluated in order.
    /// e.g. [(30.0, 0.25), (60.0, 0.25)] = at +30% sell a quarter, at +60% sell another quarter.
    pub take_profit_ladder: Vec<(f64, f64)>,
    /// RSI level above which we flag "overbought, consider trimming".
    pub rsi_overbought: f64,
    pub rsi_period: usize,
    /// If set AND a real ATR value (from actual OHLC bars, not the
    /// close-only approximation) is passed into `evaluate()`, the stop
    /// distance becomes `atr_stop_multiplier * real_atr` in absolute
    /// price terms instead of a flat percentage - a 2.0 multiplier is a
    /// common starting point. This is what actually adapts the stop to
    /// how volatile the specific stock is: AMAT moving 3% on a normal day
    /// wants a wider stop than a utility stock moving 0.5%. Falls back
    /// to `trailing_stop_pct` when either piece is missing, so this is
    /// purely additive - nothing breaks if you never set it.
    pub atr_stop_multiplier: Option<f64>,
}

impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            trailing_stop_pct: 15.0,
            take_profit_ladder: vec![(30.0, 0.25), (60.0, 0.25), (100.0, 0.25)],
            rsi_overbought: 75.0,
            rsi_period: 14,
            atr_stop_multiplier: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Signal {
    Hold,
    /// Sell the whole remaining position - trailing stop breached.
    SellAll {
        reason: String,
    },
    /// Sell a fraction of the position - take-profit rung hit.
    TrimProfit {
        fraction: f64,
        reason: String,
    },
    /// Informational only - nothing mechanical fires, but worth a look.
    Alert {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct Evaluation {
    pub symbol: String,
    pub current_price: f64,
    pub entry_price: f64,
    pub peak_price: f64,
    pub gain_pct: f64,
    pub drawdown_from_peak_pct: f64,
    pub rsi: Option<f64>,
    pub sma20: Option<f64>,
    pub atr: Option<f64>,
    /// True if the stop check used a real ATR-scaled distance rather than
    /// the flat `trailing_stop_pct`.
    pub used_atr_stop: bool,
    pub signal: Signal,
}

/// Pure rule evaluation - no I/O, no network, fully unit-testable.
/// This is intentionally NOT trying to predict the future. It reacts to
/// where price already is relative to your entry and your peak, which is
/// exactly the information that would have protected the Applied Materials gain.
pub fn evaluate(
    position: &Position,
    current_price: f64,
    cfg: &StrategyConfig,
    real_atr: Option<f64>,
) -> Evaluation {
    let history = position.history_vec();
    let gain_pct = position.gain_pct(current_price);
    let drawdown = position.drawdown_from_peak_pct(current_price);
    let rsi_val = rsi(&history, cfg.rsi_period);
    let sma20 = sma(&history, 20);
    let atr = atr_from_closes(&history, 14);
    let atr_stop: Option<(f64, f64)> = match (cfg.atr_stop_multiplier, real_atr) {
        (Some(mult), Some(a)) if a > 0.0 => {
            let stop_price = position.peak_price - mult * a;
            Some((stop_price, mult))
        }
        _ => None,
    };
    let used_atr_stop = atr_stop.is_some();

    // Priority 1: trailing stop. If we've fallen too far from the peak, get out.
    let stop_triggered = if let Some((stop_price, _)) = atr_stop {
        current_price <= stop_price
    } else {
        drawdown <= -cfg.trailing_stop_pct
    };

    // Priority 1: trailing stop. If we've fallen too far from the peak, get out.
    let signal = if stop_triggered {
        let reason = if let Some((stop_price, mult)) = atr_stop {
            format!(
                "Price {current_price:.2} is at or below the ATR-scaled stop of {stop_price:.2} \
                 ({mult:.1}x ATR of {:.2} below peak {:.2}).",
                real_atr.unwrap_or(0.0),
                position.peak_price
            )
        } else {
            format!(
                "Price is {:.1}% below its peak of {:.2}, past your {:.0}% trailing stop.",
                drawdown.abs(),
                position.peak_price,
                cfg.trailing_stop_pct
            )
        };
        Signal::SellAll { reason }
    } else if let Some(&(threshold, fraction)) = cfg
        .take_profit_ladder
        .iter()
        .rev() // check highest rungs first so we report the one actually hit
        .find(|(threshold, _)| gain_pct >= *threshold && position.realized_fraction < 1.0)
    {
        Signal::TrimProfit {
            fraction,
            reason: format!(
                "Gain hit +{:.0}% (rung at {:.0}%). Consider locking in {:.0}% of the position.",
                gain_pct,
                threshold,
                fraction * 100.0
            ),
        }
    } else if let Some(r) = rsi_val {
        if r >= cfg.rsi_overbought {
            Signal::Alert {
                reason: format!(
                    "RSI is {:.0}, above your overbought threshold of {:.0}. No automatic action, but worth a look.",
                    r, cfg.rsi_overbought
                ),
            }
        } else {
            Signal::Hold
        }
    } else {
        Signal::Hold
    };

    Evaluation {
        symbol: position.symbol.clone(),
        current_price,
        entry_price: position.entry_price,
        peak_price: position.peak_price,
        gain_pct,
        drawdown_from_peak_pct: drawdown,
        rsi: rsi_val,
        sma20,
        atr,
        used_atr_stop,
        signal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_stop_fires_on_the_applied_materials_style_pullback() {
        let mut pos = Position::new("AMAT".into(), 100.0, 10.0);
        for p in [110.0, 130.0, 150.0, 170.0] {
            pos.record_price(p);
        }
        let cfg = StrategyConfig {
            trailing_stop_pct: 15.0,
            ..Default::default()
        };
        // peak is 170; a drop to 144 is a 15.3% drawdown from peak -> should trigger SellAll
        let eval = evaluate(&pos, 144.0, &cfg, None);
        matches!(eval.signal, Signal::SellAll { .. });
        assert!(!eval.used_atr_stop);
    }

    #[test]
    fn take_profit_rung_fires_before_stop_is_relevant() {
        let mut pos = Position::new("XYZ".into(), 100.0, 5.0);
        pos.record_price(131.0);
        let cfg = StrategyConfig::default();
        let eval = evaluate(&pos, 131.0, &cfg, None); // +31%, crosses the 30% rung
        matches!(eval.signal, Signal::TrimProfit { .. });
    }

    #[test]
    fn atr_stop_fires_when_configured_and_real_atr_provided() {
        let mut pos = Position::new("AMAT".into(), 100.0, 10.0);
        for p in [110.0, 130.0, 150.0, 170.0] {
            pos.record_price(p);
        }
        let cfg = StrategyConfig {
            trailing_stop_pct: 90.0, // deliberately way too loose to fire on its own
            atr_stop_multiplier: Some(2.0),
            ..Default::default()
        };
        // real ATR of 10 -> stop at peak(170) - 2*10 = 150
        let eval = evaluate(&pos, 149.0, &cfg, Some(10.0));
        assert!(matches!(eval.signal, Signal::SellAll { .. }));
        assert!(eval.used_atr_stop);
    }

    #[test]
    fn atr_stop_falls_back_to_flat_pct_when_no_real_atr_available() {
        let mut pos = Position::new("AMAT".into(), 100.0, 10.0);
        for p in [110.0, 130.0, 150.0, 170.0] {
            pos.record_price(p);
        }
        let cfg = StrategyConfig {
            trailing_stop_pct: 15.0,
            atr_stop_multiplier: Some(2.0), // configured, but no real_atr passed in
            ..Default::default()
        };
        let eval = evaluate(&pos, 144.0, &cfg, None);
        assert!(matches!(eval.signal, Signal::SellAll { .. }));
        assert!(
            !eval.used_atr_stop,
            "should fall back to flat pct, not silently do nothing"
        );
    }
}
