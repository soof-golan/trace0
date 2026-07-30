"""Measure tracing overhead across CPython's own benchmark suite.

A single hand-picked workload only says what tracing costs *that* program.
pyperformance is what CPython itself is judged on, and its benchmarks vary
enormously in how many function calls they make per unit of work -- which
is the only thing a per-call tracer charges for. Reporting the spread is
more honest than reporting one number.

Each benchmark runs unmodified, through pyperf's worker mode so the
workload executes exactly once and pyperf reports its own timing,
excluding interpreter startup.

    uv run --python 3.13t scripts/bench_pyperformance.py
    uv run --python 3.13t scripts/bench_pyperformance.py raytrace richards
"""

import re
import subprocess
import sys
import tempfile
from pathlib import Path

BENCHMARKS = [
    "nbody",
    "float",
    "spectral_norm",
    "fannkuch",
    "chaos",
    "raytrace",
    "richards",
    "deltablue",
    "go",
    "hexiom",
    "scimark",
    "telco",
    "json_dumps",
    "pyflate",
    "logging",
    "unpack_sequence",
]
REPS = 3
TIME_RE = re.compile(r":\s*([\d.]+)\s*(ms|us|sec|ns)\b")
UNITS = {"ns": 1e-9, "us": 1e-6, "ms": 1e-3, "sec": 1.0}


def suite_dir() -> Path:
    import pyperformance

    return Path(pyperformance.__file__).parent / "data-files" / "benchmarks"


def parse_time(output: str) -> float | None:
    match = TIME_RE.search(output)
    if not match:
        return None
    return float(match.group(1)) * UNITS[match.group(2)]


def run(cmd: list[str]) -> float | None:
    proc = subprocess.run(cmd, capture_output=True, text=True)
    if proc.returncode != 0:
        return None
    return parse_time(proc.stdout)


def measure(script: Path, trace: Path | None) -> float | None:
    worker = ["--worker", "-l", "1", "-w", "0", "-n", "1"]
    if trace is None:
        cmd = [sys.executable, str(script), *worker]
    else:
        exe = Path(sys.executable).parent / "trace0"
        cmd = [
            str(exe),
            "run",
            "--output",
            str(trace),
            "--format",
            "protobuf",
            str(script),
            *worker,
        ]
    times = [t for _ in range(REPS) if (t := run(cmd)) is not None]
    return min(times) if times else None


# Benchmarks whose timed region is shorter than this are dominated by
# measurement noise, and their ratio means nothing.
MIN_MEASURABLE = 1e-3


def count_events(trace: Path) -> int:
    """Count length-delimited packets without decoding their contents.

    This counts the whole process, including imports and per-benchmark
    setup. pyperf times only the benchmark function, so these two numbers
    do not share a denominator -- an overhead-per-event figure computed
    from them would be meaningless. Use `bench_producer.py` for that.
    """
    data = trace.read_bytes()
    total, i, size = 0, 0, len(data)
    while i < size:
        i += 1
        shift = length = 0
        while True:
            byte = data[i]
            i += 1
            length |= (byte & 0x7F) << shift
            if not byte & 0x80:
                break
            shift += 7
        i += length
        total += 1
    return total


def selected() -> list[str]:
    """Named benchmarks, or all of them. Naming a few keeps an A/B of one
    change down to the benchmarks that change is supposed to affect."""
    if not sys.argv[1:]:
        return BENCHMARKS
    unknown = set(sys.argv[1:]) - set(BENCHMARKS)
    if unknown:
        raise SystemExit(f"unknown benchmarks: {sorted(unknown)}")
    return [b for b in BENCHMARKS if b in set(sys.argv[1:])]


def main() -> int:
    root = suite_dir()
    print(
        f"{'benchmark':<18}{'vanilla':>10}{'traced':>10}{'ratio':>8}"
        f"{'events (whole run)':>20}"
    )
    rows = []
    with tempfile.TemporaryDirectory() as d:
        for name in selected():
            script = root / f"bm_{name}" / "run_benchmark.py"
            if not script.exists():
                continue
            trace = Path(d) / f"{name}.pb"
            base = measure(script, None)
            traced = measure(script, trace)
            if base is None or traced is None:
                print(f"{name:<18}{'skipped (did not run)':>40}")
                continue
            if base < MIN_MEASURABLE:
                print(f"{name:<18}{base * 1000:>9.2f}ms  too short to measure")
                continue
            events = count_events(trace) if trace.exists() else 0
            rows.append((name, base, traced, traced / base, events))
            print(
                f"{name:<18}{base * 1000:>9.1f}ms{traced * 1000:>9.1f}ms"
                f"{traced / base:>7.2f}x{events:>20,}"
            )

    if rows:
        ratios = sorted(r[3] for r in rows)
        mid = ratios[len(ratios) // 2]
        slowest = max(rows, key=lambda r: r[3])
        fastest = min(rows, key=lambda r: r[3])
        print(f"\nmedian slowdown: {mid:.2f}x over {len(rows)} benchmarks")
        print(f"  least affected: {fastest[0]} at {fastest[3]:.2f}x")
        print(f"  most affected:  {slowest[0]} at {slowest[3]:.2f}x")
        print("\nThe spread is call density: a tracer charges per call, so")
        print("arithmetic in tight loops barely notices and call-heavy code pays.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
