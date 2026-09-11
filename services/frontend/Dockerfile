FROM rust:1.98-alpine3.24 AS dev
RUN apk add --no-cache musl-dev build-base watchexec
WORKDIR /w
CMD ["watchexec", "--restart", "--watch", "services/frontend", "--watch", "Cargo.toml", "--watch", "Cargo.lock", "--", "cargo", "run", "-p", "frontend"]

FROM rust:1.98-alpine3.24 AS builder
RUN apk add --no-cache musl-dev build-base
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY common common
COPY services services
# Cache mounts keep the cargo registry and target/ between builds, so only changed crates recompile.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release -p frontend \
    && cp target/release/frontend /usr/local/bin/frontend

FROM alpine:3.24
RUN apk add --no-cache ca-certificates && adduser -D -u 10001 app
COPY --from=builder /usr/local/bin/frontend /usr/local/bin/frontend
USER app
EXPOSE 8082
CMD ["frontend"]
