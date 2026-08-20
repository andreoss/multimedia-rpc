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
export MM_DATABASE_URL="${MM_DATABASE_URL:-postgres://mm:mm@127.0.0.1:5433/mm}"
export MM_GATE_BASE_REF="${MM_GATE_BASE_REF:-master}"
exec cargo xtask ci
