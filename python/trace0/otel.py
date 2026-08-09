"""Attach flight-recorder snapshots to sampled OpenTelemetry traces.

Usage::

    tracer = Tracer("dumps/", record_last_mb=64).__enter__()
    provider.add_span_processor(Trace0SpanProcessor(tracer))

Each sampled local-root span runs inside a snapshot block named
``otel-{trace_id}-{span_id}``, so the dump joins its trace by file name.
Child spans ride in their root's dump.
"""

from opentelemetry.sdk.trace import ReadableSpan, Span, SpanProcessor

from trace0 import Tracer


def local_root(span: Span) -> bool:
    return span.parent is None or span.parent.is_remote


class Trace0SpanProcessor(SpanProcessor):
    def __init__(self, tracer: Tracer):
        self.tracer = tracer
        self.open = {}

    def on_start(self, span: Span, parent_context=None):
        ctx = span.context
        if not ctx.trace_flags.sampled or not local_root(span):
            return
        try:
            snapshot = self.tracer.snapshot(
                f"otel-{ctx.trace_id:032x}-{ctx.span_id:016x}"
            )
        except RuntimeError:
            return
        self.open[(ctx.trace_id, ctx.span_id)] = snapshot.__enter__()

    def on_end(self, span: ReadableSpan):
        ctx = span.context
        snapshot = self.open.pop((ctx.trace_id, ctx.span_id), None)
        if snapshot is not None:
            snapshot.__exit__(None, None, None)
