#!/bin/sh
set -eu

root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$root"

reachable() {
    host="$1"
    port="$2"
    name="$3"
    if ! nc -z -w 2 "$host" "$port" 2>/dev/null; then
        echo "composition-conformance: $name at $host:$port is unreachable;" \
            "start the stack with 'docker compose up -d'" >&2
        exit 1
    fi
}

reachable 127.0.0.1 "${MM_DB_PORT:-5433}" "database"
reachable 127.0.0.1 "${MM_STORE_PORT:-8333}" "object store"
reachable 127.0.0.1 "${MM_QUEUE_PORT:-9324}" "queue"

echo "composition-conformance: building the daemon and the black-box client"
cargo build -p mmrpcd --quiet

echo "composition-conformance: exercising a batch, a graph and a template" \
    "against a daemon the client does not link"
cargo test -p mmrpcd --test parity composition -- --test-threads=1 --nocapture
