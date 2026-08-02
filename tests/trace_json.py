"""Reading back the Chrome Trace Event JSON the tracer writes.

The tracer emits the JSON Array Format: entries each followed by a comma, with
no closing bracket. That is what lets every traced process append to one file,
and what lets a process killed mid-run still leave everything up to its last
whole entry.
"""

import json
from collections.abc import Iterable, Iterator
from pathlib import Path

CHUNK = 1 << 20


def _start_of_next(buf: str, at: int) -> int | None:
    while at < len(buf) and buf[at] in ", \n\t":
        at += 1
    return at if at < len(buf) else None


def iter_events(path: Path) -> Iterator[dict]:
    """Decode the entries one at a time, holding only a window in memory.

    A server traced across four workers writes hundreds of megabytes onto a
    single line, which is more than a test wants to hold as one string.
    """
    decoder = json.JSONDecoder()
    with Path(path).open() as handle:
        assert handle.read(1) == "[", "trace does not open a JSON array"
        buf = ""
        while chunk := handle.read(CHUNK):
            buf += chunk
            at = 0
            while (start := _start_of_next(buf, at)) is not None:
                try:
                    event, at = decoder.raw_decode(buf, start)
                except ValueError:
                    break
                yield event
            buf = buf[at:]


def load(path: Path) -> list[dict]:
    return list(iter_events(path))


def phase(events: list[dict], ph: str) -> list[dict]:
    return [e for e in events if e["ph"] == ph]


def named_meta(events: list[dict]) -> list[dict]:
    return [e for e in phase(events, "M") if e["name"] == "thread_name"]


def slice_names(events: list[dict]) -> set[str]:
    return {e["name"] for e in phase(events, "B")}


def thread_names(events: list[dict]) -> set[str]:
    return {e["args"]["name"] for e in named_meta(events)}


def threads_with_slices(events: list[dict]) -> set[str]:
    named = {e["tid"]: e["args"]["name"] for e in named_meta(events)}
    return {named.get(e["tid"], str(e["tid"])) for e in phase(events, "B")}


def dropped_events(events: list[dict]) -> int:
    return sum(
        e["args"]["count"] for e in events if e.get("name") == "trace0_dropped_events"
    )


def pids(events: list[dict]) -> set[int]:
    return {e["pid"] for e in events}


def pids_calling(events: Iterable[dict], name: str) -> set[int]:
    return {e["pid"] for e in events if e["ph"] == "B" and e["name"] == name}


def threads_calling(events: Iterable[dict], name: str) -> dict[str, int]:
    """Count the calls to `name` per thread, keyed by the thread's own name.

    Thread names are written when a process finishes, so the count and the
    naming can only be joined once the whole trace has been read.
    """
    calls: dict[tuple[int, int], int] = {}
    named: dict[tuple[int, int], str] = {}
    for e in events:
        key = (e["pid"], e["tid"])
        if e["ph"] == "B" and e["name"] == name:
            calls[key] = calls.get(key, 0) + 1
        elif e["ph"] == "M" and e["name"] == "thread_name":
            named[key] = e["args"]["name"]
    return {named.get(key, str(key[1])): n for key, n in calls.items()}
