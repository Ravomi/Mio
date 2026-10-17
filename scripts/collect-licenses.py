#!/usr/bin/env python3
"""License metadata + verbatim text inventory for the SDK dependency closure.

This tool resolves the *real* dependency graph with
``cargo metadata --locked --offline`` (graph resolution only, no compilation)
and, for every package in that graph, records:

* the package identity (name / version / source kind) without local paths;
* the SPDX-ish license expression exactly as declared in the manifest metadata
  (never rewritten or normalized by this tool);
* verbatim copies of the license-family text files found in the package's
  manifest directory (``LICENSE`` / ``LICENCE`` / ``COPYING`` / ``NOTICE`` /
  ``UNLICENSE`` / ``PATENTS`` and common suffixed variants) plus any path
  declared through Cargo's ``license_file`` field;
* a SHA-256 for every copied byte and the verification that the copy on disk
  hashes back to the exact bytes that were read.

Everything that cannot be proven is reported as ``unknown``. Missing text,
unsupported filenames, symlinks, path escapes, oversized files and exhausted
byte budgets are never silently upgraded to "complete", and no entry is ever
labelled as a legal verification.

This is a metadata and text inventory, **not** a legal review and **not** an
SPDX normalization. It does not decide whether distribution obligations are
met. See ``docs/licenses.md`` for the non-claims that accompany the output.

Artifacts are written only under ``target/license-inventory`` by default:

* ``index.json`` - public-safe index (no absolute local paths, no credentials);
* ``text/`` - verbatim copied license texts, one file per source file;
* ``private/local-paths.json`` - local-only mapping to manifest directories,
  explicitly marked private and never part of the public index.

Safety properties enforced here:

* the output directory must resolve inside the repository, must not be a
  symlink and must not contain a symlink component below the repository root;
* existing evidence is never overwritten (``O_EXCL`` for every file, and a
  pre-existing ``index.json`` aborts the run);
* text files that are symlinks or that resolve outside their manifest
  directory are rejected instead of followed;
* per-file, per-package, global-byte and package-count limits are enforced,
  and hitting one makes the affected package ``unknown`` (the package count
  overflow is a hard error instead of a silent truncation);
* no environment variable is read for credentials and no environment contents
  are recorded.

Reproduce a local inventory with:

    cargo metadata --locked --offline --format-version 1
    python3 scripts/collect-licenses.py --output target/license-inventory

``--all-features`` only changes the resolved graph and still never compiles;
if the local cargo cache is incomplete, ``--fetch`` runs ``cargo fetch
--locked`` first (network) and then continues offline.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

SCRIPT_REL = "scripts/collect-licenses.py"
SCHEMA = "mio-license-inventory/1"
DEFAULT_OUTPUT = "target/license-inventory"
DEFAULT_PRIVATE_REL = "private/local-paths.json"
DEFAULT_INDEX_NAME = "index.json"
DEFAULT_TEXT_DIR = "text"

DISCLAIMER = (
    "Metadata and verbatim text inventory only. This is not a legal review, "
    "not an SPDX normalization, and not a statement that distribution or "
    "attribution obligations have been satisfied. Entries that cannot be "
    "proven from the resolved package contents are reported as unknown and "
    "are never marked verified. License texts are copied byte-for-byte from "
    "the resolved packages; this tool never rewrites license text."
)

DEFAULT_LIMITS = {
    "max_packages": 2000,
    "max_files_per_package": 8,
    "max_file_bytes": 1_048_576,
    "max_total_bytes": 67_108_864,
    "max_filename_chars": 80,
}

# Same conservative allowlist as scripts/check-sdk.py. Tokens outside this set
# are surfaced for manual review and are never treated as a verified closure.
KNOWN_LICENSE_TOKENS = {
    "MIT", "MIT-0", "Apache-2.0", "Unicode-3.0", "Unlicense", "ISC",
    "BSD-2-Clause", "BSD-3-Clause", "CDLA-Permissive-2.0", "Zlib",
    "CC0-1.0", "BSL-1.0", "LGPL-2.1-or-later", "LLVM-exception",
}
LICENSE_OPERATORS = {"AND", "OR", "WITH"}

LICENSE_NAME_RE = re.compile(
    r"^(licen[cs]e|copying|copyright|notice|unlicen[cs]e|patents)([-._].*)?$",
    re.IGNORECASE,
)
SAFE_CHARS_RE = re.compile(r"[^A-Za-z0-9._-]+")


class InventoryError(RuntimeError):
    """Fatal, reportable failure (unsafe path, bad graph, broken evidence)."""


class FileIssue(Exception):
    """A license text candidate that must not be copied as evidence."""

    def __init__(self, reason: str, detail: str | None = None):
        super().__init__(reason)
        self.reason = reason
        self.detail = detail


# ---------------------------------------------------------------------------
# Small helpers
# ---------------------------------------------------------------------------


def _is_within(path: str | os.PathLike[str], root: str | os.PathLike[str]) -> bool:
    try:
        Path(os.path.realpath(path)).relative_to(Path(os.path.realpath(root)))
    except ValueError:
        return False
    return True


def safe_component(value: str, max_chars: int = 80) -> str:
    """Filesystem-safe single path component; never empty, never a traversal."""
    cleaned = SAFE_CHARS_RE.sub("_", value).strip("._")
    if not cleaned:
        cleaned = "_"
    return cleaned[:max_chars]


def sha256_file(path: str | os.PathLike[str]) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(65_536), b""):
            digest.update(chunk)
    return digest.hexdigest()


class Budget:
    """Global byte budget; exhausting it downgrades evidence to unknown."""

    def __init__(self, limit: int):
        self.limit = limit
        self.remaining = limit

    def take(self, amount: int) -> bool:
        if amount > self.remaining:
            return False
        self.remaining -= amount
        return True


# ---------------------------------------------------------------------------
# Metadata
# ---------------------------------------------------------------------------


def cargo_version(repo_root: Path) -> str | None:
    try:
        proc = subprocess.run(
            ["cargo", "--version"], cwd=repo_root, capture_output=True, text=True, timeout=60
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if proc.returncode != 0:
        return None
    return proc.stdout.strip() or None


def cargo_metadata_command(
    offline: bool = True, all_features: bool = False, features: list[str] | None = None
) -> list[str]:
    command = ["cargo", "metadata", "--format-version", "1", "--locked"]
    if all_features:
        command.append("--all-features")
    if features:
        command += ["--features", ",".join(features)]
    if offline:
        command.append("--offline")
    return command


def _run(command: list[str], repo_root: Path, timeout: int) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(
            command, cwd=repo_root, capture_output=True, text=True, timeout=timeout
        )
    except FileNotFoundError as exc:
        raise InventoryError(f"cargo is not available: {exc}") from exc
    except subprocess.TimeoutExpired as exc:
        raise InventoryError(f"command timed out after {timeout}s: {' '.join(command)}") from exc


def load_metadata(
    repo_root: Path,
    *,
    offline: bool = True,
    all_features: bool = False,
    features: list[str] | None = None,
    fetch: bool = False,
    timeout: int = 600,
) -> tuple[dict, list[str]]:
    """Resolve the real dependency graph. Never compiles."""
    if fetch:
        fetch_command = ["cargo", "fetch", "--locked"]
        # `cargo fetch` fetches the locked dependency set and has no feature
        # switches. Apply feature selection only to `cargo metadata` below.
        proc = _run(fetch_command, repo_root, timeout)
        if proc.returncode != 0:
            raise InventoryError(
                "cargo fetch --locked failed; local cache is incomplete "
                f"({proc.returncode}): {proc.stderr.strip()[:2000]}"
            )
    command = cargo_metadata_command(offline=offline, all_features=all_features, features=features)
    proc = _run(command, repo_root, timeout)
    if proc.returncode != 0:
        stderr = proc.stderr.strip()
        hint = ""
        if offline and "offline" in stderr and "download" in stderr:
            hint = (
                " The local cargo cache is incomplete; rerun with --fetch to run "
                "`cargo fetch --locked` for public crates first."
            )
        raise InventoryError(f"cargo metadata failed ({proc.returncode}): {stderr[:2000]}{hint}")
    try:
        document = json.loads(proc.stdout)
    except json.JSONDecodeError as exc:
        raise InventoryError(f"cargo metadata returned invalid JSON: {exc}") from exc
    if not isinstance(document.get("packages"), list):
        raise InventoryError("cargo metadata document has no packages list")
    return document, command


# ---------------------------------------------------------------------------
# Closure and classification
# ---------------------------------------------------------------------------


def dependency_scope(meta: dict) -> tuple[set[str], set[str], set[str]]:
    """Return (workspace_members, shipping_ids, full_ids) from the resolve graph."""
    members = set(meta.get("workspace_members") or [])
    resolve = meta.get("resolve") or {}
    nodes = {node["id"]: node for node in resolve.get("nodes") or []}
    if not nodes:
        raise InventoryError(
            "cargo metadata did not return a resolve graph; refusing to guess the closure"
        )

    def walk(non_dev_only: bool) -> set[str]:
        seen: set[str] = set()
        pending = list(members)
        while pending:
            package_id = pending.pop()
            if package_id in seen:
                continue
            seen.add(package_id)
            for dependency in nodes.get(package_id, {}).get("deps") or []:
                kinds = dependency.get("dep_kinds") or [{}]
                if non_dev_only and all(
                    (kind.get("kind") or "normal") == "dev" for kind in kinds
                ):
                    continue
                pending.append(dependency["pkg"])
        return seen

    return members, walk(non_dev_only=True), walk(non_dev_only=False)


def source_kind(package: dict) -> str:
    source = package.get("source")
    if not source:
        return "workspace_or_path"
    if source.startswith(("registry+", "sparse+")):
        return "registry"
    if source.startswith("git+"):
        return "git"
    if source.startswith("path+"):
        return "path"
    return "other"


def ship_scope(package_id: str, members: set[str], shipping: set[str], full: set[str]) -> str:
    if package_id in members:
        return "workspace"
    if package_id in shipping:
        return "shipping"
    if package_id in full:
        return "dev_only"
    return "unreachable"


def spdx_report(expression: str | None) -> dict:
    """Describe the declared expression. Never normalizes or rewrites it."""
    if expression is None or not expression.strip():
        return {
            "status": "missing",
            "expression": expression,
            "recognized_tokens": [],
            "unrecognized_tokens": [],
        }
    verbatim = expression.strip()
    tokens = [
        token
        for token in re.findall(r"[A-Za-z0-9.+-]+", verbatim)
        if token.upper() not in LICENSE_OPERATORS
    ]
    recognized = sorted({token for token in tokens if token in KNOWN_LICENSE_TOKENS})
    unrecognized = sorted({token for token in tokens if token not in KNOWN_LICENSE_TOKENS})
    if "LicenseRef-" in verbatim:
        status = "license_ref"
    elif "/" in verbatim:
        status = "nonstandard_separator"
    elif unrecognized:
        status = "unrecognized_tokens"
    else:
        status = "declared"
    return {
        "status": status,
        "expression": expression,
        "recognized_tokens": recognized,
        "unrecognized_tokens": unrecognized,
    }


# ---------------------------------------------------------------------------
# Text discovery and copying
# ---------------------------------------------------------------------------


def candidate_names(package: dict, manifest_dir: str, per_file_limit: int) -> list[str]:
    names: list[str] = []
    declared = package.get("license_file")
    if declared:
        names.append(declared)
    try:
        entries = sorted(os.listdir(manifest_dir))
    except OSError as exc:
        raise FileIssue("manifest_dir_unreadable", str(exc)) from exc
    for entry in entries:
        if entry in names:
            continue
        if LICENSE_NAME_RE.match(entry):
            names.append(entry)
        elif re.fullmatch(r"authors(?:\.(?:txt|md))?", entry, re.IGNORECASE):
            # Some published crates (notably r-efi) carry the permission
            # notice and copyright holders in AUTHORS instead of LICENSE.
            # A plain contributor list is not sufficient license evidence.
            try:
                text = read_text_file(os.path.realpath(manifest_dir), os.path.join(manifest_dir, entry), per_file_limit)
                if b"permission is hereby granted" in text.lower():
                    names.append(entry)
            except FileIssue:
                names.append(entry)  # preserve unreadable/unsafe evidence as unknown
    return names


def read_text_file(manifest_dir_real: str, candidate: str, per_file_limit: int) -> bytes:
    """Return verbatim bytes, or raise FileIssue. Never follows symlinks."""
    if os.path.islink(candidate):
        raise FileIssue("symlink_not_followed")
    if not os.path.isfile(candidate):
        raise FileIssue("not_a_regular_file")
    resolved = os.path.realpath(candidate)
    if not _is_within(resolved, manifest_dir_real):
        raise FileIssue("path_escape")
    try:
        size = os.path.getsize(resolved)
    except OSError as exc:
        raise FileIssue("stat_failed", str(exc)) from exc
    if size == 0:
        raise FileIssue("empty_file")
    if size > per_file_limit:
        raise FileIssue("file_exceeds_limit")
    try:
        with open(resolved, "rb") as handle:
            data = handle.read(per_file_limit + 1)
    except OSError as exc:
        raise FileIssue("read_failed", str(exc)) from exc
    if len(data) > per_file_limit:
        raise FileIssue("file_exceeds_limit")
    if not data.strip():
        raise FileIssue("empty_file")
    if b"\x00" in data[:8192]:
        raise FileIssue("binary_not_text")
    return data


def write_exclusive(path: Path, data: bytes) -> None:
    """Create a new file; refuse to overwrite any existing evidence."""
    try:
        descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
    except FileExistsError as exc:
        raise InventoryError(f"refusing to overwrite existing evidence: {path}") from exc
    with os.fdopen(descriptor, "wb") as handle:
        handle.write(data)


def collect_package_text(
    package: dict,
    manifest_dir: str,
    *,
    limits: dict,
    budget: Budget,
    text_dir: Path,
    prefix: str,
) -> tuple[list[dict], str, str | None]:
    """Return (files, text_status, unknown_reason)."""
    manifest_dir_real = os.path.realpath(manifest_dir)
    declared = package.get("license_file")
    try:
        names = candidate_names(package, manifest_dir, limits["max_file_bytes"])
    except FileIssue as issue:
        return [], "unknown", issue.reason

    if not names:
        return [], "missing", "no_license_text_in_manifest_dir"
    if len(names) > limits["max_files_per_package"]:
        return [], "unknown", "too_many_license_files"

    files: list[dict] = []
    first_reason: str | None = None
    for name in names:
        is_declared = bool(declared) and name == declared
        display = os.path.basename(name.rstrip("/")) or name
        candidate = name if os.path.isabs(name) else os.path.join(manifest_dir, name)
        try:
            data = read_text_file(manifest_dir_real, candidate, limits["max_file_bytes"])
        except FileIssue as issue:
            files.append(
                {
                    "name": display,
                    "role": "declared_license_file" if is_declared else "convention",
                    "status": "unknown",
                    "reason": issue.reason,
                    "copied_as": None,
                    "bytes": None,
                    "sha256": None,
                    "copy_hash_match": False,
                }
            )
            first_reason = first_reason or issue.reason
            continue
        if not budget.take(len(data)):
            files.append(
                {
                    "name": display,
                    "role": "declared_license_file" if is_declared else "convention",
                    "status": "unknown",
                    "reason": "total_bytes_budget_exhausted",
                    "copied_as": None,
                    "bytes": len(data),
                    "sha256": hashlib.sha256(data).hexdigest(),
                    "copy_hash_match": False,
                }
            )
            first_reason = first_reason or "total_bytes_budget_exhausted"
            continue
        digest = hashlib.sha256(data).hexdigest()
        relative_name = f"{prefix}-{safe_component(name, limits['max_filename_chars'])}"
        destination = text_dir / relative_name
        if not _is_within(destination, text_dir):
            raise InventoryError(f"refusing unsafe text destination: {relative_name}")
        write_exclusive(destination, data)
        copied_digest = sha256_file(destination)
        if copied_digest != digest:
            raise InventoryError(
                f"copied text does not hash back to the source bytes: {relative_name}"
            )
        files.append(
            {
                "name": display,
                "role": "declared_license_file" if is_declared else "convention",
                "status": "collected",
                "reason": None,
                "copied_as": f"{DEFAULT_TEXT_DIR}/{relative_name}",
                "bytes": len(data),
                "sha256": digest,
                "copy_hash_match": True,
            }
        )

    if first_reason is not None:
        return files, "unknown", first_reason
    return files, "collected", None


# ---------------------------------------------------------------------------
# Output directory safety
# ---------------------------------------------------------------------------


def prepare_output_dir(out_dir: Path, repo_root: Path) -> Path:
    repo_real = os.path.realpath(repo_root)
    if out_dir.is_symlink():
        raise InventoryError(f"output directory is a symlink: {out_dir}")
    resolved = os.path.realpath(out_dir)
    if not _is_within(resolved, repo_real):
        raise InventoryError(
            f"output directory escapes the repository root: {out_dir} -> {resolved}"
        )
    # Reject a symlinked component between the repository root and the output.
    cursor = Path(resolved)
    while True:
        if cursor.is_symlink():
            raise InventoryError(f"output path contains a symlink component: {cursor}")
        if os.path.realpath(cursor) == repo_real or cursor == cursor.parent:
            break
        cursor = cursor.parent
    if out_dir.exists() and not out_dir.is_dir():
        raise InventoryError(f"output path exists and is not a directory: {out_dir}")
    if (out_dir / DEFAULT_INDEX_NAME).exists():
        raise InventoryError(
            f"refusing to overwrite existing evidence: {out_dir / DEFAULT_INDEX_NAME} exists"
        )
    out_dir.mkdir(parents=True, exist_ok=True)
    return resolved  # type: ignore[return-value]


# ---------------------------------------------------------------------------
# Collection
# ---------------------------------------------------------------------------


def build_index(
    meta: dict,
    *,
    repo_root: Path,
    out_dir: Path,
    limits: dict,
    command: list[str],
    cargo_release: str | None,
    generated_at: str,
    private_path: Path | None,
) -> tuple[dict, dict, list[str]]:
    members, shipping, full = dependency_scope(meta)
    packages = sorted(
        meta["packages"],
        key=lambda package: (package["name"], package["version"], package.get("source") or ""),
    )
    if len(packages) > limits["max_packages"]:
        raise InventoryError(
            f"package count {len(packages)} exceeds max_packages={limits['max_packages']}; "
            "refusing a partial inventory"
        )

    text_dir = out_dir / DEFAULT_TEXT_DIR
    text_dir.mkdir(parents=True, exist_ok=True)
    budget = Budget(limits["max_total_bytes"])
    manifest_dirs: dict[str, str] = {}
    records: list[dict] = []
    totals = {
        "packages": len(packages),
        "workspace_packages": 0,
        "shipping_packages": 0,
        "dev_only_packages": 0,
        "unreachable_packages": 0,
        "text_collected": 0,
        "text_missing": 0,
        "text_unknown": 0,
        "files_copied": 0,
        "bytes_copied": 0,
        "spdx_standard": 0,
        "spdx_nonstandard_separator": 0,
        "spdx_unrecognized_tokens": 0,
        "spdx_license_ref": 0,
        "spdx_missing": 0,
        "unknown_packages": 0,
    }

    for position, package in enumerate(packages, start=1):
        manifest_path = package.get("manifest_path")
        if not manifest_path:
            raise InventoryError(
                f"package {package['name']} {package['version']} has no manifest_path"
            )
        manifest_dir = os.path.dirname(os.path.abspath(manifest_path))
        if not os.path.isdir(manifest_dir):
            raise InventoryError(f"manifest directory is missing: {manifest_dir}")
        scope = ship_scope(package["id"], members, shipping, full)
        prefix = f"{position:04d}-{safe_component(package['name'])}-{safe_component(package['version'])}"

        files, text_status, text_reason = collect_package_text(
            package,
            manifest_dir,
            limits=limits,
            budget=budget,
            text_dir=text_dir,
            prefix=prefix,
        )
        spdx = spdx_report(package.get("license"))
        if text_status == "collected":
            totals["text_collected"] += 1
        elif text_status == "missing":
            totals["text_missing"] += 1
        else:
            totals["text_unknown"] += 1
        if spdx["status"] == "missing":
            totals["spdx_missing"] += 1
        elif spdx["status"] == "nonstandard_separator":
            totals["spdx_nonstandard_separator"] += 1
        elif spdx["status"] == "unrecognized_tokens":
            totals["spdx_unrecognized_tokens"] += 1
        elif spdx["status"] == "license_ref":
            totals["spdx_license_ref"] += 1
        else:
            totals["spdx_standard"] += 1
        copied = [entry for entry in files if entry["status"] == "collected"]
        totals["files_copied"] += len(copied)
        totals["bytes_copied"] += sum(entry["bytes"] or 0 for entry in copied)
        totals[scope + "_packages"] = totals.get(scope + "_packages", 0) + 1
        if text_status != "collected":
            # "unknown" bookkeeping only; it never implies a legal conclusion.
            totals["unknown_packages"] += 1

        records.append(
            {
                "name": package["name"],
                "version": package["version"],
                "source_kind": source_kind(package),
                "source_id": package.get("source"),
                "ship_scope": scope,
                "license_declared": package.get("license"),
                "license_file_declared": package.get("license_file"),
                "spdx": spdx,
                "text_status": text_status,
                "text_unknown_reason": text_reason,
                "files": files,
            }
        )
        manifest_dirs[f"{package['name']}@{package['version']}@{scope}"] = manifest_path

    index = {
        "schema": SCHEMA,
        "generated_at": generated_at,
        "tool": {
            "script": SCRIPT_REL,
            "python": sys.version.split()[0],
            "inventory_only": True,
        },
        "disclaimer": DISCLAIMER,
        "scope": {
            "cargo_metadata_command": command,
            "cargo_version": cargo_release,
            "features": "all" if "--all-features" in command else "default",
            "metadata_package_count": len(packages),
            "workspace_member_count": len(members),
            "shipping_closure_package_count": len(shipping),
        },
        "limits": dict(limits),
        "reproduce": [
            " ".join(command),
            f"python3 {SCRIPT_REL} --output {DEFAULT_OUTPUT}",
        ],
        "totals": totals,
        "packages": records,
    }
    private = {
        "private": True,
        "notice": (
            "Local-only report. Contains absolute local paths; do not publish, "
            "commit or ship. The public index.json intentionally omits every "
            "local path."
        ),
        "generated_at": generated_at,
        "cargo_metadata_command": command,
        "repository_root": str(repo_root),
        "output_directory": str(out_dir),
        "private_report_path": str(private_path) if private_path else None,
        "manifest_directories": manifest_dirs,
    }
    return index, private, sorted(manifest_dirs.values())


def check_public_index(index: dict, forbidden: list[str]) -> None:
    blob = json.dumps(index, ensure_ascii=False)
    for needle in forbidden:
        if needle and needle in blob:
            raise InventoryError(f"public index leaked a local path: {needle}")


def collect(
    meta: dict,
    *,
    repo_root: Path,
    out_dir: Path,
    limits: dict | None = None,
    command: list[str] | None = None,
    cargo_release: str | None = None,
    private_report: bool = True,
    generated_at: str | None = None,
) -> dict:
    limits = dict(limits or DEFAULT_LIMITS)
    command = list(command or cargo_metadata_command(offline=True))
    generated_at = generated_at or datetime.now(timezone.utc).replace(microsecond=0).isoformat()
    resolved_out = prepare_output_dir(out_dir, repo_root)
    private_path = (Path(resolved_out) / DEFAULT_PRIVATE_REL) if private_report else None

    index, private, manifest_dirs = build_index(
        meta,
        repo_root=repo_root,
        out_dir=Path(resolved_out),
        limits=limits,
        command=command,
        cargo_release=cargo_release,
        generated_at=generated_at,
        private_path=private_path,
    )
    # The public index must never contain a local absolute path.
    check_public_index(index, [str(repo_root), str(resolved_out), *manifest_dirs])

    write_exclusive(
        Path(resolved_out) / DEFAULT_INDEX_NAME,
        (json.dumps(index, ensure_ascii=False, indent=2, sort_keys=False) + "\n").encode("utf-8"),
    )
    if private_path is not None:
        private_path.parent.mkdir(parents=True, exist_ok=True)
        write_exclusive(
            private_path,
            (json.dumps(private, ensure_ascii=False, indent=2) + "\n").encode("utf-8"),
        )
    return index


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Collect dependency license metadata and verbatim license texts into "
            "target/license-inventory (inventory only, never a legal review)."
        )
    )
    parser.add_argument("--output", default=DEFAULT_OUTPUT, help="output directory inside the repository")
    parser.add_argument("--all-features", action="store_true", help="resolve the graph with all features")
    parser.add_argument("--features", default="", help="comma-separated feature list for graph resolution")
    parser.add_argument("--fetch", action="store_true", help="run `cargo fetch --locked` first (network)")
    parser.add_argument("--online", action="store_true", help="do not pass --offline to cargo metadata")
    parser.add_argument("--timeout", type=int, default=600, help="cargo timeout in seconds")
    parser.add_argument("--no-private-report", action="store_true", help="skip the private local-path report")
    parser.add_argument("--max-file-bytes", type=int, default=DEFAULT_LIMITS["max_file_bytes"])
    parser.add_argument("--max-total-bytes", type=int, default=DEFAULT_LIMITS["max_total_bytes"])
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    repo_root = Path(__file__).resolve().parent.parent
    out_dir = Path(args.output)
    if not out_dir.is_absolute():
        out_dir = repo_root / out_dir
    features = [item for item in (part.strip() for part in args.features.split(",")) if item]
    limits = dict(DEFAULT_LIMITS)
    limits["max_file_bytes"] = args.max_file_bytes
    limits["max_total_bytes"] = args.max_total_bytes

    started = time.monotonic()
    try:
        meta, command = load_metadata(
            repo_root,
            offline=not args.online,
            all_features=args.all_features,
            features=features,
            fetch=args.fetch,
            timeout=args.timeout,
        )
        index = collect(
            meta,
            repo_root=repo_root,
            out_dir=out_dir,
            limits=limits,
            command=command,
            cargo_release=cargo_version(repo_root),
            private_report=not args.no_private_report,
        )
    except InventoryError as exc:
        print(f"collect-licenses: {exc}", file=sys.stderr)
        return 2

    totals = index["totals"]
    index_path = out_dir / DEFAULT_INDEX_NAME
    text_dir = out_dir / DEFAULT_TEXT_DIR
    print(
        "collect-licenses: {packages} packages; texts collected {collected}, missing {missing}, "
        "unknown {unknown}; files copied {files} ({bytes} bytes)".format(
            packages=totals["packages"],
            collected=totals["text_collected"],
            missing=totals["text_missing"],
            unknown=totals["text_unknown"],
            files=totals["files_copied"],
            bytes=totals["bytes_copied"],
        )
    )
    print(
        "collect-licenses: SPDX declared {standard}, nonstandard {nonstandard}, "
        "unrecognized {unrecognized}, licenseRef {refs}, missing {spdx_missing}".format(
            standard=totals["spdx_standard"],
            nonstandard=totals["spdx_nonstandard_separator"],
            unrecognized=totals["spdx_unrecognized_tokens"],
            refs=totals["spdx_license_ref"],
            spdx_missing=totals["spdx_missing"],
        )
    )
    print(f"collect-licenses: public index {index_path}")
    print(f"collect-licenses: verbatim texts {text_dir}")
    if not args.no_private_report:
        print(f"collect-licenses: private local-path report {out_dir / DEFAULT_PRIVATE_REL}")
    print(f"collect-licenses: inventory only, not a legal review ({time.monotonic() - started:.1f}s)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
