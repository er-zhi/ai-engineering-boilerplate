#!/bin/sh
# Runs cargo in the test image, so no local Rust toolchain is needed.
# Database tests start Postgres with testcontainers, so the container gets the Docker socket
# and host networking to reach the ports testcontainers maps.
# Usage: scripts/test.sh [cargo args...]   (default: nextest run --workspace)
set -eu
cd "$(dirname "$0")/.."
docker build -q -t ai-boilerplate-test -f scripts/test.Dockerfile scripts >/dev/null
[ "$#" -eq 0 ] && set -- nextest run --workspace
exec docker run --rm --network host \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v "$PWD":/w -w /w \
  -v ai-boilerplate-test-target:/target -e CARGO_TARGET_DIR=/target \
  -v ai-boilerplate-cargo-registry:/usr/local/cargo/registry \
  ai-boilerplate-test cargo "$@"
