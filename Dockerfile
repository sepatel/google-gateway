# syntax=docker/dockerfile:1.7

FROM rust:1.99.1-alpine3.21 AS builder
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
    && cp target/release/gmail-gateway /gmail-gateway

FROM alpine:3.21
RUN apk add --no-cache ca-certificates tini \
    && addgroup -S gateway && adduser -S -G gateway gateway \
    && mkdir /data && chown gateway:gateway /data
COPY --from=builder /gmail-gateway /usr/local/bin/gmail-gateway
USER gateway
VOLUME /data
EXPOSE 8080
ENV PORT=8080 \
    DATA_DIR=/data \
    RUST_LOG=info
HEALTHCHECK --interval=30s --timeout=3s CMD wget -qO- http://127.0.0.1:8080/healthz || exit 1
ENTRYPOINT ["/sbin/tini", "--", "/usr/local/bin/gmail-gateway"]
