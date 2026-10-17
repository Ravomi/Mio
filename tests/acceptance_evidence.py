"""Offline tests for the opt-in sanitized acceptance-evidence collector.

Every test builds synthetic acceptance sources and recorded fake ``check.log``
files; no real acceptance script, model, credential or Rust build is touched. The
collector must stay bounded, sanitized and honest about unknown/truncated states,
and the runner must only expose the field when ``--diagnostics-dir`` is opted in.
"""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import stat
import sys
import tempfile
import unittest
from unittest import mock

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts" / "agent_eval.py"
spec = importlib.util.spec_from_file_location("agent_eval_acceptance_evidence", SCRIPT)
evaluation = importlib.util.module_from_spec(spec)
spec.loader.exec_module(evaluation)

NO_OP_AGENT = [sys.executable, "-c", "pass", "{prompt}"]
DEFAULT_RESULT_KEYS = {
    "task", "status", "failure_class", "accepted", "duration_seconds", "agent",
    "acceptance", "usage", "timing", "diagnostic_categories", "artifacts", "diagnostics",
}


def write_source(root, text="a = 1\nb = 2\nc = 3\n"):
    path = Path(root) / "acceptance.py"
    path.write_text(text, encoding="utf-8")
    return path


def write_log(root, text, name="check.log"):
    path = Path(root) / name
    path.write_text(text, encoding="utf-8")
    return path


def traceback_log(entries, final, header="Traceback (most recent call last):"):
    lines = [header]
    for path, line in entries:
        lines.append(f'  File "{path}", line {line}, in <module>')
        lines.append("    raise RuntimeError('source line')")
    lines.append(final)
    return "\n".join(lines) + "\n"


class ExtractionTests(unittest.TestCase):
    def test_exact_source_line_and_allowlisted_exception(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = write_source(temporary, "a = 1\nb = 2\nc = 3\nd = 4\ne = 5\n")
            log = write_log(temporary, traceback_log(
                [(source, 3)], "AssertionError: PRIVATE MESSAGE BODY"
            ))
            expected_digest = hashlib.sha256(source.read_bytes()).hexdigest()
            block = evaluation.collect_acceptance_evidence(
                {"exit_code": 1, "timed_out": False}, source, log
            )
        self.assertEqual(block["status"], "collected")
        self.assertIsNone(block["reason"])
        self.assertEqual(block["exception_kind"], "AssertionError")
        self.assertEqual(block["frames"]["lines"], [3])
        self.assertEqual(block["frames"]["count"], 1)
        self.assertFalse(block["frames"]["truncated"])
        self.assertEqual(block["frames"]["out_of_range"], 0)
        self.assertEqual(block["source"]["sha256"], expected_digest)
        self.assertEqual(block["source"]["lines"], 5)
        self.assertTrue(block["source"]["readable"])
        self.assertTrue(block["log"]["opened"])
        self.assertEqual(block["log"]["format"], "python_traceback")
        self.assertEqual(block["trust"], "untrusted_reported_check_log")
        self.assertNotIn("PRIVATE", json.dumps(block))

    def test_success_is_not_applicable_and_claims_nothing(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = write_source(temporary)
            log = write_log(temporary, traceback_log(
                [(source, 1)], "AssertionError: should not be read"
            ))
            block = evaluation.collect_acceptance_evidence(
                {"exit_code": 0, "timed_out": False}, source, log
            )
        self.assertEqual(block["status"], "not_applicable")
        self.assertEqual(block["reason"], "acceptance_passed")
        self.assertEqual(block["exception_kind"], "unknown")
        self.assertEqual(block["frames"]["lines"], [])
        self.assertFalse(block["log"]["opened"])
        self.assertEqual(block["log"]["format"], "not_read")

    def test_exception_allowlist_and_unlisted_types_collapse_to_other(self):
        cases = (
            ("ValueError: x", "ValueError"),
            ("TypeError: x", "TypeError"),
            ("KeyError: 'k'", "KeyError"),
            ("AssertionError", "AssertionError"),
            ("my.module.PrivateError: do not leak", "other"),
        )
        for final, expected in cases:
            with self.subTest(final=final):
                with tempfile.TemporaryDirectory() as temporary:
                    source = write_source(temporary)
                    log = write_log(temporary, traceback_log([(source, 2)], final))
                    block = evaluation.collect_acceptance_evidence(
                        {"exit_code": 1}, source, log
                    )
                self.assertEqual(block["exception_kind"], expected)
                self.assertNotIn("PrivateError", json.dumps(block))
                self.assertNotIn("do not leak", json.dumps(block))

    def test_wrong_file_frames_are_ignored(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = write_source(temporary, "a = 1\nb = 2\nc = 3\n")
            other = Path(temporary) / "other.py"
            near = str(source) + ".bak"
            log = write_log(temporary, traceback_log(
                [(other, 1), (near, 2), (source, 2)], "ValueError: x"
            ))
            block = evaluation.collect_acceptance_evidence({"exit_code": 1}, source, log)
        self.assertEqual(block["frames"]["lines"], [2])
        serialized = json.dumps(block)
        self.assertNotIn("other.py", serialized)
        self.assertNotIn(str(other), serialized)
        self.assertNotIn(str(near), serialized)

    def test_chained_tracebacks_keep_the_final_exception_and_all_frames(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = write_source(temporary, "a = 1\nb = 2\nc = 3\nd = 4\n")
            log = write_log(temporary, (
                "Traceback (most recent call last):\n"
                f'  File "{source}", line 2, in <module>\n'
                "    f()\n"
                "ValueError: first\n"
                "During handling of the above exception, another exception occurred:\n"
                "Traceback (most recent call last):\n"
                f'  File "{source}", line 4, in g\n'
                "    raise TypeError()\n"
                "TypeError: second\n"
            ))
            block = evaluation.collect_acceptance_evidence({"exit_code": 1}, source, log)
        self.assertEqual(block["exception_kind"], "TypeError")
        self.assertEqual(block["frames"]["lines"], [2, 4])

    def test_fake_frame_and_message_are_untrusted_and_never_leak_raw_body(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = write_source(temporary)
            hostile = "/tmp/sk-live-HOSTILE-PATH/api.py"
            log = write_log(temporary, (
                "Traceback (most recent call last):\n"
                f'  File "{source}", line 1, in <module>\n'
                '    exec("api_key=sk-live-1234567890")\n'
                f'  File "{hostile}", line 9, in <module>\n'
                "AssertionError: raw body sk-live-1234567890 PRIVATE\n"
            ))
            block = evaluation.collect_acceptance_evidence({"exit_code": 1}, source, log)
        serialized = json.dumps(block)
        self.assertEqual(block["trust"], "untrusted_reported_check_log")
        self.assertEqual(block["exception_kind"], "AssertionError")
        self.assertEqual(block["frames"]["lines"], [1])
        for forbidden in ("sk-live", "api_key", "raw body", "PRIVATE", hostile, "exec("):
            self.assertNotIn(forbidden, serialized)
        self.assertIn("forged", block["note"])


class UnknownAndMalformedTests(unittest.TestCase):
    def collect(self, acceptance, source_text, log=None, log_text=None, **kwargs):
        with tempfile.TemporaryDirectory() as temporary:
            source = write_source(temporary, source_text)
            log_path = Path(temporary) / "check.log"
            if log_text is not None:
                log_path.write_text(log_text, encoding="utf-8")
            return evaluation.collect_acceptance_evidence(acceptance, source, log_path)

    def test_missing_log_is_explicitly_unknown(self):
        block = self.collect({"exit_code": 1}, "a = 1\n")
        self.assertEqual(block["status"], "unknown")
        self.assertEqual(block["reason"], "log_missing")
        self.assertFalse(block["log"]["opened"])

    def test_non_python_log_is_unsupported(self):
        block = self.collect({"exit_code": 1}, "a = 1\n", log_text="boom: not a traceback\n")
        self.assertEqual(block["status"], "unsupported")
        self.assertEqual(block["reason"], "no_python_traceback")
        self.assertEqual(block["log"]["format"], "unknown")

    def test_malformed_traceback_is_unknown(self):
        block = self.collect(
            {"exit_code": 1}, "a = 1\nb = 2\n",
            log_text=(
                "Traceback (most recent call last):\n"
                '  File "x.py", line 1, in <module>\n'
            ),
        )
        self.assertEqual(block["status"], "unknown")
        self.assertEqual(block["reason"], "malformed_traceback")

    def test_truncated_log_prefix_is_unknown_not_fake_unsupported(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = write_source(temporary, "a = 1\n")
            log = write_log(temporary, "A" * 200 + "\n" + traceback_log([(source, 1)], "ValueError: x"))
            original = evaluation.MAX_ACCEPTANCE_LOG_BYTES
            evaluation.MAX_ACCEPTANCE_LOG_BYTES = 64
            try:
                block = evaluation.collect_acceptance_evidence({"exit_code": 1}, source, log)
            finally:
                evaluation.MAX_ACCEPTANCE_LOG_BYTES = original
        self.assertEqual(block["status"], "unknown")
        self.assertEqual(block["reason"], "log_truncated")
        self.assertTrue(block["log"]["truncated"])

    def test_unreadable_log_is_unknown(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = write_source(temporary, "a = 1\n")
            log = write_log(temporary, traceback_log([(source, 1)], "ValueError: x"))
            original_open = os.open

            def denied_log(path, flags, *args, **kwargs):
                if Path(path) == log:
                    raise PermissionError("synthetic permission denial")
                return original_open(path, flags, *args, **kwargs)

            with mock.patch.object(evaluation.os, "open", side_effect=denied_log):
                block = evaluation.collect_acceptance_evidence({"exit_code": 1}, source, log)
        self.assertEqual(block["status"], "unknown")
        self.assertEqual(block["reason"], "log_unreadable")

    def test_symlinked_log_is_rejected_as_unknown(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = write_source(temporary, "a = 1\n")
            real = write_log(temporary, traceback_log([(source, 1)], "ValueError: x"), "real.log")
            link = Path(temporary) / "check.log"
            link.symlink_to(real)
            block = evaluation.collect_acceptance_evidence({"exit_code": 1}, source, link)
        self.assertEqual(block["status"], "unknown")
        self.assertEqual(block["reason"], "log_not_regular")

    def test_acceptance_error_and_timeout_are_unknown(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = write_source(temporary)
            log = write_log(temporary, traceback_log([(source, 1)], "ValueError: x"))
            errored = evaluation.collect_acceptance_evidence(
                {"exit_code": None, "timed_out": False, "error": "spawn failed"}, source, log
            )
            timed_out = evaluation.collect_acceptance_evidence(
                {"exit_code": None, "timed_out": True}, source, log
            )
        self.assertEqual((errored["status"], errored["reason"]), ("unknown", "acceptance_error"))
        self.assertEqual((timed_out["status"], timed_out["reason"]), ("unknown", "acceptance_timeout"))

    def test_unreadable_or_missing_acceptance_source_is_unknown(self):
        with tempfile.TemporaryDirectory() as temporary:
            missing = Path(temporary) / "missing.py"
            log = write_log(temporary, "Traceback (most recent call last):\n")
            block = evaluation.collect_acceptance_evidence({"exit_code": 1}, missing, log)
        self.assertEqual(block["status"], "unknown")
        self.assertEqual(block["reason"], "source_missing")
        self.assertIsNone(block["source"]["sha256"])
        self.assertFalse(block["source"]["readable"])
        self.assertNotIn("missing.py", json.dumps(block))

    def test_oversized_source_is_unknown_without_a_partial_hash(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = write_source(temporary, "a = 1\n" * 100)
            log = write_log(temporary, traceback_log([(source, 1)], "ValueError: x"))
            original = evaluation.MAX_ACCEPTANCE_SOURCE_BYTES
            evaluation.MAX_ACCEPTANCE_SOURCE_BYTES = 10
            try:
                block = evaluation.collect_acceptance_evidence({"exit_code": 1}, source, log)
            finally:
                evaluation.MAX_ACCEPTANCE_SOURCE_BYTES = original
        self.assertEqual(block["status"], "unknown")
        self.assertEqual(block["reason"], "source_oversized")
        self.assertIsNone(block["source"]["sha256"])
        self.assertIsNone(block["source"]["lines"])
        self.assertTrue(block["source"]["truncated"])


class BoundsTests(unittest.TestCase):
    def test_giant_frame_number_is_unknown_without_raising(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = write_source(temporary)
            log = write_log(temporary, traceback_log(
                [(source, "9" * 10_000)], "AssertionError: synthetic body"
            ))
            block = evaluation.collect_acceptance_evidence({"exit_code": 1}, source, log)
        self.assertEqual((block["status"], block["reason"]),
                         ("unknown", "malformed_traceback"))
        self.assertEqual(block["frames"]["lines"], [])
        self.assertNotIn("9" * 100, json.dumps(block))

    def test_frame_lines_are_bounded_to_16_and_sorted(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = write_source(temporary, "a = 1\n" * 200)
            entries = [(source, line) for line in range(40, 0, -1)]
            log = write_log(temporary, traceback_log(entries, "ValueError: x"))
            block = evaluation.collect_acceptance_evidence({"exit_code": 1}, source, log)
        self.assertEqual(block["status"], "truncated")
        self.assertEqual(block["frames"]["count"], 16)
        self.assertEqual(block["frames"]["lines"], list(range(1, 17)))
        self.assertTrue(block["frames"]["truncated"])

    def test_out_of_range_frame_lines_are_counted_but_never_reported(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = write_source(temporary, "a = 1\nb = 2\n")
            log = write_log(temporary, traceback_log(
                [(source, 0), (source, 10 ** 9), (source, 2)], "ValueError: x"
            ))
            block = evaluation.collect_acceptance_evidence({"exit_code": 1}, source, log)
        self.assertEqual(block["frames"]["lines"], [2])
        self.assertEqual(block["frames"]["out_of_range"], 2)
        self.assertEqual(block["status"], "collected")

    def test_control_characters_and_oversize_never_reach_the_output(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = write_source(temporary, "a = 1\n")
            log = write_log(temporary, (
                "Traceback (most recent call last):\n"
                f'  File "{source}", line 1, in <module>\n'
                "    pass\n"
                "ValueError: \x00\x07\x1b[31mCONTROL\x1b[0m\n"
            ))
            block = evaluation.collect_acceptance_evidence({"exit_code": 1}, source, log)
        serialized = json.dumps(block)
        self.assertNotIn("\x00", serialized)
        self.assertNotIn("\x07", serialized)
        self.assertNotIn("\x1b", serialized)
        self.assertNotIn("CONTROL", serialized)
        self.assertLessEqual(len(serialized.encode("utf-8")), evaluation.MAX_ACCEPTANCE_EVIDENCE_BYTES)

    def test_serialized_evidence_stays_under_the_byte_cap(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = write_source(temporary, "a = 1\n" * 100)
            entries = [(source, line) for line in range(1, 100)]
            log = write_log(temporary, traceback_log(entries, "ValueError: x"))
            block = evaluation.collect_acceptance_evidence({"exit_code": 1}, source, log)
        serialized = json.dumps(block, separators=(",", ":")).encode("utf-8")
        self.assertLessEqual(len(serialized), evaluation.MAX_ACCEPTANCE_EVIDENCE_BYTES)
        self.assertLess(evaluation.MAX_ACCEPTANCE_EVIDENCE_BYTES, 4096 + 1)

    def test_over_cap_block_is_explicitly_truncated(self):
        oversized = {
            "kind": evaluation.ACCEPTANCE_EVIDENCE_KIND,
            "version": evaluation.ACCEPTANCE_EVIDENCE_VERSION,
            "status": "collected",
            "reason": None,
            "trust": "untrusted_reported_check_log",
            "source": {"readable": True, "reason": None, "sha256": "0" * 64,
                       "lines": 10 ** 6, "truncated": False},
            "log": {"opened": True, "bytes_read": 10 ** 6, "size": 10 ** 6,
                    "truncated": False, "format": "python_traceback"},
            "exception_kind": "ValueError",
            "frames": {"lines": list(range(1, 4097)), "count": 4096,
                       "truncated": False, "out_of_range": 0},
            "note": "n",
        }
        trimmed = evaluation._acceptance_evidence_within_limit(oversized)
        self.assertEqual(trimmed["status"], "truncated")
        self.assertEqual(trimmed["reason"], "output_truncated")
        self.assertEqual(trimmed["frames"]["lines"], [])
        self.assertTrue(trimmed["frames"]["truncated"])
        self.assertEqual(trimmed["exception_kind"], "ValueError")


class NonRegularOpenTests(unittest.TestCase):
    def test_non_regular_file_is_rejected_before_potentially_blocking_open(self):
        fifo = os.stat_result((stat.S_IFIFO, 0, 0, 0, 0, 0, 0, 0, 0, 0))
        with mock.patch.object(evaluation.os, "lstat", return_value=fifo), \
                mock.patch.object(evaluation.os, "open", side_effect=AssertionError(
                    "opening a FIFO may block before fstat can inspect it"
                )) as opened:
            result = evaluation._read_bounded_regular(Path("synthetic.fifo"), 1024)
        self.assertEqual(result[3], "not_regular")
        opened.assert_not_called()

    def test_file_replaced_after_precheck_is_opened_nonblocking_then_rejected(self):
        regular = os.stat_result((stat.S_IFREG, 0, 0, 0, 0, 0, 0, 0, 0, 0))
        fifo = os.stat_result((stat.S_IFIFO, 0, 0, 0, 0, 0, 0, 0, 0, 0))
        nonblock = getattr(os, "O_NONBLOCK", 0) or 0x800

        def raced_open(_path, flags):
            self.assertTrue(flags & nonblock, "a replacement FIFO must not block open")
            return 123

        with mock.patch.object(evaluation.os, "lstat", return_value=regular), \
                mock.patch.object(evaluation.os, "O_NONBLOCK", nonblock, create=True), \
                mock.patch.object(evaluation.os, "open", side_effect=raced_open), \
                mock.patch.object(evaluation.os, "fstat", return_value=fifo), \
                mock.patch.object(evaluation.os, "close") as closed:
            result = evaluation._read_bounded_regular(Path("raced.fifo"), 1024)
        self.assertEqual(result[3], "not_regular")
        closed.assert_called_once_with(123)


class OptInIntegrationTests(unittest.TestCase):
    def synthetic_task(self, root, source):
        task_dir = Path(root) / "synthetic-task"
        task_dir.mkdir(parents=True, exist_ok=True)
        acceptance = task_dir / "acceptance.py"
        acceptance.write_text(source, encoding="utf-8")
        return {
            "id": "synthetic-acceptance",
            "task_set": "probe-set",
            "task_set_version": 1,
            "prompt": "synthetic",
            "files": {"module.py": "value = 1\n"},
            "acceptance": acceptance,
            "acceptance_timeout_seconds": 10,
        }

    def test_default_run_adds_no_key_and_keeps_the_baseline_result_shape(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            task = self.synthetic_task(root, "import module\nassert module.value == 2\n")
            result = evaluation.evaluate(task, NO_OP_AGENT, 10, root / "runs")
        self.assertNotIn("acceptance_evidence", result)
        self.assertEqual(set(result), DEFAULT_RESULT_KEYS)

    def test_default_run_never_invokes_the_collector(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            task = self.synthetic_task(root, "import module\nassert module.value == 2\n")
            with mock.patch.object(
                evaluation, "collect_acceptance_evidence",
                side_effect=AssertionError("collector must not run without opt-in"),
            ):
                result = evaluation.evaluate(task, NO_OP_AGENT, 10, root / "runs")
        self.assertNotIn("acceptance_evidence", result)

    def test_opt_in_does_not_mutate_the_agent_command_template(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            task = self.synthetic_task(root, "import module\nassert module.value == 2\n")
            template = [sys.executable, "-c", "pass", "{prompt}"]
            before = list(template)
            evaluation.evaluate(
                task, template, 10, root / "runs", evaluation.MIO_JSONL,
                diagnostics_dir=root / "diag",
            )
        self.assertEqual(template, before)

    def test_opt_in_records_location_and_preserves_classification(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            task = self.synthetic_task(
                root, "import module\nassert module.value == 2, 'PRIVATE ASSERTION MESSAGE'\n"
            )
            plain = evaluation.evaluate(task, NO_OP_AGENT, 10, root / "plain")
            result = evaluation.evaluate(
                task, NO_OP_AGENT, 10, root / "runs", evaluation.MIO_JSONL,
                diagnostics_dir=root / "diag",
            )
            self.assertTrue((root / "diag" / "synthetic-acceptance.json").is_file())
        evidence = result["acceptance_evidence"]
        self.assertEqual(evidence["status"], "collected")
        self.assertEqual(evidence["exception_kind"], "AssertionError")
        self.assertEqual(evidence["frames"]["lines"], [2])
        self.assertEqual(evidence["source"]["lines"], 2)
        # Classification, agent and acceptance outcomes are untouched.
        for key in ("status", "failure_class", "accepted"):
            self.assertEqual(result[key], plain[key], key)
        for key in ("exit_code", "timed_out"):
            self.assertEqual(result["agent"][key], plain["agent"][key], "agent." + key)
            self.assertEqual(result["acceptance"][key], plain["acceptance"][key],
                             "acceptance." + key)
        self.assertEqual(result["status"], "failed")
        self.assertEqual(result["failure_class"], "acceptance_failed")
        self.assertEqual(result["agent"]["exit_code"], 0)
        self.assertEqual(result["acceptance"]["exit_code"], 1)
        self.assertFalse(result["acceptance"]["timed_out"])
        # No raw body, path or message survives anywhere in the result.
        serialized = json.dumps(result)
        for forbidden in ("PRIVATE ASSERTION MESSAGE", str(task["acceptance"]),
                          "synthetic-task", "check.log"):
            self.assertNotIn(forbidden, serialized)

    def test_opt_in_is_the_only_trigger_for_the_field(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            task = self.synthetic_task(root, "import module\nassert module.value == 2\n")
            for label, diagnostics_dir in (("off", None), ("on", root / "diag")):
                with self.subTest(diagnostics_dir=label):
                    result = evaluation.evaluate(
                        task, NO_OP_AGENT, 10, root / ("runs-" + label),
                        evaluation.MIO_JSONL, diagnostics_dir=diagnostics_dir,
                    )
                    self.assertEqual(
                        "acceptance_evidence" in result, diagnostics_dir is not None
                    )

    def test_opt_in_evidence_serializes_under_the_cap(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            task = self.synthetic_task(
                root, "import module\nassert module.value == 2\n" + "x = 1\n" * 50
            )
            result = evaluation.evaluate(
                task, NO_OP_AGENT, 10, root / "runs", evaluation.MIO_JSONL,
                diagnostics_dir=root / "diag",
            )
        encoded = json.dumps(result["acceptance_evidence"], separators=(",", ":")).encode()
        self.assertLessEqual(len(encoded), evaluation.MAX_ACCEPTANCE_EVIDENCE_BYTES)


class BoundsConstantsTests(unittest.TestCase):
    def test_public_bounds_are_finite_and_bounded(self):
        self.assertLessEqual(evaluation.MAX_ACCEPTANCE_FRAMES, 16)
        self.assertGreater(evaluation.MAX_ACCEPTANCE_FRAMES, 0)
        self.assertLess(evaluation.MAX_ACCEPTANCE_EVIDENCE_BYTES, 4096 + 1)
        self.assertGreater(evaluation.MAX_ACCEPTANCE_LOG_BYTES, 0)
        self.assertGreater(evaluation.MAX_ACCEPTANCE_SOURCE_BYTES, 0)
        self.assertIn("AssertionError", evaluation.ACCEPTANCE_EXCEPTION_KINDS)
        self.assertIn("ValueError", evaluation.ACCEPTANCE_EXCEPTION_KINDS)

    def test_collector_does_not_read_other_files(self):
        # The collector's own module only exposes fixed labels; it never imports or
        # executes the acceptance code (a source with a side effect must not run).
        with tempfile.TemporaryDirectory() as temporary:
            marker = Path(temporary) / "ran.marker"
            source = write_source(
                temporary,
                f"open({str(marker)!r}, 'w').write('ran')\nassert False\n",
            )
            log = write_log(temporary, traceback_log([(source, 2)], "AssertionError: x"))
            evaluation.collect_acceptance_evidence({"exit_code": 1}, source, log)
            self.assertFalse(marker.exists())


if __name__ == "__main__":
    unittest.main()
