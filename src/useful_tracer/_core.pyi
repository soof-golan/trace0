from types import TracebackType
from typing import Literal, Optional

TraceFormat = Literal["json", "protobuf"]


class Tracer:
    def __init__(
        self,
        output: str,
        format: TraceFormat = "json",
        capacity: Optional[int] = None,
    ) -> None: ...

    def start(self) -> None: ...
    def stop(self) -> None: ...
    def __enter__(self) -> "Tracer": ...
    def __exit__(
        self,
        exc_type: Optional[type[BaseException]],
        exc_val: Optional[BaseException],
        exc_tb: Optional[TracebackType],
    ) -> bool: ...
