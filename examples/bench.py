"""Per-event overhead microbenchmark.

Runs a deterministic CPU-bound workload (recursive fib) twice — once
untraced, once with the tracer attached — and reports the difference per
recorded event. Disable GC during timing so allocator noise doesn't drown
the per-callback signal.

Usage:
    uv run python examples/bench.py [--n 25] [--iters 5] [--threads 0]
"""

from __future__ import annotations

import argparse
import gc
import json
import os
import statistics
import sys
import tempfile
import threading
import time
from pathlib import Path

from useful_tracer import Tracer


def fib(n: int) -> int:
    return n if n < 2 else fib(n - 1) + fib(n - 2)


def workload(n: int, threads: int) -> None:
    if threads <= 0:
        fib(n)
        return
    ts = [threading.Thread(target=fib, args=(n,), name=f"w-{i}") for i in range(threads)]
    for t in ts:
        t.start()
    for t in ts:
        t.join()


def time_run(n: int, threads: int) -> float:
    gc.disable()
    try:
        t0 = time.perf_counter_ns()
        workload(n, threads)
        t1 = time.perf_counter_ns()
    finally:
        gc.enable()
    return (t1 - t0) / 1e9


def time_traced(n: int, threads: int, fmt: str) -> tuple[float, int, int]:
    out = Path(tempfile.mkstemp(suffix=("." + fmt))[1])
    try:
        gc.disable()
        try:
            with Tracer(str(out), fmt):
                t0 = time.perf_counter_ns()
                workload(n, threads)
                t1 = time.perf_counter_ns()
        finally:
            gc.enable()

        events = 0
        dropped = 0
        if fmt == "json":
            with open(out) as f:
                data = json.load(f)
            events = len(data["traceEvents"])
            dropped = data.get("droppedEvents", 0)
        else:
            events = -1
            dropped = -1
        return (t1 - t0) / 1e9, events, dropped
    finally:
        try:
            out.unlink()
        except FileNotFoundError:
            pass


def main() -> None:
    p = argparse.ArgumentParser()
    p.add_argument("--n", type=int, default=25, help="fib argument")
    p.add_argument("--iters", type=int, default=5, help="repetitions per case")
    p.add_argument("--threads", type=int, default=0, help="0 = main only, >0 = N parallel workers")
    p.add_argument("--format", choices=["json", "protobuf"], default="json")
    args = p.parse_args()

    print(f"python: {sys.version.split()[0]}", end="")
    if hasattr(sys, "_is_gil_enabled"):
        print(f"  GIL: {'on' if sys._is_gil_enabled() else 'OFF (free-threaded)'}", end="")
    print()
    print(f"workload: fib({args.n})  threads={args.threads}  iters={args.iters}")
    print()

    untraced = [time_run(args.n, args.threads) for _ in range(args.iters)]
    print(f"untraced (s):  median={statistics.median(untraced):.4f}  min={min(untraced):.4f}")

    traced_runs = [time_traced(args.n, args.threads, args.format) for _ in range(args.iters)]
    times = [t for t, _, _ in traced_runs]
    events = traced_runs[0][1]
    dropped_total = sum(d for _, _, d in traced_runs if d >= 0)
    print(f"traced   (s):  median={statistics.median(times):.4f}  min={min(times):.4f}")
    print(f"events/run:    {events:,}   dropped (sum across runs): {dropped_total}")

    overhead_med = statistics.median(times) - statistics.median(untraced)
    overhead_min = min(times) - min(untraced)
    if events > 0:
        per_event_med = overhead_med * 1e9 / events
        per_event_min = overhead_min * 1e9 / events
        print()
        print(f"overhead (s):  median={overhead_med:.4f}  min={overhead_min:.4f}")
        print(f"per event:     median={per_event_med:.1f} ns   min={per_event_min:.1f} ns")
    print()


if __name__ == "__main__":
    main()
