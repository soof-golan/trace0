"""A thread the tracer cannot name must not cost more than one it can.

`threading.current_thread()` answers `Dummy-N` until a thread enters
`threading._active`. For a real `Thread` that window is a few bootstrap
frames; for `_thread.start_new_thread` it never closes. Retrying the
lookup on every event costs four Python API calls per event forever, so
the retry has to give up.
"""

import _thread
import threading
import time

from trace0 import Tracer

RUNS = 3
DEPTH = 20
RATIO = 3.0


def fib(n: int) -> int:
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)


def time_on(spawn, tmp_path, tag: str) -> float:
    best = float("inf")
    for run in range(RUNS):
        elapsed = []
        with Tracer(str(tmp_path / f"{tag}{run}.pb")):
            spawn(elapsed)
        best = min(best, elapsed[0])
    return best


def on_a_named_thread(elapsed: list[float]) -> None:
    def body():
        start = time.perf_counter()
        fib(DEPTH)
        elapsed.append(time.perf_counter() - start)

    t = threading.Thread(target=body, name="named")
    t.start()
    t.join()


def on_a_raw_thread(elapsed: list[float]) -> None:
    done = threading.Event()

    def body():
        start = time.perf_counter()
        fib(DEPTH)
        elapsed.append(time.perf_counter() - start)
        done.set()

    _thread.start_new_thread(body, ())
    assert done.wait(60), "raw thread never ran"


def test_an_unnameable_thread_is_not_punished_for_it(tmp_path):
    named = time_on(on_a_named_thread, tmp_path, "named")
    raw = time_on(on_a_raw_thread, tmp_path, "raw")
    assert raw < named * RATIO, (
        f"raw _thread cost {raw / named:.1f}x a named thread "
        f"({raw * 1000:.0f}ms vs {named * 1000:.0f}ms)"
    )
