# ── Stage 1: builder ────────────────────────────────────────────────────────
FROM rust:1.88-bookworm AS builder

WORKDIR /app

# System libraries needed to compile openssl-sys and similar C-backed crates.
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Pre-fetch dependencies before copying source so that changes to src/ don't
# invalidate the dependency layer (the Cargo.lock stays the same).
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo fetch \
    && rm -rf src

# Copy the real source tree and build.
# NOTE: sqlx::migrate!() embeds migrations at compile time, so the migrations/
# directory does not need to be present in the runtime image.
COPY src/ src/
COPY migrations/ migrations/

# No SQLX_OFFLINE needed: the codebase uses runtime sqlx::query() calls (not
# compile-time sqlx::query!() macros), so no live DB or .sqlx/ cache is needed
# at build time. The release profile sets strip = true and lto = true (Cargo.toml).
RUN cargo build --release

# ── Stage 2: runtime ────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

LABEL org.opencontainers.image.source="https://github.com/brunojppb/feedrelay"

# ca-certificates: reqwest needs the system cert store for HTTPS.
# curl:            used by the HEALTHCHECK below.
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Create a non-root user so the process does not run as UID 0.
RUN useradd --uid 1001 --no-create-home --shell /usr/sbin/nologin feedrelay

WORKDIR /app

# Volume mount point for the SQLite database file.
# The host path (e.g. ./feedrelay-data) is mounted here by docker-compose.
RUN mkdir /data && chown feedrelay:feedrelay /data

COPY --from=builder /app/target/release/feedrelay /app/feedrelay

USER feedrelay

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS http://localhost:8080/management/health || exit 1

CMD ["/app/feedrelay"]
