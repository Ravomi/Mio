"""Independent acceptance checks for the stable-toposort task."""

from graph import order_tasks as order

assert order({}) == []
assert order({"build": ["fetch"], "test": ["build"], "lint": []}) == [
    "fetch",
    "build",
    "lint",
    "test",
]

source = {"b": ["a", "a"], "c": []}
assert order(source) == ["a", "b", "c"]
assert source["b"] == ["a", "a"], "order_tasks must not mutate its input"
assert order({"b": iter(["a"])}) == ["a", "b"]

for bad in [{"a": ["a"]}, {"a": ["b"], "b": ["a"]}]:
    try:
        order(bad)
    except ValueError:
        pass
    else:
        raise AssertionError("cycle accepted")
