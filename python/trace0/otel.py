"""Attach flight-recorder snapshots to sampled OpenTelemetry traces.

Usage::

    tracer = Tracer("dumps/", record_last_mb=64).__enter__()
    provider.add_span_processor(Trace0SpanProcessor(tracer))

Each sampled local-root span gets a dump of its own time window, named
``otel-{trace_id}-{span_id}`` so the dump joins its trace by file name.
Ended spans are read-only, so the name is the only link.
"""

import time

from opentelemetry.sdk.trace import ReadableSpan, SpanProcessor

from trace0 import Tracer


class Trace0SpanProcessor(SpanProcessor):
    def __init__(self, tracer: Tracer):
        self.tracer = tracer

    def on_end(self, span: ReadableSpan):
        if not span.context.trace_flags.sampled:
            return
        if span.parent is not None and not span.parent.is_remote:
            return
        ctx = span.context
        try:
            anchor = time.time_ns() - self.tracer.now_ns()
            self.tracer.snapshot(
                f"otel-{ctx.trace_id:032x}-{ctx.span_id:016x}",
                start=max(span.start_time - anchor, 0),
                end=max(span.end_time - anchor, 1),
            )
        except RuntimeError:
            pass
