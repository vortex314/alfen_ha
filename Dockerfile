# syntax=docker/dockerfile:1.7

FROM rust:1.88-slim-bookworm AS builder
ARG APP_BIN=alfen-mqtt
WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release --bin ${APP_BIN} \
    && cp /app/target/release/${APP_BIN} /tmp/app_bin

FROM debian:bookworm-slim AS runtime
ARG APP_BIN=alfen-mqtt
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /tmp/app_bin /usr/local/bin/app
COPY config.toml /app/config.toml

ENTRYPOINT ["/usr/local/bin/app"]
CMD ["/app/config.toml"]
