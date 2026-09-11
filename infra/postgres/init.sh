#!/bin/sh
# First-start bootstrap for the shared Postgres: pgvector, plus one schema and one login role per service.
# Each role owns its schema and has no grants anywhere else, so Postgres itself enforces the isolation.
# Runs once, when the data volume is empty (docker-entrypoint-initdb.d).
set -eu

psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname "$POSTGRES_DB" \
  -c 'CREATE EXTENSION IF NOT EXISTS vector'

for service in crawler gateway llm_router; do
  password_var="$(echo "$service" | tr '[:lower:]' '[:upper:]')_DB_PASSWORD"
  password="$(printenv "$password_var")" || { echo "$password_var is not set" >&2; exit 1; }

  # search_path keeps public so the pgvector type resolves without a schema prefix.
  psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname "$POSTGRES_DB" \
    -v role="${service}_user" -v schema="$service" -v password="$password" <<'SQL'
CREATE ROLE :"role" LOGIN PASSWORD :'password';
CREATE SCHEMA :"schema" AUTHORIZATION :"role";
ALTER ROLE :"role" SET search_path TO :"schema", public;
SQL
done
