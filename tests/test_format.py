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
    with Tracer(str(path), **kwargs).start():
        work()


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


def test_none_is_not_a_way_to_ask_for_the_default(tmp_path):
    with pytest.raises(TypeError):
        Tracer(str(tmp_path / "out"), format=None)


def test_an_unknown_format_is_rejected_before_anything_is_opened(tmp_path):
    out = tmp_path / "out"
    with pytest.raises(ValueError, match="unknown format: yaml"):
        Tracer(str(out), format="yaml")
    assert not out.exists()
