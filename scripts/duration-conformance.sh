#!/bin/sh
set -eu

root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$root"

workdir="scratch/d06/duration"
database_url="${MM_DATABASE_URL:-postgres://mm:mm@127.0.0.1:5433/mm}"
store_endpoint="${MM_OBJECTSTORE_ENDPOINT:-http://127.0.0.1:8333}"
bucket="${MM_OBJECTSTORE_BUCKET:-mm-duration-conformance}"
access_key="${MM_OBJECTSTORE_ACCESS_KEY:-mm-test-access}"
secret_key="${MM_OBJECTSTORE_SECRET_KEY:-mm-test-secret}"
region="${MM_OBJECTSTORE_REGION:-us-east-1}"
queue_endpoint="${MM_QUEUE_ENDPOINT:-http://127.0.0.1:9324}"

mkdir -p "$workdir"
log="$workdir/daemon.log"

for tool in node ffmpeg psql; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "duration-conformance: $tool is not installed; the harness that" \
             "decodes the callback must come from outside this repository" >&2
        exit 1
    fi
done

echo "duration-conformance: building the daemon"
cargo build -p mmrpcd --quiet

duration_db="mm_duration_$(date +%s)_$$"
duration_base="$(printf '%s' "$database_url" | sed 's#/[^/]*$#/#')"
duration_url="${duration_base}${duration_db}"
psql "$database_url" -c "CREATE DATABASE $duration_db" >/dev/null

cleanup_db() {
    psql "$database_url" -c "DROP DATABASE IF EXISTS $duration_db WITH (FORCE)" >/dev/null 2>&1 || true
}

echo "duration-conformance: starting a combined server and worker"
MM_BIND_ADDR=127.0.0.1:0 \
MM_DATABASE_URL="$duration_url" \
MM_MIGRATE=on \
MM_OBJECTSTORE_ENDPOINT="$store_endpoint" \
MM_OBJECTSTORE_BUCKET="$bucket" \
MM_OBJECTSTORE_ACCESS_KEY="$access_key" \
MM_OBJECTSTORE_SECRET_KEY="$secret_key" \
MM_OBJECTSTORE_REGION="$region" \
MM_QUEUE_ENDPOINT="$queue_endpoint" \
MM_QUEUE_REGION=local \
MM_QUEUE_ACCESS_KEY=queue-access \
MM_QUEUE_SECRET_KEY=queue-secret \
MM_WORKER_STAGING_DIR="$workdir/staging" \
MM_WORKER_RECONCILE_SECS=1 \
MM_WORKER_PROGRESS_SECS=1 \
    target/debug/mmrpcd combined >"$log" 2>&1 &
daemon=$!

cleanup() {
    kill "$daemon" 2>/dev/null || true
    wait "$daemon" 2>/dev/null || true
    cleanup_db
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
        echo "duration-conformance: the daemon exited before announcing" >&2
        cat "$log" >&2
        exit 1
    fi
    attempt=$((attempt + 1))
    sleep 1
done

if [ -z "$address" ]; then
    echo "duration-conformance: the daemon never announced an address" >&2
    cat "$log" >&2
    exit 1
fi

echo "duration-conformance: decoding the terminal callback from outside this repository"
MM_QUEUE_ENDPOINT="$queue_endpoint" node scripts/duration-conformance.mjs "$address"
outcome=$?

if [ "$outcome" -ne 0 ]; then
    echo "duration-conformance: the external run failed; the daemon said:" >&2
    cat "$log" >&2
fi

exit "$outcome"
