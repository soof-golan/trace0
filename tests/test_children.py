"""A forked child inherits an exporter thread that did not survive the fork.

Everything here is about that: the child must not touch the run it inherited,
must get a trace of its own beside the parent's, and must never block on a lock
the vanished thread was holding.
"""

import json
import os
import subprocess
import sys
from pathlib import Path

import pytest

FORK_ONLY = pytest.mark.skipif(
    not hasattr(os, "fork"), reason="fork is not available on this platform"
)


def run_script(tmp_path: Path, body: str, out: Path, fmt: str = "json") -> str:
    script = tmp_path / "workload.py"
    script.write_text(body)
    result = subprocess.run(
        [sys.executable, "-m", "trace0", "run", "-o", str(out), "-f", fmt, str(script)],
        capture_output=True,
        text=True,
        timeout=120,
    )
    assert result.returncode == 0, result.stderr
    return result.stdout


def names(path: Path) -> set[str]:
    trace = json.loads(path.read_text())
    return {e["name"] for e in trace["traceEvents"] if e["ph"] == "B"}


def child_traces(out: Path) -> list[Path]:
    return sorted(out.parent.glob(f"{out.name}.*"))


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
def test_a_forked_child_writes_its_own_trace(tmp_path: Path):
    out = tmp_path / "t.json"
    run_script(tmp_path, FORKING_WORKLOAD, out)

    children = child_traces(out)
    assert len(children) == 1, f"expected one child trace, got {children}"
    assert "only_the_child_calls_this" in names(children[0])


@FORK_ONLY
def test_the_parent_trace_holds_only_the_parents_work(tmp_path: Path):
    out = tmp_path / "t.json"
    run_script(tmp_path, FORKING_WORKLOAD, out)

    parent = names(out)
    assert "only_the_parent_calls_this" in parent
    assert "only_the_child_calls_this" not in parent


@FORK_ONLY
def test_a_child_trace_is_named_for_its_pid(tmp_path: Path):
    out = tmp_path / "t.json"
    stdout = run_script(
        tmp_path,
        """
import os, sys
pid = os.fork()
if pid == 0:
    sys.exit(0)
print(pid)
os.waitpid(pid, 0)
""",
        out,
    )
    child_pid = stdout.strip()
    assert (tmp_path / f"t.json.{child_pid}").exists()


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


def test_a_spawned_child_writes_its_own_trace(tmp_path: Path):
    """A process reached by exec shares no memory with its parent, so it is
    picked up by the `.pth` at interpreter startup rather than a fork hook."""
    out = tmp_path / "t.json"
    run_script(
        tmp_path,
        """
import subprocess, sys
subprocess.run(
    [sys.executable, "-c", "def spawned_work():\\n    return sum(range(300))\\nspawned_work()\\n"],
    check=True,
)
def parent_work():
    return sum(range(100))
parent_work()
""",
        out,
    )
    children = child_traces(out)
    assert len(children) == 1, f"expected one child trace, got {children}"
    assert "spawned_work" in names(children[0])
    assert "parent_work" in names(out)


def test_multiprocessing_workers_are_traced(tmp_path: Path):
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
    children = child_traces(out)
    assert children, "no worker was traced"
    assert any("worker_body" in names(t) for t in children)


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
    assert not child_traces(out)


@FORK_ONLY
def test_a_child_that_forks_again_still_traces(tmp_path: Path):
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
    traces = child_traces(out)
    assert len(traces) == 2, f"a child and a grandchild, got {traces}"
    assert any("grandchild_work" in names(t) for t in traces)
