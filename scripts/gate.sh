#!/bin/sh
set -eu

root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"

if [ -z "${MM_GATE_SHELL:-}" ] && [ "${IN_NIX_SHELL:-}" != impure ]; then
    MM_GATE_SHELL=1
    export MM_GATE_SHELL
    exec nix --extra-experimental-features 'nix-command flakes' develop \
        -c "$root/scripts/gate.sh" "$@"
fi

cd "$root"

database_url="${MM_DATABASE_URL:-postgres://mm:mm@127.0.0.1:5433/mm}"
base_ref="${MM_GATE_BASE_REF:-master}"
base_ref="${base_ref#origin/}"

DATABASE_URL="$database_url"
export DATABASE_URL

echo "gate: migrating the database the sqlx macros compile against"
sqlx migrate run --source crates/mm-jobstore/migrations

echo "gate: format"
cargo fmt --all --check

echo "gate: lint"
cargo clippy --workspace --all-targets -- -D warnings

echo "gate: contract"
buf lint
buf breaking --against ".git#branch=$base_ref"

threads="${MM_GATE_TEST_THREADS:-4}"
echo "gate: suite"
mem_total=$(awk '/^MemTotal:/ {print $2}' /proc/meminfo)
mem_available=$(awk '/^MemAvailable:/ {print $2}' /proc/meminfo)
echo "gate: host memory at start: $((mem_total / 1024)) MB total, $((mem_available / 1024)) MB available"
echo "gate: running each test binary at $threads threads (MM_GATE_TEST_THREADS overrides)"
cargo test --workspace --no-fail-fast -- --test-threads="$threads"

echo "gate: coverage"
cargo llvm-cov --workspace --no-fail-fast --fail-under-lines 85 -- --test-threads="$threads"

echo "gate: green"