#!/bin/sh
set -eu

root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$root"

reachable() {
    host="$1"
    port="$2"
    name="$3"
    if ! nc -z -w 2 "$host" "$port" 2>/dev/null; then
        echo "parity: $name at $host:$port is unreachable;" \
            "start the stack with 'docker compose up -d'" >&2
        exit 1
    fi
}

reachable 127.0.0.1 "${MM_DB_PORT:-5433}" "database"
reachable 127.0.0.1 "${MM_STORE_PORT:-8333}" "object store"
reachable 127.0.0.1 "${MM_QUEUE_PORT:-9324}" "queue"

echo "parity: building the daemon and the suite"
cargo build -p mmrpcd --quiet

echo "parity: running the suite against the compose stack"
cargo test -p mmrpcd --test parity -- --test-threads=4 --nocapture
