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

**Median slowdown 1.19×, ranging 1.00× to 1.48×.**

| | slowdown | |
| --- | --- | --- |
| `telco`, `fannkuch`, `nbody`, `scimark` | 1.00–1.04× | arithmetic in tight loops |
| `json_dumps`, `pyflate`, `chaos`, `float`, `go`, `spectral_norm` | 1.12–1.25× | |
| `raytrace`, `hexiom`, `deltablue`, `richards` | 1.36–1.48× | call-heavy |

The spread *is* the story: a tracer charges per call, so what a program
pays depends entirely on how many calls it makes per unit of work, not on
how long it runs. Reproduce with `scripts/bench_pyperformance.py`.

Against the alternatives, on a call-dense workload — transpiling SQL with
[sqlglot](https://github.com/tobymao/sqlglot), 10.8M events per second of
work. Reproduce with `scripts/bench_alternatives.py`:

| | wall time | vs vanilla |
| --- | --- | --- |
| vanilla | 0.97 s | — |
| **trace0** (protobuf) | **1.16 s** | **1.20×** |
| trace0 (json) | 1.35 s | 1.40× |
| `cProfile` | 2.36 s | 2.45× |
| `profile` | 19.11 s | 19.8× |

trace0 adds about a seventh of `cProfile`'s overhead *and* keeps every event,
where `cProfile` only keeps per-function aggregates. That protobuf trace was
210 MB — a full timeline is not free, it is just cheaper in time than in disk.
The same run as JSON is 3 GB.

For the callback in isolation, `scripts/bench_producer.py` reports the
difference against the same workload untraced: **~4 ns per event at 8
threads**, ~13 ns on a single thread. An event is one `PY_START` or
`PY_RETURN`, so a function call is two.

## Try it

No install — `uvx` fetches the wheel, traces your script, and leaves nothing
behind:

```bash
uvx trace0 run --output trace.pb your_script.py
```

Drop `trace.pb` onto <https://ui.perfetto.dev> and you have a flame chart.

The traced script runs *inside* the tracer's environment, so name what it
imports with `--with`, and the interpreter with `--python`:

```bash
uvx --with httpx --python 3.13t trace0 run --output trace.pb your_script.py
```

Free-threaded builds (3.13t, 3.14t) are a first-class target — every thread is
traced, not just the calling one.

`-m` runs a library module, exactly as `python -m` does; everything after the
module name belongs to it:

```bash
uvx --with uvicorn trace0 run --output trace.pb -m uvicorn app:app --port 8000
```

A server traced from launch spends most of its trace importing. To trace only
the part you care about, wrap it with the `Tracer` API below instead.

## Install

Install it for the `Tracer` API, which also puts the same CLI on your
project's path. Requires Python 3.13+.

```bash
uv add trace0
```

```python
from trace0 import Tracer

with Tracer("trace.pb"):
    your_workload()
```

Output is Perfetto protobuf by default. `--format json`, or
`Tracer(..., format="json")`, writes Chrome Trace Event instead — larger and
slower to write, but human-readable and diffable.

## Status

Early. The tracer works end to end and is covered by tests, but the API is not
yet stable and wheels are not yet built for every platform.

## Layout

Rust lives under `crates/`, Python under `python/`.

| crate | role |
| --- | --- |
| `trace0-core` | clock, event model, event queue, exporter contract |
| `trace0-json` | Chrome Trace Event output |
| `trace0-proto` | Perfetto protobuf output |
| `trace0-py` | the `sys.monitoring` callbacks and the `_core` extension |

Only `trace0-py` links against CPython, and it is the one crate with no
tests: a cdylib built with `extension-module` leaves the interpreter's
symbols undefined, so it cannot link outside maturin. Everything worth
testing lives in the other three, which `cargo test` runs directly.

## License

MIT
