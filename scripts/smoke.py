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


def trace_to(path: Path) -> dict:
    workers = [threading.Thread(target=worker, name=f"w{i}") for i in range(3)]
    with Tracer(str(path), format="json"):
        for t in workers:
            t.start()
        slow_call()
        for t in workers:
            t.join()
    return json.loads(path.read_text())


def check_slices_are_balanced(events: list[dict]) -> None:
    depth: dict[int, int] = {}
    for e in events:
        if e["ph"] == "B":
            depth[e["tid"]] = depth.get(e["tid"], 0) + 1
        elif e["ph"] == "E":
            depth[e["tid"]] = depth.get(e["tid"], 0) - 1
            assert depth[e["tid"]] >= 0, f"slice stack underflowed on tid {e['tid']}"
    assert all(d == 0 for d in depth.values()), f"unclosed slices: {depth}"


def check_durations_are_wall_clock(events: list[dict]) -> None:
    """A 50ms sleep must read as ~50ms, not 1.2ms and not 2s."""
    opened: dict[tuple[int, str], float] = {}
    longest = 0.0
    for e in events:
        key = (e["tid"], e["name"])
        if e["ph"] == "B":
            opened[key] = e["ts"]
        elif e["ph"] == "E" and key in opened:
            longest = max(longest, e["ts"] - opened.pop(key))
    seconds = longest / 1_000_000
    assert SLEEP * 0.8 < seconds < SLEEP * 4, (
        f"longest slice was {seconds:.6f}s, expected ~{SLEEP}s — timebase is wrong"
    )


def check_worker_threads_are_named(events: list[dict]) -> None:
    names = {e["args"]["name"] for e in events if e["ph"] == "M"}
    missing = {"w0", "w1", "w2"} - names
    assert not missing, f"unnamed worker threads: {missing} (saw {names})"


def main() -> None:
    with tempfile.TemporaryDirectory() as d:
        trace = trace_to(Path(d) / "smoke.json")

    events = trace["traceEvents"]
    assert events, "traced a real workload but got no events"
    assert trace["droppedEvents"] == 0, f"dropped {trace['droppedEvents']} events"

    check_slices_are_balanced(events)
    check_durations_are_wall_clock(events)
    check_worker_threads_are_named(events)

    print(f"ok: {len(events)} events, {trace['droppedEvents']} dropped")


if __name__ == "__main__":
    main()
