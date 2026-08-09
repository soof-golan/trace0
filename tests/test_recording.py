"""Flight-recorder mode keeps the recent past and dumps slices of it.

With `record_last_mb` set, the tracer writes nothing while it runs. The
exporter thread holds the last N megabytes of raw events in a ring. A
snapshot block marks a window; when the block exits, the window becomes
a file in the output directory. The run itself always ends with a dump,
so a crash or a plain exit still leaves the recent past on disk.
"""

import gc
import subprocess
import sys
import time
from pathlib import Path

import pytest
from trace_json import load, slice_names

from trace0 import Tracer


def before_marker():
    pass


def inside_marker():
    pass


def after_marker():
    pass


def outer_marker():
    pass


def inner_marker():
    pass


def dumps_named(out: Path, reason: str) -> list[Path]:
    return sorted(p for p in out.iterdir() if f"-{reason}." in p.name)


def recorder(out: Path) -> Tracer:
    return Tracer(str(out), format="json", record_last_mb=64)


def test_a_snapshot_block_captures_exactly_its_own_markers(tmp_path: Path):
    out = tmp_path / "dumps"
    with recorder(out) as t:
        before_marker()
        with t.snapshot("checkout"):
            inside_marker()
        after_marker()
    files = dumps_named(out, "checkout")
    assert len(files) == 1
    names = slice_names(load(files[0]))
    assert "inside_marker" in names
    assert "before_marker" not in names
    assert "after_marker" not in names


def test_a_raising_block_dumps_with_the_exception_tag(tmp_path: Path):
    out = tmp_path / "dumps"
    with recorder(out) as t:
        with pytest.raises(ValueError):
            with t.snapshot("risky"):
                inside_marker()
                raise ValueError("boom")
    files = dumps_named(out, "risky-ValueError")
    assert len(files) == 1
    assert "inside_marker" in slice_names(load(files[0]))


def test_nested_blocks_window_independently(tmp_path: Path):
    out = tmp_path / "dumps"
    with recorder(out) as t:
        with t.snapshot("outer"):
            outer_marker()
            with t.snapshot("inner"):
                inner_marker()
    inner = slice_names(load(dumps_named(out, "inner")[0]))
    outer = slice_names(load(dumps_named(out, "outer")[0]))
    assert "inner_marker" in inner
    assert "outer_marker" not in inner
    assert {"inner_marker", "outer_marker"} <= outer


def test_a_function_open_before_the_slice_shows_as_an_open_span(tmp_path: Path):
    def enclosing(t: Tracer):
        with t.snapshot("deep"):
            inside_marker()

    out = tmp_path / "dumps"
    with recorder(out) as t:
        enclosing(t)
    names = slice_names(load(dumps_named(out, "deep")[0]))
    assert any(name.endswith(".enclosing") for name in names)
    assert "inside_marker" in names


def test_the_run_always_ends_with_an_exit_dump(tmp_path: Path):
    out = tmp_path / "dumps"
    with recorder(out):
        before_marker()
    files = dumps_named(out, "exit")
    assert len(files) == 1
    assert "before_marker" in slice_names(load(files[0]))


def test_an_escaping_exception_tags_the_final_dump(tmp_path: Path):
    out = tmp_path / "dumps"
    with pytest.raises(KeyError):
        with recorder(out):
            before_marker()
            raise KeyError("boom")
    files = dumps_named(out, "exception-KeyError")
    assert len(files) == 1
    assert "before_marker" in slice_names(load(files[0]))
    assert dumps_named(out, "exit") == []


def test_dump_writes_the_whole_ring_on_demand(tmp_path: Path):
    out = tmp_path / "dumps"
    with recorder(out) as t:
        before_marker()
        t.dump("manual")
        after_marker()
    names = slice_names(load(dumps_named(out, "manual")[0]))
    assert "before_marker" in names
    assert "after_marker" not in names


def test_a_past_slice_dumps_by_epoch_bounds(tmp_path: Path):
    out = tmp_path / "dumps"
    with recorder(out) as t:
        before_marker()
        time.sleep(0.002)
        start = time.time_ns()
        time.sleep(0.002)
        inside_marker()
        time.sleep(0.002)
        end = time.time_ns()
        time.sleep(0.002)
        after_marker()
        t.snapshot("span", start=start, end=end)
    names = slice_names(load(dumps_named(out, "span")[0]))
    assert "inside_marker" in names
    assert "before_marker" not in names
    assert "after_marker" not in names


def test_a_snapshot_block_reports_its_dump_path(tmp_path: Path):
    out = tmp_path / "dumps"
    with recorder(out) as t:
        with t.snapshot("checkout") as snap:
            assert snap.path is None
            inside_marker()
    assert snap.path is not None
    assert Path(snap.path) == dumps_named(out, "checkout")[0]


def test_a_past_slice_returns_its_dump_path(tmp_path: Path):
    out = tmp_path / "dumps"
    with recorder(out) as t:
        start = time.time_ns()
        before_marker()
        path = t.snapshot("span", start=start, end=time.time_ns())
    assert Path(path) == dumps_named(out, "span")[0]


def test_dump_returns_its_path(tmp_path: Path):
    out = tmp_path / "dumps"
    with recorder(out) as t:
        before_marker()
        path = t.dump("manual")
    assert Path(path) == dumps_named(out, "manual")[0]


def test_a_streaming_tracer_refuses_snapshots(tmp_path: Path):
    with Tracer(str(tmp_path / "t.json"), format="json") as t:
        with pytest.raises(RuntimeError):
            t.snapshot("nope")
        with pytest.raises(RuntimeError):
            t.dump("nope")


def test_a_recorder_that_is_not_running_refuses_snapshots(tmp_path: Path):
    t = Tracer(str(tmp_path / "dumps"), format="json", record_last_mb=64)
    with pytest.raises(RuntimeError):
        t.snapshot("early")
    with pytest.raises(RuntimeError):
        t.dump("early")


def test_half_a_nanosecond_range_is_rejected(tmp_path: Path):
    out = tmp_path / "dumps"
    with recorder(out) as t:
        with pytest.raises(ValueError):
            t.snapshot("x", start=5)


def test_the_cli_records_when_asked(tmp_path: Path):
    script = tmp_path / "s.py"
    script.write_text("def cli_marker():\n    pass\n\ncli_marker()\n")
    out = tmp_path / "dumps"
    subprocess.run(
        [
            sys.executable,
            "-m",
            "trace0",
            "run",
            "--record-last-mb",
            "64",
            "--format",
            "json",
            "-o",
            str(out),
            str(script),
        ],
        check=True,
    )
    files = dumps_named(out, "exit")
    assert len(files) == 1
    assert "cli_marker" in slice_names(load(files[0]))


def fresh_marker():
    pass


def filler_step():
    pass


def churn(count: int):
    for i in range(count):
        space = {}
        exec(f"def churned_{i}():\n    pass", space)
        space[f"churned_{i}"]()


def test_code_churn_beyond_the_id_space_keeps_tracing(tmp_path: Path, monkeypatch):
    monkeypatch.setenv("TRACE0_CODE_CAPACITY", "256")
    out = tmp_path / "dumps"
    with Tracer(str(out), format="json", record_last_mb=1) as t:
        filler_step()
        churn(300)
        gc.collect()
        for _ in range(200_000):
            filler_step()
        t.dump("advance")
        with t.snapshot("fresh"):
            fresh_marker()
    names = slice_names(load(dumps_named(out, "fresh")[0]))
    assert "fresh_marker" in names


def test_a_spawned_child_dumps_its_own_file(tmp_path: Path):
    out = tmp_path / "dumps"
    child = tmp_path / "child.py"
    child.write_text("def child_marker():\n    pass\n\nchild_marker()\n")
    with recorder(out):
        subprocess.run([sys.executable, str(child)], check=True)
    exits = dumps_named(out, "exit")
    assert len(exits) == 2
    names = [slice_names(load(p)) for p in exits]
    assert any("child_marker" in n for n in names)
