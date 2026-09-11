# Test runner image: the workspace toolchain plus cargo-nextest, clippy, and rustfmt.
FROM rust:1.98-alpine3.24
RUN apk add --no-cache musl-dev build-base protoc protobuf-dev curl \
    && rustup component add clippy rustfmt \
    && curl -LsSf "https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-0.9.144/cargo-nextest-0.9.144-$(uname -m)-unknown-linux-musl.tar.gz" \
       | tar zxf - -C /usr/local/cargo/bin
