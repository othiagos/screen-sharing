# Multi-stage build for the ultra-lightweight P2P Signaling Server
FROM rust:1-slim-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/

# Build only the pure-Rust signaling-server binary (no GUI/media C libraries needed)
RUN cargo build --release -p signaling-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/signaling-server /usr/local/bin/signaling-server

ENV PORT=9001
EXPOSE 9001

CMD ["signaling-server"]
