#!/usr/bin/env bash
# Sweep BATCH_N against the per-event overhead metric.
#
# BATCH_N sets how often the hot path falls into `slow_path`: once every
# BATCH_N events a batch is shipped down the ring and a fresh one is
# allocated, then freed later on the drain thread. Larger batches make
# that rarer, at the cost of BATCHES_CAPACITY * BATCH_N * 8 bytes of
# in-flight buffer per thread and a longer lag before events are visible.
#
# Run at 8 threads: cross-thread allocator traffic is what this tests,
# and it does not exist at one thread.
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
