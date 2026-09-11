FROM rust:1.98-alpine3.24 AS dev
RUN apk add --no-cache musl-dev build-base protoc protobuf-dev watchexec
WORKDIR /w
CMD ["watchexec", "--restart", "--watch", "common", "--watch", "services/crawler", "--watch", "Cargo.toml", "--watch", "Cargo.lock", "--", "cargo", "run", "-p", "crawler"]

FROM rust:1.98-alpine3.24 AS builder
RUN apk add --no-cache musl-dev build-base protoc protobuf-dev
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY common common
COPY services services
# Cache mounts keep the cargo registry and target/ between builds, so only changed crates recompile.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release -p crawler \
    && cp target/release/crawler /usr/local/bin/crawler

FROM alpine:3.24
RUN apk add --no-cache ca-certificates && adduser -D -u 10001 app
COPY --from=builder /usr/local/bin/crawler /usr/local/bin/crawler
USER app
EXPOSE 8081
CMD ["crawler"]
