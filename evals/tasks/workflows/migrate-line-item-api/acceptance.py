"""Independent acceptance checks for the migrate-line-item-api task.

Executed with the task workspace first on sys.path and the workspace as cwd, so
`invoicing` and `app.py` resolve to the agent's files, never to a copy shipped
with the task.
"""
import json
import subprocess
import sys

from invoicing import core, report

# The deprecated positional entry point must be gone, which forces every caller
# to move to the migrated API.
assert not hasattr(core, "compute"), "deprecated core.compute() is still present"
assert callable(getattr(core, "compute_total", None)), "core.compute_total() is missing"

# Preserved line-item behaviour: integer cents, empty -> 0, negative rejected.
assert core.compute_total([]) == 0
assert core.compute_total([{"unit_cents": 250, "quantity": 4}]) == 1000
assert core.compute_total([{"unit_cents": 1, "quantity": 0}]) == 0
try:
    core.compute_total([{"unit_cents": 100, "quantity": -1}])
except ValueError:
    pass
else:
    raise AssertionError("negative quantity must raise ValueError")

# New tax argument: keyword-only basis points, integer half-up rounding.
assert core.compute_total([{"unit_cents": 100, "quantity": 1}], tax_rate_bps=50) == 101
assert core.compute_total([{"unit_cents": 101, "quantity": 1}], tax_rate_bps=825) == 109
assert core.compute_total([{"unit_cents": 999, "quantity": 3}], tax_rate_bps=10000) == 5994
assert core.compute_total([{"unit_cents": 100, "quantity": 1}], tax_rate_bps=0) == 100
try:
    core.compute_total([], 800)
except TypeError:
    pass
else:
    raise AssertionError("tax_rate_bps must be keyword-only")
for bad_bps in (-1, 10001):
    try:
        core.compute_total([], tax_rate_bps=bad_bps)
    except ValueError:
        pass
    else:
        raise AssertionError("out-of-range tax_rate_bps must raise ValueError")

# The roll-up caller must use the migrated API with the agreed default rate.
assert report.DEFAULT_TAX_RATE_BPS == 800, "default roll-up rate changed"
assert report.monthly_total([]) == 0
assert report.monthly_total([{"unit_cents": 250, "quantity": 4}]) == 1080

# The CLI still works end to end.
completed = subprocess.run(
    [sys.executable, "app.py", json.dumps([{"unit_cents": 250, "quantity": 4}])],
    capture_output=True,
    text=True,
)
assert completed.returncode == 0, completed.stderr
assert completed.stdout.strip() == "1080", completed.stdout
