# ---- Build stage ----
FROM rust:1-bookworm AS builder

WORKDIR /app

# Cache dependencies: build with a stub main so the (slow) dependency compile is
# reused as long as Cargo.toml / Cargo.lock don't change.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src

# Build the real binary.
COPY src ./src
RUN touch src/main.rs && cargo build --release

# ---- Runtime stage ----
FROM debian:bookworm-slim AS runtime

# rustls bundles its own roots, but ca-certificates keeps TLS robust.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Run as an unprivileged user, with a writable data directory.
RUN useradd --system --uid 10001 potsworth \
    && mkdir -p /data \
    && chown potsworth /data

COPY --from=builder /app/target/release/dnd-food-rota-bot /usr/local/bin/potsworth

# State lives on a mounted volume so it survives container restarts.
ENV DATA_PATH=/data/rota_data.json
VOLUME ["/data"]

USER potsworth
CMD ["potsworth"]
