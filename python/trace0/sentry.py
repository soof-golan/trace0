"""Attach flight-recorder snapshots to sampled Sentry transactions.

Usage::

    tracer = Tracer("dumps/", record_last_mb=64).__enter__()
    sentry_sdk.init(..., integrations=[Trace0Integration(tracer)])

trace0 is a tracer, so it follows Sentry's tracing decision: every sampled
transaction gets a dump of its own time window, and the event carries the
dump path under ``contexts.trace0.dump``. An unsampled transaction produces
no event, so it costs nothing.
"""

import sentry_sdk
from sentry_sdk.integrations import Integration
from sentry_sdk.scope import add_global_event_processor

from trace0 import Tracer


class Trace0Integration(Integration):
    identifier = "trace0"

    def __init__(self, tracer: Tracer):
        self.tracer = tracer

    @staticmethod
    def setup_once():
        @add_global_event_processor
        def attach_snapshot(event, hint):
            if event.get("type") != "transaction":
                return event
            client = sentry_sdk.get_client()
            integration = client.get_integration(Trace0Integration)
            if integration is None:
                return event
            tracer = integration.tracer
            name = event.get("transaction") or "transaction"
            try:
                dump = tracer.snapshot(
                    f"sentry-{name}",
                    start=int(event["start_timestamp"].timestamp() * 1e9),
                    end=int(event["timestamp"].timestamp() * 1e9),
                )
            except RuntimeError:
                return event
            event.setdefault("contexts", {})["trace0"] = {"dump": dump}
            return event
