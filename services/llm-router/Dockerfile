# LLM router image: compiles the router with cached dependencies and ships the static binary alone on Alpine.
FROM rust:1.98.0-alpine3.24 AS build
RUN apk add --no-cache protoc protobuf-dev
WORKDIR /app
ARG BUILD_PROFILE=release
ENV RUSTUP_TOOLCHAIN=1.98.0 CARGO_HOME=/var/cache/cargo CARGO_TARGET_DIR=/var/cache/target
RUN --mount=type=bind,target=. \
    --mount=type=cache,target=/var/cache/cargo \
    --mount=type=cache,target=/var/cache/target \
    cargo build --locked --profile "$BUILD_PROFILE" --package llm-router \
    && cp "/var/cache/target/$([ "$BUILD_PROFILE" = dev ] && echo debug || echo "$BUILD_PROFILE")/llm-router" /usr/local/bin/

FROM alpine:3.24
COPY --from=build /usr/local/bin/llm-router /usr/local/bin/
USER 10001:10001
EXPOSE 8083
HEALTHCHECK --interval=5s CMD ["wget", "-qO-", "http://127.0.0.1:8083/health"]
CMD ["llm-router"]
