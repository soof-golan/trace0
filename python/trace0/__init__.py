import atexit
import os
from typing import Literal, Optional

from trace0._core import Tracer

TraceFormat = Literal["json", "protobuf", "pprof"]

__all__ = ["Tracer", "TraceFormat", "process_startup"]

CHILD_OUTPUT = "TRACE0_CHILD_OUTPUT"
CHILD_FORMAT = "TRACE0_CHILD_FORMAT"

_started: Optional[Tracer] = None


def process_startup() -> Optional[Tracer]:
    """Trace this process because the one that launched it is being traced.

    Called from the `.pth` shipped with the package, which runs before any
    user code in every interpreter that starts while a trace is running. A
    process reached by `fork` is picked up by the fork hooks instead and never
    arrives here, because only `exec` re-runs interpreter startup.

    Calling this twice traces nothing twice: `site` may process the same
    directory more than once, and a second tracer would fight the first for
    the profiler tool id.
    """
    global _started
    if _started is not None:
        return _started

    output = os.environ.get(CHILD_OUTPUT)
    if not output:
        return None

    tracer = Tracer(output, format=os.environ.get(CHILD_FORMAT, "protobuf"))
    tracer._enter_as_child()
    atexit.register(tracer.__exit__, None, None, None)
    _started = tracer
    return tracer
