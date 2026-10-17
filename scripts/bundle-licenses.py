#!/usr/bin/env python3
"""Bundle original dependency notices with pinned provenance; no legal verdict.

The input inventory comes from collect-licenses.py. Missing archive texts may
use reviewed, SHA-256 checked supplements from the package's recorded upstream
commit. This command never downloads or guesses a license. Existing bundles
are not overwritten. --verify checks every bundled file against its index.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import tomllib
from urllib.parse import urlsplit

MAX_FILE = 1024 * 1024
MAX_TOTAL = 64 * MAX_FILE
MAX_PACKAGES = 2000
DISCLAIMER = "Original notice texts and provenance only; no legal review, SPDX normalization or assertion of fulfilled distribution obligations."


def document(path):
    raw = Path(path).read_bytes()
    if len(raw) > 8 * MAX_FILE:
        raise ValueError("index exceeds 8 MiB")
    return json.loads(raw)


def checked_file(root, relative, expected):
    path = Path(relative)
    if path.is_absolute() or ".." in path.parts or not path.parts:
        raise ValueError("notice path must stay inside its evidence directory")
    target = Path(root)
    for part in path.parts:
        target = target / part
        if target.is_symlink():
            raise ValueError("notice paths must not contain symlinks")
    if not target.is_file():
        raise ValueError("notice must be a regular file")
    with target.open("rb") as handle:
        data = handle.read(MAX_FILE + 1)
    if len(data) > MAX_FILE or not data.strip():
        raise ValueError("notice is empty or exceeds 1 MiB")
    if hashlib.sha256(data).hexdigest() != expected:
        raise ValueError("notice SHA-256 does not match its evidence")
    return data


def supplement_origin(record, file):
    commit = record.get("commit", "")
    if not re.fullmatch("[0-9a-f]{40}", commit):
        raise ValueError("upstream supplement needs a full source commit")
    repository = urlsplit(record["repository"])
    source = urlsplit(file["url"])
    prefix = repository.path.strip("/") + "/" + commit + "/"
    if (repository.scheme != "https" or repository.netloc != "github.com"
            or len(repository.path.strip("/").split("/")) != 2
            or source.scheme != "https" or source.netloc != "raw.githubusercontent.com"
            or not source.path.lstrip("/").startswith(prefix)
            or source.query or source.fragment):
        raise ValueError("supplement URL is not pinned to the recorded repository and commit")
    return {"kind": "pinned_upstream", "url": file["url"], "commit": commit}


def plan(inventory_path, supplements_path, lockfile):
    inventory = document(inventory_path)
    supplements = document(supplements_path) if supplements_path else {"packages": []}
    if inventory.get("schema") != "mio-license-inventory/1":
        raise ValueError("unsupported license inventory schema")
    packages = inventory["packages"]
    if not packages or len(packages) > MAX_PACKAGES:
        raise ValueError("invalid inventory package count")
    with Path(lockfile).open("rb") as handle:
        locked = tomllib.load(handle)
    checksums = {(p["name"], p["version"], p.get("source")): p.get("checksum")
                 for p in locked["package"]}
    output = []
    blobs = {}
    total = 0
    seen = set()
    for package in packages:
        if package["ship_scope"] != "shipping":
            continue
        name, version, source_id = (package[k] for k in ("name", "version", "source_id"))
        key = (name, version, source_id)
        if key in seen or not all(re.fullmatch(r"[A-Za-z0-9_.+-]+", value) for value in (name, version)):
            raise ValueError("duplicate or unsafe package identity")
        seen.add(key)
        checksum = checksums.get(key)
        if package["source_kind"] != "registry" or not re.fullmatch("[0-9a-f]{64}", checksum or ""):
            raise ValueError(f"{name}@{version}: no locked registry archive checksum")
        candidates = []
        if package["text_status"] == "collected":
            for file in package["files"]:
                if file["status"] != "collected":
                    raise ValueError("incomplete package text inventory")
                data = checked_file(Path(inventory_path).parent, file["copied_as"], file["sha256"])
                candidates.append((data, file["name"], {"kind": "published_crate", "archiveSha256": checksum}))
        else:
            matches = [p for p in supplements["packages"]
                       if p["name"] == name and p["version"] == version
                       and p.get("status") == "upstream_text_found"]
            if len(matches) != 1:
                raise ValueError(f"{name}@{version}: no unambiguous notice evidence")
            for file in matches[0]["files"]:
                origin = supplement_origin(matches[0], file)
                data = checked_file(Path(supplements_path).parent, file["path"], file["sha256"])
                candidates.append((data, Path(file["path"]).name, origin))
        if not candidates:
            raise ValueError(f"{name}@{version}: empty notice evidence")
        files = []
        for data, original_name, origin in candidates:
            digest = hashlib.sha256(data).hexdigest()
            relative = f"text/{digest}.txt"
            if relative not in blobs:
                total += len(data)
                if total > MAX_TOTAL:
                    raise ValueError("bundle exceeds 64 MiB")
                blobs[relative] = data
            files.append({"name": original_name, "path": relative,
                          "sha256": digest, "bytes": len(data), "origin": origin})
        output.append({"name": name, "version": version, "source": source_id,
                       "archiveSha256": checksum, "licenseDeclared": package["license_declared"],
                       "spdxDeclarationStatus": package["spdx"]["status"], "files": files})
    if not output:
        raise ValueError("inventory has no shipping dependency notices")
    report = {"schema": "mio-dependency-notices/1", "disclaimer": DISCLAIMER,
              "scope": "non-dev dependencies reachable from workspace members in the resolved inventory; all target conditions retained; optional features outside that inventory are not covered",
              "inventoryFeatures": inventory.get("scope"),
              "lockfileSha256": hashlib.sha256(Path(lockfile).read_bytes()).hexdigest(),
              "packages": output, "textFiles": len(blobs), "textBytes": total}
    return report, blobs


def write_bundle(output, report, blobs):
    output = Path(output)
    # Validate every source before creating any deliverable. The index is the
    # commit marker: an interrupted write is never a verifiable bundle.
    output.mkdir(parents=True, exist_ok=False)
    (output / "text").mkdir()
    for relative, data in blobs.items():
        with (output / relative).open("xb") as handle:
            handle.write(data)
    with (output / "index.json").open("x") as handle:
        json.dump(report, handle, indent=2)
        handle.write("\n")


def verify(output, lockfile=None):
    output = Path(output)
    if output.is_symlink():
        raise ValueError("bundle root must not be a symlink")
    report = document(output / "index.json")
    if report.get("schema") != "mio-dependency-notices/1" or not report.get("packages"):
        raise ValueError("unsupported or empty notice bundle")
    if len(report["packages"]) > MAX_PACKAGES:
        raise ValueError("bundle exceeds package limit")
    if lockfile is not None and hashlib.sha256(Path(lockfile).read_bytes()).hexdigest() != report["lockfileSha256"]:
        raise ValueError("notice bundle is stale for the current Cargo.lock")
    seen = set()
    total = 0
    for package in report["packages"]:
        if not package["files"]:
            raise ValueError("package has no notice")
        for file in package["files"]:
            data = checked_file(output, file["path"], file["sha256"])
            if len(data) != file["bytes"]:
                raise ValueError("notice size mismatch")
            if file["path"] not in seen:
                seen.add(file["path"])
                total += len(data)
    if len(seen) != report["textFiles"] or total != report["textBytes"] or total > MAX_TOTAL:
        raise ValueError("bundle totals mismatch")
    actual = {path.relative_to(output).as_posix() for path in output.rglob("*") if path.is_file() or path.is_symlink()}
    if actual != seen | {"index.json"}:
        raise ValueError("bundle contains unindexed files or symlinks")
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inventory", type=Path)
    parser.add_argument("--supplements", type=Path)
    parser.add_argument("--lockfile", type=Path, default=Path("Cargo.lock"))
    parser.add_argument("--output", type=Path, default=Path("licenses/dependencies"))
    parser.add_argument("--verify", action="store_true")
    args = parser.parse_args()
    try:
        if args.verify:
            report = verify(args.output, args.lockfile)
        else:
            if args.inventory is None:
                parser.error("--inventory is required when building")
            report, blobs = plan(args.inventory, args.supplements, args.lockfile)
            write_bundle(args.output, report, blobs)
            verify(args.output, args.lockfile)
    except (OSError, ValueError, KeyError) as error:
        parser.exit(1, f"license bundle: {error}\n")
    print(f"PASS: {len(report['packages'])} dependency notices, {report['textFiles']} unique original texts ({report['textBytes']} bytes); {DISCLAIMER}")


if __name__ == "__main__":
    main()
