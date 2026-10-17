"""Independent review acceptance for a single off-by-one retry ceiling bug."""

import json
from pathlib import Path


root = Path.cwd()
retry = root / "retry.py"
diff = root / "change.diff"
report = root / "review.json"
assert retry.read_text() == (Path(__file__).parent / "initial/retry.py").read_text()
assert diff.read_text() == (Path(__file__).parent / "initial/change.diff").read_text()
assert report.stat().st_size <= 16 * 1024, "review exceeds 16 KiB"
result = json.loads(report.read_text())
assert set(result) == {"findings", "conclusion"}
assert isinstance(result["conclusion"], str) and result["conclusion"].strip()
findings = result["findings"]
assert isinstance(findings, list) and len(findings) == 1, "expected exactly one finding"
finding = findings[0]
assert set(finding) == {
    "severity", "confidence", "path", "line", "evidence", "explanation"
}
assert finding["path"] == "retry.py"
assert finding["severity"] in {"high", "medium", "low"}
assert type(finding["confidence"]) in (int, float)
assert 0 <= finding["confidence"] <= 1
assert isinstance(finding["evidence"], str) and finding["evidence"].strip()
assert isinstance(finding["explanation"], str) and finding["explanation"].strip()
assert len(finding["evidence"]) <= 2048
assert len(finding["explanation"]) <= 2048
assert type(finding["line"]) is int
assert finding["line"] in (8, 9, 10), f"unexpected line: {finding['line']}"
evidence = finding["evidence"].lower()
cause = (finding["evidence"] + " " + finding["explanation"]).lower()
assert "attempt > max_attempts" in evidence, "finding must quote 'attempt > max_attempts'"
assert any(
    word in cause
    for word in (
        "off-by-one", "extra", "exceed", "attempt == max_attempts", "max_attempts + 1",
        "one more", "boundary", "多一次", "超出", "边界",
    )
), "finding must explain that attempt == max_attempts is now retried"
