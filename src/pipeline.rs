use crate::finnhub_extra::FinnhubExtras;
use crate::indicators::{bollinger_bands, macd, rsi, sma};
use crate::news::NewsProvider;
use crate::ollama::{NewsAnalysis, OllamaClient};
use crate::portfolio::Position;
use crate::strategy::{evaluate as evaluate_technical, Signal, StrategyConfig};
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// One piece of evidence the pipeline considered, kept so every verdict is
/// explainable rather than a black box. `weight` is roughly -1.0 (bearish/
/// negative for the position) to 1.0 (bullish/positive); 0.0 for evidence
/// that's informational only.
#[derive(Debug, Clone, Serialize)]
pub struct Evidence {
    pub source: String,
    pub label: String,
    pub detail: String,
    pub weight: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    /// Hard rule fired - not overridable by news/sentiment.
    SellAll {
        reason: String,
    },
    TrimProfit {
        fraction: f64,
        reason: String,
    },
    Hold,
    /// For candidates you don't yet hold.
    Buy {
        confidence: f64,
    },
    Watch {
        confidence: f64,
    },
    Avoid {
        confidence: f64,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct PipelineResult {
    pub symbol: String,
    pub trace: Vec<Evidence>,
    pub verdict: Verdict,
}

/// Wires together data sources. The pipeline itself holds no state -
/// everything it needs comes in as arguments, so it's easy to test.
pub struct Pipeline {
    pub news: Arc<dyn NewsProvider>,
    pub llm: Arc<OllamaClient>,
    /// Caps *concurrent calls into Ollama specifically*, independent of how
    /// many tickers the scanner is processing at once. A local model
    /// serving one request at a time (the common case without a beefy GPU)
    /// gains nothing from 20 concurrent inference calls - they just queue
    /// up inside Ollama and tie up 20 tokio tasks waiting. Keep this low
    /// (1-3) and let the outer scanner semaphore be more generous, since
    /// network I/O (quotes, news) parallelizes fine even when inference
    /// doesn't.
    llm_semaphore: Arc<Semaphore>,
    /// Optional richer Finnhub data (structured news, earnings calendar,
    /// analyst consensus). None when running on MockProvider / no API key -
    /// every call site below treats its absence as "skip this evidence",
    /// never as an error.
    extras: Option<Arc<FinnhubExtras>>,
}

impl Pipeline {
    pub fn new(
        news: Arc<dyn NewsProvider>,
        llm: Arc<OllamaClient>,
        llm_semaphore: Arc<Semaphore>,
        extras: Option<Arc<FinnhubExtras>>,
    ) -> Self {
        Self {
            news,
            llm,
            llm_semaphore,
            extras,
        }
    }

    /// Structured Finnhub company news when available (deduped, timestamped,
    /// sourced); falls back to the RSS scraper otherwise. This is the
    /// "news dedup + source quality" roadmap item, resolved for anyone
    /// running with a Finnhub key.
    async fn gather_headlines(&self, symbol: &str) -> Vec<String> {
        if let Some(extras) = &self.extras {
            if let Ok(items) = extras.company_news(symbol, 7).await {
                if !items.is_empty() {
                    return items.into_iter().map(|i| i.headline).collect();
                }
            }
        }
        self.news.headlines(symbol, None).await.unwrap_or_default()
    }

    /// Earnings-proximity and analyst-consensus evidence, shared by both
    /// the position and candidate paths. Always informational (weight
    /// scaled down) - context for you to weigh, not a trigger.
    async fn gather_finnhub_context(&self, symbol: &str) -> Vec<Evidence> {
        let mut out = Vec::new();
        let Some(extras) = &self.extras else {
            return out;
        };

        if let Ok(Some(earnings)) = extras.next_earnings(symbol, 7).await {
            out.push(Evidence {
                source: "finnhub".into(),
                label: "earnings calendar".into(),
                detail: format!(
                    "Earnings release on {} ({}) - expect elevated volatility around this date.",
                    earnings.date,
                    earnings.hour.unwrap_or_else(|| "time unspecified".into())
                ),
                weight: 0.0,
            });
        }

        if let Ok(Some(trend)) = extras.recommendation_trend(symbol).await {
            if let Some(score) = trend.consensus_score() {
                out.push(Evidence {
                    source: "finnhub".into(),
                    label: "analyst consensus".into(),
                    detail: format!(
                        "{} period: {} strong buy, {} buy, {} hold, {} sell, {} strong sell.",
                        trend.period,
                        trend.strong_buy,
                        trend.buy,
                        trend.hold,
                        trend.sell,
                        trend.strong_sell
                    ),
                    weight: score * 0.15, // modest - aggregated analyst opinion, not a hard signal
                });
            }
        }

        out
    }

    /// All news+LLM calls go through here so the concurrency cap is never
    /// accidentally bypassed by a new call site.
    async fn analyze(&self, symbol: &str, headlines: &[String]) -> anyhow::Result<NewsAnalysis> {
        let _permit = self.llm_semaphore.acquire().await?;
        self.llm.analyze_headlines(symbol, headlines).await
    }

    /// State machine for a position you already hold:
    ///
    ///   RiskCheck --(hard rule fires)--> Done (news attached for context only)
    ///        |
    ///        v (no hard rule)
    ///      News/LLM --(risk flag found)--> Watch (human review)
    ///        |
    ///        v
    ///       Hold
    pub async fn run_position(
        &self,
        position: &Position,
        current_price: f64,
        cfg: &StrategyConfig,
        real_atr: Option<f64>,
    ) -> PipelineResult {
        let mut trace = Vec::new();
        let tech_eval = evaluate_technical(position, current_price, cfg, real_atr);
        trace.push(Evidence {
            source: "technical".into(),
            label: "risk check".into(),
            detail: format!(
                "gain {:.1}%, drawdown from peak {:.1}%",
                tech_eval.gain_pct, tech_eval.drawdown_from_peak_pct
            ),
            weight: 0.0,
        });

        // Hard rules are authoritative and short-circuit the state machine.
        // News is still fetched so the response can explain *why* the price
        // moved, but it cannot upgrade or downgrade the verdict.
        let hard_verdict = match &tech_eval.signal {
            Signal::SellAll { reason } => Some((
                Evidence {
                    source: "technical".into(),
                    label: "trailing stop".into(),
                    detail: reason.clone(),
                    weight: -1.0,
                },
                Verdict::SellAll {
                    reason: reason.clone(),
                },
            )),
            Signal::TrimProfit { fraction, reason } => Some((
                Evidence {
                    source: "technical".into(),
                    label: "take-profit rung".into(),
                    detail: reason.clone(),
                    weight: 0.5,
                },
                Verdict::TrimProfit {
                    fraction: *fraction,
                    reason: reason.clone(),
                },
            )),
            _ => None,
        };

        if let Some((evidence, verdict)) = hard_verdict {
            trace.push(evidence);
            let headlines = self.gather_headlines(&position.symbol).await;
            if !headlines.is_empty() {
                if let Ok(news) = self.analyze(&position.symbol, &headlines).await {
                    trace.push(news_evidence(&news));
                }
            }
            trace.extend(self.gather_finnhub_context(&position.symbol).await);
            return PipelineResult {
                symbol: position.symbol.clone(),
                trace,
                verdict,
            };
        }

        // No hard trigger yet - news is advisory. A risk flag escalates to
        // "Watch" (go look at this yourself), never to an automatic sell.
        let headlines = self.gather_headlines(&position.symbol).await;
        if let Ok(news) = self.analyze(&position.symbol, &headlines).await {
            let has_risk = !news.risk_flags.is_empty();
            trace.push(news_evidence(&news));
            if has_risk {
                trace.extend(self.gather_finnhub_context(&position.symbol).await);
                return PipelineResult {
                    symbol: position.symbol.clone(),
                    trace,
                    verdict: Verdict::Watch {
                        confidence: news.confidence,
                    },
                };
            }
        }
        trace.extend(self.gather_finnhub_context(&position.symbol).await);

        PipelineResult {
            symbol: position.symbol.clone(),
            trace,
            verdict: Verdict::Hold,
        }
    }

    /// State machine for a candidate you don't yet own: "should I buy this?"
    /// Every contributing factor is logged in `trace` with its weight, and
    /// a news risk flag forces Avoid regardless of how good the technicals
    /// look - the same "hard-ish safety valve" pattern as the position side.
    pub async fn run_candidate(&self, symbol: &str, price_history: &[f64]) -> PipelineResult {
        let mut trace = Vec::new();
        let mut score = 0.0_f64;

        if let Some(r) = rsi(price_history, 14) {
            let (w, label) = if r < 30.0 {
                (0.3, "oversold - potential value")
            } else if r > 75.0 {
                (-0.3, "overbought")
            } else {
                (0.0, "neutral")
            };
            score += w;
            trace.push(Evidence {
                source: "technical".into(),
                label: "RSI".into(),
                detail: format!("RSI={r:.0} ({label})"),
                weight: w,
            });
        }

        if let (Some(s20), Some(&last)) = (sma(price_history, 20), price_history.last()) {
            let w = if last > s20 { 0.15 } else { -0.15 };
            score += w;
            trace.push(Evidence {
                source: "technical".into(),
                label: "trend".into(),
                detail: format!(
                    "price is {} its 20-period SMA",
                    if last > s20 { "above" } else { "below" }
                ),
                weight: w,
            });
        }

        if let Some((macd_line, signal_line, hist)) = macd(price_history, 12, 26, 9) {
            let w = if hist > 0.0 { 0.15 } else { -0.15 };
            score += w;
            trace.push(Evidence {
                source: "technical".into(),
                label: "MACD".into(),
                detail: format!(
                    "MACD {:.2} vs signal {:.2} ({})",
                    macd_line,
                    signal_line,
                    if hist > 0.0 {
                        "bullish crossover"
                    } else {
                        "bearish crossover"
                    }
                ),
                weight: w,
            });
        }

        if let (Some((lower, _mid, upper)), Some(&last)) = (
            bollinger_bands(price_history, 20, 2.0),
            price_history.last(),
        ) {
            let (w, label) = if last > upper {
                (-0.1, "above upper band - stretched, caution")
            } else if last < lower {
                (0.1, "below lower band - potentially oversold")
            } else {
                (0.0, "within bands")
            };
            score += w;
            trace.push(Evidence {
                source: "technical".into(),
                label: "Bollinger Bands".into(),
                detail: format!("price {last:.2} vs band [{lower:.2}, {upper:.2}] - {label}"),
                weight: w,
            });
        }

        if let Some(dd) = crate::indicators::drawdown_from_peak(price_history) {
            trace.push(Evidence {
                source: "technical".into(),
                label: "drawdown from recent peak".into(),
                detail: format!("{dd:.1}% off the peak of the tracked history window"),
                weight: 0.0, // informational only - RSI/trend/MACD already cover direction
            });
        }

        let headlines = self.gather_headlines(symbol).await;
        if let Ok(news) = self.analyze(symbol, &headlines).await {
            let w = news.sentiment * news.confidence * 0.5; // weighted less than price trend
            score += w;
            let has_risk = !news.risk_flags.is_empty();
            trace.push(news_evidence(&news));
            if has_risk {
                trace.extend(self.gather_finnhub_context(symbol).await);
                return PipelineResult {
                    symbol: symbol.to_string(),
                    trace,
                    verdict: Verdict::Avoid {
                        confidence: news.confidence,
                    },
                };
            }
        }

        let context = self.gather_finnhub_context(symbol).await;
        score += context.iter().map(|e| e.weight).sum::<f64>();
        trace.extend(context);

        let confidence = score.abs().min(1.0);
        let verdict = if score > 0.35 {
            Verdict::Buy { confidence }
        } else if score < -0.35 {
            Verdict::Avoid { confidence }
        } else {
            Verdict::Watch { confidence }
        };

        PipelineResult {
            symbol: symbol.to_string(),
            trace,
            verdict,
        }
    }
}

/// Portfolio-level override: if the aggregate drawdown circuit breaker is
/// tripped, no new Buy verdict should go out, regardless of how good an
/// individual candidate looks. This is intentionally separate from - and
/// applied *after* - the per-symbol pipeline, because "should I buy
/// anything right now" is a portfolio-level question the single-symbol
/// pipeline has no visibility into. Pure and unit-tested so the override
/// logic itself is verifiable, not just trusted.
pub fn apply_circuit_breaker(mut result: PipelineResult, tripped: bool) -> PipelineResult {
    if tripped {
        if let Verdict::Buy { confidence } = result.verdict {
            result.trace.push(Evidence {
                source: "portfolio".into(),
                label: "circuit breaker".into(),
                detail: "Portfolio drawdown circuit breaker is tripped - new buys are suppressed until it clears.".into(),
                weight: -1.0,
            });
            result.verdict = Verdict::Avoid { confidence };
        }
    }
    result
}

fn news_evidence(n: &NewsAnalysis) -> Evidence {
    Evidence {
        source: "news+llm".into(),
        label: "sentiment".into(),
        detail: if n.key_points.is_empty() {
            "no notable headlines found".into()
        } else {
            n.key_points.join("; ")
        },
        weight: n.sentiment * n.confidence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn circuit_breaker_downgrades_buy_to_avoid_when_tripped() {
        let result = PipelineResult {
            symbol: "NVDA".into(),
            trace: vec![],
            verdict: Verdict::Buy { confidence: 0.8 },
        };
        let gated = apply_circuit_breaker(result, true);
        assert!(matches!(gated.verdict, Verdict::Avoid { .. }));
        assert!(gated.trace.iter().any(|e| e.label == "circuit breaker"));
    }

    #[test]
    fn circuit_breaker_leaves_buy_alone_when_not_tripped() {
        let result = PipelineResult {
            symbol: "NVDA".into(),
            trace: vec![],
            verdict: Verdict::Buy { confidence: 0.8 },
        };
        let gated = apply_circuit_breaker(result, false);
        assert!(matches!(gated.verdict, Verdict::Buy { .. }));
    }

    #[test]
    fn circuit_breaker_does_not_touch_non_buy_verdicts() {
        let result = PipelineResult {
            symbol: "AMAT".into(),
            trace: vec![],
            verdict: Verdict::SellAll {
                reason: "stop".into(),
            },
        };
        let gated = apply_circuit_breaker(result, true);
        assert!(matches!(gated.verdict, Verdict::SellAll { .. }));
    }
}
