"""The Sentry integration snapshots the flight recorder around sampled transactions.

Transaction sampling is free: an unsampled transaction produces no event,
so the processor never runs. Profile sampling reads the client's
`profiles_sample_rate` and rolls per transaction.
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


def test_a_sampled_transaction_dumps_and_links_the_snapshot(tmp_path: Path):
    out = tmp_path / "dumps"
    transport = RecordingTransport()
    with Tracer(str(out), format="json", record_last_mb=64) as t:
        init_sentry(t, transport, traces_sample_rate=1.0, profiles_sample_rate=1.0)
        with sentry_sdk.start_transaction(name="checkout"):
            checkout_marker()
            time.sleep(0.002)
        sentry_sdk.flush()
    events = transaction_events(transport)
    assert len(events) == 1
    dump = events[0]["contexts"]["trace0"]["dump"]
    assert "-sentry-checkout." in Path(dump).name
    assert "checkout_marker" in slice_names(load(Path(dump)))


def test_a_zero_profiles_rate_keeps_the_transaction_but_skips_the_dump(tmp_path: Path):
    out = tmp_path / "dumps"
    transport = RecordingTransport()
    with Tracer(str(out), format="json", record_last_mb=64) as t:
        init_sentry(t, transport, traces_sample_rate=1.0)
        with sentry_sdk.start_transaction(name="checkout"):
            checkout_marker()
        sentry_sdk.flush()
        assert sentry_dumps(out) == []
    events = transaction_events(transport)
    assert len(events) == 1
    assert "trace0" not in events[0].get("contexts", {})


def test_a_profiles_sampler_grant_dumps_the_snapshot(tmp_path: Path):
    out = tmp_path / "dumps"
    transport = RecordingTransport()
    with Tracer(str(out), format="json", record_last_mb=64) as t:
        init_sentry(
            t,
            transport,
            traces_sample_rate=1.0,
            profiles_sampler=lambda context: 1.0,
        )
        with sentry_sdk.start_transaction(name="checkout"):
            checkout_marker()
        sentry_sdk.flush()
        assert len(sentry_dumps(out)) == 1
    events = transaction_events(transport)
    assert len(events) == 1
    assert "trace0" in events[0]["contexts"]


def test_a_profiles_sampler_veto_skips_the_dump(tmp_path: Path):
    out = tmp_path / "dumps"
    transport = RecordingTransport()
    with Tracer(str(out), format="json", record_last_mb=64) as t:
        init_sentry(
            t,
            transport,
            traces_sample_rate=1.0,
            profiles_sample_rate=1.0,
            profiles_sampler=lambda context: 0.0,
        )
        with sentry_sdk.start_transaction(name="checkout"):
            checkout_marker()
        sentry_sdk.flush()
        assert sentry_dumps(out) == []
    events = transaction_events(transport)
    assert len(events) == 1
    assert "trace0" not in events[0].get("contexts", {})


def test_an_unsampled_transaction_dumps_nothing(tmp_path: Path):
    out = tmp_path / "dumps"
    transport = RecordingTransport()
    with Tracer(str(out), format="json", record_last_mb=64) as t:
        init_sentry(t, transport, traces_sample_rate=0.0, profiles_sample_rate=1.0)
        with sentry_sdk.start_transaction(name="checkout"):
            checkout_marker()
        sentry_sdk.flush()
        assert sentry_dumps(out) == []
    assert transaction_events(transport) == []


def test_a_streaming_tracer_leaves_the_transaction_alone(tmp_path: Path):
    transport = RecordingTransport()
    with Tracer(str(tmp_path / "t.json"), format="json") as t:
        init_sentry(t, transport, traces_sample_rate=1.0, profiles_sample_rate=1.0)
        with sentry_sdk.start_transaction(name="checkout"):
            checkout_marker()
        sentry_sdk.flush()
    events = transaction_events(transport)
    assert len(events) == 1
    assert "trace0" not in events[0].get("contexts", {})
