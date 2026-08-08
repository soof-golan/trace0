"""Attach flight-recorder snapshots to sampled Sentry transactions.

Usage::

    tracer = Tracer("dumps/", record_last_mb=64).__enter__()
    sentry_sdk.init(..., integrations=[Trace0Integration(tracer)])

Each transaction that Sentry itself sampled for profiling (per
``profiles_sample_rate`` or ``profiles_sampler``) gets a dump of its own
time window, and the event carries the dump path under
``contexts.trace0.dump``. The integration reads Sentry's per-transaction
decision; it never rolls its own.
"""

import time

import sentry_sdk
from sentry_sdk.integrations import Integration
from sentry_sdk.scope import add_global_event_processor

from trace0 import Tracer


def profiled(event) -> bool:
    profile = event.get("profile")
    if profile is not None:
        return bool(profile.sampled)
    transaction = sentry_sdk.get_current_scope().transaction
    if transaction is None or transaction._profile is None:
        return False
    if transaction.span_id != event["contexts"]["trace"]["span_id"]:
        return False
    return bool(transaction._profile.sampled)


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
            if not profiled(event):
                return event
            tracer = integration.tracer
            name = event.get("transaction") or "transaction"
            try:
                anchor = time.time_ns() - tracer.now_ns()
                start = int(event["start_timestamp"].timestamp() * 1e9) - anchor
                end = int(event["timestamp"].timestamp() * 1e9) - anchor
                dump = tracer.snapshot(
                    f"sentry-{name}", start=max(start, 0), end=max(end, 1)
                )
            except RuntimeError:
                return event
            event.setdefault("contexts", {})["trace0"] = {"dump": dump}
            return event
