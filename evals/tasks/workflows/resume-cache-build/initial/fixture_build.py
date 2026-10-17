"""Deterministic build fixture: python3 fixture_build.py build

This is the task's fixture harness, not part of the build library. It appends
one JSON line per attempt to state/attempts.jsonl. Against a fresh state
directory the first attempt is expected to fail with a real traceback; fix the
pipeline so a later attempt completes and writes the artifacts. Do not edit this
file: acceptance pins its exact contents.
"""
import argparse
import json
import os
import sys

from buildlib import pipeline


def _attempt_count(journal):
    """Count finished start records so attempts are numbered monotonically."""
    if not os.path.exists(journal):
        return 0
    count = 0
    with open(journal, encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                entry = json.loads(line)
            except ValueError:
                continue
            if entry.get("status") == "start":
                count += 1
    return count


def _record(journal, attempt, entry):
    directory = os.path.dirname(journal)
    if directory:
        os.makedirs(directory, exist_ok=True)
    payload = json.dumps({"attempt": attempt, **entry}, sort_keys=True)
    with open(journal, "a", encoding="utf-8") as handle:
        handle.write(payload + "\n")


def main(argv):
    parser = argparse.ArgumentParser(description="run the deterministic build fixture")
    parser.add_argument("command", choices=["build"])
    parser.add_argument("--state-dir", default="state")
    parser.add_argument("--out-dir", default="out")
    parser.add_argument("--inputs-dir", default="data")
    args = parser.parse_args(argv[1:])

    journal = os.path.join(args.state_dir, "attempts.jsonl")
    attempt = _attempt_count(journal) + 1
    _record(journal, attempt, {"status": "start"})
    try:
        artifacts = pipeline.build_all(
            args.inputs_dir, os.path.join(args.state_dir, "state.json"), args.out_dir
        )
    except Exception as error:  # noqa: BLE001 - the fixture records any failure
        _record(
            journal,
            attempt,
            {"status": "error", "error_type": type(error).__name__, "error": str(error)},
        )
        print("fixture failed: {}: {}".format(type(error).__name__, error), file=sys.stderr)
        return 1
    _record(journal, attempt, {"status": "ok", "artifacts": sorted(artifacts)})
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
