# trace0

A low-overhead tracing profiler for Python, written in Rust.

`trace0` hooks [PEP 669](https://peps.python.org/pep-0669/) `sys.monitoring`,
packs each event into 8 bytes in a thread-local buffer, and hands whole batches
to a background thread over lock-free SPSC rings. It emits
[Perfetto](https://ui.perfetto.dev)-compatible traces — open them at
<https://ui.perfetto.dev>.

Roughly **13 ns per traced function call** at 4 threads on an M-series Mac.

## Install

```bash
uv add trace0
```

Requires Python 3.13+. Free-threaded builds (3.13t, 3.14t) are a first-class
target — every thread in the interpreter is traced, not just the calling one.
To profile under a free-threaded interpreter specifically:

```bash
uv add trace0 --python 3.13t
```

## Use

As a context manager:

```python
from trace0 import Tracer

with Tracer("trace.pb", "protobuf"):
    your_workload()
```

Or from the command line:

```bash
uv run trace0 run --output trace.pb --format protobuf your_script.py
```

Without adding it to a project, use `uvx`:

```bash
uvx trace0 run --output trace.pb --format protobuf your_script.py
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
