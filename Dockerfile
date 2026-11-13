# Multi-stage build for the QUIC SFU Media Relay Server (pure Rust, zero GUI/media libs)
FROM rust:1-slim-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/

RUN cargo build --release -p sfu-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/sfu-server /usr/local/bin/sfu-server

ENV SFU_BIND_ADDR=0.0.0.0:4433
ENV SFU_CERT_DIR=/data/certs
VOLUME ["/data/certs"]

EXPOSE 4433/udp
EXPOSE 4433/tcp

CMD ["sfu-server"]
