"""Independent review acceptance for three introduced billing regressions."""

import json
from pathlib import Path


root = Path.cwd()
invoice = root / "invoice.py"
diff = root / "change.diff"
report = root / "review.json"
assert invoice.read_text() == (Path(__file__).parent / "initial/invoice.py").read_text()
assert diff.read_text() == (Path(__file__).parent / "initial/change.diff").read_text()
assert report.stat().st_size <= 16 * 1024, "review exceeds 16 KiB"
result = json.loads(report.read_text())
assert set(result) == {"findings", "conclusion"}
assert isinstance(result["conclusion"], str) and result["conclusion"].strip()
findings = result["findings"]
assert isinstance(findings, list) and len(findings) in (2, 3), "expected two or three distinct findings"
slots = set()
for finding in findings:
    assert set(finding) == {
        "severity", "confidence", "path", "line", "evidence", "explanation"
    }
    assert finding["path"] == "invoice.py"
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
    explanation = finding["explanation"].lower()
    if line in (6, 7):
        assert any(token in evidence for token in ("discount_rate", "round(", "1 -")), (
            "line-item finding must quote the changed float/discount_rate expression"
        )
        claims = set()
        if "discount_bps" in explanation and any(
            word in explanation
            for word in ("renam", "old key", "existing caller", "ignored", "dropped", "fallback", "fall back", "compatib", "旧字段", "忽略", "兼容")
        ):
            claims.add("field")
        if any(word in explanation for word in ("float", "round", "precision", "binary", "浮点", "精度", "舍入")) and any(
            word in explanation for word in ("cent", "integer", "total", "off-by-one", "分", "金额")
        ):
            claims.add("rounding")
        assert claims, "line-item finding must explain the old-key or cent-rounding consequence"
        assert not slots.intersection(claims), "duplicate line-item regression"
        slots.update(claims)
    elif line in (10, 11):
        assert any(
            token in evidence
            for token in ("discount +=", 'coupon["off_cents"]', "coupon['off_cents']", "if coupon is not None")
        ), "coupon finding must quote the changed coupon accumulation"
        assert any(
            word in cause
            for word in (
                "stack", "double", "existing_discount", "cumulat", "add",
                "叠加", "重复", "累加", "双重",
            )
        ), "coupon finding must explain coupon stacking with existing_discount_cents"
        assert "coupon" not in slots, "duplicate coupon regression"
        slots.add("coupon")
    else:
        raise AssertionError(f"unexpected finding line: {line}")
assert slots == {"field", "rounding", "coupon"}, f"missing or duplicate finding slots: {slots}"
