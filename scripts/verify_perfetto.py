"""Validate a trace with Perfetto's own trace_processor.

The Rust tests decode our output with prost, which only proves we can read
back what we wrote. This runs the real consumer: it resolves interned
names, pairs slice begins with ends, and builds the thread tracks. Every
encoding trick -- interning, bare slice ends, dense track uuids -- is only
correct if this still produces the right slices.

Needs the `perfetto` package, which downloads a trace_processor binary on
first use:

    uv run --with perfetto scripts/verify_perfetto.py
"""

import sys
import tempfile
import threading
import time
from pathlib import Path

from perfetto.trace_processor import TraceProcessor

from trace0 import Tracer

WORKERS = 3
DEPTH = 18
SLEEP = 0.05


def fib(n: int) -> int:
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)


def worker() -> None:
    fib(DEPTH)
    time.sleep(SLEEP)


def record(path: Path) -> float:
    threads = [threading.Thread(target=worker, name=f"w{i}") for i in range(WORKERS)]
    with Tracer(str(path), format="protobuf"):
        started = time.perf_counter()
        for t in threads:
            t.start()
        fib(DEPTH)
        for t in threads:
            t.join()
        wall = time.perf_counter() - started
    return wall


def one(tp: TraceProcessor, sql: str):
    return list(tp.query(sql))[0]


def check(tp: TraceProcessor, wall: float) -> None:
    slices = one(tp, "select count(*) as n from slice").n
    assert slices > 0, "trace_processor found no slices at all"

    # A slice exists only if an end was matched to a begin. Unmatched
    # events would silently vanish here rather than error.
    unfinished = one(tp, "select count(*) as n from slice where dur < 0").n
    assert unfinished == 0, f"{unfinished} slices never closed"

    # Interning is only correct if the consumer can resolve every id.
    nameless = one(tp, "select count(*) as n from slice where name is null or name = ''").n
    assert nameless == 0, f"{nameless} slices lost their name through interning"

    names = {r.name for r in tp.query("select distinct name from slice")}
    for expected in ("fib", "worker"):
        assert expected in names, f"{expected!r} missing from {sorted(names)[:10]}"

    # Thread tracks come from the descriptors, not the events.
    threads = {r.name for r in tp.query("select distinct name from thread where name is not null")}
    missing = {f"w{i}" for i in range(WORKERS)} - threads
    assert not missing, f"unnamed worker threads: {missing} (saw {threads})"

    # Durations must reflect real time. A wrong timebase scales these
    # without breaking anything else in the trace.
    longest = one(tp, "select max(dur) as n from slice").n
    seconds = longest / 1e9
    assert wall / 10 < seconds < wall * 1.5, (
        f"longest slice {seconds:.6f}s inside a {wall:.6f}s traced region"
    )

    # Every slice must sit on a real thread track.
    orphaned = one(
        tp,
        "select count(*) as n from slice s left join thread_track t "
        "on s.track_id = t.id where t.id is null",
    ).n
    assert orphaned == 0, f"{orphaned} slices are not attached to a thread track"

    print(
        f"ok: {slices:,} slices, {len(names)} distinct names, "
        f"{len(threads)} threads, longest {seconds * 1000:.1f}ms in "
        f"{wall * 1000:.1f}ms"
    )


def main() -> int:
    with tempfile.TemporaryDirectory() as d:
        path = Path(d) / "verify.pb"
        wall = record(path)
        assert path.stat().st_size > 0, "tracer produced an empty file"
        tp = TraceProcessor(trace=str(path))
        try:
            check(tp, wall)
        finally:
            tp.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
