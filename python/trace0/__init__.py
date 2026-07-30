from typing import Literal

from trace0._core import Session, Tracer

TraceFormat = Literal["json", "protobuf"]

__all__ = ["Session", "Tracer", "TraceFormat"]

