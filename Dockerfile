# Build stage - a full Rust toolchain, discarded from the final image.
FROM rust:1-slim-bookworm AS builder

# rusqlite's `bundled` feature compiles SQLite from source, so a C
# compiler is needed at build time (not at runtime - it's statically
# linked into the binary).
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# --locked: fail the build rather than silently re-resolving dependencies
# to something that wasn't tested. Reproducible builds matter more than
# usual here given how much dependency-version pinning this project
# needed to get a working Cargo.lock in the first place.
RUN cargo build --release --locked

# Runtime stage - small, no Rust toolchain, no build tools.
FROM debian:bookworm-slim

# ca-certificates: needed for TLS verification (Finnhub, Telegram,
# Google News, Ollama-over-HTTPS if you're not running it locally).
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --shell /bin/false sentinel

WORKDIR /app
COPY --from=builder /app/target/release/stock-sentinel /app/stock-sentinel

# SQLite database lives here - mount a volume at this path so positions
# and history survive container restarts/redeploys.
RUN mkdir -p /data && chown sentinel:sentinel /data
ENV DATABASE_PATH=/data/stock-sentinel.db
VOLUME ["/data"]

USER sentinel
EXPOSE 8080
ENTRYPOINT ["/app/stock-sentinel"]
