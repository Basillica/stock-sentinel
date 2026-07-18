# stock-sentinel

A Rust/Axum server that watches your open positions and tells you when a
**rule**, not a guess, says it's time to act. It does not predict prices.

## Architecture: a state machine, not a black box

For each position, the pipeline (`src/pipeline.rs`) walks a sequence of
stages and can short-circuit early:

```
Ingest quote -> Risk check (hard rules) -> [triggered?] -> Done (sell/trim)
                                          -> [clear] -> News + local LLM -> Aggregate -> Hold / Watch
```

**Hard rules are authoritative and cannot be overridden by news or
sentiment.** If your trailing stop fires, it fires — the news/LLM stage
still runs so the response can explain _why_ the price moved, but it never
gets to veto or delay the sell. This is deliberate: LLM sentiment on
headlines is a genuinely weak, noisy signal (true for any model, local or
frontier), and letting it override a tested risk rule would make the
system less reliable, not more.

For a candidate you don't own, a parallel pipeline scores RSI, trend, and
news sentiment into a `Buy` / `Watch` / `Avoid` verdict — always with a
`trace: Vec<Evidence>` listing exactly what contributed and by how much.
A news risk flag (trading halt, fraud investigation, guidance withdrawal,
etc.) forces `Avoid` regardless of how good the technicals look.

Nothing in this system places an order. Trade Republic has no public API,
and even if it did, keeping a human in the loop on anything that moves
money is the right call.

## Persistence

Positions, watchlist, price history, and a full evaluation audit log all
live in SQLite (`src/db.rs`, WAL mode). The in-memory `DashMap` of
positions is still there as a fast-path cache — it's hydrated from SQLite
on startup and written through on every change, so a restart doesn't lose
your positions and you're not doing a disk read on every `/signal` poll.

```bash
export DATABASE_PATH=stock-sentinel.db   # default, override for e.g. a mounted volume
```

## The background scanner: parallel, dynamic ticker list

`src/scanner.rs` is a loop that fires every `SCAN_INTERVAL_SECS` (default
15 min) and, each cycle:

1. **Re-reads the ticker list fresh** — held positions from the live cache,
   watchlist candidates from SQLite. Add or remove a position or watchlist
   symbol through the API while the server is running, and the _next_
   cycle picks it up automatically. Nothing is captured once at startup.
2. **Fans every ticker out as its own task** via `tokio::task::JoinSet`,
   bounded by a `Semaphore` (`MAX_CONCURRENT_SCANS`, default 8). `JoinSet`
   over `join_all` specifically because it (a) surfaces results as they
   complete rather than waiting for the slowest ticker, and (b) turns a
   panic in one ticker's task into an `Err` you can log, instead of
   poisoning the whole cycle.
3. **Gates the LLM calls with a second, narrower semaphore** living inside
   `Pipeline` itself (`LLM_CONCURRENCY`, default 2) — independent of how
   many tickers are scanning at once. This is the detail that matters most
   if you're running Ollama locally without a serious GPU: quote and news
   fetches are network I/O and parallelize fine, but a single local model
   instance mostly serializes inference anyway. Handing it 8 concurrent
   requests just means 8 tokio tasks sit there waiting instead of 2 - no
   faster, more memory, more timeout risk. The pipeline enforces this
   itself so no future call site can accidentally bypass it.

```bash
export SCAN_INTERVAL_SECS=900       # 15 minutes
export MAX_CONCURRENT_SCANS=8       # ticker-level fan-out
export LLM_CONCURRENCY=2            # Ollama-specific throttle
```

One more thing worth knowing if you extend this: a `DashMap::get_mut`
guard is held only long enough to update and clone the position, then
dropped _before_ the `await` on the pipeline call. Holding a DashMap lock
across an await point is a classic way to accidentally serialize what
should be concurrent work (or deadlock if two tasks touch the same key) —
`scan_position` clones out a `Position` snapshot first for exactly this
reason.

## Fixing the "scheme is not http" error

If you hit `invalid URL, scheme is not http` calling Finnhub: that message
comes from hyper's _plain_ HTTP connector, not from Finnhub. It means the
binary that got built has no TLS backend compiled in at all, so it can't
speak `https://`. `reqwest::Client::new()` fails **silently** into a
plaintext-only client if the TLS feature isn't active — no compile error,
just a confusing runtime one.

The fix (already applied in `src/data.rs` and `src/news.rs`): build the
client with `.use_rustls_tls()` explicitly instead of `Client::new()`. If
the `rustls-tls` feature is somehow missing, this fails at **compile
time** with a clear error, instead of failing at runtime months later. If
you still see this after pulling the latest code: `rm -rf target
Cargo.lock && cargo build` for a fully clean rebuild.

## Finnhub extras (`src/finnhub_extra.rs`)

Cross-checked against your swagger export — a lot of Finnhub is
premium-gated (notably `/stock/candle`, so real OHLC backfill still needs
a paid plan or a different provider). These are free-tier-friendly and
now wired in:

- **`/company-news`** — structured, deduped, timestamped news. Preferred
  over the RSS scraper automatically whenever `FINNHUB_API_KEY` is set;
  RSS remains the fallback.
- **`/calendar/earnings`** — flags an upcoming earnings date as
  informational context (`weight: 0.0` — never fires a rule on its own).
- **`/stock/recommendation`** — analyst buy/hold/sell consensus, folded
  into candidate scoring at low weight (aggregated opinion, not a hard
  signal).
- **`/stock/metric`** and **`/stock/peers`** — exposed directly via
  `GET /fundamentals/:symbol` and `GET /peers/:symbol` rather than folded
  into the verdict, so you can pull them up without them silently
  swaying a Buy/Avoid call.

All of these return `503` cleanly if no Finnhub key is configured, and
individual failures (rate limit, premium-gated) degrade to "skip this
evidence" rather than breaking the pipeline.

## Run it

```bash
cargo run
# server listens on 0.0.0.0:8080
```

Needs a local Ollama for the news/LLM stage:

```bash
ollama pull llama3.2:3b     # or qwen2.5:3b-instruct, phi3.5 - small is fine, this is classification not reasoning
ollama serve
```

Price data defaults to `MockProvider` (fake data, fine for dev). For live
quotes:

```bash
export FINNHUB_API_KEY=your_key_here   # free tier at finnhub.io
export OLLAMA_BASE_URL=http://localhost:11434   # default, override if remote
export OLLAMA_MODEL=llama3.2:3b                 # default
cargo run
```

News comes from Google News RSS — no API key needed. It's a quick-and-dirty
string-based title extractor (see `src/news.rs`); swap for a real feed if
you outgrow it (NewsAPI, Finnhub news, Alpha Vantage news all give you
publisher + timestamp for dedup, which RSS titles alone don't).

## API

```
GET    /health
POST   /positions                    {"symbol":"AMAT","entry_price":100.0,"quantity":10}
GET    /positions
DELETE /positions/:symbol
GET    /positions/:symbol/signal        <- fast: technical rules only, poll often
GET    /positions/:symbol/full-signal   <- slow: technical + news + LLM, on-demand check
GET    /evaluations/:symbol             <- audit trail: what the scanner decided, and why, over time
POST   /watchlist                    {"symbol":"AMD"}   <- add a candidate to the scanned list
GET    /watchlist
DELETE /watchlist/:symbol
POST   /candidates/evaluate          {"symbols":["NVDA","AMD"]}   <- one-off "what should I buy?"
GET    /fundamentals/:symbol         <- P/E, beta, 52-week range (needs Finnhub key)
GET    /peers/:symbol                <- same-sector peer tickers (needs Finnhub key)
```

Example `/full-signal` response:

```json
{
  "symbol": "AMAT",
  "trace": [
    {
      "source": "technical",
      "label": "risk check",
      "detail": "gain 44.0%, drawdown from peak -15.3%",
      "weight": 0.0
    },
    {
      "source": "technical",
      "label": "trailing stop",
      "detail": "Price is 15.3% below its peak of 170.00, past your 15% trailing stop.",
      "weight": -1.0
    },
    {
      "source": "news+llm",
      "label": "sentiment",
      "detail": "chip sector pullback; no company-specific bad news",
      "weight": -0.2
    }
  ],
  "verdict": {
    "verdict": "sell_all",
    "reason": "Price is 15.3% below its peak of 170.00, past your 15% trailing stop."
  }
}
```

Notice the verdict matches the technical trigger exactly — the news
evidence is there for context, not as the deciding vote.

## What's deliberately NOT here yet (roadmap)

1. **Notifications** — the scanner currently just logs
   (`tracing::warn!("ACTION ...")`). `scan_position` in `src/scanner.rs` has
   a clearly marked hook point — wire in a Telegram bot or ntfy.sh POST
   there next.
2. **Real historical bars for candidates** — confirmed against your swagger
   export: `/stock/candle` is premium-only on Finnhub, so free-tier users
   can't backfill OHLC history in one call. `/candidates/evaluate` and the
   watchlist scan instead build history from repeated live `/quote` polls
   over time (persisted in `price_history`, so it improves the longer the
   scanner runs). If you're on a paid Finnhub tier, swap in `/stock/candle`
   for instant backfill; otherwise Twelve Data's free tier does include
   historical daily bars and is worth a look.
3. **Real ATR** — approximated from closes; swap in real high/low bars.
4. **News dedup + source quality** — RSS titles have no publisher/timestamp
   right now, so the same story from five outlets counts five times. A
   structured news API fixes this cheaply.
5. **Backtesting** — before trusting any threshold (15% trailing stop? the
   0.35 buy/avoid score cutoff?) on real money, replay it against your own
   trade history and a few volatile names, using the `evaluation_log`
   table you're now building up automatically. ATR-scaled stops (e.g. "2x
   ATR below peak") usually beat one flat percentage across every stock.
6. **Trade Republic has no public API** — this can't place orders. It's a
   decision-support/alerting layer; you still execute manually.

## Project layout

```
src/
  indicators.rs   pure math: SMA, EMA, RSI, ATR, drawdown-from-peak (unit tested)
  strategy.rs      hard technical rules: trailing stop, take-profit ladder (unit tested)
  pipeline.rs      the state machine: combines technical + news/LLM into an explainable verdict
  news.rs          NewsProvider trait + Google News RSS implementation (unit tested)
  ollama.rs        local Ollama client, structured JSON news analysis
  data.rs          MarketDataProvider trait + Finnhub impl + Mock impl
  db.rs            SQLite persistence: positions, watchlist, price history, evaluation audit log
  scanner.rs       background loop: dynamic ticker list, parallel fan-out, two-tier concurrency limits
  portfolio.rs      Position struct, price history
  routes.rs        Axum handlers
  state.rs         shared AppState
  main.rs          wiring + server bootstrap
```

## Deploying

Single binary behind Axum — works on Fly.io, Railway, a $5 DigitalOcean
droplet, or a container on any cloud. Mount a persistent volume for
`DATABASE_PATH` so positions survive redeploys. If you deploy this away
from your home network, Ollama needs to be reachable at `OLLAMA_BASE_URL`
— either run Ollama on the same box/VPC, or accept the latency of a
remote call. A minimal `Dockerfile` (multi-stage build) is a good next
step; ask and I'll add one.
