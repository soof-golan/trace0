from typing import Literal

from trace0._core import Tracer

TraceFormat = Literal["json", "protobuf"]

__all__ = ["Tracer", "TraceFormat"]

