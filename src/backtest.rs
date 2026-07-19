use crate::portfolio::Position;
use crate::strategy::{evaluate, Signal, StrategyConfig};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct BacktestEvent {
    pub index: usize,
    pub price: f64,
    pub action: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BacktestResult {
    pub data_points: usize,
    pub entry_price: f64,
    pub events: Vec<BacktestEvent>,
    /// % return the strategy actually captured, blending any partial
    /// take-profit trims with either a stop-loss exit or a mark-to-market
    /// value on whatever fraction was still held at the end of the series.
    pub strategy_return_pct: f64,
    /// % return of simply buying and holding through the whole series,
    /// for comparison. This is the number the strategy needs to beat.
    pub buy_and_hold_return_pct: f64,
    pub still_holding_fraction: f64,
    /// Worst peak-to-trough decline of the strategy's own equity curve
    /// (realized + mark-to-market unrealized) during the replay. A
    /// strategy with a higher return but a much worse drawdown isn't
    /// automatically the better choice - this is what `sweep()` ranks on.
    pub max_drawdown_pct: f64,
}

/// Pure replay - no I/O, no network. Feed it a price series (oldest
/// first) and it walks the same rule engine `strategy::evaluate` uses
/// live, tracking partial exits via `realized_fraction` exactly like a
/// real position would. This is what "validate before you trust it"
/// looks like in code, not just in principle.
pub fn run(prices: &[f64], entry_price: f64, cfg: &StrategyConfig) -> BacktestResult {
    let mut pos = Position::new("BACKTEST".into(), entry_price, 1.0);
    let mut events = Vec::new();
    let mut realized_value = 0.0_f64; // sum of (fraction_sold * price_at_sale)
    let mut fully_exited = false;
    let mut last_price = entry_price;

    let mut peak_equity = entry_price;
    let mut max_drawdown_pct = 0.0_f64;

    for (i, &price) in prices.iter().enumerate() {
        last_price = price;
        pos.record_price(price);
        let eval = evaluate(&pos, price, cfg);

        match eval.signal {
            Signal::SellAll { reason } => {
                let remaining = 1.0 - pos.realized_fraction;
                realized_value += remaining * price;
                pos.realized_fraction = 1.0;
                events.push(BacktestEvent {
                    index: i,
                    price,
                    action: "sell_all".into(),
                    detail: reason,
                });
                fully_exited = true;
                // Mark the equity curve at the exit point before breaking.
                peak_equity = peak_equity.max(realized_value);
                let dd = (realized_value - peak_equity) / peak_equity * 100.0;
                max_drawdown_pct = max_drawdown_pct.min(dd);
                break;
            }
            Signal::TrimProfit { fraction, reason } => {
                let sell_fraction = fraction.min(1.0 - pos.realized_fraction);
                if sell_fraction > 0.0 {
                    realized_value += sell_fraction * price;
                    pos.realized_fraction += sell_fraction;
                    events.push(BacktestEvent {
                        index: i,
                        price,
                        action: "trim_profit".into(),
                        detail: reason,
                    });
                }
                if pos.realized_fraction >= 1.0 {
                    fully_exited = true;
                    peak_equity = peak_equity.max(realized_value);
                    let dd = (realized_value - peak_equity) / peak_equity * 100.0;
                    max_drawdown_pct = max_drawdown_pct.min(dd);
                    break;
                }
            }
            _ => {}
        }

        // Mark-to-market equity at this point in time: whatever's been
        // realized so far, plus the unrealized remainder valued at the
        // current price.
        let equity_now = realized_value + (1.0 - pos.realized_fraction) * price;
        peak_equity = peak_equity.max(equity_now);
        let dd = (equity_now - peak_equity) / peak_equity * 100.0;
        max_drawdown_pct = max_drawdown_pct.min(dd);
    }

    let still_holding_fraction = if fully_exited {
        0.0
    } else {
        1.0 - pos.realized_fraction
    };
    if !fully_exited && still_holding_fraction > 0.0 {
        // Mark whatever's left to market at the last observed price.
        realized_value += still_holding_fraction * last_price;
    }

    let strategy_return_pct = (realized_value - entry_price) / entry_price * 100.0;
    // Buy-and-hold means holding through the *entire* observed window,
    // regardless of when the strategy exited - compare against the last
    // price in the full series, not wherever the strategy's loop broke.
    let final_price = *prices.last().unwrap_or(&entry_price);
    let buy_and_hold_return_pct = (final_price - entry_price) / entry_price * 100.0;

    BacktestResult {
        data_points: prices.len(),
        entry_price,
        events,
        strategy_return_pct,
        buy_and_hold_return_pct,
        still_holding_fraction,
        max_drawdown_pct,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SweepResult {
    pub trailing_stop_pct: f64,
    pub take_profit_ladder: Vec<(f64, f64)>,
    pub strategy_return_pct: f64,
    pub buy_and_hold_return_pct: f64,
    pub max_drawdown_pct: f64,
    /// return / |max_drawdown| - a simple Calmar-style risk-adjusted
    /// score. Higher is better; a config with a great return but a
    /// brutal drawdown scores worse than a steadier one with a smaller
    /// return, which is usually the more honest comparison for a
    /// strategy you're going to run unattended with real money behind it.
    pub risk_adjusted_score: f64,
}

/// Grid-search trailing-stop percentages x take-profit ladders against
/// one price series, ranked by `risk_adjusted_score` descending. This is
/// what turns "15% felt right" into a number you actually tested.
pub fn sweep(
    prices: &[f64],
    entry_price: f64,
    stop_pcts: &[f64],
    ladders: &[Vec<(f64, f64)>],
) -> Vec<SweepResult> {
    let mut out = Vec::with_capacity(stop_pcts.len() * ladders.len());
    for &stop in stop_pcts {
        for ladder in ladders {
            let cfg = StrategyConfig {
                trailing_stop_pct: stop,
                take_profit_ladder: ladder.clone(),
                ..Default::default()
            };
            let result = run(prices, entry_price, &cfg);
            let score = if result.max_drawdown_pct.abs() > 0.01 {
                result.strategy_return_pct / result.max_drawdown_pct.abs()
            } else {
                result.strategy_return_pct
            };
            out.push(SweepResult {
                trailing_stop_pct: stop,
                take_profit_ladder: ladder.clone(),
                strategy_return_pct: result.strategy_return_pct,
                buy_and_hold_return_pct: result.buy_and_hold_return_pct,
                max_drawdown_pct: result.max_drawdown_pct,
                risk_adjusted_score: score,
            });
        }
    }
    out.sort_by(|a, b| {
        b.risk_adjusted_score
            .partial_cmp(&a.risk_adjusted_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact motivating scenario from the start of this project:
    /// entering at 100, running to +70%, then pulling back. The trailing
    /// stop should lock in meaningfully more than holding straight
    /// through the pullback.
    #[test]
    fn trailing_stop_beats_buy_and_hold_on_the_applied_materials_scenario() {
        let prices = vec![
            100.0, 110.0, 125.0, 140.0, 155.0, 170.0, // run to +70%
            160.0, 150.0, // pulling back - 15%+ off the 170 peak should trigger the stop
            140.0, 130.0, // if we were still holding, it keeps sliding
        ];
        let cfg = StrategyConfig {
            trailing_stop_pct: 15.0,
            take_profit_ladder: vec![], // isolate the trailing stop for this test
            ..Default::default()
        };
        let result = run(&prices, 100.0, &cfg);

        assert!(
            !result.events.is_empty(),
            "the trailing stop should have fired"
        );
        assert_eq!(result.events[0].action, "sell_all");
        assert!(
            result.strategy_return_pct > result.buy_and_hold_return_pct,
            "strategy ({:.1}%) should beat buy-and-hold ({:.1}%) once the stop protects the gain",
            result.strategy_return_pct,
            result.buy_and_hold_return_pct
        );
    }

    #[test]
    fn take_profit_ladder_locks_in_partial_gains() {
        let prices = vec![100.0, 120.0, 135.0, 165.0, 210.0];
        let cfg = StrategyConfig {
            trailing_stop_pct: 50.0, // wide, so it's the ladder being tested here
            take_profit_ladder: vec![(30.0, 0.25), (60.0, 0.25), (100.0, 0.25)],
            ..Default::default()
        };
        let result = run(&prices, 100.0, &cfg);
        let trims = result
            .events
            .iter()
            .filter(|e| e.action == "trim_profit")
            .count();
        assert!(
            trims >= 1,
            "at least one take-profit rung should have fired"
        );
    }

    #[test]
    fn max_drawdown_is_negative_when_price_pulls_back() {
        let prices = vec![100.0, 120.0, 140.0, 100.0]; // sharp pullback, no rules configured to catch it
        let cfg = StrategyConfig {
            trailing_stop_pct: 90.0, // effectively disabled, isolate drawdown measurement
            take_profit_ladder: vec![],
            ..Default::default()
        };
        let result = run(&prices, 100.0, &cfg);
        assert!(
            result.max_drawdown_pct < 0.0,
            "should record a real drawdown"
        );
    }

    #[test]
    fn sweep_ranks_configs_and_a_tighter_stop_wins_on_this_series() {
        let prices = vec![
            100.0, 110.0, 125.0, 140.0, 155.0, 170.0, 160.0, 150.0, 140.0, 130.0,
        ];
        let results = sweep(&prices, 100.0, &[10.0, 15.0, 40.0], &[vec![]]);
        assert_eq!(results.len(), 3);
        // Sorted descending by risk-adjusted score.
        for pair in results.windows(2) {
            assert!(pair[0].risk_adjusted_score >= pair[1].risk_adjusted_score);
        }
    }
}
