from typing import Literal

from useful_tracer._core import Tracer

TraceFormat = Literal["json", "protobuf"]

__all__ = ["Tracer", "TraceFormat"]

