#!/bin/sh
set -eu

root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$root"

workdir="scratch/s28/merge"
database_url="${MM_DATABASE_URL:-postgres://mm:mm@127.0.0.1:5433/mm}"
store_endpoint="${MM_OBJECTSTORE_ENDPOINT:-http://127.0.0.1:8333}"
bucket="${MM_OBJECTSTORE_BUCKET:-mm-merge-conformance}"
access_key="${MM_OBJECTSTORE_ACCESS_KEY:-mm-test-access}"
secret_key="${MM_OBJECTSTORE_SECRET_KEY:-mm-test-secret}"
region="${MM_OBJECTSTORE_REGION:-us-east-1}"

mkdir -p "$workdir"
log="$workdir/daemon.log"

if ! command -v node >/dev/null 2>&1; then
    echo "merge-conformance: node is not installed; the client that exercises" \
         "the reference facade must come from outside this repository" >&2
    exit 1
fi

echo "merge-conformance: building the daemon"
cargo build -p mmrpcd --quiet

echo "merge-conformance: starting a server that speaks the reference facade"
MM_BIND_ADDR=127.0.0.1:0 \
MM_DATABASE_URL="$database_url" \
MM_MIGRATE=on \
MM_OBJECTSTORE_ENDPOINT="$store_endpoint" \
MM_OBJECTSTORE_BUCKET="$bucket" \
MM_OBJECTSTORE_ACCESS_KEY="$access_key" \
MM_OBJECTSTORE_SECRET_KEY="$secret_key" \
MM_OBJECTSTORE_REGION="$region" \
    target/debug/mmrpcd server >"$log" 2>&1 &
daemon=$!

cleanup() {
    kill "$daemon" 2>/dev/null || true
    wait "$daemon" 2>/dev/null || true
}
trap cleanup EXIT

address=""
attempt=0
while [ "$attempt" -lt 100 ]; do
    if [ -s "$log" ]; then
        address=$(sed -n 's/^listening //p' "$log" | head -n 1)
        [ -n "$address" ] && break
    fi
    if ! kill -0 "$daemon" 2>/dev/null; then
        echo "merge-conformance: the server exited before announcing" >&2
        cat "$log" >&2
        exit 1
    fi
    attempt=$((attempt + 1))
    sleep 1
done

if [ -z "$address" ]; then
    echo "merge-conformance: the server never announced an address" >&2
    cat "$log" >&2
    exit 1
fi

echo "merge-conformance: driving the four reference RPCs from outside this repository"
node scripts/merge-conformance.mjs "$address"
outcome=$?

if [ "$outcome" -ne 0 ]; then
    echo "merge-conformance: the external run failed; the server said:" >&2
    cat "$log" >&2
fi

exit "$outcome"