#!/usr/bin/env python3
"""Static DES-03 CLI -> engine/SDK semantic-map checker.

This tool is intentionally static. It reads a versioned manifest
(`docs/cli-sdk-semantics.json`), extracts the real CLI flags from the parser
source (the same principle as `scripts/check-site.py`), and checks that:

* every real flag/alias is assigned to exactly one manifest entry (coverage);
* no alias appears in two entries (no duplicate aliases);
* no manifest alias is absent from the parser (no residual/obsolete flags);
* engine entries name a real `Args` field, a real wiring location, real public
  API declarations, and a real test definition;
* allowed-difference entries use one of the bounded DES-03 reasons and carry an
  explanation, and semantics-critical flags are not excused as rendering;
* declared "missing" targets for incomplete entries are still missing (so a
  future merge that closes the gap is detected as a stale manifest).

It does NOT execute the referenced tests and never claims a behavior test
passed. Exit codes: 0 = pass, 1 = validation failure (or strict mode with
incomplete entries), 3 = incomplete and accepted with --allow-incomplete.

The report contains only repository-relative paths, symbol names, reasons and
sha256 digests of tracked input sources. It never prints environment variables
or credential material.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import sys
from pathlib import Path

TOOL = "check-cli-semantics.py"
MANIFEST_VERSION = 1

# Bounded DES-03 allowed-difference reasons. Hardcoded so a manifest cannot widen
# the enum by editing itself.
ALLOWED_DIFFERENCE_REASONS = {
    "terminal_rendering",
    "approval_interaction",
    "persistence_defaults",
    "host_configuration",
    "maintenance",
}

# Execution-semantics flags that must never be excused as a rendering difference.
# The manifest must declare this exact set and every member must be engine_mapping.
CRITICAL_FLAGS = {
    "--no-shell",
    "--dedup-key",
    "--tools",
    "--exclude-tools",
    "--no-tools",
    "--no-mcp",
    "--subagents",
    "--auto-compact",
    "--context-window",
    "--max-output-tokens",
    "--temperature",
    "--sandbox",
    "--sandbox-read-only",
    "--rewind",
    "--output-cleanup-plan",
    "--output-cleanup-confirm",
    "--checkpoints",
    "--recovery",
    "--max-model-calls",
    "--max-total-tokens",
}

# Synthetic flags handled by the binary entry point / argument parser rather than
# the `parse()` match arms. Mirrors scripts/check-site.py.
SYNTHETIC_FLAGS = ("-h", "--help", "-V", "--version", "--")

PUBLIC_DECL = re.compile(
    r"^\s*pub\s+(?:async\s+)?(?:unsafe\s+)?(?:struct|enum|trait|fn|const|type|union|mod)\s+{symbol}\b",
    re.M,
)
PRIVATE_DECL = re.compile(
    r"^\s*pub\s*\([^)]*\)\s+(?:async\s+)?(?:unsafe\s+)?(?:struct|enum|trait|fn|const|type|union|mod)\s+{symbol}\b",
    re.M,
)
TEST_DECL = re.compile(r"\bfn\s+{name}\s*\(")
ARGS_STRUCT = re.compile(r"pub\(super\)\s+struct\s+Args\s*\{(.*?)\n\}", re.S)
ARGS_FIELD = re.compile(r"pub\(super\)\s+([A-Za-z_][A-Za-z0-9_]*)\s*:")


class Checker:
    def __init__(self, root: Path, manifest_path: Path, flags_source: Path | None) -> None:
        self.root = root.resolve()
        self.manifest_path = manifest_path
        self.flags_source = flags_source
        self.errors: list[str] = []
        self.warnings: list[str] = []
        self.checks: dict[str, bool] = {}
        self.hashes: dict[str, str] = {}
        self.flag_count = 0
        self.entry_count = 0
        self.classification: dict[str, int] = {}
        self.reason_counts: dict[str, int] = {}
        self.incomplete: list[dict] = []

    # ---- path + hashing helpers -------------------------------------------------
    def resolve(self, rel: str, what: str) -> Path | None:
        if not isinstance(rel, str) or not rel:
            self.errors.append(f"{what}: expected a non-empty repository-relative path")
            return None
        candidate = Path(rel)
        if candidate.is_absolute():
            self.errors.append(f"{what}: absolute paths are not allowed: {rel}")
            return None
        if ".." in candidate.parts:
            self.errors.append(f"{what}: path escapes the repository root: {rel}")
            return None
        resolved = (self.root / candidate).resolve()
        if resolved != self.root and self.root not in resolved.parents:
            self.errors.append(f"{what}: path escapes the repository root: {rel}")
            return None
        if not resolved.is_file():
            self.errors.append(f"{what}: source file does not exist: {rel}")
            return None
        self.hashes.setdefault(rel, sha256_file(resolved))
        return resolved

    # ---- extraction -------------------------------------------------------------
    def extract_flags(self) -> set[str]:
        text = self.flags_source.read_text(encoding="utf-8")
        if "fn parse(" not in text:
            self.errors.append("flags source has no `fn parse(`; cannot verify coverage")
            return set()
        parser = text.split("fn parse(", 1)[1].split("#[cfg(test)]", 1)[0]
        flags: set[str] = set()
        for line in parser.splitlines():
            if re.match(r'\s*"-', line) and "=>" in line:
                flags.update(re.findall(r'"(-[\w-]+)"', line.split("=>", 1)[0]))
        flags.update(SYNTHETIC_FLAGS)
        return flags

    def extract_args_fields(self) -> set[str]:
        text = self.flags_source.read_text(encoding="utf-8")
        match = ARGS_STRUCT.search(text)
        if not match:
            self.errors.append("could not locate `pub(super) struct Args` in the flags source")
            return set()
        return set(ARGS_FIELD.findall(match.group(1)))

    # ---- per-file symbol checks -------------------------------------------------
    def has_public_symbol(self, rel: str, symbol: str) -> bool:
        path = self.resolve(rel, f"publicApi[{symbol}]")
        if path is None:
            return False
        text = path.read_text(encoding="utf-8", errors="replace")
        pattern = re.compile(PUBLIC_DECL.pattern.format(symbol=re.escape(symbol)), re.M)
        if pattern.search(text):
            return True
        private = re.compile(PRIVATE_DECL.pattern.format(symbol=re.escape(symbol)), re.M)
        if private.search(text):
            self.errors.append(f"publicApi[{symbol}]: {rel} declares it with restricted visibility, not `pub`")
        else:
            self.errors.append(f"publicApi[{symbol}]: no public declaration in {rel}")
        return False

    def has_test(self, rel: str, name: str) -> bool:
        path = self.resolve(rel, f"behaviorTest[{name}]")
        if path is None:
            return False
        text = path.read_text(encoding="utf-8", errors="replace")
        if re.search(TEST_DECL.pattern.format(name=re.escape(name)), text):
            return True
        self.errors.append(f"behaviorTest[{name}]: no definition in {rel}")
        return False

    def check_wiring(self, entry_name: str, wiring: list) -> None:
        if not isinstance(wiring, list) or not wiring:
            self.errors.append(f"{entry_name}: engine entry needs a non-empty wiring list")
            return
        for item in wiring:
            rel = item.get("file")
            snippet = item.get("contains")
            path = self.resolve(rel, f"{entry_name}.wiring")
            if path is None:
                continue
            if not isinstance(snippet, str) or not snippet:
                self.errors.append(f"{entry_name}.wiring: missing non-empty `contains` snippet")
                continue
            if snippet not in path.read_text(encoding="utf-8", errors="replace"):
                self.errors.append(
                    f"{entry_name}.wiring: snippet not found in {rel}: {snippet!r}"
                )

    def check_missing_targets(self, entry_name: str, missing: list) -> None:
        if not isinstance(missing, list) or not missing:
            self.errors.append(f"{entry_name}: incomplete entry needs a non-empty missingTargets list")
            return
        for item in missing:
            rel = item.get("file")
            symbol = item.get("symbol")
            kind = item.get("kind")
            if kind not in ("mod", "symbol"):
                self.errors.append(f"{entry_name}.missingTargets: unsupported kind {kind!r}")
                continue
            path = self.resolve(rel, f"{entry_name}.missingTargets[{symbol}]")
            if path is None:
                continue
            text = path.read_text(encoding="utf-8", errors="replace")
            keyword = "mod" if kind == "mod" else r"(?:struct|enum|trait|fn|const|type|union)"
            present = re.search(
                rf"^\s*pub\s+(?:async\s+)?{keyword}\s+{re.escape(symbol)}\b", text, re.M
            )
            if present:
                self.errors.append(
                    f"{entry_name}.missingTargets: stale incomplete declaration - "
                    f"{symbol} is now public in {rel}; mark the entry complete"
                )

    # ---- top level --------------------------------------------------------------
    def run(self, allow_incomplete: bool) -> dict:
        try:
            raw = self.manifest_path.read_text(encoding="utf-8")
        except OSError as error:
            self.errors.append(f"cannot read manifest {self.manifest_path}: {error}")
            return self.report("fail", allow_incomplete, 1)
        try:
            manifest = json.loads(raw)
        except json.JSONDecodeError as error:
            self.errors.append(f"manifest is not valid JSON: {error}")
            return self.report("fail", allow_incomplete, 1)
        self.hashes["manifest"] = hashlib.sha256(raw.encode("utf-8")).hexdigest()

        if manifest.get("manifestVersion") != MANIFEST_VERSION:
            self.errors.append(
                f"unsupported manifestVersion {manifest.get('manifestVersion')!r}; expected {MANIFEST_VERSION}"
            )
        if manifest.get("design") != "DES-03":
            self.errors.append("manifest.design must be DES-03")

        if self.flags_source is None:
            declared = manifest.get("flagSource")
            resolved = self.resolve(declared, "manifest.flagSource")
            if resolved is None:
                return self.report("fail", allow_incomplete, 1)
            self.flags_source = resolved
        self.hashes["flagsSource"] = sha256_file(self.flags_source)

        declared_reasons = manifest.get("allowedDifferenceReasons")
        if not isinstance(declared_reasons, dict) or set(declared_reasons) != ALLOWED_DIFFERENCE_REASONS:
            self.errors.append(
                "manifest.allowedDifferenceReasons must be exactly the bounded DES-03 reason set"
            )

        flags = self.extract_flags()
        self.flag_count = len(flags)
        self.checks["flagExtraction"] = bool(flags)

        declared_critical = set(manifest.get("semanticsCriticalFlags") or [])
        required_critical = CRITICAL_FLAGS & flags
        if declared_critical != required_critical:
            self.errors.append(
                "manifest.semanticsCriticalFlags must equal the checker's semantics-critical "
                "flags that are present in the parser"
            )

        entries = manifest.get("entries")
        if not isinstance(entries, list) or not entries:
            self.errors.append("manifest.entries must be a non-empty list")
            return self.report("fail", allow_incomplete, 1)
        self.entry_count = len(entries)

        args_fields = self.extract_args_fields()

        seen: dict[str, str] = {}
        covered: set[str] = set()
        for index, entry in enumerate(entries):
            if not isinstance(entry, dict):
                self.errors.append(f"entries[{index}] must be an object")
                continue
            name = entry.get("name")
            aliases = entry.get("aliases")
            if not isinstance(name, str) or not name:
                self.errors.append(f"entries[{index}] has no name")
                continue
            if not isinstance(aliases, list) or not aliases or not all(
                isinstance(a, str) and a for a in aliases
            ):
                self.errors.append(f"{name}: aliases must be a non-empty list of strings")
                continue
            if name not in aliases:
                self.errors.append(f"{name}: aliases must include the entry name")
            for alias in aliases:
                if alias in seen:
                    self.errors.append(f"duplicate alias {alias}: used by {seen[alias]} and {name}")
                else:
                    seen[alias] = name
                if alias in flags:
                    covered.add(alias)
                else:
                    self.errors.append(f"{name}: residual/unknown flag not present in the parser: {alias}")

            classification = entry.get("classification")
            self.classification[classification] = self.classification.get(classification, 0) + 1
            status = entry.get("status", "complete")
            if status not in ("complete", "incomplete"):
                self.errors.append(f"{name}: status must be complete or incomplete")

            if classification == "engine_mapping":
                field = entry.get("argsField")
                if not isinstance(field, str) or field not in args_fields:
                    self.errors.append(f"{name}: engine_mapping argsField {field!r} is not a real Args field")
                self.check_wiring(name, entry.get("wiring"))
                public = entry.get("publicApi")
                if not isinstance(public, list) or not public:
                    self.errors.append(f"{name}: engine_mapping needs a non-empty publicApi list")
                elif status == "complete":
                    for item in public:
                        if not isinstance(item, dict):
                            self.errors.append(f"{name}.publicApi: entries must be objects")
                            continue
                        self.has_public_symbol(item.get("file"), item.get("symbol"))
                tests = entry.get("behaviorTests")
                if not isinstance(tests, list) or not tests:
                    self.errors.append(f"{name}: engine_mapping needs a non-empty behaviorTests list")
                else:
                    for item in tests:
                        if not isinstance(item, dict):
                            self.errors.append(f"{name}.behaviorTests: entries must be objects")
                            continue
                        self.has_test(item.get("file"), item.get("name"))
                if status == "incomplete":
                    self.check_missing_targets(name, entry.get("missingTargets"))
                    self.incomplete.append({"flag": name, "missingTargets": entry.get("missingTargets") or []})
                boundary = entry.get("evidenceBoundary")
                if not isinstance(boundary, str) or len(boundary.strip()) < 20:
                    self.errors.append(f"{name}: engine_mapping needs a substantive evidenceBoundary")
            elif classification == "allowed_difference":
                reason = entry.get("reason")
                if reason not in ALLOWED_DIFFERENCE_REASONS:
                    self.errors.append(f"{name}: unknown allowed_difference reason {reason!r}")
                else:
                    self.reason_counts[reason] = self.reason_counts.get(reason, 0) + 1
                note = entry.get("reasonNote")
                if not isinstance(note, str) or len(note.strip()) < 20:
                    self.errors.append(f"{name}: allowed_difference needs a substantive reasonNote")
                for item in entry.get("relatedPublicApi") or []:
                    if isinstance(item, dict):
                        self.has_public_symbol(item.get("file"), item.get("symbol"))
            else:
                self.errors.append(f"{name}: unknown classification {classification!r}")

        missing = flags - covered
        for flag in sorted(missing):
            self.errors.append(f"unclassified flag: {flag}")
        self.checks["flagCoverage"] = not missing
        self.checks["noDuplicateAliases"] = len(seen) == sum(
            len(e.get("aliases") or []) for e in entries if isinstance(e, dict)
        )

        # Semantics-critical flags present in this parser must be engine_mapping.
        for critical in sorted(CRITICAL_FLAGS & flags):
            entry = next((e for e in entries if isinstance(e, dict) and critical in (e.get("aliases") or [])), None)
            if entry is None:
                self.errors.append(f"semantics-critical flag missing from manifest: {critical}")
            elif entry.get("classification") != "engine_mapping":
                self.errors.append(
                    f"semantics-critical flag {critical} must be engine_mapping, got {entry.get('classification')!r}"
                )
        self.checks["criticalFlagsEngineMapped"] = not any(
            "semantics-critical" in error for error in self.errors
        )
        self.checks["manifestSchema"] = not any(
            error.startswith("manifest") or error.startswith("unsupported manifestVersion") for error in self.errors
        )

        if self.errors:
            return self.report("fail", allow_incomplete, 1)
        if self.incomplete:
            if allow_incomplete:
                return self.report("incomplete", allow_incomplete, 3)
            self.errors.append(
                f"strict mode: {len(self.incomplete)} incomplete entry(ies); rerun with --allow-incomplete to report without pretending pass"
            )
            return self.report("incomplete", allow_incomplete, 1)
        return self.report("pass", allow_incomplete, 0)

    def report(self, status: str, allow_incomplete: bool, exit_code: int) -> dict:
        return {
            "tool": TOOL,
            "manifestVersion": MANIFEST_VERSION,
            "design": "DES-03",
            "strict": not allow_incomplete,
            "allowIncomplete": allow_incomplete,
            "status": status,
            "exitCode": exit_code,
            "flagCount": self.flag_count,
            "entryCount": self.entry_count,
            "classification": self.classification,
            "allowedDifferenceReasons": self.reason_counts,
            "incompleteEntries": [item["flag"] for item in self.incomplete],
            "checks": self.checks,
            "errors": self.errors,
            "warnings": self.warnings,
            "inputSha256": self.hashes,
            "note": "Static mapping/symbol existence only. Passing does not prove runtime CLI/SDK parity or that any test passed.",
        }


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(65536), b""):
            digest.update(chunk)
    return digest.hexdigest()


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--root", default=None, help="repository root (default: parent of this script)")
    parser.add_argument("--manifest", default="docs/cli-sdk-semantics.json", help="manifest path relative to root")
    parser.add_argument("--flags-source", default=None, help="override the CLI parser source (for fixtures/tests)")
    parser.add_argument("--allow-incomplete", action="store_true", help="report incomplete entries instead of failing (exit 3)")
    parser.add_argument("--json", action="store_true", help="print a machine-readable JSON report")
    parser.add_argument("--json-report", default=None, help="also write the JSON report to this path")
    parser.add_argument("--quiet", action="store_true", help="suppress the human summary line")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    options = parse_args(sys.argv[1:] if argv is None else argv)
    root = Path(options.root).resolve() if options.root else Path(__file__).resolve().parent.parent
    manifest_path = (root / options.manifest).resolve()
    # When overridden (fixtures/tests) the path stays caller-supplied; otherwise the
    # manifest's own `flagSource` is resolved under the root inside Checker.run().
    flags_source = Path(options.flags_source).resolve() if options.flags_source else None

    checker = Checker(root, manifest_path, flags_source)
    report = checker.run(bool(options.allow_incomplete))

    if options.json:
        print(json.dumps(report, indent=2, ensure_ascii=False))
    if options.json_report:
        Path(options.json_report).write_text(
            json.dumps(report, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
        )
    if not options.quiet and not options.json:
        summary = (
            f"{report['status'].upper()}: {report['flagCount']} flags across {report['entryCount']} entries; "
            f"classification={report['classification']}; incomplete={report['incompleteEntries']}"
        )
        print(summary)
        for error in report["errors"]:
            print(f"FAIL: {error}", file=sys.stderr)
    return int(report["exitCode"])


if __name__ == "__main__":
    raise SystemExit(main())
