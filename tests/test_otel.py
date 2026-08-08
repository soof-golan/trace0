"""The OTel span processor snapshots the flight recorder around sampled local roots.

The SDK sampler already decided; the processor only reads
`span.context.trace_flags.sampled`. Child spans ride in their root's dump.
"""

from pathlib import Path

from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.sampling import ALWAYS_OFF
from trace_json import load, slice_names

from trace0 import Tracer
from trace0.otel import Trace0SpanProcessor


def work_marker():
    pass


def provider_with(tracer: Tracer, **kwargs) -> TracerProvider:
    provider = TracerProvider(**kwargs)
    provider.add_span_processor(Trace0SpanProcessor(tracer))
    return provider


def otel_dumps(out: Path) -> list[Path]:
    if not out.exists():
        return []
    return sorted(p for p in out.iterdir() if "-otel-" in p.name)


def test_a_sampled_root_span_dumps_a_named_snapshot(tmp_path: Path):
    out = tmp_path / "dumps"
    with Tracer(str(out), format="json", record_last_mb=64) as t:
        otel = provider_with(t).get_tracer("test")
        with otel.start_as_current_span("checkout") as span:
            work_marker()
        ctx = span.get_span_context()
    name = f"otel-{ctx.trace_id:032x}-{ctx.span_id:016x}"
    files = [p for p in otel_dumps(out) if f"-{name}." in p.name]
    assert len(files) == 1
    assert "work_marker" in slice_names(load(files[0]))


def test_a_child_span_rides_in_its_roots_dump(tmp_path: Path):
    out = tmp_path / "dumps"
    with Tracer(str(out), format="json", record_last_mb=64) as t:
        otel = provider_with(t).get_tracer("test")
        with otel.start_as_current_span("parent"):
            with otel.start_as_current_span("child"):
                work_marker()
        assert len(otel_dumps(out)) == 1
    assert "work_marker" in slice_names(load(otel_dumps(out)[0]))


def test_an_unsampled_span_dumps_nothing(tmp_path: Path):
    out = tmp_path / "dumps"
    with Tracer(str(out), format="json", record_last_mb=64) as t:
        otel = provider_with(t, sampler=ALWAYS_OFF).get_tracer("test")
        with otel.start_as_current_span("checkout"):
            work_marker()
        assert otel_dumps(out) == []


def test_a_streaming_tracer_leaves_the_span_alone(tmp_path: Path):
    with Tracer(str(tmp_path / "t.json"), format="json") as t:
        otel = provider_with(t).get_tracer("test")
        with otel.start_as_current_span("checkout"):
            work_marker()
