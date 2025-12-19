FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
RUN apt-get update && apt-get install -y libssl-dev pkg-config
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release --bin letterboxd-compare

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y libssl3 ca-certificates && update-ca-certificates && rm -rf /var/lib/apt/lists/*
ENV SSL_CERT_DIR=/etc/ssl/certs
WORKDIR /app
COPY --from=builder /app/target/release/letterboxd-compare /usr/local/bin
ENTRYPOINT ["/usr/local/bin/letterboxd-compare"]
