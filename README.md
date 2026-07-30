# trace0

A low-overhead tracing profiler for Python, written in Rust.

`trace0` hooks [PEP 669](https://peps.python.org/pep-0669/) `sys.monitoring`,
packs each event into 8 bytes in a thread-local buffer, and hands whole batches
to a background thread over lock-free SPSC rings. It emits
[Perfetto](https://ui.perfetto.dev)-compatible traces — open them at
<https://ui.perfetto.dev>.

## How much does it cost?

Across 14 benchmarks from [pyperformance](https://github.com/python/pyperformance),
the suite CPython itself is judged on, run unmodified on an M4 Mac:

**Median slowdown 1.19×, ranging 0.99× to 1.49×.**

| | slowdown | |
| --- | --- | --- |
| `telco`, `scimark`, `fannkuch`, `nbody` | 0.99–1.03× | arithmetic in tight loops |
| `pyflate`, `chaos`, `float`, `go` | 1.17–1.22× | |
| `raytrace`, `hexiom`, `deltablue`, `richards` | 1.40–1.49× | call-heavy |

The spread *is* the story: a tracer charges per call, so what a program
pays depends entirely on how many calls it makes per unit of work, not on
how long it runs. Reproduce with `scripts/bench_pyperformance.py`.

Against the alternatives, on a call-dense workload — transpiling SQL with
[sqlglot](https://github.com/tobymao/sqlglot), 10.8M events per second of
work, measured with `hyperfine`:

| | wall time | vs vanilla |
| --- | --- | --- |
| vanilla | 0.98 s | — |
| **trace0** (protobuf) | **1.38 s** | **1.41×** |
| `cProfile` | 2.42 s | 2.47× |
| `profile` | 19.27 s | 19.5× |

trace0 costs about a third of what `cProfile` does *and* keeps every event,
where `cProfile` only keeps per-function aggregates. That trace was 210MB —
a full timeline is not free, it is just cheaper in time than in disk.

For the callback in isolation, `scripts/bench_producer.py` reports the
difference against the same workload untraced: **~5 ns per event at 8
threads**, ~14 ns on a single thread. An event is one `PY_START` or
`PY_RETURN`, so a function call is two.

## Try it, without installing anything

`uvx` fetches the wheel, traces your script, and leaves nothing behind:

```bash
uvx trace0 run --output trace.pb --format protobuf your_script.py
```

Drop `trace.pb` onto <https://ui.perfetto.dev> and you have a flame chart.

The traced script runs *inside* the tracer's environment, so give it whatever
it imports with `--with`:

```bash
uvx --with httpx --with pandas trace0 run --output trace.pb your_script.py
```

To trace under a free-threaded interpreter, name it — every thread is traced,
not just the calling one:

```bash
uvx --python 3.13t trace0 run --output trace.pb your_script.py
```

## Install

Add it to a project when you want the `Tracer` API:

```bash
uv add trace0
```

Requires Python 3.13+. Free-threaded builds (3.13t, 3.14t) are a first-class
target.

```python
from trace0 import Tracer

with Tracer("trace.pb", "protobuf"):
    your_workload()
```

The same CLI is then on your project's path:

```bash
uv run trace0 run --output trace.pb --format protobuf your_script.py
```

Formats are `json` (Chrome Trace Event, human-readable and diffable) and
`protobuf` (Perfetto, smaller and faster to write).

## Status

Early. The tracer works end to end and is covered by tests, but the API is not
yet stable and wheels are not yet built for every platform.

## Layout

| crate | role |
| --- | --- |
| `trace0-core` | clock, event model, event queue, exporter contract |
| `trace0-json` | Chrome Trace Event output |
| `trace0-proto` | Perfetto protobuf output |

## License

MIT
