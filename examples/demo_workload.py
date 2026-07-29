"""Example workload traced with trace0.

Generates two trace files:
  - examples/demo_trace.json  (Chrome / Perfetto JSON)
  - examples/demo_trace.pb    (Perfetto protobuf, length-delimited Trace stream)

Open https://ui.perfetto.dev and drag either file in.
"""

from __future__ import annotations

import threading
import time
from pathlib import Path

from trace0 import Tracer

HERE = Path(__file__).resolve().parent


def fib(n: int) -> int:
    return n if n < 2 else fib(n - 1) + fib(n - 2)


def gen(n: int):
    for i in range(n):
        yield i * i


def consume(g) -> int:
    return sum(g)


def cpu_worker(label: str) -> int:
    total = 0
    for k in range(3):
        total += fib(14 + (k % 2))
    return total


def io_worker(label: str) -> None:
    for _ in range(3):
        time.sleep(0.002)


def run_workload() -> None:
    fib(15)
    consume(gen(50))

    threads = [
        threading.Thread(target=cpu_worker, args=("cpu-a",), name="cpu-a"),
        threading.Thread(target=cpu_worker, args=("cpu-b",), name="cpu-b"),
        threading.Thread(target=io_worker, args=("io-a",), name="io-a"),
    ]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    try:
        raise ValueError("demo unwind")
    except ValueError:
        pass


def main() -> None:
    json_path = HERE / "demo_trace.json"
    pb_path = HERE / "demo_trace.pb"

    with Tracer(str(json_path), "json"):
        run_workload()
    print(f"json: {json_path}  ({json_path.stat().st_size:,} bytes)")

    with Tracer(str(pb_path), "protobuf"):
        run_workload()
    print(f"pb:   {pb_path}  ({pb_path.stat().st_size:,} bytes)")


if __name__ == "__main__":
    main()
