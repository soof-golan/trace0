"""End-to-end check that the built extension actually traces.

Run by CI on every interpreter the release matrix targets. The duration
assertion is the important one: it is what catches a wrong timebase, which
is invisible to a trace that merely parses.
"""

import json
import tempfile
import threading
import time
from pathlib import Path

from trace0 import Tracer

SLEEP = 0.05


def slow_call() -> None:
    time.sleep(SLEEP)


def worker() -> None:
    slow_call()


def trace_to(path: Path) -> tuple[dict, float]:
    """Trace the workload, and time it independently of the tracer."""
    workers = [threading.Thread(target=worker, name=f"w{i}") for i in range(3)]
    tracer = Tracer(str(path), format="json")
    tracer.start()
    # Timed from inside the tracer: clock calibration and the final drain
    # are real costs, but they are not part of any slice.
    started = time.perf_counter()
    for t in workers:
        t.start()
    slow_call()
    for t in workers:
        t.join()
    wall = time.perf_counter() - started
    tracer.stop()
    return json.loads(path.read_text()), wall


def check_slices_are_balanced(events: list[dict]) -> None:
    depth: dict[int, int] = {}
    for e in events:
        if e["ph"] == "B":
            depth[e["tid"]] = depth.get(e["tid"], 0) + 1
        elif e["ph"] == "E":
            depth[e["tid"]] = depth.get(e["tid"], 0) - 1
            assert depth[e["tid"]] >= 0, f"slice stack underflowed on tid {e['tid']}"
    assert all(d == 0 for d in depth.values()), f"unclosed slices: {depth}"


def check_durations_are_wall_clock(events: list[dict], wall: float) -> None:
    """Slice durations must agree with time actually spent.

    Compared against the measured duration of the traced region, not
    against the nominal sleep: a loaded machine overshoots `time.sleep`
    badly, and that inflates both numbers together. A wrong timebase moves
    only one of them, which is what this is here to catch -- the bug that
    shipped scaled every duration by 41.67x.
    """
    opened: dict[tuple[int, str], float] = {}
    longest = 0.0
    for e in events:
        key = (e["tid"], e["name"])
        if e["ph"] == "B":
            opened[key] = e["ts"]
        elif e["ph"] == "E" and key in opened:
            longest = max(longest, e["ts"] - opened.pop(key))
    seconds = longest / 1_000_000
    # The sleep dominates the region, so the longest slice should be a good
    # fraction of it, and cannot meaningfully exceed it.
    assert wall / 10 < seconds < wall * 1.5, (
        f"longest slice was {seconds:.6f}s inside a {wall:.6f}s traced "
        f"region — timebase is wrong"
    )


def check_worker_threads_are_named(events: list[dict]) -> None:
    names = {e["args"]["name"] for e in events if e["ph"] == "M"}
    missing = {"w0", "w1", "w2"} - names
    assert not missing, f"unnamed worker threads: {missing} (saw {names})"


def main() -> None:
    with tempfile.TemporaryDirectory() as d:
        trace, wall = trace_to(Path(d) / "smoke.json")

    events = trace["traceEvents"]
    assert events, "traced a real workload but got no events"
    assert trace["droppedEvents"] == 0, f"dropped {trace['droppedEvents']} events"

    check_slices_are_balanced(events)
    check_durations_are_wall_clock(events, wall)
    check_worker_threads_are_named(events)

    print(
        f"ok: {len(events)} events, {trace['droppedEvents']} dropped, "
        f"{wall * 1000:.1f}ms traced"
    )


if __name__ == "__main__":
    main()
