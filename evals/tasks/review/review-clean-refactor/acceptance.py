"""Independent clean-diff review acceptance: zero false positives."""

import json
from pathlib import Path


root = Path.cwd()
assert (root / "labels.py").read_text() == (
    Path(__file__).parent / "initial/labels.py"
).read_text()
assert (root / "change.diff").read_text() == (
    Path(__file__).parent / "initial/change.diff"
).read_text()
report = root / "review.json"
assert report.stat().st_size <= 16 * 1024, "review exceeds 16 KiB"
result = json.loads(report.read_text())
assert set(result) == {"findings", "conclusion"}
assert result["findings"] == [], "clean refactor must have no defect findings"
assert isinstance(result["conclusion"], str) and result["conclusion"].strip()
