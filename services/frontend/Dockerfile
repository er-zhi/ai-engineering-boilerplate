# Frontend image: compiles the frontend with cached dependencies and ships the static binary alone on Alpine.
FROM rust:1.98-alpine3.24 AS build
WORKDIR /app
ENV CARGO_HOME=/var/cache/cargo CARGO_TARGET_DIR=/var/cache/target
RUN --mount=type=bind,target=. \
    --mount=type=cache,target=/var/cache/cargo \
    --mount=type=cache,target=/var/cache/target \
    cargo build --locked --release --package frontend \
    && cp /var/cache/target/release/frontend /usr/local/bin/

FROM alpine:3.24
COPY --from=build /usr/local/bin/frontend /usr/local/bin/
USER 10001:10001
EXPOSE 8082
HEALTHCHECK --interval=5s CMD ["wget", "-qO-", "http://127.0.0.1:8082/health"]
CMD ["frontend"]
