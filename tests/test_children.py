"""Every traced process writes into one file, while it runs.

A child reaches tracing one of two ways. `fork` clones the address space, so
the child inherits an exporter thread that no longer runs and locks nobody will
release; the fork hooks hand it a run of its own instead. `exec` shares nothing
but the environment, so the `.pth` picks that child up at interpreter startup.
Either way the events land in the file the launched process opened, and stay
separable by pid once they are interleaved there.
"""

import os
import subprocess
import sys
from pathlib import Path

import pytest
from trace_json import dropped_events, load, named_meta, phase, pids, slice_names

from trace0 import Tracer

FORK_ONLY = pytest.mark.skipif(
    not hasattr(os, "fork"), reason="fork is not available on this platform"
)


def run_script(
    tmp_path: Path,
    body: str,
    out: Path,
    fmt: str = "json",
    flags: tuple[str, ...] = (),
) -> str:
    script = tmp_path / "workload.py"
    script.write_text(body)
    result = subprocess.run(
        [
            *(sys.executable, "-m", "trace0", "run"),
            *("-o", str(out), "-f", fmt),
            *flags,
            str(script),
        ],
        capture_output=True,
        text=True,
        timeout=180,
    )
    assert result.returncode == 0, result.stderr
    return result.stdout


def sibling_files(out: Path) -> list[Path]:
    return sorted(p for p in out.parent.glob(f"{out.name}.*"))


def names_by_pid(out: Path) -> dict[int, set[str]]:
    events = load(out)
    by_pid: dict[int, set[str]] = {}
    for e in phase(events, "B"):
        by_pid.setdefault(e["pid"], set()).add(e["name"])
    return by_pid


FORKING_WORKLOAD = """
import os, sys

def only_the_child_calls_this():
    return sum(i * i for i in range(200))

def only_the_parent_calls_this():
    return sum(i for i in range(200))

pid = os.fork()
if pid == 0:
    only_the_child_calls_this()
    sys.exit(0)
os.waitpid(pid, 0)
only_the_parent_calls_this()
"""


@FORK_ONLY
def test_a_fork_puts_both_processes_in_one_file(tmp_path: Path):
    out = tmp_path / "t.json"
    run_script(tmp_path, FORKING_WORKLOAD, out)

    assert sibling_files(out) == [], "a child wrote its own file"
    names = slice_names(load(out))
    assert "only_the_parent_calls_this" in names
    assert "only_the_child_calls_this" in names


@FORK_ONLY
def test_the_two_processes_stay_apart_inside_that_file(tmp_path: Path):
    out = tmp_path / "t.json"
    run_script(tmp_path, FORKING_WORKLOAD, out)

    by_pid = names_by_pid(out)
    assert len(by_pid) == 2, f"expected two processes, got {sorted(by_pid)}"
    parent = next(p for p, n in by_pid.items() if "only_the_parent_calls_this" in n)
    child = next(p for p, n in by_pid.items() if "only_the_child_calls_this" in n)
    assert parent != child
    assert "only_the_child_calls_this" not in by_pid[parent]
    assert "only_the_parent_calls_this" not in by_pid[child]


SPAWNING_WORKLOAD = """
import subprocess, sys
subprocess.run(
    [sys.executable, "-c", "def spawned_work():\\n    return sum(range(300))\\nspawned_work()\\n"],
    check=True,
)
def parent_work():
    return sum(range(100))
parent_work()
"""


def test_a_spawned_child_lands_in_the_same_file(tmp_path: Path):
    out = tmp_path / "t.json"
    run_script(tmp_path, SPAWNING_WORKLOAD, out)
    assert sibling_files(out) == []
    by_pid = names_by_pid(out)
    assert len(by_pid) == 2, f"expected two processes, got {sorted(by_pid)}"
    assert any("spawned_work" in n for n in by_pid.values())
    assert any("parent_work" in n for n in by_pid.values())


def test_multiprocessing_workers_land_in_the_same_file(tmp_path: Path):
    """`Pool` as a context manager terminates its workers, and a worker killed
    by a signal never runs the atexit that finishes its trace. Shut the pool
    down gracefully so the workers get to write theirs.
    """
    out = tmp_path / "t.json"
    run_script(
        tmp_path,
        """
import multiprocessing as mp

def worker_body(n):
    return sum(i * i for i in range(n))

if __name__ == "__main__":
    ctx = mp.get_context("spawn")
    pool = ctx.Pool(2)
    pool.map(worker_body, [200, 300])
    pool.close()
    pool.join()
""",
        out,
    )
    assert sibling_files(out) == []
    by_pid = names_by_pid(out)
    assert any("worker_body" in n for n in by_pid.values())
    assert len(by_pid) >= 2


@FORK_ONLY
def test_a_grandchild_reaches_the_file_too(tmp_path: Path):
    out = tmp_path / "t.json"
    run_script(
        tmp_path,
        """
import os, sys

def grandchild_work():
    return sum(range(200))

pid = os.fork()
if pid == 0:
    inner = os.fork()
    if inner == 0:
        grandchild_work()
        sys.exit(0)
    os.waitpid(inner, 0)
    sys.exit(0)
os.waitpid(pid, 0)
""",
        out,
    )
    assert "grandchild_work" in slice_names(load(out))
    assert len(pids(load(out))) >= 3, "a launched process, a child and a grandchild"


@FORK_ONLY
def test_many_forks_never_leave_a_child_hung(tmp_path: Path):
    """The exporter thread holds a lock over the interner while it serialises.
    A fork taken at that instant used to leave the child waiting on it forever.
    """
    out = tmp_path / "t.json"
    stdout = run_script(
        tmp_path,
        """
import os, sys, time

def noise(n):
    return sum(i * i for i in range(n))

def child_work():
    return noise(200)

hung = 0
for _ in range(120):
    noise(3000)
    pid = os.fork()
    if pid == 0:
        child_work()
        os._exit(0)
    deadline = time.time() + 10.0
    while time.time() < deadline:
        done, _ = os.waitpid(pid, os.WNOHANG)
        if done:
            break
        time.sleep(0.002)
    else:
        hung += 1
        os.kill(pid, 9)
        os.waitpid(pid, 0)
print(hung)
""",
        out,
    )
    assert stdout.strip() == "0", f"{stdout.strip()} forked children hung"


@FORK_ONLY
def test_a_forked_child_can_drop_code_objects_without_its_parents_locks(
    tmp_path: Path,
):
    """A child inherits the registry of live interners, and the parent's
    interner lock may have been held by an exporter thread that does not
    exist in the child. Dropping a code object in the child must reach
    for neither."""
    out = tmp_path / "t.json"
    stdout = run_script(
        tmp_path,
        """
import gc, os, sys, time

def noise(n):
    return sum(i * i for i in range(n))

def churn(rounds):
    for i in range(rounds):
        ns = {}
        exec(f"def ephemeral_{i}(): pass", ns)
        ns[f"ephemeral_{i}"]()
        del ns
        gc.collect()

hung = 0
for _ in range(80):
    noise(3000)
    pid = os.fork()
    if pid == 0:
        churn(50)
        os._exit(0)
    deadline = time.time() + 10.0
    while time.time() < deadline:
        done, _ = os.waitpid(pid, os.WNOHANG)
        if done:
            break
        time.sleep(0.002)
    else:
        hung += 1
        os.kill(pid, 9)
        os.waitpid(pid, 0)
print(hung)
""",
        out,
    )
    assert stdout.strip() == "0", f"{stdout.strip()} forked children hung"


@FORK_ONLY
def test_a_forked_child_keeps_the_events_it_records_just_before_exiting(
    tmp_path: Path,
):
    """A forked child ends its run before it exits, and writes what it held.

    `sys.exit` unwinds out of the script and back into the CLI, which stops
    the tracer it started -- and in the child that is the run the fork hooks
    installed. Everything the child recorded since the exporter last drained
    reaches the file there, so the count is exact and its threads are named.
    """
    out = tmp_path / "t.json"
    run_script(
        tmp_path,
        """
import os, sys

CHILDREN = 8
CALLS = 300

def child_tail_marker():
    return 1

for _ in range(CHILDREN):
    pid = os.fork()
    if pid == 0:
        for _ in range(CALLS):
            child_tail_marker()
        sys.exit(0)
    os.waitpid(pid, 0)
""",
        out,
    )
    events = load(out)
    finished = {e["pid"] for e in named_meta(events)}
    assert pids(events) == finished, (
        f"processes that never ended their run: {sorted(pids(events) - finished)}"
    )
    recorded = sum(1 for e in phase(events, "B") if e["name"] == "child_tail_marker")
    assert dropped_events(events) == 0
    assert recorded == 8 * 300, f"expected 2400 calls, recorded {recorded}"


def test_concurrent_children_never_tear_the_file(tmp_path: Path):
    """Several processes committing at once must never splice one's entries
    into the middle of another's, or the file stops parsing."""
    out = tmp_path / "t.json"
    run_script(
        tmp_path,
        """
import multiprocessing as mp

def busy(n):
    total = 0
    for i in range(n):
        total += sum(range(50))
    return total

if __name__ == "__main__":
    ctx = mp.get_context("spawn")
    pool = ctx.Pool(4)
    pool.map(busy, [200] * 8)
    pool.close()
    pool.join()
""",
        out,
    )
    events = load(out)
    assert len(events) > 1000
    assert len(pids(events)) >= 4


def test_a_pool_loses_no_calls_at_all(tmp_path: Path):
    """The count is exact on purpose. Every worker runs its calls on a second
    thread that is still alive when the worker's run ends, which is where
    events used to go missing without even being counted as dropped.
    """
    out = tmp_path / "t.json"
    run_script(
        tmp_path,
        """
import multiprocessing as mp, threading

CALLS = 500

def marker_fn():
    return 1

def on_a_thread():
    for _ in range(CALLS):
        marker_fn()

def worker(_):
    t = threading.Thread(target=on_a_thread, name="worker-side")
    t.start()
    t.join()

if __name__ == "__main__":
    ctx = mp.get_context("spawn")
    pool = ctx.Pool(4)
    pool.map(worker, range(8))
    pool.close()
    pool.join()
""",
        out,
    )
    events = load(out)
    recorded = sum(1 for e in phase(events, "B") if e["name"] == "marker_fn")
    assert dropped_events(events) == 0
    assert recorded == 8 * 500, f"expected 4000 calls, recorded {recorded}"


def test_no_trace_subprocesses_leaves_a_spawned_child_alone(tmp_path: Path):
    out = tmp_path / "t.json"
    run_script(tmp_path, SPAWNING_WORKLOAD, out, flags=("--no-trace-subprocesses",))

    by_pid = names_by_pid(out)
    assert len(by_pid) == 1, f"expected one process, got {sorted(by_pid)}"
    assert "parent_work" in next(iter(by_pid.values()))
    assert "spawned_work" not in slice_names(load(out))


def test_no_trace_subprocesses_advertises_nothing_to_children(tmp_path: Path):
    out = tmp_path / "t.json"
    stdout = run_script(
        tmp_path,
        """
import os
print(os.environ.get("TRACE0_CHILD_OUTPUT"))
""",
        out,
        flags=("--no-trace-subprocesses",),
    )
    assert stdout.strip() == "None"


@FORK_ONLY
def test_no_trace_subprocesses_still_hands_a_forked_child_a_safe_state(
    tmp_path: Path,
):
    """The child inherits an exporter thread that no longer runs. Leaving it
    untraced is the point, but it still has to be handed a run that is over
    rather than one it will block on."""
    out = tmp_path / "t.json"
    run_script(tmp_path, FORKING_WORKLOAD, out, flags=("--no-trace-subprocesses",))

    by_pid = names_by_pid(out)
    assert len(by_pid) == 1, f"expected one process, got {sorted(by_pid)}"
    assert "only_the_child_calls_this" not in slice_names(load(out))


def test_the_python_argument_turns_subprocess_tracing_off(tmp_path: Path):
    path = tmp_path / "off.json"
    with Tracer(str(path), format="json", trace_subprocesses=False):
        subprocess.run(
            [sys.executable, "-c", "def child_work():\n    return 1\nchild_work()\n"],
            check=True,
        )
    events = load(path)
    assert "child_work" not in slice_names(events)
    assert len(pids(events)) == 1


def test_nothing_is_traced_once_the_run_is_over(tmp_path: Path):
    """The environment variable the `.pth` keys on must not outlive the run,
    or every later subprocess would write a stray trace."""
    out = tmp_path / "t.json"
    run_script(tmp_path, "pass\n", out)

    leaked = subprocess.run(
        [sys.executable, "-c", "import os; print(os.environ.get('TRACE0_CHILD_OUTPUT'))"],
        capture_output=True,
        text=True,
    )
    assert leaked.stdout.strip() == "None"
