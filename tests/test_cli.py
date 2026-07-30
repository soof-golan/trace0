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


def trace0(*args: str, cwd: Path | None = None) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, "-m", "trace0", *args],
        capture_output=True,
        text=True,
        cwd=cwd,
    )


@pytest.mark.parametrize(
    "text",
    [
        "Output file path",
        "Output format",
        "Python script to run",
        "Arguments forwarded",
        "library module",
        "[default: protobuf]",
    ],
)
def test_run_help_documents_every_argument(text: str):
    assert text in trace0("run", "--help").stdout


def test_the_default_format_is_protobuf(tmp_path: Path):
    """A run with no `--format` writes a length-delimited stream of
    `Trace.packet` entries, whose first byte is field 1, wire type 2."""
    script = tmp_path / "workload.py"
    script.write_text("def f():\n    return 1\n\nf()\n")
    out = tmp_path / "out.pb"

    result = trace0("run", "--output", str(out), str(script))
    assert result.returncode == 0, result.stderr
    assert out.read_bytes()[0] == 0x0A


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


def names_in(out: Path) -> set[str]:
    trace = json.loads(out.read_text())
    return {e["name"] for e in trace["traceEvents"] if e["ph"] == "B"}


def test_a_module_runs_like_python_dash_m(tmp_path: Path):
    (tmp_path / "workload.py").write_text("def f():\n    return 1\n\nf()\n")
    out = tmp_path / "out.json"

    result = trace0(
        "run", "--output", str(out), "--format", "json", "-m", "workload", cwd=tmp_path
    )
    assert result.returncode == 0, result.stderr
    assert "f" in names_in(out)


def test_a_package_runs_its_dunder_main(tmp_path: Path):
    pkg = tmp_path / "pkg"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "__main__.py").write_text("def g():\n    return 2\n\ng()\n")
    out = tmp_path / "out.json"

    result = trace0(
        "run", "--output", str(out), "--format", "json", "-m", "pkg", cwd=tmp_path
    )
    assert result.returncode == 0, result.stderr
    assert "g" in names_in(out)


def test_a_module_sees_its_own_arguments(tmp_path: Path):
    """`python -m mod a b` gives the module `sys.argv[1:] == ['a', 'b']`, and
    `sys.argv[0]` set to the module's file rather than its name."""
    (tmp_path / "argv.py").write_text(
        "import sys\nprint(sys.argv[1:])\nprint(sys.argv[0].endswith('argv.py'))\n"
    )

    result = trace0(
        "run",
        "--output",
        str(tmp_path / "out.json"),
        "--format",
        "json",
        "-m",
        "argv",
        "a",
        "b",
        cwd=tmp_path,
    )
    assert result.returncode == 0, result.stderr
    assert "['a', 'b']" in result.stdout
    assert "True" in result.stdout


def test_a_module_keeps_arguments_that_look_like_flags(tmp_path: Path):
    """Everything after the module name belongs to the module, including
    arguments that collide with trace0's own flags."""
    (tmp_path / "flags.py").write_text("import sys\nprint(sys.argv[1:])\n")

    result = trace0(
        "run",
        "--output",
        str(tmp_path / "out.json"),
        "--format",
        "json",
        "-m",
        "flags",
        "--output",
        "theirs.txt",
        "-v",
        cwd=tmp_path,
    )
    assert result.returncode == 0, result.stderr
    assert "['--output', 'theirs.txt', '-v']" in result.stdout
    assert not (tmp_path / "theirs.txt").exists()


def test_a_missing_module_fails_without_writing_a_trace(tmp_path: Path):
    out = tmp_path / "out.json"
    result = trace0(
        "run", "--output", str(out), "--format", "json", "-m", "no_such_module",
        cwd=tmp_path,
    )
    assert result.returncode != 0
    assert "no_such_module" in result.stderr


def test_a_run_needs_either_a_script_or_a_module(tmp_path: Path):
    result = trace0("run", "--output", str(tmp_path / "out.json"))
    assert result.returncode != 0


def test_an_unknown_format_is_rejected(tmp_path: Path):
    script = tmp_path / "s.py"
    script.write_text("pass\n")
    result = trace0(
        "run", "--output", str(tmp_path / "o"), "--format", "yaml", str(script)
    )
    assert result.returncode != 0
