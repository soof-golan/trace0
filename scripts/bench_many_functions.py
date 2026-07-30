"""Overhead when no two consecutive events share a code object.

`bench_producer.py` recurses through a single function, so the one cached
code object in the thread's hot state hits on every event. That is the
best case, and it hides the cost of resolving a code object entirely.

This is the other end: enough distinct functions, called round-robin,
that the cached entry misses every single time. Together the two bracket
what any real program pays -- real code sits somewhere between, and where
it sits is a property of the program, not of the tracer.
"""

import sys
import time

from trace0 import Tracer

N_FUNCS = 64
ROUNDS = 20_000
REPS = 7
EVENTS = 2 * N_FUNCS * ROUNDS

# Built by exec so each one is a genuinely distinct code object; 64
# copies of the same source would share one.
_defs: dict[str, object] = {}
for _i in range(N_FUNCS):
    exec(f"def f{_i}(): pass", _defs)
FUNCS = [_defs[f"f{i}"] for i in range(N_FUNCS)]


def workload() -> None:
    for _ in range(ROUNDS):
        for f in FUNCS:
            f()


def timed(traced: bool) -> float:
    """Time the workload alone -- start-up and the final drain are real
    costs but they are not what this measures."""
    if not traced:
        start = time.perf_counter()
        workload()
        return time.perf_counter() - start
    tracer = Tracer("/dev/null", format="protobuf")
    tracer.start()
    start = time.perf_counter()
    workload()
    elapsed = time.perf_counter() - start
    tracer.stop()
    return elapsed


def main() -> None:
    base, traced = [], []
    for _ in range(REPS):
        base.append(timed(False))
        traced.append(timed(True))
    overhead = (min(traced) - min(base)) / EVENTS * 1e9
    print(
        f"{N_FUNCS} functions round-robin: untraced {min(base):.4f}s  "
        f"traced {min(traced):.4f}s  {EVENTS:,} events"
    )
    print(f"overhead: {overhead:.2f} ns/ev (every event a code-cache miss)")


if __name__ == "__main__":
    sys.exit(main())
