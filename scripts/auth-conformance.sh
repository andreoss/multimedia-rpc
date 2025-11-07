#!/bin/sh
set -eu

root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$root"

workdir="scratch/s16/auth"
database_url="${MM_DATABASE_URL:-postgres://mm:mm@127.0.0.1:5433/mm}"
store_endpoint="${MM_OBJECTSTORE_ENDPOINT:-http://127.0.0.1:8333}"
bucket="${MM_OBJECTSTORE_BUCKET:-mm-conformance}"
access_key="${MM_OBJECTSTORE_ACCESS_KEY:-mm-test-access}"
secret_key="${MM_OBJECTSTORE_SECRET_KEY:-mm-test-secret}"
region="${MM_OBJECTSTORE_REGION:-us-east-1}"

mkdir -p "$workdir"
jwks="$workdir/published.json"
tokens="$workdir/tokens.json"
log="$workdir/daemon.log"

if ! command -v node >/dev/null 2>&1; then
    echo "auth-conformance: node is not installed; the tokens and the client" \
         "that exercise the service must come from outside this repository" >&2
    exit 1
fi

echo "auth-conformance: building the daemon"
cargo build -p mmrpcd --quiet

if command -v sqlx >/dev/null 2>&1; then
    echo "auth-conformance: applying migrations to $database_url"
    sqlx migrate run --source crates/mm-jobstore/migrations --database-url "$database_url" >/dev/null
fi

echo "auth-conformance: minting a published key and a token set outside this repository"
node scripts/auth-conformance.mjs keys "$jwks" "$tokens"

echo "auth-conformance: starting a server that verifies that key"
MM_BIND_ADDR=127.0.0.1:0 \
MM_DATABASE_URL="$database_url" \
MM_OBJECTSTORE_ENDPOINT="$store_endpoint" \
MM_OBJECTSTORE_BUCKET="$bucket" \
MM_OBJECTSTORE_ACCESS_KEY="$access_key" \
MM_OBJECTSTORE_SECRET_KEY="$secret_key" \
MM_OBJECTSTORE_REGION="$region" \
MM_AUTH_JWKS_FILE="$jwks" \
MM_AUTH_ISSUER=conformance-issuer \
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
        echo "auth-conformance: the server exited before announcing" >&2
        cat "$log" >&2
        exit 1
    fi
    attempt=$((attempt + 1))
    sleep 1
done

if [ -z "$address" ]; then
    echo "auth-conformance: the server never announced an address" >&2
    cat "$log" >&2
    exit 1
fi

echo "auth-conformance: exercising $address from outside this repository"
node scripts/auth-conformance.mjs run "$address" "$tokens"
outcome=$?

if [ "$outcome" -ne 0 ]; then
    echo "auth-conformance: the external run failed; the server said:" >&2
    cat "$log" >&2
fi

exit "$outcome"