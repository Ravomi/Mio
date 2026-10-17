#!/usr/bin/env python3
"""Offline SDK contract checks for the Mio workspace (REQ-SDK / REQ-DISTRIBUTION).

The checks resolve the *real* dependency graph with
``cargo metadata --locked --offline`` instead of re-reading manifests, then
audit:

1. dependency direction across the three-layer SDK
   (``mio-core`` -> ``mio-agent`` -> ``mio-coding-agent``) and the helper
   crates (``mio-http``, ``mio-derive``, ``mio-reqwest``, ``mio-tungstenite``);
2. publish/version closure of in-workspace path dependencies, plus packaging
   hygiene of the crate sources (reserved cargo package artifacts);
3. license metadata closure of every package in the resolved graph;
4. the declared feature matrix against features that actually exist.

No external credentials are used: the static checks are offline and the
optional ``--run-local`` matrix only compiles with a locked, offline cargo.

Exit status is 0 when no gate fails. Dependency-direction and license-metadata
violations fail by default. Publish version closure is reported as findings and
only fails under ``--strict-publish`` because the workspace manifests are out
of scope for this baseline.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import time
import tomllib
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_LOG_DIR = Path("/tmp/mio-sdk-qa")

# ---------------------------------------------------------------------------
# Policy: SDK layering (REQ-SDK).
#
# Lower rank is closer to the foundation. A normal (non-dev, non-build)
# dependency edge is only valid when it points strictly downward. Terminal,
# RPC, approval, runner and workspace code all live in ``mio-coding-agent``,
# so "core must not depend on the terminal/workspace" is the same rule as
# "core must not depend on the coding layer".
# ---------------------------------------------------------------------------

LAYERS: dict[str, tuple[int, str]] = {
    "mio-derive": (0, "proc-macro foundation"),
    "mio-http": (0, "transport contracts"),
    "mio-reqwest": (1, "HTTP transport helper (depends only on mio-http)"),
    "mio-tungstenite": (1, "WebSocket transport helper (depends only on mio-http)"),
    "mio-core": (2, "model/provider contracts and protocol"),
    "mio-agent": (3, "agent runtime"),
    "mio-coding-agent": (4, "coding layer: CLI, terminal, RPC, approval, runner, workspace"),
}

# Explicit invariants named in TODO/PRD, used to make failures reviewable.
NAMED_INVARIANTS: dict[str, tuple[str, ...]] = {
    "mio-core": ("mio-agent", "mio-coding-agent"),
    "mio-agent": ("mio-coding-agent",),
}

# ---------------------------------------------------------------------------
# Policy: feature matrix (REQ-SDK). Every entry must reference features that
# exist in the resolved metadata; unknown names are a hard failure so this
# file cannot drift into documenting features the crates do not have.
# ``network`` marks combos whose transitive crates are not guaranteed to be in
# a warm local cargo cache, so the offline local run reports them as skipped
# instead of pretending they were verified.
# ---------------------------------------------------------------------------

FEATURE_MATRIX: list[dict] = [
    # ---- examples: minimal and full host embed ----
    {"group": "examples", "package": "mio-agent", "args": ["--examples"],
     "purpose": "minimal SDK consumer: agent runtime only, no coding layer"},
    {"group": "examples", "package": "mio-coding-agent", "args": ["--example", "host"],
     "purpose": "full host embed: Host/AgentEvent/Services API compiles"},
    {"group": "examples", "package": "mio-coding-agent", "args": ["--example", "host_loopback"],
     "purpose": "complete offline host example: run_with_tools_tracked with real TaskHandle/journal/bound approval"},
    # ---- helper crate feature matrix ----
    {"group": "helpers", "package": "mio-derive", "args": ["--examples"], "purpose": "proc-macro examples"},
    {"group": "helpers", "package": "mio-http", "features": [], "purpose": "transport contracts, no optional features"},
    {"group": "helpers", "package": "mio-http", "features": ["websocket"], "purpose": "websocket contract"},
    {"group": "helpers", "package": "mio-http", "features": ["test-utils"], "purpose": "test transport helpers"},
    {"group": "helpers", "package": "mio-reqwest", "no_default_features": True, "features": [],
     "purpose": "HTTP transport without a TLS backend"},
    {"group": "helpers", "package": "mio-reqwest", "features": ["rustls"], "purpose": "default rustls transport"},
    {"group": "helpers", "package": "mio-reqwest", "no_default_features": True, "features": ["native-tls"],
     "purpose": "platform native TLS transport"},
    {"group": "helpers", "package": "mio-reqwest", "features": ["socks"], "purpose": "SOCKS proxy support"},
    {"group": "helpers", "package": "mio-reqwest", "features": ["reqwest-middleware"],
     "purpose": "middleware stack over rustls", "network": True},
    {"group": "helpers", "package": "mio-reqwest", "no_default_features": True,
     "features": ["reqwest-middleware-native-tls"], "purpose": "middleware stack over native TLS", "network": True},
    {"group": "helpers", "package": "mio-tungstenite", "no_default_features": True, "features": [],
     "purpose": "websocket backend without a TLS backend"},
    {"group": "helpers", "package": "mio-tungstenite", "features": ["rustls"], "purpose": "default rustls websocket"},
    {"group": "helpers", "package": "mio-tungstenite", "no_default_features": True, "features": ["native-tls"],
     "purpose": "platform native TLS websocket"},
    {"group": "helpers", "package": "mio-core", "no_default_features": True, "features": [],
     "purpose": "core with no providers/transports"},
    {"group": "helpers", "package": "mio-core", "features": [], "purpose": "core defaults (derive, rustls)"},
    {"group": "helpers", "package": "mio-core", "no_default_features": True, "features": ["reqwest"],
     "purpose": "core + reqwest transport"},
    {"group": "helpers", "package": "mio-core", "no_default_features": True, "features": ["reqwest", "native-tls"],
     "purpose": "core + reqwest transport + platform native TLS"},
    {"group": "helpers", "package": "mio-core", "features": ["tungstenite"],
     "purpose": "core websocket protocol + tungstenite backend"},
    {"group": "helpers", "package": "mio-core", "features": ["audio", "image", "test-utils"],
     "purpose": "capability flags and test utilities"},
    {"group": "helpers", "package": "mio-core", "features": ["pdf"], "purpose": "PDF parsing", "network": True},
    {"group": "helpers", "package": "mio-core", "features": ["epub"], "purpose": "EPUB parsing", "network": True},
    {"group": "helpers", "package": "mio-agent", "no_default_features": True, "features": [],
     "purpose": "agent runtime without derive"},
    {"group": "helpers", "package": "mio-agent", "features": ["image", "audio"],
     "purpose": "agent capability flags"},
    {"group": "helpers", "package": "mio-agent", "features": ["test-utils"], "purpose": "agent test utilities"},
]

# Common SPDX identifiers plus the exception id used in this graph. Tokens
# outside this set are surfaced for manual review, never silently accepted as
# a verified license closure.
KNOWN_LICENSE_TOKENS = {
    "MIT", "MIT-0", "Apache-2.0", "Unicode-3.0", "Unlicense", "ISC",
    "BSD-2-Clause", "BSD-3-Clause", "CDLA-Permissive-2.0", "Zlib",
    "CC0-1.0", "BSL-1.0", "LGPL-2.1-or-later", "LLVM-exception",
}
LICENSE_OPERATORS = {"AND", "OR", "WITH"}

# ---------------------------------------------------------------------------
# Metadata loading
# ---------------------------------------------------------------------------


def run_cargo_metadata(offline: bool = True, timeout: int = 300) -> dict:
    """Resolve the workspace graph with cargo metadata. Raises RuntimeError."""
    cmd = ["cargo", "metadata", "--locked", "--format-version", "1"]
    if offline:
        cmd.append("--offline")
    try:
        proc = subprocess.run(
            cmd, cwd=str(ROOT), capture_output=True, text=True, timeout=timeout, check=False
        )
    except FileNotFoundError as exc:  # pragma: no cover - environment dependent
        raise RuntimeError(f"cargo not found: {exc}") from exc
    except subprocess.TimeoutExpired as exc:  # pragma: no cover
        raise RuntimeError(f"cargo metadata timed out after {timeout}s") from exc
    if proc.returncode != 0:
        detail = (proc.stderr or proc.stdout or "").strip()
        raise RuntimeError(f"cargo metadata failed (rc={proc.returncode}): {detail}")
    return json.loads(proc.stdout)


def load_metadata(path: str | None, offline: bool = True) -> dict:
    if path:
        source = Path(path)
        try:
            return json.loads(source.read_text(encoding="utf-8"))
        except OSError as exc:
            raise RuntimeError(f"cannot read metadata file {source}: {exc}") from exc
        except json.JSONDecodeError as exc:
            raise RuntimeError(f"metadata file {source} is not valid JSON: {exc}") from exc
    return run_cargo_metadata(offline=offline)


def by_id(meta: dict) -> dict[str, dict]:
    return {pkg["id"]: pkg for pkg in meta["packages"]}


def workspace_ids(meta: dict) -> set[str]:
    return set(meta["workspace_members"])


# ---------------------------------------------------------------------------
# Resolved-graph edges
# ---------------------------------------------------------------------------

def resolved_edges(meta: dict) -> list[dict]:
    """Return resolved dependency edges from the cargo resolve graph.

    Only the ``resolve`` section is read for direction checks, so the result
    reflects what cargo actually resolved (including optional features that
    are active in the locked graph), not just the manifest text.
    """
    packages = by_id(meta)
    edges: list[dict] = []
    for node in meta.get("resolve", {}).get("nodes", []):
        source = packages.get(node["id"])
        if source is None:
            continue
        for dep in node.get("deps", []):
            target = packages.get(dep["pkg"])
            if target is None:
                continue
            kinds = sorted({dk.get("kind") for dk in dep.get("dep_kinds", [])}, key=lambda k: k or "")
            normal = any(kind is None for kind in kinds)
            edges.append(
                {
                    "from": source["name"],
                    "from_id": source["id"],
                    "to": target["name"],
                    "to_id": target["id"],
                    "kinds": kinds,
                    "normal": normal,
                }
            )
    return edges


# ---------------------------------------------------------------------------
# Gate 1: dependency direction
# ---------------------------------------------------------------------------


def check_dependency_direction(meta: dict) -> dict:
    """Audit normal dependency edges against the SDK layer policy.

    Uses the resolved graph, so a reversed edge is caught even when the
    manifest entry is absent or reshaped (see tests/sdk_contract.py).
    """
    ws = workspace_ids(meta)
    packages = by_id(meta)
    violations: list[dict] = []
    normal_edges: list[dict] = []

    workspace_names = sorted(
        packages[member]["name"] for member in ws if member in packages
    )
    for name in workspace_names:
        if name not in LAYERS:
            violations.append(
                {
                    "kind": "unclassified-workspace-crate",
                    "from": name,
                    "to": None,
                    "reason": (
                        "workspace crate is missing from the SDK layer policy; "
                        "classify it before adding dependencies"
                    ),
                }
            )

    for edge in resolved_edges(meta):
        if edge["from_id"] not in ws:
            continue
        if not edge["normal"]:
            continue
        normal_edges.append({"from": edge["from"], "to": edge["to"]})
        source_rank = LAYERS.get(edge["from"])
        target_rank = LAYERS.get(edge["to"], None)
        if source_rank is None or edge["to"] not in LAYERS:
            # Either reported above as an unclassified member, or the target is
            # a registry crate (not an SDK-direction edge).
            continue
        if target_rank[0] >= source_rank[0]:
            same = edge["from"] == edge["to"]
            named = edge["to"] in NAMED_INVARIANTS.get(edge["from"], ())
            violations.append(
                {
                    "kind": "self-dependency" if same else "upward-dependency",
                    "from": edge["from"],
                    "to": edge["to"],
                    "from_layer": source_rank[0],
                    "to_layer": target_rank[0],
                    "invariant": f"{edge['from']} must not depend on {edge['to']}" if named else None,
                    "reason": (
                        f"{edge['from']} (L{source_rank[0]}) must not depend on "
                        f"{edge['to']} (L{target_rank[0]}); dependencies may only point downward"
                    ),
                }
            )

    missing = sorted(set(LAYERS) - set(workspace_names))
    status = "pass" if not violations else "fail"
    return {
        "status": status,
        "normal_edge_count": len(normal_edges),
        "workspace_crate_count": len(ws),
        "violations": violations,
        "policy_crates_absent_from_workspace": missing,
        "named_invariants": {k: list(v) for k, v in NAMED_INVARIANTS.items()},
        "layers": {name: {"rank": rank, "purpose": purpose} for name, (rank, purpose) in LAYERS.items()},
    }


# ---------------------------------------------------------------------------
# Gate 2: publish version closure
# ---------------------------------------------------------------------------


def _parse_version(text: str) -> tuple[int, int, int]:
    core = text.split("+", 1)[0].split("-", 1)[0]
    parts: list[int] = []
    for piece in core.split("."):
        piece = piece.strip()
        parts.append(int(piece) if piece.isdigit() else 0)
    while len(parts) < 3:
        parts.append(0)
    return (parts[0], parts[1], parts[2])


def _compare(a: tuple[int, int, int], b: tuple[int, int, int]) -> int:
    return (a > b) - (a < b)


def _bound(version: str, bump: int) -> tuple[int, int, int]:
    major, minor, patch = _parse_version(version)
    if bump == 2:
        return (major + 1, 0, 0)
    if bump == 1:
        return (major, minor + 1, 0)
    return (major, minor, patch + 1)


def _caret_upper(text: str) -> tuple[int, int, int]:
    major, minor, patch = _parse_version(text)
    precision = len(text.split("."))
    if major or precision == 1:
        return (major + 1, 0, 0)
    if minor or precision == 2:
        return (0, minor + 1, 0)
    return (0, 0, patch + 1)


def _clause_matches(version: tuple[int, int, int], clause: str) -> bool:
    clause = clause.strip()
    if clause in ("", "*"):
        return True
    if not re.fullmatch(r"(?:[=^~]|[<>]=?)?[0-9]+(?:\.[0-9]+){0,2}", clause):
        return False
    if clause[0] in "=^~<>":
        op = clause[0]
        if op in "<>" and len(clause) > 1 and clause[1] == "=":
            op, rest = clause[:2], clause[2:]
        else:
            rest = clause[1:]
        target = _parse_version(rest)
        if op == "=":
            return version == target
        if op == ">=":
            return _compare(version, target) >= 0
        if op == "<=":
            return _compare(version, target) <= 0
        if op == ">":
            return _compare(version, target) > 0
        if op == "<":
            return _compare(version, target) < 0
        if op == "^":
            return target <= version < _caret_upper(rest)
        if op == "~":
            return target <= version < _bound(rest, 2 if len(rest.split(".")) == 1 else 1)
    # Bare requirement keeps cargo's caret semantics.
    target = _parse_version(clause)
    return target <= version < _caret_upper(clause)


def semver_matches(version: str, req: str) -> bool:
    """Minimal matcher for the requirement forms cargo accepts here."""
    # This local audit handles stable numeric requirements only. Fail closed
    # for prereleases and unrecognized syntax, rather than stripping suffixes
    # and accidentally approving an incompatible release. Cargo remains the
    # authority for the resolved graph and more complex requirements.
    if not re.fullmatch(r"[0-9]+(?:\.[0-9]+){0,2}", version):
        return False
    if req.strip() != "*" and not re.fullmatch(r"[0-9.,=^~<>\s]+", req):
        return False
    clauses = [c.strip() for c in req.split(",") if c.strip()]
    if not clauses:
        return True
    current = _parse_version(version)
    return all(_clause_matches(current, clause) for clause in clauses)


def check_version_closure(meta: dict) -> dict:
    """Audit publishable path dependencies for a version requirement.

    ``cargo package``/``cargo publish`` strips path dependencies and requires a
    matching registry requirement; a path-only dependency (cargo default req
    ``*``) or a mismatched requirement blocks the crates.io path closure.
    """
    ws = workspace_ids(meta)
    packages = by_id(meta)
    findings: list[dict] = []
    checked = 0

    for pkg in meta["packages"]:
        if pkg["id"] not in ws:
            continue
        publish = pkg.get("publish")
        publishable = not (isinstance(publish, list) and len(publish) == 0)
        for dep in pkg.get("dependencies", []):
            if dep.get("kind") == "dev":
                # dev-dependencies are not published and may legitimately omit versions.
                continue
            path = dep.get("path")
            if not path:
                continue
            target = _resolve_path_dep(packages, ws, path)
            if target is None:
                findings.append(
                    {
                        "package": pkg["name"],
                        "dependency": dep["name"],
                        "kind": dep.get("kind"),
                        "path": path,
                        "reason": "path dependency does not resolve to a workspace member",
                    }
                )
                continue
            if target["id"] not in ws:
                continue
            checked += 1
            req = dep.get("req", "*")
            if req in ("", "*"):
                findings.append(
                    {
                        "package": pkg["name"],
                        "dependency": dep["name"],
                        "kind": dep.get("kind"),
                        "req": req,
                        "target_version": target["version"],
                        "publishable": publishable,
                        "reason": (
                            "in-workspace path dependency has no version requirement; "
                            "cargo package/publish requires one that matches the workspace version"
                        ),
                    }
                )
            elif not semver_matches(target["version"], req):
                findings.append(
                    {
                        "package": pkg["name"],
                        "dependency": dep["name"],
                        "kind": dep.get("kind"),
                        "req": req,
                        "target_version": target["version"],
                        "publishable": publishable,
                        "reason": (
                            f"version requirement {req!r} does not match workspace version "
                            f"{target['version']}"
                        ),
                    }
                )
    return {
        "status": "pass" if not findings else "findings",
        "checked_in_workspace_path_deps": checked,
        "publish_ready": not findings,
        "findings": findings,
    }


def _resolve_path_dep(packages: dict[str, dict], ws: set[str], path: str) -> dict | None:
    resolved = str(Path(path).resolve())
    for pkg in packages.values():
        if pkg["id"] not in ws:
            continue
        manifest_dir = str(Path(pkg["manifest_path"]).resolve().parent)
        if manifest_dir == resolved:
            return pkg
    return None


# ---------------------------------------------------------------------------
# Gate 3: packaging hygiene (cargo package/publish source readiness)
# ---------------------------------------------------------------------------

# ``cargo package``/``cargo publish`` reject reserved file names in the crate
# source. These two files came in with the upstream vendoring and are tracked
# in this repository.
RESERVED_PACKAGE_ARTIFACTS = ("Cargo.toml.orig", ".cargo_vcs_info.json")


def check_packaging_hygiene(meta: dict) -> dict:
    """Detect source-tree artifacts that make ``cargo package`` fail.

    This is what blocks the crates.io path today; it is a findings-only gate by
    default and a hard failure under ``--strict-publish``.
    """
    ws = workspace_ids(meta)
    findings: list[dict] = []
    for pkg in meta["packages"]:
        if pkg["id"] not in ws:
            continue
        crate_dir = Path(pkg["manifest_path"]).resolve().parent
        try:
            manifest = tomllib.loads(Path(pkg["manifest_path"]).read_text())
            package_config = manifest.get("package", {})
        except (OSError, tomllib.TOMLDecodeError):
            package_config = {}
        # Archival vendoring metadata may remain in the checkout. Recognize
        # only exact root exclusions, conservatively leaving complex patterns
        # for Cargo/manual verification. `include` takes precedence over
        # `exclude`, so exclusions alone cannot prove safety when it is set.
        exclusions = package_config.get("exclude", []) if "include" not in package_config else []
        for name in RESERVED_PACKAGE_ARTIFACTS:
            candidate = crate_dir / name
            if not candidate.exists():
                continue
            if any(pattern in (name, f"/{name}") for pattern in exclusions):
                continue
            try:
                shown = candidate.relative_to(ROOT)
            except ValueError:
                shown = candidate
            findings.append(
                {
                    "package": pkg["name"],
                    "file": str(shown),
                    "reason": (
                        f"reserved file {name} is present in the crate source; "
                        "cargo package/publish rejects it with 'invalid inclusion of "
                        f"reserved file name {name} in package source'"
                    ),
                }
            )
    return {
        "status": "pass" if not findings else "findings",
        "findings": findings,
        "reserved_names": list(RESERVED_PACKAGE_ARTIFACTS),
    }


# ---------------------------------------------------------------------------
# Gate 4: license metadata closure
# ---------------------------------------------------------------------------


def _license_tokens(expr: str) -> list[str]:
    tokens = re.findall(r"[A-Za-z0-9.+-]+", expr)
    return [t for t in tokens if t not in LICENSE_OPERATORS]


def check_license_closure(meta: dict, allow_unknown: bool) -> dict:
    unknown: list[dict] = []
    license_file_only: list[dict] = []
    expressions: dict[str, int] = {}
    review: list[dict] = []

    for pkg in meta["packages"]:
        license_expr = pkg.get("license")
        license_file = pkg.get("license_file")
        if not license_expr and not license_file:
            unknown.append({"name": pkg["name"], "version": pkg["version"], "source": pkg.get("source")})
            continue
        if not license_expr and license_file:
            license_file_only.append(
                {"name": pkg["name"], "version": pkg["version"], "license_file": license_file}
            )
            expressions["<license-file>"] = expressions.get("<license-file>", 0) + 1
            continue
        expressions[license_expr] = expressions.get(license_expr, 0) + 1
        legacy = "/" in license_expr
        odd_tokens = sorted(t for t in _license_tokens(license_expr) if t not in KNOWN_LICENSE_TOKENS)
        if legacy or odd_tokens:
            review.append(
                {
                    "name": pkg["name"],
                    "version": pkg["version"],
                    "license": license_expr,
                    "legacy_separator": legacy,
                    "unrecognized_tokens": odd_tokens,
                }
            )

    verified = not unknown
    if unknown and allow_unknown:
        status = "unknown"
    elif unknown:
        status = "fail"
    else:
        status = "pass"
    return {
        "status": status,
        "license_metadata_complete": verified,
        "package_count": len(meta["packages"]),
        "missing_metadata": unknown,
        "missing_metadata_count": len(unknown),
        "license_file_only": license_file_only,
        "expressions": dict(sorted(expressions.items(), key=lambda kv: (-kv[1], kv[0]))),
        "review": review,
    }


# ---------------------------------------------------------------------------
# Gate 5: feature matrix
# ---------------------------------------------------------------------------


def existing_features(meta: dict) -> dict[str, set[str]]:
    packages = by_id(meta)
    ws = workspace_ids(meta)
    return {
        pkg["name"]: set(pkg.get("features", {}))
        for pkg in packages.values()
        if pkg["id"] in ws
    }


def validate_feature_matrix(meta: dict, matrix: list[dict] | None = None) -> dict:
    matrix = FEATURE_MATRIX if matrix is None else matrix
    features = existing_features(meta)
    errors: list[dict] = []
    for entry in matrix:
        package = entry["package"]
        if package not in features:
            errors.append({"package": package, "feature": None, "reason": "not a workspace crate"})
            continue
        for feature in entry.get("features", []):
            if feature not in features[package]:
                errors.append(
                    {
                        "package": package,
                        "feature": feature,
                        "available": sorted(features[package]),
                        "reason": "feature does not exist in the resolved metadata",
                    }
                )
    return {
        "status": "pass" if not errors else "fail",
        "entries": len(matrix),
        "unknown_features": errors,
        "matrix": [
            {
                "group": e["group"],
                "package": e["package"],
                "features": e.get("features", []),
                "no_default_features": bool(e.get("no_default_features")),
                "network": bool(e.get("network")),
                "purpose": e.get("purpose", ""),
            }
            for e in matrix
        ],
    }


# ---------------------------------------------------------------------------
# Optional local execution matrix
# ---------------------------------------------------------------------------


def _cargo_cmd(entry: dict, offline: bool, toolchain: str) -> list[str]:
    cargo = "cargo"
    if toolchain:
        cargo = f"cargo +{toolchain}"
    cmd = cargo.split() + ["check", "--locked"]
    if offline:
        cmd.append("--offline")
    cmd += ["-p", entry["package"]]
    if entry.get("no_default_features"):
        cmd.append("--no-default-features")
    features = entry.get("features")
    if features:
        cmd += ["--features", ",".join(features)]
    cmd += entry.get("args", [])
    return cmd


def run_local_matrix(log_dir: Path, offline: bool = True, toolchain: str = "1.95.0",
                     only_group: str | None = None, only_network: bool = False,
                     timeout: int = 3600) -> dict:
    log_dir.mkdir(parents=True, exist_ok=True)
    results = []
    # Keep the stable FEATURE_MATRIX declaration position on every label so two
    # same-package/default entries (e.g. `host` vs `host_loopback`) cannot
    # collide and overwrite each other's raw log.
    indexed = list(enumerate(FEATURE_MATRIX, start=1))
    entries = [
        (index, e)
        for index, e in indexed
        if (only_group is None or e["group"] == only_group) and (not only_network or e.get("network"))
    ]
    for index, entry in entries:
        cmd = _cargo_cmd(entry, offline=offline, toolchain=toolchain)
        feature_part = "-".join(entry.get("features") or ["default"])
        label = (
            f"{index:03d}-{entry['group']}-{entry['package']}-"
            f"{'nodefault-' if entry.get('no_default_features') else ''}{feature_part}"
        )
        started = time.monotonic()
        try:
            proc = subprocess.run(
                cmd, cwd=str(ROOT), capture_output=True, text=True, timeout=timeout, check=False
            )
            rc, out, err = proc.returncode, proc.stdout, proc.stderr
        except subprocess.TimeoutExpired:
            rc, out, err = 124, "", f"timed out after {timeout}s"
        except FileNotFoundError as exc:
            rc, out, err = 127, "", str(exc)
        elapsed = round(time.monotonic() - started, 2)
        combined = (out or "") + (err or "")
        needs_network = offline and (
            "attempting to make an HTTP request, but --offline was specified" in combined
            or "failed to download" in combined
        )
        if needs_network:
            status = "skip-needs-network"
        elif rc == 0:
            status = "pass"
        else:
            status = "fail"
        log_path = log_dir / f"{label}.log"
        log_path.write_text(
            "\n".join(
                [
                    f"# {datetime.now(timezone.utc).isoformat()}",
                    f"$ {' '.join(cmd)}",
                    f"rc={rc} elapsed={elapsed}s status={status}",
                    "",
                    combined,
                ]
            ),
            encoding="utf-8",
        )
        tail = [line for line in combined.splitlines() if line.startswith("error")][-4:]
        results.append(
            {
                "label": label,
                "group": entry["group"],
                "package": entry["package"],
                "features": entry.get("features", []),
                "no_default_features": bool(entry.get("no_default_features")),
                "declared_network_required": bool(entry.get("network")),
                "command": " ".join(cmd),
                "rc": rc,
                "elapsed_seconds": elapsed,
                "status": status,
                "log": str(log_path),
                "error_tail": tail,
            }
        )
    failed = [r for r in results if r["status"] == "fail"]
    skipped = [r for r in results if r["status"] == "skip-needs-network"]
    if failed:
        status = "fail"
    elif skipped:
        status = "pass-with-skips"
    else:
        status = "pass"
    returned = {
        "status": status,
        "offline": offline,
        "toolchain": toolchain,
        "log_dir": str(log_dir),
        "counts": {"pass": len(results) - len(failed) - len(skipped), "fail": len(failed), "skipped": len(skipped)},
        "runs": results,
    }
    (log_dir / "sdk-local-matrix.json").write_text(json.dumps(returned, indent=2), encoding="utf-8")
    return returned


# ---------------------------------------------------------------------------
# Driver
# ---------------------------------------------------------------------------


def build_report(args) -> dict:
    meta = load_metadata(args.metadata, offline=not args.online)
    direction = check_dependency_direction(meta)
    version = check_version_closure(meta)
    hygiene = check_packaging_hygiene(meta)
    license_report = check_license_closure(meta, allow_unknown=args.allow_unknown_license)
    matrix = validate_feature_matrix(meta)

    failed_gates: list[str] = []
    if direction["status"] == "fail":
        failed_gates.append("dependency_direction")
    if license_report["status"] == "fail":
        failed_gates.append("license_closure")
    if matrix["status"] == "fail":
        failed_gates.append("feature_matrix")
    if args.strict_publish and not version["publish_ready"]:
        failed_gates.append("version_closure")
    if args.strict_publish and hygiene["findings"]:
        failed_gates.append("packaging_hygiene")

    report = {
        "schema": "mio.sdk_contract.v1",
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "workspace_root": str(ROOT),
        "metadata_source": args.metadata or "cargo metadata --locked" + ("" if args.online else " --offline"),
        "metadata_locked": True,
        "strict_publish": bool(args.strict_publish),
        "allow_unknown_license": bool(args.allow_unknown_license),
        "gates": {
            "dependency_direction": direction,
            "version_closure": version,
            "packaging_hygiene": hygiene,
            "license_closure": license_report,
            "feature_matrix": matrix,
        },
        "summary": {
            "status": "fail" if failed_gates else "pass",
            "failed_gates": failed_gates,
            "publish_ready": version["publish_ready"] and not hygiene["findings"],
            "publication_scope": "static version/source checks only; registry resolution, archive verification and publishing are not proven",
            "license_metadata_complete": license_report["license_metadata_complete"],
        },
    }
    return report


def print_human(report: dict) -> None:
    g = report["gates"]
    d = g["dependency_direction"]
    if d["status"] == "pass":
        print(
            f"[PASS] dependency-direction: {d['normal_edge_count']} normal edges over "
            f"{d['workspace_crate_count']} workspace crates, no upward dependency"
        )
    else:
        print(f"[FAIL] dependency-direction: {len(d['violations'])} violation(s)")
        for v in d["violations"]:
            print(f"        - {v['from']} -> {v['to']}: {v['reason']}")

    v = g["version_closure"]
    if v["publish_ready"]:
        print(
            f"[PASS] version-closure: {v['checked_in_workspace_path_deps']} in-workspace path deps "
            "carry a matching version requirement"
        )
    else:
        level = "FAIL" if report["strict_publish"] else "FINDINGS"
        print(
            f"[{level}] version-closure: {len(v['findings'])} of {v['checked_in_workspace_path_deps']} "
            "in-workspace path deps are not publish-ready (publish_ready=false)"
        )
        for f in v["findings"]:
            print(f"        - {f['package']} -> {f['dependency']}: {f['reason']}")

    hy = g["packaging_hygiene"]
    if hy["status"] == "pass":
        print("[PASS] packaging-hygiene: no reserved cargo package artifacts in crate sources")
    else:
        level = "FAIL" if report["strict_publish"] else "FINDINGS"
        print(
            f"[{level}] packaging-hygiene: {len(hy['findings'])} reserved artifact(s) would make "
            "cargo package/publish fail"
        )
        for f in hy["findings"]:
            print(f"        - {f['package']}: {f['file']}")

    lc = g["license_closure"]
    if lc["status"] == "pass":
        print(
            f"[PASS] license-closure: {lc['package_count']} packages, 0 missing license metadata "
            f"({len(lc['review'])} legacy/unrecognized expression(s) for manual review)"
        )
    elif lc["status"] == "unknown":
        print(
            f"[UNKNOWN] license-closure: {lc['missing_metadata_count']} package(s) have no license metadata; "
            "license_metadata_complete=false (--allow-unknown-license)"
        )
    else:
        print(f"[FAIL] license-closure: {lc['missing_metadata_count']} package(s) missing license metadata")
        for m in lc["missing_metadata"]:
            print(f"        - {m['name']} {m['version']} ({m.get('source') or 'workspace'})")

    fm = g["feature_matrix"]
    if fm["status"] == "pass":
        combos = len(fm["matrix"])
        features = sum(len(e["features"]) for e in fm["matrix"])
        print(f"[PASS] feature-matrix: {combos} combos / {features} feature selections all exist in metadata")
    else:
        print(f"[FAIL] feature-matrix: {len(fm['unknown_features'])} unknown feature reference(s)")
        for e in fm["unknown_features"]:
            print(f"        - {e['package']} feature={e['feature']}: {e['reason']}; available={e.get('available')}")

    print(f"[SUMMARY] status={report['summary']['status']} failed_gates={report['summary']['failed_gates'] or 'none'}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Offline SDK contract checks for the Mio workspace")
    parser.add_argument("--metadata", help="use a saved cargo-metadata JSON instead of invoking cargo")
    parser.add_argument("--online", action="store_true", help="allow cargo metadata to use the network (default: --offline)")
    parser.add_argument("--json", nargs="?", const="", default=None, help="write the machine-readable report")
    parser.add_argument("--strict-publish", action="store_true",
                        help="treat version-closure and packaging-hygiene findings as a failure")
    parser.add_argument("--allow-unknown-license", action="store_true",
                        help="report missing license metadata as explicit unknown instead of failing")
    parser.add_argument("--run-local", action="store_true", help="also compile the local example/feature matrix with cargo")
    parser.add_argument("--only-group", choices=["examples", "helpers"], help="restrict --run-local to one group")
    parser.add_argument("--only-network", action="store_true",
                        help="with --run-local, only the combos whose crates may not be cached")
    parser.add_argument("--log-dir", default=str(DEFAULT_LOG_DIR), help="log directory for --run-local")
    parser.add_argument("--toolchain", default="1.95.0", help="rustup toolchain for --run-local (default: 1.95.0)")
    parser.add_argument("--quiet", action="store_true")
    args = parser.parse_args(argv)

    try:
        report = build_report(args)
    except RuntimeError as exc:
        print(f"[ERROR] {exc}", file=sys.stderr)
        return 2

    if args.run_local:
        log_dir = Path(args.log_dir)
        report["local_matrix"] = run_local_matrix(
            log_dir, offline=not args.online, toolchain=args.toolchain,
            only_group=args.only_group, only_network=args.only_network,
        )
        if report["local_matrix"]["status"] == "fail":
            report["summary"]["status"] = "fail"
            report["summary"]["failed_gates"].append("local_matrix")

    if not args.quiet:
        print_human(report)
        if args.run_local:
            lm = report["local_matrix"]
            print(
                f"[LOCAL] matrix status={lm['status']} "
                f"pass={lm['counts']['pass']} fail={lm['counts']['fail']} skipped={lm['counts']['skipped']} "
                f"logs={lm['log_dir']}"
            )
            for run in lm["runs"]:
                note = ""
                if run["status"] == "skip-needs-network":
                    note = " (crate cache miss; run without --offline)"
                print(f"        [{run['status']}] {run['command']}{note}")

    json_path = args.json
    if json_path == "":
        json_path = str(Path(args.log_dir) / "sdk-report.json") if args.run_local else "/tmp/mio-sdk-qa/sdk-report.json"
    if json_path:
        dest = Path(json_path)
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    return 0 if report["summary"]["status"] == "pass" else 1


if __name__ == "__main__":
    sys.exit(main())
