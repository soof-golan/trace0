"""Thread names come from `threading.current_thread()`.

That returns a `_DummyThread` named `Dummy-N` while the calling thread is
not yet in `threading._active`, which is true for the first PY_START
frames of every new `Thread` -- `_bootstrap_inner` registers it only
after those have already fired. A tracer that asked once and latched on
the answer would leave every worker thread unnamed.
"""

import _thread
import threading

from trace_json import slice_names, thread_names, threads_with_slices

WORKERS = 4


def work() -> None:
    sum(range(100))


def spawn_threads() -> None:
    workers = [
        threading.Thread(target=work, name=f"w{i}") for i in range(WORKERS)
    ]
    for t in workers:
        t.start()
    for t in workers:
        t.join()


def test_worker_threads_report_the_name_they_were_given(traced):
    names = thread_names(traced(spawn_threads))
    expected = {f"w{i}" for i in range(WORKERS)}
    assert expected <= names, f"unnamed workers: {expected - names}"


def test_no_thread_is_recorded_under_its_placeholder_name(traced):
    names = thread_names(traced(spawn_threads))
    assert not {n for n in names if n.startswith("Dummy-")}


def test_every_thread_that_recorded_a_slice_is_named(traced):
    trace = traced(spawn_threads)
    unnamed = {t for t in threads_with_slices(trace) if t.isdigit()}
    assert not unnamed, f"slices on threads with no metadata record: {unnamed}"


def test_a_raw_thread_never_registered_with_threading_is_survivable(traced):
    """`_thread.start_new_thread` never enters `threading._active`, so its
    name stays a placeholder for the thread's whole life. The tracer must
    keep recording rather than latch or fail."""
    done = threading.Event()

    def workload():
        _thread.start_new_thread(lambda: (work(), done.set()), ())
        assert done.wait(5), "raw thread never ran"

    assert thread_names(traced(workload)) is not None


def test_a_thread_still_parked_at_the_end_keeps_its_last_events(traced):
    """A pool thread does not exit when the run ends, so nothing used to
    collect the batch it was still filling. Its newest events were dropped
    silently -- not even counted, because they never reached the queue.
    """
    import threading

    release = threading.Event()
    parked = threading.Event()

    def only_the_parked_thread_calls_this():
        return sum(range(20))

    def park():
        for _ in range(30):
            only_the_parked_thread_calls_this()
        parked.set()
        release.wait(10)

    def workload():
        worker = threading.Thread(target=park, name="parked")
        worker.start()
        parked.wait(10)

    events = traced(workload)
    release.set()

    recorded = slice_names(events)
    assert any("only_the_parked_thread_calls_this" in name for name in recorded)


def test_a_busy_thread_does_not_starve_a_quiet_one(traced):
    """The drain takes only enough for one batch before it stops. Starting
    that walk at the front every time let a thread that always has events
    ready hide every thread registered behind it.
    """
    import threading

    done = threading.Event()

    def quiet_marker():
        return 1

    def loud_marker():
        return 1

    def loud():
        while not done.is_set():
            for _ in range(200):
                loud_marker()

    def quiet():
        for _ in range(300):
            quiet_marker()

    def workload():
        noisy = threading.Thread(target=loud, name="loud")
        noisy.start()
        calm = threading.Thread(target=quiet, name="quiet")
        calm.start()
        calm.join()
        done.set()
        noisy.join()

    recorded = slice_names(traced(workload))
    assert any("quiet_marker" in name for name in recorded), (
        "the quiet thread's events never reached the trace"
    )
