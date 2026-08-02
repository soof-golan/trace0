"""A script launched by the CLI is traced on every thread it starts.

`trace0 run` enters the tracer on one thread, and the worker threads the
script goes on to start have never met it. Each has to register itself as it
records its first event, and hand its last batch over at the end.
"""

import subprocess
import sys
from pathlib import Path

from trace_json import iter_events, threads_calling

APPS = Path(__file__).parent / "e2e_apps"
sys.path.insert(0, str(APPS))

from threads_workload import CALLS_PER_WORKER, THREAD_NAME, WORKERS


def test_every_worker_thread_records_all_of_its_own_calls(tmp_path: Path):
    out = tmp_path / "t.json"
    result = subprocess.run(
        [
            *(sys.executable, "-m", "trace0", "run"),
            *("-o", str(out), "-f", "json"),
            str(APPS / "threads_workload.py"),
        ],
        capture_output=True,
        text=True,
        timeout=300,
    )
    assert result.returncode == 0, result.stderr

    per_thread = threads_calling(iter_events(out), "worker_marker")
    assert per_thread == {
        f"{THREAD_NAME}-{i}": CALLS_PER_WORKER for i in range(WORKERS)
    }
