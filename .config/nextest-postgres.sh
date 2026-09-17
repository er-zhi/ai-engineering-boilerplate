#!/bin/sh
# Starts one disposable Postgres shared by every database test process in this nextest run.

set -eu

if [ -z "${NEXTEST_ENV:-}" ] || [ -z "${NEXTEST_RUN_ID:-}" ]; then
  echo "nextest did not provide its setup environment" >&2
  exit 1
fi

container_name="ai-engineering-test-${NEXTEST_RUN_ID}"
parent_pid="$PPID"

cleanup() {
  docker rm -f "$container_name" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

docker run --detach --rm \
  --name "$container_name" \
  --publish 127.0.0.1::5432 \
  --env POSTGRES_PASSWORD=test \
  --env POSTGRES_DB=postgres \
  pgvector/pgvector:pg18 >/dev/null

attempt=0
until docker exec "$container_name" sh -c '[ "$(cat /proc/1/comm)" = postgres ]' >/dev/null 2>&1 \
  && docker exec "$container_name" pg_isready -U postgres -d postgres >/dev/null 2>&1; do
  attempt=$((attempt + 1))
  if [ "$attempt" -ge 120 ]; then
    echo "test Postgres did not become ready" >&2
    exit 1
  fi
  sleep 0.25
done

# Every service whose tests take a database. A service missing here silently falls back to a
# container of its own per test (`common::test_db::start`), which is what makes a suite slow.
SCHEMAS="crawler gateway knowledge_base llm_router engine tool chat"

bootstrap_roles() {
  for schema in $SCHEMAS; do
    printf "DO \$\$ BEGIN
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = '%s_user') THEN
    CREATE ROLE %s_user LOGIN PASSWORD 'test';
  END IF;
END \$\$;
ALTER ROLE %s_user SET search_path TO %s, public;
" "$schema" "$schema" "$schema" "$schema"
  done | docker exec -i "$container_name" psql -v ON_ERROR_STOP=1 -U postgres -d postgres >/dev/null
}

attempt=0
until bootstrap_roles; do
  attempt=$((attempt + 1))
  if [ "$attempt" -ge 120 ]; then
    echo "test Postgres did not finish initialization" >&2
    exit 1
  fi
  sleep 0.25
done

port=$(docker port "$container_name" 5432/tcp | sed -n 's/.*://p' | head -n 1)
printf 'TEST_POSTGRES_HOST=127.0.0.1\nTEST_POSTGRES_PORT=%s\n' "$port" >> "$NEXTEST_ENV"

trap - EXIT INT TERM
nohup sh -c '
  parent_pid="$1"
  container_name="$2"
  while kill -0 "$parent_pid" 2>/dev/null; do sleep 1; done
  docker rm -f "$container_name" >/dev/null 2>&1 || true
' sh "$parent_pid" "$container_name" >/dev/null 2>&1 &
