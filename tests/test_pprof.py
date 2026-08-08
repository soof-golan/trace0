"""The pprof format folds the trace into self-time samples.

One uncompressed `perftools.profiles.Profile` message per process: the
launched process writes the requested path, and every traced child writes a
sibling file named `<path>.<pid>`, because a pprof profile has no way to
interleave writers the way the JSON and Perfetto streams do. `go tool pprof`
accepts several profiles at once, so the siblings stay usable together.
"""

import subprocess
import sys
from pathlib import Path
from typing import get_args

from pprof_profile import function_names, load, sample_types, stack_names, total_ns

from trace0 import TraceFormat, Tracer


def work() -> int:
    return sum(range(200))


def trace_to(path: Path) -> None:
    with Tracer(str(path), format="pprof"):
        for _ in range(50):
            work()


def test_a_traced_function_shows_up_by_qualname(tmp_path: Path):
    out = tmp_path / "out.pprof"
    trace_to(out)
    assert "work" in function_names(load(out))


def test_samples_measure_wall_nanoseconds(tmp_path: Path):
    out = tmp_path / "out.pprof"
    trace_to(out)
    profile = load(out)
    assert sample_types(profile) == [("wall", "nanoseconds")]
    assert profile["strings"][0] == ""
    assert total_ns(profile) > 0


def test_every_sample_resolves_to_a_stack_of_names(tmp_path: Path):
    out = tmp_path / "out.pprof"
    trace_to(out)
    profile = load(out)
    assert profile["samples"]
    for sample in profile["samples"]:
        names = stack_names(profile, sample)
        assert len(names) == len(sample["location_id"])
        assert all(names)


def test_the_type_hint_admits_pprof():
    assert "pprof" in get_args(TraceFormat)


def test_the_cli_accepts_pprof(tmp_path: Path):
    script = tmp_path / "workload.py"
    script.write_text("def f():\n    return 1\n\nfor _ in range(50):\n    f()\n")
    out = tmp_path / "out.pprof"

    result = subprocess.run(
        [sys.executable, "-m", "trace0", "run", "-o", str(out), "-f", "pprof", str(script)],
        capture_output=True,
        text=True,
        timeout=180,
    )
    assert result.returncode == 0, result.stderr
    assert "f" in function_names(load(out))


def test_a_spawned_child_writes_its_own_profile(tmp_path: Path):
    script = tmp_path / "workload.py"
    script.write_text(
        "import subprocess, sys\n"
        "subprocess.run(\n"
        "    [sys.executable, '-c',\n"
        "     'def spawned_work():\\n    return sum(range(300))\\n'\n"
        "     'for _ in range(50):\\n    spawned_work()\\n'],\n"
        "    check=True,\n"
        ")\n"
    )
    out = tmp_path / "out.pprof"

    result = subprocess.run(
        [sys.executable, "-m", "trace0", "run", "-o", str(out), "-f", "pprof", str(script)],
        capture_output=True,
        text=True,
        timeout=180,
    )
    assert result.returncode == 0, result.stderr

    siblings = sorted(out.parent.glob(f"{out.name}.*"))
    assert len(siblings) == 1, "the spawned child writes exactly one profile of its own"
    assert "spawned_work" in function_names(load(siblings[0]))
