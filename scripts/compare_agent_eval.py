#!/usr/bin/env python3
r"""Compare two agent_eval reports, but only when their profiles actually match.

This is a companion to scripts/agent_eval.py. It reads two versioned
`mio-eval-report-v1` reports and refuses to print a direct comparison unless the
task set, provider, budget profile, model, thinking level, fixed sampling
temperature, per-request output cap, declared budget and runner-enforced wall
timeout are identical. That guard exists because a success rate only means
something under the same task set and the same declared profile, including the
sampling settings that shaped the answers; comparing different runs would
silently launder incomparable numbers into a "comparison".

The output keeps unknown metrics unknown: any counter the underlying report did
not fully observe stays `null` instead of being shown as 0, and the script never
claims a winner, ranking or token hard budget. Only the runner's per-task
wall-clock timeout is enforced; the budget block is declared metadata on both
sides.

The two agents do not share a model finish-reason vocabulary. Each side's native
counters stay under its own key (`stop_reasons` for Pi, `finish_reasons` for Mio),
only `stop`/`length`/`toolUse`-`tool_calls` are treated as the same event, and the
`aborted_model_calls` counter is reported as incomparable instead of being shown
as a zero for the agent that cannot report it.

Usage:
  python3 scripts/compare_agent_eval.py \
    --baseline target/evals/mio-deepseek-comparison.json \
    --candidate target/evals/pi-deepseek-comparison.json

Exit codes: 0 = compatible comparison produced, 2 = rejected or unreadable input.
"""

import argparse
import datetime
import json
import math
import sys
from pathlib import Path

COMPARISON_VERSION = "mio-eval-comparison-v1"

# Counters carried in every report's usage block. cost_usd is never copied.
COMPARED_COUNTERS = (
    "input_tokens",
    "output_tokens",
    "total_tokens",
    "cached_input_tokens",
    "cache_creation_input_tokens",
)

# A direct comparison requires every one of these to be present and equal.
# `temperature` and `max_output_tokens` are the fixed sampling settings the PRD
# requires; a report that did not record them is unverifiable, not "no setting".
REQUIRED_EQUAL = (
    ("report_version", "report format version"),
    ("schema_version", "report schema version"),
    ("task_set_id", "task set id"),
    ("task_set_version", "task set version"),
    ("task_set_tasks", "task set task list"),
    ("provider", "provider"),
    ("budget_version", "budget profile version"),
    ("model", "model"),
    ("thinking", "thinking level"),
    ("temperature", "fixed sampling temperature"),
    ("max_output_tokens", "per-request output cap"),
    ("wall_timeout_seconds", "runner-enforced wall timeout"),
    ("diagnostics_enabled", "opt-in diagnostic collection mode"),
    ("diagnostics_constraints", "offline diagnostic constraint allowlist"),
    ("diagnostics_limits", "diagnostic collection bounds"),
)

# These may legitimately be null (no declared cap), so null == null is equal.
OPTIONAL_EQUAL = (
    ("max_model_calls", "declared max model calls"),
    ("max_total_tokens", "declared max total tokens"),
)

RECOVERY_PROBE_TASK = "resume-cache-build"

# Pi and Mio both report why a model call ended, but not on the same scale. Only
# these pairs describe the same event; everything else is agent-specific and is
# reported as unmapped instead of being forced into a shared bucket, which would
# invent a zero for the agent that cannot report it.
FINISH_REASON_ALIGNMENT = (
    {"pi": "stop", "mio": "stop", "meaning": "the model ended its turn normally"},
    {"pi": "length", "mio": "length", "meaning": "the response hit the output-token limit"},
    {"pi": "toolUse", "mio": "tool_calls", "meaning": "the model stopped to call tools"},
)
FINISH_REASON_PI_ONLY = ("error", "aborted", "deferred")
FINISH_REASON_MIO_ONLY = ("content_filter", "other", "unreported")

LIMITS = (
    "Compatible comparison of two self-acceptance probes. Both agent commands were "
    "configured with the same provider, model, thinking level, fixed sampling temperature, "
    "per-request output cap, task set version and runner-enforced wall-clock timeout, with "
    "matching declared model-call and token limits. "
    "The runner enforces only the per-task wall timeout; the budget block is declared "
    "metadata and no token hard budget is claimed. Token and cache numbers are the "
    "Mio provider observations or Pi's normalized event counters, remapped to a shared vocabulary, and "
    "stay null whenever a report did not fully observe them. This is not a ranking and "
    "does not establish quality leadership. Pi may synthesize zero counters when a provider omits usage; its zeros cannot always establish zero billed usage. "
    "Model finish reasons are kept in each agent's own vocabulary and only the listed "
    "pairs are treated as the same event; the aborted-cancellation counter is not "
    "cross-comparable and no delta is computed for it."
)


def read_report(path):
    """Load one report JSON object, failing loudly on malformed input."""
    path = Path(path)
    try:
        report = json.loads(path.read_text(encoding="utf-8"))
    except OSError as error:
        raise ValueError(f"{path}: cannot read report: {error}") from error
    except json.JSONDecodeError as error:
        raise ValueError(f"{path}: invalid JSON: {error}") from error
    if not isinstance(report, dict):
        raise ValueError(f"{path}: report must be a JSON object")
    return report


def diagnostics_profile(report):
    """Old reports predate diagnostics; malformed new declarations are unknown."""
    if "diagnostics" not in report:
        return False, [], {}
    block = report["diagnostics"]
    if not isinstance(block, dict) or not isinstance(block.get("enabled"), bool):
        return None, None, None
    if not block["enabled"]:
        return False, [], {}
    constraints = block.get("constraints")
    if not isinstance(constraints, list) or not all(isinstance(item, str) for item in constraints):
        constraints = None
    else:
        constraints = sorted(set(constraints))
    limits = block.get("limits")
    if not isinstance(limits, dict) or not limits:
        limits = None
    return True, constraints, limits


def profile_of(report):
    """The subset of a report that decides whether two runs are comparable."""
    task_set = report.get("task_set") if isinstance(report.get("task_set"), dict) else {}
    budget = report.get("budget") if isinstance(report.get("budget"), dict) else {}
    enforced = report.get("enforced") if isinstance(report.get("enforced"), dict) else {}
    diagnostics_enabled, diagnostics_constraints, diagnostics_limits = diagnostics_profile(report)
    return {
        "diagnostics_enabled": diagnostics_enabled,
        "diagnostics_constraints": diagnostics_constraints,
        "diagnostics_limits": diagnostics_limits,
        "report_version": report.get("report_version"),
        "schema_version": report.get("schema_version"),
        "task_set_id": task_set.get("id"),
        "task_set_version": task_set.get("version"),
        "task_set_tasks": task_set.get("tasks"),
        "provider": budget.get("provider"),
        "budget_version": budget.get("version"),
        "model": budget.get("model"),
        "thinking": budget.get("thinking"),
        "temperature": budget.get("temperature"),
        "max_output_tokens": budget.get("max_output_tokens"),
        "max_model_calls": budget.get("max_model_calls"),
        "max_total_tokens": budget.get("max_total_tokens"),
        "wall_timeout_seconds": enforced.get("wall_timeout_seconds"),
        "budget_enforced_by_runner": budget.get("enforced_by_runner"),
        "enforced_budget_metadata_only": enforced.get("budget_enforced_by_runner"),
    }


def check_compatibility(baseline, candidate):
    """Return (checks, mismatches); mismatches is non-empty when uncomparable."""
    left, right = profile_of(baseline), profile_of(candidate)
    checks = []
    mismatches = []
    for key, description in REQUIRED_EQUAL:
        entry = {"field": key, "description": description,
                 "baseline": left[key], "candidate": right[key]}
        if left[key] is None or right[key] is None:
            entry["result"] = "unknown"
            mismatches.append({**entry, "reason": "value not recorded, cannot verify equality"})
        elif left[key] == right[key]:
            entry["result"] = "equal"
        else:
            entry["result"] = "different"
            mismatches.append({**entry, "reason": "values differ"})
        checks.append(entry)
    for key, description in OPTIONAL_EQUAL:
        # None means "no declared cap" on both sides; that is a valid, equal profile.
        entry = {"field": key, "description": description,
                 "baseline": left[key], "candidate": right[key]}
        present = all(isinstance(report.get("budget"), dict) and key in report["budget"] for report in (baseline, candidate))
        if not present:
            entry["result"] = "unknown"
            mismatches.append({**entry, "reason": "budget field not recorded"})
        elif left[key] == right[key]:
            entry["result"] = "equal"
        else:
            entry["result"] = "different"
            mismatches.append({**entry, "reason": "values differ"})
        checks.append(entry)
    for key in ("budget_enforced_by_runner", "enforced_budget_metadata_only"):
        entry = {"field": key, "baseline": left[key], "candidate": right[key]}
        if left[key] is False and right[key] is False:
            entry["result"] = "equal"
            entry["description"] = "budget declared as metadata only"
        else:
            entry["result"] = "different"
            entry["description"] = "budget enforcement claim"
            mismatches.append({**entry, "reason": "report claims an enforced budget"})
        checks.append(entry)
    return checks, mismatches


def _is_number(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value) and value >= 0


def _usage_counter(usage, name):
    value = usage.get(name) if isinstance(usage, dict) else None
    return value if isinstance(value, int) and not isinstance(value, bool) and value >= 0 else None


def summarise_agent(report):
    """Neutral per-agent metrics; every unobserved value stays null."""
    results = report.get("results") if isinstance(report.get("results"), list) else []
    total = len(results)
    statuses = [result.get("status") for result in results if isinstance(result, dict)]
    statuses_known = len(statuses) == total and all(isinstance(status, str) for status in statuses)

    passed = sum(1 for status in statuses if status == "passed")
    success_rate = round(passed / total, 6) if total and statuses_known else None

    acceptance_passes = 0
    acceptance_known = True
    for result in results:
        acceptance = result.get("acceptance") if isinstance(result, dict) else None
        if not isinstance(acceptance, dict) or not isinstance(acceptance.get("exit_code"), int) \
                or isinstance(acceptance.get("exit_code"), bool):
            acceptance_known = False
            continue
        if acceptance["exit_code"] == 0 and not acceptance.get("timed_out"):
            acceptance_passes += 1
    correctness_rate = round(acceptance_passes / total, 6) if total and acceptance_known else None

    durations = [result.get("duration_seconds") for result in results if isinstance(result, dict)]
    durations_known = len(durations) == total and all(_is_number(value) for value in durations)
    latency_total = round(sum(durations), 3) if total and durations_known else None
    if not total:
        latency_coverage = "unknown"
    elif durations_known:
        latency_coverage = "complete"
    else:
        latency_coverage = "partial"

    usage = report.get("usage") if isinstance(report.get("usage"), dict) else {}
    usage_summary = {name: _usage_counter(usage, name) for name in COMPARED_COUNTERS}
    usage_summary.update({
        "model_calls": _usage_counter(usage, "model_calls"),
        "source": usage.get("source", "unknown"),
        "coverage": usage.get("coverage") if isinstance(usage.get("coverage"), str) else "unknown",
        "cost_usd": None,
        "note": (
            "Copied from the report's cross-task aggregate. Null means the report did not "
            "fully observe that counter; it is never displayed as 0. Cost is never inferred. "
            "Pi counters are agent-normalized and may contain synthetic zeros when upstream usage is missing."
        ),
    })

    timeout_flags = [
        result.get("agent", {}).get("timed_out") if isinstance(result.get("agent"), dict) else None
        for result in results
    ]
    timeouts_known = len(timeout_flags) == total and all(isinstance(flag, bool) for flag in timeout_flags)
    wall_timeouts = sum(1 for flag in timeout_flags if flag) if total and timeouts_known else None
    timeout_statuses = (
        sum(1 for status in statuses if status == "timeout") if statuses_known else None
    )
    stop_reasons = usage.get("stop_reasons") if isinstance(usage.get("stop_reasons"), dict) else None
    finish_reasons = (
        usage.get("finish_reasons") if isinstance(usage.get("finish_reasons"), dict) else None
    )
    if stop_reasons is not None:
        native_key, native_reasons = "stop_reasons", stop_reasons
    elif finish_reasons is not None:
        native_key, native_reasons = "finish_reasons", finish_reasons
    else:
        native_key, native_reasons = None, None
    model_aborts = stop_reasons.get("aborted") if stop_reasons is not None else None
    cancel = {
        "wall_timeout_tasks": wall_timeouts,
        "timeout_status_tasks": timeout_statuses,
        "aborted_model_calls": model_aborts if _is_number(model_aborts) else None,
        "native_finish_reasons": native_reasons,
        "native_finish_reason_key": native_key,
        "coverage": "observed" if wall_timeouts is not None else "unknown",
        "note": (
            "Wall-clock cancellations enforced by the runner process-group kill. "
            "aborted_model_calls is an agent-native counter: Pi reports it from its "
            "'aborted' stop reason, while Mio's completion_call finish reason has no "
            "aborted or cancelled variant, so a null there means the agent cannot "
            "report it rather than that no cancellation happened. The full native "
            "counters are kept under native_finish_reasons and are never converted "
            "into the other agent's scale."
        ),
    }

    probe = next(
        (result for result in results
         if isinstance(result, dict) and result.get("task") == RECOVERY_PROBE_TASK),
        None,
    )
    recovery_probe = None
    if probe is not None:
        acceptance = probe.get("acceptance") if isinstance(probe.get("acceptance"), dict) else {}
        recovery_probe = {
            "task": RECOVERY_PROBE_TASK,
            "status": probe.get("status"),
            "acceptance_exit_code": acceptance.get("exit_code"),
            "note": "Task-level acceptance on the recovery probe, not a general recovery metric.",
        }
    recovery = {
        "status": "unknown",
        "reason": (
            "Neither report records recovery observations (restart, reconnect or crash "
            "recovery). The recovery task's acceptance result is the only available signal "
            "and is reported separately as a probe."
        ),
        "probe": recovery_probe,
    }

    return {
        "label": report.get("label"),
        "report_version": report.get("report_version"),
        "event_format": report.get("event_format"),
        "success": {"passed": passed if statuses_known else None, "total": total,
                    "rate": success_rate},
        "correctness": {"acceptance_passed": acceptance_passes if acceptance_known else None,
                        "total": total, "rate": correctness_rate},
        "latency": {
            "total_seconds": latency_total,
            "coverage": latency_coverage,
            "per_task": {
                result.get("task"): result.get("duration_seconds")
                for result in results if isinstance(result, dict)
            },
            "note": "Sum of observed per-task durations; null unless every task reported one.",
        },
        "usage": usage_summary,
        "cancel": cancel,
        "recovery": recovery,
    }


def delta(baseline, candidate):
    if _is_number(baseline) and _is_number(candidate):
        return round(candidate - baseline, 6)
    return None


def build_deltas(baseline, candidate):
    """Signed candidate-minus-baseline differences; null when either side is unknown."""
    return {
        "success_rate": delta(baseline["success"]["rate"], candidate["success"]["rate"]),
        "correctness_rate": delta(
            baseline["correctness"]["rate"], candidate["correctness"]["rate"]
        ),
        "latency_total_seconds": delta(
            baseline["latency"]["total_seconds"], candidate["latency"]["total_seconds"]
        ),
        "input_tokens": delta(
            baseline["usage"]["input_tokens"], candidate["usage"]["input_tokens"]
        ),
        "output_tokens": delta(
            baseline["usage"]["output_tokens"], candidate["usage"]["output_tokens"]
        ),
        "total_tokens": delta(
            baseline["usage"]["total_tokens"], candidate["usage"]["total_tokens"]
        ),
        "cached_input_tokens": delta(
            baseline["usage"]["cached_input_tokens"], candidate["usage"]["cached_input_tokens"]
        ),
        "cache_creation_input_tokens": delta(
            baseline["usage"]["cache_creation_input_tokens"],
            candidate["usage"]["cache_creation_input_tokens"],
        ),
        "model_calls": delta(
            baseline["usage"]["model_calls"], candidate["usage"]["model_calls"]
        ),
        "wall_timeout_tasks": delta(
            baseline["cancel"]["wall_timeout_tasks"], candidate["cancel"]["wall_timeout_tasks"]
        ),
        "note": (
            "Signed candidate-minus-baseline. Null means at least one side is unknown, not "
            "zero. A positive or negative value is not a quality judgment."
        ),
    }


def build_comparison(baseline, candidate, baseline_label=None, candidate_label=None):
    checks, mismatches = check_compatibility(baseline, candidate)
    baseline_label = baseline_label or baseline.get("label") or "baseline"
    candidate_label = candidate_label or candidate.get("label") or "candidate"
    if baseline_label == candidate_label:
        baseline_label = f"{baseline_label} (baseline)"
        candidate_label = f"{candidate_label} (candidate)"

    baseline_summary = summarise_agent(baseline)
    candidate_summary = summarise_agent(candidate)
    comparison = {
        "comparison_version": COMPARISON_VERSION,
        "generated_at": datetime.datetime.now(datetime.timezone.utc)
        .isoformat(timespec="seconds")
        .replace("+00:00", "Z"),
        "compatible": not mismatches,
        "compatibility": {
            "checks": checks,
            "mismatches": mismatches,
            "note": (
                "A direct comparison is produced only when every required field is present "
                "and equal. Task set id/version/tasks, provider, model, thinking level, fixed "
                "sampling temperature, per-request output cap, declared "
                "max_model_calls/max_total_tokens and the runner-enforced wall timeout must "
                "match, as must diagnostic mode, bounds and constraint allowlists. Reports "
                "predating diagnostics are treated as disabled; malformed declarations "
                "are unverifiable."
            ),
        },
        "profile": {
            "diagnostics_enabled": profile_of(baseline)["diagnostics_enabled"],
            "diagnostics_constraints": profile_of(baseline)["diagnostics_constraints"],
            "diagnostics_limits": profile_of(baseline)["diagnostics_limits"],
            "task_set": (baseline.get("task_set") or {}).get("id"),
            "task_set_version": (baseline.get("task_set") or {}).get("version"),
            "provider": (baseline.get("budget") or {}).get("provider"),
            "model": (baseline.get("budget") or {}).get("model"),
            "thinking": (baseline.get("budget") or {}).get("thinking"),
            "temperature": (baseline.get("budget") or {}).get("temperature"),
            "max_output_tokens": (baseline.get("budget") or {}).get("max_output_tokens"),
            "max_model_calls": (baseline.get("budget") or {}).get("max_model_calls"),
            "max_total_tokens": (baseline.get("budget") or {}).get("max_total_tokens"),
            "wall_timeout_seconds": (baseline.get("enforced") or {}).get("wall_timeout_seconds"),
            "token_hard_budget": False,
            "model_call_hard_budget": False,
            "note": (
                "Only the wall timeout is runner-enforced. No model-call or token cap was "
                "declared on either side, so no hard budget is claimed."
            ),
        },
        "agents": {
            baseline_label: baseline_summary,
            candidate_label: candidate_summary,
        },
        "comparability": {
            "wall_timeouts": {
                "comparable": True,
                "reason": (
                    "Both sides are measured by the runner's own per-task wall-clock "
                    "timeout and process-group kill, independent of agent output."
                ),
            },
            "aborted_model_calls": {
                "comparable": False,
                "reason": (
                    "Pi counts a caller-cancelled model read as stopReason 'aborted'. "
                    "Mio's completion_call `finishReason` only carries mio-core's "
                    "normalized FinishReason, which has no aborted or cancelled "
                    "variant; a cancelled read is recorded on the message as "
                    "StopReason::Aborted, not as a completion-call finish reason. "
                    "Mio's counter is therefore absent rather than zero, the two "
                    "numbers do not describe the same event, and no delta is "
                    "computed for it."
                ),
            },
            "finish_reason_vocabularies": {
                "reliable_pairs": [dict(pair) for pair in FINISH_REASON_ALIGNMENT],
                "pi_only": list(FINISH_REASON_PI_ONLY),
                "mio_only": list(FINISH_REASON_MIO_ONLY),
                "note": (
                    "Each agent's native counters stay under its own key "
                    "(stop_reasons for Pi, finish_reasons for Mio) and are never "
                    "converted into each other. Only the listed pairs mean the same "
                    "event; everything else is reported as agent-specific and "
                    "unmapped."
                ),
            },
        },
        "deltas": build_deltas(baseline_summary, candidate_summary),
        "claims": {
            "leadership": False,
            "ranking": False,
            "cross_vendor_comparison": False,
            "statistical_significance": False,
            "note": (
                "This report only places two same-profile self-acceptance probes side by "
                "side. It does not name a winner, rank the agents, compare different "
                "providers, or claim statistical significance."
            ),
        },
        "limits": LIMITS,
    }
    return comparison


def build_parser():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--baseline", type=Path, required=True, help="baseline report JSON")
    parser.add_argument("--candidate", type=Path, required=True, help="candidate report JSON")
    parser.add_argument("--baseline-label", help="override the baseline agent label")
    parser.add_argument("--candidate-label", help="override the candidate agent label")
    parser.add_argument("--output", type=Path, help="write the comparison JSON here as well")
    return parser


def main(argv=None):
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        baseline = read_report(args.baseline)
        candidate = read_report(args.candidate)
    except ValueError as error:
        print(json.dumps({"comparison_version": COMPARISON_VERSION, "error": str(error)}))
        return 2

    comparison = build_comparison(
        baseline, candidate, args.baseline_label, args.candidate_label
    )
    rendered = json.dumps(comparison, indent=2) + "\n"
    if args.output is not None:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered)
    sys.stdout.write(rendered)
    return 0 if comparison["compatible"] else 2


if __name__ == "__main__":
    raise SystemExit(main())
