#!/usr/bin/env python3
"""Check both sides of the independent structured-review task oracles."""

import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
TASKS = ROOT / "evals/tasks/review"


def run(task_name, report):
    task = TASKS / task_name
    with tempfile.TemporaryDirectory(prefix="mio-review-oracle-") as directory:
        work = Path(directory)
        for source in (task / "initial").iterdir():
            shutil.copy2(source, work / source.name)
        (work / "review.json").write_text(json.dumps(report))
        return subprocess.run(
            [sys.executable, "-I", str(task / "acceptance.py")],
            cwd=work,
            capture_output=True,
            text=True,
            timeout=15,
            check=False,
        ).returncode


def verify_reverse_diffs():
    for task in sorted(TASKS.iterdir()):
        if not task.is_dir():
            continue
        with tempfile.TemporaryDirectory(prefix="mio-review-diff-") as directory:
            work = Path(directory)
            for source in (task / "initial").iterdir():
                shutil.copy2(source, work / source.name)
            proc = subprocess.run(
                ["git", "apply", "--reverse", "--check", "change.diff"],
                cwd=work,
                capture_output=True,
                text=True,
                timeout=15,
                check=False,
            )
            assert proc.returncode == 0, (
                f"{task.name}: reverse diff check failed: {proc.stderr}"
            )


def finding(line):
    if line == 7:
        evidence = "key = account_id loses the tenant namespace and leaks cached records"
        explanation = "Account IDs collide across tenants in the shared cache."
    else:
        evidence = 'return account.status == "active" or "trial" is always truthy'
        explanation = "The literal allows any status to enter."
    return {
        "severity": "high",
        "confidence": 0.9,
        "path": "service.py",
        "line": line,
        "evidence": evidence,
        "explanation": explanation,
    }


def invoice_finding(line, kind="combined"):
    if line in (6, 7):
        evidence = 'line_total = round(item["unit_cents"] * item["qty"] * (1 - item.get("discount_rate", 0.0)))'
        if kind == "field":
            explanation = "Old discount_bps callers are ignored after the field rename."
        elif kind == "rounding":
            explanation = "Float rounding changes integer cents totals."
        else:
            explanation = "Old discount_bps callers are ignored and float rounding changes integer cents totals."
    else:
        evidence = 'if coupon is not None: discount += coupon["off_cents"]'
        explanation = "Stacks coupon off_cents on top of existing_discount_cents instead of requiring existing_discount_cents == 0."
    return {
        "severity": "high",
        "confidence": 0.92,
        "path": "invoice.py",
        "line": line,
        "evidence": evidence,
        "explanation": explanation,
    }


def retry_finding(line=9):
    return {
        "severity": "medium",
        "confidence": 0.95,
        "path": "retry.py",
        "line": line,
        "evidence": "if attempt > max_attempts: return None",
        "explanation": "Off-by-one boundary: attempt == max_attempts now returns a backoff instead of None, allowing one extra retry.",
    }


def main():
    verify_reverse_diffs()

    # 1. review-session-cache (two findings)
    good = {"findings": [finding(7), finding(14)], "conclusion": "Two regressions."}
    assert run("review-session-cache", good) == 0
    assert run("review-session-cache", {"findings": [finding(7), finding(13)], "conclusion": "Adjacent function line."}) == 0
    assert run("review-session-cache", {"findings": [finding(7)], "conclusion": "One."}) != 0
    assert run("review-session-cache", {"findings": [finding(7), finding(7)], "conclusion": "Duplicate."}) != 0
    assert run("review-session-cache", {"findings": [finding(7), finding(12)], "conclusion": "Wrong line."}) != 0
    empty_reason = finding(7)
    empty_reason["evidence"] = "introduced behavior at this line"
    empty_reason["explanation"] = "independent regression"
    assert run("review-session-cache", {"findings": [empty_reason, finding(14)], "conclusion": "Unsupported."}) != 0
    wrong_cache_cause = finding(7)
    wrong_cache_cause["evidence"] = "key = account_id speeds up lookup"
    wrong_cache_cause["explanation"] = "A style preference."
    assert run("review-session-cache", {"findings": [wrong_cache_cause, finding(14)], "conclusion": "Wrong cause."}) != 0
    wrong_access_cause = finding(14)
    wrong_access_cause["evidence"] = 'return account.status == "active" or "trial"'
    wrong_access_cause["explanation"] = "Trial users can enter."
    assert run("review-session-cache", {"findings": [finding(7), wrong_access_cause], "conclusion": "Wrong cause."}) != 0

    # 2. review-clean-refactor (zero findings)
    clean = {"findings": [], "conclusion": "No actionable defects."}
    assert run("review-clean-refactor", clean) == 0
    assert run("review-clean-refactor", {"findings": [finding(5)], "conclusion": "False alarm."}) != 0
    assert run("review-clean-refactor", {"findings": [], "conclusion": "x" * 17_000}) != 0

    # 3. review-invoice-totals (three independent causes in two changed expressions)
    assert run(
        "review-invoice-totals",
        {"findings": [invoice_finding(7), invoice_finding(11)], "conclusion": "Three billing regressions."},
    ) == 0
    assert run(
        "review-invoice-totals",
        {"findings": [invoice_finding(7, "field"), invoice_finding(7, "rounding"), invoice_finding(11)], "conclusion": "Three billing regressions."},
    ) == 0
    assert run(
        "review-invoice-totals",
        {"findings": [invoice_finding(7, "field"), invoice_finding(11)], "conclusion": "Missed rounding bug."},
    ) != 0
    assert run(
        "review-invoice-totals",
        {"findings": [invoice_finding(7, "rounding"), invoice_finding(11)], "conclusion": "Missed field bug."},
    ) != 0
    assert run(
        "review-invoice-totals",
        {"findings": [invoice_finding(6), invoice_finding(10)], "conclusion": "Adjacent lines."},
    ) == 0
    assert run(
        "review-invoice-totals",
        {"findings": [invoice_finding(7)], "conclusion": "Omitted coupon bug."},
    ) != 0
    assert run(
        "review-invoice-totals",
        {"findings": [invoice_finding(6), invoice_finding(7)], "conclusion": "Duplicate slot."},
    ) != 0
    assert run(
        "review-invoice-totals",
        {"findings": [invoice_finding(7), invoice_finding(13)], "conclusion": "Wrong line."},
    ) != 0
    bad_invoice = invoice_finding(7)
    bad_invoice["evidence"] = 'line_total = round(item["unit_cents"])'
    bad_invoice["explanation"] = "Looks shorter."
    assert run(
        "review-invoice-totals",
        {"findings": [bad_invoice, invoice_finding(11)], "conclusion": "Unsupported cause."},
    ) != 0

    # 4. review-retry-backoff (single finding)
    assert run(
        "review-retry-backoff",
        {"findings": [retry_finding(9)], "conclusion": "Off-by-one retry limit."},
    ) == 0
    assert run(
        "review-retry-backoff",
        {"findings": [retry_finding(10)], "conclusion": "Return line."},
    ) == 0
    assert run(
        "review-retry-backoff",
        {"findings": [retry_finding(8)], "conclusion": "Adjacent preceding return line with exact changed guard quoted."},
    ) == 0
    assert run(
        "review-retry-backoff",
        {"findings": [], "conclusion": "Missed defect."},
    ) != 0
    assert run(
        "review-retry-backoff",
        {"findings": [retry_finding(9), retry_finding(10)], "conclusion": "Duplicate finding."},
    ) != 0
    assert run(
        "review-retry-backoff",
        {"findings": [retry_finding(7)], "conclusion": "Wrong guard."},
    ) != 0
    bad_retry = retry_finding(9)
    bad_retry["explanation"] = "Different comparison operator."
    assert run(
        "review-retry-backoff",
        {"findings": [bad_retry], "conclusion": "Missing boundary consequence."},
    ) != 0

    # 5. review-clean-guard-clauses (zero findings)
    assert run("review-clean-guard-clauses", clean) == 0
    assert run(
        "review-clean-guard-clauses",
        {"findings": [retry_finding(8)], "conclusion": "False alarm on clean guard."},
    ) != 0

    print("PASS: review oracles (5 tasks) verify reverse diffs and reject omissions, duplicates, false positives, unsupported causes and oversized reports")


if __name__ == "__main__":
    main()
