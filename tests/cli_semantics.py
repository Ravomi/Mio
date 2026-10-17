#!/usr/bin/env python3
"""Self-test for scripts/check-cli-semantics.py.

Uses a small real on-disk fixture (a mini parser, public declarations and test
functions) plus mutation tests. It also runs the checker against the real
repository and records the actual flag/entry counts and the strict
complete/incomplete status. The real tree must pass the strict checker without
--allow-incomplete; the fixture keeps the negative controls (incomplete entry,
unclassified flag, private/restricted API target) so the gate cannot go green
by accident.

Every mutation must make the checker fail for the intended reason; the fixture
baseline must pass. This tests the checker logic rather than mirroring the real
implementation, so it cannot go green by construction.
"""

from __future__ import annotations

import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CHECKER = ROOT / "scripts" / "check-cli-semantics.py"

FIVE_REASONS = {
    "terminal_rendering": "fixture reason",
    "approval_interaction": "fixture reason",
    "persistence_defaults": "fixture reason",
    "host_configuration": "fixture reason",
    "maintenance": "fixture reason",
}

ARGS_RS = """//! Fixture CLI argument parser.
pub(super) struct Args {
    pub(super) alpha: Option<String>,
    pub(super) beta: bool,
    pub(super) gamma: Vec<String>,
}

pub(super) fn parse(args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args::default();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--alpha" => parsed.alpha = Some(arg.clone()),
            "--beta" => parsed.beta = true,
            "--gamma" => parsed.gamma.push(arg.clone()),
            "--" => { break; }
            _ => return Err("unknown option".into()),
        }
    }
    Ok(parsed)
}
"""

LIB_RS = """pub struct RunOptions {
    pub max_turns: usize,
}

pub fn run_with_options() {}

pub(super) struct HiddenOptions {
    pub(super) secret: usize,
}
"""

BEHAVIOUR_RS = """#[test]
fn alpha_reaches_engine() {}

#[test]
fn beta_reaches_engine() {}
"""


def base_manifest() -> dict:
    return {
        "manifestVersion": 1,
        "design": "DES-03",
        "flagSource": "src/bin/mio_cli/args.rs",
        "allowedDifferenceReasons": dict(FIVE_REASONS),
        "semanticsCriticalFlags": [],
        "entries": [
            {
                "name": "--alpha",
                "aliases": ["--alpha"],
                "classification": "engine_mapping",
                "argsField": "alpha",
                "wiring": [{"file": "src/bin/mio_cli/args.rs", "contains": "parsed.alpha"}],
                "publicApi": [{"file": "src/lib.rs", "symbol": "RunOptions"}],
                "behaviorTests": [{"file": "tests/behaviour.rs", "name": "alpha_reaches_engine"}],
                "evidenceBoundary": "Static fixture: declarations exist; behavior is not proven here.",
            },
            {
                "name": "--beta",
                "aliases": ["--beta"],
                "classification": "engine_mapping",
                "argsField": "beta",
                "wiring": [{"file": "src/bin/mio_cli/args.rs", "contains": "parsed.beta"}],
                "publicApi": [{"file": "src/lib.rs", "symbol": "run_with_options"}],
                "behaviorTests": [{"file": "tests/behaviour.rs", "name": "beta_reaches_engine"}],
                "evidenceBoundary": "Static fixture: declarations exist; behavior is not proven here.",
            },
            {
                "name": "--gamma",
                "aliases": ["--gamma"],
                "classification": "allowed_difference",
                "reason": "maintenance",
                "reasonNote": "Fixture-only standalone command outside the run loop.",
            },
            {
                "name": "--",
                "aliases": ["--"],
                "classification": "allowed_difference",
                "reason": "maintenance",
                "reasonNote": "Fixture-only option terminator feeding the task prompt.",
            },
            {
                "name": "-h",
                "aliases": ["-h", "--help"],
                "classification": "allowed_difference",
                "reason": "maintenance",
                "reasonNote": "Fixture-only help command printed before the engine.",
            },
            {
                "name": "-V",
                "aliases": ["-V", "--version"],
                "classification": "allowed_difference",
                "reason": "maintenance",
                "reasonNote": "Fixture-only version command printed before the engine.",
            },
        ],
    }


class Fixture:
    def __init__(self) -> None:
        self.dir = Path(tempfile.mkdtemp(prefix="cli-semantics-fixture-"))
        (self.dir / "src" / "bin" / "mio_cli").mkdir(parents=True)
        (self.dir / "tests").mkdir()
        (self.dir / "docs").mkdir()
        (self.dir / "src" / "bin" / "mio_cli" / "args.rs").write_text(ARGS_RS, encoding="utf-8")
        (self.dir / "src" / "lib.rs").write_text(LIB_RS, encoding="utf-8")
        (self.dir / "tests" / "behaviour.rs").write_text(BEHAVIOUR_RS, encoding="utf-8")
        self.write_manifest(base_manifest())

    def write_manifest(self, manifest: dict) -> None:
        (self.dir / "docs" / "cli-sdk-semantics.json").write_text(
            json.dumps(manifest, indent=2), encoding="utf-8"
        )

    def write_raw_manifest(self, raw: str) -> None:
        (self.dir / "docs" / "cli-sdk-semantics.json").write_text(raw, encoding="utf-8")

    def manifest(self) -> dict:
        return json.loads((self.dir / "docs" / "cli-sdk-semantics.json").read_text(encoding="utf-8"))

    def run(self, *extra: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [
                sys.executable,
                str(CHECKER),
                "--root",
                str(self.dir),
                "--manifest",
                "docs/cli-sdk-semantics.json",
                *extra,
            ],
            capture_output=True,
            text=True,
            check=False,
        )

    def cleanup(self) -> None:
        shutil.rmtree(self.dir, ignore_errors=True)


def reported_errors(result: subprocess.CompletedProcess) -> list[str]:
    report = json.loads(result.stdout)
    return report["errors"]


class CheckerSelfTest(unittest.TestCase):
    def setUp(self) -> None:
        self.fixture = Fixture()

    def tearDown(self) -> None:
        self.fixture.cleanup()

    # ---- baseline fixture -------------------------------------------------------
    def test_fixture_baseline_passes(self) -> None:
        result = self.fixture.run("--json", "--quiet")
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        report = json.loads(result.stdout)
        self.assertEqual(report["status"], "pass")
        self.assertEqual(report["flagCount"], 8)
        self.assertEqual(report["entryCount"], 6)

    # ---- required mutations -----------------------------------------------------
    def test_new_unclassified_flag_fails(self) -> None:
        args = self.fixture.dir / "src" / "bin" / "mio_cli" / "args.rs"
        args.write_text(ARGS_RS.replace('"--beta" =>', '"--delta" => parsed.beta = true, "--beta" =>'), encoding="utf-8")
        result = self.fixture.run("--json", "--quiet")
        self.assertEqual(result.returncode, 1)
        self.assertTrue(any("unclassified flag: --delta" in e for e in reported_errors(result)), result.stdout)

    def test_missing_classification_fails(self) -> None:
        manifest = self.fixture.manifest()
        manifest["entries"] = [e for e in manifest["entries"] if e["name"] != "--alpha"]
        self.fixture.write_manifest(manifest)
        result = self.fixture.run("--json", "--quiet")
        self.assertEqual(result.returncode, 1)
        self.assertTrue(any("unclassified flag: --alpha" in e for e in reported_errors(result)), result.stdout)

    def test_duplicate_alias_fails(self) -> None:
        manifest = self.fixture.manifest()
        for entry in manifest["entries"]:
            if entry["name"] == "--gamma":
                entry["aliases"] = ["--gamma", "--alpha"]
        self.fixture.write_manifest(manifest)
        result = self.fixture.run("--json", "--quiet")
        self.assertEqual(result.returncode, 1)
        self.assertTrue(any("duplicate alias --alpha" in e for e in reported_errors(result)), result.stdout)

    def test_residual_obsolete_flag_fails(self) -> None:
        manifest = self.fixture.manifest()
        manifest["entries"][2]["aliases"] = ["--gamma", "--obsolete"]
        self.fixture.write_manifest(manifest)
        result = self.fixture.run("--json", "--quiet")
        self.assertEqual(result.returncode, 1)
        self.assertTrue(any("residual/unknown flag" in e and "--obsolete" in e for e in reported_errors(result)), result.stdout)

    def test_unknown_reason_fails(self) -> None:
        manifest = self.fixture.manifest()
        for entry in manifest["entries"]:
            if entry["name"] == "--gamma":
                entry["reason"] = "bogus_reason"
        self.fixture.write_manifest(manifest)
        result = self.fixture.run("--json", "--quiet")
        self.assertEqual(result.returncode, 1)
        self.assertTrue(any("unknown allowed_difference reason" in e for e in reported_errors(result)), result.stdout)

    def test_nonexistent_public_target_fails(self) -> None:
        manifest = self.fixture.manifest()
        manifest["entries"][0]["publicApi"] = [{"file": "src/lib.rs", "symbol": "DoesNotExist"}]
        self.fixture.write_manifest(manifest)
        result = self.fixture.run("--json", "--quiet")
        self.assertEqual(result.returncode, 1)
        self.assertTrue(any("no public declaration" in e for e in reported_errors(result)), result.stdout)

    def test_private_public_target_fails(self) -> None:
        manifest = self.fixture.manifest()
        manifest["entries"][0]["publicApi"] = [{"file": "src/lib.rs", "symbol": "HiddenOptions"}]
        self.fixture.write_manifest(manifest)
        result = self.fixture.run("--json", "--quiet")
        self.assertEqual(result.returncode, 1)
        self.assertTrue(any("restricted visibility" in e for e in reported_errors(result)), result.stdout)

    def test_private_related_public_api_fails(self) -> None:
        manifest = self.fixture.manifest()
        for entry in manifest["entries"]:
            if entry["name"] == "--gamma":
                entry["relatedPublicApi"] = [{"file": "src/lib.rs", "symbol": "HiddenOptions"}]
        self.fixture.write_manifest(manifest)
        result = self.fixture.run("--json", "--quiet")
        self.assertEqual(result.returncode, 1)
        self.assertTrue(any("restricted visibility" in e for e in reported_errors(result)), result.stdout)

    def test_deleted_behavior_test_fails(self) -> None:
        manifest = self.fixture.manifest()
        manifest["entries"][0]["behaviorTests"] = [
            {"file": "tests/behaviour.rs", "name": "deleted_test_name"}
        ]
        self.fixture.write_manifest(manifest)
        result = self.fixture.run("--json", "--quiet")
        self.assertEqual(result.returncode, 1)
        self.assertTrue(any("no definition in tests/behaviour.rs" in e for e in reported_errors(result)), result.stdout)

    def test_corrupt_json_fails(self) -> None:
        self.fixture.write_raw_manifest('{"manifestVersion": 1, "entries": [}')
        result = self.fixture.run("--json", "--quiet")
        self.assertEqual(result.returncode, 1)
        self.assertTrue(any("not valid JSON" in e for e in reported_errors(result)), result.stdout)

    def test_out_of_bounds_source_path_fails(self) -> None:
        manifest = self.fixture.manifest()
        manifest["entries"][0]["wiring"] = [{"file": "../../etc/passwd", "contains": "root"}]
        self.fixture.write_manifest(manifest)
        result = self.fixture.run("--json", "--quiet")
        self.assertEqual(result.returncode, 1)
        self.assertTrue(any("escapes the repository root" in e for e in reported_errors(result)), result.stdout)

    # ---- incompleteness lifecycle ----------------------------------------------
    def test_incomplete_entry_strict_exit1_allow_exit3(self) -> None:
        manifest = self.fixture.manifest()
        manifest["entries"][0]["status"] = "incomplete"
        manifest["entries"][0]["missingTargets"] = [
            {"file": "src/lib.rs", "kind": "mod", "symbol": "task_lifecycle",
             "note": "fixture gap"},
        ]
        self.fixture.write_manifest(manifest)
        strict = self.fixture.run("--json", "--quiet")
        self.assertEqual(strict.returncode, 1)
        self.assertTrue(any("strict mode" in e for e in reported_errors(strict)), strict.stdout)
        allowed = self.fixture.run("--allow-incomplete", "--json", "--quiet")
        self.assertEqual(allowed.returncode, 3)
        self.assertEqual(json.loads(allowed.stdout)["status"], "incomplete")

    def test_stale_incomplete_declaration_fails(self) -> None:
        manifest = self.fixture.manifest()
        manifest["entries"][0]["status"] = "incomplete"
        manifest["entries"][0]["missingTargets"] = [
            {"file": "src/lib.rs", "kind": "symbol", "symbol": "RunOptions",
             "note": "but it is actually present"},
        ]
        self.fixture.write_manifest(manifest)
        result = self.fixture.run("--allow-incomplete", "--json", "--quiet")
        self.assertEqual(result.returncode, 1)
        self.assertTrue(any("stale incomplete declaration" in e for e in reported_errors(result)), result.stdout)

    def test_critical_flag_cannot_be_rendering_exemption(self) -> None:
        args = self.fixture.dir / "src" / "bin" / "mio_cli" / "args.rs"
        args.write_text(ARGS_RS.replace('"--beta" =>', '"--no-shell" => parsed.beta = true, "--beta" =>'), encoding="utf-8")
        manifest = self.fixture.manifest()
        manifest["semanticsCriticalFlags"] = ["--no-shell"]
        manifest["entries"].append(
            {
                "name": "--no-shell",
                "aliases": ["--no-shell"],
                "classification": "allowed_difference",
                "reason": "terminal_rendering",
                "reasonNote": "Deliberately wrong fixture classification.",
            }
        )
        self.fixture.write_manifest(manifest)
        result = self.fixture.run("--json", "--quiet")
        self.assertEqual(result.returncode, 1)
        self.assertTrue(any("must be engine_mapping" in e for e in reported_errors(result)), result.stdout)


class RealRepositoryCheck(unittest.TestCase):
    """Runs the checker on the real tree and records actual counts."""

    def run_checker(self, *extra: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [sys.executable, str(CHECKER), *extra],
            capture_output=True,
            text=True,
            check=False,
        )

    def test_real_repo_strict_passes(self) -> None:
        result = self.run_checker("--json", "--quiet")
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        report = json.loads(result.stdout)
        self.assertEqual(report["status"], "pass")
        self.assertEqual(report["flagCount"], 83)
        self.assertEqual(report["entryCount"], 73)
        self.assertEqual(report["checks"]["flagCoverage"], True)
        self.assertEqual(report["incompleteEntries"], [])
        self.assertEqual(report["classification"]["engine_mapping"], 45)
        self.assertEqual(report["classification"]["allowed_difference"], 28)

    def test_real_repo_task_entry_is_complete_and_new_flags_are_mapped(self) -> None:
        manifest = json.loads(
            (ROOT / "docs" / "cli-sdk-semantics.json").read_text(encoding="utf-8")
        )
        entries = {entry["name"]: entry for entry in manifest["entries"]}
        subagents = entries["--subagents"]
        self.assertEqual(subagents["classification"], "engine_mapping")
        self.assertNotIn("missingTargets", subagents)
        self.assertNotEqual(subagents.get("status", "complete"), "incomplete")
        for flag in ("--list-unresolved-effects", "--ack-unresolved-effect"):
            entry = entries[flag]
            self.assertEqual(entry["classification"], "allowed_difference")
            self.assertEqual(entry["reason"], "maintenance")
            self.assertTrue(entry["relatedPublicApi"])

    def test_real_repo_needs_no_allow_incomplete(self) -> None:
        # The real tree must pass strictly and must not rely on --allow-incomplete
        # to turn a failing strict gate into a pass.
        strict = self.run_checker("--json", "--quiet")
        lenient = self.run_checker("--allow-incomplete", "--json", "--quiet")
        self.assertEqual(strict.returncode, 0, strict.stderr + strict.stdout)
        self.assertEqual(lenient.returncode, 0, lenient.stderr + lenient.stdout)
        strict_report = json.loads(strict.stdout)
        lenient_report = json.loads(lenient.stdout)
        self.assertEqual(strict_report["status"], "pass")
        self.assertEqual(lenient_report["status"], "pass")
        self.assertEqual(lenient_report["incompleteEntries"], [])

    def test_real_repo_report_has_no_environment_leak(self) -> None:
        result = self.run_checker("--json", "--quiet")
        report = json.loads(result.stdout)
        serialized = json.dumps(report)
        for marker in ("API_KEY", "TOKEN", "SECRET", "PASSWORD"):
            self.assertNotIn(marker, serialized)


if __name__ == "__main__":
    unittest.main(verbosity=2)
