"""Which format a `Tracer` writes when it is not told.

Protobuf, matching the CLI: it is what ui.perfetto.dev loads, and it is
smaller and faster to write than the JSON alternative.
"""

import json

import pytest

from trace0 import Tracer

PACKET_TAG = 0x0A


def work() -> None:
    sum(range(100))


def trace_to(path, **kwargs) -> None:
    tracer = Tracer(str(path), **kwargs)
    tracer.start()
    work()
    tracer.stop()


def test_the_default_format_is_protobuf(tmp_path):
    out = tmp_path / "out.pb"
    trace_to(out)
    assert out.read_bytes()[0] == PACKET_TAG


def test_protobuf_can_be_asked_for_by_name(tmp_path):
    out = tmp_path / "named.pb"
    trace_to(out, format="protobuf")
    assert out.read_bytes()[0] == PACKET_TAG


def test_json_is_still_available_by_name(tmp_path):
    out = tmp_path / "out.json"
    trace_to(out, format="json")
    assert json.loads(out.read_text())["traceEvents"]


@pytest.mark.parametrize("alias", ["proto", "pb"])
def test_protobuf_answers_to_its_short_names(tmp_path, alias):
    out = tmp_path / f"{alias}.pb"
    trace_to(out, format=alias)
    assert out.read_bytes()[0] == PACKET_TAG


def test_an_unknown_format_is_rejected(tmp_path):
    tracer = Tracer(str(tmp_path / "out"), format="yaml")
    with pytest.raises(OSError, match="unknown format"):
        tracer.start()
