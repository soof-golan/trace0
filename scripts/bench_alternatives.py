"""Compare trace0 against Python's own profilers on one call-dense workload.

`hyperfine` drives the runs because the alternatives are whole programs, not
callables: `cProfile` and `profile` are `-m` modules and trace0 is a CLI, so
process wall time is the only measure that treats all four the same. It also
handles warmup and repetition, which a hand-rolled loop gets wrong.

Ratios come from each command's *minimum*, not its mean. A busy machine can
only ever make a run slower, so the fastest sample is the least contaminated
one available. The reported spread is max-min: when it approaches the gap
between two commands, the machine was too busy and the run should be repeated.

    uv run --with sqlglot scripts/bench_alternatives.py
"""

import json
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

WORKLOAD = Path(__file__).parent / "workload_sqlglot.py"
RUNS = 10
# `profile` is pure Python and ~20x slower than everything else here. Its
# overhead dwarfs the noise fewer runs trade away.
SLOW_RUNS = 3
SLOW_SECONDS = 5.0


def commands(out: Path) -> list[tuple[str, list[str]]]:
    exe = Path(sys.executable)
    trace0 = [str(exe.parent / "trace0"), "run"]
    workload = str(WORKLOAD)
    return [
        ("vanilla", [str(exe), workload]),
        ("trace0 (protobuf)", [*trace0, "-o", str(out / "t.pb"), "-f", "protobuf", workload]),
        ("trace0 (json)", [*trace0, "-o", str(out / "t.json"), "-f", "json", workload]),
        ("cProfile", [str(exe), "-m", "cProfile", "-o", str(out / "c.prof"), workload]),
        ("profile", [str(exe), "-m", "profile", "-o", str(out / "p.prof"), workload]),
    ]


def probe(name: str, argv: list[str]) -> float:
    """Run the command once to prove it works, and time it to pick a run count."""
    start = time.perf_counter()
    done = subprocess.run(argv, capture_output=True)
    if done.returncode != 0:
        raise SystemExit(f"{name} failed:\n{done.stderr.decode()}")
    return time.perf_counter() - start


def measure(name: str, argv: list[str], export: Path) -> tuple[float, float]:
    runs = SLOW_RUNS if probe(name, argv) > SLOW_SECONDS else RUNS
    subprocess.run(
        ["hyperfine", "--warmup", "1", "--runs", str(runs),
         "--command-name", name, "--export-json", str(export), shlex.join(argv)],
        check=True,
        stdout=subprocess.DEVNULL,
    )
    times = json.loads(export.read_text())["results"][0]["times"]
    return min(times), max(times) - min(times)


def main() -> int:
    if shutil.which("hyperfine") is None:
        raise SystemExit("hyperfine not found: brew install hyperfine")

    with tempfile.TemporaryDirectory() as d:
        out = Path(d)
        rows = [(name, *measure(name, argv, out / "hf.json")) for name, argv in commands(out)]

        base = rows[0][1]
        print(f"\n{'':<20}{'wall time':>11}{'spread':>9}{'vs vanilla':>12}")
        for name, best, spread in rows:
            ratio = "—" if best == base else f"{best / base:.2f}x"
            print(f"{name:<20}{best:>10.3f}s{spread:>8.3f}s{ratio:>12}")

        for label, path in (("protobuf", out / "t.pb"), ("json", out / "t.json")):
            print(f"\n{label} trace: {path.stat().st_size / 1e6:,.0f} MB")
    return 0


if __name__ == "__main__":
    sys.exit(main())
