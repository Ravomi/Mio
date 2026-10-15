"""Independent acceptance checks for the median-boundaries task.

Executed with the task workspace first on sys.path, so `stats` resolves to the
agent's file, never to a copy shipped with the task.
"""

from stats import median

assert median([9, 1, 5]) == 5
assert median([10, 2, 4, 8]) == 6
assert median(iter([3, 1])) == 2
assert median([-8, -2, -5]) == -5

values = [4, 1, 3]
median(values)
assert values == [4, 1, 3], "median must not mutate its input"

try:
    median([])
except ValueError:
    pass
else:
    raise AssertionError("empty input must raise ValueError")
