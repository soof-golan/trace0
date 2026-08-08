from types import TracebackType
from typing import Literal, Optional

TraceFormat = Literal["json", "protobuf", "pprof"]


class Snapshot:
    path: Optional[str]

    def __enter__(self) -> "Snapshot": ...
    def __exit__(
        self,
        exc_type: Optional[type[BaseException]],
        exc_val: Optional[BaseException],
        exc_tb: Optional[TracebackType],
    ) -> bool: ...


class Tracer:
    def __init__(
        self,
        output: str,
        format: TraceFormat = "protobuf",
        trace_subprocesses: bool = True,
        record_last_mb: Optional[int] = None,
    ) -> None: ...

    def __enter__(self) -> "Tracer": ...
    def __exit__(
        self,
        exc_type: Optional[type[BaseException]],
        exc_val: Optional[BaseException],
        exc_tb: Optional[TracebackType],
    ) -> bool: ...

    def snapshot(
        self,
        reason: str,
        start: Optional[int] = None,
        end: Optional[int] = None,
    ) -> "Snapshot | str": ...

    def dump(self, reason: str) -> str: ...

    def now_ns(self) -> int: ...


def cli_main(argv: list[str]) -> int: ...
