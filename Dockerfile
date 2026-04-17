# syntax=docker/dockerfile:1.7

FROM rust:1.86-slim-bookworm AS builder
WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/alfen-mqtt /usr/local/bin/alfen-mqtt
COPY config.toml /app/config.toml

ENTRYPOINT ["/usr/local/bin/alfen-mqtt"]
CMD ["/app/config.toml"]
