#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

RUNS=${RUNS:-3}
THREADS=${THREADS:-8}
read -r -a sizes <<<"${SIZES:-1024 4096 16384}"

build_with() {
    sed -i '' "s/pub const BATCH_N: usize = [0-9]*;/pub const BATCH_N: usize = ${1};/" \
        crates/core/src/evqueue.rs
    uv run --with maturin maturin develop --release >/dev/null 2>&1
}

trap 'git checkout -- crates/core/src/evqueue.rs' EXIT

for n in "${sizes[@]}"; do
    build_with "$n"
    printf '=== BATCH_N=%-6s %2d KiB/batch, %2d MiB/thread in flight ===\n' \
        "$n" "$((n * 8 / 1024))" "$((n * 8 * 64 / 1048576))"
    for _ in $(seq "$RUNS"); do
        uv run scripts/bench_producer.py "$THREADS" | tail -3 | head -1
    done
done
