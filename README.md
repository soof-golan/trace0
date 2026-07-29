# trace0

A low-overhead tracing profiler for Python, written in Rust.

`trace0` hooks [PEP 669](https://peps.python.org/pep-0669/) `sys.monitoring`,
packs each event into 8 bytes in a thread-local buffer, and hands whole batches
to a background thread over lock-free SPSC rings. It emits
[Perfetto](https://ui.perfetto.dev)-compatible traces — open them at
<https://ui.perfetto.dev>.

Roughly **13 ns per traced function call** at 4 threads on an M-series Mac.

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
