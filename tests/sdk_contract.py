"""Offline tests for the SDK contract checker (scripts/check-sdk.py).

The negative cases build synthetic resolve graphs with reversed dependency
edges and assert that the checker really blocks them, so the gate cannot
silently degrade into echoing the manifest text.
"""
import importlib.util
import json
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts" / "check-sdk.py"
spec = importlib.util.spec_from_file_location("check_sdk", SCRIPT)
check_sdk = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check_sdk)

LAYER_NAMES = list(check_sdk.LAYERS)


def _pid(name, version="0.1.0"):
    return f"path+file:///ws/{name}#{name}@{version}"


def _dep(name, req="=0.1.0", kind=None, path=None, optional=False):
    return {
        "name": name,
        "source": None,
        "req": req,
        "kind": kind,
        "rename": None,
        "optional": optional,
        "uses_default_features": True,
        "features": [],
        "target": None,
        "path": path,
        "registry": None,
    }


def _package(name, version="0.1.0", license="MIT", license_file=None,
             features=None, dependencies=None, publish=None):
    return {
        "name": name,
        "version": version,
        "id": _pid(name, version),
        "license": license,
        "license_file": license_file,
        "source": None,
        "dependencies": dependencies or [],
        "targets": [],
        "features": features or {},
        "manifest_path": f"/ws/{name}/Cargo.toml",
        "publish": publish,
        "metadata": {},
    }


def make_meta(packages, edges, workspace=None):
    """Build a minimal cargo-metadata document.

    ``edges`` is a list of ``(from, to, kind)`` tuples where ``kind`` is None
    for a normal dependency, ``"dev"`` or ``"build"``.
    """
    ids = {p["name"]: p["id"] for p in packages}
    nodes = {p["id"]: {"id": p["id"], "deps": [], "features": [], "dependencies": []} for p in packages}
    for source, target, kind in edges:
        if source not in ids or target not in ids:
            continue
        nodes[ids[source]]["deps"].append(
            {"pkg": ids[target], "dep_kinds": [{"kind": kind, "target": None}]}
        )
    return {
        "packages": packages,
        "workspace_members": workspace if workspace is not None else list(ids.values()),
        "resolve": {"root": None, "nodes": list(nodes.values())},
    }


def base_packages(**overrides):
    """A valid layered workspace; ``overrides`` replace individual packages."""
    defaults = {
        "mio-derive": _package("mio-derive", features={}),
        "mio-http": _package("mio-http", features={"websocket": [], "test-utils": []}),
        "mio-reqwest": _package("mio-reqwest", features={"rustls": [], "native-tls": [], "socks": []}),
        "mio-tungstenite": _package("mio-tungstenite", features={"rustls": [], "native-tls": []}),
        "mio-core": _package("mio-core", features={"derive": [], "rustls": [], "reqwest": [], "audio": []}),
        "mio-agent": _package("mio-agent", features={"derive": [], "image": [], "audio": []}),
        "mio-coding-agent": _package("mio-coding-agent", features={}),
    }
    defaults.update(overrides)
    return list(defaults.values())


VALID_EDGES = [
    ("mio-reqwest", "mio-http", None),
    ("mio-tungstenite", "mio-http", None),
    ("mio-core", "mio-http", None),
    ("mio-core", "mio-derive", None),
    ("mio-core", "mio-reqwest", None),
    ("mio-core", "mio-tungstenite", None),
    ("mio-agent", "mio-core", None),
    ("mio-agent", "mio-derive", None),
    ("mio-coding-agent", "mio-core", None),
    ("mio-coding-agent", "mio-agent", None),
    ("mio-coding-agent", "mio-reqwest", None),
]


class DependencyDirectionTests(unittest.TestCase):
    def test_accepts_layered_graph(self):
        report = check_sdk.check_dependency_direction(make_meta(base_packages(), VALID_EDGES))
        self.assertEqual(report["status"], "pass", report["violations"])
        self.assertEqual(report["violations"], [])
        self.assertEqual(report["normal_edge_count"], len(VALID_EDGES))

    def test_blocks_core_depending_on_agent(self):
        edges = VALID_EDGES + [("mio-core", "mio-agent", None)]
        report = check_sdk.check_dependency_direction(make_meta(base_packages(), edges))
        self.assertEqual(report["status"], "fail")
        self.assertEqual(len(report["violations"]), 1)
        violation = report["violations"][0]
        self.assertEqual((violation["from"], violation["to"]), ("mio-core", "mio-agent"))
        self.assertEqual(violation["invariant"], "mio-core must not depend on mio-agent")
        self.assertEqual(violation["kind"], "upward-dependency")

    def test_blocks_agent_depending_on_coding_layer(self):
        edges = VALID_EDGES + [("mio-agent", "mio-coding-agent", None)]
        report = check_sdk.check_dependency_direction(make_meta(base_packages(), edges))
        self.assertEqual(report["status"], "fail")
        violation = report["violations"][0]
        self.assertEqual((violation["from"], violation["to"]), ("mio-agent", "mio-coding-agent"))
        self.assertIn("must not depend on mio-coding-agent", violation["invariant"])

    def test_blocks_helper_depending_on_core(self):
        edges = VALID_EDGES + [("mio-reqwest", "mio-core", None)]
        report = check_sdk.check_dependency_direction(make_meta(base_packages(), edges))
        self.assertEqual(report["status"], "fail")
        self.assertEqual(report["violations"][0]["kind"], "upward-dependency")

    def test_blocks_self_dependency(self):
        edges = VALID_EDGES + [("mio-http", "mio-http", None)]
        report = check_sdk.check_dependency_direction(make_meta(base_packages(), edges))
        self.assertEqual(report["status"], "fail")
        self.assertEqual(report["violations"][0]["kind"], "self-dependency")

    def test_uses_resolved_graph_not_manifest_text(self):
        # Manifest carries no dependency entries at all, yet the resolve graph
        # has a reversed edge: the checker must still block it.
        packages = base_packages()
        self.assertTrue(all(not p["dependencies"] for p in packages))
        edges = VALID_EDGES + [("mio-tungstenite", "mio-agent", None)]
        report = check_sdk.check_dependency_direction(make_meta(packages, edges))
        self.assertEqual(report["status"], "fail")
        self.assertEqual(
            [(v["from"], v["to"]) for v in report["violations"]], [("mio-tungstenite", "mio-agent")]
        )

    def test_dev_edges_do_not_block(self):
        edges = VALID_EDGES + [("mio-core", "mio-agent", "dev"), ("mio-agent", "mio-coding-agent", "dev")]
        report = check_sdk.check_dependency_direction(make_meta(base_packages(), edges))
        self.assertEqual(report["status"], "pass", report["violations"])

    def test_unclassified_workspace_crate_is_rejected(self):
        packages = base_packages() + [_package("mio-newcomer")]
        edges = VALID_EDGES + [("mio-newcomer", "mio-core", None)]
        report = check_sdk.check_dependency_direction(make_meta(packages, edges))
        self.assertEqual(report["status"], "fail")
        self.assertEqual(report["violations"][0]["kind"], "unclassified-workspace-crate")

    def test_removing_reverse_edge_flips_verdict(self):
        # Negative digraph control: the same graph passes once the reversed
        # edge is removed, so the gate is actually edge-sensitive.
        broken = make_meta(base_packages(), VALID_EDGES + [("mio-http", "mio-core", None)])
        self.assertEqual(check_sdk.check_dependency_direction(broken)["status"], "fail")
        fixed = make_meta(base_packages(), VALID_EDGES)
        self.assertEqual(check_sdk.check_dependency_direction(fixed)["status"], "pass")


class VersionClosureTests(unittest.TestCase):
    def test_missing_version_requirement_is_a_finding(self):
        packages = base_packages(mio_core=_package("mio-core"))
        packages[list(p["name"] for p in packages).index("mio-core")]["dependencies"] = [
            _dep("mio-http", req="*", path="/ws/mio-http")
        ]
        report = check_sdk.check_version_closure(make_meta(packages, VALID_EDGES))
        self.assertFalse(report["publish_ready"])
        self.assertEqual(len(report["findings"]), 1)
        self.assertEqual(report["findings"][0]["dependency"], "mio-http")

    def test_mismatched_version_is_a_finding(self):
        packages = base_packages()
        packages[-1]["dependencies"] = [_dep("mio-agent", req="=0.2.0", path="/ws/mio-agent")]
        report = check_sdk.check_version_closure(make_meta(packages, VALID_EDGES))
        self.assertFalse(report["publish_ready"])
        self.assertIn("does not match", report["findings"][0]["reason"])

    def test_matching_version_passes(self):
        packages = base_packages()
        packages[-1]["dependencies"] = [_dep("mio-agent", req="=0.1.0", path="/ws/mio-agent")]
        report = check_sdk.check_version_closure(make_meta(packages, VALID_EDGES))
        self.assertTrue(report["publish_ready"])
        self.assertEqual(report["checked_in_workspace_path_deps"], 1)

    def test_dev_dependency_without_version_is_ignored(self):
        packages = base_packages()
        packages[-1]["dependencies"] = [_dep("mio-agent", req="*", path="/ws/mio-agent", kind="dev")]
        report = check_sdk.check_version_closure(make_meta(packages, VALID_EDGES))
        self.assertTrue(report["publish_ready"])
        self.assertEqual(report["checked_in_workspace_path_deps"], 0)


class LicenseClosureTests(unittest.TestCase):
    def test_missing_metadata_fails(self):
        packages = base_packages(mio_http=_package("mio-http", license=None, license_file=None))
        report = check_sdk.check_license_closure(make_meta(packages, VALID_EDGES), allow_unknown=False)
        self.assertEqual(report["status"], "fail")
        self.assertFalse(report["license_metadata_complete"])
        self.assertEqual([m["name"] for m in report["missing_metadata"]], ["mio-http"])

    def test_license_file_only_is_accepted(self):
        packages = base_packages(
            mio_http=_package("mio-http", license=None, license_file="LICENSE-APACHE")
        )
        report = check_sdk.check_license_closure(make_meta(packages, VALID_EDGES), allow_unknown=False)
        self.assertEqual(report["status"], "pass")
        self.assertTrue(report["license_metadata_complete"])
        self.assertEqual(report["license_file_only"][0]["license_file"], "LICENSE-APACHE")

    def test_allow_unknown_marks_unverified(self):
        packages = base_packages(mio_http=_package("mio-http", license=None, license_file=None))
        report = check_sdk.check_license_closure(make_meta(packages, VALID_EDGES), allow_unknown=True)
        self.assertEqual(report["status"], "unknown")
        self.assertFalse(report["license_metadata_complete"])
        self.assertEqual(report["missing_metadata_count"], 1)

    def test_legacy_expression_is_review_only(self):
        packages = base_packages(mio_http=_package("mio-http", license="MIT/Apache-2.0"))
        report = check_sdk.check_license_closure(make_meta(packages, VALID_EDGES), allow_unknown=False)
        self.assertEqual(report["status"], "pass")
        self.assertTrue(any(r["legacy_separator"] for r in report["review"]))


class FeatureMatrixTests(unittest.TestCase):
    def test_rejects_unknown_feature(self):
        matrix = [{"group": "helpers", "package": "mio-core", "features": ["does-not-exist"]}]
        report = check_sdk.validate_feature_matrix(make_meta(base_packages(), VALID_EDGES), matrix=matrix)
        self.assertEqual(report["status"], "fail")
        self.assertEqual(report["unknown_features"][0]["feature"], "does-not-exist")

    def test_accepts_declared_features(self):
        matrix = [{"group": "helpers", "package": "mio-core", "features": ["derive", "audio"]}]
        report = check_sdk.validate_feature_matrix(make_meta(base_packages(), VALID_EDGES), matrix=matrix)
        self.assertEqual(report["status"], "pass", report["unknown_features"])

    def test_rejects_unknown_package(self):
        matrix = [{"group": "helpers", "package": "not-a-crate", "features": []}]
        report = check_sdk.validate_feature_matrix(make_meta(base_packages(), VALID_EDGES), matrix=matrix)
        self.assertEqual(report["status"], "fail")


class LocalMatrixLogTests(unittest.TestCase):
    """The optional local matrix must keep one raw log per combo.

    Two combos can share group/package/features (the `host` and `host_loopback`
    examples do) while compiling different targets. A label built only from
    group/package/features made the second run overwrite the first run's raw
    log, silently hiding its real failure.
    """

    def test_same_package_entries_keep_distinct_raw_logs_and_failures(self):
        import tempfile
        from unittest import mock

        entries = [
            {"group": "examples", "package": "mio-coding-agent", "args": ["--example", "host"]},
            {
                "group": "examples",
                "package": "mio-coding-agent",
                "args": ["--example", "host_loopback"],
            },
        ]
        # First combo really fails, second really passes; each keeps its own log.
        outcomes = [
            mock.Mock(returncode=101, stdout="", stderr="error: first example failed to build"),
            mock.Mock(returncode=0, stdout="Finished `dev` profile", stderr=""),
        ]
        original_matrix = check_sdk.FEATURE_MATRIX
        check_sdk.FEATURE_MATRIX = entries
        try:
            with tempfile.TemporaryDirectory() as tmp:
                with mock.patch.object(check_sdk.subprocess, "run", side_effect=outcomes) as run:
                    report = check_sdk.run_local_matrix(Path(tmp), offline=True, toolchain="")
                self.assertEqual(run.call_count, 2)
                self.assertEqual(report["counts"], {"pass": 1, "fail": 1, "skipped": 0})
                self.assertEqual(report["status"], "fail")
                labels = [r["label"] for r in report["runs"]]
                self.assertEqual(len(set(labels)), 2, labels)
                failed, passed = report["runs"]
                self.assertEqual(failed["status"], "fail")
                self.assertEqual(passed["status"], "pass")
                failed_log = Path(failed["log"])
                passed_log = Path(passed["log"])
                self.assertNotEqual(failed_log, passed_log)
                self.assertTrue(failed_log.exists() and passed_log.exists())
                # The failing run's real reason survives and is never overwritten
                # by the later passing run; the pass keeps its own raw output too.
                self.assertIn(
                    "error: first example failed to build", failed_log.read_text(encoding="utf-8")
                )
                self.assertNotIn(
                    "error: first example failed to build", passed_log.read_text(encoding="utf-8")
                )
                self.assertIn("Finished", passed_log.read_text(encoding="utf-8"))
                self.assertEqual(failed["error_tail"], ["error: first example failed to build"])
        finally:
            check_sdk.FEATURE_MATRIX = original_matrix


class SemverTests(unittest.TestCase):
    def test_supported_requirement_forms(self):
        cases = [
            ("0.1.0", "=0.1.0", True),
            ("0.1.1", "=0.1.0", False),
            ("0.1.5", "^0.1.0", True),
            ("0.2.0", "^0.1.0", False),
            ("0.1.0", "0.1.0", True),
            ("0.2.0", "0.1.0", False),
            ("0.5.0", "^0.1.0", False),
            ("0.1.0", ">=0.1, <0.2", True),
            ("0.2.0", ">=0.1, <0.2", False),
            ("1.2.3", "~1.2.0", True),
            ("1.3.0", "~1.2.0", False),
            ("9.9.9", "*", True),
            ("0.0.2", "^0.0.1", False),
            ("0.0.1", "^0.0.1", True),
            ("0.9.0", "^0", True),
            ("0.0.9", "^0.0", True),
            ("1.9.0", "~1", True),
            ("0.1.0-alpha", "=0.1.0", False),
            ("0.1.0", "=0.1.0-alpha", False),
        ]
        for version, req, expected in cases:
            with self.subTest(version=version, req=req):
                self.assertEqual(check_sdk.semver_matches(version, req), expected)


class PackagingHygieneTests(unittest.TestCase):
    def test_archival_files_are_allowed_only_when_explicitly_excluded(self):
        import tempfile

        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            (directory / "Cargo.toml.orig").write_text("archival upstream manifest")
            manifest = directory / "Cargo.toml"
            manifest.write_text('[package]\nexclude = ["/Cargo.toml.orig"]\n')
            package = _package("mio-http")
            package["manifest_path"] = str(manifest)
            meta = make_meta([package], [])
            self.assertEqual(check_sdk.check_packaging_hygiene(meta)["status"], "pass")
            # Cargo ignores exclude when include is present. Do not mistake
            # that configuration for proof that the archive is excluded.
            manifest.write_text('[package]\nexclude = ["/Cargo.toml.orig"]\ninclude = ["*"]\n')
            self.assertEqual(check_sdk.check_packaging_hygiene(meta)["status"], "findings")
            self.assertEqual((directory / "Cargo.toml.orig").read_text(), "archival upstream manifest")

    def test_flags_reserved_cargo_artifacts(self):
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            crate_dir = Path(tmp) / "mio-http"
            crate_dir.mkdir()
            (crate_dir / ".cargo_vcs_info.json").write_text("{}")
            (crate_dir / "Cargo.toml.orig").write_text("[package]\n")
            package = _package("mio-http")
            package["manifest_path"] = str(crate_dir / "Cargo.toml")
            report = check_sdk.check_packaging_hygiene(make_meta([package], []))
        self.assertEqual(report["status"], "findings")
        self.assertEqual(
            sorted(f["file"] for f in report["findings"]),
            [str(crate_dir / ".cargo_vcs_info.json"), str(crate_dir / "Cargo.toml.orig")],
        )

    def test_clean_crate_dir_passes(self):
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            crate_dir = Path(tmp) / "mio-http"
            crate_dir.mkdir()
            package = _package("mio-http")
            package["manifest_path"] = str(crate_dir / "Cargo.toml")
            report = check_sdk.check_packaging_hygiene(make_meta([package], []))
        self.assertEqual(report["status"], "pass")
        self.assertEqual(report["findings"], [])


class RealWorkspaceTests(unittest.TestCase):
    """Run the checker against the real locked workspace when cargo is present."""

    @classmethod
    def setUpClass(cls):
        try:
            cls.meta = check_sdk.run_cargo_metadata(offline=True)
        except RuntimeError as exc:
            raise unittest.SkipTest(f"cargo metadata unavailable: {exc}")

    def test_real_graph_has_no_upward_dependency(self):
        report = check_sdk.check_dependency_direction(self.meta)
        self.assertEqual(report["status"], "pass", report["violations"])

    def test_real_graph_license_metadata_is_complete(self):
        report = check_sdk.check_license_closure(self.meta, allow_unknown=False)
        self.assertTrue(report["license_metadata_complete"], report["missing_metadata"])

    def test_shipped_feature_matrix_matches_metadata(self):
        report = check_sdk.validate_feature_matrix(self.meta)
        self.assertEqual(report["status"], "pass", report["unknown_features"])

    def test_real_workspace_members_are_all_classified(self):
        names = {p["name"] for p in self.meta["packages"] if p["id"] in set(self.meta["workspace_members"])}
        self.assertEqual(names, set(check_sdk.LAYERS))

    def test_cli_report_is_stable_and_json_serializable(self):
        import argparse
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            meta_path = Path(tmp) / "metadata.json"
            meta_path.write_text(json.dumps(self.meta), encoding="utf-8")
            report = check_sdk.build_report(
                argparse.Namespace(
                    metadata=str(meta_path),
                    online=False,
                    strict_publish=True,
                    allow_unknown_license=False,
                )
            )
        json.dumps(report)  # must serialize
        self.assertEqual(report["schema"], "mio.sdk_contract.v1")
        version_ready = report["gates"]["version_closure"]["publish_ready"]
        hygiene_clean = report["gates"]["packaging_hygiene"]["status"] == "pass"
        expected = "fail" if not (version_ready and hygiene_clean) else "pass"
        self.assertEqual(report["summary"]["status"], expected)
        self.assertEqual(report["summary"]["publish_ready"], version_ready and hygiene_clean)

    def test_missing_metadata_file_fails_loudly(self):
        with self.assertRaises(RuntimeError):
            check_sdk.load_metadata("/nonexistent/metadata.json")


if __name__ == "__main__":
    unittest.main()
