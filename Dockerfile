# syntax=docker/dockerfile:1

# Build stage
FROM rust:1.98.0-slim-bookworm@sha256:1469a27c125cb5a3aebfa4f4e4665d935b02fb72cc093b2c974b3d740e43f157 AS builder

# Install required dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Ship executable code without compiler symbol/debug tables. Scan every runtime
# image layer below; stripping is not a secret-scan exemption.
ENV CARGO_PROFILE_RELEASE_STRIP=symbols

# Copy manifests, the build script, and the protos. `build = "build.rs"` runs
# tonic-build over proto/, so both build.rs and proto/ must be present even for
# the dependency-only pre-build below.
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto ./proto
# Cargo.toml patches Google signing to the audited local source.
COPY vendor/reqsign-google ./vendor/reqsign-google
COPY vendor/chalk-federated-auth ./vendor/chalk-federated-auth

# Pre-build dependencies first (better layer caching). Stub the bin and the
# declared `[[test]] integration_test` target: tests/ is excluded by
# .dockerignore, so the test file can't be COPYed and must be created here or
# the manifest fails to parse ("can't find integration_test test ...").
RUN mkdir -p src tests && \
    echo "fn main() {}" > src/main.rs && \
    touch tests/integration_test.rs && \
    cargo build --locked --release --bin openai-server && \
    rm -rf src

# Migrations must be present at COMPILE time: `sqlx::migrate!("migrations/default")`
# in src/main.rs embeds the .sql files into the binary (no DB needed at build, and
# the runtime image needs no migration files). Copied after the deps pre-build so
# migration edits don't bust the dependency cache. NOTE: migrations-clickhouse/ is
# NOT embedded/read by the binary (operator-applied SQL for the future ClickHouse
# backend), so it is intentionally not copied.
COPY migrations ./migrations

# Pricing history is embedded from `prices/rate_cards/` at compile time.
# Keep it separate from source so rate updates only invalidate the final layer.
COPY prices ./prices

# Copy real source and build. tests/ stays stubbed — its real contents are
# .dockerignored (they make paid network calls) and aren't needed to build the
# binary; the stub only satisfies the manifest's declared test target.
COPY src ./src
RUN touch src/main.rs && cargo build --locked --release --bin openai-server

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd --gid 10001 router && useradd --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin router

WORKDIR /app

# Copy the binary from builder
COPY --from=builder /app/target/release/openai-server /app/openai-server

# Expose port (adjust as needed)
EXPOSE 8080

USER 10001:10001

# Run the binary
CMD ["/app/openai-server"]
