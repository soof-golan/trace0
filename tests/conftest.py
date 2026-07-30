import itertools
import json
from pathlib import Path

import pytest

from trace0 import Tracer


@pytest.fixture
def traced(tmp_path: Path):
    """Run a workload under a tracer and hand back the decoded trace.

    Each call writes its own file, so one test can trace several times and
    compare the runs.
    """
    counter = itertools.count()

    def run(workload) -> dict:
        path = tmp_path / f"trace{next(counter)}.json"
        tracer = Tracer(str(path), format="json")
        tracer.start()
        try:
            workload()
        finally:
            tracer.stop()
        return json.loads(path.read_text())

    return run
