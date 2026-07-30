"""`Tracer` is configuration; `Tracer.start()` hands back a running
`Session`. Splitting them removes the states a single object had to
represent and reject: built but not started, started twice, stopped
without starting.

What remains is the interesting one. A second `Session` builds a fresh
queue, interner and thread registry, but the threads that traced the
first still hold the per-thread state it left behind: a write cursor
into the old batch, a cached code id from the old numbering, and a
thread already marked as named. Each of those was a shipped bug.
"""

import json
import threading

import pytest

from trace0 import Tracer
from trace_json import phase, threads_with_slices

WORKERS = 3


def work() -> None:
    sum(range(100))


def workload() -> None:
    workers = [threading.Thread(target=work, name=f"w{i}") for i in range(WORKERS)]
    for t in workers:
        t.start()
    work()
    for t in workers:
        t.join()


def test_every_thread_that_traced_the_first_run_traces_the_second(traced):
    """Compared by thread, not by event count: worker threads are new each
    run and record correctly either way, so a run that lost everything the
    calling thread did still looks mostly populated."""
    first = threads_with_slices(traced(workload))
    second = threads_with_slices(traced(workload))
    assert first == second, f"recorded in run 1 but not run 2: {first - second}"


def test_the_second_run_resolves_names_against_its_own_interner(traced):
    first = {e["name"] for e in phase(traced(work), "B")}
    second = {e["name"] for e in phase(traced(work), "B")}
    assert first == second


def test_one_tracer_starts_as_many_sessions_as_asked(tmp_path):
    """The config carries no run state, so it is reusable by construction."""
    tracer = Tracer(str(tmp_path / "reused.json"), format="json")
    for _ in range(3):
        session = tracer.start()
        work()
        session.stop()
    assert phase(json.loads((tmp_path / "reused.json").read_text()), "B")


def test_repeated_start_stop_cycles_stay_sound(tmp_path):
    """PEP 669 does not promise zero callbacks after `set_events(0)`
    returns. A straggler on the stopping thread must land in a live batch
    rather than through the cursor of one already shipped."""
    for i in range(25):
        path = tmp_path / f"cycle{i}.json"
        session = Tracer(str(path), format="json").start()
        work()
        session.stop()
        assert json.loads(path.read_text())["droppedEvents"] == 0


def test_no_event_precedes_the_clock_anchor(traced):
    trace = traced(workload)
    timed = phase(trace, "B") + phase(trace, "E")
    assert timed
    assert all(e["ts"] >= 0 for e in timed)


def test_a_second_session_cannot_run_beside_the_first(tmp_path):
    """sys.monitoring hands out one PROFILER_ID, so overlapping sessions
    are refused by the interpreter rather than by us."""
    session = Tracer(str(tmp_path / "a.json"), format="json").start()
    try:
        with pytest.raises(ValueError):
            Tracer(str(tmp_path / "b.json"), format="json").start()
    finally:
        session.stop()


def test_stopping_a_session_twice_is_harmless(tmp_path):
    path = tmp_path / "twice.json"
    session = Tracer(str(path), format="json").start()
    work()
    session.stop()
    session.stop()
    assert phase(json.loads(path.read_text()), "B")


def test_the_session_is_the_context_manager(tmp_path):
    path = tmp_path / "ctx.json"
    with Tracer(str(path), format="json").start():
        work()
    assert phase(json.loads(path.read_text()), "B")
