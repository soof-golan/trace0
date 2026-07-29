#!/usr/bin/env bash
# Sweep BATCH_N and rebuild + bench at threads=1 and threads=4.
set -e
cd "$(dirname "$0")/.."

sizes=(64 256 1024 4096 16384)
for n in "${sizes[@]}"; do
    sed -i '' "s/pub const BATCH_N: usize = [0-9]*;/pub const BATCH_N: usize = ${n};/" crates/core/src/evqueue.rs
    uv run --with maturin maturin develop --release 2>&1 | tail -1 > /dev/null
    echo "=== BATCH_N=${n} ==="
    echo "-- threads=1 --"
    uv run python examples/bench_realistic.py --threads 1 --iters 10 --requests 10000 2>&1 | tail -3 | head -2
    echo "-- threads=4 --"
    uv run python examples/bench_realistic.py --threads 4 --iters 10 --requests 10000 2>&1 | tail -3 | head -2
done

sed -i '' "s/pub const BATCH_N: usize = [0-9]*;/pub const BATCH_N: usize = 1024;/" crates/core/src/evqueue.rs
uv run --with maturin maturin develop --release 2>&1 | tail -1 > /dev/null
