#!/usr/bin/env python3
"""Offline notice-bundle integrity and provenance failure cases."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("bundle", Path(__file__).resolve().parents[1] / "scripts/bundle-licenses.py")
bundle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bundle)


class NoticeBundleTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.inventory = self.root / "inventory.json"
        self.lock = self.root / "Cargo.lock"
        self.output = self.root / "bundle"
        (self.root / "notice").write_bytes(b"Original permission notice\n")
        source = "registry+https://github.com/rust-lang/crates.io-index"
        self.lock.write_text(f'[[package]]\nname="demo"\nversion="1.0.0"\nsource="{source}"\nchecksum="{"0" * 64}"\n')
        self.package = {
            "name": "demo", "version": "1.0.0", "source_id": source,
            "source_kind": "registry", "ship_scope": "shipping",
            "license_declared": "MIT", "spdx": {"status": "declared"},
            "text_status": "collected", "files": [{"status": "collected", "name": "LICENSE",
                "copied_as": "notice", "sha256": hashlib.sha256((self.root / "notice").read_bytes()).hexdigest()}],
        }

    def plan(self):
        self.inventory.write_text(json.dumps({"schema": "mio-license-inventory/1", "packages": [self.package]}))
        return bundle.plan(self.inventory, None, self.lock)

    def build(self):
        report, blobs = self.plan()
        bundle.write_bundle(self.output, report, blobs)
        return report

    def test_verbatim_text_is_bound_to_hash_and_lockfile(self):
        self.build()
        report = bundle.verify(self.output, self.lock)
        copied = self.output / report["packages"][0]["files"][0]["path"]
        self.assertEqual(copied.read_bytes(), (self.root / "notice").read_bytes())
        copied.write_bytes(b"tampered")
        with self.assertRaisesRegex(ValueError, "SHA-256"):
            bundle.verify(self.output, self.lock)

    def test_changed_lockfile_requires_a_new_inventory(self):
        self.build()
        self.lock.write_text(self.lock.read_text() + "\n# changed\n")
        with self.assertRaisesRegex(ValueError, "stale"):
            bundle.verify(self.output, self.lock)

    def test_existing_bundle_is_never_overwritten(self):
        report = self.build()
        before = (self.output / "index.json").read_bytes()
        with self.assertRaises(FileExistsError):
            bundle.write_bundle(self.output, report, {})
        self.assertEqual((self.output / "index.json").read_bytes(), before)

    def test_path_escape_and_missing_text_fail_before_writing(self):
        self.package["files"][0]["copied_as"] = "../escape"
        with self.assertRaisesRegex(ValueError, "inside"):
            self.plan()
        self.package["text_status"] = "missing"
        with self.assertRaisesRegex(ValueError, "unambiguous"):
            self.plan()
        self.assertFalse(self.output.exists())

    @unittest.skipUnless(os.name == "posix", "POSIX symlink fixture")
    def test_symlink_cannot_supply_license_evidence(self):
        (self.root / "alias").symlink_to(self.root / "notice")
        self.package["files"][0]["copied_as"] = "alias"
        with self.assertRaisesRegex(ValueError, "symlink"):
            self.plan()

    def test_unindexed_files_are_not_part_of_a_valid_bundle(self):
        self.build()
        (self.output / "unexpected").write_text("stale data")
        with self.assertRaisesRegex(ValueError, "unindexed"):
            bundle.verify(self.output, self.lock)

    def test_upstream_url_must_match_the_full_recorded_commit(self):
        record = {"repository": "https://github.com/example/project", "commit": "a" * 40}
        with self.assertRaisesRegex(ValueError, "pinned"):
            bundle.supplement_origin(record, {"url": "https://raw.githubusercontent.com/example/project/main/LICENSE"})


if __name__ == "__main__":
    unittest.main()
