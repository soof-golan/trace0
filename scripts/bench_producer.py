"""Measure tracing overhead per event on the traced thread itself.

Only the traced threads are timed -- the exporter drain is deliberately
excluded, because the question is what tracing costs the program being
traced, not what it costs to write the trace out.

Untraced and traced runs are interleaved rather than run in blocks, so
that thermal drift or a busy machine moves both arms together instead of
landing entirely on one of them. Each arm reports its minimum, which is
the least noise-contaminated sample available.
"""

import statistics
import sys
import threading
import time

from trace0 import Tracer

DEPTH = 27
# fib(n) makes 2*fib(n+1)-1 calls, each of which is a PY_START and a
# PY_RETURN.
CALLS = 2 * 317811 - 1
EVENTS_PER_THREAD = 2 * CALLS
REPS = 9


def fib(n: int) -> int:
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)


def time_threads(n_threads: int) -> float:
    threads = [
        threading.Thread(target=fib, args=(DEPTH,), name=f"w{i}") for i in range(n_threads)
    ]
    start = time.perf_counter()
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    return time.perf_counter() - start


def one_traced(n_threads: int) -> float:
    tracer = Tracer("/dev/null", format="protobuf")
    tracer.start()
    elapsed = time_threads(n_threads)
    tracer.stop()
    return elapsed


def measure(n_threads: int) -> tuple[float, float]:
    base = []
    traced = []
    for _ in range(REPS):
        base.append(time_threads(n_threads))
        traced.append(one_traced(n_threads))
    return min(base), min(traced)


def main() -> None:
    counts = [int(a) for a in sys.argv[1:]] or [1, 2, 4, 8]
    print(f"{'thr':>4} {'untraced':>10} {'traced':>10} {'events':>12} {'overhead':>12}")
    results = []
    for n in counts:
        base, traced = measure(n)
        events = EVENTS_PER_THREAD * n
        overhead_ns = (traced - base) / events * 1e9
        results.append(overhead_ns)
        print(
            f"{n:>4} {base:>9.4f}s {traced:>9.4f}s {events:>12,} "
            f"{overhead_ns:>9.2f} ns/ev"
        )
    print(f"\nsingle-thread overhead: {results[0]:.2f} ns/ev")
    if len(results) > 1:
        print(f"median across counts:   {statistics.median(results):.2f} ns/ev")


if __name__ == "__main__":
    main()
