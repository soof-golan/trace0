"""Reading back the Chrome Trace Event JSON the tracer writes.

The tracer emits the JSON Array Format: entries each followed by a comma, with
no closing bracket. That is what lets every traced process append to one file,
and what lets a process killed mid-run still leave everything up to its last
whole entry.
"""

import json
from pathlib import Path


def load(path: Path) -> list[dict]:
    text = Path(path).read_text().rstrip()
    return json.loads(text.rstrip(",") + "]")


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
