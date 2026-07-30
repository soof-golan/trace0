"""Starting, stopping, and restarting a tracer inside one process.

A second `Tracer` builds a fresh queue, interner and thread registry, but
the threads that traced the first run still hold the per-thread state it
left behind: a write cursor into the old run's batch, a cached code id
from the old run's numbering, and a thread already marked as named. Each
of those was a shipped bug.
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
    def named_workload():
        work()

    first = {e["name"] for e in phase(traced(named_workload), "B")}
    second = {e["name"] for e in phase(traced(named_workload), "B")}
    assert first == second


def test_repeated_start_stop_cycles_stay_sound(tmp_path):
    """PEP 669 does not promise zero callbacks after `set_events(0)`
    returns. A straggler on the stopping thread must land in a live batch
    rather than through the cursor of one already shipped."""
    for i in range(25):
        path = tmp_path / f"cycle{i}.json"
        tracer = Tracer(str(path), format="json")
        tracer.start()
        work()
        tracer.stop()
        trace = json.loads(path.read_text())
        assert trace["droppedEvents"] == 0


def test_no_event_precedes_the_clock_anchor(traced):
    trace = traced(workload)
    timed = phase(trace, "B") + phase(trace, "E")
    assert timed
    assert all(e["ts"] >= 0 for e in timed)


def test_starting_a_running_tracer_is_refused(tmp_path):
    tracer = Tracer(str(tmp_path / "t.json"), format="json")
    tracer.start()
    try:
        with pytest.raises(RuntimeError, match="already started"):
            tracer.start()
    finally:
        tracer.stop()


def test_stopping_a_tracer_that_never_ran_is_refused(tmp_path):
    tracer = Tracer(str(tmp_path / "t.json"), format="json")
    with pytest.raises(RuntimeError, match="not running"):
        tracer.stop()


def test_the_context_manager_traces_and_closes(tmp_path):
    path = tmp_path / "ctx.json"
    with Tracer(str(path), format="json"):
        work()
    assert phase(json.loads(path.read_text()), "B")
