# stock-sentinel

A Rust/Axum server that watches your open positions and tells you when a
**rule**, not a guess, says it's time to act. It does not predict prices.

## On "professional" and "not losing money"

No indicator, library, or LLM guarantees you won't lose money — including
everything added in this update. What actually separates disciplined
trading from gambling is two things this project now has:

1. **Position sizing** (`src/risk.rs`) — risking a fixed, small % of your
   account per trade, sized by your stop distance, so one bad trade can't
   do serious damage regardless of how good the setup looked.
2. **Backtesting** (`src/backtest.rs`) — replaying your actual rules
   against real price history _before_ trusting them live, so "15%
   trailing stop" is a tested number, not a guess.

MACD and Bollinger Bands (below) are useful context. They are not more
important than those two things.

## Risk management: position sizing

```
POST /risk/position-size
{"account_equity": 10000, "risk_pct": 1, "entry_price": 100, "stop_price": 85}

-> {"risk_amount":100.0,"risk_per_share":15.0,"shares":6.0,"position_value":600.0,"position_pct_of_account":6.0,"warning":null}
```

Fixed-fractional sizing: you decide how much of your account you're
willing to lose on this trade (1-2% is conventional), and the position
size falls out of that and your stop distance — not the other way
around. If your stop is too tight for your account size, you get a
`warning` instead of a silently oversized position.

## Backtesting: validate before you trust it

```bash
# Paste historical closes (oldest first) - e.g. from a downloaded CSV
curl -X POST localhost:8080/backtest/AMAT/import \
  -d '{"prices":[100,110,125,140,155,170,160,150,140,130]}'

# Replay the strategy against that history
curl -X POST localhost:8080/backtest/AMAT \
  -d '{"entry_price":100,"trailing_stop_pct":15,"take_profit_ladder":[]}'

-> {"strategy_return_pct":40.0,"buy_and_hold_return_pct":30.0, "events":[...]}
```

This replays the exact same `strategy::evaluate` function the live
scanner uses — same code path, not a separate approximation — against
whatever price history is stored for that symbol (accumulated live, or
imported). On the scenario that started this project, the trailing stop
locks in +40% versus +30% for holding straight through the pullback.

Two honest limits: it only backtests one entry per run (no multi-trade
compounding), and Finnhub's free tier doesn't offer bulk historical bars
(see roadmap), so live-accumulated history starts thin and grows the
longer the scanner runs — import real historical closes if you want to
test against more than what's accumulated so far.

**Sweep instead of guessing one number:**

```bash
curl -X POST localhost:8080/backtest/AMAT/sweep \
  -d '{"entry_price":100,"stop_pcts":[10,15,25,40]}'
```

Grid-searches every stop % against a no-ladder and a standard-ladder
config (or pass your own `"ladders"`), ranked by `risk_adjusted_score`
(return ÷ |max drawdown|) — so a config with a slightly lower return but
a much gentler equity curve can rank above a flashier one. On the AMAT
scenario, a tight stop _combined with_ a take-profit ladder outscored a
bare trailing stop by roughly 8x on this metric — locking in partial
gains before the pullback even matters, not just reacting to it.

## Portfolio-level circuit breaker

Every rule so far is per-position. This one isn't: the scanner tracks
total position value (quantity × price, summed) every cycle, compares it
to the all-time peak, and if aggregate drawdown breaches
`MAX_PORTFOLIO_DRAWDOWN_PCT` (default 15%), it **suppresses new candidate
Buy verdicts** — both from the scanner and from on-demand
`/candidates/evaluate` calls — until the drawdown recovers. You still get
a Telegram alert once on the transition into and out of the tripped
state, not every cycle.

```
GET /portfolio/status
-> {"current_value":8200.0,"peak_value":10000.0,"drawdown_pct":-18.0,"tripped":true,"limit_pct":15.0}
```

This does **not** touch existing positions or their trailing stops —
those keep protecting you regardless. It only stops _new_ buying while
things are already going badly, which is a standard piece of portfolio
risk discipline that per-symbol rules alone can't provide.

## Trend indicators: MACD and Bollinger Bands

Added to `src/indicators.rs` (pure, unit-tested) and folded into
candidate scoring in `src/pipeline.rs` at modest weight, same as
RSI - context, not a trigger:

- **MACD** (12/26/9) — bullish momentum when the MACD line is above its
  signal line.
- **Bollinger Bands** (20-period, 2 std dev) — price outside the bands is
  a common (imperfect) overbought/oversold read.

`GET /history/:symbol?limit=200` returns the stored price series (used
by these indicators) if you want to chart it yourself.

## Telegram alerts

```bash
# 1. Message @BotFather on Telegram, /newbot, follow the prompts - it gives you a token
# 2. Message your new bot anything, then visit:
#    https://api.telegram.org/bot<YOUR_TOKEN>/getUpdates
#    and read "chat":{"id": ...} from the response - that's your chat ID
export TELEGRAM_BOT_TOKEN=123456:ABC-your-token
export TELEGRAM_CHAT_ID=987654321
cargo run
```

With both set, every `SellAll`, `TrimProfit`, `Watch`, candidate `Buy`
verdict, portfolio circuit breaker transition, and theme alert (below)
gets pushed to Telegram in addition to the logs. Without them, alerts
stay in the logs only — nothing breaks, `state.notifier` is just `None`
(see `src/telegram.rs`).

## Macro/theme watch — the Rheinmetal case, honestly

Worth being direct about this one: there is no way to guarantee catching
the next Rheinmetal, and any single past example looks obvious in
hindsight in a way it wasn't at the time. What `src/themes.rs` actually
does is narrower and more honest — make sure a genuinely large, sustained
policy shift in a theme you're already tracking doesn't go unnoticed
while you're not actively reading the news yourself. It is a "go
research this" alert, never a buy signal, following the same pattern as
every other risk-flag in this project.

```bash
curl -X POST localhost:8080/themes -d '{
  "name": "german_defense",
  "keywords": ["Germany defense spending", "NATO budget increase", "Bundeswehr Sondervermogen"],
  "symbols": ["RHM.DE", "RNMBY"]
}'
```

Every scan cycle, the scanner searches news for each keyword, pools the
headlines, and asks the local LLM to rate `relevance` (0.0-1.0,
deliberately conservative — most news cycles are noise) and name which
tracked symbols plausibly are affected and why. Only `relevance >= 0.6`
actually alerts; everything else just logs to `theme_log`
(`GET /themes/:name/history`) so you can review the pattern of what's
been happening even when nothing crossed the alert bar.

```
GET  /themes                 - list tracked themes
POST /themes                 - add or update one
DELETE /themes/:name
GET  /themes/:name/history   - past checks, relevant or not
```

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

## Production hardening

Five things separate "runs on my laptop" from "safe to leave running on
a public cloud box unattended":

1. **Auth** (`src/auth.rs`) — set `API_AUTH_TOKEN` and every route except
   `/health` requires `Authorization: Bearer <token>`. Unset it and the
   server logs a loud, repeated warning at startup rather than silently
   staying open — this thing can add positions, spend your Finnhub quota,
   and fire Telegram messages, so it shouldn't be reachable by strangers.
2. **Rate limiting** (`src/ratelimit.rs`) — a hand-rolled sliding-window
   limiter (no new dependency) shared between `FinnhubProvider` and
   `FinnhubExtras`, capped under the free tier's 60 calls/min by default
   (`FINNHUB_RATE_LIMIT_PER_MIN=55`). The scanner can't accidentally burn
   through your quota just because it's scanning several tickers in
   parallel.
3. **Retry with backoff** — `FinnhubProvider::quote` retries transient
   failures 3x with exponential backoff before giving up on a symbol for
   that cycle, instead of one blip taking a ticker out of a whole scan.
4. **Graceful shutdown** — `axum::serve(...).with_graceful_shutdown(...)`
   listens for Ctrl+C and SIGTERM (what cloud platforms send on
   redeploy/restart) and finishes in-flight requests instead of dropping
   them mid-response.
5. **Docker** (`Dockerfile`, `docker-compose.yml`) — multi-stage build,
   non-root user, SQLite statically linked (no runtime dependency beyond
   `ca-certificates` for TLS), a named volume for the database so it
   survives redeploys. `docker-compose.yml` wires up Ollama alongside it
   and **refuses to start** if `API_AUTH_TOKEN` isn't set in your `.env` -
   copy `.env.example` to `.env` and fill it in first.

```bash
cp .env.example .env   # fill in API_AUTH_TOKEN at minimum
docker compose up -d
docker compose exec ollama ollama pull llama3.2:3b   # one-time model download
```

Honest gap: graceful shutdown covers the HTTP server; the background
scanner task is aborted rather than allowed to finish its current cycle.
For a personal-scale service polling every few minutes this is a minor
gap (worst case, a cycle's results are lost, not corrupted), but a
`CancellationToken` threaded through the scanner loop would close it
properly if you want to take this further.

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

All routes below require `Authorization: Bearer <API_AUTH_TOKEN>` if
that env var is set (see Production hardening); `/health` never does.

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
POST   /candidates/evaluate          {"symbols":["NVDA","AMD"]}   <- one-off "what should I buy?" (circuit-breaker gated)
GET    /fundamentals/:symbol         <- P/E, beta, 52-week range (needs Finnhub key)
GET    /peers/:symbol                <- same-sector peer tickers (needs Finnhub key)
GET    /history/:symbol?limit=200    <- stored price series, for charting or your own analysis
POST   /risk/position-size           {"account_equity":10000,"risk_pct":1,"entry_price":100,"stop_price":85}
POST   /backtest/:symbol/import      {"prices":[...]}   <- bulk-load historical closes for backtesting
POST   /backtest/:symbol             {"entry_price":100,"trailing_stop_pct":15,"take_profit_ladder":[]}
POST   /backtest/:symbol/sweep       {"entry_price":100,"stop_pcts":[10,15,25,40]}   <- grid search, ranked
GET    /portfolio/status             <- aggregate drawdown + circuit breaker state
GET    /themes                       <- macro/theme watch (Rheinmetal-style monitoring)
POST   /themes                       {"name":"...","keywords":[...],"symbols":[...]}
DELETE /themes/:name
GET    /themes/:name/history
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

1. **Real historical bars for candidates** — confirmed against your swagger
   export: `/stock/candle` is premium-only on Finnhub, so free-tier users
   can't backfill OHLC history in one call. `/candidates/evaluate` and the
   watchlist scan instead build history from repeated live `/quote` polls
   over time (persisted in `price_history`, so it improves the longer the
   scanner runs). If you're on a paid Finnhub tier, swap in `/stock/candle`
   for instant backfill; otherwise Twelve Data's free tier does include
   historical daily bars and is worth a look.
2. **Real ATR** — approximated from closes; swap in real high/low bars.
   Once real OHLC is available, an ATR-scaled trailing stop (e.g. "2x ATR
   below peak") is worth backtesting against the flat-percentage one via
   `/backtest/:symbol/sweep` - volatile names like AMAT likely want a
   wider stop than a utility stock, and now you can actually test that
   instead of guessing.
3. **Scanner shutdown isn't fully graceful** — see the honest gap noted in
   Production hardening above; a `CancellationToken` would close it.
4. **Trade Republic has no public API** — this can't place orders. It's a
   decision-support/alerting layer; you still execute manually.

## Project layout

```
src/
  indicators.rs    pure math: SMA, EMA, RSI, MACD, Bollinger Bands, ATR, drawdown-from-peak (unit tested)
  strategy.rs      hard technical rules: trailing stop, take-profit ladder (unit tested)
  pipeline.rs      the state machine: combines technical + news/LLM into an explainable verdict; circuit-breaker gate (unit tested)
  backtest.rs      replays strategy.rs against stored price history, incl. grid-search sweep (unit tested)
  risk.rs          fixed-fractional position sizing (unit tested)
  ratelimit.rs     sliding-window rate limiter, shared across all Finnhub traffic (unit tested)
  auth.rs          bearer-token middleware
  news.rs          NewsProvider trait + Google News RSS implementation (unit tested)
  ollama.rs        local Ollama client - structured JSON news analysis and macro theme analysis
  themes.rs        macro/geopolitical theme monitoring (the Rheinmetal use case)
  data.rs          MarketDataProvider trait + Finnhub impl (rate-limited, retried) + Mock impl
  finnhub_extra.rs company news, earnings calendar, analyst consensus, fundamentals, peers
  telegram.rs      Telegram bot alerts for scanner verdicts, circuit breaker, and theme alerts
  db.rs            SQLite persistence: positions, watchlist, price history, evaluation log, portfolio equity, themes
  scanner.rs       background loop: dynamic ticker list, parallel fan-out, portfolio circuit breaker, theme scan
  portfolio.rs      Position struct, price history
  routes.rs        Axum handlers
  state.rs         shared AppState
  main.rs          wiring + server bootstrap + graceful shutdown
```

## Deploying

```bash
cp .env.example .env   # fill in API_AUTH_TOKEN at minimum
docker compose up -d
docker compose exec ollama ollama pull llama3.2:3b
```

That's the whole deployment on any box with Docker - Fly.io, Railway, a
DigitalOcean droplet, a home server. The compose file mounts a named
volume for the SQLite database, so positions and history survive
redeploys. Without Docker: it's a single static-ish binary behind Axum,
so `cargo build --release --locked` and run the binary directly works
too, as long as Ollama is reachable at `OLLAMA_BASE_URL`.
