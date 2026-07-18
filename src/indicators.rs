//! Pure, dependency-free technical indicator math.
//! Kept separate from I/O so it's trivially unit-testable.

/// Simple moving average of the last `period` values.
pub fn sma(values: &[f64], period: usize) -> Option<f64> {
    if values.len() < period || period == 0 {
        return None;
    }
    let slice = &values[values.len() - period..];
    Some(slice.iter().sum::<f64>() / period as f64)
}

/// Exponential moving average over the full series.
pub fn ema(values: &[f64], period: usize) -> Option<f64> {
    if values.len() < period || period == 0 {
        return None;
    }
    let k = 2.0 / (period as f64 + 1.0);
    // seed with the SMA of the oldest `period` values in the window, then roll forward
    let window = &values[values.len() - period..];
    let mut ema_val = window.iter().sum::<f64>() / period as f64;
    for &v in &values[values.len() - period + 1..] {
        ema_val = v * k + ema_val * (1.0 - k);
    }
    Some(ema_val)
}

/// Relative Strength Index (Wilder's method, simplified for a rolling window).
/// Returns a value in [0, 100]. >70 is conventionally "overbought", <30 "oversold".
pub fn rsi(values: &[f64], period: usize) -> Option<f64> {
    if values.len() < period + 1 {
        return None;
    }
    let window = &values[values.len() - period - 1..];
    let mut gains = 0.0;
    let mut losses = 0.0;
    for pair in window.windows(2) {
        let change = pair[1] - pair[0];
        if change > 0.0 {
            gains += change;
        } else {
            losses -= change;
        }
    }
    let avg_gain = gains / period as f64;
    let avg_loss = losses / period as f64;
    if avg_loss == 0.0 {
        return Some(100.0);
    }
    let rs = avg_gain / avg_loss;
    Some(100.0 - (100.0 / (1.0 + rs)))
}

/// Average True Range approximated from close-to-close changes when
/// high/low aren't available (fine for a first cut; swap in real H/L later).
pub fn atr_from_closes(values: &[f64], period: usize) -> Option<f64> {
    if values.len() < period + 1 {
        return None;
    }
    let window = &values[values.len() - period - 1..];
    let ranges: Vec<f64> = window.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
    Some(ranges.iter().sum::<f64>() / ranges.len() as f64)
}

/// Percentage drawdown from the running peak of the series.
/// This is the number that would have told you "you're down 20% from
/// the high" on the Applied Materials trade.
pub fn drawdown_from_peak(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let peak = values.iter().cloned().fold(f64::MIN, f64::max);
    let last = *values.last().unwrap();
    if peak <= 0.0 {
        return None;
    }
    Some((last - peak) / peak * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sma_basic() {
        let v = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(sma(&v, 3), Some((3.0 + 4.0 + 5.0) / 3.0));
        assert_eq!(sma(&v, 10), None);
    }

    #[test]
    fn drawdown_detects_the_applied_materials_scenario() {
        // price ran from 100 -> 170 (a +70% gain) -> 150 (peak-relative pain)
        let v = vec![100.0, 120.0, 150.0, 170.0, 160.0, 150.0];
        let dd = drawdown_from_peak(&v).unwrap();
        assert!((dd - (-11.76)).abs() < 0.1);
    }

    #[test]
    fn rsi_bounds() {
        let v: Vec<f64> = (0..20).map(|i| 100.0 + i as f64).collect(); // steady uptrend
        let r = rsi(&v, 14).unwrap();
        assert!(r > 90.0); // all gains, near-100 RSI
    }
}
