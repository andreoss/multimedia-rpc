#!/bin/sh
set -eu

root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$root"

workdir="scratch/ladder-conformance"
database_url="${MM_DATABASE_URL:-postgres://mm:mm@127.0.0.1:5433/mm}"
store_endpoint="${MM_OBJECTSTORE_ENDPOINT:-http://127.0.0.1:8333}"
bucket="${MM_OBJECTSTORE_BUCKET:-mm-ladder-conformance}"
access_key="${MM_OBJECTSTORE_ACCESS_KEY:-mm-test-access}"
secret_key="${MM_OBJECTSTORE_SECRET_KEY:-mm-test-secret}"
region="${MM_OBJECTSTORE_REGION:-us-east-1}"
queue_endpoint="${MM_QUEUE_ENDPOINT:-http://127.0.0.1:9324}"

mkdir -p "$workdir"
log="$workdir/daemon.log"

for tool in node ffmpeg psql aws; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "ladder-conformance: $tool is not installed; the harness that" \
             "reads the playlists must come from outside this repository" >&2
        exit 1
    fi
done

head_bucket() {
    AWS_ACCESS_KEY_ID="$access_key" AWS_SECRET_ACCESS_KEY="$secret_key" AWS_DEFAULT_REGION="$region" \
        aws --endpoint-url "$store_endpoint" s3api head-bucket --bucket "$1" >/dev/null 2>&1
}

echo "ladder-conformance: building the daemon"
cargo build -p mmrpcd --quiet

store_write_probe() {
    AWS_ACCESS_KEY_ID="$access_key" AWS_SECRET_ACCESS_KEY="$secret_key" AWS_DEFAULT_REGION="$region" \
        aws --endpoint-url "$store_endpoint" s3api put-object \
        --bucket "$1" --key "probes/ladder-conformance" --body "$workdir/probe.txt" 2>&1
}

echo "ladder-conformance: making sure the bucket takes a write"
echo probe >"$workdir/probe.txt"
if ! head_bucket "$bucket"; then
    AWS_ACCESS_KEY_ID="$access_key" AWS_SECRET_ACCESS_KEY="$secret_key" AWS_DEFAULT_REGION="$region" \
        aws --endpoint-url "$store_endpoint" s3api create-bucket --bucket "$bucket" >/dev/null 2>&1 || true
fi

attempt=0
while [ "$attempt" -lt 10 ]; do
    if store_write_probe "$bucket" >/dev/null 2>&1; then
        break
    fi
    attempt=$((attempt + 1))
    sleep 1
done

if [ "$attempt" -ge 10 ]; then
    echo "ladder-conformance: the object store refuses writes into $bucket:" >&2
    store_write_probe "$bucket" >&2 || true
    echo "ladder-conformance: the store answers writes into its older buckets and not" >&2
    echo "ladder-conformance: into this one, so the harness cannot claim a ladder it" >&2
    echo "ladder-conformance: cannot read back. Point MM_OBJECTSTORE_BUCKET at a bucket" >&2
    echo "ladder-conformance: the store already serves." >&2
    exit 1
fi

ladder_db="mm_ladder_$(date +%s)_$"
ladder_base="$(printf '%s' "$database_url" | sed 's#/[^/]*$#/#')"
ladder_url="${ladder_base}${ladder_db}"
psql "$database_url" -c "CREATE DATABASE $ladder_db" >/dev/null

cleanup_db() {
    psql "$database_url" -c "DROP DATABASE IF EXISTS $ladder_db WITH (FORCE)" >/dev/null 2>&1 || true
}

echo "ladder-conformance: starting a combined server and worker"
MM_BIND_ADDR=127.0.0.1:0 \
MM_DATABASE_URL="$ladder_url" \
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
        echo "ladder-conformance: the daemon exited before announcing" >&2
        cat "$log" >&2
        exit 1
    fi
    attempt=$((attempt + 1))
    sleep 1
done

if [ -z "$address" ]; then
    echo "ladder-conformance: the daemon never announced an address" >&2
    cat "$log" >&2
    exit 1
fi

echo "ladder-conformance: submitting a ladder and reading it back from outside"
set +e
MM_OBJECTSTORE_ENDPOINT="$store_endpoint" \
MM_OBJECTSTORE_BUCKET="$bucket" \
MM_OBJECTSTORE_ACCESS_KEY="$access_key" \
MM_OBJECTSTORE_SECRET_KEY="$secret_key" \
MM_OBJECTSTORE_REGION="$region" \
    node scripts/ladder-conformance.mjs "$address"
outcome=$?
set -e

if [ "$outcome" -ne 0 ]; then
    echo "ladder-conformance: the external run failed; the daemon said:" >&2
    cat "$log" >&2
fi

exit "$outcome"