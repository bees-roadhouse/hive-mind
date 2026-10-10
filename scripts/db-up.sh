#!/usr/bin/env bash
# Bring up a local Postgres for the database tests (D43) and print the
# connection string the tests read. Idempotent: run it as often as you like.
#
#   ./scripts/db-up.sh            # start, wait until it answers, print the URL
#   ./scripts/db-up.sh --quiet    # print only the URL, for `export X=$(...)`
#
# One `podman run`, no compose. The previous version of this script resolved a
# compose provider and, on Windows, picked `podman compose` whenever podman
# existed, never noticing there was no provider behind it (#106). A plain
# container needs nothing resolved.
#
# The password is generated once, when the container is created, and lives
# only in the container's environment; later runs read it back from there.
# Nothing writes it to disk and this script never echoes it except inside the
# URL it prints, which is the whole point of printing the URL.
set -uo pipefail
cd "$(dirname "$0")/.."

quiet=0
[ "${1:-}" = "--quiet" ] && quiet=1
say() { [ "$quiet" -eq 1 ] || echo "$@"; }

name="hive-mind-pg-rust"
image="docker.io/pgvector/pgvector:pg17"
port="${HIVE_SANDBOX_PG_PORT:-55434}"
user="hive"
db="hive_test"

if ! command -v podman >/dev/null 2>&1; then
  echo "podman not found. The test database runs under Podman; docs/development.md installs it." >&2
  exit 1
fi

if ! podman container exists "$name" 2>/dev/null; then
  say "==> creating $name from $image on 127.0.0.1:$port"
  password=$(head -c 24 /dev/urandom | base64 | tr -d '/+=' | head -c 24)
  if ! podman run --detach --name "$name" \
      --publish "127.0.0.1:${port}:5432" \
      --env "POSTGRES_USER=$user" \
      --env "POSTGRES_PASSWORD=$password" \
      --env "POSTGRES_DB=$db" \
      "$image" >/dev/null; then
    echo "podman run failed" >&2
    exit 1
  fi
elif [ "$(podman inspect "$name" --format '{{.State.Status}}')" != "running" ]; then
  say "==> starting $name"
  podman start "$name" >/dev/null || { echo "podman start failed" >&2; exit 1; }
fi

password=$(podman inspect "$name" --format '{{range .Config.Env}}{{println .}}{{end}}' | sed -n 's/^POSTGRES_PASSWORD=//p')
if [ -z "$password" ]; then
  echo "$name has no POSTGRES_PASSWORD in its environment; remove it and run again" >&2
  exit 1
fi
port=$(podman port "$name" 5432 | head -n 1 | sed 's/.*://')
if [ -z "$port" ]; then
  echo "$name publishes no port" >&2
  exit 1
fi

# A listening port is not readiness: during initdb the server accepts local
# connections and then restarts. Poll with a real query, as the host would.
say "==> waiting for Postgres to answer a query"
deadline=$((SECONDS + 120))
until [ "$(podman exec "$name" psql -U "$user" -d "$db" -tAc 'select 1' 2>/dev/null)" = "1" ]; do
  if [ "$SECONDS" -ge "$deadline" ]; then
    echo "Postgres did not answer within 120s. Last log lines:" >&2
    podman logs --tail 30 "$name" >&2
    exit 1
  fi
  sleep 0.5
done

# pgvector is pre-installed in template1 on the cluster (D43, infra notes)
# because an org role cannot create it; the same here, so every database a
# test creates inherits it.
podman exec "$name" psql -U "$user" -d template1 -tAc 'create extension if not exists vector' >/dev/null 2>&1 || true

url="postgres://${user}:${password}@127.0.0.1:${port}/${db}?sslmode=disable"
if [ "$quiet" -eq 1 ]; then
  echo "$url"
  exit 0
fi
echo
echo "POSTGRES READY on 127.0.0.1:$port ($name)"
echo
echo "Point the database tests at it for this shell:"
echo "  export HIVE_SANDBOX_TEST_DATABASE_URL='$url'"
echo "  cargo test -p hive-db -p hive-schema"
echo
echo "Stop it when you are done (it holds ~250 MB of somebody's video otherwise):"
echo "  podman stop $name"
