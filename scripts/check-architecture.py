#!/usr/bin/env python3
"""Reject upward production dependencies between Mio's three engine layers."""
from pathlib import Path
import sys
import tomllib

ROOT = Path(__file__).resolve().parent.parent
LAYERS = {"mio-core": 0, "mio-agent": 1, "mio-coding-agent": 2}


def dependency_tables(manifest):
    # Dev dependencies do not enter the shipped library's dependency graph.
    for key in ("dependencies", "build-dependencies"):
        yield manifest.get(key, {})
    for target in manifest.get("target", {}).values():
        for key in ("dependencies", "build-dependencies"):
            yield target.get(key, {})


def violations(graph):
    errors = []
    for origin, level in LAYERS.items():
        pending = [(origin, [origin])]
        visited = set()
        while pending:
            name, route = pending.pop()
            if name in visited:
                continue
            visited.add(name)
            for dependency in sorted(graph.get(name, ())):
                chain = route + [dependency]
                if dependency in LAYERS and LAYERS[dependency] > level:
                    errors.append(" -> ".join(chain))
                elif dependency not in visited:
                    pending.append((dependency, chain))
    return sorted(set(errors))


def load_graph(root):
    workspace = tomllib.loads((root / "Cargo.toml").read_text())
    inherited = workspace.get("workspace", {}).get("dependencies", {})
    manifests = {root / "Cargo.toml"}
    for pattern in workspace["workspace"]["members"]:
        if pattern == ".":
            continue
        for member in root.glob(pattern):
            manifests.add(member / "Cargo.toml")
    graph = {}
    for path in sorted(manifests):
        manifest = tomllib.loads(path.read_text())
        package = manifest.get("package", {}).get("name")
        if package is None:
            continue
        edges = set()
        for table in dependency_tables(manifest):
            for alias, spec in table.items():
                if isinstance(spec, dict) and spec.get("workspace"):
                    spec = inherited[alias]
                name = spec.get("package", alias) if isinstance(spec, dict) else alias
                edges.add(name)
        graph[package] = edges
    return graph


def main():
    errors = violations(load_graph(ROOT))
    if errors:
        print("Upward Mio production dependencies:", file=sys.stderr)
        for error in errors:
            print(error, file=sys.stderr)
        return 1
    print("Mio architecture: core <- agent <- coding-agent; no upward production paths")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
