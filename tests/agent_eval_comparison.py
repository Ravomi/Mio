"""Offline tests of the cross-agent comparison gate; no agent or model is called."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts" / "compare_agent_eval.py"
spec = importlib.util.spec_from_file_location("compare_agent_eval", SCRIPT)
comparison = importlib.util.module_from_spec(spec)
spec.loader.exec_module(comparison)

TASKS = ["migrate-line-item-api", "resume-cache-build", "retrieve-wire-spec"]


def result(task, status="passed", duration=10.0, agent_timed_out=False, acceptance_exit=0):
    return {
        "task": task,
        "status": status,
        "failure_class": None if status == "passed" else "acceptance_failed",
        "accepted": status == "passed",
        "duration_seconds": duration,
        "agent": {"exit_code": 0, "timed_out": agent_timed_out, "seconds": duration - 0.5},
        "acceptance": {"exit_code": acceptance_exit, "timed_out": False, "seconds": 0.5},
    }


def make_report(label, *, task_set_version=2, model="deepseek-chat", thinking="off",
                temperature=0, max_output_tokens=8192,
                wall_timeout=180, max_model_calls=None, max_total_tokens=None,
                results=None, usage=None, budget_enforced=False, enforced_metadata=False,
                event_format="pi-jsonl"):
    return {
        "schema_version": 2,
        "report_version": "mio-eval-report-v1",
        "report_type": "self-acceptance-probe",
        "label": label,
        "task_set": {"id": "mio-workflows-v1", "version": task_set_version, "tasks": list(TASKS)},
        "budget": {
            "version": "deepseek-chat-off-180s-nocap-workflows-v1",
            "provider": "deepseek",
            "model": model,
            "thinking": thinking,
            "temperature": temperature,
            "max_output_tokens": max_output_tokens,
            "max_model_calls": max_model_calls,
            "max_total_tokens": max_total_tokens,
            "enforced_by_runner": budget_enforced,
        },
        "enforced": {
            "wall_timeout_seconds": wall_timeout,
            "budget_enforced_by_runner": enforced_metadata,
            "note": "wall-clock only",
        },
        "event_format": event_format,
        "results": results if results is not None else [
            result("migrate-line-item-api", "passed", 10.0),
            result("resume-cache-build", "passed", 20.0),
            result("retrieve-wire-spec", "failed", 30.0, acceptance_exit=1),
        ],
        "usage": usage if usage is not None else {
            "input_tokens": 1000, "output_tokens": 100, "total_tokens": 1100,
            "cached_input_tokens": 400, "cache_creation_input_tokens": 0,
            "tool_use_prompt_tokens": None, "reasoning_tokens": 5,
            "model_calls": 6, "coverage": "complete", "cost_usd": None,
            "stop_reasons": {"stop": 5, "toolUse": 1, "length": 0, "error": 0,
                             "aborted": 1, "deferred": 0},
        },
    }


def mio_usage(**overrides):
    """A Mio report's usage block: finish_reasons instead of Pi's stop_reasons."""
    usage = {
        "input_tokens": 1000, "output_tokens": 100, "total_tokens": 1100,
        "cached_input_tokens": 400, "cache_creation_input_tokens": 0,
        "tool_use_prompt_tokens": None, "reasoning_tokens": 5,
        "model_calls": 6, "coverage": "complete", "cost_usd": None,
        "finish_reasons": {"stop": 4, "length": 1, "tool_calls": 1,
                           "content_filter": 0, "other": 0, "unreported": 0},
    }
    usage.update(overrides)
    return usage


class CompatibilityTests(unittest.TestCase):
    def assert_rejected(self, baseline, candidate, field=None):
        report = comparison.build_comparison(baseline, candidate)
        self.assertFalse(report["compatible"])
        mismatched = {entry["field"] for entry in report["compatibility"]["mismatches"]}
        if field is not None:
            self.assertIn(field, mismatched)

    def test_diagnostic_mode_must_match(self):
        baseline, candidate = make_report("mio"), make_report("pi")
        candidate["diagnostics"] = {"enabled": True, "constraints": [], "limits": {"max_events": 2048}}
        self.assert_rejected(baseline, candidate, "diagnostics_enabled")

    def test_legacy_reports_match_explicitly_disabled_diagnostics(self):
        baseline, candidate = make_report("mio"), make_report("pi")
        candidate["diagnostics"] = {"enabled": False, "constraints": [], "limits": None}
        self.assertTrue(comparison.build_comparison(baseline, candidate)["compatible"])

    def test_enabled_diagnostics_require_equal_bounds_and_allowlists(self):
        baseline, candidate = make_report("mio"), make_report("pi")
        baseline["diagnostics"] = {"enabled": True, "constraints": ["rule-a", "rule-b"], "limits": {"max_events": 2048}}
        candidate["diagnostics"] = {"enabled": True, "constraints": ["rule-b", "rule-a"], "limits": {"max_events": 2048}}
        self.assertTrue(comparison.build_comparison(baseline, candidate)["compatible"])
        candidate["diagnostics"]["limits"] = {"max_events": 1024}
        self.assert_rejected(baseline, candidate, "diagnostics_limits")
        candidate["diagnostics"]["limits"] = {"max_events": 2048}
        candidate["diagnostics"]["constraints"] = ["rule-a"]
        self.assert_rejected(baseline, candidate, "diagnostics_constraints")

    def test_invalid_diagnostic_declarations_cannot_fake_matching_modes(self):
        for declaration in (None, {}, {"enabled": 0}, {"enabled": "false"}, {"enabled": True}):
            with self.subTest(declaration=declaration):
                baseline, candidate = make_report("mio"), make_report("pi")
                baseline["diagnostics"] = declaration
                candidate["diagnostics"] = declaration
                checks, mismatches = comparison.check_compatibility(baseline, candidate)
                self.assertTrue(mismatches, checks)

    def test_compatible_reports_compare(self):
        baseline = make_report("mio")
        candidate = make_report("pi", usage={**make_report("x")["usage"],
                                             "input_tokens": 900, "total_tokens": 1000})
        report = comparison.build_comparison(baseline, candidate)
        self.assertTrue(report["compatible"])
        self.assertEqual(report["compatibility"]["mismatches"], [])
        self.assertEqual(report["comparison_version"], "mio-eval-comparison-v1")
        self.assertEqual(report["profile"]["provider"], "deepseek")
        self.assertEqual(report["profile"]["temperature"], 0)
        self.assertEqual(report["profile"]["max_output_tokens"], 8192)
        agents = list(report["agents"].values())
        self.assertEqual(agents[0]["success"]["rate"], round(2 / 3, 6))
        self.assertEqual(report["deltas"]["input_tokens"], -100)

    def test_differing_task_set_version_rejected(self):
        self.assert_rejected(make_report("mio"), make_report("pi", task_set_version=3),
                             "task_set_version")

    def test_differing_model_rejected(self):
        self.assert_rejected(make_report("mio"), make_report("pi", model="deepseek-reasoner"),
                             "model")

    def test_differing_thinking_rejected(self):
        self.assert_rejected(make_report("mio"), make_report("pi", thinking="high"), "thinking")

    def test_differing_temperature_rejected(self):
        self.assert_rejected(make_report("mio"), make_report("pi", temperature=1.0),
                             "temperature")

    def test_differing_output_cap_rejected(self):
        self.assert_rejected(make_report("mio"), make_report("pi", max_output_tokens=4096),
                             "max_output_tokens")

    def test_unrecorded_sampling_is_rejected_as_unverifiable(self):
        for key in ("temperature", "max_output_tokens"):
            with self.subTest(field=key):
                self.assert_rejected(make_report("mio"), make_report("pi", **{key: None}), key)
                left, right = make_report("mio"), make_report("pi")
                del right["budget"][key]
                _, mismatches = comparison.check_compatibility(left, right)
                self.assertTrue(mismatches)

    def test_differing_wall_timeout_rejected(self):
        self.assert_rejected(make_report("mio"), make_report("pi", wall_timeout=240),
                             "wall_timeout_seconds")

    def test_differing_model_call_cap_rejected(self):
        self.assert_rejected(make_report("mio"), make_report("pi", max_model_calls=20),
                             "max_model_calls")

    def test_differing_token_cap_rejected(self):
        self.assert_rejected(make_report("mio"), make_report("pi", max_total_tokens=100000),
                             "max_total_tokens")

    def test_missing_values_are_rejected_as_unverifiable(self):
        self.assert_rejected(make_report("mio", wall_timeout=None), make_report("pi"),
                             "wall_timeout_seconds")
        self.assert_rejected(make_report("mio", model=None), make_report("pi"), "model")
        self.assert_rejected(make_report("mio", thinking=None), make_report("pi"), "thinking")

    def test_enforced_budget_claim_rejected(self):
        self.assert_rejected(make_report("mio"), make_report("pi", budget_enforced=True),
                             "budget_enforced_by_runner")
        self.assert_rejected(make_report("mio"), make_report("pi", enforced_metadata=True),
                             "enforced_budget_metadata_only")

    def test_null_caps_on_both_sides_are_compatible(self):
        report = comparison.build_comparison(
            make_report("mio", max_model_calls=None), make_report("pi", max_model_calls=None)
        )
        self.assertTrue(report["compatible"])


class MetricTests(unittest.TestCase):
    def test_unknown_usage_counters_stay_null(self):
        unknown = {"input_tokens": None, "output_tokens": None, "total_tokens": None,
                   "cached_input_tokens": None, "cache_creation_input_tokens": None,
                   "model_calls": None, "coverage": "unknown", "cost_usd": None}
        report = comparison.build_comparison(
            make_report("mio", usage=unknown), make_report("pi")
        )
        self.assertTrue(report["compatible"])
        mio = report["agents"]["mio"]
        self.assertIsNone(mio["usage"]["input_tokens"])
        self.assertIsNone(report["deltas"]["input_tokens"])
        self.assertIsNone(report["deltas"]["total_tokens"])

    def test_cost_is_never_copied(self):
        usage = {**make_report("x")["usage"], "cost_usd": 12.34}
        report = comparison.build_comparison(make_report("mio", usage=usage), make_report("pi"))
        self.assertIsNone(report["agents"]["mio"]["usage"]["cost_usd"])

    def test_correctness_uses_acceptance_not_agent_exit(self):
        baseline_results = [
            result("migrate-line-item-api", "failed", 10.0, acceptance_exit=0),
            result("resume-cache-build", "passed", 10.0),
            result("retrieve-wire-spec", "passed", 10.0),
        ]
        report = comparison.build_comparison(
            make_report("mio", results=baseline_results), make_report("pi", results=baseline_results)
        )
        mio = report["agents"]["mio"]
        # One agent exited non-zero, but its files still passed acceptance.
        self.assertEqual(mio["success"]["rate"], round(2 / 3, 6))
        self.assertEqual(mio["correctness"]["rate"], 1.0)

    def test_latency_unknown_when_any_task_is_missing(self):
        results = [result(task, "passed", 10.0) for task in TASKS]
        results.append({"task": "broken", "status": "passed", "duration_seconds": None,
                        "agent": {"timed_out": None}, "acceptance": {}})
        report = comparison.build_comparison(
            make_report("mio", results=results), make_report("pi")
        )
        self.assertIsNone(report["agents"]["mio"]["latency"]["total_seconds"])
        self.assertEqual(report["agents"]["mio"]["latency"]["coverage"], "partial")

    def test_cancel_reports_wall_timeouts_and_model_aborts(self):
        results = [
            result("migrate-line-item-api", "timeout", 180.0, agent_timed_out=True,
                   acceptance_exit=1),
            result("resume-cache-build", "passed", 10.0),
            result("retrieve-wire-spec", "passed", 10.0),
        ]
        report = comparison.build_comparison(
            make_report("mio", results=results), make_report("pi")
        )
        cancel = report["agents"]["mio"]["cancel"]
        self.assertEqual(cancel["wall_timeout_tasks"], 1)
        self.assertEqual(cancel["timeout_status_tasks"], 1)
        self.assertEqual(cancel["aborted_model_calls"], 1)

    def test_cancel_unknown_without_stop_reasons(self):
        usage = {**make_report("x")["usage"]}
        usage.pop("stop_reasons")
        report = comparison.build_comparison(make_report("mio", usage=usage), make_report("pi"))
        self.assertIsNone(report["agents"]["mio"]["cancel"]["aborted_model_calls"])

    def test_recovery_headline_stays_unknown_with_probe(self):
        report = comparison.build_comparison(make_report("mio"), make_report("pi"))
        recovery = report["agents"]["mio"]["recovery"]
        self.assertEqual(recovery["status"], "unknown")
        self.assertIn("recovery", recovery["reason"])
        self.assertEqual(recovery["probe"]["task"], "resume-cache-build")
        self.assertEqual(recovery["probe"]["status"], "passed")

    def test_aborted_cancellation_is_reported_as_incomparable(self):
        report = comparison.build_comparison(make_report("mio"), make_report("pi"))
        comparability = report["comparability"]
        self.assertFalse(comparability["aborted_model_calls"]["comparable"])
        reason = comparability["aborted_model_calls"]["reason"]
        self.assertIn("aborted", reason)
        self.assertIn("Mio", reason)
        # The runner-measured wall timeout stays comparable.
        self.assertTrue(comparability["wall_timeouts"]["comparable"])
        # No delta is invented for the incomparable counter.
        self.assertNotIn("aborted_model_calls", report["deltas"])

    def test_native_finish_reasons_are_never_converted(self):
        report = comparison.build_comparison(
            make_report("mio", event_format="mio-jsonl", usage=mio_usage()),
            make_report("pi"),
        )
        self.assertTrue(report["compatible"])
        mio_cancel = report["agents"]["mio"]["cancel"]
        pi_cancel = report["agents"]["pi"]["cancel"]
        self.assertEqual(mio_cancel["native_finish_reason_key"], "finish_reasons")
        self.assertEqual(pi_cancel["native_finish_reason_key"], "stop_reasons")
        self.assertEqual(mio_cancel["native_finish_reasons"]["tool_calls"], 1)
        self.assertNotIn("aborted", mio_cancel["native_finish_reasons"])
        # Mio cannot report an aborted read: the counter is null, never zero.
        self.assertIsNone(mio_cancel["aborted_model_calls"])
        self.assertEqual(pi_cancel["aborted_model_calls"], 1)
        pairs = report["comparability"]["finish_reason_vocabularies"]["reliable_pairs"]
        self.assertEqual(
            {(pair["pi"], pair["mio"]) for pair in pairs},
            {("stop", "stop"), ("length", "length"), ("toolUse", "tool_calls")},
        )
        vocabulary = report["comparability"]["finish_reason_vocabularies"]
        self.assertIn("aborted", vocabulary["pi_only"])
        self.assertIn("content_filter", vocabulary["mio_only"])

    def test_cancel_without_a_vocabulary_stays_null(self):
        report = comparison.build_comparison(
            make_report("mio", event_format="mio-jsonl", usage=mio_usage(finish_reasons=None)),
            make_report("pi"),
        )
        cancel = report["agents"]["mio"]["cancel"]
        self.assertIsNone(cancel["native_finish_reasons"])
        self.assertIsNone(cancel["native_finish_reason_key"])
        self.assertIsNone(cancel["aborted_model_calls"])

    def test_claims_never_declare_leadership_or_ranking(self):
        report = comparison.build_comparison(make_report("mio"), make_report("pi"))
        claims = report["claims"]
        self.assertFalse(claims["leadership"])
        self.assertFalse(claims["ranking"])
        self.assertFalse(claims["cross_vendor_comparison"])
        self.assertFalse(claims["statistical_significance"])
        self.assertFalse(report["profile"]["token_hard_budget"])
        self.assertFalse(report["profile"]["model_call_hard_budget"])
        # No structured winner/order field, whatever the prose disclaimer says.
        for forbidden in ("winner", "better_agent", "ranking_order", "leader"):
            self.assertNotIn(forbidden, report)


class MainTests(unittest.TestCase):
    def write(self, directory, name, report):
        path = Path(directory) / name
        path.write_text(json.dumps(report))
        return path

    def run_main(self, argv):
        stream = io.StringIO()
        with contextlib.redirect_stdout(stream):
            code = comparison.main(argv)
        return code, stream.getvalue()

    def test_main_returns_zero_and_writes_output_for_compatible_reports(self):
        with tempfile.TemporaryDirectory() as temporary:
            baseline = self.write(temporary, "mio.json", make_report("mio"))
            candidate = self.write(temporary, "pi.json", make_report("pi"))
            output = Path(temporary) / "comparison.json"
            code, stdout = self.run_main([
                "--baseline", str(baseline), "--candidate", str(candidate),
                "--output", str(output),
            ])
            self.assertEqual(code, 0)
            self.assertTrue(output.is_file())
            self.assertEqual(json.loads(output.read_text())["compatible"], True)
            self.assertEqual(json.loads(stdout)["comparison_version"], "mio-eval-comparison-v1")

    def test_main_rejects_incompatible_reports(self):
        with tempfile.TemporaryDirectory() as temporary:
            baseline = self.write(temporary, "mio.json", make_report("mio"))
            candidate = self.write(temporary, "pi.json", make_report("pi", thinking="high"))
            code, stdout = self.run_main([
                "--baseline", str(baseline), "--candidate", str(candidate),
            ])
            self.assertEqual(code, 2)
            payload = json.loads(stdout)
            self.assertFalse(payload["compatible"])
            fields = {entry["field"] for entry in payload["compatibility"]["mismatches"]}
            self.assertIn("thinking", fields)

    def test_main_reports_unreadable_input(self):
        with tempfile.TemporaryDirectory() as temporary:
            baseline = self.write(temporary, "mio.json", make_report("mio"))
            missing = Path(temporary) / "missing.json"
            code, stdout = self.run_main([
                "--baseline", str(baseline), "--candidate", str(missing),
            ])
            self.assertEqual(code, 2)
            self.assertIn("error", json.loads(stdout))


class AdditionalProfileGuards(unittest.TestCase):
    def test_provider_and_budget_version_must_match(self):
        for key, value in (("provider", "other"), ("version", "other-profile")):
            left, right = make_report("mio"), make_report("pi")
            right["budget"][key] = value
            _, mismatches = comparison.check_compatibility(left, right)
            self.assertTrue(mismatches)

    def test_missing_budget_fields_are_not_an_explicit_unlimited_profile(self):
        for key in ("max_model_calls", "max_total_tokens"):
            left, right = make_report("mio"), make_report("pi")
            del right["budget"][key]
            _, mismatches = comparison.check_compatibility(left, right)
            self.assertTrue(mismatches)


if __name__ == "__main__":
    unittest.main()
