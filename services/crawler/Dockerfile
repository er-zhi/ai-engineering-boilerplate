# Crawler image: compiles the crawler with cached dependencies and ships the static binary alone on Alpine.
FROM rust:1.98-alpine3.24 AS build
RUN apk add --no-cache protoc
WORKDIR /app
# `dev` (fast, unoptimised) for the local stack; `release` for anything shipped. Cargo writes a
# `dev` build to target/debug, every other profile to target/<profile>.
ARG BUILD_PROFILE=release
ENV CARGO_HOME=/var/cache/cargo CARGO_TARGET_DIR=/var/cache/target
RUN --mount=type=bind,target=. \
    --mount=type=cache,target=/var/cache/cargo \
    --mount=type=cache,target=/var/cache/target \
    cargo build --locked --profile "$BUILD_PROFILE" --package crawler \
    && cp "/var/cache/target/$([ "$BUILD_PROFILE" = dev ] && echo debug || echo "$BUILD_PROFILE")/crawler" /usr/local/bin/

FROM alpine:3.24
COPY --from=build /usr/local/bin/crawler /usr/local/bin/
ENV SPIDER_MAX_SIZE_BYTES=5242880
USER 10001:10001
EXPOSE 8081
HEALTHCHECK --interval=5s CMD ["wget", "-qO-", "http://127.0.0.1:8081/health"]
CMD ["crawler"]
