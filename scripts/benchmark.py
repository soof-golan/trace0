"""Every number the README claims, and the commands that produce them.

    uv run scripts/benchmark.py overhead
    uv run --with pyperformance scripts/benchmark.py suite
    uv run --with sqlglot scripts/benchmark.py alternatives

Each arm reports a minimum rather than a mean. A busy machine can only ever
make a run slower, so the fastest sample is the least contaminated one
available, and comparing minima keeps a background process from landing
entirely on one side of an A/B. Where two arms are compared they are
interleaved in one session for the same reason.
"""

import argparse
import json
import shlex
import shutil
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

HERE = Path(__file__).parent


DEPTH = 27
OVERHEAD_REPS = 9


def fib_calls(depth: int) -> int:
    """How many calls `fib(depth)` makes: 2*fib(depth+1)-1."""
    a, b = 0, 1
    for _ in range(depth + 1):
        a, b = b, a + b
    return 2 * a - 1


EVENTS_PER_THREAD = 2 * fib_calls(DEPTH)


def fib(n: int) -> int:
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)


def time_threads(n_threads: int) -> float:
    threads = [
        threading.Thread(target=fib, args=(DEPTH,), name=f"w{i}") for i in range(n_threads)
    ]
    start = time.perf_counter()
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    return time.perf_counter() - start


def measure_overhead(n_threads: int) -> tuple[float, float]:
    """Only the traced threads are timed. The exporter drain is deliberately
    excluded: the question is what tracing costs the program being traced,
    not what it costs to write the trace out."""
    from trace0 import Tracer

    base, traced = [], []
    for _ in range(OVERHEAD_REPS):
        base.append(time_threads(n_threads))
        with Tracer("/dev/null", format="protobuf"):
            traced.append(time_threads(n_threads))
    return min(base), min(traced)


def cmd_overhead(args: argparse.Namespace) -> int:
    counts = args.threads or [1, 2, 4, 8]
    print(f"{'thr':>4} {'untraced':>10} {'traced':>10} {'events':>12} {'overhead':>12}")
    results = []
    for n in counts:
        base, traced = measure_overhead(n)
        events = EVENTS_PER_THREAD * n
        per_event = (traced - base) / events * 1e9
        results.append(per_event)
        print(
            f"{n:>4} {base:>9.4f}s {traced:>9.4f}s {events:>12,} {per_event:>9.2f} ns/ev"
        )
    print(f"\nsingle-thread overhead: {results[0]:.2f} ns/ev")
    if len(results) > 1:
        print(f"median across counts:   {statistics.median(results):.2f} ns/ev")
    return 0


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
SUITE_REPS = 3
MIN_MEASURABLE = 1e-3


def suite_dir() -> Path:
    import pyperformance

    return Path(pyperformance.__file__).parent / "data-files" / "benchmarks"


def parse_time(output: str) -> float | None:
    import re

    match = re.search(r":\s*([\d.]+)\s*(ms|us|sec|ns)\b", output)
    if not match:
        return None
    units = {"ns": 1e-9, "us": 1e-6, "ms": 1e-3, "sec": 1.0}
    return float(match.group(1)) * units[match.group(2)]


def time_benchmark(script: Path, trace: Path | None) -> float | None:
    worker = ["--worker", "-l", "1", "-w", "0", "-n", "1"]
    if trace is None:
        cmd = [sys.executable, str(script), *worker]
    else:
        exe = Path(sys.executable).parent / "trace0"
        cmd = [str(exe), "run", "-o", str(trace), "-f", "protobuf", str(script), *worker]

    times = []
    for _ in range(SUITE_REPS):
        done = subprocess.run(cmd, capture_output=True, text=True)
        if done.returncode == 0 and (t := parse_time(done.stdout)) is not None:
            times.append(t)
    return min(times) if times else None


def count_packets(trace: Path) -> int:
    """Count length-delimited packets without decoding them.

    This covers the whole process, imports and setup included, while pyperf
    times only the benchmark function. The two do not share a denominator, so
    an overhead-per-event figure built from them would be meaningless -- use
    `overhead` for that.
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


def cmd_suite(args: argparse.Namespace) -> int:
    unknown = set(args.names) - set(BENCHMARKS)
    if unknown:
        raise SystemExit(f"unknown benchmarks: {sorted(unknown)}")
    chosen = [b for b in BENCHMARKS if b in set(args.names)] if args.names else BENCHMARKS

    root = suite_dir()
    print(f"{'benchmark':<18}{'vanilla':>10}{'traced':>10}{'ratio':>8}{'packets':>20}")
    rows = []
    with tempfile.TemporaryDirectory() as d:
        for name in chosen:
            script = root / f"bm_{name}" / "run_benchmark.py"
            if not script.exists():
                continue
            trace = Path(d) / f"{name}.pb"
            base = time_benchmark(script, None)
            traced = time_benchmark(script, trace)
            if base is None or traced is None:
                print(f"{name:<18}{'skipped (did not run)':>40}")
                continue
            if base < MIN_MEASURABLE:
                print(f"{name:<18}{base * 1000:>9.2f}ms  too short to measure")
                continue
            packets = count_packets(trace) if trace.exists() else 0
            rows.append((name, base, traced, traced / base))
            print(
                f"{name:<18}{base * 1000:>9.1f}ms{traced * 1000:>9.1f}ms"
                f"{traced / base:>7.2f}x{packets:>20,}"
            )

    if rows:
        ratios = sorted(r[3] for r in rows)
        print(f"\nmedian slowdown: {ratios[len(ratios) // 2]:.2f}x over {len(rows)}")
        print(f"  least affected: {min(rows, key=lambda r: r[3])[0]}")
        print(f"  most affected:  {max(rows, key=lambda r: r[3])[0]}")
        print("\nThe spread is call density: a tracer charges per call, so")
        print("arithmetic in tight loops barely notices and call-heavy code pays.")
    return 0


WORKLOAD = HERE / "workload_sqlglot.py"
HYPERFINE_RUNS = 10
SLOW_RUNS = 3
SLOW_SECONDS = 5.0


def alternative_commands(out: Path) -> list[tuple[str, list[str]]]:
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
    """Run once to prove it works, and time it to pick a run count.

    Anything past SLOW_SECONDS gets SLOW_RUNS instead: `profile` is pure
    Python and around 20x slower than the rest, so its own overhead dwarfs
    the noise that fewer runs trade away.
    """
    start = time.perf_counter()
    done = subprocess.run(argv, capture_output=True)
    if done.returncode != 0:
        raise SystemExit(f"{name} failed:\n{done.stderr.decode()}")
    return time.perf_counter() - start


def hyperfine(name: str, argv: list[str], export: Path) -> tuple[float, float]:
    runs = SLOW_RUNS if probe(name, argv) > SLOW_SECONDS else HYPERFINE_RUNS
    subprocess.run(
        ["hyperfine", "--warmup", "1", "--runs", str(runs),
         "--command-name", name, "--export-json", str(export), shlex.join(argv)],
        check=True,
        stdout=subprocess.DEVNULL,
    )
    times = json.loads(export.read_text())["results"][0]["times"]
    return min(times), max(times) - min(times)


def cmd_alternatives(args: argparse.Namespace) -> int:
    """`hyperfine` drives this because the alternatives are whole programs,
    not callables: cProfile and profile are `-m` modules and trace0 is a CLI,
    so process wall time is the only measure that treats them the same.

    The reported spread is max-min. When it approaches the gap between two
    commands the machine was too busy, and the run should be repeated.
    """
    if shutil.which("hyperfine") is None:
        raise SystemExit("hyperfine not found: brew install hyperfine")

    with tempfile.TemporaryDirectory() as d:
        out = Path(d)
        rows = [
            (name, *hyperfine(name, argv, out / "hf.json"))
            for name, argv in alternative_commands(out)
        ]

        base = rows[0][1]
        print(f"\n{'':<20}{'wall time':>11}{'spread':>9}{'vs vanilla':>12}")
        for name, best, spread in rows:
            ratio = "—" if best == base else f"{best / base:.2f}x"
            print(f"{name:<20}{best:>10.3f}s{spread:>8.3f}s{ratio:>12}")

        for label, path in (("protobuf", out / "t.pb"), ("json", out / "t.json")):
            print(f"\n{label} trace: {path.stat().st_size / 1e6:,.0f} MB")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)

    p = sub.add_parser("overhead", help="what the callback costs per event")
    p.add_argument("threads", nargs="*", type=int, help="thread counts (default 1 2 4 8)")
    p.set_defaults(func=cmd_overhead)

    p = sub.add_parser("suite", help="slowdown across CPython's own benchmarks")
    p.add_argument("names", nargs="*", help="benchmarks to run (default all)")
    p.set_defaults(func=cmd_suite)

    p = sub.add_parser("alternatives", help="trace0 against cProfile and profile")
    p.set_defaults(func=cmd_alternatives)

    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
