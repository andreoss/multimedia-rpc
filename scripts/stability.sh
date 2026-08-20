#!/bin/sh
set -u

root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$root"

runs="${RUNS:-10}"
slots="${LOAD_SLOTS:-4 7}"
logdir="scratch/stability"
summary="$logdir/summary.txt"

mkdir -p "$logdir"
: > "$summary"

wait_for_stack() {
    attempt=0
    while [ "$attempt" -lt 60 ]; do
        if nc -z -w 2 127.0.0.1 "${MM_DB_PORT:-5433}" 2>/dev/null \
            && nc -z -w 2 127.0.0.1 "${MM_STORE_PORT:-8333}" 2>/dev/null \
            && nc -z -w 2 127.0.0.1 "${MM_QUEUE_PORT:-9324}" 2>/dev/null; then
            return 0
        fi
        attempt=$((attempt + 1))
        sleep 1
    done
    echo "stability: the stack did not answer on every port" >&2
    return 1
}

reset_stack() {
    docker compose down -v >/dev/null 2>&1
    docker compose up -d >/dev/null 2>&1
    wait_for_stack
}

cargo build --workspace --quiet

index=1
while [ "$index" -le "$runs" ]; do
    run=$(printf '%02d' "$index")
    reset_stack || exit 1
    load=""
    for slot in $slots; do
        if [ "$slot" = "$index" ]; then
            sh scripts/parity.sh > "$logdir/load-$run.log" 2>&1 &
            load=$!
        fi
    done
    cargo test --workspace --no-fail-fast > "$logdir/run$run.log" 2>&1
    rc=$?
    passed=$(grep -E '^test result' "$logdir/run$run.log" \
        | awk -F'[ ;]+' '{s += $4} END {print s + 0}')
    failed=$(grep -E '^test result' "$logdir/run$run.log" \
        | awk -F'[ ;]+' '{s += $6} END {print s + 0}')
    echo "run$run rc=$rc passed=$passed failed=$failed" >> "$summary"
    if [ -n "$load" ]; then
        wait "$load"
        load_rc=$?
        echo "load$run rc=$load_rc" >> "$summary"
    fi
    if [ "$rc" -ne 0 ] || [ "$failed" != "0" ]; then
        echo "run$run FAILED" >> "$summary"
    fi
    index=$((index + 1))
done

echo DONE >> "$summary"
cat "$summary"
if grep -q FAILED "$summary"; then
    exit 1
fi