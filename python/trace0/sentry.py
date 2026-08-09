"""Attach flight-recorder snapshots to sampled Sentry transactions.

Usage::

    tracer = Tracer("dumps/", record_last_mb=64).__enter__()
    sentry_sdk.init(..., integrations=[Trace0Integration(tracer)])

trace0 is a tracer, so it follows Sentry's tracing decision: every sampled
transaction runs inside a snapshot block, and the dump file travels with the
transaction envelope as an attachment. An unsampled transaction opens no
snapshot, so it costs nothing.
"""

import sentry_sdk
from sentry_sdk.integrations import Integration
from sentry_sdk.tracing import Transaction

from trace0 import Tracer


class Trace0Integration(Integration):
    identifier = "trace0"

    def __init__(self, tracer: Tracer):
        self.tracer = tracer
        self.open = {}

    @staticmethod
    def setup_once():
        start_transaction = sentry_sdk.Scope.start_transaction
        finish = Transaction.finish

        def start_inside_a_snapshot(scope, *args, **kwargs):
            transaction = start_transaction(scope, *args, **kwargs)
            integration = sentry_sdk.get_client().get_integration(Trace0Integration)
            if integration is None or not getattr(transaction, "sampled", None):
                return transaction
            try:
                block = integration.tracer.snapshot(f"sentry-{transaction.name}")
            except RuntimeError:
                return transaction
            integration.open[transaction.span_id] = (block, block.__enter__())
            return transaction

        def finish_the_snapshot(transaction, *args, **kwargs):
            integration = sentry_sdk.get_client().get_integration(Trace0Integration)
            opened = None
            if integration is not None:
                opened = integration.open.pop(transaction.span_id, None)
            if opened is None:
                return finish(transaction, *args, **kwargs)
            block, snapshot = opened
            block.__exit__(None, None, None)
            with sentry_sdk.new_scope() as scope:
                scope.add_attachment(path=snapshot.path, add_to_transactions=True)
                return finish(transaction, *args, **kwargs)

        sentry_sdk.Scope.start_transaction = start_inside_a_snapshot
        Transaction.finish = finish_the_snapshot
