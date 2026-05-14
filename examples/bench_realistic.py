"""Realistic workload microbenchmark.

Approximates a normal Python program rather than a tight recursive
hot loop. Each "request" does a mix of: list comprehension, dict
construction, integer math, a generator, and a few layers of
function dispatch. Events per request: ~10x batch_size. Untraced
event rate is a few million per second per thread, much closer to
real applications than fib()'s ~60M/s.

Usage:
    uv run python examples/bench_realistic.py [--requests N] [--batch N] [--iters N] [--threads N]
"""

from __future__ import annotations

import argparse
import gc
import json
import statistics
import sys
import tempfile
import threading
import time
from pathlib import Path

from useful_tracer import Tracer


def normalize(x: int, scale: int) -> dict:
    return {"value": x * scale, "squared": x * x, "tag": x & 0xF}


def keep(item: dict) -> bool:
    return item["value"] % 2 == 0 and item["squared"] < 10_000


def summarize(items: list[dict]) -> dict:
    total = sum(i["value"] for i in items)
    tags = {}
    for it in items:
        tags[it["tag"]] = tags.get(it["tag"], 0) + 1
    return {"total": total, "tags": tags}


def handle_request(batch_size: int, scale: int) -> dict:
    raw = list(range(batch_size))
    normalized = [normalize(x, scale) for x in raw]
    filtered = [it for it in normalized if keep(it)]
    return summarize(filtered)


def workload(requests: int, batch: int) -> None:
    for i in range(requests):
        handle_request(batch, 3 + (i & 7))


def time_untraced(requests: int, batch: int, threads: int) -> float:
    gc.disable()
    try:
        t0 = time.perf_counter_ns()
        run(requests, batch, threads)
        t1 = time.perf_counter_ns()
    finally:
        gc.enable()
    return (t1 - t0) / 1e9


def run(requests: int, batch: int, threads: int) -> None:
    if threads <= 0:
        workload(requests, batch)
        return
    ts = [
        threading.Thread(target=workload, args=(requests, batch), name=f"w-{i}")
        for i in range(threads)
    ]
    for t in ts:
        t.start()
    for t in ts:
        t.join()


def time_traced(requests: int, batch: int, threads: int, fmt: str) -> tuple[float, int, int]:
    out = Path(tempfile.mkstemp(suffix=("." + fmt))[1])
    try:
        gc.disable()
        try:
            with Tracer(str(out), fmt):
                t0 = time.perf_counter_ns()
                run(requests, batch, threads)
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
    p.add_argument("--requests", type=int, default=5000, help="requests per thread")
    p.add_argument("--batch", type=int, default=20, help="items per request")
    p.add_argument("--iters", type=int, default=5, help="repetitions per case")
    p.add_argument("--threads", type=int, default=0, help="0 = main only, >0 = N parallel workers")
    p.add_argument("--format", choices=["json", "protobuf"], default="json")
    args = p.parse_args()

    print(f"python: {sys.version.split()[0]}", end="")
    if hasattr(sys, "_is_gil_enabled"):
        print(f"  GIL: {'on' if sys._is_gil_enabled() else 'OFF (free-threaded)'}", end="")
    print()
    print(
        f"workload: handle_request x {args.requests}  batch={args.batch}  "
        f"threads={args.threads}  iters={args.iters}"
    )
    print()

    untraced = [time_untraced(args.requests, args.batch, args.threads) for _ in range(args.iters)]
    print(f"untraced (s):  median={statistics.median(untraced):.4f}  min={min(untraced):.4f}")

    traced_runs = [
        time_traced(args.requests, args.batch, args.threads, args.format) for _ in range(args.iters)
    ]
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
