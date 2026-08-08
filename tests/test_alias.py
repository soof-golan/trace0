"""`tracezero` ships as an alias of `trace0`, from the same tag.

The alias is a pure-Python shim whose only job is to depend on the real
package, pinned to the exact version released beside it. A drifting pin
would hand someone yesterday's tracer under today's version number.
"""

import tomllib
from pathlib import Path

ROOT = Path(__file__).parent.parent


def project(path: str) -> dict:
    with (ROOT / path / "pyproject.toml").open("rb") as f:
        return tomllib.load(f)["project"]


def test_the_alias_carries_the_same_version():
    assert project("alias/tracezero")["version"] == project(".")["version"]


def test_the_alias_pins_the_real_package_to_that_exact_version():
    version = project(".")["version"]
    assert project("alias/tracezero")["dependencies"] == [f"trace0=={version}"]


def test_the_alias_offers_the_same_entry_point():
    assert project("alias/tracezero")["scripts"]["tracezero"] == (
        project(".")["scripts"]["trace0"]
    )
