#!/usr/bin/env python3
"""Unit tests for scripts/check-ravomi-host.py.

These tests deliberately stay off the heavy path: they never run cargo, never
compile the Rust probe, never start a Mio process and never touch the real
Ravomi checkout. What they do verify are the properties that would let a broken
implementation look green by accident:

* the required scenario manifest exists and covers the four minimum scenarios;
* argument parsing and the sanitized child environment behave as documented;
* the Rust probe source embeds the *real* absolute host path via `#[path]` and
  drives `Osc7501Scanner::scan` in chunks instead of re-implementing a parser;
* the probe output parser reads real event lines;
* `validate_report` cannot certify an incomplete report as `pass`;
* a missing host source is a hard failure, never a skip-as-pass;
* `--prepare-only` writes a plan bound to real file hashes and does not build;
* the bounded process runner actually kills a hung child.

Run with:  PYTHONDONTWRITEBYTECODE=1 python3 tests/ravomi_host_contract.py
"""

from __future__ import annotations

import contextlib
import hashlib
import importlib.util
import io
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

sys.dont_write_bytecode = True

ROOT = pathlib.Path(__file__).resolve().parent.parent
SCRIPT_PATH = ROOT / "scripts" / "check-ravomi-host.py"


def load_script_module():
    spec = importlib.util.spec_from_file_location("check_ravomi_host_under_test", SCRIPT_PATH)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


MOD = load_script_module()


def make_pass_report():
    """A structurally valid passing report, written independently of the script."""
    return {
        "schema": MOD.SCHEMA_ID,
        "status": "pass",
        "mode": "run",
        "generated_at": "2026-01-01T00:00:00Z",
        "artifacts": {
            "host_source": {"path": "/host/session_feed.rs", "present": True, "sha256": "aa"},
            "mio_binary": {"path": "/bin/mio", "present": True, "sha256": "bb"},
        },
        "toolchain": {"rust_version": "rustc 1.95.0 (59807616e 2026-04-14)"},
        "limits": {},
        "scope": {"not_claimed": ["full GUI"]},
        "counts": {
            "total": len(MOD.ALL_SCENARIOS),
            "passed": len(MOD.ALL_SCENARIOS),
            "failed": 0,
            "required_passed": True,
            "minimum_passed": True,
        },
        "scenarios": [{"id": scenario, "status": "pass"} for scenario in MOD.ALL_SCENARIOS],
    }


class ManifestTests(unittest.TestCase):
    def test_minimum_scenarios_present(self):
        manifest = MOD.build_scenario_manifest()
        ids = [item["id"] for item in manifest]
        self.assertEqual(set(MOD.REQUIRED_SCENARIOS), {"success", "error", "approval", "fragment"})
        for scenario in MOD.MINIMUM_SCENARIOS:
            self.assertIn(scenario, ids)
        self.assertEqual(set(ids), set(MOD.ALL_SCENARIOS))
        self.assertTrue(all(item["required"] for item in manifest))

    def test_no_scenario_resembles_a_skip(self):
        manifest = MOD.build_scenario_manifest()
        self.assertEqual(len(manifest), len(MOD.ALL_SCENARIOS))
        self.assertEqual(len({item["id"] for item in manifest}), len(manifest))


class ArgumentTests(unittest.TestCase):
    def test_roundtrip(self):
        args = MOD.parse_args(
            [
                "--ravomi-root",
                "/tmp/ravomi",
                "--mio-binary",
                "/tmp/mio",
                "--report",
                "/tmp/report.json",
                "--chunk-sizes",
                "1,3",
                "--timeout",
                "12.5",
            ]
        )
        self.assertEqual(args.ravomi_root, pathlib.Path("/tmp/ravomi"))
        self.assertEqual(args.mio_binary, pathlib.Path("/tmp/mio"))
        self.assertEqual(args.report, pathlib.Path("/tmp/report.json"))
        self.assertEqual(args.chunk_sizes, (1, 3))
        self.assertEqual(args.timeout, 12.5)
        self.assertEqual(args.toolchain, MOD.DEFAULT_TOOLCHAIN)

    def test_binary_required_unless_prepare_only(self):
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit):
                MOD.parse_args(["--ravomi-root", "/tmp/ravomi"])
        args = MOD.parse_args(["--ravomi-root", "/tmp/ravomi", "--prepare-only"])
        self.assertTrue(args.prepare_only)
        self.assertIsNone(args.mio_binary)

    def test_bad_chunk_sizes_rejected(self):
        for value in ("0,1", "-2", "abc", ""):
            with contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit):
                    MOD.parse_args(["--ravomi-root", "/tmp/ravomi", "--prepare-only", "--chunk-sizes", value])


class EnvPolicyTests(unittest.TestCase):
    def test_environment_is_built_from_empty_dict(self):
        secret = "super-secret-key-value"
        with tempfile.TemporaryDirectory() as home:
            poisoned = {
                "OPENAI_API_KEY": secret,
                "HTTPS_PROXY": "http://proxy.invalid:8080",
                "MIO_MODEL": "leaked-model",
                "MIO_TOKEN": secret,
                "PATH": "/usr/bin:/bin",
            }
            with mock.patch.dict(os.environ, poisoned, clear=True):
                env = MOD.build_clean_env(home)
        self.assertEqual(env["HOME"], home)
        self.assertEqual(env[MOD.CREDENTIAL_ENV], MOD.LOCAL_TEST_CREDENTIAL)
        for banned in poisoned:
            if banned == "PATH":
                continue
            self.assertNotIn(banned, env)
        self.assertNotIn(secret, env.values())
        self.assertNotIn("MIO_TOKEN", env)
        self.assertNotIn("HTTPS_PROXY", env)
        self.assertEqual(env["PATH"], "/usr/local/bin:/usr/bin:/bin")

    def test_env_policy_documents_stripping(self):
        policy = MOD.env_policy()
        self.assertTrue(policy["cleared"])
        self.assertIn("MIO_", policy["stripped_prefixes"])
        self.assertIn("https_proxy", policy["stripped_proxy_names"])


class ProbeSourceTests(unittest.TestCase):
    def test_probe_embeds_real_host_path_and_scanner(self):
        host = pathlib.Path("/real/ravomi/crates/ravomi-makepad/src/session_feed.rs")
        source = MOD.build_probe_source(host)
        self.assertIn('#[path = "/real/ravomi/crates/ravomi-makepad/src/session_feed.rs"]', source)
        self.assertIn("mod session_feed;", source)
        self.assertIn("Osc7501Scanner::new()", source)
        self.assertIn("scanner.scan(&data[index..end])", source)
        self.assertIn('"--chunk"', source)
        # No local re-implementation of the parser.
        self.assertNotIn("fn parse_osc_7501", source)
        self.assertNotIn("fn sanitize_text", source)

    def test_probe_path_is_escaped(self):
        host = pathlib.Path('/odd/"quoted"/session_feed.rs')
        source = MOD.build_probe_source(host)
        self.assertIn('#[path = "/odd/\\"quoted\\"/session_feed.rs"]', source)

    def test_rust_string_literal(self):
        self.assertEqual(MOD.rust_string_literal('a"b\\c'), '"a\\"b\\\\c"')
        self.assertEqual(MOD.rust_string_literal("plain"), '"plain"')


class ProbeParserTests(unittest.TestCase):
    def test_parses_events_and_summary(self):
        text = "\n".join(
            [
                "EVENT state=working kind=- app=b64:bWlv id=- title=- msg=- progress=5",
                "EVENT state=blocked kind=permission app=b64:bWlv id=b64:YQ== title=b64:aGk= msg=- progress=45",
                "PROBE_SUMMARY events=2 bytes=99 chunk=1",
                "",
            ]
        )
        parsed = MOD.parse_probe_output(text)
        self.assertEqual(parsed["summary"], {"events": 2, "bytes": 99, "chunk": 1})
        self.assertEqual(len(parsed["events"]), 2)
        first, second = parsed["events"]
        self.assertEqual((first["state"], first["app"], first["progress"]), ("working", "mio", 5))
        self.assertIsNone(first["kind"])
        self.assertEqual(second["state"], "blocked")
        self.assertEqual(second["kind"], "permission")
        self.assertEqual((second["id"], second["title"]), ("a", "hi"))

    def test_empty_output_is_zero_events(self):
        parsed = MOD.parse_probe_output("")
        self.assertEqual(parsed["events"], [])
        self.assertIsNone(parsed["summary"])


class ValidateReportTests(unittest.TestCase):
    def test_valid_pass_report_has_no_errors(self):
        self.assertEqual(MOD.validate_report(make_pass_report()), [])

    def test_pass_without_required_counts_fails(self):
        report = make_pass_report()
        report["counts"]["required_passed"] = False
        errors = MOD.validate_report(report)
        self.assertTrue(any("required_passed" in error for error in errors), errors)

    def test_pass_without_scenarios_fails(self):
        report = make_pass_report()
        report["scenarios"] = []
        errors = MOD.validate_report(report)
        self.assertTrue(any("scenario" in error for error in errors), errors)

    def test_missing_host_source_never_passes(self):
        report = make_pass_report()
        report["artifacts"]["host_source"] = {"path": "/host", "present": False}
        errors = MOD.validate_report(report)
        self.assertTrue(any("host source" in error for error in errors), errors)

    def test_pass_without_binary_hash_fails(self):
        report = make_pass_report()
        report["artifacts"]["mio_binary"] = {"path": "/bin/mio", "present": True}
        errors = MOD.validate_report(report)
        self.assertTrue(any("mio binary" in error for error in errors), errors)

    def test_pass_without_rust_version_fails(self):
        report = make_pass_report()
        report["toolchain"] = {"rust_version": None}
        errors = MOD.validate_report(report)
        self.assertTrue(any("rust_version" in error for error in errors), errors)

    def test_non_pass_status_allowed_without_counts(self):
        report = {
            "schema": MOD.SCHEMA_ID,
            "status": "not_run",
            "mode": "prepare_only",
            "generated_at": "2026-01-01T00:00:00Z",
            "artifacts": {"host_source": {"path": "/h", "present": True, "sha256": "aa"}},
            "toolchain": {},
            "limits": {},
            "scope": {"not_claimed": ["x"]},
        }
        self.assertEqual(MOD.validate_report(report), [])


class FragmentWiringTests(unittest.TestCase):
    """Regression: the fragment scenario must read chunk sizes from ctx["limits"]."""

    @staticmethod
    def _event(state, progress=None, kind=None):
        return {
            "state": state,
            "kind": kind,
            "app": None,
            "id": None,
            "title": None,
            "message": None,
            "progress": progress,
        }

    def test_fragment_reads_chunk_sizes_from_limits(self):
        calls = []

        def fake_run_probe(ctx, label, raw_bytes, chunk):
            calls.append((label, chunk))
            if label == "fragment-synthetic":
                events = [self._event("working", progress=5), self._event("done")]
            elif label == "fragment-lone-esc":
                events = [self._event("idle")]
            else:
                events = [self._event("working"), self._event("done")]
            return {
                "chunk": chunk,
                "raw_sha256": "00",
                "raw_bytes": len(raw_bytes),
                "probe_returncode": 0,
                "probe_timed_out": False,
                "probe_duration_s": 0.0,
                "probe_stderr": "",
                "events": events,
                "summary": {"events": len(events), "bytes": len(raw_bytes), "chunk": chunk},
                "other": [],
            }

        ctx = {"limits": {"chunk_sizes": [1, 7]}}
        with mock.patch.object(MOD, "run_probe", fake_run_probe):
            scenario, _raw = MOD.scenario_fragment(
                ctx, None, b"\x1b]7501;state=working\x1b\\\x1b]7501;state=done\x1b\\"
            )
        self.assertEqual(scenario["status"], "pass", scenario["checks"])
        self.assertEqual(scenario["chunk_sizes"], [1, 7])
        self.assertEqual(calls[0], ("fragment", 1))
        self.assertEqual(calls[1], ("fragment", 7))


class ProbeEvidenceTests(unittest.TestCase):
    def test_reused_probe_is_not_marked_compiled_this_run(self):
        reused = MOD.probe_evidence(True, pathlib.Path("/host/session_feed.rs"), "aa", [])
        self.assertFalse(reused["host_source_compiled_this_run"])
        self.assertEqual(reused["probe_origin"], "reused_prebuilt_probe")
        self.assertTrue(reused["boundary"])
        compiled = MOD.probe_evidence(
            False, pathlib.Path("/host/session_feed.rs"), "aa", [{"name": "serde"}]
        )
        self.assertTrue(compiled["host_source_compiled_this_run"])
        self.assertEqual(compiled["probe_origin"], "compiled_in_this_run")
        self.assertEqual(compiled["rlib_count"], 1)

    def test_reused_pass_requires_boundary(self):
        report = make_pass_report()
        report["probe_evidence"] = {"host_source_compiled_this_run": False, "boundary": ""}
        self.assertTrue(any("boundary" in error for error in MOD.validate_report(report)))
        report["probe_evidence"]["boundary"] = "not compiled in this run"
        self.assertEqual(MOD.validate_report(report), [])


class HelperTests(unittest.TestCase):
    def test_snapshot_stability(self):
        self.assertTrue(MOD.snapshot_stable("aa", "aa"))
        self.assertFalse(MOD.snapshot_stable("aa", "bb"))
        self.assertFalse(MOD.snapshot_stable(None, "aa"))

    def test_describe_file_missing(self):
        info = MOD.describe_file(pathlib.Path("/definitely/not/here/session_feed.rs"))
        self.assertFalse(info["present"])

    def test_guard_report_path_refuses_ravomi_checkout(self):
        ravomi_root = pathlib.Path("/tmp/ravomi-checkout")
        host_source = ravomi_root / MOD.HOST_SOURCE_RELATIVE
        with self.assertRaises(MOD.SetupError):
            MOD.guard_report_path(ravomi_root / "report.json", ravomi_root, host_source)
        # Outside the checkout is fine.
        MOD.guard_report_path(pathlib.Path("/tmp/report.json"), ravomi_root, host_source)

    def test_run_process_timeout_kills_child(self):
        started = MOD.time.monotonic()
        result = MOD.run_process(
            [sys.executable, "-c", "import time; time.sleep(30)"],
            timeout=0.4,
        )
        elapsed = MOD.time.monotonic() - started
        self.assertTrue(result["timed_out"], result)
        self.assertNotEqual(result["returncode"], 0)
        self.assertLess(elapsed, 10.0, "hung child was not reaped promptly")


class PrepareOnlyTests(unittest.TestCase):
    def _tree(self, root: pathlib.Path, with_source: bool):
        ravomi_root = root / "ravomi"
        host = ravomi_root / MOD.HOST_SOURCE_RELATIVE
        if with_source:
            host.parent.mkdir(parents=True, exist_ok=True)
            host.write_text("// stand-in for the real host source\n", encoding="utf-8")
        else:
            ravomi_root.mkdir(parents=True, exist_ok=True)
        mio_binary = root / "mio"
        mio_binary.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        return ravomi_root, host, mio_binary

    def _run(self, ravomi_root, mio_binary, report, extra=None):
        cmd = [
            sys.executable,
            str(SCRIPT_PATH),
            "--ravomi-root",
            str(ravomi_root),
            "--report",
            str(report),
            "--prepare-only",
        ]
        if mio_binary is not None:
            cmd += ["--mio-binary", str(mio_binary)]
        if extra:
            cmd += list(extra)
        return subprocess.run(cmd, capture_output=True, text=True, timeout=180)

    def test_prepare_only_writes_bound_plan_and_does_not_build(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            ravomi_root, host, mio_binary = self._tree(root, with_source=True)
            report_path = root / "report.json"
            completed = self._run(ravomi_root, mio_binary, report_path)
            self.assertEqual(completed.returncode, MOD.EXIT_NOT_RUN, completed.stderr)
            data = json.loads(report_path.read_text(encoding="utf-8"))
            self.assertEqual(data["mode"], "prepare_only")
            self.assertEqual(data["status"], "not_run")
            expected = hashlib.sha256(host.read_bytes()).hexdigest()
            self.assertEqual(data["artifacts"]["host_source"]["sha256"], expected)
            self.assertEqual(data["validation"], [])
            self.assertNotIn("counts", data)
            self.assertTrue(data["scope"]["not_claimed"])
            self.assertIn("--locked", data["planned_build_command"])
            self.assertIn("build", data["planned_build_command"])

    def test_missing_source_is_fail_not_skip(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            ravomi_root, _host, mio_binary = self._tree(root, with_source=False)
            report_path = root / "report.json"
            completed = self._run(ravomi_root, mio_binary, report_path)
            self.assertEqual(completed.returncode, MOD.EXIT_FAIL, completed.stderr)
            data = json.loads(report_path.read_text(encoding="utf-8"))
            self.assertEqual(data["status"], "fail")
            self.assertFalse(data["artifacts"]["host_source"]["present"])
            self.assertTrue(any("host source missing" in reason for reason in data["blocking"]))
            self.assertTrue(data["validation"], "missing source must produce validation errors")


if __name__ == "__main__":
    unittest.main(verbosity=2)
