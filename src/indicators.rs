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

/// Exponential moving average over the full series. Kept as a standalone
/// convenience on top of `ema_series` - `macd()` uses `ema_series`
/// directly, but this is here for anyone wiring up a single-EMA signal.
#[allow(dead_code)]
pub fn ema(values: &[f64], period: usize) -> Option<f64> {
    ema_series(values, period)?.last().copied()
}

/// Full EMA series (not just the latest point) - needed to build MACD,
/// which is itself an EMA of a derived series. Seeds with the SMA of the
/// first `period` values (the standard approach), then rolls forward.
/// Returned series has length `values.len() - period + 1`.
pub fn ema_series(values: &[f64], period: usize) -> Option<Vec<f64>> {
    if values.len() < period || period == 0 {
        return None;
    }
    let k = 2.0 / (period as f64 + 1.0);
    let seed = values[..period].iter().sum::<f64>() / period as f64;
    let mut out = Vec::with_capacity(values.len() - period + 1);
    out.push(seed);
    for &v in &values[period..] {
        let prev = *out.last().unwrap();
        out.push(v * k + prev * (1.0 - k));
    }
    Some(out)
}

/// MACD: (macd_line, signal_line, histogram). Standard 12/26/9 defaults.
/// Bullish momentum when macd_line > signal_line (histogram positive).
pub fn macd(values: &[f64], fast: usize, slow: usize, signal: usize) -> Option<(f64, f64, f64)> {
    let fast_series = ema_series(values, fast)?;
    let slow_series = ema_series(values, slow)?;
    // fast_series starts earlier (shorter period -> longer series) than
    // slow_series; align them to the same tail before subtracting.
    if fast_series.len() < slow_series.len() {
        return None;
    }
    let offset = fast_series.len() - slow_series.len();
    let macd_series: Vec<f64> = fast_series[offset..]
        .iter()
        .zip(slow_series.iter())
        .map(|(f, s)| f - s)
        .collect();
    let signal_series = ema_series(&macd_series, signal)?;
    let macd_line = *macd_series.last()?;
    let signal_line = *signal_series.last()?;
    Some((macd_line, signal_line, macd_line - signal_line))
}

/// Bollinger Bands: (lower, middle, upper). `num_std` is conventionally 2.0.
/// Price above the upper band is a common (imperfect) "overbought" read;
/// below the lower band, "oversold" - same caveats as any single indicator.
pub fn bollinger_bands(values: &[f64], period: usize, num_std: f64) -> Option<(f64, f64, f64)> {
    let mid = sma(values, period)?;
    let window = &values[values.len() - period..];
    let variance = window.iter().map(|v| (v - mid).powi(2)).sum::<f64>() / period as f64;
    let std = variance.sqrt();
    Some((mid - num_std * std, mid, mid + num_std * std))
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

    #[test]
    fn macd_is_bullish_on_a_steady_uptrend() {
        let v: Vec<f64> = (0..60).map(|i| 100.0 + i as f64 * 0.5).collect();
        let (macd_line, signal_line, hist) = macd(&v, 12, 26, 9).unwrap();
        assert!(macd_line > 0.0, "macd should be positive in an uptrend");
        assert!(
            hist > -0.01,
            "histogram should not be strongly negative in a steady uptrend"
        );
        let _ = signal_line; // sanity: just needs to compute without panicking
    }

    #[test]
    fn bollinger_bands_widen_with_volatility() {
        let flat = vec![100.0; 20];
        let (lower, mid, upper) = bollinger_bands(&flat, 20, 2.0).unwrap();
        assert_eq!(lower, mid);
        assert_eq!(upper, mid);

        let mut volatile = vec![100.0; 19];
        volatile.push(130.0); // one spike
        let (lower_v, _mid_v, upper_v) = bollinger_bands(&volatile, 20, 2.0).unwrap();
        assert!(upper_v - lower_v > 0.0);
    }
}
