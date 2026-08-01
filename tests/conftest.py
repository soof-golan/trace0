import itertools
from pathlib import Path

import pytest

from trace0 import Tracer
from trace_json import load


@pytest.fixture
def traced(tmp_path: Path):
    """Run a workload under a tracer and hand back the decoded trace.

    Each call writes its own file, so one test can trace several times and
    compare the runs.
    """
    counter = itertools.count()

    def run(workload) -> list[dict]:
        path = tmp_path / f"trace{next(counter)}.json"
        with Tracer(str(path), format="json"):
            workload()
        return load(path)

    return run
