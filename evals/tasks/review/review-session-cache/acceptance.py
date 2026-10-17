"""Independent review acceptance for two introduced defects and their causes."""

import json
from pathlib import Path


root = Path.cwd()
service = root / "service.py"
diff = root / "change.diff"
report = root / "review.json"
assert service.read_text() == (Path(__file__).parent / "initial/service.py").read_text()
assert diff.read_text() == (Path(__file__).parent / "initial/change.diff").read_text()
assert report.stat().st_size <= 16 * 1024, "review exceeds 16 KiB"
result = json.loads(report.read_text())
assert set(result) == {"findings", "conclusion"}
assert isinstance(result["conclusion"], str) and result["conclusion"].strip()
findings = result["findings"]
assert isinstance(findings, list) and len(findings) == 2, "expected two distinct findings"
locations = set()
for finding in findings:
    assert set(finding) == {
        "severity", "confidence", "path", "line", "evidence", "explanation"
    }
    assert finding["path"] == "service.py"
    assert finding["severity"] in {"high", "medium", "low"}
    assert type(finding["confidence"]) in (int, float)
    assert 0 <= finding["confidence"] <= 1
    assert isinstance(finding["evidence"], str) and finding["evidence"].strip()
    assert isinstance(finding["explanation"], str) and finding["explanation"].strip()
    assert len(finding["evidence"]) <= 2048
    assert len(finding["explanation"]) <= 2048
    assert type(finding["line"]) is int
    line = finding["line"]
    evidence = finding["evidence"].lower()
    cause = (finding["evidence"] + " " + finding["explanation"]).lower()
    if line == 7:
        assert "key = account_id" in evidence, "cache finding must quote the changed key"
        assert any(word in cause for word in ("tenant", "租户")), "cache finding must identify tenant scope"
        assert any(word in cause for word in ("leak", "collid", "mix", "wrong account", "串", "泄露", "冲突")), "cache finding must explain the collision"
    elif line in (13, 14):
        assert 'or "trial"' in evidence or "or 'trial'" in evidence, "access finding must quote the changed expression"
        assert any(word in cause for word in ("truth", "always", "unconditional", "any status", "all status", "恒", "始终", "所有状态")), "access finding must explain unconditional access"
    locations.add(line)
assert 7 in locations and len(locations & {13, 14}) == 1, (
    f"wrong or duplicate locations: {locations}"
)
