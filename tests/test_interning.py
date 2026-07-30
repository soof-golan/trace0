"""The per-thread code cache keys on a code object's address.

That is sound only while the interner holds a strong reference to every
code object it has interned. A freed code object's address gets handed
straight back to the next allocation of the same size, and the cache
would answer for it with the dead object's id -- so events would be
recorded under another function's name.
"""

import gc

from trace_json import slice_names

CHURN = 200


def define(name: str):
    scope: dict = {}
    exec(f"def {name}(): pass", scope)
    return scope[name]


def churn(names: list[str]) -> None:
    """Call each function, then drop it so its memory can be recycled."""
    for name in names:
        fn = define(name)
        fn()
        del fn
        gc.collect()


def test_a_recycled_address_does_not_inherit_the_dead_objects_name(traced):
    names = [f"churn_{i}" for i in range(CHURN)]
    recorded = slice_names(traced(lambda: churn(names)))
    missing = sorted(set(names) - recorded)
    assert not missing, (
        f"{len(missing)} of {CHURN} functions lost their name to a recycled "
        f"address, first few: {missing[:5]}"
    )


def test_one_function_called_many_times_interns_one_name(traced):
    def workload():
        fn = define("called_often")
        for _ in range(500):
            fn()

    assert "called_often" in slice_names(traced(workload))
