#!/usr/bin/env python3
"""DES-18 acceptance: file checkpoints combined with a real process crash.

What this proves
----------------
`--checkpoints` is a plain-CLI feature, so this probe restarts a *real* Mio
process with `--checkpoints`, lets a loopback mock model drive a real `write` /
`edit` workspace call, waits until the side effect **and** the checkpoint index
are durably on disk, and then delivers a real SIGKILL. It then restarts Mio
through the actual offline CLI maintenance commands (`--recovery`,
`--list-checkpoints`, `--rewind`) and checks that:

  * an executed write survives the crash and is rewound back to the original
    bytes only by an explicit `--rewind`;
  * an interrupted `write` / `edit` whose checkpoint was never committed stays
    `unknown`, is never replayed, and yields no rewindable checkpoint;
  * a user edit made after the crash is refused (never overwritten);
  * workspace `.git` metadata (a self-built sentinel tree, no `git` binary and
    no repository commands) is byte-identical before and after every step.

How it reuses the existing crash harness
----------------------------------------
This file is a thin DES-18 layer on top of `tests/crash_recovery.py`, which owns
the compiled `tests/fixtures/crash_probe.c` LD_PRELOAD shim, the loopback mock
model server, the sandboxed process helpers and the four-point SIGKILL
synchronisation. Reusing that harness is deliberate: there is exactly one
injection mechanism and one definition of "real process crash" in the suite.

The pause points used here
--------------------------
  * ``connect`` hit 2 - the shim blocks immediately before the follow-up model
    request, i.e. after the write tool returned and after
    `CheckpointStore::confirm` marked the entry `Active`. This is the
    deterministic barrier for "side effect + checkpoint already on disk".
  * ``fsync`` on a descriptor whose path contains ``.mio/checkpoints/`` - the
    shim blocks inside `record_pre_edit`, right at the snapshot `sync_all`
    *before* the checkpoint index is persisted and *before* the target file is
    replaced. This is the deterministic barrier for "intent recorded, effect
    still unknown, checkpoint not committed".

Both pause points are real libc calls reached by the real product binary; the
test observes an on-disk marker and sends SIGKILL. No product source, no
journal and no checkpoint index is edited by the test itself.

Honest scope and boundaries
---------------------------
* Linux only (LD_PRELOAD + POSIX process groups).
* SIGKILL process death, **not** power loss: page cache is intact, so this
  exercises crash consistency of the write order, not fsync durability under
  hardware failure.
* Loopback mock only: no real provider, no internet, no TLS.
* `.git` is a hand-built sentinel directory; the test never runs `git` and the
  product never touches it.
* If `cc`/`gcc`/`clang` is missing or the shim cannot be compiled, cases are
  reported as `skip` (exit 3 unless `--allow-skip`), never as `pass`.
* A pause point that is never reached, or a post-condition that does not hold at
  the pause, is reported as `fail` with the observed state - never adjusted and
  never downgraded to a skip.

Exit codes: 0 = all selected cases passed; 1 = at least one failed; 3 = no
failure but at least one skip; 2 = the binary/setup itself is missing.
"""

import argparse
import hashlib
import json
import os
import shutil
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import crash_recovery as cr  # noqa: E402
from crash_recovery import (  # noqa: E402
    SESSION_REL,
    CaseFailure,
    Probe,
    Sandbox,
    binary_is_dynamic,
    cli_argv,
    cli_recovery,
    find_cc,
    journal_ops,
    kill_hard,
    parse_json_lines,
    platform_info,
    read_json,
    recovery_executed,
    recovery_of,
    recovery_unknown,
    rpc_argv,
    run_cli,
    seed_session,
    sha256_file,
    wait_for_file,
)

CHECKPOINT_DIR_REL = ".mio/checkpoints"
CHECKPOINT_INDEX_REL = ".mio/checkpoints/index.json"
WRITE_TARGET = "notes.txt"
ORIGINAL = "original content\n"
AGENT_WRITE = "agent write after crash"
AGENT_EDIT = "edited by agent\n"
USER_EDIT = "user later edit\n"

# A self-built `.git` sentinel: no `git` binary, no repository commands, just
# bytes the product must leave alone. Values are arbitrary but stable.
GIT_SENTINEL = {
    "HEAD": "ref: refs/heads/main\n",
    "config": "[core]\n\trepositoryformatversion = 0\n",
    "refs/heads/main": "0000000000000000000000000000000000000000\n",
    "objects/info/packs": "",
    "ORIG_HEAD": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n",
}


# --------------------------------------------------------------------------- #
# Small helpers
# --------------------------------------------------------------------------- #

def fingerprint(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def tree_hashes(root):
    """Deterministic content map of a directory tree (files, dirs, symlinks)."""
    root = Path(root)
    hashes = {}
    if not root.exists():
        return hashes
    for path in sorted(root.rglob("*")):
        rel = path.relative_to(root).as_posix()
        if path.is_symlink():
            hashes[rel] = "symlink:" + os.readlink(path)
        elif path.is_dir():
            hashes[rel + "/"] = "dir"
        else:
            hashes[rel] = sha256_file(path)
    return hashes


def write_git_sentinel(workspace):
    git = Path(workspace) / ".git"
    for rel, content in GIT_SENTINEL.items():
        target = git / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8")
    return tree_hashes(git)


def assert_git_unchanged(git_before, sandbox, where):
    after = tree_hashes(Path(sandbox.ws) / ".git")
    if after != git_before:
        raise CaseFailure(
            f"workspace .git metadata changed {where}: {git_before} -> {after}"
        )


def checkpoint_index_path(sandbox):
    return sandbox.ws / CHECKPOINT_INDEX_REL


def checkpoint_index(sandbox):
    path = checkpoint_index_path(sandbox)
    if not path.is_file():
        return None
    return read_json(path)


def checkpoint_entries(sandbox, target=None):
    document = checkpoint_index(sandbox)
    entries = list((document or {}).get("entries") or [])
    if target is not None:
        entries = [entry for entry in entries if entry.get("target") == target]
    return entries


def target_ops(sandbox, target):
    return [op for op in journal_ops(sandbox) if op.get("target") == target]


def cli_list_checkpoints(sandbox, timeout=60):
    result = run_cli(
        sandbox, "http://127.0.0.1:1/v1", ["--json", "--list-checkpoints"], timeout=timeout
    )
    if result.returncode != 0:
        raise CaseFailure(
            f"--list-checkpoints exited {result.returncode}: {result.stderr.strip()}"
        )
    for event in parse_json_lines(result.stdout):
        if event.get("type") == "checkpoints":
            return event
    raise CaseFailure(f"no checkpoints event in --list-checkpoints output: {result.stdout!r}")


def cli_rewind(sandbox, selector, timeout=60, expect_success=True):
    result = run_cli(
        sandbox,
        "http://127.0.0.1:1/v1",
        ["--json", "--rewind", selector],
        timeout=timeout,
    )
    if not expect_success:
        return result
    if result.returncode != 0:
        raise CaseFailure(
            f"--rewind {selector} exited {result.returncode}: {result.stderr.strip()}"
        )
    for event in parse_json_lines(result.stdout):
        if event.get("type") == "rewound":
            return event
    raise CaseFailure(f"no rewound event in --rewind output: {result.stdout!r}")


def post_crash_text_turn(sandbox, timeout):
    """A fresh successful turn on the crashed workspace; must replay nothing."""
    mock = sandbox.mock(initial="text")
    result = run_cli(
        sandbox,
        mock.base_url,
        ["--json", "--checkpoints", "--session", SESSION_REL, "-p", "post-crash turn"],
        timeout=timeout,
    )
    if result.returncode != 0:
        raise CaseFailure(
            f"post-crash turn failed rc={result.returncode}: {result.stderr.strip()}"
        )
    return result


def cli_checkpoint_crash_argv(sandbox, base_url, token):
    return cli_argv(
        sandbox.binary,
        sandbox.ws,
        base_url,
        ["--json", "--checkpoints", "--session", SESSION_REL, "-p", f"crash {token}"],
    )


def assert_recovery_goal(data, token, point):
    if data.get("interrupted_runs", 0) < 1:
        raise CaseFailure(f"{point}: expected an interrupted run, got {data!r}")
    goals = data.get("unfinished_goals") or []
    if not any(
        goal.get("goal") == f"crash {token}" and not goal.get("truncated") for goal in goals
    ):
        raise CaseFailure(f"{point}: interrupted user request not preserved: {goals!r}")


def assert_recovery_view(event, saved_session, point):
    view = event.get("view") or {}
    if (
        view.get("sessionId") != saved_session["id"]
        or view.get("sessionSaved") is not True
        or view.get("savedMessages") != len(saved_session["history"])
        or view.get("automaticReplay") is not False
        or view.get("evidenceField") != "data"
    ):
        raise CaseFailure(f"{point}: inconsistent persisted recovery view: {view!r}")


# --------------------------------------------------------------------------- #
# Crash builders
# --------------------------------------------------------------------------- #

def crash_after_tool(sandbox, probe, timeout, operation, tool_arguments, expected_after, label):
    """SIGKILL after the write/edit tool returned and the checkpoint is Active."""
    token = f"MIO-CP-{label}"
    target = sandbox.ws / WRITE_TARGET
    target.write_text(ORIGINAL, encoding="utf-8")
    git_before = write_git_sentinel(sandbox.ws)
    seed_session(sandbox, timeout)
    seed_bytes = sandbox.session_path().read_bytes()
    session_hash = sha256_file(sandbox.session_path())

    crash_mock = sandbox.mock(initial="tool_call", tool_arguments=tool_arguments)
    ready = sandbox.logs / f"ready.{label}"
    extra = probe.env(ready, connect_hit=2, connect_port=crash_mock.port)
    proc = sandbox.spawn_cli(
        cli_checkpoint_crash_argv(sandbox, crash_mock.base_url, token),
        sandbox.env(extra),
        f"crash.{label}",
    )
    checkpoint_id = None
    try:
        if not wait_for_file(ready, timeout):
            raise CaseFailure(f"{label}: post-tool pause point was never reached")
        detail = ready.read_text(errors="replace").strip()
        if not detail.startswith("connect"):
            raise CaseFailure(f"{label}: unexpected pause point: {detail!r}")

        # ----- deterministic barrier: side effect + Active checkpoint on disk ----
        current = target.read_text(encoding="utf-8")
        if current != expected_after:
            raise CaseFailure(
                f"{label}: side effect was not on disk at the pause: {current!r}"
            )
        entries = checkpoint_entries(sandbox, WRITE_TARGET)
        if len(entries) != 1 or entries[0].get("state") != "active":
            raise CaseFailure(f"{label}: checkpoint not confirmed Active at pause: {entries!r}")
        entry = entries[0]
        checkpoint_id = entry["id"]
        if entry.get("operation") != operation:
            raise CaseFailure(f"{label}: checkpoint operation is not {operation}: {entry!r}")
        if entry.get("after_sha256") != fingerprint(expected_after):
            raise CaseFailure(f"{label}: checkpoint after-fingerprint mismatch: {entry!r}")
        if entry.get("existed") is not True or entry.get("before_sha256") != fingerprint(ORIGINAL):
            raise CaseFailure(f"{label}: checkpoint before-fingerprint mismatch: {entry!r}")
        ops = target_ops(sandbox, WRITE_TARGET)
        if len(ops) != 1 or ops[0]["status"] != "executed":
            raise CaseFailure(f"{label}: journal did not record one executed op: {ops!r}")
        assert_git_unchanged(git_before, sandbox, "at the pause point")
    finally:
        kill_hard(proc)  # real SIGKILL of the process group, then wait

    # ----- artefacts survived the real SIGKILL ------------------------------
    if target.read_text(encoding="utf-8") != expected_after:
        raise CaseFailure(f"{label}: side effect did not survive the SIGKILL")
    entries = checkpoint_entries(sandbox, WRITE_TARGET)
    if len(entries) != 1 or entries[0]["id"] != checkpoint_id or entries[0]["state"] != "active":
        raise CaseFailure(f"{label}: checkpoint index changed across the SIGKILL: {entries!r}")
    if sandbox.session_path().read_bytes() != seed_bytes:
        raise CaseFailure(f"{label}: crashed turn unexpectedly rewrote the session file")
    assert_git_unchanged(git_before, sandbox, "after the SIGKILL")

    return {
        "token": token,
        "label": label,
        "operation": operation,
        "checkpoint_id": checkpoint_id,
        "target": WRITE_TARGET,
        "expected_after": expected_after,
        "git_before": git_before,
        "session_hash": session_hash,
        "pause": detail,
    }


def crash_at_checkpoint_snapshot(sandbox, probe, timeout, label):
    """SIGKILL inside record_pre_edit, before the index/effect are committed."""
    token = f"MIO-CP-{label}"
    target = sandbox.ws / WRITE_TARGET
    target.write_text(ORIGINAL, encoding="utf-8")
    git_before = write_git_sentinel(sandbox.ws)
    seed_session(sandbox, timeout)
    seed_bytes = sandbox.session_path().read_bytes()

    crash_mock = sandbox.mock(
        initial="tool_call",
        tool_arguments={"operation": "write", "path": WRITE_TARGET, "value": AGENT_WRITE},
    )
    ready = sandbox.logs / f"ready.{label}"
    extra = probe.env(ready, fsync_match=CHECKPOINT_DIR_REL)
    proc = sandbox.spawn_cli(
        cli_checkpoint_crash_argv(sandbox, crash_mock.base_url, token),
        sandbox.env(extra),
        f"crash.{label}",
    )
    try:
        if not wait_for_file(ready, timeout):
            raise CaseFailure(f"{label}: checkpoint-snapshot pause point was never reached")
        detail = ready.read_text(errors="replace").strip()
        if CHECKPOINT_DIR_REL not in detail:
            raise CaseFailure(f"{label}: unexpected pause point: {detail!r}")

        # ----- deterministic barrier: intent recorded, nothing committed -------
        if target.read_text(encoding="utf-8") != ORIGINAL:
            raise CaseFailure(f"{label}: target was modified before the pause point")
        committed = checkpoint_entries(sandbox, WRITE_TARGET)
        if committed:
            raise CaseFailure(
                f"{label}: a checkpoint was already committed at the snapshot pause: {committed!r}"
            )
        ops = target_ops(sandbox, WRITE_TARGET)
        if len(ops) != 1 or ops[0]["status"] not in ("pending", "dispatched"):
            raise CaseFailure(
                f"{label}: expected one write intent before the effect: {ops!r}"
            )
        assert_git_unchanged(git_before, sandbox, "at the pause point")
    finally:
        kill_hard(proc)  # real SIGKILL of the process group, then wait

    if target.read_text(encoding="utf-8") != ORIGINAL:
        raise CaseFailure(f"{label}: uncommitted write leaked into the target")
    if sandbox.session_path().read_bytes() != seed_bytes:
        raise CaseFailure(f"{label}: crashed turn unexpectedly rewrote the session file")
    assert_git_unchanged(git_before, sandbox, "after the SIGKILL")

    return {
        "token": token,
        "label": label,
        "target": WRITE_TARGET,
        "git_before": git_before,
        "pause": detail,
    }


def crash_after_tool_rpc(sandbox, probe, timeout, label):
    """RPC variant of `crash_after_tool`; the restart is still real CLI."""
    token = f"MIO-CP-{label}"
    target = sandbox.ws / WRITE_TARGET
    target.write_text(ORIGINAL, encoding="utf-8")
    git_before = write_git_sentinel(sandbox.ws)
    seed_session(sandbox, timeout)
    session_hash = sha256_file(sandbox.session_path())

    crash_mock = sandbox.mock(
        initial="tool_call",
        tool_arguments={"operation": "write", "path": WRITE_TARGET, "value": AGENT_WRITE},
    )
    ready = sandbox.logs / f"ready.{label}"
    extra = probe.env(ready, connect_hit=2, connect_port=crash_mock.port)
    session = sandbox.rpc(
        rpc_argv(sandbox.binary, sandbox.ws, crash_mock.base_url) + ["--checkpoints"],
        sandbox.env(extra),
        f"crash.{label}",
    )
    checkpoint_id = None
    try:
        session.send({"id": "1", "type": "prompt", "message": f"crash {token}"})
        if not wait_for_file(ready, timeout):
            raise CaseFailure(f"{label}: post-tool pause point was never reached")
        detail = ready.read_text(errors="replace").strip()
        if not detail.startswith("connect"):
            raise CaseFailure(f"{label}: unexpected pause point: {detail!r}")
        if target.read_text(encoding="utf-8") != AGENT_WRITE:
            raise CaseFailure(f"{label}: side effect was not on disk at the pause")
        entries = checkpoint_entries(sandbox, WRITE_TARGET)
        if len(entries) != 1 or entries[0].get("state") != "active":
            raise CaseFailure(f"{label}: RPC checkpoint not confirmed Active: {entries!r}")
        checkpoint_id = entries[0]["id"]
        assert_git_unchanged(git_before, sandbox, "at the pause point")
    finally:
        session.kill_hard()

    entries = checkpoint_entries(sandbox, WRITE_TARGET)
    if len(entries) != 1 or entries[0]["id"] != checkpoint_id:
        raise CaseFailure(f"{label}: RPC checkpoint index changed across the SIGKILL")
    assert_git_unchanged(git_before, sandbox, "after the SIGKILL")
    return {
        "token": token,
        "label": label,
        "checkpoint_id": checkpoint_id,
        "target": WRITE_TARGET,
        "git_before": git_before,
        "session_hash": session_hash,
        "pause": detail,
    }


# --------------------------------------------------------------------------- #
# Cases
# --------------------------------------------------------------------------- #

def case_cli_write_rewind_after_crash(sandbox, probe, timeout):
    state = crash_after_tool(
        sandbox,
        probe,
        timeout,
        operation="write",
        tool_arguments={"operation": "write", "path": WRITE_TARGET, "value": AGENT_WRITE},
        expected_after=AGENT_WRITE,
        label="write-active",
    )
    token = state["token"]
    checkpoint_id = state["checkpoint_id"]
    git_before = state["git_before"]
    target = sandbox.ws / WRITE_TARGET

    # Real CLI restart: read-only recovery must not replay or roll back anything.
    event = cli_recovery(sandbox, timeout=timeout)
    data = recovery_of(event)
    assert_recovery_view(event, read_json(sandbox.session_path()), "write-active")
    assert_recovery_goal(data, token, "write-active")
    executed = [op for op in recovery_executed(data) if op.get("target") == WRITE_TARGET]
    if len(executed) != 1:
        raise CaseFailure(f"write-active: recovery did not report one executed op: {data!r}")
    if recovery_unknown(data):
        raise CaseFailure(f"write-active: recovery reported unknown ops: {data!r}")
    if target.read_text(encoding="utf-8") != AGENT_WRITE:
        raise CaseFailure("write-active: --recovery replayed or reverted the write")
    if sha256_file(sandbox.session_path()) != state["session_hash"]:
        raise CaseFailure("write-active: --recovery rewrote the session file")
    assert_git_unchanged(git_before, sandbox, "after --recovery")

    # Real CLI checkpoint listing sees the crashed Active entry.
    listing = cli_list_checkpoints(sandbox, timeout=timeout)
    listed = [e for e in listing.get("data") or [] if e.get("target") == WRITE_TARGET]
    if len(listed) != 1 or listed[0].get("id") != checkpoint_id:
        raise CaseFailure(f"write-active: --list-checkpoints lost the entry: {listing!r}")
    if listed[0].get("state") != "active":
        raise CaseFailure(f"write-active: listed checkpoint is not Active: {listed!r}")

    # No automatic replay: a fresh successful turn adds no op and no checkpoint.
    post_crash_text_turn(sandbox, timeout)
    if target.read_text(encoding="utf-8") != AGENT_WRITE:
        raise CaseFailure("write-active: post-crash turn replayed or reverted the write")
    if len(target_ops(sandbox, WRITE_TARGET)) != 1:
        raise CaseFailure("write-active: post-crash turn re-recorded the write")
    if len(checkpoint_entries(sandbox, WRITE_TARGET)) != 1:
        raise CaseFailure("write-active: post-crash turn created another checkpoint")

    # Real CLI rewind restores the original bytes, exactly once.
    # Capture the session hash after the (legitimately saving) post-crash turn,
    # so the maintenance commands themselves are what gets checked for writes.
    session_hash_before_rewind = sha256_file(sandbox.session_path())
    rewound = cli_rewind(sandbox, checkpoint_id, timeout=timeout)
    outcome = rewound.get("data") or {}
    if outcome.get("action") != "restored" or outcome.get("target") != WRITE_TARGET:
        raise CaseFailure(f"write-active: unexpected rewind outcome: {rewound!r}")
    if target.read_text(encoding="utf-8") != ORIGINAL:
        raise CaseFailure("write-active: rewind did not restore the original bytes")
    again = cli_rewind(sandbox, checkpoint_id, timeout=timeout, expect_success=False)
    if again.returncode == 0:
        raise CaseFailure("write-active: a rewound checkpoint was accepted a second time")
    if target.read_text(encoding="utf-8") != ORIGINAL:
        raise CaseFailure("write-active: refused second rewind changed the file")
    if sha256_file(sandbox.session_path()) != session_hash_before_rewind:
        raise CaseFailure("write-active: checkpoint maintenance rewrote the session file")
    assert_git_unchanged(git_before, sandbox, "after --rewind")

    return {
        "checkpoint_id": checkpoint_id,
        "operation": "write",
        "pause": state["pause"],
        "recovery_executed_ops": 1,
        "recovery_unknown_ops": 0,
        "replayed": False,
        "rewind": "restored",
        "second_rewind_refused": True,
        "git_metadata_unchanged": True,
    }


def case_cli_edit_rewind_after_crash(sandbox, probe, timeout):
    state = crash_after_tool(
        sandbox,
        probe,
        timeout,
        operation="edit",
        tool_arguments={
            "operation": "edit",
            "path": WRITE_TARGET,
            "old_text": "original content",
            "new_text": "edited by agent",
        },
        expected_after=AGENT_EDIT,
        label="edit-active",
    )
    token = state["token"]
    checkpoint_id = state["checkpoint_id"]
    git_before = state["git_before"]
    target = sandbox.ws / WRITE_TARGET

    event = cli_recovery(sandbox, timeout=timeout)
    data = recovery_of(event)
    assert_recovery_view(event, read_json(sandbox.session_path()), "edit-active")
    assert_recovery_goal(data, token, "edit-active")
    executed = [op for op in recovery_executed(data) if op.get("target") == WRITE_TARGET]
    if len(executed) != 1 or recovery_unknown(data):
        raise CaseFailure(f"edit-active: unexpected recovery evidence: {data!r}")

    post_crash_text_turn(sandbox, timeout)
    if target.read_text(encoding="utf-8") != AGENT_EDIT:
        raise CaseFailure("edit-active: post-crash turn replayed or reverted the edit")
    if len(checkpoint_entries(sandbox, WRITE_TARGET)) != 1:
        raise CaseFailure("edit-active: post-crash turn created another checkpoint")

    rewound = cli_rewind(sandbox, checkpoint_id, timeout=timeout)
    if (rewound.get("data") or {}).get("action") != "restored":
        raise CaseFailure(f"edit-active: unexpected rewind outcome: {rewound!r}")
    if target.read_text(encoding="utf-8") != ORIGINAL:
        raise CaseFailure("edit-active: rewind did not restore the pre-edit bytes")
    assert_git_unchanged(git_before, sandbox, "after --rewind")

    return {
        "checkpoint_id": checkpoint_id,
        "operation": "edit",
        "pause": state["pause"],
        "replayed": False,
        "rewind": "restored",
        "git_metadata_unchanged": True,
    }


def case_cli_user_edit_refused_after_crash(sandbox, probe, timeout):
    state = crash_after_tool(
        sandbox,
        probe,
        timeout,
        operation="write",
        tool_arguments={"operation": "write", "path": WRITE_TARGET, "value": AGENT_WRITE},
        expected_after=AGENT_WRITE,
        label="write-user-edit",
    )
    checkpoint_id = state["checkpoint_id"]
    git_before = state["git_before"]
    target = sandbox.ws / WRITE_TARGET

    # The user changes the file after the crash, before rewinding.
    target.write_text(USER_EDIT, encoding="utf-8")
    edited_hash = sha256_file(target)

    result = cli_rewind(sandbox, checkpoint_id, timeout=timeout, expect_success=False)
    if result.returncode == 0:
        raise CaseFailure("write-user-edit: rewind overwrote a later user edit")
    if target.read_text(encoding="utf-8") != USER_EDIT or sha256_file(target) != edited_hash:
        raise CaseFailure("write-user-edit: refused rewind still changed the user file")

    # The entry stays Active; only an explicit matching rewind may consume it.
    entries = checkpoint_entries(sandbox, WRITE_TARGET)
    if len(entries) != 1 or entries[0]["state"] != "active":
        raise CaseFailure(f"write-user-edit: checkpoint state was mutated by refusal: {entries!r}")
    assert_git_unchanged(git_before, sandbox, "after refused --rewind")

    # Restoring the agent bytes lets the same checkpoint rewind cleanly.
    target.write_text(AGENT_WRITE, encoding="utf-8")
    rewound = cli_rewind(sandbox, checkpoint_id, timeout=timeout)
    if (rewound.get("data") or {}).get("action") != "restored":
        raise CaseFailure(f"write-user-edit: recovery rewind failed: {rewound!r}")
    if target.read_text(encoding="utf-8") != ORIGINAL:
        raise CaseFailure("write-user-edit: rewind did not restore the original bytes")
    assert_git_unchanged(git_before, sandbox, "after successful --rewind")

    return {
        "checkpoint_id": checkpoint_id,
        "pause": state["pause"],
        "user_edit_refused": True,
        "user_edit_preserved": True,
        "later_rewind_restored": True,
        "git_metadata_unchanged": True,
    }


def case_cli_unknown_not_replayed_after_crash(sandbox, probe, timeout):
    state = crash_at_checkpoint_snapshot(sandbox, probe, timeout, label="write-unknown")
    token = state["token"]
    git_before = state["git_before"]
    target = sandbox.ws / WRITE_TARGET

    # The interrupted write is reported as unknown, never as executed.
    event = cli_recovery(sandbox, timeout=timeout)
    data = recovery_of(event)
    assert_recovery_view(event, read_json(sandbox.session_path()), "write-unknown")
    assert_recovery_goal(data, token, "write-unknown")
    executed = [op for op in recovery_executed(data) if op.get("target") == WRITE_TARGET]
    if executed:
        raise CaseFailure(f"write-unknown: an uncommitted write reported as executed: {data!r}")
    unknown = [
        op for op in recovery_unknown(data) if op.get("target") == WRITE_TARGET
    ]
    if len(unknown) != 1:
        raise CaseFailure(f"write-unknown: expected one unknown op, got {data!r}")
    if target.read_text(encoding="utf-8") != ORIGINAL:
        raise CaseFailure("write-unknown: --recovery touched the file")
    assert_git_unchanged(git_before, sandbox, "after --recovery")

    # Nothing committed means nothing is rewindable.
    listing = cli_list_checkpoints(sandbox, timeout=timeout)
    rewindable = [
        e
        for e in listing.get("data") or []
        if e.get("target") == WRITE_TARGET and e.get("state") == "active"
    ]
    if rewindable:
        raise CaseFailure(f"write-unknown: found a rewindable checkpoint: {rewindable!r}")

    # A later successful turn must not replay the unknown write.
    post_crash_text_turn(sandbox, timeout)
    if target.read_text(encoding="utf-8") != ORIGINAL:
        raise CaseFailure("write-unknown: the unknown write was replayed")
    ops = target_ops(sandbox, WRITE_TARGET)
    if len(ops) != 1 or ops[0]["status"] not in ("unknown", "dispatched"):
        raise CaseFailure(f"write-unknown: unknown op was realized or duplicated: {ops!r}")
    assert_git_unchanged(git_before, sandbox, "after the post-crash turn")

    return {
        "pause": state["pause"],
        "unknown_op": WRITE_TARGET,
        "executed_ops": 0,
        "replayed": False,
        "rewindable_checkpoints": 0,
        "git_metadata_unchanged": True,
    }


def case_rpc_write_cli_rewind_after_crash(sandbox, probe, timeout):
    state = crash_after_tool_rpc(sandbox, probe, timeout, label="rpc-write")
    checkpoint_id = state["checkpoint_id"]
    git_before = state["git_before"]
    target = sandbox.ws / WRITE_TARGET

    listing = cli_list_checkpoints(sandbox, timeout=timeout)
    listed = [e for e in listing.get("data") or [] if e.get("target") == WRITE_TARGET]
    if len(listed) != 1 or listed[0].get("id") != checkpoint_id or listed[0].get("state") != "active":
        raise CaseFailure(f"rpc-write: CLI did not see the RPC checkpoint: {listing!r}")

    rewound = cli_rewind(sandbox, checkpoint_id, timeout=timeout)
    if (rewound.get("data") or {}).get("action") != "restored":
        raise CaseFailure(f"rpc-write: CLI rewind of an RPC checkpoint failed: {rewound!r}")
    if target.read_text(encoding="utf-8") != ORIGINAL:
        raise CaseFailure("rpc-write: rewind did not restore the original bytes")
    if sha256_file(sandbox.session_path()) != state["session_hash"]:
        raise CaseFailure("rpc-write: CLI maintenance rewrote the session file")
    assert_git_unchanged(git_before, sandbox, "after CLI --rewind")

    return {
        "checkpoint_id": checkpoint_id,
        "pause": state["pause"],
        "entry_point": "rpc",
        "restart_entry_point": "cli",
        "rewind": "restored",
        "git_metadata_unchanged": True,
    }


HANDLERS = {
    "cli_checkpoint_write_rewind_after_crash": case_cli_write_rewind_after_crash,
    "cli_checkpoint_edit_rewind_after_crash": case_cli_edit_rewind_after_crash,
    "cli_checkpoint_user_edit_refused_after_crash": case_cli_user_edit_refused_after_crash,
    "cli_checkpoint_unknown_not_replayed_after_crash": case_cli_unknown_not_replayed_after_crash,
    "rpc_checkpoint_write_cli_rewind_after_crash": case_rpc_write_cli_rewind_after_crash,
}

ALL_CASES = list(HANDLERS)


# --------------------------------------------------------------------------- #
# Runner
# --------------------------------------------------------------------------- #

def run_selected(args):
    binary = Path(args.binary).resolve()
    if not binary.is_file():
        print(
            json.dumps(
                {
                    "error": f"mio binary not found: {binary}",
                    "hint": "build it first or pass --binary PATH",
                }
            )
        )
        return 2

    info = platform_info()
    linux = sys.platform.startswith("linux")
    cc = find_cc()
    dynamic = binary_is_dynamic(binary)

    probe = None
    probe_error = None
    probe_dir = None
    if linux and cc:
        probe_dir = tempfile.mkdtemp(prefix="mio-checkpoint-probe-")
        probe = Probe(cc, probe_dir)
        if not probe.compile():
            probe_error = probe.error
            probe = None
    elif not linux:
        probe_error = "LD_PRELOAD crash injection is Linux-only"
    else:
        probe_error = "no C compiler (cc/gcc/clang) available to build the shim"

    if dynamic is False:
        probe_error = probe_error or (
            "binary does not dynamically import connect/rename/fsync; "
            "LD_PRELOAD cannot interpose"
        )
        probe = None

    results = []
    for name in args.cases:
        started = time.monotonic()
        sandbox = Sandbox(binary, keep_temp=args.keep_temp)
        status = "pass"
        reason = None
        evidence = None
        try:
            if not linux:
                raise CaseFailure("skip:platform")
            if probe is None:
                raise CaseFailure(f"skip:probe-unavailable ({probe_error})")
            evidence = HANDLERS[name](sandbox, probe, args.timeout)
        except CaseFailure as error:
            text = str(error)
            if text.startswith("skip:"):
                status = "skip"
                reason = text[len("skip:"):]
            else:
                status = "fail"
                reason = text
        except Exception as error:  # noqa: BLE001
            status = "fail"
            reason = f"{type(error).__name__}: {error}"
        finally:
            sandbox.cleanup()
        duration = round(time.monotonic() - started, 3)
        record = {"case": name, "status": status, "duration_s": duration}
        if reason:
            record["reason"] = reason
        if evidence:
            record["evidence"] = evidence
        results.append(record)
        if not args.json:
            suffix = f" ({reason})" if reason else ""
            print(f"[{status.upper():4}] {name} {duration:.2f}s{suffix}", flush=True)

    summary = {
        "passed": sum(1 for r in results if r["status"] == "pass"),
        "failed": sum(1 for r in results if r["status"] == "fail"),
        "skipped": sum(1 for r in results if r["status"] == "skip"),
        "total": len(results),
    }
    output = {
        "scope": {
            "decision": "DES-18 checkpoint + real SIGKILL",
            "platform": info,
            "injection": "real SIGKILL at externally synchronised libc pause points",
            "synchronisation": "reused LD_PRELOAD connect/fsync shim + on-disk pause marker",
            "restart": "real CLI --recovery / --list-checkpoints / --rewind",
            "power_loss": False,
            "cross_platform": False,
            "network": "loopback mock only; no real provider",
            "git": "self-built .git sentinel only; git never invoked",
        },
        "binary": str(binary),
        "probe": {
            "compiler": cc,
            "compiled": probe is not None,
            "error": probe_error,
            "source": str(cr.FIXTURE_C),
        },
        "cases": results,
        "summary": summary,
    }
    if args.json:
        print(json.dumps(output, indent=2))
    else:
        print(
            f"\nsummary: {summary['passed']} passed, {summary['failed']} failed, "
            f"{summary['skipped']} skipped (of {summary['total']})"
        )
        if probe_error:
            print(f"probe: unavailable ({probe_error})")
        print("scope: SIGKILL only (no power loss), Linux/loopback, .git sentinel untouched")

    if probe_dir:
        shutil.rmtree(probe_dir, ignore_errors=True)

    if summary["failed"]:
        return 1
    if summary["skipped"]:
        return 0 if args.allow_skip else 3
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--binary",
        default=os.environ.get("MIO_BIN", str(cr.DEFAULT_BIN)),
        help="path to the mio binary (default: target/debug/mio or MIO_BIN)",
    )
    parser.add_argument(
        "--case",
        action="append",
        dest="case_list",
        help="run only the named case(s); repeatable",
    )
    parser.add_argument("--list", action="store_true", help="list case names and exit")
    parser.add_argument("--json", action="store_true", help="emit a JSON report")
    parser.add_argument("--timeout", type=float, default=90.0, help="per-case timeout in seconds")
    parser.add_argument(
        "--allow-skip", action="store_true", help="return 0 when cases are skipped"
    )
    parser.add_argument("--keep-temp", action="store_true", help="do not delete per-case temp dirs")
    args = parser.parse_args()

    if args.list:
        print("\n".join(ALL_CASES))
        return 0

    if args.case_list:
        unknown = [c for c in args.case_list if c not in ALL_CASES]
        if unknown:
            print(f"unknown case(s): {', '.join(unknown)}", file=sys.stderr)
            return 2
        args.cases = args.case_list
    else:
        args.cases = list(ALL_CASES)

    return run_selected(args)


if __name__ == "__main__":
    sys.exit(main())
