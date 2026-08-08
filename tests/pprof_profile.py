"""Reading back the pprof profile the tracer writes.

The file is one uncompressed `perftools.profiles.Profile` message. This
decoder walks only the fields the tests assert on, so the tests carry no
protobuf dependency.
"""

from pathlib import Path


def _varint(buf: bytes, at: int) -> tuple[int, int]:
    value = shift = 0
    while True:
        byte = buf[at]
        at += 1
        value |= (byte & 0x7F) << shift
        if not byte & 0x80:
            return value, at
        shift += 7


def _fields(buf: bytes):
    at = 0
    while at < len(buf):
        key, at = _varint(buf, at)
        field, wire = key >> 3, key & 7
        if wire == 0:
            value, at = _varint(buf, at)
        elif wire == 2:
            size, at = _varint(buf, at)
            value, at = buf[at : at + size], at + size
        elif wire == 5:
            value, at = buf[at : at + 4], at + 4
        elif wire == 1:
            value, at = buf[at : at + 8], at + 8
        else:
            raise ValueError(f"unsupported wire type {wire}")
        yield field, wire, value


def _ints(value, wire: int) -> list[int]:
    if wire == 0:
        return [value]
    out, at = [], 0
    while at < len(value):
        item, at = _varint(value, at)
        out.append(item)
    return out


def _value_type(buf: bytes) -> dict:
    out = {"type": 0, "unit": 0}
    for field, _, value in _fields(buf):
        if field == 1:
            out["type"] = value
        elif field == 2:
            out["unit"] = value
    return out


def _sample(buf: bytes) -> dict:
    out = {"location_id": [], "value": []}
    for field, wire, value in _fields(buf):
        if field == 1:
            out["location_id"] += _ints(value, wire)
        elif field == 2:
            out["value"] += _ints(value, wire)
    return out


def _line(buf: bytes) -> dict:
    out = {"function_id": 0, "line": 0}
    for field, _, value in _fields(buf):
        if field == 1:
            out["function_id"] = value
        elif field == 2:
            out["line"] = value
    return out


def _location(buf: bytes) -> dict:
    out = {"id": 0, "lines": []}
    for field, _, value in _fields(buf):
        if field == 1:
            out["id"] = value
        elif field == 4:
            out["lines"].append(_line(value))
    return out


def _function(buf: bytes) -> dict:
    out = {"id": 0, "name": 0, "filename": 0, "start_line": 0}
    for field, _, value in _fields(buf):
        if field == 1:
            out["id"] = value
        elif field == 2:
            out["name"] = value
        elif field == 4:
            out["filename"] = value
        elif field == 5:
            out["start_line"] = value
    return out


def load(path: Path) -> dict:
    buf = Path(path).read_bytes()
    profile = {
        "sample_type": [],
        "samples": [],
        "locations": [],
        "functions": [],
        "strings": [],
        "comments": [],
    }
    for field, wire, value in _fields(buf):
        if field == 1:
            profile["sample_type"].append(_value_type(value))
        elif field == 2:
            profile["samples"].append(_sample(value))
        elif field == 4:
            profile["locations"].append(_location(value))
        elif field == 5:
            profile["functions"].append(_function(value))
        elif field == 6:
            profile["strings"].append(value.decode())
        elif field == 13:
            profile["comments"] += _ints(value, wire)
    return profile


def sample_types(profile: dict) -> list[tuple[str, str]]:
    strings = profile["strings"]
    return [(strings[vt["type"]], strings[vt["unit"]]) for vt in profile["sample_type"]]


def function_names(profile: dict) -> set[str]:
    return {profile["strings"][f["name"]] for f in profile["functions"]}


def total_ns(profile: dict) -> int:
    return sum(s["value"][0] for s in profile["samples"])


def stack_names(profile: dict, sample: dict) -> list[str]:
    locations = {loc["id"]: loc for loc in profile["locations"]}
    functions = {f["id"]: f for f in profile["functions"]}
    return [
        profile["strings"][functions[locations[i]["lines"][0]["function_id"]]["name"]]
        for i in sample["location_id"]
    ]
