"""The tracer must not decide how long a code object lives, and a freed
code object's address must not inherit its identity.

Code objects die mid-run -- anything that execs templates or snippets
creates and drops them continuously. Pinning them would grow memory for
as long as the run lasts; letting a recycled address reuse a dead
object's interned id would file events under the wrong name. CPython
reuses freed addresses eagerly, so each loop below only has to spin a
few hundred times to put a new code object where a dead one lived.
"""

import gc
import types
import weakref
from collections import Counter

ROUNDS = 300


def begin_counts(events: list[dict], prefix: str) -> dict[str, int]:
    counts = Counter(e["name"] for e in events if e["ph"] == "B")
    return {name: n for name, n in counts.items() if name.startswith(prefix)}


def test_the_run_does_not_keep_a_dead_code_object_alive(traced):
    def workload():
        ns = {}
        exec("def short_lived(): pass", ns)
        fn = ns["short_lived"]
        grave = weakref.ref(fn.__code__)
        fn()
        del fn, ns
        gc.collect()
        assert grave() is None, "the run pinned a code object the program dropped"

    traced(workload)


def test_a_recycled_address_does_not_inherit_the_dead_objects_name(traced):
    def workload():
        for i in range(ROUNDS):
            ns = {}
            exec(f"def marker_{i}(): pass", ns)
            ns[f"marker_{i}"]()
            del ns
            gc.collect()

    events = traced(workload)
    expected = {f"marker_{i}": 1 for i in range(ROUNDS)}
    assert begin_counts(events, "marker_") == expected


def test_reuse_with_no_traced_call_in_between_is_not_misattributed(traced):
    """`compile`, `replace`, and `FunctionType` run no Python on the way to
    the next call, so the recycled address arrives while the previous
    code object is still the last one this thread resolved."""

    def workload():
        for i in range(ROUNDS):
            code = compile("None", "generated", "eval").replace(
                co_name=f"reborn_{i}", co_qualname=f"reborn_{i}"
            )
            fn = types.FunctionType(code, {})
            fn()
            del fn, code
            gc.collect()

    events = traced(workload)
    expected = {f"reborn_{i}": 1 for i in range(ROUNDS)}
    assert begin_counts(events, "reborn_") == expected
