"""`trace0 run --help` is assembled by clap from doc comments on the
argument structs in `crates/py/src/cli.rs`. They are the one place in the
codebase where a doc comment is load-bearing at runtime, so deleting one
is a silent regression in the CLI rather than a documentation change.
"""

import json
import subprocess
import sys
from pathlib import Path

import pytest


def trace0(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, "-m", "trace0", *args],
        capture_output=True,
        text=True,
    )


@pytest.mark.parametrize(
    "text",
    ["Output file path", "Output format", "Python script to run", "Arguments forwarded"],
)
def test_run_help_documents_every_argument(text: str):
    assert text in trace0("run", "--help").stdout


def test_run_traces_a_script_to_the_requested_path(tmp_path: Path):
    script = tmp_path / "workload.py"
    script.write_text("def f():\n    return 1\n\nfor _ in range(50):\n    f()\n")
    out = tmp_path / "out.json"

    result = trace0("run", "--output", str(out), "--format", "json", str(script))
    assert result.returncode == 0, result.stderr

    trace = json.loads(out.read_text())
    assert "f" in {e["name"] for e in trace["traceEvents"] if e["ph"] == "B"}


def test_the_script_sees_its_own_arguments(tmp_path: Path):
    script = tmp_path / "argv.py"
    script.write_text("import sys\nprint(sys.argv[1:])\n")
    out = tmp_path / "out.json"

    result = trace0(
        "run", "--output", str(out), "--format", "json", str(script), "a", "b"
    )
    assert result.returncode == 0, result.stderr
    assert "['a', 'b']" in result.stdout


def test_an_unknown_format_is_rejected(tmp_path: Path):
    script = tmp_path / "s.py"
    script.write_text("pass\n")
    result = trace0(
        "run", "--output", str(tmp_path / "o"), "--format", "yaml", str(script)
    )
    assert result.returncode != 0
