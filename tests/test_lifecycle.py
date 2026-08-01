"""Tracing is a `with` block and nothing else.

There is no `start`/`stop` pair to call out of order, so a tracer that
was never started, or started twice, or stopped without starting, cannot
be reached from Python at all.

What remains is the interesting case. A second run builds a fresh queue,
interner and thread registry, but the threads that traced the first still
hold the per-thread state it left behind: a write cursor into the old
batch, a cached code id from the old numbering, and a thread already
marked as named. Each of those was a shipped bug.
"""

import json
import threading

import pytest

from trace0 import Tracer
from trace_json import dropped_events, load, phase, threads_with_slices

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


def test_one_tracer_traces_as_many_times_as_asked(tmp_path):
    """The tracer carries no run state between blocks, so it is reusable."""
    tracer = Tracer(str(tmp_path / "reused.json"), format="json")
    for _ in range(3):
        with tracer:
            work()
    assert phase(load(tmp_path / "reused.json"), "B")


def test_repeated_cycles_stay_sound(tmp_path):
    """PEP 669 does not promise zero callbacks after `set_events(0)`
    returns. A straggler on the stopping thread must land in a live batch
    rather than through the cursor of one already shipped."""
    for i in range(25):
        path = tmp_path / f"cycle{i}.json"
        with Tracer(str(path), format="json"):
            work()
        assert dropped_events(load(path)) == 0


def test_no_event_precedes_the_clock_anchor(traced):
    trace = traced(workload)
    timed = phase(trace, "B") + phase(trace, "E")
    assert timed
    assert all(e["ts"] >= 0 for e in timed)


def test_the_block_yields_the_tracer(tmp_path):
    tracer = Tracer(str(tmp_path / "as.json"), format="json")
    with tracer as entered:
        pass
    assert entered is tracer


def test_a_second_tracer_cannot_run_inside_the_first(tmp_path):
    """sys.monitoring hands out one PROFILER_ID, so overlapping runs are
    refused by the interpreter rather than by us. The outer run survives
    it."""
    outer = tmp_path / "outer.json"
    with Tracer(str(outer), format="json"):
        with pytest.raises(ValueError):
            with Tracer(str(tmp_path / "inner.json"), format="json"):
                pass
        work()
    assert phase(load(outer), "B")


def test_a_workload_that_raises_still_leaves_a_trace(tmp_path):
    path = tmp_path / "raised.json"
    with pytest.raises(ZeroDivisionError):
        with Tracer(str(path), format="json"):
            work()
            1 / 0
    assert phase(load(path), "B")
