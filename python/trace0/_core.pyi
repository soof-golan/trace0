from types import TracebackType
from typing import Literal, Optional

TraceFormat = Literal["json", "protobuf"]


class Session:
    def stop(self) -> None: ...
    def __enter__(self) -> "Session": ...
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
    ) -> None: ...

    def start(self) -> Session: ...


def cli_main(argv: list[str]) -> int: ...
