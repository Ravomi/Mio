#!/usr/bin/env python3
"""Real SIGKILL crash-recovery acceptance probe for Mio (REQ-RECOVERY).

What this proves
----------------
It starts a real Mio process (CLI one-shot or `--mode rpc`) against a loopback
mock model server and delivers a real SIGKILL at four externally-synchronised
points of one agent turn:

  1. pre_request          - before the first model request is sent
  2. during_tool          - while a real `sh -c '<marker>; sleep 60'` tool runs
  3. after_tool           - after the tool result was recorded, before the
                            follow-up model request
  4. before_session_save  - inside the atomic session-file replace window

After each kill it restarts Mio read-only (`--recovery` or RPC `get_state`)
and checks that execution evidence is complete, that the crashed side effect
is never replayed, that an unrecorded result stays `unknown`, and that a user
edit made after the crash is preserved. Two extra cases cover the dedup ledger:
a key claimed by a process that is SIGKILLed while its tool is still running
must never be replayed, and the persisted task id cannot be a pre-seeded
fixture because the ledger file is produced by the killed process itself.

How the pause points are synchronised
-------------------------------------
`tests/fixtures/crash_probe.c` is compiled at run time into a temporary
directory and injected with LD_PRELOAD. It intercepts `connect()` (pause before
the Nth matching TCP connect) and `rename()` / `renameat()` (pause before a
rename whose destination matches a substring, i.e. the session atomic replace).
At a pause point it atomically creates a marker file, then blocks. The test
observes the marker and sends SIGKILL. This is real external synchronisation
against real libc calls; it is not a production backdoor and it does not modify
any product source.

`during_tool` needs no shim: the real shell tool writes its own process-group
id to a file before sleeping, and the test kills Mio and that process group once
the marker appears.

Honest scope and boundaries
---------------------------
* Linux only (LD_PRELOAD + process groups + /proc-independent but POSIX).
* This is process death via SIGKILL, **not** power loss: page cache is intact,
  so it exercises crash consistency of the write order, not fsync durability
  under hardware failure.
* Loopback mock only; no real provider, no internet, no TLS.
* If `cc` (or the shim compile) is unavailable the six shim-dependent cases are
  reported as `skip`, never as `pass`.
* If a pause point cannot be reached or a post-condition (e.g. "tool result was
  already recorded") is not satisfied at the pause, the case is reported as
  `fail` with the observed state, not silently adjusted.

Exit codes: 0 = all selected cases passed; 1 = at least one failed; 3 = no
failure but at least one skip; 2 = the binary/setup itself is missing.
"""

import argparse
import hashlib
import json
import os
import platform
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_BIN = ROOT / "target" / "debug" / "mio"
FIXTURE_C = Path(__file__).resolve().parent / "fixtures" / "crash_probe.c"

SESSION_REL = ".mio/sessions/recovery.json"
JOURNAL_DIR_REL = ".mio/journal"
DEDUP_REL = ".mio/dedup/task-keys.json"
UNRESOLVED_REL = ".mio/unresolved-effects/effects.json"
# The mock model always dispatches its single crash tool call under this id, so
# crash tests can identify the call even when the canonical shell target is
# redacted (raw command args must never leak into canonical metadata).
CRASH_TOOL_CALL_ID = "call_crash"
USER_EDIT_NAME = "USER-LATER-EDIT-MARKER"
PROBE_CASES = ["pre_request", "after_tool", "before_session_save"]

ALL_CASES = [
    "cli_pre_request",
    "cli_during_tool",
    "cli_after_tool",
    "cli_before_session_save",
    "rpc_pre_request",
    "rpc_during_tool",
    "rpc_after_tool",
    "rpc_before_session_save",
    "cli_dedup_after_crash",
    "rpc_dedup_after_crash",
    "ledger_recovery_ack",
    "ledger_legacy_workspace_scan",
]


class CaseFailure(Exception):
    pass


# --------------------------------------------------------------------------- #
# Mock model server (loopback only)
# --------------------------------------------------------------------------- #

def _sse(delta, finish):
    def chunk(d, f):
        return json.dumps(
            {
                "id": "test",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "test",
                "choices": [{"index": 0, "delta": d, "finish_reason": f}],
            }
        )

    body = (
        f"data: {chunk(delta, None)}\n\n"
        f"data: {chunk({}, finish)}\n\n"
        "data: [DONE]\n\n"
    )
    return body.encode("utf-8")


def _read_request(conn, timeout):
    conn.settimeout(timeout)
    data = b""
    while b"\r\n\r\n" not in data:
        try:
            part = conn.recv(65536)
        except (socket.timeout, OSError):
            break
        if not part:
            break
        data += part
    if b"\r\n\r\n" not in data:
        return {}
    _, rest = data.split(b"\r\n\r\n", 1)
    headers = {}
    for line in data.split(b"\r\n\r\n", 1)[0].split(b"\r\n")[1:]:
        if b":" in line:
            k, v = line.split(b":", 1)
            headers[k.strip().lower()] = v.strip()
    length = int(headers.get(b"content-length", b"0") or b"0")
    while len(rest) < length:
        try:
            part = conn.recv(length - len(rest))
        except (socket.timeout, OSError):
            break
        if not part:
            break
        rest += part
    try:
        return json.loads(rest.decode("utf-8", "replace"))
    except Exception:
        return {}


class MockModel:
    """Minimal OpenAI-compatible SSE server bound to 127.0.0.1.

    On the first request it returns a `workspace` tool call (when
    ``initial="tool_call"``); once the request already carries a tool result it
    always returns plain text. A ``text`` server always returns text.
    """

    def __init__(self, initial="text", tool_command=None, tool_arguments=None):
        self.initial = initial
        self.tool_command = tool_command
        # Checkpoint acceptance needs a real `write` / `edit` workspace call, not
        # the historical shell shape. When given, these exact arguments replace
        # the `{"operation": "shell", "value": tool_command}` envelope; existing
        # callers that only pass `tool_command` keep byte-identical behaviour.
        self.tool_arguments = tool_arguments
        self._stop = threading.Event()
        self._lock = threading.Lock()
        self.requests = []
        self.listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(16)
        self.listener.settimeout(0.2)
        self.port = self.listener.getsockname()[1]
        self._thread = threading.Thread(target=self._serve, daemon=True)
        self._thread.start()

    @property
    def base_url(self):
        return f"http://127.0.0.1:{self.port}/v1"

    def count(self):
        with self._lock:
            return len(self.requests)

    def _serve(self):
        while not self._stop.is_set():
            try:
                conn, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            try:
                self._handle(conn)
            except Exception:
                pass
            finally:
                try:
                    conn.close()
                except Exception:
                    pass

    def _handle(self, conn):
        request = _read_request(conn, 5.0)
        with self._lock:
            self.requests.append(request)
            first_request = len(self.requests) == 1
        messages = request.get("messages", []) if isinstance(request, dict) else []
        # Only the first request of a turn asks for the tool; every later step
        # gets a final answer. Detecting the tool result by role/type is
        # preferred, but the request ordinal is a safe fallback for wire
        # formats that encode tool results differently.
        used_tool = not first_request
        for message in messages:
            if not isinstance(message, dict):
                continue
            if message.get("role") == "tool":
                used_tool = True
            if message.get("role") == "assistant" and message.get("tool_calls"):
                used_tool = True
            content = message.get("content")
            if isinstance(content, list):
                for part in content:
                    if isinstance(part, str) and "tool_call" in part:
                        used_tool = True
                    if isinstance(part, dict) and part.get("type") in ("tool_call", "tool_result"):
                        used_tool = True
        if (
            self.initial == "tool_call"
            and not used_tool
            and (self.tool_arguments is not None or self.tool_command)
        ):
            if self.tool_arguments is not None:
                arguments = json.dumps(self.tool_arguments)
            else:
                arguments = json.dumps({"operation": "shell", "value": self.tool_command})
            delta = {
                "role": "assistant",
                "tool_calls": [
                    {
                        "index": 0,
                        "id": "call_crash",
                        "type": "function",
                        "function": {
                            "name": "workspace",
                            "arguments": arguments,
                        },
                    }
                ],
            }
            body = _sse(delta, "tool_calls")
        else:
            body = _sse({"role": "assistant", "content": "final answer"}, "stop")
        head = (
            "HTTP/1.1 200 OK\r\n"
            "Content-Type: text/event-stream\r\n"
            f"Content-Length: {len(body)}\r\n"
            "Connection: close\r\n\r\n"
        ).encode("ascii")
        try:
            conn.sendall(head + body)
        except OSError:
            pass

    def close(self):
        self._stop.set()
        try:
            self.listener.close()
        except Exception:
            pass
        self._thread.join(timeout=2.0)


# --------------------------------------------------------------------------- #
# Process / environment helpers
# --------------------------------------------------------------------------- #

def clean_env(home):
    env = {}
    for key, value in os.environ.items():
        if key.startswith("MIO_"):
            continue
        if key.endswith("_API_KEY") or "_API_KEY_" in key:
            continue
        if key.lower() in (
            "http_proxy", "https_proxy", "all_proxy", "no_proxy", "ftp_proxy",
        ):
            continue
        env[key] = value
    env["HOME"] = str(home)
    env["MIO_CONFIG_DIR"] = str(Path(home) / ".config" / "mio")
    env["MIO_API_KEY"] = "local-test-key"
    env.pop("LD_PRELOAD", None)
    return env


def kill_hard(proc):
    if proc is None:
        return
    if proc.poll() is not None:
        return
    try:
        os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
    except (ProcessLookupError, PermissionError, OSError):
        try:
            proc.kill()
        except Exception:
            pass
    try:
        proc.wait(timeout=15)
    except subprocess.TimeoutExpired:
        try:
            proc.kill()
            proc.wait(timeout=5)
        except Exception:
            pass


def kill_group(pid):
    try:
        os.killpg(pid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError, OSError):
        pass


def wait_for_file(path, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if Path(path).exists():
            return True
        time.sleep(0.02)
    return False


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(65536), b""):
            digest.update(block)
    return digest.hexdigest()


def read_json(path):
    with open(path, "r", encoding="utf-8") as handle:
        return json.load(handle)


# --------------------------------------------------------------------------- #
# Probe fixture
# --------------------------------------------------------------------------- #

class Probe:
    def __init__(self, cc, workdir):
        self.cc = cc
        self.workdir = Path(workdir)
        self.path = None
        self.error = None

    def compile(self):
        target = self.workdir / "crash_probe.so"
        cmd = [
            self.cc,
            "-shared",
            "-fPIC",
            "-O2",
            "-Wall",
            "-Wextra",
            "-o",
            str(target),
            str(FIXTURE_C),
            "-ldl",
        ]
        try:
            result = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
        except Exception as error:  # noqa: BLE001
            self.error = f"compiler invocation failed: {error}"
            return False
        if result.returncode != 0:
            self.error = (result.stderr or result.stdout or "unknown compiler error").strip()
            return False
        if not target.is_file():
            self.error = "compiler produced no shared object"
            return False
        self.path = target
        return True

    def env(self, ready, connect_hit=None, connect_port=None, rename_match=None, fsync_match=None, hold_ms=60000):
        env = {
            "LD_PRELOAD": str(self.path),
            "MIO_CRASH_PROBE_READY": str(ready),
            "MIO_CRASH_PROBE_HOLD_MS": str(hold_ms),
        }
        if connect_hit is not None:
            env["MIO_CRASH_PROBE_CONNECT_HIT"] = str(connect_hit)
        if connect_port is not None:
            env["MIO_CRASH_PROBE_CONNECT_PORT"] = str(connect_port)
        if rename_match is not None:
            env["MIO_CRASH_PROBE_RENAME_MATCH"] = rename_match
        if fsync_match is not None:
            env["MIO_CRASH_PROBE_FSYNC_MATCH"] = fsync_match
        return env


# --------------------------------------------------------------------------- #
# RPC session helper
# --------------------------------------------------------------------------- #

class RpcSession:
    def __init__(self, argv, env, cwd, stderr_path):
        self.stderr_file = open(stderr_path, "w", encoding="utf-8")
        self.proc = subprocess.Popen(
            argv,
            env=env,
            cwd=cwd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.stderr_file,
            text=True,
            bufsize=1,
            start_new_session=True,
        )
        self.lines = []
        self._thread = threading.Thread(target=self._reader, daemon=True)
        self._thread.start()

    def _reader(self):
        try:
            for line in self.proc.stdout:
                line = line.strip()
                if line:
                    self.lines.append(line)
        except Exception:
            pass

    def send(self, obj):
        assert self.proc.stdin is not None
        self.proc.stdin.write(json.dumps(obj) + "\n")
        self.proc.stdin.flush()

    def wait_for(self, predicate, timeout):
        deadline = time.monotonic() + timeout
        index = 0
        while time.monotonic() < deadline:
            while index < len(self.lines):
                try:
                    parsed = json.loads(self.lines[index])
                except Exception:
                    index += 1
                    continue
                index += 1
                if predicate(parsed):
                    return parsed
            time.sleep(0.02)
        raise CaseFailure(
            "RPC response not observed in time: " + repr(self.lines[-5:])
        )

    def close_stdin(self):
        try:
            if self.proc.stdin:
                self.proc.stdin.close()
        except Exception:
            pass

    def wait(self, timeout=5.0):
        try:
            self.proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            pass

    def kill_hard(self):
        kill_hard(self.proc)

    def close(self):
        self.kill_hard()
        try:
            if self.proc.stdout:
                self.proc.stdout.close()
        except Exception:
            pass
        try:
            self.stderr_file.close()
        except Exception:
            pass


# --------------------------------------------------------------------------- #
# Sandbox
# --------------------------------------------------------------------------- #

class Sandbox:
    def __init__(self, binary, keep_temp=False):
        self.binary = Path(binary).resolve()
        self.keep_temp = keep_temp
        self.root = Path(tempfile.mkdtemp(prefix="mio-crash-recovery-"))
        self.ws = self.root / "workspace"
        self.home = self.root / "home"
        self.logs = self.root / "logs"
        for directory in (self.ws, self.home, self.logs):
            directory.mkdir(parents=True, exist_ok=True)
        self.mocks = []
        self.procs = []
        self.rpcs = []

    def env(self, extra=None):
        env = clean_env(self.home)
        if extra:
            env.update(extra)
        return env

    def mock(self, **kwargs):
        server = MockModel(**kwargs)
        self.mocks.append(server)
        return server

    def spawn_cli(self, argv, env, name):
        out = open(self.logs / f"{name}.out", "w", encoding="utf-8")
        err = open(self.logs / f"{name}.err", "w", encoding="utf-8")
        proc = subprocess.Popen(
            argv,
            env=env,
            cwd=self.ws,
            stdout=out,
            stderr=err,
            start_new_session=True,
        )
        self.procs.append(proc)
        return proc

    def rpc(self, argv, env, name):
        session = RpcSession(argv, env, self.ws, self.logs / f"{name}.err")
        self.rpcs.append(session)
        return session

    def session_path(self):
        return self.ws / SESSION_REL

    def journal_path(self):
        session = read_json(self.session_path())
        return self.ws / JOURNAL_DIR_REL / f"{session['id']}.json"

    def cleanup(self):
        for session in self.rpcs:
            session.close()
        for proc in self.procs:
            kill_hard(proc)
        for server in self.mocks:
            server.close()
        if not self.keep_temp:
            shutil.rmtree(self.root, ignore_errors=True)


# --------------------------------------------------------------------------- #
# Mio command builders
# --------------------------------------------------------------------------- #

def cli_argv(binary, ws, base_url, tail):
    return [
        str(binary),
        "--cwd", str(ws),
        "--no-context",
        "--no-mcp",
        "--no-shared-limits",
        "--provider", "custom",
        "--model", "test",
        "--base-url", base_url,
        *tail,
    ]


def rpc_argv(binary, ws, base_url, session=True):
    argv = [
        str(binary),
        "--cwd", str(ws),
        "--no-context",
        "--no-mcp",
        "--no-shared-limits",
        "--provider", "custom",
        "--model", "test",
        "--base-url", base_url,
        "--mode", "rpc",
    ]
    if session:
        argv += ["--session", SESSION_REL]
    return argv


def run_cli(sandbox, base_url, tail, extra_env=None, timeout=120):
    argv = cli_argv(sandbox.binary, sandbox.ws, base_url, tail)
    return subprocess.run(
        argv,
        env=sandbox.env(extra_env),
        cwd=sandbox.ws,
        capture_output=True,
        text=True,
        timeout=timeout,
    )


def parse_json_lines(text):
    events = []
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            events.append(json.loads(line))
        except Exception:
            continue
    return events


def cli_recovery(sandbox, timeout=60):
    result = run_cli(sandbox, "http://127.0.0.1:1/v1", ["--json", "--recovery"], timeout=timeout)
    if result.returncode != 0:
        raise CaseFailure(
            f"--recovery exited {result.returncode}: {result.stderr.strip()}"
        )
    for event in parse_json_lines(result.stdout):
        if event.get("type") == "recovery":
            return event
    raise CaseFailure(f"no recovery event in --recovery output: {result.stdout!r}")


def rpc_state(sandbox, base_url="http://127.0.0.1:1/v1", timeout=60):
    session = sandbox.rpc(rpc_argv(sandbox.binary, sandbox.ws, base_url), sandbox.env(), "state")
    session.send({"id": "state", "type": "get_state"})
    response = session.wait_for(
        lambda obj: obj.get("type") == "response" and obj.get("command") == "get_state",
        timeout,
    )
    session.close_stdin()
    session.wait(5.0)
    return response["data"]


# --------------------------------------------------------------------------- #
# Journal helpers
# --------------------------------------------------------------------------- #

def journal_ops(sandbox):
    path = sandbox.journal_path()
    if not path.exists():
        return []
    data = read_json(path)
    ops = []
    for run in data.get("runs", []):
        for op in run.get("operations", []):
            ops.append(
                {
                    "run_state": run.get("state"),
                    "operation": op.get("operation"),
                    "target": op.get("target", ""),
                    "status": op.get("status"),
                }
            )
        # Engine tool calls are recorded as canonical (DES-05) calls. They are
        # the authoritative evidence for a tool the engine dispatched, so a
        # crash test must read them too; the legacy `operations` list stays
        # supported for engine-less intents.
        for call in run.get("calls", []):
            ops.append(
                {
                    "run_state": run.get("state"),
                    "operation": call.get("operation"),
                    "target": call.get("target", ""),
                    "status": call.get("phase"),
                    "tool_call_id": call.get("tool_call_id"),
                }
            )
    return ops


def op_matches_case(op, token):
    """Identify this crash case's operation.

    The canonical shell target is redacted on purpose, so the raw token then
    survives only in the goal and in `detail`. Fall back to the fixed mock
    tool_call id in that case; a raw target match still covers legacy intents.
    """
    if token in (op.get("target") or ""):
        return True
    return op.get("tool_call_id") == CRASH_TOOL_CALL_ID


def ops_with_token(ops, token):
    return [op for op in ops if op_matches_case(op, token)]


def recovery_of(recovery_event):
    return recovery_event.get("data") or {}


def recovery_ops(data, key):
    return data.get(key) or []


def canonical_calls(data):
    """Canonical tool calls from a recovery report, normalized like ops."""
    return [
        {
            "operation": call.get("operation"),
            "target": call.get("target", ""),
            "phase": call.get("phase"),
            "tool_call_id": call.get("tool_call_id"),
        }
        for call in (data.get("calls") or [])
    ]


def recovery_unknown(data):
    """Unknown evidence from legacy intents plus canonical calls.

    A canonical call of an interrupted run may still read `dispatched` when the
    report is only read (it has not been rewritten yet); a dispatched effect that
    was never confirmed is conservatively unknown too.
    """
    ops = list(recovery_ops(data, "unknown_operations"))
    ops += [
        call
        for call in data.get("calls", [])
        if call.get("phase") in ("unknown", "dispatched")
    ]
    return ops


def recovery_executed(data):
    ops = list(recovery_ops(data, "executed"))
    ops += [call for call in data.get("calls", []) if call.get("phase") == "executed"]
    return ops


def recovery_failed(data):
    ops = list(recovery_ops(data, "failed_operations"))
    ops += [call for call in data.get("calls", []) if call.get("phase") == "failed"]
    return ops
# --------------------------------------------------------------------------- #
# Case: four injection points (CLI and RPC)
# --------------------------------------------------------------------------- #

def tool_command(sandbox, token, long_running):
    exec_log = sandbox.logs / "exec.log"
    marker = sandbox.logs / "tool.marker"
    parts = [
        f'printf "%s\\n" "$$" >> {shlex.quote(str(exec_log))}',
        f': > {shlex.quote(str(marker))}',
    ]
    if long_running:
        pids = sandbox.logs / "shell.pids"
        parts.insert(1, f'printf "%s\\n" "$$" > {shlex.quote(str(pids))}')
        parts.append("sleep 60")
    parts.append(f'printf "{token}\\n"')
    return "; ".join(parts)


def seed_session(sandbox, timeout):
    mock = sandbox.mock(initial="text")
    result = run_cli(
        sandbox,
        mock.base_url,
        ["--json", "--session", SESSION_REL, "-p", "seed turn"],
        timeout=timeout,
    )
    if result.returncode != 0:
        raise CaseFailure(
            f"seed turn failed rc={result.returncode}: {result.stderr.strip()}"
        )
    if not sandbox.session_path().is_file():
        raise CaseFailure("seed turn did not save a session file")


def probe_injection_env(mode, point, sandbox, probe, probe_available, crash_mock):
    """Build the LD_PRELOAD environment for a shim-dependent injection point."""
    if point not in PROBE_CASES:
        return None
    if not probe_available:
        raise CaseFailure("skip:probe-unavailable")
    ready = sandbox.logs / f"ready.{point}"
    if point == "pre_request":
        return probe.env(ready, connect_hit=1, connect_port=crash_mock.port)
    if point == "after_tool":
        return probe.env(ready, connect_hit=2, connect_port=crash_mock.port)
    # before_session_save: block the temp file's fsync (last call before the
    # atomic replace) and keep the libc rename hook as a fallback.
    return probe.env(ready, fsync_match=".mio/sessions/", rename_match=".mio/sessions/")


def injection_ready(sandbox, point, token, timeout):
    """Wait until the chosen pause point is reached; return evidence."""
    if point == "during_tool":
        # The command writes its pgid, then the marker, then sleeps. Waiting for
        # the marker proves the real side effect happened before we kill.
        marker = sandbox.logs / "tool.marker"
        pids = sandbox.logs / "shell.pids"
        if not wait_for_file(marker, timeout):
            raise CaseFailure("shell tool never created its marker file")
        if not wait_for_file(pids, timeout):
            raise CaseFailure("shell tool never wrote its process group id")
        shell_pid = int(pids.read_text().split()[0])
        return {"kind": "process_group", "shell_pid": shell_pid}
    ready = sandbox.logs / f"ready.{point}"
    if not wait_for_file(ready, timeout):
        raise CaseFailure(f"injection point {point} was never reached (no ready marker)")
    detail = ready.read_text(errors="replace").strip()
    evidence = {"kind": detail}
    if point == "pre_request":
        if not detail.startswith("connect"):
            raise CaseFailure(f"pre_request paused on an unexpected call: {detail!r}")
        ops = ops_with_token(journal_ops(sandbox), token)
        if ops:
            raise CaseFailure(f"pre_request pause already has {len(ops)} recorded op(s)")
        if not sandbox.journal_path().exists():
            raise CaseFailure("journal file missing at pre_request pause")
        if read_json(sandbox.journal_path()).get("runs")[-1].get("state") != "active":
            raise CaseFailure("journal run is not active at pre_request pause")
    if point in ("after_tool", "before_session_save"):
        ops = ops_with_token(journal_ops(sandbox), token)
        if not ops or ops[-1]["status"] != "executed":
            raise CaseFailure(
                f"{point} pause reached but the tool result was not yet recorded"
            )
    if point == "before_session_save" and ".mio/sessions/" not in detail:
        raise CaseFailure(f"session-save pause did not target the session directory: {detail!r}")
    return evidence


def kill_at_point(sandbox, proc, point, evidence):
    if point == "during_tool":
        shell_pid = evidence.get("shell_pid")
        kill_hard(proc)
        if shell_pid:
            kill_group(shell_pid)
    else:
        kill_hard(proc)


def assert_recovery(point, token, data):
    if data.get("interrupted_runs", 0) < 1:
        raise CaseFailure(f"expected an interrupted run, got {data!r}")
    goals = data.get("unfinished_goals") or []
    if not any(goal.get("goal") == f"crash {token}" and not goal.get("truncated") for goal in goals):
        raise CaseFailure(f"interrupted user request not preserved: {goals!r}")
    if point == "pre_request":
        if recovery_executed(data):
            raise CaseFailure(f"pre_request recorded executed ops: {data!r}")
        if recovery_unknown(data):
            raise CaseFailure(f"pre_request recorded unknown ops: {data!r}")
        if ops_with_token(canonical_calls(data), token):
            raise CaseFailure(f"pre_request recorded canonical calls: {data!r}")
    elif point == "during_tool":
        if recovery_executed(data):
            raise CaseFailure(f"during_tool recorded executed ops: {data!r}")
        ops = [op for op in recovery_unknown(data) if op_matches_case(op, token)]
        if len(ops) != 1:
            raise CaseFailure(
                f"during_tool expected exactly one unresolved canonical call for {token}: {data!r}"
            )
        calls = ops_with_token(canonical_calls(data), token)
        if len(calls) != 1 or calls[0]["phase"] not in ("dispatched", "unknown"):
            raise CaseFailure(
                f"during_tool expected exactly one unresolved canonical call for {token}: {data!r}"
            )
    elif point == "after_tool":
        ops = [op for op in recovery_executed(data) if op_matches_case(op, token)]
        if len(ops) != 1:
            raise CaseFailure(f"after_tool expected one executed op: {data!r}")
        if recovery_unknown(data):
            raise CaseFailure(f"after_tool has unknown ops: {data!r}")
        calls = ops_with_token(canonical_calls(data), token)
        if len(calls) != 1 or calls[0]["phase"] != "executed":
            raise CaseFailure(
                f"after_tool expected one executed canonical call: {data!r}"
            )
    elif point == "before_session_save":
        ops = [op for op in recovery_executed(data) if op_matches_case(op, token)]
        if len(ops) != 1:
            raise CaseFailure(f"before_session_save expected one executed op: {data!r}")
        calls = ops_with_token(canonical_calls(data), token)
        if len(calls) != 1 or calls[0]["phase"] != "executed":
            raise CaseFailure(
                f"before_session_save expected one executed canonical call: {data!r}"
            )
    return data


def case_crash(mode, point, sandbox, probe, timeout, probe_available):
    token = f"MIO-CRASH-{mode}-{point}"
    long_running = point == "during_tool"
    command = tool_command(sandbox, token, long_running)
    seed_session(sandbox, timeout)
    seed_bytes = sandbox.session_path().read_bytes()

    crash_mock = sandbox.mock(initial="tool_call", tool_command=command)
    if mode == "cli":
        extra = probe_injection_env(mode, point, sandbox, probe, probe_available, crash_mock)
        argv = cli_argv(
            sandbox.binary,
            sandbox.ws,
            crash_mock.base_url,
            ["--json", "--session", SESSION_REL, "-p", f"crash {token}"],
        )
        proc = sandbox.spawn_cli(argv, sandbox.env(extra), f"crash.{mode}.{point}")
    else:
        extra = probe_injection_env(mode, point, sandbox, probe, probe_available, crash_mock)
        session = sandbox.rpc(
            rpc_argv(sandbox.binary, sandbox.ws, crash_mock.base_url),
            sandbox.env(extra),
            f"crash.{mode}.{point}",
        )
        session.send({"id": "1", "type": "prompt", "message": f"crash {token}"})
        proc = session.proc

    evidence = injection_ready(sandbox, point, token, timeout)
    kill_at_point(sandbox, proc, point, evidence)

    # Real side effect check for during_tool: the marker proves the command ran.
    marker = sandbox.logs / "tool.marker"
    exec_log = sandbox.logs / "exec.log"
    if point == "during_tool" and not marker.exists():
        raise CaseFailure("during_tool tool marker missing: the command did not run")
    if point == "pre_request" and exec_log.exists():
        raise CaseFailure("pre_request crash ran a tool even though no request was sent")
    if exec_log.exists() and len(exec_log.read_text().splitlines()) != 1:
        raise CaseFailure("tool did not run exactly once during the crash turn")
    if sandbox.session_path().read_bytes() != seed_bytes:
        raise CaseFailure("crashed turn unexpectedly rewrote the session file")

    if mode == "cli":
        event = cli_recovery(sandbox)
        if event.get("session") != read_json(sandbox.session_path())["id"]:
            raise CaseFailure(f"recovery report is for a different session: {event!r}")
        data = recovery_of(event)
        view = event.get("view") or {}
        evidence_field = "data"
    else:
        state = rpc_state(sandbox)
        data = state.get("recovery") or {}
        if data.get("session_id") != read_json(sandbox.session_path())["id"]:
            raise CaseFailure(f"get_state recovery is for a different session: {data!r}")
        view = state.get("recoveryView") or {}
        evidence_field = "recovery"
    saved_session = read_json(sandbox.session_path())
    if (view.get("sessionId") != saved_session["id"]
            or view.get("sessionSaved") is not True
            or view.get("savedMessages") != len(saved_session["history"])
            or view.get("automaticReplay") is not False
            or view.get("evidenceField") != evidence_field):
        raise CaseFailure(f"inconsistent persisted recovery view: {view!r}")
    assert_recovery(point, token, data)

    # No replay: a fresh successful text turn must not re-run the crashed tool,
    # and restart must not add a second record for it. `during_tool` is
    # expected to move from pending to unknown on restart (mark_interrupted);
    # the other points stay executed. A different final status would mean the
    # engine silently realized or replayed the effect.
    expected_status = {
        "pre_request": None,
        "during_tool": "unknown",
        "after_tool": "executed",
        "before_session_save": "executed",
    }[point]
    before_snapshot = ops_with_token(journal_ops(sandbox), token)
    text_mock = sandbox.mock(initial="text")
    after = run_cli(
        sandbox,
        text_mock.base_url,
        ["--json", "--session", SESSION_REL, "-p", "post-crash turn"],
        timeout=timeout,
    )
    if after.returncode != 0:
        raise CaseFailure(f"post-crash turn failed rc={after.returncode}: {after.stderr.strip()}")
    if exec_log.exists() and len(exec_log.read_text().splitlines()) != 1:
        raise CaseFailure("crashed tool was replayed during the post-crash turn")
    after_snapshot = ops_with_token(journal_ops(sandbox), token)
    if len(after_snapshot) != len(before_snapshot):
        raise CaseFailure(
            f"recovery created an extra op for {token}: {before_snapshot} -> {after_snapshot}"
        )
    if expected_status is None:
        if after_snapshot:
            raise CaseFailure(f"pre_request should have no recorded op: {after_snapshot}")
    else:
        if len(after_snapshot) != 1 or after_snapshot[0]["status"] != expected_status:
            raise CaseFailure(
                f"expected one {expected_status} op for {token}, got {after_snapshot}"
            )

    report = {
        "injection": evidence,
        "recovery": {
            "interrupted_runs": data.get("interrupted_runs"),
            "executed_operations": data.get("executed_operations"),
            "unknown_operations": len(recovery_ops(data, "unknown_operations")),
        },
        "tool_status": expected_status,
        "tool_ran_once": point != "pre_request",
        "replayed": False,
    }

    if point == "before_session_save":
        report["user_edit_preserved"] = verify_user_edit(sandbox, timeout, mode)

    return report


def verify_user_edit(sandbox, timeout, mode):
    """A user edit made after the crash must survive recovery and later saves."""
    session_path = sandbox.session_path()
    document = read_json(session_path)
    document["name"] = USER_EDIT_NAME
    session_path.write_text(json.dumps(document, indent=2), encoding="utf-8")
    edited_hash = sha256_file(session_path)

    if mode == "cli":
        # --recovery is read-only and must not touch the user's edit.
        cli_recovery(sandbox, timeout=timeout)
        if sha256_file(session_path) != edited_hash:
            raise CaseFailure("--recovery rewrote the user-edited session file")
    else:
        # get_state must load the user's edit and leave the file alone.
        state = rpc_state(sandbox, timeout=timeout)
        if state.get("sessionName") != USER_EDIT_NAME:
            raise CaseFailure(f"get_state did not load the user's session edit: {state!r}")
        if sha256_file(session_path) != edited_hash:
            raise CaseFailure("get_state rewrote the user-edited session file")

    # A user edit to a side-effect artifact must not be overwritten either.
    marker = sandbox.logs / "tool.marker"
    if marker.exists():
        marker.write_text("user-edit\n", encoding="utf-8")

    text_mock = sandbox.mock(initial="text")
    result = run_cli(
        sandbox,
        text_mock.base_url,
        ["--json", "--session", SESSION_REL, "-p", "post-edit turn"],
        timeout=timeout,
    )
    if result.returncode != 0:
        raise CaseFailure(f"post-edit turn failed rc={result.returncode}: {result.stderr.strip()}")
    final = read_json(session_path)
    if final.get("name") != USER_EDIT_NAME:
        raise CaseFailure("a later successful save discarded the user's session edit")
    if marker.exists() and marker.read_text(encoding="utf-8").strip() != "user-edit":
        raise CaseFailure("a later save overwrote the user's artifact edit")
    return True


# --------------------------------------------------------------------------- #
# Case: dedup key after a real SIGKILL
# --------------------------------------------------------------------------- #

def case_dedup(mode, sandbox, timeout):
    key = f"dedup-crash-{mode}"
    token = f"MIO-CRASH-dedup-{mode}"
    command = tool_command(sandbox, token, long_running=True)
    crash_mock = sandbox.mock(initial="tool_call", tool_command=command)
    ledger = sandbox.ws / DEDUP_REL
    if ledger.exists():
        raise CaseFailure("ledger existed before the crash run; test cannot prove it was forged-free")

    if mode == "cli":
        argv = cli_argv(
            sandbox.binary,
            sandbox.ws,
            crash_mock.base_url,
            ["--json", "--session", SESSION_REL, "--dedup-key", key, "-p", "dedup crash task"],
        )
        proc = sandbox.spawn_cli(argv, sandbox.env(), "crash.dedup.cli")
    else:
        session = sandbox.rpc(
            rpc_argv(sandbox.binary, sandbox.ws, crash_mock.base_url),
            sandbox.env(),
            "crash.dedup.rpc",
        )
        session.send(
            {
                "id": "1",
                "type": "prompt",
                "message": "dedup crash task",
                "dedup_key": key,
            }
        )
        proc = session.proc

    marker = sandbox.logs / "tool.marker"
    pids = sandbox.logs / "shell.pids"
    if not wait_for_file(marker, timeout) or not wait_for_file(pids, timeout):
        raise CaseFailure("dedup tool never started; cannot prove an active claim was killed")
    shell_pid = int(pids.read_text().split()[0])
    kill_hard(proc)
    kill_group(shell_pid)

    if not ledger.is_file():
        raise CaseFailure("dedup ledger was not persisted by the killed process")
    ledger_hash = sha256_file(ledger)
    document = read_json(ledger)
    entries = [entry for entry in document.get("entries", []) if entry.get("key") == key]
    if len(entries) != 1:
        raise CaseFailure(f"expected one ledger entry for {key!r}, got {document!r}")
    entry = entries[0]
    if entry.get("state") != "active":
        raise CaseFailure(f"crashed claim is not active: {entry!r}")
    try:
        uuid.UUID(entry["task_id"])
    except Exception:
        raise CaseFailure(f"persisted task id is not a UUID: {entry!r}")
    persisted_task_id = entry["task_id"]

    exec_log = sandbox.logs / "exec.log"
    if exec_log.exists() and len(exec_log.read_text().splitlines()) != 1:
        raise CaseFailure("dedup crash turn did not run the tool exactly once")
    requests_before = crash_mock.count()

    # Retry against an unreachable endpoint: a dedup hit must answer without any
    # model request, so a successful run proves the key was not replayed.
    if mode == "cli":
        retry = run_cli(
            sandbox,
            "http://127.0.0.1:1/v1",
            ["--json", "--session", SESSION_REL, "--dedup-key", key, "-p", "retry"],
            timeout=timeout,
        )
        if retry.returncode != 0:
            raise CaseFailure(f"dedup retry failed rc={retry.returncode}: {retry.stderr.strip()}")
        tasks = [e for e in parse_json_lines(retry.stdout) if e.get("type") == "task"]
        if not tasks:
            raise CaseFailure(f"retry produced no task event: {retry.stdout!r}")
        task = tasks[0]
        if not task.get("reused") or task.get("state") != "active":
            raise CaseFailure(f"retry did not replay the active dedup record: {task!r}")
        if task.get("task_id") != persisted_task_id:
            raise CaseFailure(f"retry returned a different task id: {task!r}")
    else:
        session = sandbox.rpc(
            rpc_argv(sandbox.binary, sandbox.ws, "http://127.0.0.1:1/v1"),
            sandbox.env(),
            "retry.dedup.rpc",
        )
        session.send(
            {
                "id": "2",
                "type": "prompt",
                "message": "retry",
                "dedup_key": key,
            }
        )
        response = session.wait_for(
            lambda obj: obj.get("type") == "response"
            and obj.get("command") == "prompt",
            timeout,
        )
        session.close_stdin()
        session.wait(5.0)
        data = response.get("data") or {}
        if data.get("status") != "deduplicated" or data.get("state") != "active":
            raise CaseFailure(f"RPC retry did not replay the active dedup record: {response!r}")
        if data.get("task_id") != persisted_task_id:
            raise CaseFailure(f"RPC retry returned a different task id: {response!r}")

    if crash_mock.count() != requests_before:
        raise CaseFailure("dedup retry reached the model server instead of reusing the key")
    if exec_log.exists() and len(exec_log.read_text().splitlines()) != 1:
        raise CaseFailure("dedup retry re-executed the crashed tool")
    if sha256_file(ledger) != ledger_hash:
        raise CaseFailure("dedup retry rewrote the persisted ledger")

    return {
        "dedup_key": key,
        "persisted_task_id": persisted_task_id,
        "state": "active",
        "reused_on_retry": True,
        "model_requests_on_retry": 0,
        "tool_replayed": False,
        "ledger_unchanged_on_retry": True,
        "seeded_by_test": False,
    }


# --------------------------------------------------------------------------- #
# Platform / setup checks
# --------------------------------------------------------------------------- #

def find_cc():
    for name in ("cc", "gcc", "clang"):
        path = shutil.which(name)
        if path:
            return path
    return None


def binary_is_dynamic(binary):
    """Best-effort check that connect/rename are dynamic imports."""
    for tool, args in (
        ("nm", ["-D", str(binary)]),
        ("readelf", ["-Ws", str(binary)]),
    ):
        path = shutil.which(tool)
        if not path:
            continue
        try:
            result = subprocess.run([path, *args], capture_output=True, text=True, timeout=60)
        except Exception:
            continue
        if result.returncode != 0:
            continue
        text = result.stdout
        return ("connect" in text) and ("rename" in text)
    return None


def platform_info():
    libc_name, libc_version = platform.libc_ver()
    return {
        "os": sys.platform,
        "kernel": platform.release(),
        "arch": platform.machine(),
        "python": platform.python_version(),
        "libc": libc_name,
        "libc_version": libc_version,
    }


# --------------------------------------------------------------------------- #
# Runner
# --------------------------------------------------------------------------- #


def operations_with_token(sandbox, token):
    """All journal operations/canonical calls belonging to this crash case."""
    journal = sandbox.ws / JOURNAL_DIR_REL
    ops = []
    if not journal.is_dir():
        return ops
    for path in sorted(journal.glob("*.json")):
        try:
            data = read_json(path)
        except Exception:
            continue
        for run in data.get("runs", []):
            for op in run.get("operations", []):
                if op_matches_case(
                    {"target": op.get("target", ""), "tool_call_id": op.get("tool_call_id")},
                    token,
                ):
                    ops.append(
                        {
                            "path": str(path),
                            "run_state": run.get("state"),
                            "operation": op.get("operation"),
                            "status": op.get("status"),
                        }
                    )
            for call in run.get("calls", []):
                if op_matches_case(
                    {
                        "target": call.get("target", ""),
                        "tool_call_id": call.get("tool_call_id"),
                    },
                    token,
                ):
                    ops.append(
                        {
                            "path": str(path),
                            "run_state": run.get("state"),
                            "operation": call.get("operation"),
                            "status": call.get("phase"),
                        }
                    )
    return ops


def read_unresolved_ledger(sandbox):
    path = sandbox.ws / UNRESOLVED_REL
    if not path.exists():
        return None
    return read_json(path)


def approve_until_settled(session, timeout):
    """Approve every approval request until the turn settles; return operations."""
    cursor = 0
    seen = set()
    approved = []
    settled = False
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline and not settled:
        progressed = False
        while cursor < len(session.lines):
            line = session.lines[cursor]
            cursor += 1
            try:
                event = json.loads(line)
            except Exception:
                continue
            progressed = True
            if event.get("type") == "approval_request":
                request_id = event.get("request_id")
                if request_id not in seen:
                    seen.add(request_id)
                    approved.append(event.get("operation"))
                    session.send(
                        {
                            "type": "approve_response",
                            "request_id": request_id,
                            "approved": True,
                        }
                    )
            elif event.get("type") == "agent_settled" and event.get("status") == "completed":
                settled = True
        if not settled and not progressed:
            time.sleep(0.02)
    if not settled:
        raise CaseFailure(f"rpc ack turn never settled: {session.lines[-5:]!r}")
    return approved


def case_ledger_recovery_ack(sandbox, timeout):
    if not sys.platform.startswith("linux"):
        raise CaseFailure("skip:platform")
    token = "MIO-CRASH-ledger-ack"
    command = tool_command(sandbox, token, long_running=True)
    seed_session(sandbox, timeout)
    session_a_id = read_json(sandbox.session_path())["id"]
    session_a_journal = sandbox.ws / JOURNAL_DIR_REL / f"{session_a_id}.json"

    ledger_dir = sandbox.ws / ".mio" / "unresolved-effects"
    # 1. Read-only `--recovery` creates nothing.
    if ledger_dir.exists():
        raise CaseFailure("unresolved-effect ledger existed before the crash run")
    cli_recovery(sandbox, timeout=timeout)
    if ledger_dir.exists():
        raise CaseFailure("--recovery created the unresolved-effect ledger directory")

    # 2. Ephemeral `--no-session` performs zero extra ledger writes.
    no_session_mock = sandbox.mock(initial="text")
    no_session = run_cli(
        sandbox,
        no_session_mock.base_url,
        ["--json", "--no-session", "-p", "ephemeral turn"],
        timeout=timeout,
    )
    if no_session.returncode != 0:
        raise CaseFailure(
            f"--no-session turn failed rc={no_session.returncode}: {no_session.stderr.strip()}"
        )
    if ledger_dir.exists():
        raise CaseFailure("--no-session created the unresolved-effect ledger")

    # 3. A real dispatched shell tool, then a real SIGKILL while it runs.
    crash_mock = sandbox.mock(initial="tool_call", tool_command=command)
    argv = cli_argv(
        sandbox.binary,
        sandbox.ws,
        crash_mock.base_url,
        ["--json", "--session", SESSION_REL, "-p", f"crash {token}"],
    )
    proc = sandbox.spawn_cli(argv, sandbox.env(), "ledger.crash")
    if not wait_for_file(sandbox.logs / "tool.marker", timeout):
        raise CaseFailure("ledger crash: tool never created its marker")
    if not wait_for_file(sandbox.logs / "shell.pids", timeout):
        raise CaseFailure("ledger crash: shell never wrote its process group id")
    shell_pid = int((sandbox.logs / "shell.pids").read_text().split()[0])
    kill_hard(proc)
    kill_group(shell_pid)
    time.sleep(0.2)

    ledger = read_unresolved_ledger(sandbox)
    if not ledger or not ledger.get("entries"):
        raise CaseFailure(f"crashed dispatch left no durable record: {ledger!r}")
    outstanding = [e for e in ledger["entries"] if e.get("state") == "outstanding"]
    if len(outstanding) != 1:
        raise CaseFailure(f"expected exactly one outstanding dispatch: {ledger!r}")
    original_key = (outstanding[0]["scope"], outstanding[0]["call_index"])

    # 4. Restarting the crashed session recovers the pending dispatch as a
    #    conservative unknown, with no fabricated facts and no replay.
    text_mock = sandbox.mock(initial="text")
    post = run_cli(
        sandbox,
        text_mock.base_url,
        ["--json", "--session", SESSION_REL, "-p", "post-crash turn"],
        timeout=timeout,
    )
    if post.returncode != 0:
        raise CaseFailure(f"post-crash turn failed rc={post.returncode}: {post.stderr.strip()}")
    recovered = read_unresolved_ledger(sandbox)
    recovered_entry = next(
        (e for e in recovered["entries"] if (e["scope"], e["call_index"]) == original_key),
        None,
    )
    if recovered_entry is None or recovered_entry.get("state") != "unknown":
        raise CaseFailure(f"restart did not recover the dispatch as unknown: {recovered!r}")
    if recovered_entry.get("recovery") is None:
        raise CaseFailure(f"recovered unknown lacks its recovery record: {recovered_entry!r}")
    if recovered_entry.get("facts"):
        raise CaseFailure("recovery must not fabricate typed facts")
    crashed = [
        op for op in operations_with_token(sandbox, token) if op.get("status") == "unknown"
    ]
    if len(crashed) != 1:
        raise CaseFailure(f"crashed journal op was not marked unknown: {crashed!r}")
    original_journal_bytes = session_a_journal.read_bytes()

    # 5. A different session with a new provider call id must still be refused:
    #    a new session never clears the unknown.
    blocked_path = sandbox.ws / "blocked-write.txt"
    refusal_mock = sandbox.mock(
        initial="tool_call",
        tool_arguments={
            "operation": "write",
            "path": "blocked-write.txt",
            "content": "should not run",
        },
    )
    refusal = run_cli(
        sandbox,
        refusal_mock.base_url,
        ["--json", "--session", ".mio/sessions/refusal.json", "-p", "new write"],
        timeout=timeout,
    )
    if refusal.returncode != 0:
        raise CaseFailure(
            f"refusal turn failed rc={refusal.returncode}: {refusal.stderr.strip()}"
        )
    if blocked_path.exists():
        raise CaseFailure("a new write ran while an unresolved unknown existed")
    refusal_session = read_json(sandbox.ws / ".mio" / "sessions" / "refusal.json")
    refusal_journal = sandbox.ws / JOURNAL_DIR_REL / f"{refusal_session['id']}.json"
    refusal_calls = [
        call
        for run in read_json(refusal_journal).get("runs", [])
        for call in run.get("calls", [])
    ]
    if not any(
        call.get("phase") == "denied"
        and "unknown" in (call.get("detail") or "").lower()
        for call in refusal_calls
    ):
        raise CaseFailure(
            f"the refusal turn did not record an unknown-side-effect denial: {refusal_calls!r}"
        )
    still = read_unresolved_ledger(sandbox)
    still_entry = next(
        (e for e in still["entries"] if (e["scope"], e["call_index"]) == original_key),
        None,
    )
    if still_entry is None or still_entry.get("state") != "unknown":
        raise CaseFailure(f"a refused new session changed the durable unknown: {still!r}")

    # 6. RPC: the host confirms the exact shown snapshot; the ack is persisted
    #    and only then does a new call succeed.
    acked_path = sandbox.ws / "acked-write.txt"
    ack_mock = sandbox.mock(
        initial="tool_call",
        tool_arguments={
            "operation": "write",
            "path": "acked-write.txt",
            "content": "acked",
        },
    )
    rpc_argv_full = rpc_argv(sandbox.binary, sandbox.ws, ack_mock.base_url, session=False)
    rpc_argv_full += ["--session", ".mio/sessions/ack.json", "--approve"]
    session = sandbox.rpc(rpc_argv_full, sandbox.env(), "ledger.ack")
    session.send({"id": "p1", "type": "prompt", "message": "approved write"})
    approved = approve_until_settled(session, timeout)
    if "confirm_unknown_side_effect" not in approved:
        raise CaseFailure(f"host was never asked to confirm the unknown: {approved!r}")
    if not any(op in ("write", "workspace") for op in approved):
        raise CaseFailure(f"the new write was never approved: {approved!r}")
    if not acked_path.is_file():
        raise CaseFailure("the confirmed write did not run after the host ack")
    final = read_unresolved_ledger(sandbox)
    final_entry = next(
        (e for e in final["entries"] if (e["scope"], e["call_index"]) == original_key),
        None,
    )
    if final_entry is None or final_entry.get("state") != "acknowledged":
        raise CaseFailure(f"the host ack was not persisted: {final!r}")
    if not final_entry.get("facts") and not final_entry.get("recovery"):
        raise CaseFailure("acknowledged record lost the original unknown evidence")

    # 7. The ack only confirms the site: the crashed session journal is
    #    byte-identical and its original unknown is unchanged.
    if session_a_journal.read_bytes() != original_journal_bytes:
        raise CaseFailure("acknowledging the ledger rewrote the original journal")
    after_ops = [
        op for op in operations_with_token(sandbox, token) if op.get("status") == "unknown"
    ]
    if len(after_ops) != 1:
        raise CaseFailure(f"original journal unknown changed after ack: {after_ops!r}")

    return {
        "recovered_state": "unknown",
        "new_session_refused": True,
        "rpc_confirmed": "confirm_unknown_side_effect" in approved,
        "acked_state": "acknowledged",
        "original_journal_unchanged": True,
        "no_session_zero_writes": True,
        "recovery_zero_writes": True,
    }



def case_ledger_legacy_workspace_scan(sandbox, timeout):
    """A legacy unknown in another session's journal must block a new session.

    This is the real cross-session boundary: the durable evidence exists only
    in a v1 journal of an old session (no ledger record), so a fresh session
    must still find it via the bounded whole-workspace scan, expose it under a
    distinct origin derived from its real journal identity, and let the
    operator acknowledge exactly that record before a new side effect runs.
    """
    if not sys.platform.startswith("linux"):
        raise CaseFailure("skip:platform")
    seed_session(sandbox, timeout)
    ledger_dir = sandbox.ws / ".mio" / "unresolved-effects"
    if ledger_dir.exists():
        raise CaseFailure("unresolved-effect ledger existed before the legacy scan")

    # A read-only `--recovery` must neither create the ledger nor migrate.
    cli_recovery(sandbox, timeout=timeout)
    if ledger_dir.exists():
        raise CaseFailure("--recovery created the unresolved-effect ledger")

    journal_dir = sandbox.ws / JOURNAL_DIR_REL
    journal_dir.mkdir(parents=True, exist_ok=True)
    legacy_session = "legacy-old-session"
    legacy_run = "11111111-1111-4111-8111-111111111111"
    legacy_journal = {
        "version": 1,
        "session_id": legacy_session,
        "updated": 0,
        "dropped_runs": 0,
        "truncated_operations": 0,
        "runs": [
            {
                "run_id": legacy_run,
                "turn": 1,
                "started": 0,
                "state": "interrupted",
                "ended": 1,
                "operations": [
                    {
                        "op_id": 7,
                        "operation": "mcp__legacy__write",
                        "target": "legacy-target.txt",
                        "status": "unknown",
                        "started": 0,
                        "finished": 1,
                        "detail": "process ended before the operation result was recorded",
                    }
                ],
                "calls": [],
            }
        ],
    }
    (journal_dir / f"{legacy_session}.json").write_text(
        json.dumps(legacy_journal), encoding="utf-8"
    )
    if ledger_dir.exists():
        raise CaseFailure("planting the legacy journal created the unresolved-effect ledger")

    # A fresh session B tries a real write. Its own journal says nothing, so
    # only the whole-workspace scan can refuse it.
    blocked_path = sandbox.ws / "legacy-blocked.txt"
    refusal_mock = sandbox.mock(
        initial="tool_call",
        tool_arguments={
            "operation": "write",
            "path": "legacy-blocked.txt",
            "content": "should not run",
        },
    )
    refusal = run_cli(
        sandbox,
        refusal_mock.base_url,
        ["--json", "--session", ".mio/sessions/legacy-b.json", "-p", "must be refused"],
        timeout=timeout,
    )
    if refusal.returncode != 0:
        raise CaseFailure(
            f"legacy refusal turn failed rc={refusal.returncode}: {refusal.stderr.strip()}"
        )
    if blocked_path.exists():
        raise CaseFailure("a write ran while a legacy workspace unknown existed")

    ledger = read_unresolved_ledger(sandbox)
    if not ledger or not ledger.get("entries"):
        raise CaseFailure(f"legacy workspace scan created no durable record: {ledger!r}")
    legacy_entries = [
        entry for entry in ledger["entries"] if entry.get("scope", "").startswith("journal-legacy:")
    ]
    if len(legacy_entries) != 1:
        raise CaseFailure(f"expected exactly one legacy-origin record: {ledger!r}")
    entry = legacy_entries[0]
    expected_scope = f"journal-legacy:{legacy_session}:{legacy_run}"
    if entry.get("scope") != expected_scope or entry.get("call_index") != 7:
        raise CaseFailure(f"legacy origin is not its real journal identity: {entry!r}")
    if entry.get("state") != "unknown":
        raise CaseFailure(f"legacy record is not a pending unknown: {entry!r}")

    # The bounded maintenance path lists the exact record...
    listing = run_cli(
        sandbox,
        "http://127.0.0.1:1/v1",
        ["--json", "--list-unresolved-effects"],
        timeout=timeout,
    )
    if listing.returncode != 0:
        raise CaseFailure(f"listing failed rc={listing.returncode}: {listing.stderr.strip()}")
    list_events = [
        event for event in parse_json_lines(listing.stdout) if event.get("type") == "unresolved_effects"
    ]
    if not list_events:
        raise CaseFailure(f"listing produced no unresolved_effects event: {listing.stdout!r}")
    rows = list_events[0]["data"]["entries"]
    row = next((item for item in rows if item.get("scope") == expected_scope), None)
    if row is None or not row.get("acknowledgeable"):
        raise CaseFailure(f"legacy record not listed as acknowledgeable: {rows!r}")

    # ...and acknowledges exactly that record (never a delete, never a guess).
    ack = run_cli(
        sandbox,
        "http://127.0.0.1:1/v1",
        [
            "--json",
            "--ack-unresolved-effect",
            json.dumps(
                {
                    "scope": expected_scope,
                    "call_index": 7,
                    "snapshot": row["snapshot"],
                    "note": "operator reviewed the legacy evidence",
                }
            ),
        ],
        timeout=timeout,
    )
    if ack.returncode != 0:
        raise CaseFailure(f"maintenance ack failed rc={ack.returncode}: {ack.stderr.strip()}")
    acked = read_unresolved_ledger(sandbox)
    acked_entry = next(
        (item for item in acked["entries"] if item.get("scope") == expected_scope), None
    )
    if acked_entry is None or acked_entry.get("state") != "acknowledged":
        raise CaseFailure(f"legacy record was not acknowledged exactly: {acked!r}")

    # After the exact ack a new write is allowed and really runs.
    acked_path = sandbox.ws / "legacy-acked.txt"
    ack_mock = sandbox.mock(
        initial="tool_call",
        tool_arguments={
            "operation": "write",
            "path": "legacy-acked.txt",
            "content": "acked",
        },
    )
    allowed = run_cli(
        sandbox,
        ack_mock.base_url,
        ["--json", "--session", ".mio/sessions/legacy-acked.json", "-p", "now allowed"],
        timeout=timeout,
    )
    if allowed.returncode != 0:
        raise CaseFailure(f"post-ack turn failed rc={allowed.returncode}: {allowed.stderr.strip()}")
    if not acked_path.is_file():
        raise CaseFailure("the write did not run after the exact legacy ack")

    return {
        "legacy_scope": expected_scope,
        "refused_before_ack": True,
        "listed_acknowledgeable": True,
        "acked_then_allowed": True,
        "recovery_zero_writes": True,
    }


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
        probe_dir = tempfile.mkdtemp(prefix="mio-crash-probe-")
        probe = Probe(cc, probe_dir)
        if not probe.compile():
            probe_error = probe.error
            probe = None
    elif not linux:
        probe_error = "LD_PRELOAD crash injection is Linux-only"
    else:
        probe_error = "no C compiler (cc/gcc/clang) available to build the shim"

    if dynamic is False:
        probe_error = probe_error or "binary does not dynamically import connect/rename; LD_PRELOAD cannot interpose"
        probe = None

    selected = args.cases
    results = []
    for name in selected:
        started = time.monotonic()
        sandbox = Sandbox(binary, keep_temp=args.keep_temp)
        status = "pass"
        reason = None
        evidence = None
        try:
            if not linux:
                raise CaseFailure("skip:platform")
            if name == "ledger_recovery_ack":
                evidence = case_ledger_recovery_ack(sandbox, args.timeout)
            elif name == "ledger_legacy_workspace_scan":
                evidence = case_ledger_legacy_workspace_scan(sandbox, args.timeout)
            else:
                mode, point = name.split("_", 1)
                if mode == "dedup" or "dedup" in name:
                    mode = name.split("_")[0]
                    evidence = case_dedup(mode, sandbox, args.timeout)
                else:
                    point = name.split("_", 1)[1]
                    probe_available = probe is not None
                    evidence = case_crash(mode, point, sandbox, probe, args.timeout, probe_available)
        except CaseFailure as error:
            text = str(error)
            if text.startswith("skip:"):
                status = "skip"
                reason = text[len("skip:"):]
            else:
                status = "fail"
                reason = text
        except subprocess.TimeoutExpired:
            status = "fail"
            reason = f"case exceeded {args.timeout}s"
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
            "platform": info,
            "injection": "real SIGKILL at externally synchronised libc pause points",
            "synchronisation": "LD_PRELOAD connect/rename shim + real shell marker file",
            "power_loss": False,
            "cross_platform": False,
            "network": "loopback mock only",
        },
        "binary": str(binary),
        "probe": {
            "compiler": cc,
            "compiled": probe is not None,
            "error": probe_error,
            "source": str(FIXTURE_C),
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
        print(f"scope: SIGKILL only (no power loss), Linux/loopback, {info['libc']} {info['libc_version']}")

    if probe_dir:
        shutil.rmtree(probe_dir, ignore_errors=True)

    if summary["failed"]:
        return 1
    if summary["skipped"]:
        return 0 if args.allow_skip else 3
    return 0


def self_check(args):
    binary = Path(args.binary).resolve()
    info = platform_info()
    cc = find_cc()
    probe_ok = False
    probe_error = None
    if cc:
        probe_dir = tempfile.mkdtemp(prefix="mio-crash-probe-")
        probe = Probe(cc, probe_dir)
        probe_ok = probe.compile()
        probe_error = probe.error
        shutil.rmtree(probe_dir, ignore_errors=True)
    else:
        probe_error = "no C compiler available"
    output = {
        "platform": info,
        "linux": sys.platform.startswith("linux"),
        "binary": str(binary),
        "binary_present": binary.is_file(),
        "binary_dynamic": binary_is_dynamic(binary) if binary.is_file() else None,
        "compiler": cc,
        "probe_compiled": probe_ok,
        "probe_error": probe_error,
        "cases": ALL_CASES,
    }
    print(json.dumps(output, indent=2))
    return 0 if probe_ok else 3


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--binary",
        default=os.environ.get("MIO_BIN", str(DEFAULT_BIN)),
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
    parser.add_argument("--allow-skip", action="store_true", help="return 0 when cases are skipped")
    parser.add_argument("--keep-temp", action="store_true", help="do not delete per-case temp dirs")
    parser.add_argument("--self-check", action="store_true", help="compile the shim and report platform support")
    args = parser.parse_args()

    if args.list:
        print("\n".join(ALL_CASES))
        return 0
    if args.self_check:
        return self_check(args)

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
