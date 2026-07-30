"""Reading back the Chrome Trace Event JSON the tracer writes."""


def phase(trace: dict, ph: str) -> list[dict]:
    return [e for e in trace["traceEvents"] if e["ph"] == ph]


def slice_names(trace: dict) -> set[str]:
    return {e["name"] for e in phase(trace, "B")}


def thread_names(trace: dict) -> set[str]:
    return {e["args"]["name"] for e in phase(trace, "M")}


def threads_with_slices(trace: dict) -> set[str]:
    named = {e["tid"]: e["args"]["name"] for e in phase(trace, "M")}
    return {named.get(e["tid"], str(e["tid"])) for e in phase(trace, "B")}
