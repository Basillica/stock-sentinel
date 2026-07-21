use crate::pipeline::PipelineResult;
use crate::portfolio::Position;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension};
use std::sync::{Arc, Mutex};

mod signal;

/// rusqlite::Connection is not Send-across-await-friendly to hold inside an
/// async fn directly, so every call goes through `run`, which hops onto a
/// blocking thread. That keeps SQLite (which is inherently synchronous and
/// single-writer) from ever blocking the async runtime's worker threads,
/// which matters once the scanner is firing off a dozen concurrent tasks.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path).context("failed to open sqlite database")?;
        conn.execute_batch(
            "
            PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS positions (
                symbol             TEXT PRIMARY KEY,
                entry_price        REAL NOT NULL,
                quantity           REAL NOT NULL,
                peak_price         REAL NOT NULL,
                realized_fraction  REAL NOT NULL,
                opened_at          TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS watchlist (
                symbol   TEXT PRIMARY KEY,
                added_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS price_history (
                symbol      TEXT NOT NULL,
                price       REAL NOT NULL,
                recorded_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_price_history_symbol
                ON price_history(symbol, recorded_at);
            CREATE TABLE IF NOT EXISTS evaluation_log (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                symbol      TEXT NOT NULL,
                verdict     TEXT NOT NULL,
                trace_json  TEXT NOT NULL,
                recorded_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_eval_log_symbol
                ON evaluation_log(symbol, recorded_at);
            CREATE TABLE IF NOT EXISTS portfolio_equity (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                total_value REAL NOT NULL,
                recorded_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_portfolio_equity_time
                ON portfolio_equity(recorded_at);
            CREATE TABLE IF NOT EXISTS themes (
                name        TEXT PRIMARY KEY,
                keywords    TEXT NOT NULL,
                symbols     TEXT NOT NULL,
                added_at    TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS theme_log (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                theme       TEXT NOT NULL,
                summary     TEXT NOT NULL,
                relevance   REAL NOT NULL,
                symbols     TEXT NOT NULL,
                recorded_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_theme_log_theme
                ON theme_log(theme, recorded_at);
            CREATE TABLE IF NOT EXISTS digest_log (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                overview     TEXT NOT NULL,
                items_json   TEXT NOT NULL,
                generated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_digest_log_time
                ON digest_log(generated_at);
            CREATE TABLE IF NOT EXISTS ohlc_bars (
                symbol TEXT NOT NULL,
                date   TEXT NOT NULL,
                open   REAL NOT NULL,
                high   REAL NOT NULL,
                low    REAL NOT NULL,
                close  REAL NOT NULL,
                PRIMARY KEY (symbol, date)
            );
            ",
        )
        .context("failed to run schema migration")?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Run a closure against the connection on a blocking thread.
    async fn run<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().expect("sqlite connection mutex poisoned");
            f(&guard)
        })
        .await
        .context("sqlite task panicked")?
    }

    pub async fn upsert_position(&self, p: Position) -> Result<()> {
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO positions (symbol, entry_price, quantity, peak_price, realized_fraction, opened_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(symbol) DO UPDATE SET
                    entry_price = excluded.entry_price,
                    quantity = excluded.quantity,
                    peak_price = excluded.peak_price,
                    realized_fraction = excluded.realized_fraction",
                rusqlite::params![
                    p.symbol,
                    p.entry_price,
                    p.quantity,
                    p.peak_price,
                    p.realized_fraction,
                    p.opened_at.to_rfc3339(),
                ],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn delete_position(&self, symbol: String) -> Result<()> {
        self.run(move |conn| {
            conn.execute("DELETE FROM positions WHERE symbol = ?1", [symbol])?;
            Ok(())
        })
        .await
    }

    /// Loaded once at startup to hydrate the in-memory DashMap cache.
    pub async fn load_all_positions(&self) -> Result<Vec<Position>> {
        self.run(|conn| {
            let mut stmt = conn.prepare(
                "SELECT symbol, entry_price, quantity, peak_price, realized_fraction, opened_at FROM positions",
            )?;
            let rows = stmt.query_map([], |row| {
                let opened_at_str: String = row.get(5)?;
                let opened_at = DateTime::parse_from_rfc3339(&opened_at_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now());
                Ok(Position {
                    symbol: row.get(0)?,
                    entry_price: row.get(1)?,
                    quantity: row.get(2)?,
                    peak_price: row.get(3)?,
                    realized_fraction: row.get(4)?,
                    history: Default::default(),
                    opened_at,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    pub async fn add_watchlist_symbol(&self, symbol: String) -> Result<()> {
        self.run(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO watchlist (symbol, added_at) VALUES (?1, ?2)",
                rusqlite::params![symbol, Utc::now().to_rfc3339()],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn remove_watchlist_symbol(&self, symbol: String) -> Result<()> {
        self.run(move |conn| {
            conn.execute("DELETE FROM watchlist WHERE symbol = ?1", [symbol])?;
            Ok(())
        })
        .await
    }

    /// Read fresh every scan cycle - this is what makes the ticker list
    /// dynamic. Add or remove a symbol via the API and the next cycle
    /// picks it up with no restart.
    pub async fn list_watchlist(&self) -> Result<Vec<String>> {
        self.run(|conn| {
            let mut stmt = conn.prepare("SELECT symbol FROM watchlist")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    /// Bulk-insert a chronological price series (oldest first) for
    /// backtesting - e.g. pasted from a downloaded CSV. Spaces synthetic
    /// timestamps one day apart ending now, purely so `recent_prices`'
    /// ORDER BY recorded_at keeps them in the order they were given.
    pub async fn import_price_series(&self, symbol: String, prices: Vec<f64>) -> Result<usize> {
        let count = prices.len();
        self.run(move |conn| {
            let now = Utc::now();
            let tx = conn.unchecked_transaction()?;
            for (i, price) in prices.iter().enumerate() {
                let ts = now - chrono::Duration::days((prices.len() - i) as i64);
                tx.execute(
                    "INSERT INTO price_history (symbol, price, recorded_at) VALUES (?1, ?2, ?3)",
                    rusqlite::params![symbol, price, ts.to_rfc3339()],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await?;
        Ok(count)
    }

    pub async fn record_price(&self, symbol: String, price: f64) -> Result<()> {
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO price_history (symbol, price, recorded_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![symbol, price, Utc::now().to_rfc3339()],
            )?;
            Ok(())
        })
        .await
    }

    /// Most recent `limit` prices for a symbol, oldest first (matches the
    /// order `indicators.rs` expects - last element is the most recent).
    pub async fn recent_prices(&self, symbol: String, limit: usize) -> Result<Vec<f64>> {
        self.run(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT price FROM price_history WHERE symbol = ?1
                 ORDER BY recorded_at DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![symbol, limit as i64], |row| {
                row.get::<_, f64>(0)
            })?;
            let mut out: Vec<f64> = rows.collect::<Result<_, _>>()?;
            out.reverse(); // we queried newest-first, indicators.rs wants oldest-first
            Ok(out)
        })
        .await
    }

    pub async fn log_evaluation(&self, result: &PipelineResult) -> Result<()> {
        let symbol = result.symbol.clone();
        let verdict_label = match &result.verdict {
            crate::pipeline::Verdict::SellAll { .. } => "sell_all",
            crate::pipeline::Verdict::TrimProfit { .. } => "trim_profit",
            crate::pipeline::Verdict::Hold => "hold",
            crate::pipeline::Verdict::Buy { .. } => "buy",
            crate::pipeline::Verdict::Watch { .. } => "watch",
            crate::pipeline::Verdict::Avoid { .. } => "avoid",
        }
        .to_string();
        let trace_json = serde_json::to_string(&result.trace).unwrap_or_default();
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO evaluation_log (symbol, verdict, trace_json, recorded_at) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![symbol, verdict_label, trace_json, Utc::now().to_rfc3339()],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn recent_evaluations(
        &self,
        symbol: String,
        limit: usize,
    ) -> Result<Vec<(String, String, String)>> {
        // (verdict, trace_json, recorded_at), newest first
        self.run(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT verdict, trace_json, recorded_at FROM evaluation_log
                 WHERE symbol = ?1 ORDER BY recorded_at DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![symbol, limit as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    // --- Portfolio-level equity / circuit breaker ---

    pub async fn record_portfolio_equity(&self, total_value: f64) -> Result<()> {
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO portfolio_equity (total_value, recorded_at) VALUES (?1, ?2)",
                rusqlite::params![total_value, Utc::now().to_rfc3339()],
            )?;
            Ok(())
        })
        .await
    }

    /// (current_value, peak_value_ever_recorded, drawdown_pct). `None` if
    /// no equity has been recorded yet (e.g. server just started, no
    /// positions held).
    pub async fn portfolio_drawdown(&self) -> Result<Option<(f64, f64, f64)>> {
        self.run(|conn| {
            let current: Option<f64> = conn
                .query_row(
                    "SELECT total_value FROM portfolio_equity ORDER BY recorded_at DESC LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(current) = current else {
                return Ok(None);
            };
            let peak: f64 =
                conn.query_row("SELECT MAX(total_value) FROM portfolio_equity", [], |row| {
                    row.get(0)
                })?;
            let drawdown_pct = if peak > 0.0 {
                (current - peak) / peak * 100.0
            } else {
                0.0
            };
            Ok(Some((current, peak, drawdown_pct)))
        })
        .await
    }

    // --- Theme watch (macro / geopolitical monitoring) ---

    pub async fn add_theme(
        &self,
        name: String,
        keywords: Vec<String>,
        symbols: Vec<String>,
    ) -> Result<()> {
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO themes (name, keywords, symbols, added_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(name) DO UPDATE SET keywords = excluded.keywords, symbols = excluded.symbols",
                rusqlite::params![
                    name,
                    keywords.join(","),
                    symbols.join(","),
                    Utc::now().to_rfc3339()
                ],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn remove_theme(&self, name: String) -> Result<()> {
        self.run(move |conn| {
            conn.execute("DELETE FROM themes WHERE name = ?1", [name])?;
            Ok(())
        })
        .await
    }

    /// (name, keywords, symbols) for every tracked theme.
    pub async fn list_themes(&self) -> Result<Vec<(String, Vec<String>, Vec<String>)>> {
        self.run(|conn| {
            let mut stmt = conn.prepare("SELECT name, keywords, symbols FROM themes")?;
            let rows = stmt.query_map([], |row| {
                let name: String = row.get(0)?;
                let keywords: String = row.get(1)?;
                let symbols: String = row.get(2)?;
                Ok((name, keywords, symbols))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (name, keywords, symbols) = row?;
                out.push((
                    name,
                    keywords
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect(),
                    symbols
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect(),
                ));
            }
            Ok(out)
        })
        .await
    }

    pub async fn log_theme_event(
        &self,
        theme: String,
        summary: String,
        relevance: f64,
        symbols: Vec<String>,
    ) -> Result<()> {
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO theme_log (theme, summary, relevance, symbols, recorded_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![theme, summary, relevance, symbols.join(","), Utc::now().to_rfc3339()],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn recent_theme_events(
        &self,
        theme: String,
        limit: usize,
    ) -> Result<Vec<(String, f64, String, String)>> {
        // (summary, relevance, symbols_csv, recorded_at), newest first
        self.run(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT summary, relevance, symbols, recorded_at FROM theme_log
                 WHERE theme = ?1 ORDER BY recorded_at DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![theme, limit as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, f64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    // --- Real OHLC bars (Twelve Data backfill) ---

    pub async fn upsert_ohlc_bars(
        &self,
        symbol: String,
        bars: Vec<crate::data::twelvedata::OhlcBar>,
    ) -> Result<usize> {
        let count = bars.len();
        self.run(move |conn| {
            let tx = conn.unchecked_transaction()?;
            for bar in &bars {
                tx.execute(
                    "INSERT INTO ohlc_bars (symbol, date, open, high, low, close) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(symbol, date) DO UPDATE SET
                        open = excluded.open, high = excluded.high, low = excluded.low, close = excluded.close",
                    rusqlite::params![symbol, bar.date, bar.open, bar.high, bar.low, bar.close],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await?;
        Ok(count)
    }

    /// Oldest-first, up to `limit` most recent bars.
    pub async fn recent_ohlc(
        &self,
        symbol: String,
        limit: usize,
    ) -> Result<Vec<crate::data::twelvedata::OhlcBar>> {
        self.run(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT date, open, high, low, close FROM ohlc_bars
                 WHERE symbol = ?1 ORDER BY date DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![symbol, limit as i64], |row| {
                Ok(crate::data::twelvedata::OhlcBar {
                    date: row.get(0)?,
                    open: row.get(1)?,
                    high: row.get(2)?,
                    low: row.get(3)?,
                    close: row.get(4)?,
                })
            })?;
            let mut out: Vec<crate::data::twelvedata::OhlcBar> = rows.collect::<Result<_, _>>()?;
            out.reverse(); // queried newest-first, callers want oldest-first
            Ok(out)
        })
        .await
    }

    /// Convenience: fetch stored OHLC bars for a symbol and compute a real
    /// ATR from them (`indicators::atr`), or `None` if there isn't enough
    /// backfilled history yet. This is what `evaluate()`'s `real_atr`
    /// parameter is meant to be fed from.
    pub async fn latest_real_atr(&self, symbol: String, period: usize) -> Result<Option<f64>> {
        let bars = self.recent_ohlc(symbol, period + 5).await?;
        let highs: Vec<f64> = bars.iter().map(|b| b.high).collect();
        let lows: Vec<f64> = bars.iter().map(|b| b.low).collect();
        let closes: Vec<f64> = bars.iter().map(|b| b.close).collect();
        Ok(crate::indicators::atr(&highs, &lows, &closes, period))
    }

    // --- Global market digest ---

    pub async fn log_digest(&self, overview: String, items_json: String) -> Result<()> {
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO digest_log (overview, items_json, generated_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![overview, items_json, Utc::now().to_rfc3339()],
            )?;
            Ok(())
        })
        .await
    }

    /// (overview, items_json, generated_at) for the most recent digest,
    /// or `None` if none has run yet.
    pub async fn latest_digest(&self) -> Result<Option<(String, String, String)>> {
        self.run(|conn| {
            conn.query_row(
                "SELECT overview, items_json, generated_at FROM digest_log ORDER BY generated_at DESC LIMIT 1",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)),
            )
            .optional()
            .map_err(Into::into)
        })
        .await
    }

    pub async fn recent_digests(&self, limit: usize) -> Result<Vec<(String, String, String)>> {
        self.run(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT overview, items_json, generated_at FROM digest_log ORDER BY generated_at DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map([limit as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }
}
