# Chat image: compiles the session/topic orchestration service with cached dependencies and ships the static binary alone on Alpine.
FROM rust:1.98-alpine3.24 AS build
RUN apk add --no-cache protoc
WORKDIR /app
ENV CARGO_HOME=/var/cache/cargo CARGO_TARGET_DIR=/var/cache/target
RUN --mount=type=bind,target=. \
    --mount=type=cache,target=/var/cache/cargo \
    --mount=type=cache,target=/var/cache/target \
    cargo build --locked --release --package chat \
    && cp /var/cache/target/release/chat /usr/local/bin/

FROM alpine:3.24
COPY --from=build /usr/local/bin/chat /usr/local/bin/
USER 10001:10001
EXPOSE 8088
HEALTHCHECK --interval=5s CMD ["wget", "-qO-", "http://127.0.0.1:8088/health"]
CMD ["chat"]
