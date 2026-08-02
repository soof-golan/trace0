"""Every worker process a real server starts is traced into one file.

The three servers reach their workers differently. `gunicorn` forks its
arbiter, so the workers inherit a tracer mid-run and the fork hooks have to
give each one a run of its own. `uvicorn` and `hypercorn` spawn fresh
interpreters, which reach tracing through the `.pth` at interpreter startup
instead. Both halves of the child-process support are needed for the same
assertion to hold.

Needs the `e2e` dependency group:

    uv run --group e2e pytest -m e2e
"""

import json
import socket
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from urllib.error import URLError
from urllib.request import urlopen

import pytest
from trace_json import iter_events, pids_calling

for module in ("uvicorn", "gunicorn", "hypercorn", "fastapi", "django", "uvloop"):
    pytest.importorskip(module)

pytestmark = pytest.mark.e2e

APPS = Path(__file__).parent / "e2e_apps"
HOST = "127.0.0.1"
WORKERS = 4
STARTUP_TIMEOUT = 300.0
SPREAD_TIMEOUT = 120.0
SETTLE = 2.0


def uvicorn_argv(port: int, target: str, loop: str) -> list[str]:
    return [
        *("uvicorn", "--host", HOST, "--port", str(port)),
        *("--workers", str(WORKERS), "--loop", loop),
        target,
    ]


def hypercorn_argv(port: int, target: str, loop: str) -> list[str]:
    return [
        *("hypercorn", "--bind", f"{HOST}:{port}"),
        *("--workers", str(WORKERS), "--worker-class", loop),
        target,
    ]


def gunicorn_argv(port: int, target: str, loop: str) -> list[str]:
    worker = {"asyncio": "AsyncioWorker", "uvloop": "UvloopWorker"}[loop]
    return [
        *("gunicorn", "--bind", f"{HOST}:{port}"),
        *("--workers", str(WORKERS), "--worker-class", f"e2e_workers.{worker}"),
        *("--timeout", "300", "--graceful-timeout", "60"),
        target,
    ]


SERVERS = {
    "gunicorn": gunicorn_argv,
    "hypercorn": hypercorn_argv,
    "uvicorn": uvicorn_argv,
}

FRAMEWORKS = {
    "django": ("django_app:asgi_app", "django_endpoint_marker"),
    "fastapi": ("fastapi_app:app", "fastapi_endpoint_marker"),
}

LOOPS = ("asyncio", "uvloop")


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind((HOST, 0))
        return sock.getsockname()[1]


def answering_pid(port: int, timeout: float) -> int:
    with urlopen(f"http://{HOST}:{port}/work", timeout=timeout) as response:
        return json.load(response)["pid"]


def wait_until_serving(server: subprocess.Popen, port: int) -> None:
    deadline = time.monotonic() + STARTUP_TIMEOUT
    while time.monotonic() < deadline:
        if server.poll() is not None:
            raise AssertionError(f"server exited with {server.returncode}")
        try:
            answering_pid(port, 10.0)
            return
        except (URLError, OSError):
            time.sleep(0.2)
    raise AssertionError(f"no reply within {STARTUP_TIMEOUT}s of launching")


def pids_that_answered(port: int) -> set[int]:
    """Keep asking until every worker has served a request.

    Which worker accepts a connection is the kernel's choice, so watching the
    replies is the only way to learn which processes ran the endpoint.
    """
    seen: set[int] = set()
    deadline = time.monotonic() + SPREAD_TIMEOUT
    with ThreadPoolExecutor(WORKERS * 2) as pool:
        while len(seen) < WORKERS and time.monotonic() < deadline:
            batch = [pool.submit(answering_pid, port, 60.0) for _ in range(WORKERS * 3)]
            seen.update(future.result() for future in batch)
    return seen


def serve(out: Path, argv: list[str], port: int) -> set[int]:
    """Run the server until every worker has answered, then shut it down.

    The settle before shutting down is what makes the trace complete rather
    than nearly complete. A worker only writes its last partial batch when it
    ends its run, and `gunicorn` reaches its workers through
    `uvicorn.workers`, which restores the default SIGTERM disposition once its
    own shutdown finishes -- so the arbiter's next SIGTERM kills the worker
    outright, with no chance to flush. Waiting lets the exporter drain the
    requests we assert on well before that.
    """
    server = subprocess.Popen(
        [
            *(sys.executable, "-m", "trace0", "run"),
            *("-o", str(out), "-f", "json", "-m"),
            *argv,
        ],
        cwd=APPS,
    )
    try:
        wait_until_serving(server, port)
        answered = pids_that_answered(port)
        time.sleep(SETTLE)
        return answered
    finally:
        server.terminate()
        try:
            server.wait(timeout=STARTUP_TIMEOUT)
        except subprocess.TimeoutExpired:
            server.kill()
            raise


@pytest.mark.parametrize("loop", LOOPS)
@pytest.mark.parametrize("framework", FRAMEWORKS)
@pytest.mark.parametrize("server", SERVERS)
def test_every_worker_that_served_a_request_is_in_the_trace(
    tmp_path: Path, server: str, framework: str, loop: str
):
    target, marker = FRAMEWORKS[framework]
    out = tmp_path / "t.json"
    port = free_port()

    answered = serve(out, SERVERS[server](port, target, loop), port)
    assert len(answered) == WORKERS, f"{len(answered)} of {WORKERS} workers replied"

    assert sorted(out.parent.glob(f"{out.name}.*")) == [], "a worker wrote its own file"
    traced = pids_calling(iter_events(out), marker)
    assert answered <= traced, f"no trace from workers {sorted(answered - traced)}"
