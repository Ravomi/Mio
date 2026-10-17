"""Independent acceptance checks for the resume-cache-build task.

The fixture script is pinned by SHA-256 so it cannot be rewritten to fake a
recovery. Acceptance then (a) re-runs the pinned fixture against the agent's
library in a throwaway sandbox and recomputes the expected artifacts itself, and
(b) reads the journal the pinned fixture wrote into the workspace during the
agent's session, requiring a recorded failure before the successful attempt.
"""
import hashlib
import json
import subprocess
import sys
import tempfile
from pathlib import Path

WORK = Path.cwd()
EXPECTED_FIXTURE_SHA256 = "5714ff08092297fd5720de75cf0ec84233e9f1f08f084ba45165b62e6dedaef8"
EXPECTED_ARTIFACTS = {
    "alpha.out": "alpha beta\n",
    "beta.out": "solo\n",
    "gamma.out": "gamma delta epsilon\n",
}


def sha256_lf(path):
    """Hash the script with newlines normalised so line endings do not matter."""
    return hashlib.sha256(path.read_bytes().replace(b"\r\n", b"\n")).hexdigest()


fixture = WORK / "fixture_build.py"
assert fixture.is_file(), "fixture_build.py is missing"
actual_fixture_hash = sha256_lf(fixture)
assert actual_fixture_hash == EXPECTED_FIXTURE_SHA256, (
    "fixture_build.py was modified: " + actual_fixture_hash
)

# 1. Re-run the pinned fixture against the agent's library from a clean state and
#    recompute the artifacts independently of the agent's own run.
with tempfile.TemporaryDirectory() as temporary:
    sandbox = Path(temporary)
    completed = subprocess.run(
        [
            sys.executable,
            str(fixture),
            "build",
            "--state-dir",
            str(sandbox / "state"),
            "--out-dir",
            str(sandbox / "out"),
        ],
        cwd=WORK,
        capture_output=True,
        text=True,
    )
    assert completed.returncode == 0, "fixture still fails:\n" + completed.stderr
    produced = {path.name: path.read_text() for path in sorted((sandbox / "out").glob("*.out"))}
    assert produced == EXPECTED_ARTIFACTS, produced

# 2. The workspace journal must be written by that same pinned fixture and must
#    show a real failure before the successful attempt.
journal = WORK / "state" / "attempts.jsonl"
assert journal.is_file(), "no fixture journal: the fixture was never run"
entries = [json.loads(line) for line in journal.read_text().splitlines() if line.strip()]
statuses = [entry.get("status") for entry in entries]
assert "start" in statuses, statuses
first_error = next(
    (index for index, entry in enumerate(entries) if entry.get("status") == "error"), None
)
assert first_error is not None, "no failed attempt was recorded: " + repr(statuses)
assert entries[first_error].get("error_type") in ("FileNotFoundError", "OSError"), entries[first_error]
first_ok = next(
    (
        index
        for index, entry in enumerate(entries)
        if entry.get("status") == "ok" and index > first_error
    ),
    None,
)
assert first_ok is not None, "no successful attempt after the failure: " + repr(statuses)

# 3. The recovery also left the expected artifacts in the workspace out/ tree.
workspace_artifacts = {
    path.name: path.read_text() for path in sorted((WORK / "out").glob("*.out"))
}
assert workspace_artifacts == EXPECTED_ARTIFACTS, workspace_artifacts
