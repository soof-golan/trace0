"""A run ended by a signal still writes what it recorded.

SIGTERM's default action ends the process between one bytecode and the next.
Nothing unwinds, no `atexit` runs, and everything the exporter had not yet
drained goes with it -- which is how most deployments stop: `docker stop`, a
Kubernetes eviction, `systemctl stop`. So the tracer takes over the signals
whose default action is death, ends its run, and then lets the signal do what
it was always going to do.

It takes over nothing else. A signal the program handles itself, or has asked
to ignore, was never going to lose the trace, so the tracer stays out of the
way and the program keeps control.
"""

import os
import signal
import subprocess
import sys
from pathlib import Path

import pytest
from trace_json import load, named_meta, phase

from trace0 import Tracer

POSIX_ONLY = pytest.mark.skipif(
    os.name != "posix", reason="posix signal dispositions"
)

BUSY_UNTIL_SIGNALLED = """
import time

def signalled_marker():
    return 1

print("ready", flush=True)
deadline = time.monotonic() + 120
while time.monotonic() < deadline:
    signalled_marker()
"""

INTERRUPTIBLE = """
import time

print("ready", flush=True)
try:
    time.sleep(120)
except KeyboardInterrupt:
    print("interrupted", flush=True)
"""

HANDLES_SIGTERM_ITSELF = """
import signal
import sys
import time

def after_the_signal():
    return 1

def on_term(signum, frame):
    for _ in range(50):
        after_the_signal()
    print("handled", flush=True)
    sys.exit(0)

signal.signal(signal.SIGTERM, on_term)
print("ready", flush=True)
time.sleep(120)
"""


def launch(tmp_path: Path, body: str, out: Path) -> subprocess.Popen:
    script = tmp_path / "workload.py"
    script.write_text(body)
    server = subprocess.Popen(
        [
            *(sys.executable, "-m", "trace0", "run"),
            *("-o", str(out), "-f", "json"),
            str(script),
        ],
        stdout=subprocess.PIPE,
        text=True,
    )
    assert server.stdout.readline().strip() == "ready", "workload never started"
    return server


def recorded(out: Path, name: str) -> bool:
    return any(e["name"] == name for e in phase(load(out), "B"))


@POSIX_ONLY
def test_a_run_ended_by_sigterm_still_finishes_its_trace(tmp_path: Path):
    out = tmp_path / "t.json"
    proc = launch(tmp_path, BUSY_UNTIL_SIGNALLED, out)
    proc.send_signal(signal.SIGTERM)

    assert proc.wait(timeout=120) == -signal.SIGTERM, "the exit status changed"
    assert named_meta(load(out)), "no thread metadata: the run never finished"
    assert recorded(out, "signalled_marker")


@POSIX_ONLY
def test_a_run_ended_by_sighup_still_finishes_its_trace(tmp_path: Path):
    out = tmp_path / "t.json"
    proc = launch(tmp_path, BUSY_UNTIL_SIGNALLED, out)
    proc.send_signal(signal.SIGHUP)

    assert proc.wait(timeout=120) == -signal.SIGHUP
    assert named_meta(load(out)), "no thread metadata: the run never finished"


@POSIX_ONLY
def test_a_program_that_handles_sigterm_itself_keeps_control(tmp_path: Path):
    """The tracer replaces the disposition it found at `__enter__`, and this
    program installs its own afterwards. Ending the run from underneath it
    would stop tracing everything the program does on the way out."""
    out = tmp_path / "t.json"
    proc = launch(tmp_path, HANDLES_SIGTERM_ITSELF, out)
    proc.send_signal(signal.SIGTERM)

    assert proc.wait(timeout=120) == 0
    assert "handled" in proc.stdout.read()
    assert recorded(out, "after_the_signal")


@POSIX_ONLY
def test_sigint_still_reaches_the_program_as_keyboard_interrupt(tmp_path: Path):
    """Python already turns SIGINT into an exception that unwinds, which ends
    the run on its own. Taking it over would break every program that stops on
    Ctrl-C."""
    out = tmp_path / "t.json"
    proc = launch(tmp_path, INTERRUPTIBLE, out)
    proc.send_signal(signal.SIGINT)

    assert proc.wait(timeout=120) == 0
    assert "interrupted" in proc.stdout.read()


def after_the_signal() -> int:
    return 1


@POSIX_ONLY
def test_a_handler_the_program_installed_first_is_chained_to(tmp_path: Path):
    path = tmp_path / "chained.json"
    seen: list[int] = []
    previous = signal.signal(signal.SIGHUP, lambda signum, frame: seen.append(signum))
    try:
        with Tracer(str(path), format="json"):
            os.kill(os.getpid(), signal.SIGHUP)
            after_the_signal()
    finally:
        signal.signal(signal.SIGHUP, previous)

    assert seen == [signal.SIGHUP], "the program's own handler never ran"
    assert recorded(path, "after_the_signal"), "the run ended at the signal"


@POSIX_ONLY
def test_the_run_leaves_the_dispositions_as_it_found_them(tmp_path: Path):
    def handler(signum, frame):
        return None

    previous = signal.signal(signal.SIGHUP, handler)
    try:
        with Tracer(str(tmp_path / "restored.json"), format="json"):
            assert signal.getsignal(signal.SIGHUP) is not handler
        assert signal.getsignal(signal.SIGHUP) is handler
    finally:
        signal.signal(signal.SIGHUP, previous)
