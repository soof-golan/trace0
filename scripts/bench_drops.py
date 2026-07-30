"""Measure event loss as thread count rises.

Emits a JSON trace per thread count and reports how many events survived.
Usage: bench_drops.py [threads...]
"""

import json
import sys
import tempfile
import threading
import time
from pathlib import Path

from trace0 import Tracer

DEPTH = 24


def fib(n: int) -> int:
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)


def run(n_threads: int) -> dict:
    threads = [
        threading.Thread(target=fib, args=(DEPTH,), name=f"w{i}") for i in range(n_threads)
    ]
    with tempfile.TemporaryDirectory() as d:
        out = Path(d) / "bench.json"
        start = time.perf_counter()
        with Tracer(str(out), format="json").start():
            for t in threads:
                t.start()
            for t in threads:
                t.join()
        elapsed = time.perf_counter() - start
        trace = json.loads(out.read_text())

    kept = sum(1 for e in trace["traceEvents"] if e["ph"] in ("B", "E"))
    dropped = trace["droppedEvents"]
    total = kept + dropped
    return {
        "threads": n_threads,
        "elapsed_s": elapsed,
        "kept": kept,
        "dropped": dropped,
        "total": total,
        "loss_pct": 100.0 * dropped / total if total else 0.0,
        "events_per_s": total / elapsed if elapsed else 0.0,
    }


def main() -> None:
    counts = [int(a) for a in sys.argv[1:]] or [1, 2, 4, 8]
    print(f"{'thr':>4} {'elapsed':>9} {'total':>12} {'kept':>12} {'dropped':>12} {'loss':>7} {'ev/s':>12}")
    for n in counts:
        r = run(n)
        print(
            f"{r['threads']:>4} {r['elapsed_s']:>8.3f}s {r['total']:>12,} {r['kept']:>12,} "
            f"{r['dropped']:>12,} {r['loss_pct']:>6.1f}% {r['events_per_s']:>12,.0f}"
        )


if __name__ == "__main__":
    main()
