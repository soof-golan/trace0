"""The Sentry integration snapshots the flight recorder around sampled transactions.

trace0 is a tracer, so it follows Sentry's tracing decision: every sampled
transaction gets a dump, attached to its envelope. An unsampled transaction
produces no event, so the processor never runs and nothing is written.
"""

import time
from pathlib import Path

import sentry_sdk
from sentry_sdk.transport import Transport
from trace_json import load, slice_names

from trace0 import Tracer
from trace0.sentry import Trace0Integration


class RecordingTransport(Transport):
    def __init__(self):
        super().__init__()
        self.envelopes = []

    def capture_envelope(self, envelope):
        self.envelopes.append(envelope)


def checkout_marker():
    time.sleep(0.005)


def init_sentry(tracer: Tracer, transport: RecordingTransport, **rates):
    sentry_sdk.init(
        dsn="http://key@localhost:9/1",
        transport=transport,
        integrations=[Trace0Integration(tracer)],
        default_integrations=False,
        **rates,
    )


def transaction_events(transport: RecordingTransport) -> list[dict]:
    events = [e.get_transaction_event() for e in transport.envelopes]
    return [e for e in events if e is not None]


def sentry_dumps(out: Path) -> list[Path]:
    if not out.exists():
        return []
    return sorted(p for p in out.iterdir() if "-sentry-" in p.name)


def transaction_envelopes(transport: RecordingTransport) -> list:
    return [e for e in transport.envelopes if e.get_transaction_event() is not None]


def attachments(envelope) -> list:
    return [item for item in envelope.items if item.type == "attachment"]


def test_a_sampled_transaction_attaches_the_dump(tmp_path: Path):
    out = tmp_path / "dumps"
    transport = RecordingTransport()
    with Tracer(str(out), format="json", record_last_mb=64) as t:
        init_sentry(t, transport, traces_sample_rate=1.0)
        with sentry_sdk.start_transaction(name="checkout"):
            checkout_marker()
            time.sleep(0.002)
        sentry_sdk.flush()
    [envelope] = transaction_envelopes(transport)
    [attachment] = attachments(envelope)
    [dump] = sentry_dumps(out)
    assert "-sentry-checkout." in dump.name
    assert attachment.headers["filename"] == dump.name
    assert attachment.get_bytes() == dump.read_bytes()
    assert "checkout_marker" in slice_names(load(dump))


def test_each_transaction_carries_only_its_own_dump(tmp_path: Path):
    out = tmp_path / "dumps"
    transport = RecordingTransport()
    with Tracer(str(out), format="json", record_last_mb=64) as t:
        init_sentry(t, transport, traces_sample_rate=1.0)
        with sentry_sdk.start_transaction(name="checkout"):
            checkout_marker()
        with sentry_sdk.start_transaction(name="browse"):
            checkout_marker()
        sentry_sdk.flush()
    envelopes = transaction_envelopes(transport)
    assert len(envelopes) == 2
    names = []
    for envelope in envelopes:
        [attachment] = attachments(envelope)
        names.append(attachment.headers["filename"])
    assert "-sentry-checkout." in names[0]
    assert "-sentry-browse." in names[1]


def test_an_unsampled_transaction_dumps_nothing(tmp_path: Path):
    out = tmp_path / "dumps"
    transport = RecordingTransport()
    with Tracer(str(out), format="json", record_last_mb=64) as t:
        init_sentry(t, transport, traces_sample_rate=0.0)
        with sentry_sdk.start_transaction(name="checkout"):
            checkout_marker()
        sentry_sdk.flush()
        assert sentry_dumps(out) == []
    assert transaction_events(transport) == []


def test_a_streaming_tracer_leaves_the_transaction_alone(tmp_path: Path):
    transport = RecordingTransport()
    with Tracer(str(tmp_path / "t.json"), format="json") as t:
        init_sentry(t, transport, traces_sample_rate=1.0)
        with sentry_sdk.start_transaction(name="checkout"):
            checkout_marker()
        sentry_sdk.flush()
    envelopes = transaction_envelopes(transport)
    assert len(envelopes) == 1
    assert attachments(envelopes[0]) == []
