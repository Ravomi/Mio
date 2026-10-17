#!/usr/bin/env python3
"""Real Ravomi host OSC 7501 contract check against a real Mio CLI binary.

What this script actually proves
-------------------------------
It compiles the *real* Ravomi host parser source, taken in place from the
`ravomi-makepad` mainline:

    <ravomi-root>/crates/ravomi-makepad/src/session_feed.rs

into a throwaway Rust probe with `#[path = "<absolute host path>"] mod
session_feed;`. The probe reads raw bytes and drives the host's own
`Osc7501Scanner::scan` (feeding the input in bounded chunks, so a sequence
split across reads is handled by the host code, not by this script). This
script never copies, re-implements or simulates the parser.

The Mio side is the real CLI binary: it is executed head-less (`-p` / `--json` /
`--mode rpc`) against a loopback mock OpenAI-compatible endpoint, with stderr
attached to a PTY, and the raw PTY bytes it emits are captured and then handed
to the Rust probe. Assertions are made on the probe's parsed events, not on
grep matches against the raw bytes.

Boundaries (explicitly NOT claimed)
-----------------------------------
* Only the OSC 7501 parser (`Osc7501Scanner` / `parse_osc_7501`) of the real
  host source is exercised. The host `SessionFeed` unix-datagram status socket,
  the Makepad GUI, window rendering and terminal emulation are out of scope.
* `mitty-desktop` / GPUI is a historical migration source and is NOT the
  current Ravomi host mainline; it is not used here.
* No real provider is contacted. The mock server binds 127.0.0.1 only, the
  child environment is built from an empty dict (no inherited keys/proxies and
  no inherited `MIO_*`) and only a local loopback test credential is passed.
* The Rust probe consumes Python-captured bytes (an input file / stdin), it
  does not own a live PTY fd.

Usage
-----
    # heavy path: build the probe, run every scenario, write a bound report
    scripts/check-ravomi-host.py \
        --ravomi-root ../Ravomi \
        --mio-binary target/debug/mio \
        --report target/ravomi-host-contract/report.json

    # plan only: resolve paths/hashes and print the exact build command
    scripts/check-ravomi-host.py --ravomi-root ../Ravomi --prepare-only

Exit codes: 0 = pass, 1 = fail, 2 = usage error, 3 = not run (prepare-only).
"""

from __future__ import annotations

import argparse
import base64
import errno
import hashlib
import importlib.util
import json
import os
import pty
import select
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from contextlib import contextmanager
from pathlib import Path

sys.dont_write_bytecode = True

SCHEMA_ID = "mio.ravomi-host-contract/v1"
DEFAULT_TOOLCHAIN = "1.95.0"
DEFAULT_TIMEOUT_S = 30.0
DEFAULT_PROBE_TIMEOUT_S = 15.0
DEFAULT_BUILD_TIMEOUT_S = 1800.0
DEFAULT_MAX_CAPTURE_BYTES = 4 * 1024 * 1024
DEFAULT_MAX_PROBE_INPUT_BYTES = 8 * 1024 * 1024
DEFAULT_CHUNK_SIZES = (1, 7, 4096)

# A local, non-secret loopback credential. It is deliberately distinct from any
# system value and is never read from the caller environment.
LOCAL_TEST_CREDENTIAL = "loopback-local-test-credential"
CREDENTIAL_ENV = "MIO_API_KEY"

# Direct dependency versions used by the real host source. They match
# Mio's own Cargo.lock, which is what lets the probe link the same rlibs.
HOST_DIRECT_DEPS = (
    ("base64", "0.22.1"),
    ("serde", "1.0.229"),
    ("serde_json", "1.0.151"),
    ("tempfile", "3.27.0"),
)

MINIMUM_SCENARIOS = ("success", "error", "approval", "fragment")
EXTRA_SCENARIOS = ("sigint", "json_no_osc", "rpc_no_osc")
REQUIRED_SCENARIOS = MINIMUM_SCENARIOS
ALL_SCENARIOS = MINIMUM_SCENARIOS + EXTRA_SCENARIOS

PROXY_ENV_NAMES = (
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "ftp_proxy",
)
STRIPPED_ENV_PREFIXES = ("MIO_",)

EXIT_PASS = 0
EXIT_FAIL = 1
EXIT_USAGE = 2
EXIT_NOT_RUN = 3

HOST_SOURCE_RELATIVE = Path("crates") / "ravomi-makepad" / "src" / "session_feed.rs"

_BUILD_ENV_ALLOW = ("PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN", "USER", "LOGNAME", "TERM", "LANG", "LC_ALL")


class SetupError(RuntimeError):
    """Raised when the contract cannot even be attempted (never a skip-pass)."""


# --------------------------------------------------------------------------
# Small shared helpers
# --------------------------------------------------------------------------


def utc_now() -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def describe_file(path: Path | None) -> dict:
    if path is None:
        return {"path": None, "present": False}
    path = Path(path)
    info = {"path": str(path), "present": path.is_file()}
    if info["present"]:
        info["size"] = path.stat().st_size
        info["sha256"] = sha256_file(path)
    return info


def resolve_host_source(ravomi_root: Path) -> Path:
    primary = ravomi_root / HOST_SOURCE_RELATIVE
    if primary.is_file():
        return primary
    direct = ravomi_root / "src" / "session_feed.rs"
    if direct.is_file():
        return direct
    return primary


def snapshot_stable(before: str | None, after: str | None) -> bool:
    return bool(before) and before == after


def rust_string_literal(value: str) -> str:
    out = ['"']
    for char in value:
        code = ord(char)
        if char == "\\":
            out.append("\\\\")
        elif char == '"':
            out.append('\\"')
        elif char == "\n":
            out.append("\\n")
        elif char == "\r":
            out.append("\\r")
        elif char == "\t":
            out.append("\\t")
        elif code < 0x20:
            out.append("\\u{%x}" % code)
        else:
            out.append(char)
    out.append('"')
    return "".join(out)


def build_clean_env(home_dir: str | Path, extra: dict | None = None) -> dict:
    """Build the Mio test environment from an empty dict.

    `os.environ` is intentionally not inherited so no system API key, proxy or
    `MIO_*` variable can leak into the run. Only a local loopback credential is
    passed (`CREDENTIAL_ENV`), and its value is a fixed non-secret constant.
    """
    home = Path(home_dir)
    env = {
        "PATH": "/usr/local/bin:/usr/bin:/bin",
        "HOME": str(home),
        "TERM": "xterm-256color",
        "LANG": "C.UTF-8",
        "MIO_CONFIG_DIR": str(home / ".config" / "mio"),
        CREDENTIAL_ENV: LOCAL_TEST_CREDENTIAL,
    }
    if extra:
        env.update(extra)
    return env


def env_policy() -> dict:
    return {
        "cleared": True,
        "base": "empty dict; os.environ is not inherited",
        "allowed_keys": ["PATH", "HOME", "TERM", "LANG", "MIO_CONFIG_DIR", CREDENTIAL_ENV],
        "stripped_prefixes": list(STRIPPED_ENV_PREFIXES),
        "stripped_proxy_names": list(PROXY_ENV_NAMES),
        "credential_env": CREDENTIAL_ENV,
        "credential": "fixed local loopback test credential (value never printed)",
        "network": "loopback mock OpenAI-compatible endpoint only; no outbound network",
    }


def scope_block() -> dict:
    return {
        "claim": (
            "The real Ravomi host OSC 7501 parser source "
            "(crates/ravomi-makepad/src/session_feed.rs -> Osc7501Scanner/parse_osc_7501), "
            "compiled in a throwaway probe via #[path], consumes raw Mio CLI PTY stderr bytes."
        ),
        "host_mainline": (
            "ravomi-makepad (Makepad) is the current host mainline; mitty-desktop/GPUI is a "
            "historical migration source and is not covered."
        ),
        "not_claimed": [
            "full Ravomi GUI or Makepad window rendering",
            "Mio SessionFeed unix-datagram session status socket (MIO_SESSION_STATUS_SOCKET)",
            "PTY emulation or terminal rendering fidelity",
            "real provider / network behavior",
            "SDK or CLI production semantics changes",
        ],
    }


def limitations() -> list:
    return [
        "Only the OSC 7501 parser in the real host session_feed.rs is exercised; "
        "SessionFeed/status socket, GUI and rendering are not.",
        "The Rust probe consumes bytes captured by this script (input file), not a live PTY fd.",
        "All scenarios use a loopback mock OpenAI-compatible endpoint; no real provider is contacted.",
        "mitty-desktop/GPUI is a historical source and is not treated as the current host mainline.",
        "Host source and Mio binary SHA-256 are re-checked before/after the run; a changed file "
        "invalidates the snapshot and the run is reported as failed.",
    ]


def build_scenario_manifest() -> list:
    return [
        {"id": "success", "required": True, "title": "working -> done on a successful turn"},
        {"id": "error", "required": True, "title": "working -> error on HTTP 500"},
        {"id": "approval", "required": True, "title": "blocked(permission), stdin 'y' -> done"},
        {"id": "fragment", "required": True, "title": "fragmented feed through the real scanner"},
        {"id": "sigint", "required": True, "title": "SIGINT -> idle and no later provider request"},
        {"id": "json_no_osc", "required": True, "title": "--json emits no OSC 7501"},
        {"id": "rpc_no_osc", "required": True, "title": "--mode rpc emits no OSC 7501"},
    ]


def validate_report(report: dict) -> list:
    """Structural validation. Returns a list of human-readable errors."""
    errors: list[str] = []
    if not isinstance(report, dict):
        return ["report is not an object"]

    for key in ("schema", "status", "mode", "generated_at", "artifacts", "toolchain", "limits", "scope"):
        if key not in report:
            errors.append(f"missing top-level key: {key}")
    status = report.get("status")
    if status not in ("pass", "fail", "not_run", "incomplete"):
        errors.append(f"invalid status: {status!r}")

    artifacts = report.get("artifacts") or {}
    host = artifacts.get("host_source") or {}
    if not host.get("present"):
        errors.append("host source is missing; the contract can never pass")
    elif not host.get("sha256"):
        errors.append("host source sha256 is missing")
    if not (report.get("scope") or {}).get("not_claimed"):
        errors.append("scope.not_claimed must be non-empty")

    if status == "pass":
        evidence = report.get("probe_evidence") or {}
        if evidence.get("host_source_compiled_this_run") is False and not evidence.get("boundary"):
            errors.append("a reused-probe pass must record the host-source-compilation boundary")
        binary = artifacts.get("mio_binary") or {}
        if not binary.get("sha256"):
            errors.append("mio binary sha256 is missing for a passing report")
        rust = (report.get("toolchain") or {}).get("rust_version")
        if not rust:
            errors.append("toolchain.rust_version is missing for a passing report")
        counts = report.get("counts")
        if not isinstance(counts, dict):
            errors.append("counts is required for a passing report")
        else:
            if not counts.get("required_passed"):
                errors.append("counts.required_passed is false for a passing report")
            if not counts.get("minimum_passed"):
                errors.append("counts.minimum_passed is false for a passing report")
            if counts.get("failed"):
                errors.append("counts.failed is nonzero for a passing report")
            if counts.get("total", 0) < len(MINIMUM_SCENARIOS):
                errors.append("fewer scenarios than the required minimum")
        scenarios = report.get("scenarios") or []
        if not scenarios:
            errors.append("a passing report must list scenarios")
        for scenario in scenarios:
            if scenario.get("status") != "pass":
                errors.append(f"scenario {scenario.get('id')!r} is not pass")
    return errors


def parse_args(argv=None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="check-ravomi-host.py",
        description="Real Ravomi host OSC 7501 parser vs. real Mio CLI PTY bytes.",
    )
    parser.add_argument("--ravomi-root", required=True, type=Path, help="Ravomi repository root")
    parser.add_argument("--mio-binary", type=Path, help="real mio CLI binary (required unless --prepare-only)")
    parser.add_argument("--report", type=Path, help="write the JSON report here (default: stdout)")
    parser.add_argument("--prepare-only", action="store_true", help="emit a plan; run no build and no scenarios")
    parser.add_argument("--toolchain", default=DEFAULT_TOOLCHAIN, help="rustup toolchain (default: 1.95.0)")
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT_S, help="per-scenario timeout seconds")
    parser.add_argument("--probe-timeout", type=float, default=DEFAULT_PROBE_TIMEOUT_S)
    parser.add_argument("--build-timeout", type=float, default=DEFAULT_BUILD_TIMEOUT_S)
    parser.add_argument("--max-capture-bytes", type=int, default=DEFAULT_MAX_CAPTURE_BYTES)
    parser.add_argument("--max-probe-input-bytes", type=int, default=DEFAULT_MAX_PROBE_INPUT_BYTES)
    parser.add_argument("--chunk-sizes", default=",".join(str(value) for value in DEFAULT_CHUNK_SIZES))
    parser.add_argument("--probe-binary", type=Path, help="reuse a prebuilt probe instead of building one")
    parser.add_argument("--keep-temp", action="store_true", help="keep the probe temp directory")
    args = parser.parse_args(argv)

    if not args.prepare_only and args.mio_binary is None:
        parser.error("--mio-binary is required unless --prepare-only is set")

    try:
        chunk_sizes = tuple(int(part) for part in args.chunk_sizes.split(",") if part.strip())
    except ValueError:
        parser.error("--chunk-sizes must be a comma-separated list of integers")
    if not chunk_sizes or any(chunk <= 0 for chunk in chunk_sizes):
        parser.error("--chunk-sizes values must be positive integers")
    args.chunk_sizes = chunk_sizes
    return args


# --------------------------------------------------------------------------
# Bounded process execution
# --------------------------------------------------------------------------


def kill_process_group(proc: subprocess.Popen, sig: int = signal.SIGKILL) -> None:
    try:
        os.killpg(os.getpgid(proc.pid), sig)
    except (ProcessLookupError, PermissionError, OSError):
        pass


def _read_fd(fd: int) -> bytes:
    try:
        return os.read(fd, 65536)
    except OSError as error:
        if error.errno in (errno.EIO, errno.EBADF):
            return b""
        raise


def run_process(
    cmd,
    cwd: Path | None = None,
    env: dict | None = None,
    timeout: float = DEFAULT_TIMEOUT_S,
    stdin_bytes: bytes | None = None,
    max_stdout: int = DEFAULT_MAX_CAPTURE_BYTES,
    max_stderr: int = DEFAULT_MAX_CAPTURE_BYTES,
) -> dict:
    """Run a child with bounded captured output and a hard timeout.

    Output beyond the caps is counted but discarded, never buffered, so a noisy
    child cannot blow memory. On timeout the whole process group is killed.
    """
    started = time.monotonic()
    proc = subprocess.Popen(
        [str(part) for part in cmd],
        cwd=str(cwd) if cwd else None,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    if stdin_bytes is not None:
        try:
            proc.stdin.write(stdin_bytes)
            proc.stdin.flush()
        except (BrokenPipeError, OSError):
            pass
    try:
        proc.stdin.close()
    except (BrokenPipeError, OSError):
        pass

    out_store = bytearray()
    err_store = bytearray()
    out_total = err_total = 0
    timed_out = False
    deadline = started + timeout
    stdout_fd = proc.stdout.fileno()
    stderr_fd = proc.stderr.fileno()
    eof: set[int] = set()
    grace_until: float | None = None

    try:
        while True:
            now = time.monotonic()
            if now > deadline:
                timed_out = True
                kill_process_group(proc)
                break
            active = [fd for fd in (stdout_fd, stderr_fd) if fd not in eof]
            if not active:
                break
            ready, _, _ = select.select(active, [], [], 0.05)
            if ready:
                grace_until = None
                for fd in ready:
                    data = _read_fd(fd)
                    if not data:
                        eof.add(fd)
                        continue
                    if fd == stdout_fd:
                        out_total += len(data)
                        if len(out_store) < max_stdout:
                            out_store.extend(data[: max_stdout - len(out_store)])
                    else:
                        err_total += len(data)
                        if len(err_store) < max_stderr:
                            err_store.extend(data[: max_stderr - len(err_store)])
                continue
            if proc.poll() is not None:
                if grace_until is None:
                    grace_until = now + 0.3
                elif now >= grace_until:
                    break
    finally:
        if proc.poll() is None:
            kill_process_group(proc)
        try:
            returncode = proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            kill_process_group(proc)
            returncode = proc.wait()
        for stream in (proc.stdout, proc.stderr):
            try:
                stream.close()
            except Exception:
                pass

    return {
        "cmd": [str(part) for part in cmd],
        "returncode": returncode,
        "stdout": bytes(out_store),
        "stderr": bytes(err_store),
        "stdout_total_bytes": out_total,
        "stderr_total_bytes": err_total,
        "stdout_truncated": out_total > len(out_store),
        "stderr_truncated": err_total > len(err_store),
        "timed_out": timed_out,
        "duration_s": round(time.monotonic() - started, 3),
    }


def build_env_for_cargo() -> dict:
    env = {key: value for key, value in os.environ.items() if key in _BUILD_ENV_ALLOW}
    env.setdefault("PATH", os.environ.get("PATH", "/usr/bin:/bin"))
    # No network: the lockfile and the local cargo cache must already satisfy the build.
    env["CARGO_NET_OFFLINE"] = "true"
    return env


# --------------------------------------------------------------------------
# Real Mio PTY capture
# --------------------------------------------------------------------------


def run_mio_pty(
    mio_binary: Path,
    args,
    env: dict,
    cwd: Path,
    timeout: float,
    *,
    stdin_pty: bool = False,
    close_stdin: bool = False,
    on_stderr=None,
    sigint_when=None,
    sigint_hook=None,
    max_capture: int = DEFAULT_MAX_CAPTURE_BYTES,
) -> dict:
    """Run the real mio CLI with stderr on a PTY and capture the raw bytes."""
    started = time.monotonic()
    master_err, slave_err = pty.openpty()
    master_in = slave_in = None
    stdin_arg = subprocess.PIPE
    if stdin_pty:
        master_in, slave_in = pty.openpty()
        stdin_arg = slave_in

    proc = subprocess.Popen(
        [str(mio_binary), *args],
        stdin=stdin_arg,
        stdout=subprocess.PIPE,
        stderr=slave_err,
        cwd=str(cwd),
        env=env,
        close_fds=True,
        start_new_session=True,
    )
    os.close(slave_err)
    if slave_in is not None:
        os.close(slave_in)
    if close_stdin and proc.stdin is not None:
        try:
            proc.stdin.close()
        except OSError:
            pass

    stderr_store = bytearray()
    stdout_store = bytearray()
    stderr_total = stdout_total = 0
    timed_out = False
    sigint_sent = False
    deadline = started + timeout
    stdout_fd = proc.stdout.fileno() if proc.stdout else None
    all_fds = [master_err] + ([stdout_fd] if stdout_fd is not None else [])
    eof: set[int] = set()
    grace_until: float | None = None

    def write_stdin(payload: bytes) -> None:
        try:
            if master_in is not None:
                os.write(master_in, payload)
            elif proc.stdin is not None and not proc.stdin.closed:
                proc.stdin.write(payload)
                proc.stdin.flush()
        except (BrokenPipeError, OSError):
            pass

    def store(fd: int, data: bytes) -> None:
        nonlocal stderr_total, stdout_total
        if fd == master_err:
            stderr_total += len(data)
            if len(stderr_store) < max_capture:
                stderr_store.extend(data[: max_capture - len(stderr_store)])
        else:
            stdout_total += len(data)
            if len(stdout_store) < max_capture:
                stdout_store.extend(data[: max_capture - len(stdout_store)])

    try:
        while True:
            now = time.monotonic()
            if now > deadline:
                timed_out = True
                kill_process_group(proc)
                break
            if sigint_when is not None and not sigint_sent and sigint_when():
                sigint_sent = True
                if sigint_hook is not None:
                    try:
                        sigint_hook()
                    except Exception:
                        pass
                try:
                    os.killpg(os.getpgid(proc.pid), signal.SIGINT)
                except (ProcessLookupError, PermissionError, OSError):
                    pass
            active = [fd for fd in all_fds if fd not in eof]
            if not active:
                break
            ready, _, _ = select.select(active, [], [], 0.05)
            if ready:
                grace_until = None
                for fd in ready:
                    data = _read_fd(fd)
                    if not data:
                        eof.add(fd)
                        continue
                    store(fd, data)
                    if fd == master_err and on_stderr is not None:
                        on_stderr(bytes(stderr_store), write_stdin)
                continue
            if proc.poll() is not None:
                if grace_until is None:
                    grace_until = now + 0.3
                elif now >= grace_until:
                    break
    finally:
        if proc.poll() is None:
            kill_process_group(proc)
        try:
            returncode = proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            kill_process_group(proc)
            returncode = proc.wait()
        for fd in (master_err, master_in):
            if fd is not None:
                try:
                    os.close(fd)
                except OSError:
                    pass
        if proc.stdout is not None:
            try:
                proc.stdout.close()
            except Exception:
                pass

    return {
        "args": list(args),
        "returncode": returncode,
        "stderr": bytes(stderr_store),
        "stdout": bytes(stdout_store),
        "stderr_total_bytes": stderr_total,
        "stdout_total_bytes": stdout_total,
        "stderr_truncated": stderr_total > len(stderr_store),
        "stdout_truncated": stdout_total > len(stdout_store),
        "timed_out": timed_out,
        "duration_s": round(time.monotonic() - started, 3),
    }


# --------------------------------------------------------------------------
# OSC helpers reused from tests/osc7501.py
# --------------------------------------------------------------------------


def load_osc_module(mio_root: Path):
    path = Path(mio_root) / "tests" / "osc7501.py"
    if not path.is_file():
        raise SetupError(f"public OSC helpers not found: {path}")
    spec = importlib.util.spec_from_file_location("mio_osc7501_helpers", path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def start_counting_mock(osc, mode: str):
    server, port = osc.start_mock_server(mode)

    class CountingHandler(osc.MockOpenAIHandler):
        def do_POST(self):
            self.server.request_count = getattr(self.server, "request_count", 0) + 1
            self.server.last_request_monotonic = time.monotonic()
            return super().do_POST()

    server.RequestHandlerClass = CountingHandler
    server.request_count = 0
    server.last_request_monotonic = None
    return server, port


def raw_has_osc(raw: bytes) -> bool:
    return b"\x1b]7501;" in raw


# --------------------------------------------------------------------------
# Rust probe
# --------------------------------------------------------------------------


PROBE_SOURCE_TEMPLATE = r'''#![allow(dead_code, unused_imports, unused_variables)]

// Compiles the real host parser in place. This is the actual file, not a copy.
#[path = __HOST_SOURCE_LITERAL__]
mod session_feed;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use session_feed::{ActivityState, Osc7501, Osc7501Scanner, OscEvent, WaitingFor};
use std::io::Read;
use std::process::ExitCode;

fn state_name(state: &ActivityState) -> &'static str {
    match state {
        ActivityState::Idle => "idle",
        ActivityState::Working => "working",
        ActivityState::Done => "done",
        ActivityState::Blocked => "blocked",
        ActivityState::Error => "error",
    }
}

fn kind_name(kind: &Option<WaitingFor>) -> &'static str {
    match kind {
        None => "-",
        Some(WaitingFor::Permission) => "permission",
        Some(WaitingFor::Question) => "question",
        Some(WaitingFor::Authentication) => "authentication",
    }
}

fn encode_text(value: &Option<String>) -> String {
    match value {
        None => "-".to_string(),
        Some(text) => format!("b64:{}", BASE64.encode(text.as_bytes())),
    }
}

fn emit(report: &Osc7501) {
    println!(
        "EVENT state={} kind={} app={} id={} title={} msg={} progress={}",
        state_name(&report.state),
        kind_name(&report.waiting_for),
        encode_text(&report.app),
        encode_text(&report.id),
        encode_text(&report.title),
        encode_text(&report.message),
        match report.progress {
            Some(progress) => progress.to_string(),
            None => "-".to_string(),
        },
    );
}

fn main() -> ExitCode {
    let mut chunk: usize = 0;
    let mut max_bytes: usize = 64 * 1024 * 1024;
    let mut input: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--chunk" => chunk = args.next().and_then(|value| value.parse().ok()).unwrap_or(0),
            "--max-bytes" => {
                max_bytes = args.next().and_then(|value| value.parse().ok()).unwrap_or(max_bytes)
            }
            "--input" => input = args.next(),
            _ => {}
        }
    }
    let data = match input {
        Some(path) => match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                eprintln!("PROBE_ERROR input {path}: {error}");
                return ExitCode::from(2);
            }
        },
        None => {
            let mut buffer = Vec::new();
            if let Err(error) = std::io::stdin().read_to_end(&mut buffer) {
                eprintln!("PROBE_ERROR stdin: {error}");
                return ExitCode::from(2);
            }
            buffer
        }
    };
    if max_bytes > 0 && data.len() > max_bytes {
        eprintln!("PROBE_ERROR input {} exceeds max-bytes {max_bytes}", data.len());
        return ExitCode::from(3);
    }
    let step = if chunk == 0 { data.len().max(1) } else { chunk };
    let mut scanner = Osc7501Scanner::new();
    let mut event_count: usize = 0;
    let mut index: usize = 0;
    while index < data.len() {
        let end = (index + step).min(data.len());
        for event in scanner.scan(&data[index..end]) {
            match event {
                OscEvent::Status(report) => {
                    event_count += 1;
                    emit(&report);
                }
            }
        }
        index = end;
    }
    println!("PROBE_SUMMARY events={event_count} bytes={} chunk={step}", data.len());
    ExitCode::SUCCESS
}
'''


def build_probe_source(host_source: Path) -> str:
    literal = rust_string_literal(str(host_source))
    return PROBE_SOURCE_TEMPLATE.replace("__HOST_SOURCE_LITERAL__", literal)


def parse_cargo_artifacts(stdout_text: str, deps_specs) -> dict:
    wanted = {(name, version) for name, version in deps_specs}
    found: dict[tuple[str, str], dict] = {}
    for line in stdout_text.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if message.get("reason") != "compiler-artifact":
            continue
        package_id = message.get("package_id", "")
        tail = package_id.split("#", 1)[-1]
        if "@" not in tail:
            continue
        name, version = tail.rsplit("@", 1)
        key = (name, version)
        if key not in wanted:
            continue
        for filename in message.get("filenames", []):
            if filename.endswith(".rlib"):
                found[key] = {"name": name, "version": version, "path": filename}
    return found


def build_probe(ctx: dict) -> tuple:
    """Build the throwaway probe and return (probe_binary, rlibs, build_info)."""
    probe_dir: Path = ctx["probe_dir"]
    mio_root: Path = ctx["mio_root"]
    toolchain: str = ctx["toolchain"]
    env = build_env_for_cargo()
    build_cmd = [
        "cargo",
        f"+{toolchain}",
        "build",
        "--locked",
        "--lib",
        "--bin",
        "mio",
        "--message-format=json",
    ]
    result = run_process(
        build_cmd,
        cwd=mio_root,
        env=env,
        timeout=ctx["build_timeout"],
        max_stdout=64 * 1024 * 1024,
        max_stderr=8 * 1024 * 1024,
    )
    build_info = {
        "command": build_cmd,
        "returncode": result["returncode"],
        "timed_out": result["timed_out"],
        "duration_s": result["duration_s"],
        "stdout_sha256": sha256_bytes(result["stdout"]),
        "stderr_tail": result["stderr"][-2000:].decode("utf-8", errors="replace"),
    }
    if result["returncode"] != 0:
        raise SetupError(
            f"cargo build failed (rc={result['returncode']}); stderr tail:\n{build_info['stderr_tail']}"
        )

    artifacts = parse_cargo_artifacts(result["stdout"].decode("utf-8", errors="replace"), HOST_DIRECT_DEPS)
    missing = [f"{name}@{version}" for name, version in HOST_DIRECT_DEPS if (name, version) not in artifacts]
    if missing:
        raise SetupError(f"compiler artifacts missing for locked host deps: {', '.join(missing)}")

    extern_paths = [artifacts[(name, version)]["path"] for name, version in HOST_DIRECT_DEPS]
    deps_dir = Path(extern_paths[0]).parent

    probe_source = probe_dir / "probe.rs"
    probe_source.write_text(build_probe_source(ctx["host_source"]), encoding="utf-8")
    probe_binary = probe_dir / "mio-ravomi-host-probe"

    rustc_cmd = [
        "rustup",
        "run",
        toolchain,
        "rustc",
        "--edition=2024",
        "--crate-name",
        "mio_ravomi_host_probe",
        "--crate-type",
        "bin",
        "-C",
        "opt-level=0",
        "-L",
        f"dependency={deps_dir}",
    ]
    for name, path in zip((name for name, _ in HOST_DIRECT_DEPS), extern_paths):
        rustc_cmd += ["--extern", f"{name}={path}"]
    rustc_cmd += [str(probe_source), "-o", str(probe_binary)]

    compile_result = run_process(
        rustc_cmd,
        cwd=probe_dir,
        env=env,
        timeout=ctx["build_timeout"],
        max_stdout=8 * 1024 * 1024,
        max_stderr=8 * 1024 * 1024,
    )
    build_info["rustc_command"] = rustc_cmd
    build_info["rustc_returncode"] = compile_result["returncode"]
    build_info["rustc_stderr_tail"] = compile_result["stderr"][-4000:].decode("utf-8", errors="replace")
    if compile_result["returncode"] != 0 or not probe_binary.is_file():
        raise SetupError(
            f"probe compile failed (rc={compile_result['returncode']}); stderr tail:\n"
            f"{build_info['rustc_stderr_tail']}"
        )

    rlibs = [
        {
            "name": artifacts[(name, version)]["name"],
            "version": artifacts[(name, version)]["version"],
            "path": artifacts[(name, version)]["path"],
            "sha256": sha256_file(Path(artifacts[(name, version)]["path"])),
        }
        for name, version in HOST_DIRECT_DEPS
    ]
    build_info["probe_source_sha256"] = sha256_file(probe_source)
    return probe_binary, rlibs, build_info


# --------------------------------------------------------------------------
# Probe execution + parsing
# --------------------------------------------------------------------------


def decode_probe_text(value: str | None):
    if value is None or value == "-":
        return None
    if value.startswith("b64:"):
        try:
            return base64.b64decode(value[4:]).decode("utf-8")
        except Exception:
            return None
    return value


def parse_probe_output(text: str) -> dict:
    events = []
    summary = None
    other = []
    for line in text.splitlines():
        line = line.rstrip("\r")
        if line.startswith("EVENT "):
            fields = {}
            for token in line[len("EVENT "):].strip().split(" "):
                if "=" in token:
                    key, value = token.split("=", 1)
                    fields[key] = value
            progress = fields.get("progress", "-")
            events.append(
                {
                    "state": fields.get("state"),
                    "kind": decode_probe_text(fields.get("kind")),
                    "app": decode_probe_text(fields.get("app")),
                    "id": decode_probe_text(fields.get("id")),
                    "title": decode_probe_text(fields.get("title")),
                    "message": decode_probe_text(fields.get("msg")),
                    "progress": None if progress == "-" else int(progress),
                }
            )
        elif line.startswith("PROBE_SUMMARY "):
            fields = {}
            for token in line[len("PROBE_SUMMARY "):].strip().split(" "):
                if "=" in token:
                    key, value = token.split("=", 1)
                    try:
                        fields[key] = int(value)
                    except ValueError:
                        fields[key] = value
            summary = fields
        elif line.strip():
            other.append(line)
    return {"events": events, "summary": summary, "other": other}


def run_probe(ctx: dict, label: str, raw_bytes: bytes, chunk: int) -> dict:
    probe_dir: Path = ctx["probe_dir"]
    input_path = probe_dir / f"probe-input-{label}-chunk{chunk}.bin"
    input_path.write_bytes(raw_bytes)
    cmd = [
        str(ctx["probe_binary"]),
        "--input",
        str(input_path),
        "--chunk",
        str(chunk),
        "--max-bytes",
        str(ctx["limits"]["max_probe_input_bytes"]),
    ]
    result = run_process(
        cmd,
        timeout=ctx["limits"]["probe_timeout_s"],
        max_stdout=8 * 1024 * 1024,
        max_stderr=1 * 1024 * 1024,
    )
    parsed = parse_probe_output(result["stdout"].decode("utf-8", errors="replace"))
    return {
        "chunk": chunk,
        "raw_sha256": sha256_bytes(raw_bytes),
        "raw_bytes": len(raw_bytes),
        "probe_returncode": result["returncode"],
        "probe_timed_out": result["timed_out"],
        "probe_duration_s": result["duration_s"],
        "probe_stderr": result["stderr"].decode("utf-8", errors="replace")[-1000:],
        "events": parsed["events"],
        "summary": parsed["summary"],
        "other": parsed["other"],
    }


def state_sequence(events) -> list:
    return [event.get("state") for event in events]


def check(name: str, ok: bool, detail=None) -> dict:
    return {"name": name, "ok": bool(ok), "detail": detail}


def scenario_status(checks) -> str:
    return "pass" if checks and all(item["ok"] for item in checks) else "fail"


# --------------------------------------------------------------------------
# Scenarios
# --------------------------------------------------------------------------


@contextmanager
def scenario_dirs():
    with tempfile.TemporaryDirectory(prefix="mio-host-home-") as home:
        with tempfile.TemporaryDirectory(prefix="mio-host-cwd-") as cwd:
            yield Path(home), Path(cwd)


def _capture_and_probe(ctx, osc, scenario_id, mode, extra_args, *, timeout=None, post_run=None, **run_kwargs):
    server, port = start_counting_mock(osc, mode)
    timeout = timeout or ctx["limits"]["scenario_timeout_s"]
    try:
        with scenario_dirs() as (home, cwd):
            env = build_clean_env(home)
            base = osc.get_base_cli_args(port, cwd)
            args = list(base) + list(extra_args)
            record = run_mio_pty(
                ctx["mio_binary"],
                args,
                env,
                cwd,
                timeout,
                max_capture=ctx["limits"]["max_capture_bytes"],
                **run_kwargs,
            )
            record["mock_mode"] = mode
            record["port"] = port
            record["server_request_count"] = server.request_count
            if post_run is not None:
                post_run(record, cwd)
        return record
    finally:
        server.release_event.set()
        server.shutdown()
        server.server_close()


def scenario_success(ctx, osc) -> tuple:
    record = _capture_and_probe(ctx, osc, "success", "success", ["-p", "hello"])
    raw = record["stderr"]
    probe = run_probe(ctx, "success", raw, 0)
    states = state_sequence(probe["events"])
    checks = [
        check("osc_present_in_raw_pty", raw_has_osc(raw), f"{len(raw)} bytes"),
        check("mio_exit_zero", record["returncode"] == 0, f"rc={record['returncode']}"),
        check("probe_ran", probe["probe_returncode"] == 0, probe["probe_stderr"]),
        check("working_present", "working" in states, states),
        check("last_state_done", bool(states) and states[-1] == "done", states),
    ]
    scenario = {
        "id": "success",
        "title": "working -> done on a successful turn",
        "required": True,
        "status": scenario_status(checks),
        "checks": checks,
        "mio": _mio_summary(record),
        "raw_sha256": probe["raw_sha256"],
        "raw_bytes": probe["raw_bytes"],
        "events": probe["events"],
        "probe": _probe_summary(probe),
    }
    return scenario, raw


def scenario_error(ctx, osc) -> tuple:
    record = _capture_and_probe(ctx, osc, "error", "http_500", ["-p", "hello"])
    raw = record["stderr"]
    probe = run_probe(ctx, "error", raw, 0)
    states = state_sequence(probe["events"])
    checks = [
        check("osc_present_in_raw_pty", raw_has_osc(raw), f"{len(raw)} bytes"),
        check("mio_exit_nonzero", record["returncode"] != 0, f"rc={record['returncode']}"),
        check("probe_ran", probe["probe_returncode"] == 0, probe["probe_stderr"]),
        check("working_present", "working" in states, states),
        check("last_state_error", bool(states) and states[-1] == "error", states),
    ]
    scenario = {
        "id": "error",
        "title": "working -> error on HTTP 500",
        "required": True,
        "status": scenario_status(checks),
        "checks": checks,
        "mio": _mio_summary(record),
        "raw_sha256": probe["raw_sha256"],
        "raw_bytes": probe["raw_bytes"],
        "events": probe["events"],
        "probe": _probe_summary(probe),
    }
    return scenario, raw


def scenario_approval(ctx, osc) -> tuple:
    answered = {"done": False}

    def on_stderr(buffer, write_stdin):
        if answered["done"]:
            return
        try:
            events = osc.parse_osc_events(buffer)
        except Exception:
            events = []
        if any(event.get("state") == "blocked" for event in events):
            answered["done"] = True
            write_stdin(b"y\n")

    observed = {"file_created": False}

    def post_run(_record, cwd):
        observed["file_created"] = (Path(cwd) / "approved.txt").is_file()

    record = _capture_and_probe(
        ctx,
        osc,
        "approval",
        "approve_workspace",
        ["--approve", "-p", "write file"],
        stdin_pty=True,
        on_stderr=on_stderr,
        post_run=post_run,
    )
    raw = record["stderr"]
    probe = run_probe(ctx, "approval", raw, 0)
    states = state_sequence(probe["events"])
    blocked = [event for event in probe["events"] if event["state"] == "blocked"]
    checks = [
        check("osc_present_in_raw_pty", raw_has_osc(raw), f"{len(raw)} bytes"),
        check("stdin_approve_sent", answered["done"], "wrote 'y' after blocked"),
        check("mio_exit_zero", record["returncode"] == 0, f"rc={record['returncode']}"),
        check("probe_ran", probe["probe_returncode"] == 0, probe["probe_stderr"]),
        check("working_present", "working" in states, states),
        check("blocked_present", bool(blocked), states),
        check(
            "blocked_kind_permission",
            any(event["kind"] == "permission" for event in blocked),
            [event["kind"] for event in blocked],
        ),
        check("last_state_done", bool(states) and states[-1] == "done", states),
        check("approved_file_created", observed["file_created"], "workspace write landed in cwd"),
    ]
    scenario = {
        "id": "approval",
        "title": "blocked(permission), stdin 'y' -> done",
        "required": True,
        "status": scenario_status(checks),
        "checks": checks,
        "mio": _mio_summary(record),
        "raw_sha256": probe["raw_sha256"],
        "raw_bytes": probe["raw_bytes"],
        "events": probe["events"],
        "probe": _probe_summary(probe),
    }
    return scenario, raw


def scenario_fragment(ctx, osc, success_raw: bytes) -> tuple:
    checks = []
    baseline = None
    observed = {}
    for chunk in ctx["limits"]["chunk_sizes"]:
        probe = run_probe(ctx, "fragment", success_raw, chunk)
        observed[chunk] = probe
        if baseline is None:
            baseline = probe
        same = probe["events"] == baseline["events"]
        chunks_ok = (probe["summary"] or {}).get("chunk") == chunk or len(success_raw) == 0
        checks.append(
            check(
                f"chunk_{chunk}_matches_baseline",
                same and chunks_ok and probe["probe_returncode"] == 0,
                {"same_events": same, "summary": probe["summary"]},
            )
        )
    baseline_states = state_sequence(baseline["events"]) if baseline else []
    checks.append(check("baseline_working_done", baseline_states[:1] == ["working"] and baseline_states[-1:] == ["done"], baseline_states))

    synthetic = b"\x1b]7501;state=working:progress=5\x1b\\\x1b]7501;state=done\x07plain"
    synthetic_probe = run_probe(ctx, "fragment-synthetic", synthetic, 1)
    checks.append(
        check(
            "split_terminator_at_1_byte",
            state_sequence(synthetic_probe["events"]) == ["working", "done"]
            and synthetic_probe["events"][0]["progress"] == 5
            and synthetic_probe["events"][1]["kind"] is None,
            synthetic_probe["events"],
        )
    )
    lone_esc = b"\x1b"
    lone_then_rest = lone_esc + b"]7501;state=idle\x07"
    lone_probe = run_probe(ctx, "fragment-lone-esc", lone_then_rest, 1)
    checks.append(
        check(
            "lone_esc_across_reads",
            state_sequence(lone_probe["events"]) == ["idle"],
            lone_probe["events"],
        )
    )

    scenario = {
        "id": "fragment",
        "title": "fragmented feed through the real scanner",
        "required": True,
        "status": scenario_status(checks),
        "checks": checks,
        "input_sha256": sha256_bytes(success_raw),
        "input_bytes": len(success_raw),
        "chunk_sizes": list(ctx["limits"]["chunk_sizes"]),
        "chunk_events": {str(chunk): probe["events"] for chunk, probe in observed.items()},
        "synthetic_events": synthetic_probe["events"],
        "lone_esc_events": lone_probe["events"],
    }
    return scenario, success_raw


def scenario_sigint(ctx, osc) -> tuple:
    server, port = start_counting_mock(osc, "hang")
    state = {"requests_at_sigint": None, "sent": False}

    def sigint_when():
        return server.request_count >= 1 and not state["sent"]

    def sigint_hook():
        state["sent"] = True
        state["requests_at_sigint"] = server.request_count

    timeout = ctx["limits"]["scenario_timeout_s"]
    try:
        with scenario_dirs() as (home, cwd):
            env = build_clean_env(home)
            args = osc.get_base_cli_args(port, cwd) + ["-p", "hello"]
            record = run_mio_pty(
                ctx["mio_binary"],
                args,
                env,
                cwd,
                timeout,
                sigint_when=sigint_when,
                sigint_hook=sigint_hook,
                max_capture=ctx["limits"]["max_capture_bytes"],
            )
        time.sleep(0.5)
        requests_after = server.request_count
    finally:
        server.release_event.set()
        server.shutdown()
        server.server_close()

    raw = record["stderr"]
    probe = run_probe(ctx, "sigint", raw, 0)
    states = state_sequence(probe["events"])
    requests_at_sigint = state["requests_at_sigint"]
    checks = [
        check("sigint_sent", state["sent"], state),
        check("osc_present_in_raw_pty", raw_has_osc(raw), f"{len(raw)} bytes"),
        check("mio_exit_nonzero", record["returncode"] != 0, f"rc={record['returncode']}"),
        check("not_timed_out", not record["timed_out"], f"timed_out={record['timed_out']}"),
        check("probe_ran", probe["probe_returncode"] == 0, probe["probe_stderr"]),
        check("working_present", "working" in states, states),
        check("last_state_idle", bool(states) and states[-1] == "idle", states),
        check(
            "no_request_after_sigint",
            requests_at_sigint is not None and requests_after == requests_at_sigint,
            {"at_sigint": requests_at_sigint, "after": requests_after},
        ),
    ]
    scenario = {
        "id": "sigint",
        "title": "SIGINT -> idle and no later provider request",
        "required": True,
        "status": scenario_status(checks),
        "checks": checks,
        "mio": _mio_summary(record),
        "mock_requests": {"at_sigint": requests_at_sigint, "after": requests_after},
        "raw_sha256": probe["raw_sha256"],
        "raw_bytes": probe["raw_bytes"],
        "events": probe["events"],
        "probe": _probe_summary(probe),
    }
    return scenario, raw


def scenario_json_no_osc(ctx, osc) -> tuple:
    server, port = start_counting_mock(osc, "http_500")
    timeout = ctx["limits"]["scenario_timeout_s"]
    try:
        with scenario_dirs() as (home, cwd):
            env = build_clean_env(home)
            args = ["--json"] + osc.get_base_cli_args(port, cwd) + ["-p", "hello"]
            record = run_mio_pty(
                ctx["mio_binary"],
                args,
                env,
                cwd,
                timeout,
                max_capture=ctx["limits"]["max_capture_bytes"],
            )
    finally:
        server.release_event.set()
        server.shutdown()
        server.server_close()

    raw = record["stderr"]
    probe = run_probe(ctx, "json_no_osc", raw, 0)
    stdout_text = record["stdout"].decode("utf-8", errors="replace")
    json_error = None
    for line in stdout_text.splitlines():
        line = line.strip()
        if line.startswith("{"):
            try:
                parsed = json.loads(line)
            except json.JSONDecodeError:
                continue
            if parsed.get("type") == "error":
                json_error = parsed
                break
    checks = [
        check("raw_has_no_osc_substring", not raw_has_osc(raw), f"{len(raw)} bytes"),
        check("probe_events_zero", probe["probe_returncode"] == 0 and not probe["events"], probe["events"]),
        check("mio_exit_nonzero", record["returncode"] != 0, f"rc={record['returncode']}"),
        check("json_error_on_stdout", json_error is not None, sorted(json_error) if json_error else "no error object"),
    ]
    scenario = {
        "id": "json_no_osc",
        "title": "--json emits no OSC 7501",
        "required": True,
        "status": scenario_status(checks),
        "checks": checks,
        "mio": _mio_summary(record),
        "raw_sha256": probe["raw_sha256"],
        "raw_bytes": probe["raw_bytes"],
        "events": probe["events"],
        "probe": _probe_summary(probe),
    }
    return scenario, raw


def scenario_rpc_no_osc(ctx, osc) -> tuple:
    timeout = ctx["limits"]["scenario_timeout_s"]
    with scenario_dirs() as (home, cwd):
        env = build_clean_env(home)
        # No mock server is contacted: stdin is closed immediately in RPC mode.
        args = osc.get_base_cli_args(9, cwd) + ["--mode", "rpc"]
        record = run_mio_pty(
            ctx["mio_binary"],
            args,
            env,
            cwd,
            timeout,
            close_stdin=True,
            max_capture=ctx["limits"]["max_capture_bytes"],
        )
    raw = record["stderr"]
    probe = run_probe(ctx, "rpc_no_osc", raw, 0)
    checks = [
        check("raw_has_no_osc_substring", not raw_has_osc(raw), f"{len(raw)} bytes"),
        check("probe_events_zero", probe["probe_returncode"] == 0 and not probe["events"], probe["events"]),
        check("mio_exit_zero", record["returncode"] == 0, f"rc={record['returncode']}"),
        check("not_timed_out", not record["timed_out"], f"timed_out={record['timed_out']}"),
    ]
    scenario = {
        "id": "rpc_no_osc",
        "title": "--mode rpc emits no OSC 7501",
        "required": True,
        "status": scenario_status(checks),
        "checks": checks,
        "mio": _mio_summary(record),
        "raw_sha256": probe["raw_sha256"],
        "raw_bytes": probe["raw_bytes"],
        "events": probe["events"],
        "probe": _probe_summary(probe),
    }
    return scenario, raw


def _mio_summary(record: dict) -> dict:
    return {
        "args": record["args"],
        "returncode": record["returncode"],
        "stderr_bytes": record["stderr_total_bytes"],
        "stdout_bytes": record["stdout_total_bytes"],
        "stderr_truncated": record["stderr_truncated"],
        "stdout_truncated": record["stdout_truncated"],
        "timed_out": record["timed_out"],
        "duration_s": record["duration_s"],
        "mock_mode": record.get("mock_mode"),
    }


def _probe_summary(probe: dict) -> dict:
    return {
        "returncode": probe["probe_returncode"],
        "timed_out": probe["probe_timed_out"],
        "duration_s": probe["probe_duration_s"],
        "summary": probe["summary"],
        "stderr_tail": probe["probe_stderr"],
    }


# --------------------------------------------------------------------------
# Report assembly
# --------------------------------------------------------------------------


def limits_block(args) -> dict:
    return {
        "scenario_timeout_s": args.timeout,
        "probe_timeout_s": args.probe_timeout,
        "build_timeout_s": args.build_timeout,
        "max_capture_bytes": args.max_capture_bytes,
        "max_probe_input_bytes": args.max_probe_input_bytes,
        "chunk_sizes": list(args.chunk_sizes),
        "probe_chunk_zero_means_single_feed": True,
        "host_max_osc_len_bytes": 16 * 1024,
        "notes": [
            "Mio stdout/stderr reading is capped; excess bytes are counted and discarded.",
            "The probe itself rejects inputs above max_probe_input_bytes.",
            "Each scenario has a hard timeout and its process group is killed on expiry.",
        ],
    }


def collect_toolchain(toolchain: str) -> dict:
    info = {"requested": toolchain, "cargo_version": None, "rust_version": None, "errors": []}
    cargo = run_process(["cargo", f"+{toolchain}", "--version"], env=build_env_for_cargo(), timeout=60)
    if cargo["returncode"] == 0:
        info["cargo_version"] = cargo["stdout"].decode("utf-8", errors="replace").strip()
    else:
        info["errors"].append(f"cargo +{toolchain} --version failed")
    rustc = run_process(["rustup", "run", toolchain, "rustc", "--version"], timeout=60)
    if rustc["returncode"] == 0:
        info["rust_version"] = rustc["stdout"].decode("utf-8", errors="replace").strip()
    else:
        info["errors"].append(f"rustup run {toolchain} rustc --version failed")
    return info


def build_base_report(args, ctx, *, mode: str, status: str) -> dict:
    return {
        "schema": SCHEMA_ID,
        "status": status,
        "mode": mode,
        "generated_at": utc_now(),
        "command": {
            "ravomi_root": str(ctx["ravomi_root"]),
            "mio_root": str(ctx["mio_root"]),
            "mio_binary": str(ctx["mio_binary"]) if ctx.get("mio_binary") else None,
            "report": str(args.report) if args.report else None,
            "toolchain": args.toolchain,
            "prepare_only": bool(args.prepare_only),
            "probe_binary_override": str(args.probe_binary) if args.probe_binary else None,
        },
        "scope": scope_block(),
        "limitations": limitations(),
        "env_policy": env_policy(),
        "limits": limits_block(args),
        "scenario_manifest": build_scenario_manifest(),
        "blocking": [],
        "validation": [],
    }


def build_plan_report(args, ctx) -> dict:
    host_source = ctx["host_source"]
    report = build_base_report(args, ctx, mode="prepare_only", status="not_run")
    report["artifacts"] = {
        "host_source": describe_file(host_source),
        "mio_binary": describe_file(ctx.get("mio_binary")),
        "cargo_lock": describe_file(ctx["mio_root"] / "Cargo.lock"),
        "probe_binary": describe_file(args.probe_binary) if args.probe_binary else {"path": None, "present": False},
    }
    report["toolchain"] = collect_toolchain(args.toolchain)
    report["planned_build_command"] = [
        "cargo",
        f"+{args.toolchain}",
        "build",
        "--locked",
        "--lib",
        "--bin",
        "mio",
        "--message-format=json",
    ]
    report["planned_rustc_command"] = (
        f"rustup run {args.toolchain} rustc --edition=2024 --crate-name mio_ravomi_host_probe "
        "--crate-type bin -C opt-level=0 -L dependency=<cargo deps dir> "
        "--extern base64=<rlib> --extern serde=<rlib> --extern serde_json=<rlib> "
        "--extern tempfile=<rlib> <probe.rs> -o <probe>"
    )
    report["planned_scenarios"] = [item["id"] for item in build_scenario_manifest()]

    if not report["artifacts"]["host_source"]["present"]:
        report["status"] = "fail"
        report["blocking"].append(
            "host source missing: expected " + str(host_source) + " (missing source is never a skip-as-pass)"
        )
    if args.mio_binary is not None and not report["artifacts"]["mio_binary"]["present"]:
        report["blocking"].append(f"mio binary missing: {args.mio_binary}")
    report["validation"] = validate_report(report)
    return report


def probe_evidence(reused: bool, host_source: Path, host_sha: str, rlibs) -> dict:
    """Record whether this run actually compiled the real host source.

    A reused `--probe-binary` proves only that the supplied probe consumed the
    captured bytes. It must never be conflated with the default evidence, where
    the probe is compiled in this run from the real host source via `#[path]`.
    """
    if reused:
        boundary = (
            "The probe was supplied with --probe-binary and was NOT compiled in this run, so this "
            "report cannot prove that the current host source (sha256 recorded here) was compiled "
            "or linked. It only shows that the supplied probe binary consumed the captured bytes. "
            "Do not treat a reused-probe pass as equivalent to the default in-run compilation "
            "evidence."
        )
    else:
        boundary = (
            "The probe was compiled in this run from the real host source path above via #[path]; "
            "the cargo/rustc commands, probe source sha256 and linked rlibs are recorded in "
            "probe_build and host_direct_dependencies."
        )
    return {
        "host_source_compiled_this_run": not reused,
        "probe_origin": "reused_prebuilt_probe" if reused else "compiled_in_this_run",
        "host_source_path": str(host_source),
        "host_source_sha256": host_sha,
        "rlib_count": len(rlibs),
        "boundary": boundary,
    }


def execute_run(args, ctx) -> dict:
    osc = load_osc_module(ctx["mio_root"])
    limits = ctx["limits"]
    probe_dir = Path(tempfile.mkdtemp(prefix="mio-ravomi-host-probe-"))
    ctx["probe_dir"] = probe_dir
    keep = args.keep_temp

    host_before = sha256_file(ctx["host_source"])
    mio_before = sha256_file(ctx["mio_binary"])
    report = build_base_report(args, ctx, mode="run", status="fail")
    report["toolchain"] = collect_toolchain(args.toolchain)
    build_info: dict = {}

    try:
        reused_probe = bool(args.probe_binary)
        if reused_probe:
            probe = Path(args.probe_binary)
            if not probe.is_file():
                raise SetupError(f"--probe-binary not found: {probe}")
            rlibs = []
            build_info = {"reused_probe_binary": True, "command": None}
        else:
            probe, rlibs, build_info = build_probe(ctx)
        ctx["probe_binary"] = probe

        records = []
        captured: dict[str, bytes] = {}

        def run_scenario(func, *func_args):
            name = func.__name__.replace("scenario_", "")
            try:
                scenario, raw = func(ctx, osc, *func_args)
            except SetupError:
                raise
            except Exception as error:  # noqa: BLE001 - record, never crash the run
                scenario = {
                    "id": name,
                    "title": name,
                    "required": True,
                    "status": "fail",
                    "checks": [check("scenario_raised", False, repr(error))],
                    "events": [],
                }
                raw = b""
            records.append(scenario)
            captured[scenario["id"]] = raw
            return scenario

        run_scenario(scenario_success)
        run_scenario(scenario_error)
        run_scenario(scenario_approval)
        run_scenario(scenario_fragment, captured.get("success", b""))
        run_scenario(scenario_sigint)
        run_scenario(scenario_json_no_osc)
        run_scenario(scenario_rpc_no_osc)

        host_after = sha256_file(ctx["host_source"])
        mio_after = sha256_file(ctx["mio_binary"])

        counts = _counts(records)
        source_stable = snapshot_stable(host_before, host_after)
        binary_stable = snapshot_stable(mio_before, mio_after)
        if not source_stable:
            report["blocking"].append("host source changed during the run; snapshot is not a single revision")
        if not binary_stable:
            report["blocking"].append("mio binary changed during the run")

        report["status"] = "pass" if counts["required_passed"] and source_stable and binary_stable else "fail"
        report["artifacts"] = {
            "host_source": {
                "path": str(ctx["host_source"]),
                "present": True,
                "sha256_before": host_before,
                "sha256_after": host_after,
                "sha256": host_before,
                "stable": source_stable,
            },
            "mio_binary": {
                "path": str(ctx["mio_binary"]),
                "present": True,
                "size": ctx["mio_binary"].stat().st_size,
                "sha256_before": mio_before,
                "sha256_after": mio_after,
                "sha256": mio_before,
                "stable": binary_stable,
            },
            "cargo_lock": describe_file(ctx["mio_root"] / "Cargo.lock"),
            "probe_binary": {
                "path": str(probe),
                "present": probe.is_file(),
                "sha256": sha256_file(probe),
                "reused": bool(args.probe_binary),
            },
            "host_direct_dependencies": rlibs,
        }
        report["probe_build"] = build_info
        report["probe_evidence"] = probe_evidence(reused_probe, ctx["host_source"], host_before, rlibs)
        if reused_probe:
            report["limitations"].append(
                "The probe was reused via --probe-binary and was not compiled in this run; the "
                "report cannot prove the current host source was compiled."
            )
            report["scope"]["not_claimed"].append(
                "host source compiled in this run (a reused --probe-binary is not compile evidence)"
            )
        report["scenarios"] = records
        report["counts"] = counts
    finally:
        if not keep and probe_dir.exists():
            shutil.rmtree(probe_dir, ignore_errors=True)

    report["validation"] = validate_report(report)
    if report["status"] == "pass" and report["validation"]:
        report["status"] = "fail"
        report["blocking"].append("validation failed for a pass report; downgraded to fail")
    return report


def _counts(records) -> dict:
    passed = sum(1 for record in records if record["status"] == "pass")
    minimum_passed = all(
        any(record["id"] == scenario and record["status"] == "pass" for record in records)
        for scenario in MINIMUM_SCENARIOS
    )
    return {
        "total": len(records),
        "passed": passed,
        "failed": len(records) - passed,
        "minimum_required": list(MINIMUM_SCENARIOS),
        "minimum_passed": minimum_passed,
        "required_passed": passed == len(records) and len(records) >= len(ALL_SCENARIOS),
        "checked": list(ALL_SCENARIOS),
    }


# --------------------------------------------------------------------------
# Entrypoint
# --------------------------------------------------------------------------


def write_report(report: dict, report_path: Path | None) -> None:
    text = json.dumps(report, indent=2, sort_keys=True)
    if report_path is None:
        print(text)
        return
    report_path.parent.mkdir(parents=True, exist_ok=True)
    tmp_path = report_path.with_name(report_path.name + ".tmp")
    tmp_path.write_text(text + "\n", encoding="utf-8")
    os.replace(tmp_path, report_path)


def print_summary(report: dict) -> None:
    counts = report.get("counts") or {}
    scenario_bits = ", ".join(
        f"{record['id']}={record['status']}" for record in report.get("scenarios", [])
    )
    print(
        "[ravomi-host] status={status} scenarios={passed}/{total} {bits}".format(
            status=report["status"],
            passed=counts.get("passed", 0),
            total=counts.get("total", 0),
            bits=scenario_bits,
        ),
        file=sys.stderr,
    )
    for reason in report.get("blocking", []):
        print(f"[ravomi-host] blocking: {reason}", file=sys.stderr)


def guard_report_path(report_path: Path | None, ravomi_root: Path, host_source: Path) -> None:
    if report_path is None:
        return
    resolved = Path(report_path).resolve()
    try:
        resolved.relative_to(ravomi_root.resolve())
    except ValueError:
        pass
    else:
        raise SetupError(f"refusing to write a report inside the Ravomi checkout: {resolved}")
    if resolved == host_source.resolve():
        raise SetupError("report path must not overwrite the host source")


def main(argv=None) -> int:
    args = parse_args(argv)
    mio_root = Path(__file__).resolve().parent.parent
    ravomi_root = Path(args.ravomi_root).resolve()
    host_source = resolve_host_source(ravomi_root)
    mio_binary = Path(args.mio_binary).resolve() if args.mio_binary else None

    ctx = {
        "mio_root": mio_root,
        "ravomi_root": ravomi_root,
        "host_source": host_source,
        "mio_binary": mio_binary,
        "toolchain": args.toolchain,
        "probe_binary": Path(args.probe_binary).resolve() if args.probe_binary else None,
        "probe_dir": None,
        "build_timeout": args.build_timeout,
        "limits": limits_block(args),
    }

    try:
        guard_report_path(args.report, ravomi_root, host_source)
        if args.prepare_only:
            report = build_plan_report(args, ctx)
            write_report(report, args.report)
            print_summary(report)
            return EXIT_NOT_RUN if report["status"] != "fail" else EXIT_FAIL
        if not host_source.is_file():
            raise SetupError(f"host source missing: {host_source}")
        if not mio_binary.is_file():
            raise SetupError(f"mio binary missing: {mio_binary}")
        report = execute_run(args, ctx)
    except SetupError as error:
        report = build_base_report(args, ctx, mode="run", status="fail")
        report["artifacts"] = {
            "host_source": describe_file(host_source),
            "mio_binary": describe_file(mio_binary),
            "cargo_lock": describe_file(mio_root / "Cargo.lock"),
            "probe_binary": {"path": None, "present": False},
        }
        report["toolchain"] = collect_toolchain(args.toolchain)
        report["blocking"] = [str(error)]
        report["validation"] = validate_report(report)
        write_report(report, args.report)
        print_summary(report)
        return EXIT_FAIL

    write_report(report, args.report)
    print_summary(report)
    return EXIT_PASS if report["status"] == "pass" else EXIT_FAIL


if __name__ == "__main__":
    raise SystemExit(main())
