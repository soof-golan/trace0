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
how long it runs. Reproduce with `scripts/benchmark.py suite`.

Against the alternatives, on a call-dense workload — transpiling SQL with
[sqlglot](https://github.com/tobymao/sqlglot), 10.8M events per second of
work. Reproduce with `scripts/benchmark.py alternatives`:

| | wall time | vs vanilla |
| --- | --- | --- |
| vanilla | 0.97 s | — |
| **trace0** (protobuf) | **1.16 s** | **1.20×** |
| trace0 (json) | 1.35 s | 1.40× |
| `cProfile` | 2.36 s | 2.45× |
| `profile` | 19.11 s | 19.8× |

trace0 adds about a seventh of `cProfile`'s overhead *and* keeps every event,
where `cProfile` only keeps per-function aggregates. That protobuf trace was
197 MB — a full timeline is not free, it is just cheaper in time than in disk.
The same run as JSON is an order of magnitude larger.

For the callback in isolation, `scripts/benchmark.py overhead` reports the
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

## Child processes

Children are traced too, whether they arrive by `fork` or by `exec`, so
`multiprocessing`, `subprocess` and worker-based servers all show up. They
write into the same file as they run, so there is nothing to merge afterwards:

```bash
trace0 run --output trace.pb -m uvicorn app:app --workers 4
```

leaves one `trace.pb` holding every worker, on a track of its own. Each process
buffers whole packets and commits them under a lock, so two processes writing
at once never splice into each other, and each namespaces its packet sequences
by pid so Perfetto keeps them apart.

JSON traces use the Chrome JSON Array Format for the same reason — entries
follow one another with a trailing comma and no closing bracket. That is what
lets several processes append to one array, and it means a process killed
mid-run still leaves everything up to its last whole entry.

## Status

Early. The tracer works end to end and is covered by tests, but the API is not
yet stable and wheels are not yet built for every platform.

## Developing

The benchmarks behind every number above live in one script:

```bash
uv run scripts/benchmark.py --help
```

`prek` runs the CI checks before a commit lands — formatting and clippy on
commit, both test suites on push:

```bash
uv tool install prek && prek install --hook-type pre-commit --hook-type pre-push
```

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
