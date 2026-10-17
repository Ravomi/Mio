"""Offline tests for the bounded, sanitized eval diagnostics collector.

These tests never call a model, never touch a real log and only write inside a
temporary directory. They import ``scripts/eval_diagnostics.py`` directly, the
same way ``tests/agent_eval_harness.py`` imports the evaluation runner.
"""

import importlib.util
import json
import os
from pathlib import Path
import stat
import tempfile
import unittest

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts" / "eval_diagnostics.py"
spec = importlib.util.spec_from_file_location("eval_diagnostics", SCRIPT)
diagnostics = importlib.util.module_from_spec(spec)
spec.loader.exec_module(diagnostics)

CREDENTIAL = "sk-live-DEADBEEF0123456789abcdef"


def completion_call(run_id="run-1", call_index=0, finish_reason="stop"):
    return {
        "type": "completion_call",
        "data": {
            "runId": run_id,
            "callIndex": call_index,
            "finishReason": finish_reason,
            "usage": {"input_tokens": 10},
        },
    }


def run_observation(kind, run_id="run-1", effect_id=1, turn_id=1, **extra):
    """One realistic ``--json`` run_observation line.

    Mirrors the engine's camelCase ``RunObservation`` payload and the top-level
    correlation fields the v1 envelope adds. Lifecycle observations carry no
    effect id, so an explicit ``None`` omits the field instead of inventing one.
    """
    data = {"runId": run_id, "kind": kind}
    if turn_id is not None:
        data["turnId"] = turn_id
    if effect_id is not None:
        data["effectId"] = effect_id
    data.update(extra)
    return {
        "type": "run_observation",
        "data": data,
        "schemaVersion": 1,
        "runId": run_id,
        "turnId": turn_id,
        "toolCallId": None,
        "subagentId": None,
        "parentRunId": None,
    }


class DiagnosticsTestCase(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        self.workspace = self.root / "workspace"
        self.workspace.mkdir()
        self.log = self.root / "agent.log"

    def tearDown(self):
        self._tmp.cleanup()

    def write_log(self, events=(), raw_lines=()):
        lines = [json.dumps(event, ensure_ascii=False) for event in events]
        lines.extend(raw_lines)
        self.log.write_bytes(("\n".join(lines) + "\n").encode("utf-8"))

    def collect(self, **kwargs):
        return diagnostics.collect(self.log, self.workspace, **kwargs)

    def assert_clean(self, evidence, *forbidden):
        serialized = diagnostics.serialize(evidence).decode("utf-8")
        dumped = json.dumps(evidence, ensure_ascii=False)
        for value in forbidden:
            self.assertNotIn(value, serialized)
            self.assertNotIn(value, dumped)
        return serialized


class LimitsTest(DiagnosticsTestCase):
    def test_defaults_meet_the_required_floors(self):
        self.assertFalse(diagnostics.DEFAULT_ENABLED)
        self.assertGreaterEqual(diagnostics.DEFAULT_LIMITS.max_events, 256)
        self.assertGreaterEqual(diagnostics.DEFAULT_LIMITS.max_field_chars, 256)
        self.assertGreaterEqual(diagnostics.DEFAULT_LIMITS.max_output_bytes, 64 * 1024)
        self.assertGreaterEqual(diagnostics.DEFAULT_LIMITS.max_source_bytes, 16 * 1024 * 1024)
        self.assertGreaterEqual(diagnostics.DEFAULT_LIMITS.max_line_bytes, 1024 * 1024)

    def test_limits_reject_non_positive_values(self):
        for field in ("max_events", "max_field_chars", "max_output_bytes"):
            with self.assertRaises(ValueError):
                diagnostics.Limits(**{field: 0})
            with self.assertRaises(ValueError):
                diagnostics.Limits(**{field: True})


class ModelCallTest(DiagnosticsTestCase):
    def test_completion_calls_are_deduplicated_and_bucketed(self):
        self.write_log([
            completion_call(call_index=0, finish_reason="stop"),
            completion_call(call_index=0, finish_reason="stop"),
            completion_call(call_index=1, finish_reason={"other": "weird-provider-spelling"}),
            completion_call(call_index=2, finish_reason=None),
            completion_call(call_index=3, finish_reason="garbage"),
            {"type": "model_request", "data": {"runId": "run-1"}},
        ])
        evidence = self.collect()
        calls = evidence["model_calls"]
        self.assertEqual(calls["completed"], 4)
        self.assertEqual(calls["deduplicated"], 1)
        self.assertEqual(calls["requests"], 1)
        self.assertEqual(calls["finish_reasons"]["stop"], 1)
        self.assertEqual(calls["finish_reasons"]["other"], 1)
        self.assertEqual(calls["finish_reasons"]["unreported"], 1)
        self.assertEqual(calls["finish_reasons"]["unrecognized"], 1)
        self.assertNotIn("weird-provider-spelling", diagnostics.serialize(evidence).decode())

    def test_request_count_stays_unknown_without_a_request_event(self):
        self.write_log([completion_call()])
        evidence = self.collect()
        self.assertEqual(evidence["model_calls"]["completed"], 1)
        self.assertIsNone(evidence["model_calls"]["requests"])


class ModelDispatchTest(DiagnosticsTestCase):
    """The real run_observation model attempt channel.

    ``model_dispatched`` is the engine's post-admission dispatch boundary; it
    is not a wire request and never includes a prepared attempt that admission
    denied. Nothing here reads run ids, effect ids or any other free text back
    out of the evidence.
    """

    def test_real_model_dispatched_and_prepared_observations_are_counted(self):
        self.write_log([
            run_observation("model_prepared", effect_id=7, turn_id=1),
            run_observation("model_dispatched", effect_id=7, turn_id=1),
            run_observation("model_prepared", effect_id=8, turn_id=2),
            run_observation("model_dispatched", effect_id=8, turn_id=2),
            # Valid lifecycle/tool observations must be tolerated, not counted.
            run_observation("run_started", effect_id=None, turn_id=None, sessionId="session-1"),
            run_observation("tool_dispatched", effect_id=9, turn_id=1, toolCallId="call-1"),
            run_observation("run_finished", effect_id=None, turn_id=None, status="success"),
            # A future kind is ignored, and must not mark the family observed.
            run_observation("model_teapot", effect_id=10, turn_id=3),
        ])
        evidence = self.collect()
        calls = evidence["model_calls"]
        self.assertEqual(calls["prepared"], 2)
        self.assertEqual(calls["dispatched"], 2)
        self.assertEqual(calls["prepared_duplicates"], 0)
        self.assertEqual(calls["dispatched_duplicates"], 0)
        self.assertIsNone(calls["requests"])
        self.assertIn("not a wire/HTTP request", calls["dispatch_note"])
        self.assertEqual(evidence["source"]["events_recognized"], 8)

    def test_a_prepared_refusal_is_never_a_dispatch(self):
        self.write_log([
            run_observation("model_prepared", effect_id=7, turn_id=1),
            run_observation("model_prepared", effect_id=6, turn_id=1),
            completion_call(call_index=0),
        ])
        evidence = self.collect()
        calls = evidence["model_calls"]
        # A prepared observation was really seen, so a zero dispatch count is a
        # real, meaningful zero rather than an unknown.
        self.assertEqual(calls["prepared"], 2)
        self.assertIsNotNone(calls["dispatched"])
        self.assertEqual(calls["dispatched"], 0)

    def test_multiple_dispatches_deduplicate_per_run_and_effect(self):
        self.write_log([
            run_observation("model_dispatched", run_id="run-a", effect_id=1),
            run_observation("model_dispatched", run_id="run-a", effect_id=1),
            run_observation("model_dispatched", run_id="run-a", effect_id=2),
            # The same effect id in a different run is a different dispatch.
            run_observation("model_dispatched", run_id="run-b", effect_id=1),
        ])
        evidence = self.collect()
        calls = evidence["model_calls"]
        self.assertEqual(calls["dispatched"], 3)
        self.assertEqual(calls["dispatched_duplicates"], 1)
        self.assert_clean(evidence, "run-a", "run-b")

    def test_completion_only_leaves_dispatch_unknown(self):
        self.write_log([completion_call(call_index=0), completion_call(call_index=1)])
        evidence = self.collect()
        calls = evidence["model_calls"]
        self.assertEqual(calls["completed"], 2)
        self.assertIsNone(calls["prepared"])
        self.assertIsNone(calls["dispatched"])

    def test_legacy_model_request_is_never_summed_with_dispatch(self):
        self.write_log([
            {"type": "model_request", "data": {"runId": "run-1"}},
            run_observation("model_dispatched", effect_id=1),
            completion_call(call_index=0),
        ])
        evidence = self.collect()
        calls = evidence["model_calls"]
        self.assertEqual(calls["requests"], 1)
        self.assertEqual(calls["dispatched"], 1)
        # The prepared family was not observed, so it stays unknown even though
        # a legacy request event and a dispatch both exist.
        self.assertIsNone(calls["prepared"])
        self.assertIn("never summed", calls["request_note"])

    def test_request_only_leaves_dispatch_unknown(self):
        self.write_log([{"type": "model_request", "data": {"runId": "run-1"}}])
        evidence = self.collect()
        calls = evidence["model_calls"]
        self.assertEqual(calls["requests"], 1)
        self.assertIsNone(calls["prepared"])
        self.assertIsNone(calls["dispatched"])

    def test_source_truncation_leaves_dispatch_unknown(self):
        self.write_log([
            run_observation("model_dispatched", effect_id=index) for index in range(40)
        ])
        evidence = self.collect(limits=diagnostics.Limits(max_source_bytes=200))
        self.assertTrue(evidence["truncation"]["source_truncated"])
        calls = evidence["model_calls"]
        self.assertIsNone(calls["prepared"])
        self.assertIsNone(calls["dispatched"])

    def test_run_observation_secrets_and_ids_never_leak(self):
        self.write_log([
            run_observation(
                "model_dispatched",
                run_id=CREDENTIAL,
                effect_id=1,
                sessionId=CREDENTIAL,
                subagentId=CREDENTIAL,
                parentRunId=CREDENTIAL,
                toolCallId=CREDENTIAL,
                note=CREDENTIAL,
            ),
        ])
        evidence = self.collect()
        self.assertEqual(evidence["model_calls"]["dispatched"], 1)
        self.assert_clean(evidence, CREDENTIAL, "Authorization")


class ReadTest(DiagnosticsTestCase):
    """Structured read observations win stream-wide; legacy attempts degrade.

    One successful ``read`` may be visible twice in the same log: the engine
    emits a structured ``read_observation`` and also a legacy ``tool_started``
    record for the same call. These tests pin the family preference, its
    independence from event order, and the fixed source/semantics vocabulary.
    """

    @staticmethod
    def tool_read(target):
        return {"type": "tool_started", "data": {"operation": "read", "target": target}}

    @staticmethod
    def observed(path, **extra):
        data = {"path": path}
        data.update(extra)
        return {"type": "read_observation", "data": data}

    @classmethod
    def by_key(cls, reads):
        return {
            (entry["path"], entry["range"]["kind"], entry["range"]["start"], entry["range"]["end"]): entry
            for entry in reads["files"]
        }

    def test_read_paths_and_explicit_ranges(self):
        # Superseded name kept: the stream now prefers structured observations,
        # so the same path/range coverage is asserted from that family and the
        # legacy candidates are pinned as suppressed.
        self.write_log([
            self.tool_read("src/main.rs"),
            self.tool_read("src/main.rs"),
            self.tool_read("notes-10-20.txt"),
            {"type": "tool_started", "data": {"operation": "bash", "target": "ls -la"}},
            self.observed("src/main.rs", offset=10, limit=5),
            self.observed("src/main.rs", offset=10, limit=5),
            self.observed("docs/spec.md", byteOffset=0, byteLimit=4096),
            self.observed("src/main.rs"),
        ])
        evidence = self.collect()
        reads = evidence["reads"]
        self.assertEqual(reads["source"], "read_observation")
        self.assertEqual(reads["semantics"], "successful_read_return")
        self.assertEqual(reads["legacy_attempts_suppressed"], 3)
        # Only the structured family is counted: no double count of the legacy
        # tool_started records for the same reads.
        self.assertEqual(reads["total"], 4)
        self.assertEqual(reads["distinct"], 3)
        self.assertEqual(reads["repeated_reads"], 1)
        self.assertEqual(reads["max_repeats"], 1)
        self.assertEqual(reads["ambiguous_entries"], 0)
        self.assertEqual(reads["range_known_entries"], 2)
        self.assertEqual(reads["range_unknown_entries"], 1)

        by_key = self.by_key(reads)
        for entry in reads["files"]:
            self.assertEqual(entry["origin"], "read_observation")
            self.assertEqual(entry["semantics"], "successful_read_return")
        # The legacy candidate for a path that never got a structured
        # observation is suppressed too, not silently kept as a second family.
        self.assertNotIn(("notes-10-20.txt", "unknown", None, None), by_key)

        # No explicit bounds means an unknown range with null bounds, never a
        # fabricated one.
        unknown = by_key[("src/main.rs", "unknown", None, None)]
        self.assertEqual(unknown["count"], 1)
        self.assertIsNone(unknown["range"]["length"])

        line = by_key[("src/main.rs", "line", 10, 14)]
        self.assertEqual(line["count"], 2)
        self.assertEqual(line["range"]["length"], 5)

        byte = by_key[("docs/spec.md", "byte", 0, 4095)]
        self.assertEqual(byte["count"], 1)
        self.assertEqual(byte["range"]["length"], 4096)

    def test_legacy_only_streams_degrade_to_unknown_attempts(self):
        self.write_log([
            self.tool_read("src/main.rs"),
            self.tool_read("src/main.rs"),
            self.tool_read("notes-10-20.txt"),
            self.tool_read("ok.txt"),
        ])
        reads = self.collect()["reads"]
        self.assertEqual(reads["source"], "legacy_tool_started")
        self.assertEqual(reads["semantics"], "legacy_read_attempt")
        self.assertEqual(reads["legacy_attempts_suppressed"], 0)
        self.assertEqual(reads["total"], 4)
        self.assertEqual(reads["distinct"], 3)
        self.assertEqual(reads["repeated_reads"], 1)
        self.assertEqual(reads["max_repeats"], 1)
        self.assertEqual(reads["range_known_entries"], 0)
        self.assertEqual(reads["range_unknown_entries"], 3)

        by_key = self.by_key(reads)
        # tool_started carries only a path, so every range stays unknown.
        self.assertEqual(by_key[("src/main.rs", "unknown", None, None)]["count"], 2)
        # A number inside the filename is never parsed as a range.
        self.assertIn(("notes-10-20.txt", "unknown", None, None), by_key)
        for entry in reads["files"]:
            self.assertEqual(entry["origin"], "tool_started")
            self.assertEqual(entry["semantics"], "legacy_read_attempt")
            self.assertIsNone(entry["range"]["start"])
            self.assertIsNone(entry["range"]["end"])
            self.assertIsNone(entry["range"]["length"])

    def test_one_read_is_never_double_counted_in_either_event_order(self):
        self.write_log([
            self.observed("a.txt", offset=1, limit=5),
            self.tool_read("a.txt"),
            self.observed("b/c.txt", byteOffset=4, byteLimit=64),
            self.tool_read("b/c.txt"),
        ])
        structured_first = self.collect()
        self.write_log([
            self.tool_read("a.txt"),
            self.observed("a.txt", offset=1, limit=5),
            self.tool_read("b/c.txt"),
            self.observed("b/c.txt", byteOffset=4, byteLimit=64),
        ])
        legacy_first = self.collect()
        # A late structured observation must still suppress an earlier legacy
        # candidate, and vice versa: the choice cannot depend on event order.
        self.assertEqual(structured_first["reads"], legacy_first["reads"])
        reads = structured_first["reads"]
        self.assertEqual(reads["source"], "read_observation")
        self.assertEqual(reads["legacy_attempts_suppressed"], 2)
        self.assertEqual(reads["total"], 2)
        self.assertEqual(reads["distinct"], 2)
        self.assertEqual(reads["repeated_reads"], 0)
        self.assertEqual(reads["range_known_entries"], 2)

    def test_repeated_ranges_and_multiple_paths_count_separately(self):
        self.write_log([
            self.observed("a.txt", offset=1, limit=5),
            self.observed("a.txt", offset=1, limit=5),
            self.observed("a.txt", offset=1, limit=5),
            self.observed("b.txt", offset=1, limit=5),
            self.observed("a.txt", offset=6, limit=5),
        ])
        reads = self.collect()["reads"]
        self.assertEqual(reads["total"], 5)
        self.assertEqual(reads["distinct"], 3)
        self.assertEqual(reads["repeated_reads"], 2)
        self.assertEqual(reads["max_repeats"], 2)
        by_key = self.by_key(reads)
        self.assertEqual(by_key[("a.txt", "line", 1, 5)]["count"], 3)
        self.assertEqual(by_key[("a.txt", "line", 1, 5)]["repeats"], 2)
        self.assertEqual(by_key[("b.txt", "line", 1, 5)]["count"], 1)
        # The same path with a different range is a distinct range entry.
        self.assertEqual(by_key[("a.txt", "line", 6, 10)]["count"], 1)

    def test_zero_length_empty_returns_never_fabricate_a_range(self):
        self.write_log([
            # The engine reports an empty file or empty page as a successful
            # observation with no bounds and a real zero total.
            {
                "type": "read_observation",
                "data": {"path": "empty.txt", "kind": "unknown", "total": 0, "truncated": False},
            },
            self.tool_read("empty.txt"),
            {
                "type": "read_observation",
                "data": {"path": "page.txt", "kind": "unknown", "total": 0, "truncated": True},
            },
            # A zero count reported as an explicit range is still not turned
            # into a one-line or one-byte range.
            self.observed("zero-line.txt", offset=1, limit=0),
            self.observed("zero-byte.txt", byteOffset=0, byteLimit=0),
        ])
        reads = self.collect()["reads"]
        self.assertEqual(reads["source"], "read_observation")
        self.assertEqual(reads["legacy_attempts_suppressed"], 1)
        self.assertEqual(reads["total"], 4)
        self.assertEqual(reads["range_known_entries"], 0)
        self.assertEqual(reads["range_unknown_entries"], 4)
        by_key = self.by_key(reads)
        for path in ("empty.txt", "page.txt", "zero-line.txt", "zero-byte.txt"):
            self.assertIn((path, "unknown", None, None), by_key)
        for entry in reads["files"]:
            self.assertIsNone(entry["range"]["start"])
            self.assertIsNone(entry["range"]["end"])
            self.assertIsNone(entry["range"]["length"])
            self.assertEqual(entry["semantics"], "successful_read_return")

    def test_failed_read_is_not_a_successful_return(self):
        self.write_log([
            self.tool_read("bad.txt"),
            {
                "type": "tool_finished",
                "data": {"operation": "read", "target": "bad.txt", "result": {"Err": "missing"}},
            },
            self.observed("good.txt", offset=2, limit=3),
        ])
        reads = self.collect()["reads"]
        self.assertEqual(reads["source"], "read_observation")
        self.assertEqual(reads["semantics"], "successful_read_return")
        # The failed read produced no structured observation, so it is never a
        # successful return; only its legacy attempt existed and is suppressed.
        self.assertEqual(reads["legacy_attempts_suppressed"], 1)
        self.assertEqual(reads["total"], 1)
        self.assertEqual([entry["path"] for entry in reads["files"]], ["good.txt"])

    def test_malicious_read_observation_fields_do_not_leak_or_fabricate(self):
        self.write_log([
            {"type": "read_observation", "data": {"path": "a.txt", "offset": True, "limit": 5}},
            {"type": "read_observation", "data": {"path": "b.txt", "offset": -5, "limit": 5}},
            {"type": "read_observation", "data": {"path": "c.txt", "offset": "10", "limit": 5}},
            {
                "type": "read_observation",
                "data": {"path": "d.txt", "offset": 1, "limit": 2, "byteOffset": 0, "byteLimit": 8},
            },
            {
                "type": "read_observation",
                "data": {"path": "e.txt", "kind": "line", "note": CREDENTIAL, "extra": {"nested": CREDENTIAL}},
            },
            {
                "type": "read_observation",
                "data": {"path": "f.txt", "offset": 1, "limit": 2, "runId": CREDENTIAL, "toolCallId": CREDENTIAL},
            },
            {"type": "read_observation", "data": {"path": "g.txt", "offset": 1, "limit": 999999999999999}},
            {"type": "read_observation", "data": {"path": "/etc/passwd", "offset": 1, "limit": 2}},
        ])
        evidence = self.collect()
        reads = evidence["reads"]
        self.assertEqual(reads["source"], "read_observation")
        self.assertEqual(reads["total"], 7)
        self.assertEqual(reads["range_known_entries"], 2)
        self.assertEqual(reads["range_unknown_entries"], 4)
        self.assertEqual(reads["ambiguous_entries"], 1)
        self.assertEqual(reads["rejected_paths"]["absolute"], 1)
        self.assertEqual(evidence["truncation"]["ambiguous_ranges"], 1)
        by_key = self.by_key(reads)
        self.assertIn(("g.txt", "line", 1, 999999999999999), by_key)
        self.assertIn(("e.txt", "unknown", None, None), by_key)
        self.assertIn(("f.txt", "line", 1, 2), by_key)
        # Non-integer, negative, string and bool bounds never become a range,
        # and a contradictory line+byte pair stays ambiguous with no bounds.
        ambiguous = by_key[("d.txt", "ambiguous", None, None)]
        self.assertIsNone(ambiguous["range"]["start"])
        self.assertIsNone(ambiguous["range"]["length"])
        self.assert_clean(evidence, CREDENTIAL, "/etc/passwd")

    def test_truncated_stream_leaves_the_read_family_unknown(self):
        self.write_log(
            [self.tool_read("a.txt")]
            + [self.observed("b.txt", offset=1, limit=1) for _ in range(40)]
        )
        evidence = self.collect(limits=diagnostics.Limits(max_source_bytes=120))
        self.assertTrue(evidence["truncation"]["source_truncated"])
        reads = evidence["reads"]
        # A truncated scan may have hidden the other family entirely, so the
        # choice itself is not reported as a fact.
        self.assertEqual(reads["source"], "unknown")
        self.assertEqual(reads["semantics"], "unknown")
        self.assertIsNone(reads["total"])

    def test_incomplete_or_conflicting_ranges_stay_unknown(self):
        self.write_log([
            {"type": "read_observation", "data": {"path": "a.txt", "offset": 3}},
            {"type": "read_observation", "data": {"path": "b.txt", "byteLimit": 16}},
            {"type": "read_observation", "data": {"path": "c.txt", "offset": 1.0, "limit": 2}},
            {
                "type": "read_observation",
                "data": {"path": "d.txt", "offset": 1, "limit": 2, "byteOffset": 0, "byteLimit": 8},
            },
        ])
        evidence = self.collect()
        kinds = sorted(entry["range"]["kind"] for entry in evidence["reads"]["files"])
        self.assertEqual(kinds, ["ambiguous", "unknown", "unknown", "unknown"])
        self.assertEqual(evidence["reads"]["source"], "read_observation")
        self.assertEqual(evidence["reads"]["range_unknown_entries"], 3)
        self.assertEqual(evidence["reads"]["ambiguous_entries"], 1)
        self.assertEqual(evidence["truncation"]["ambiguous_ranges"], 1)
        ambiguous = [e for e in evidence["reads"]["files"] if e["range"]["kind"] == "ambiguous"][0]
        self.assertIsNone(ambiguous["range"]["start"])


class PathRejectionTest(DiagnosticsTestCase):
    def test_host_paths_traversal_and_symlinks_are_rejected_without_leaking(self):
        outside = self.root / "outside"
        outside.mkdir()
        (outside / "host_secret.txt").write_text("host secret contents", encoding="utf-8")
        os.symlink(outside / "host_secret.txt", self.workspace / "escape_link")
        (self.workspace / "ok.txt").write_text("ok", encoding="utf-8")

        self.write_log([
            {"type": "read_observation", "data": {"path": "/etc/passwd"}},
            {"type": "read_observation", "data": {"path": "C:\\Users\\host\\secret.txt"}},
            {"type": "read_observation", "data": {"path": "../outside/host_secret.txt"}},
            {"type": "read_observation", "data": {"path": "ws/../../etc/shadow"}},
            {"type": "read_observation", "data": {"path": "escape_link"}},
            {"type": "read_observation", "data": {"path": "ok.txt"}},
            {"type": "read_observation", "data": {"path": "bad\x01name.txt"}},
            {"type": "read_observation", "data": {"path": ""}},
            {"type": "read_observation", "data": {"path": None}},
        ])
        evidence = self.collect()
        rejected = evidence["reads"]["rejected_paths"]
        self.assertGreaterEqual(rejected["absolute"], 2)
        self.assertGreaterEqual(rejected["parent_traversal"], 2)
        self.assertEqual(rejected["symlink_escape"], 1)
        self.assertEqual(rejected["control_chars"], 1)
        self.assertEqual(rejected["empty"], 1)
        self.assertEqual(rejected["not_a_string"], 1)

        stored = [entry["path"] for entry in evidence["reads"]["files"]]
        self.assertEqual(stored, ["ok.txt"])
        self.assert_clean(
            evidence,
            "/etc/passwd",
            "/etc/shadow",
            "host_secret.txt",
            "escape_link",
            "C:\\Users\\host\\secret.txt",
            "\x01",
            str(self.workspace),
            str(self.root),
        )

    def test_over_long_paths_are_rejected_and_counted_as_truncation(self):
        self.write_log([{"type": "read_observation", "data": {"path": "a" * 400}}])
        evidence = self.collect()
        self.assertEqual(evidence["reads"]["rejected_paths"]["too_long"], 1)
        self.assertEqual(evidence["truncation"]["fields_truncated"], 1)
        for entry in evidence["reads"]["files"]:
            self.assertLessEqual(len(entry["path"]), 256)


class SecretLeakTest(DiagnosticsTestCase):
    def test_replies_commands_and_arbitrary_ids_never_leak(self):
        self.write_log([
            {
                "type": "tool_finished",
                "data": {"operation": "read", "target": "ok.txt", "result": {"Ok": CREDENTIAL}},
            },
            {"type": "text", "data": CREDENTIAL},
            {"type": "diagnostic", "data": {"category": "network", "message": CREDENTIAL}},
            {
                "type": "completion_call",
                "data": {
                    "runId": "run-1",
                    "callIndex": 0,
                    "finishReason": "stop",
                    "responseId": CREDENTIAL,
                    "providerRequestId": CREDENTIAL,
                },
            },
            {
                "type": "tool_started",
                "data": {"operation": "bash", "target": "curl -H 'Authorization: " + CREDENTIAL + "'"},
            },
            {
                "type": "read_observation",
                "data": {"path": "safe.txt", "offset": 1, "limit": 2, "note": CREDENTIAL},
            },
        ])
        evidence = self.collect()
        self.assertEqual(evidence["model_calls"]["completed"], 1)
        self.assertEqual([entry["path"] for entry in evidence["reads"]["files"]], ["safe.txt"])
        self.assert_clean(evidence, CREDENTIAL, "Authorization", "curl")


class ConstraintCoverageTest(DiagnosticsTestCase):
    def test_allowlist_ids_report_preserved_dropped_and_unknown(self):
        self.write_log([
            {"type": "constraint_observation", "data": {"constraintId": "c-alpha", "status": "preserved"}},
            {"type": "constraint_observation", "data": {"constraintId": "c-beta", "status": "dropped"}},
            {"type": "constraint_observation", "data": {"constraintId": "c-beta", "status": "preserved"}},
            {"type": "constraint_observation", "data": {"constraintId": "not-allowed", "status": "preserved"}},
            {"type": "constraint_observation", "data": {"constraintId": "free text secret", "status": "preserved"}},
        ])
        evidence = self.collect(allowed_constraints=["c-gamma", "c-alpha", "c-beta"])
        coverage = evidence["constraint_coverage"]
        self.assertEqual(coverage["source"], "structured_events")
        self.assertEqual(coverage["allowed"], ["c-alpha", "c-beta", "c-gamma"])
        self.assertEqual(coverage["preserved"], ["c-alpha"])
        self.assertEqual(coverage["dropped"], [])
        self.assertEqual(coverage["conflicting"], ["c-beta"])
        self.assertEqual(coverage["unknown"], ["c-gamma"])
        self.assertEqual(coverage["non_allowlisted_ids"], 1)
        self.assert_clean(evidence, "not-allowed", "free text secret")

    def test_coverage_stays_unknown_without_structured_evidence(self):
        self.write_log([completion_call()])
        evidence = self.collect(allowed_constraints=["c-alpha"])
        coverage = evidence["constraint_coverage"]
        self.assertEqual(coverage["source"], "unknown")
        self.assertEqual(coverage["preserved"], [])
        self.assertEqual(coverage["dropped"], [])
        self.assertEqual(coverage["unknown"], ["c-alpha"])

    def test_allowlist_truncation_keeps_coverage_consistent(self):
        ids = ["c" + ("x" * 240) + "%03d" % index for index in range(256)]
        self.write_log(
            [{"type": "constraint_observation", "data": {"constraintId": ids[0], "status": "preserved"}}]
        )
        limits = diagnostics.Limits(max_output_bytes=4096)
        evidence = self.collect(allowed_constraints=ids, limits=limits)
        coverage = evidence["constraint_coverage"]
        self.assertTrue(evidence["truncation"]["allowed_truncated"])
        self.assertLess(len(coverage["allowed"]), len(ids))
        self.assertTrue(set(coverage["unknown"]).issubset(set(coverage["allowed"])))
        self.assertLessEqual(len(diagnostics.serialize(evidence)), 4096)

    def test_compaction_report_constraint_ids_are_consumed(self):
        self.write_log([
            {
                "type": "context_compaction",
                "data": {
                    "kind": "session",
                    "report": {
                        "sourceStart": 0,
                        "sourceEnd": 12,
                        "sourceBytes": 4096,
                        "summaryBytes": 512,
                        "preservedSystemMessages": 2,
                        "unrepresentedContentMessages": 3,
                        "truncatedSections": ["a", "b"],
                        "preservedConstraintIds": ["c-alpha", "unlisted"],
                        "droppedConstraintIds": [],
                    },
                },
            },
        ])
        evidence = self.collect(allowed_constraints=["c-alpha"])
        self.assertEqual(evidence["constraint_coverage"]["preserved"], ["c-alpha"])
        self.assertEqual(evidence["constraint_coverage"]["non_allowlisted_ids"], 1)


class CompactionTest(DiagnosticsTestCase):
    @staticmethod
    def compaction(kind, report):
        return {"type": "context_compaction", "data": {"kind": kind, "report": report}}

    def test_compaction_before_after_and_dropped_counts(self):
        # The legacy camelCase envelope keeps working, but the prefix source
        # range is no longer passed off as the full history: the full-history
        # before/after counts have no fields here and stay unknown.
        self.write_log([
            {
                "type": "context_compaction",
                "data": {
                    "kind": "request",
                    "report": {
                        "sourceStart": 2,
                        "sourceEnd": 14,
                        "sourceBytes": 8000,
                        "summaryBytes": 900,
                        "preservedSystemMessages": 1,
                        "unrepresentedContentMessages": 4,
                        "truncatedSections": ["Non-extracted content"],
                        "toolCallIdsTruncated": False,
                        "extractive": True,
                    },
                },
            },
            {
                "type": "context_compaction",
                "data": {
                    "kind": "session",
                    "report": {
                        "sourceStart": 0,
                        "sourceEnd": 6,
                        "sourceBytes": 2000,
                        "summaryBytes": 300,
                        "preservedSystemMessages": 0,
                        "unrepresentedContentMessages": 1,
                        "truncatedSections": [],
                        "toolCallIdsTruncated": False,
                        "extractive": True,
                    },
                },
            },
        ])
        evidence = self.collect()
        compaction = evidence["compaction"]
        self.assertEqual(compaction["events"], 2)
        self.assertEqual(compaction["kinds"], {"request": 1, "session": 1, "unknown": 0})
        # Original replaced-prefix semantics, reported under source_*.
        self.assertEqual(compaction["source_messages"], 18)
        self.assertEqual(compaction["source_bytes"], 10000)
        self.assertEqual(compaction["summary_bytes"], 1200)
        # Full-history before/after counts are absent, so they stay null rather
        # than reusing the prefix range as the whole history length.
        self.assertIsNone(compaction["messages_before"])
        self.assertIsNone(compaction["messages_after"])
        self.assertIsNone(compaction["bytes_before"])
        self.assertIsNone(compaction["bytes_after"])
        self.assertEqual(compaction["dropped_messages"], 5)
        self.assertIsNone(compaction["dropped_bytes"])
        self.assertEqual(compaction["truncated_sections"], 1)
        self.assertIs(compaction["tool_call_ids_truncated"], False)
        self.assertIs(compaction["extractive"], True)
        # No goal ids were reported, so nothing was counted.
        self.assertEqual(compaction["goal_ids"], 0)
        self.assertEqual(evidence["truncation"]["conflicting_fields"], 0)

    def test_real_snake_case_report_fields_are_consumed(self):
        # The real ContextCompactionObservation keeps a camelCase outer envelope
        # but its nested CompactionLossReport is snake_case. Full-history and
        # constraint/goal fields must be read from that real shape.
        self.write_log([
            self.compaction("session", {
                "schema_version": 1,
                "source_start": 4,
                "source_end": 10,
                "source_bytes": 3000,
                "preserved_system_messages": 2,
                "summary_bytes": 400,
                "affected_tool_call_ids": ["call-secret-1"],
                "tool_call_ids_truncated": False,
                "truncated_sections": ["Non-extracted content"],
                "unrepresented_content_messages": 2,
                "pinned_items_preserved": True,
                "extractive": True,
                "before_messages": 40,
                "after_messages": 12,
                "before_bytes": 20000,
                "after_bytes": 5000,
                "preserved_constraint_ids": ["c-alpha"],
                "dropped_constraint_ids": ["c-beta"],
                "preserved_goal_ids": ["g-one"],
            }),
        ])
        evidence = self.collect(allowed_constraints=["c-alpha", "c-beta", "c-gamma"])
        compaction = evidence["compaction"]
        self.assertEqual(compaction["events"], 1)
        self.assertEqual(compaction["source_messages"], 6)
        self.assertEqual(compaction["source_bytes"], 3000)
        self.assertEqual(compaction["summary_bytes"], 400)
        self.assertEqual(compaction["messages_before"], 40)
        self.assertEqual(compaction["messages_after"], 12)
        self.assertEqual(compaction["bytes_before"], 20000)
        self.assertEqual(compaction["bytes_after"], 5000)
        self.assertEqual(compaction["dropped_messages"], 2)
        self.assertEqual(compaction["truncated_sections"], 1)
        self.assertIs(compaction["pinned_items_preserved"], True)
        self.assertIs(compaction["extractive"], True)
        self.assertEqual(compaction["goal_ids"], 1)
        coverage = evidence["constraint_coverage"]
        self.assertEqual(coverage["preserved"], ["c-alpha"])
        self.assertEqual(coverage["dropped"], ["c-beta"])
        self.assertEqual(coverage["unknown"], ["c-gamma"])
        self.assertEqual(coverage["non_allowlisted_ids"], 0)
        # The real tool-call ids in the report are never retained.
        self.assert_clean(evidence, "call-secret-1")

    def test_camel_case_full_history_fields_stay_compatible(self):
        self.write_log([
            self.compaction("request", {
                "sourceStart": 0,
                "sourceEnd": 5,
                "beforeMessages": 30,
                "afterMessages": 8,
                "beforeBytes": 15000,
                "afterBytes": 3000,
                "preservedConstraintIds": ["c-alpha"],
                "droppedConstraintIds": [],
                "preservedGoalIds": ["g-one"],
            }),
        ])
        evidence = self.collect(allowed_constraints=["c-alpha"])
        compaction = evidence["compaction"]
        self.assertEqual(compaction["source_messages"], 5)
        self.assertEqual(compaction["messages_before"], 30)
        self.assertEqual(compaction["messages_after"], 8)
        self.assertEqual(compaction["bytes_before"], 15000)
        self.assertEqual(compaction["bytes_after"], 3000)
        self.assertEqual(compaction["goal_ids"], 1)
        self.assertEqual(evidence["constraint_coverage"]["preserved"], ["c-alpha"])
        self.assertEqual(evidence["truncation"]["conflicting_fields"], 0)

    def test_identical_dual_spellings_are_accepted(self):
        self.write_log([
            self.compaction("session", {
                "source_bytes": 100,
                "sourceBytes": 100,
                "before_messages": 5,
                "beforeMessages": 5,
            }),
        ])
        evidence = self.collect()
        compaction = evidence["compaction"]
        self.assertEqual(evidence["truncation"]["conflicting_fields"], 0)
        self.assertEqual(evidence["source"]["malformed"], 0)
        self.assertEqual(compaction["source_bytes"], 100)
        self.assertEqual(compaction["messages_before"], 5)

    def test_conflicting_dual_spellings_are_malformed_not_guessed(self):
        self.write_log([
            self.compaction("request", {
                "before_messages": 40,
                "beforeMessages": 41,
                "preserved_constraint_ids": ["c-alpha"],
                "preservedConstraintIds": ["c-beta"],
            }),
        ])
        evidence = self.collect(allowed_constraints=["c-alpha", "c-beta"])
        # Neither spelling is chosen: the record is malformed and the fields are
        # unknown, never a guessed value.
        self.assertGreaterEqual(evidence["source"]["malformed"], 1)
        self.assertGreaterEqual(evidence["truncation"]["conflicting_fields"], 1)
        compaction = evidence["compaction"]
        self.assertIsNone(compaction["events"])
        self.assertIsNone(compaction["messages_before"])
        coverage = evidence["constraint_coverage"]
        self.assertEqual(coverage["preserved"], [])
        self.assertEqual(coverage["dropped"], [])

    def test_goal_ids_are_counted_but_never_mixed_into_constraints(self):
        self.write_log([
            self.compaction("session", {
                "preserved_constraint_ids": ["c-alpha"],
                "dropped_constraint_ids": [],
                "preserved_goal_ids": ["secret-goal-value", "c-alpha"],
            }),
        ])
        evidence = self.collect(allowed_constraints=["c-alpha", "c-beta"])
        coverage = evidence["constraint_coverage"]
        self.assertEqual(coverage["preserved"], ["c-alpha"])
        self.assertEqual(coverage["dropped"], [])
        self.assertEqual(coverage["unknown"], ["c-beta"])
        # A goal id outside the allowlist is not counted as a stray constraint
        # id, and no goal id string is stored.
        self.assertEqual(coverage["non_allowlisted_ids"], 0)
        self.assertEqual(evidence["compaction"]["goal_ids"], 2)
        self.assert_clean(evidence, "secret-goal-value")

    def test_missing_history_fields_never_fake_zero(self):
        # One record reports full-history counts, one does not: the aggregate is
        # unknown rather than a partial sum of only the reporting record.
        self.write_log([
            self.compaction("session", {
                "before_messages": 10,
                "after_messages": 4,
                "source_start": 0,
                "source_end": 3,
            }),
            self.compaction("session", {}),
        ])
        compaction = self.collect()["compaction"]
        self.assertEqual(compaction["events"], 2)
        self.assertIsNone(compaction["messages_before"])
        self.assertIsNone(compaction["messages_after"])
        self.assertIsNone(compaction["bytes_before"])
        self.assertIsNone(compaction["bytes_after"])
        # The prefix count is also withheld because the second record omits it,
        # instead of silently summing only the reporting record.
        self.assertIsNone(compaction["source_messages"])

    def test_truncated_scan_leaves_compaction_history_unknown(self):
        self.write_log(
            [
                self.compaction("session", {
                    "source_start": 0,
                    "source_end": 4,
                    "before_messages": 10,
                    "after_messages": 4,
                })
            ]
            + [{"type": "text", "data": "x" * 400} for _ in range(10)]
        )
        evidence = self.collect(limits=diagnostics.Limits(max_source_bytes=200))
        self.assertTrue(evidence["truncation"]["source_truncated"])
        compaction = evidence["compaction"]
        self.assertIsNone(compaction["events"])
        self.assertIsNone(compaction["source_messages"])
        self.assertIsNone(compaction["messages_before"])
        self.assertIsNone(compaction["messages_after"])
        self.assertIsNone(compaction["goal_ids"])



class TruncationTest(DiagnosticsTestCase):
    def test_only_unrelated_output_cannot_prove_zero_calls_or_reads(self):
        self.write_log([{"type": "text", "data": "ignored private reply"}], raw_lines=["stderr diagnostic"])
        evidence = self.collect()
        self.assertEqual(evidence["source"]["events_recognized"], 0)
        self.assertEqual(evidence["source"]["unsupported_records"], 2)
        self.assertIsNone(evidence["model_calls"]["completed"])
        self.assertIsNone(evidence["reads"]["total"])
        self.assert_clean(evidence, "ignored private reply", "stderr diagnostic")

    def test_source_bytes_never_exceed_the_declared_cap(self):
        self.write_log([{"type": "text", "data": "x" * 10000}])
        evidence = self.collect(limits=diagnostics.Limits(max_source_bytes=200))
        self.assertTrue(evidence["truncation"]["source_truncated"])
        self.assertLessEqual(evidence["source"]["bytes_read"], 200)
        # Also exercise the oversized-line drain, not only its initial read.
        evidence = self.collect(limits=diagnostics.Limits(max_source_bytes=200, max_line_bytes=32))
        self.assertTrue(evidence["truncation"]["source_truncated"])
        self.assertLessEqual(evidence["source"]["bytes_read"], 200)

    def test_maximum_constraint_coverage_is_still_bounded(self):
        allowed = [f"rule-{index:03d}-" + "x" * 240 for index in range(256)]
        self.write_log([
            {"type": "constraint_observation", "data": {"constraintId": item, "status": "preserved"}}
            for item in allowed
        ])
        evidence = self.collect(allowed_constraints=allowed)
        self.assertLessEqual(len(diagnostics.serialize(evidence)), 64 * 1024)
        self.assertTrue(evidence["truncation"]["output_truncated"])
        coverage = evidence["constraint_coverage"]
        self.assertEqual(coverage["source"], "structured_events")
        self.assertEqual(coverage["evidence_ids"], 256)
        self.assertTrue(coverage["preserved"])
        for category in ("preserved", "dropped", "conflicting", "unknown"):
            self.assertTrue(set(coverage[category]).issubset(coverage["allowed"]))

    def test_impossibly_small_output_bound_is_refused(self):
        self.write_log([])
        with self.assertRaises(ValueError):
            self.collect(limits=diagnostics.Limits(max_output_bytes=16))

    def test_event_cap_is_explicit_and_unreliable_counts_become_null(self):
        events = [completion_call(call_index=i) for i in range(10)]
        self.write_log(events)
        limits = diagnostics.Limits(max_events=3)
        evidence = self.collect(limits=limits)
        self.assertTrue(evidence["truncation"]["events_truncated"])
        self.assertEqual(evidence["source"]["events_scanned"], 3)
        self.assertIsNone(evidence["model_calls"]["completed"])
        self.assertIsNone(evidence["reads"]["total"])
        self.assertEqual(evidence["model_calls"]["deduplicated"], 0)

    def test_output_byte_budget_shrinks_records_explicitly(self):
        events = [
            {
                "type": "read_observation",
                "data": {"path": "dir/" + ("p%03d" % index) * 20 + ".txt", "offset": index + 1, "limit": 1},
            }
            for index in range(400)
        ]
        self.write_log(events)
        evidence = self.collect()
        self.assertTrue(evidence["truncation"]["output_truncated"])
        self.assertGreater(evidence["truncation"]["dropped_records"], 0)
        self.assertLessEqual(len(diagnostics.serialize(evidence)), 64 * 1024)
        self.assertLess(len(evidence["reads"]["files"]), 400)
        self.assertEqual(evidence["reads"]["total"], 400)

    def test_oversized_lines_are_drained_without_being_kept(self):
        limits = diagnostics.Limits(max_line_bytes=128, max_source_bytes=4096)
        giant = "G" * 5000
        self.write_log(
            [{"type": "read_observation", "data": {"path": "kept.txt", "offset": 1, "limit": 1}}],
            raw_lines=["{\"type\":\"text\",\"data\":\"" + giant + "\"}"],
        )
        evidence = self.collect(limits=limits)
        self.assertEqual(evidence["truncation"]["oversized_lines"], 1)
        self.assertGreaterEqual(evidence["truncation"]["fields_truncated"], 1)
        self.assertEqual(evidence["source"]["events_scanned"], 1)
        self.assertIsNone(evidence["reads"]["total"])
        self.assert_clean(evidence, giant)

    def test_source_byte_cap_stops_the_scan(self):
        events = [{"type": "model_request", "data": {"padding": "x" * 80}} for _ in range(20)]
        self.write_log(events)
        limits = diagnostics.Limits(max_source_bytes=200)
        evidence = self.collect(limits=limits)
        self.assertTrue(evidence["truncation"]["source_truncated"])
        self.assertLess(evidence["source"]["events_scanned"], 20)
        self.assertIsNone(evidence["model_calls"]["requests"])


class WriteTest(DiagnosticsTestCase):
    def test_write_is_exclusive_and_owner_only(self):
        self.write_log([completion_call()])
        evidence = self.collect()
        target = self.root / "evidence" / "task.json"
        written = diagnostics.write(evidence, target, create_parents=True)
        self.assertEqual(written, len(diagnostics.serialize(evidence)))
        self.assertEqual(stat.S_IMODE(os.stat(target).st_mode), 0o600)
        with self.assertRaises(diagnostics.EvidenceAlreadyExists):
            diagnostics.write(evidence, target)
        self.assertNotIn(str(self.workspace), target.read_text(encoding="utf-8"))

    def test_collect_and_write_returns_the_evidence(self):
        self.write_log([completion_call()])
        target = self.root / "task.json"
        evidence = diagnostics.collect_and_write(self.log, self.workspace, target)
        self.assertEqual(evidence["model_calls"]["completed"], 1)
        self.assertEqual(json.loads(target.read_text(encoding="utf-8")), evidence)


class RunnerShapeTest(DiagnosticsTestCase):
    def test_real_runner_envelope_and_unknown_events_are_tolerated(self):
        # Mirrors the real --json lines: schemaVersion plus the v1 correlation
        # nulls and extra unknown keys, all of which must not leak or break parsing.
        self.write_log([
            {
                "type": "tool_started",
                "data": {"operation": "read", "target": "src/lib.rs"},
                "schemaVersion": 1,
                "runId": None,
                "turnId": None,
                "toolCallId": None,
                "subagentId": None,
            },
            {
                "type": "tool_finished",
                "data": {"operation": "read", "target": "src/lib.rs", "result": {"Ok": "private source"}},
                "schemaVersion": 1,
            },
            {"type": "unknown_future_event", "data": {"note": "ignored"}},
        ])
        evidence = self.collect()
        # A legacy-only runner stream keeps the attempt, with an unknown range.
        self.assertEqual(evidence["reads"]["source"], "legacy_tool_started")
        self.assertEqual(evidence["reads"]["semantics"], "legacy_read_attempt")
        self.assertEqual(evidence["reads"]["total"], 1)
        self.assertEqual(evidence["reads"]["files"][0]["path"], "src/lib.rs")
        self.assertEqual(evidence["reads"]["files"][0]["origin"], "tool_started")
        self.assertIsNone(evidence["reads"]["files"][0]["range"]["start"])
        self.assert_clean(evidence, "private source", "unknown_future_event", "ignored")


class ToolActivityTest(DiagnosticsTestCase):
    """Fixed-vocabulary builtin tool activity from the real CLI JSON payloads.

    The live ``--json`` schema reports the tool operation in ``data.operation``
    on ``tool_started``/``tool_finished``; these tests pin that only observed
    events are counted, that unknown or extension labels fold into ``other``
    without ever being stored, and that a phase whose event family never
    appeared stays unknown instead of a fabricated zero.
    """

    @staticmethod
    def started(operation, target="ignored-target"):
        return {"type": "tool_started", "data": {"operation": operation, "target": target}}

    @staticmethod
    def finished(operation, ok=True, target="ignored-target"):
        return {
            "type": "tool_finished",
            "data": {
                "operation": operation,
                "target": target,
                "result": {"Ok": "private result"} if ok else {"Err": "private error"},
            },
        }

    def test_real_shapes_are_counted_per_phase_and_name(self):
        self.write_log([
            self.started("read", "src/lib.rs"),
            self.finished("read", ok=True, target="src/lib.rs"),
            self.started("grep", "SECRET-PATTERN"),
            self.finished("grep", ok=False, target="SECRET-PATTERN"),
            self.started("plugin__pager__emit_long", "SECRET-ARGS"),
        ])
        evidence = self.collect()
        tools = evidence["tool_activity"]
        self.assertEqual(tools["request"]["source"], "tool_started")
        self.assertEqual(tools["result"]["source"], "tool_finished:ok")
        self.assertEqual(tools["error"]["source"], "tool_finished:err")
        self.assertEqual(tools["request"]["by_name"]["read"], 1)
        self.assertEqual(tools["request"]["by_name"]["grep"], 1)
        self.assertEqual(tools["request"]["by_name"]["other"], 1)
        self.assertEqual(tools["request"]["by_name"]["workspace"], 0)
        self.assertEqual(tools["result"]["by_name"]["read"], 1)
        self.assertEqual(tools["error"]["by_name"]["grep"], 1)
        self.assertEqual(tools["request"]["events"], 3)
        self.assertTrue(tools["request"]["observed"])
        # Dispatch is not instrumented in this legacy stream, so it is unknown.
        self.assertIsNone(tools["dispatch"]["events"])
        self.assertFalse(tools["dispatch"]["observed"])
        self.assertNotIn("plugin__pager__emit_long", tools["names"])
        self.assert_clean(
            evidence,
            "SECRET-PATTERN",
            "SECRET-ARGS",
            "private result",
            "private error",
        )

    def test_registered_but_uncalled_tool_is_not_usage(self):
        # Only `read` runs. `grep` is registered in the request's tool list
        # (outside this event log) but never produces a tool event, so it is a
        # real observed zero rather than a count.
        self.write_log([self.started("read", "a.txt")])
        tools = self.collect()["tool_activity"]
        self.assertEqual(tools["request"]["by_name"]["read"], 1)
        self.assertEqual(tools["request"]["by_name"]["grep"], 0)
        self.assertEqual(tools["request"]["events"], 1)

    def test_a_stream_without_tool_events_keeps_every_phase_unknown(self):
        self.write_log([completion_call(call_index=0)])
        tools = self.collect()["tool_activity"]
        for phase in ("request", "result", "error", "dispatch"):
            self.assertIsNone(tools[phase]["events"])
            self.assertFalse(tools[phase]["observed"])
        self.assertIsNone(tools["request"]["by_name"])

    def test_missing_phases_stay_unknown(self):
        self.write_log([self.started("ls")])
        tools = self.collect()["tool_activity"]
        self.assertEqual(tools["request"]["events"], 1)
        self.assertIsNone(tools["result"]["events"])
        self.assertIsNone(tools["error"]["events"])
        self.assertIsNone(tools["dispatch"]["events"])

        # A finished-only stream makes request unknown but keeps a real zero
        # for the error phase, because the tool_finished family was observed.
        self.write_log([self.finished("ls", ok=True)])
        tools = self.collect()["tool_activity"]
        self.assertIsNone(tools["request"]["events"])
        self.assertEqual(tools["result"]["events"], 1)
        self.assertEqual(tools["error"]["events"], 0)

    def test_dispatch_is_unnamed_and_deduplicated(self):
        self.write_log([
            run_observation("tool_dispatched", effect_id=9, turn_id=1, toolCallId="call-1"),
            run_observation("tool_dispatched", effect_id=9, turn_id=1, toolCallId="call-1"),
            run_observation("tool_dispatched", effect_id=10, turn_id=2, toolCallId="call-2"),
            run_observation("model_dispatched", effect_id=7, turn_id=1),
        ])
        evidence = self.collect()
        dispatch = evidence["tool_activity"]["dispatch"]
        self.assertTrue(dispatch["observed"])
        self.assertEqual(dispatch["events"], 2)
        self.assertEqual(dispatch["duplicates"], 1)
        self.assertIsNone(dispatch["by_name"])
        # The model prepared/dispatched family is never changed by tool events.
        self.assertEqual(evidence["model_calls"]["dispatched"], 1)
        self.assert_clean(evidence, "call-1", "call-2")

    def test_unknown_names_are_folded_and_never_stored(self):
        self.write_log([
            {"type": "tool_started", "data": {"operation": "plugin__" + CREDENTIAL, "target": CREDENTIAL}},
            {
                "type": "tool_finished",
                "data": {
                    "operation": "mcp__server__" + CREDENTIAL,
                    "target": "x",
                    "result": {"Ok": CREDENTIAL},
                },
            },
        ])
        evidence = self.collect()
        tools = evidence["tool_activity"]
        self.assertEqual(tools["request"]["by_name"]["other"], 1)
        self.assertEqual(tools["result"]["by_name"]["other"], 1)
        self.assert_clean(evidence, CREDENTIAL, "plugin__", "mcp__server__")

    def test_malformed_local_payloads_are_counted_without_guessing(self):
        limits = diagnostics.Limits(max_field_chars=64)
        self.write_log([
            {"type": "tool_started", "data": {"operation": 17}},
            {"type": "tool_started", "data": {"operation": "x" * 400}},
            {"type": "tool_finished", "data": {"operation": "read", "result": {"Ok": "a", "Err": "b"}}},
            {"type": "tool_finished", "data": {"operation": "read"}},
            {"type": "tool_finished", "data": {"operation": "read", "result": "not-an-enum"}},
        ])
        evidence = self.collect(limits=limits)
        tools = evidence["tool_activity"]
        # Four unusable payloads are local tool-activity malformed records; the
        # stream itself stays reliable because no global malformed line exists.
        self.assertEqual(evidence["source"]["malformed"], 0)
        self.assertEqual(tools["malformed"], 4)
        self.assertTrue(tools["truncated"])
        self.assertEqual(tools["request"]["events"], 1)
        self.assertEqual(tools["request"]["by_name"]["other"], 1)
        self.assertNotIn("x" * 400, diagnostics.serialize(evidence).decode("utf-8"))

    def test_a_non_object_payload_is_a_global_malformed_record(self):
        self.write_log([{"type": "tool_started", "data": "not-an-object"}])
        evidence = self.collect()
        self.assertEqual(evidence["source"]["malformed"], 1)
        # An unreliable scan cannot assert any tool count, but the local
        # malformed counter is still explicit.
        self.assertIsNone(evidence["tool_activity"]["request"]["events"])
        self.assertEqual(evidence["tool_activity"]["malformed"], 0)

    def test_existing_fields_do_not_regress_with_tool_events(self):
        self.write_log([
            completion_call(call_index=0),
            run_observation("model_dispatched", effect_id=1, turn_id=1),
            self.started("read", "a.txt"),
            self.started("read", "a.txt"),
            self.finished("read", ok=True, target="a.txt"),
            {"type": "read_observation", "data": {"path": "a.txt", "offset": 1, "limit": 5}},
        ])
        evidence = self.collect()
        self.assertEqual(evidence["model_calls"]["completed"], 1)
        self.assertEqual(evidence["model_calls"]["dispatched"], 1)
        self.assertEqual(evidence["reads"]["source"], "read_observation")
        self.assertEqual(evidence["reads"]["total"], 1)
        self.assertEqual(evidence["reads"]["legacy_attempts_suppressed"], 2)
        self.assertEqual(evidence["tool_activity"]["request"]["by_name"]["read"], 2)
        self.assertEqual(evidence["tool_activity"]["result"]["by_name"]["read"], 1)

    def test_event_and_output_caps_keep_tool_activity_bounded(self):
        self.write_log([self.started("grep", "t") for _ in range(6)])
        evidence = self.collect(limits=diagnostics.Limits(max_events=3))
        self.assertTrue(evidence["truncation"]["events_truncated"])
        self.assertIsNone(evidence["tool_activity"]["request"]["events"])
        self.assertFalse(evidence["tool_activity"]["request"]["observed"])

        # The new block is counted in the existing output cap and is dropped
        # explicitly (never silently truncated in place) when the cap is tight.
        self.write_log([self.started("grep", "t"), self.finished("grep", ok=True)])
        evidence = self.collect(limits=diagnostics.Limits(max_output_bytes=4600))
        self.assertTrue(evidence["truncation"]["output_truncated"])
        self.assertLessEqual(len(diagnostics.serialize(evidence)), 4600)
        self.assertIsNone(evidence["tool_activity"])


class UnreadableSourceTest(DiagnosticsTestCase):
    def test_missing_log_stays_unknown_not_zero(self):
        evidence = diagnostics.collect(self.root / "missing.log", self.workspace)
        self.assertFalse(evidence["source"]["opened"])
        self.assertEqual(evidence["source"]["unreadable"], "missing")
        self.assertIsNone(evidence["model_calls"]["completed"])
        self.assertIsNone(evidence["model_calls"]["requests"])
        self.assertIsNone(evidence["reads"]["total"])
        self.assertIsNone(evidence["compaction"]["events"])


if __name__ == "__main__":
    unittest.main()
