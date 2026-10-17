"""Offline tests for the license inventory collector (scripts/collect-licenses.py).

Every case builds real files in a temporary directory, then runs the collector
against a synthetic cargo-metadata document that points at those files. The
tests assert the honest-reporting contract:

* verbatim copies with SHA-256 that hashes back to the source bytes;
* missing text, unsupported names, symlinks, path escapes and budgets are
  reported as ``unknown`` and never as verified/complete;
* same-name different-version packages stay separate;
* the public index never contains an absolute local path, while the private
  report does;
* existing evidence, output escapes and symlinked outputs are refused.

No network, no cargo invocation, no compilation.
"""
import hashlib
import importlib.util
import json
import os
import tempfile
import unittest
from unittest import mock
import subprocess
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts" / "collect-licenses.py"
spec = importlib.util.spec_from_file_location("collect_licenses", SCRIPT)
collect_licenses = importlib.util.module_from_spec(spec)
spec.loader.exec_module(collect_licenses)

DEFAULT_LIMITS = dict(collect_licenses.DEFAULT_LIMITS)


def _package_id(path, name, version):
    return f"path+file://{path}#{name}@{version}"


def package(directory, name, version, license="MIT", license_file=None, source=None):
    return {
        "name": name,
        "version": version,
        "id": _package_id(directory, name, version),
        "license": license,
        "license_file": license_file,
        "source": source,
        "manifest_path": os.path.join(directory, "Cargo.toml"),
        "dependencies": [],
        "targets": [],
        "features": {},
        "metadata": {},
    }


def dependency(package_id, kind=None):
    return {"pkg": package_id, "dep_kinds": [{"kind": kind, "target": None}]}


def make_meta(root, packages, edges):
    """Build a minimal cargo-metadata document with a real resolve graph."""
    return {
        "packages": packages,
        "workspace_members": [packages[0]["id"]],
        "workspace_root": str(root),
        "resolve": {
            "nodes": [
                {"id": package_id, "deps": deps, "dependencies": [d["pkg"] for d in deps]}
                for package_id, deps in edges.items()
            ]
        },
    }


class InventoryCase(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name).resolve()
        self.out = self.root / "target" / "license-inventory"
        self.addCleanup(self._tmp.cleanup)

    def crate(self, name, version, text="MIT license text\n"):
        directory = self.root / name / version
        directory.mkdir(parents=True, exist_ok=True)
        (directory / "Cargo.toml").write_text("[package]\n")
        if text is not None:
            (directory / "LICENSE").write_text(text)
        return directory

    def run_collector(self, meta, **kwargs):
        options = dict(repo_root=self.root, out_dir=self.out)
        options.update(kwargs)
        return collect_licenses.collect(meta, **options)

    def read_index(self):
        return json.loads((self.out / "index.json").read_text())

    def single(self, directory, *, collect_kwargs=None, **package_kwargs):
        pkg = package(directory, "demo", "1.0.0", **package_kwargs)
        meta = make_meta(self.root, [pkg], {pkg["id"]: []})
        return self.run_collector(meta, **(collect_kwargs or {}))["packages"][0]

    def assert_no_verified_value(self, document):
        """No status field anywhere may claim a verified license result."""
        def walk(node):
            if isinstance(node, dict):
                for value in node.values():
                    walk(value)
            elif isinstance(node, list):
                for value in node:
                    walk(value)
            elif isinstance(node, str):
                self.assertNotEqual(node, "verified")

        walk(document)


class TextCollectionTests(InventoryCase):
    def test_authors_with_permission_notice_is_copied_verbatim(self):
        directory = self.crate("demo", "1.0.0", text=None)
        notice = "LICENSE:\nPermission is hereby granted, free of charge.\nCOPYRIGHT: Example authors\n"
        (directory / "AUTHORS").write_text(notice)
        record = self.single(directory)
        self.assertEqual(record["text_status"], "collected")
        self.assertEqual(record["files"][0]["name"], "AUTHORS")
        self.assertEqual((self.out / record["files"][0]["copied_as"]).read_text(), notice)

    def test_contributor_names_alone_do_not_substitute_for_license_text(self):
        directory = self.crate("demo", "1.0.0", text=None)
        (directory / "AUTHORS").write_text("Example author\n")
        self.assertEqual(self.single(directory)["text_status"], "missing")

    def test_copies_verbatim_text_with_matching_sha256(self):
        directory = self.crate("demo", "1.0.0", "Apache License\nVersion 2.0\n")
        (directory / "NOTICE").write_text("Demo NOTICE\n")
        record = self.single(directory, license="MIT OR Apache-2.0")
        self.assertEqual(record["text_status"], "collected")
        self.assertEqual(record["spdx"]["status"], "declared")
        self.assertEqual(len(record["files"]), 2)
        for entry in record["files"]:
            copied = self.out / entry["copied_as"]
            raw = copied.read_bytes()
            self.assertEqual(entry["sha256"], hashlib.sha256(raw).hexdigest())
            self.assertTrue(entry["copy_hash_match"])
            self.assertEqual(entry["bytes"], len(raw))
        license_entry = next(e for e in record["files"] if e["name"] == "LICENSE")
        self.assertEqual(
            (self.out / license_entry["copied_as"]).read_text(), "Apache License\nVersion 2.0\n"
        )

    def test_missing_text_is_reported_unknown_and_never_verified(self):
        directory = self.crate("demo", "1.0.0", text=None)
        record = self.single(directory)
        self.assertEqual(record["text_status"], "missing")
        self.assertEqual(record["text_unknown_reason"], "no_license_text_in_manifest_dir")
        self.assert_no_verified_value(self.read_index())
        blob = (self.out / "index.json").read_text()
        self.assertIn("not a legal review", blob)

    def test_empty_license_file_is_unknown(self):
        directory = self.crate("demo", "1.0.0", text="")
        record = self.single(directory)
        self.assertEqual(record["text_status"], "unknown")
        self.assertEqual(record["text_unknown_reason"], "empty_file")

    def test_declared_license_file_is_used_and_marked(self):
        directory = self.crate("demo", "1.0.0", text=None)
        (directory / "LICENSE.custom").write_text("Custom license body\n")
        record = self.single(directory, license=None, license_file="LICENSE.custom")
        roles = {entry["name"]: entry["role"] for entry in record["files"]}
        self.assertEqual(roles["LICENSE.custom"], "declared_license_file")
        self.assertEqual(record["text_status"], "collected")
        self.assertEqual(record["spdx"]["status"], "missing")

    def test_oversized_file_downgrades_to_unknown(self):
        directory = self.crate("demo", "1.0.0", text="x" * 200)
        record = self.single(directory, collect_kwargs={"limits": dict(DEFAULT_LIMITS, max_file_bytes=16)})
        self.assertEqual(record["text_status"], "unknown")
        self.assertEqual(record["text_unknown_reason"], "file_exceeds_limit")
        self.assertEqual(
            [entry for entry in record["files"] if entry["status"] == "collected"], []
        )

    def test_total_budget_exhaustion_marks_remaining_packages_unknown(self):
        first = self.crate("aaa", "1.0.0", "a" * 8)
        second = self.crate("bbb", "1.0.0", "b" * 8)
        packages = [package(first, "aaa", "1.0.0"), package(second, "bbb", "1.0.0")]
        meta = make_meta(self.root, packages, {packages[0]["id"]: [dependency(packages[1]["id"])], packages[1]["id"]: []})
        limits = dict(DEFAULT_LIMITS, max_total_bytes=12)
        index = self.run_collector(meta, limits=limits)
        records = {record["name"]: record for record in index["packages"]}
        self.assertEqual(records["aaa"]["text_status"], "collected")
        self.assertEqual(records["bbb"]["text_status"], "unknown")
        self.assertEqual(records["bbb"]["text_unknown_reason"], "total_bytes_budget_exhausted")


class IdentityAndScopeTests(InventoryCase):
    def test_same_name_different_versions_stay_separate(self):
        older = self.crate("demo", "1.0.0", "old license\n")
        newer = self.crate("demo", "2.0.0", "new license\n")
        packages = [package(older, "demo", "1.0.0"), package(newer, "demo", "2.0.0")]
        meta = make_meta(
            self.root,
            packages,
            {packages[0]["id"]: [dependency(packages[1]["id"])], packages[1]["id"]: []},
        )
        index = self.run_collector(meta)
        self.assertEqual(len(index["packages"]), 2)
        copies = [record["files"][0]["copied_as"] for record in index["packages"]]
        self.assertEqual(len(set(copies)), 2)
        for record in index["packages"]:
            self.assertEqual(record["text_status"], "collected")

    def test_shipping_dev_only_and_unreachable_scopes(self):
        root = self.crate("root", "0.1.0")
        ship = self.crate("ship", "1.0.0")
        dev = self.crate("devdep", "1.0.0")
        loose = self.crate("loose", "1.0.0")
        packages = [
            package(root, "root", "0.1.0"),
            package(ship, "ship", "1.0.0"),
            package(dev, "devdep", "1.0.0"),
            package(loose, "loose", "1.0.0"),
        ]
        meta = make_meta(
            self.root,
            packages,
            {
                packages[0]["id"]: [dependency(packages[1]["id"]), dependency(packages[2]["id"], "dev")],
                packages[1]["id"]: [],
                packages[2]["id"]: [],
            },
        )
        index = self.run_collector(meta)
        scopes = {record["name"]: record["ship_scope"] for record in index["packages"]}
        self.assertEqual(scopes["root"], "workspace")
        self.assertEqual(scopes["ship"], "shipping")
        self.assertEqual(scopes["devdep"], "dev_only")
        self.assertEqual(scopes["loose"], "unreachable")
        self.assertEqual(index["totals"]["shipping_packages"], 1)
        self.assertEqual(index["totals"]["dev_only_packages"], 1)

    def test_metadata_without_resolve_graph_is_rejected(self):
        package_dir = self.crate("demo", "1.0.0")
        pkg = package(package_dir, "demo", "1.0.0")
        meta = {"packages": [pkg], "workspace_members": [pkg["id"]], "resolve": None}
        with self.assertRaises(collect_licenses.InventoryError):
            self.run_collector(meta)


class SecurityTests(InventoryCase):
    def test_declared_license_file_path_escape_is_unknown(self):
        directory = self.crate("demo", "1.0.0", text=None)
        outside = self.root / "outside.txt"
        outside.write_text("outside license text\n")
        record = self.single(directory, license=None, license_file="../../outside.txt")
        self.assertEqual(record["text_status"], "unknown")
        self.assertEqual(record["text_unknown_reason"], "path_escape")
        # The escaped file must stay untouched and never be copied.
        self.assertEqual(outside.read_text(), "outside license text\n")
        self.assertEqual(list((self.out / "text").glob("*")), [])

    def test_symlinked_license_file_is_rejected(self):
        directory = self.crate("demo", "1.0.0", text=None)
        outside = self.root / "real-license.txt"
        outside.write_text("real license\n")
        os.symlink(outside, directory / "LICENSE")
        record = self.single(directory)
        self.assertEqual(record["text_status"], "unknown")
        self.assertEqual(record["text_unknown_reason"], "symlink_not_followed")
        self.assertEqual(list((self.out / "text").glob("*")), [])

    def test_symlinked_manifest_dir_file_cannot_escape(self):
        directory = self.crate("demo", "1.0.0", text=None)
        outside_dir = self.root / "elsewhere"
        outside_dir.mkdir()
        (outside_dir / "LICENSE").write_text("elsewhere license\n")
        os.symlink(outside_dir / "LICENSE", directory / "COPYING")
        record = self.single(directory)
        self.assertEqual(record["text_unknown_reason"], "symlink_not_followed")

    def test_output_escape_is_refused(self):
        directory = self.crate("demo", "1.0.0")
        pkg = package(directory, "demo", "1.0.0")
        meta = make_meta(self.root, [pkg], {pkg["id"]: []})
        with self.assertRaises(collect_licenses.InventoryError):
            collect_licenses.collect(
                meta, repo_root=self.root, out_dir=self.root / ".." / "escaped-inventory"
            )

    def test_symlinked_output_directory_is_refused(self):
        directory = self.crate("demo", "1.0.0")
        pkg = package(directory, "demo", "1.0.0")
        meta = make_meta(self.root, [pkg], {pkg["id"]: []})
        real_out = self.root / "real-out"
        real_out.mkdir()
        link = self.root / "linked-out"
        os.symlink(real_out, link)
        with self.assertRaises(collect_licenses.InventoryError):
            collect_licenses.collect(meta, repo_root=self.root, out_dir=link)

    def test_existing_evidence_is_not_overwritten(self):
        directory = self.crate("demo", "1.0.0")
        pkg = package(directory, "demo", "1.0.0")
        meta = make_meta(self.root, [pkg], {pkg["id"]: []})
        self.run_collector(meta)
        self.assertTrue((self.out / "index.json").exists())
        with self.assertRaises(collect_licenses.InventoryError):
            self.run_collector(meta)

    def test_public_index_has_no_local_paths(self):
        directory = self.crate("demo", "1.0.0")
        record_dir = self.crate("other", "2.0.0")
        packages = [package(directory, "demo", "1.0.0"), package(record_dir, "other", "2.0.0")]
        meta = make_meta(
            self.root,
            packages,
            {packages[0]["id"]: [dependency(packages[1]["id"])], packages[1]["id"]: []},
        )
        self.run_collector(meta)
        blob = (self.out / "index.json").read_text()
        forbidden = [str(self.root), str(self.out), str(directory), str(record_dir)]
        for needle in forbidden:
            self.assertNotIn(needle, blob)
        private = json.loads((self.out / "private" / "local-paths.json").read_text())
        self.assertTrue(private["private"])
        self.assertIn(str(directory), json.dumps(private["manifest_directories"]))
        self.assertIn("do not publish", private["notice"])

    def test_safe_component_neutralises_traversal_names(self):
        self.assertEqual(collect_licenses.safe_component("../../etc/passwd"), "etc_passwd")
        self.assertEqual(collect_licenses.safe_component(".."), "_")
        self.assertEqual(collect_licenses.safe_component("LICENSE-MIT"), "LICENSE-MIT")


class DeclarationTests(InventoryCase):
    def test_spdx_expression_is_recorded_verbatim(self):
        directory = self.crate("demo", "1.0.0")
        record = self.single(directory, license="MIT/Apache-2.0")
        self.assertEqual(record["license_declared"], "MIT/Apache-2.0")
        self.assertEqual(record["spdx"]["expression"], "MIT/Apache-2.0")
        self.assertEqual(record["spdx"]["status"], "nonstandard_separator")
        self.assertEqual(record["spdx"]["unrecognized_tokens"], [])

    def test_unrecognized_tokens_are_surfaced_not_guessed(self):
        directory = self.crate("demo", "1.0.0")
        record = self.single(directory, license="Some-Proprietary-Thing")
        self.assertEqual(record["spdx"]["status"], "unrecognized_tokens")
        self.assertEqual(record["spdx"]["unrecognized_tokens"], ["Some-Proprietary-Thing"])

    def test_license_ref_and_missing_metadata(self):
        directory = self.crate("demo", "1.0.0")
        record = self.single(directory, license="LicenseRef-Acme-Internal")
        self.assertEqual(record["spdx"]["status"], "license_ref")

    def test_reproduce_block_and_disclaimer_are_present(self):
        directory = self.crate("demo", "1.0.0")
        self.single(directory)
        index = self.read_index()
        self.assertEqual(index["schema"], collect_licenses.SCHEMA)
        self.assertTrue(index["tool"]["inventory_only"])
        self.assertIn("not a legal review", index["disclaimer"])
        self.assertIn("cargo metadata", index["reproduce"][0])
        self.assert_no_verified_value(index)


class MetadataCommandTests(unittest.TestCase):
    def test_feature_selection_is_not_forwarded_to_cargo_fetch(self):
        for settings, expected in (
            ({"all_features": True}, ["--all-features"]),
            ({"features": ["audio", "image"]}, ["--features", "audio,image"]),
        ):
            with self.subTest(settings=settings):
                replies = [
                    subprocess.CompletedProcess([], 0, "", ""),
                    subprocess.CompletedProcess([], 0, '{"packages": []}', ""),
                ]
                with mock.patch.object(collect_licenses, "_run", side_effect=replies) as invoke:
                    _, command = collect_licenses.load_metadata(Path.cwd(), fetch=True, **settings)
                self.assertEqual(invoke.call_args_list[0].args[0], ["cargo", "fetch", "--locked"])
                for flag in expected:
                    self.assertIn(flag, command)
                self.assertIn("--offline", command)


if __name__ == "__main__":
    unittest.main()
