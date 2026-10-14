#!/usr/bin/env python3
import errno
import json
import os
import pty
import select
import signal
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MIO_BIN = ROOT / "target" / "debug" / "mio"

def make_clean_env(home_dir: str, extra_env=None):
    env = {}
    for k, v in os.environ.items():
        if k.startswith("MIO_"):
            continue
        if k.endswith("_API_KEY") or "_API_KEY_" in k:
            continue
        if k.lower() in ("http_proxy", "https_proxy", "all_proxy", "no_proxy", "ftp_proxy"):
            continue
        env[k] = v

    env["HOME"] = home_dir
    env["MIO_API_KEY"] = "local-test-credential"
    if extra_env:
        env.update(extra_env)
    return env

class MockOpenAIHandler(BaseHTTPRequestHandler):
    def log_message(self, format, *args):
        # 静默 http log
        pass

    def do_POST(self):
        content_length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(content_length).decode("utf-8", errors="replace") if content_length > 0 else ""
        data = {}
        try:
            data = json.loads(body)
        except Exception:
            pass

        # 检查是否要求模拟不同响应
        mode = getattr(self.server, "mock_mode", "success")
        req_event = getattr(self.server, "request_event", None)
        if req_event is not None:
            req_event.set()

        if mode == "http_500":
            self.send_response(500)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"error": "mock internal server error"}')
            return

        if mode == "hang":
            # 持续挂起，通过 release_event 唤醒回收，避免 sleep 10
            release_event = getattr(self.server, "release_event", None)
            if release_event is not None:
                release_event.wait(timeout=10)
            else:
                time.sleep(1)
            return

        if mode == "approve_workspace":
            # 第一次调用：返回 workspace write 工具调用
            # 第二次调用：返回最终文本完成
            call_count = getattr(self.server, "call_count", 0)
            self.server.call_count = call_count + 1
            if call_count == 0:
                tool_args = json.dumps({"operation": "write", "path": "approved.txt", "value": "approved content"})
                chunk1 = json.dumps({
                    "id": "test",
                    "created": 0,
                    "model": "test",
                    "object": "chat.completion.chunk",
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "role": "assistant",
                            "tool_calls": [{
                                "index": 0,
                                "id": "call_write_1",
                                "type": "function",
                                "function": {"name": "workspace", "arguments": tool_args}
                            }]
                        },
                        "finish_reason": None
                    }]
                })
                chunk2 = json.dumps({
                    "id": "test",
                    "created": 0,
                    "model": "test",
                    "object": "chat.completion.chunk",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
                })
                sse_body = f"data: {chunk1}\n\ndata: {chunk2}\n\ndata: [DONE]\n\n"
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(sse_body.encode("utf-8"))
                return
            else:
                chunk1 = json.dumps({
                    "id": "test",
                    "created": 0,
                    "model": "test",
                    "object": "chat.completion.chunk",
                    "choices": [{
                        "index": 0,
                        "delta": {"role": "assistant", "content": "Approved write completed."},
                        "finish_reason": None
                    }]
                })
                chunk2 = json.dumps({
                    "id": "test",
                    "created": 0,
                    "model": "test",
                    "object": "chat.completion.chunk",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
                })
                sse_body = f"data: {chunk1}\n\ndata: {chunk2}\n\ndata: [DONE]\n\n"
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(sse_body.encode("utf-8"))
                return

        # 默认模式：简单成功返回
        chunk1 = json.dumps({
            "id": "test",
            "created": 0,
            "model": "test",
            "object": "chat.completion.chunk",
            "choices": [{
                "index": 0,
                "delta": {"role": "assistant", "content": "Task completed successfully."},
                "finish_reason": None
            }]
        })
        chunk2 = json.dumps({
            "id": "test",
            "created": 0,
            "model": "test",
            "object": "chat.completion.chunk",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
        })
        sse_body = f"data: {chunk1}\n\ndata: {chunk2}\n\ndata: [DONE]\n\n"
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(sse_body.encode("utf-8"))

def start_mock_server(mode="success"):
    server = HTTPServer(("127.0.0.1", 0), MockOpenAIHandler)
    server.mock_mode = mode
    server.call_count = 0
    server.request_event = threading.Event()
    server.release_event = threading.Event()
    port = server.server_address[1]
    t = threading.Thread(target=server.serve_forever, daemon=True)
    t.start()
    return server, port

def parse_osc_events(raw_bytes):
    # OSC 7501 格式形如: \x1b]7501;state=working:app=mio\x1b\ 或带 :msg= / :kind=
    events = []
    text = raw_bytes.decode("utf-8", errors="replace")
    start = 0
    while True:
        idx = text.find("\x1b]7501;", start)
        if idx == -1:
            break
        end = text.find("\x1b\\", idx)
        if end == -1:
            end = text.find("\x07", idx)
            if end == -1:
                break
            end_step = 1
        else:
            end_step = 2
        payload = text[idx + len("\x1b]7501;"):end]
        parts = payload.split(":")
        data = {}
        for p in parts:
            if "=" in p:
                k, v = p.split("=", 1)
                data[k] = v
        events.append(data)
        start = end + end_step
    return events

def run_test_pty(args, env_extra=None, input_bytes=b"", timeout=10, trigger_event=None, sigint_on_trigger=False, cwd=None):
    master_fd, slave_fd = pty.openpty()
    with tempfile.TemporaryDirectory() as home_dir, tempfile.TemporaryDirectory() as run_cwd:
        actual_cwd = cwd or run_cwd
        env = make_clean_env(home_dir, env_extra)

        proc = subprocess.Popen(
            [str(MIO_BIN)] + args,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=slave_fd,
            close_fds=True,
            env=env,
            cwd=actual_cwd,
            start_new_session=True,
        )
        os.close(slave_fd)

        stderr_chunks = []
        start_time = time.time()
        sigint_sent = False

        if input_bytes and not sigint_on_trigger:
            try:
                proc.stdin.write(input_bytes)
                proc.stdin.flush()
            except (BrokenPipeError, OSError):
                pass

        try:
            while True:
                now = time.time()
                if now - start_time > timeout:
                    raise TimeoutError("Command timed out")

                if sigint_on_trigger and trigger_event and not sigint_sent:
                    if trigger_event.is_set():
                        sigint_sent = True
                        try:
                            os.killpg(os.getpgid(proc.pid), signal.SIGINT)
                        except OSError:
                            pass

                r, _, _ = select.select([master_fd], [], [], 0.05)
                if master_fd in r:
                    try:
                        data = os.read(master_fd, 4096)
                        if data:
                            stderr_chunks.append(data)
                        else:
                            break
                    except OSError as e:
                        if e.errno == errno.EIO:
                            break
                        raise

                if proc.poll() is not None:
                    # 读完残留
                    while True:
                        r, _, _ = select.select([master_fd], [], [], 0.02)
                        if master_fd in r:
                            try:
                                data = os.read(master_fd, 4096)
                                if not data:
                                    break
                                stderr_chunks.append(data)
                            except OSError as e:
                                if e.errno == errno.EIO:
                                    break
                                raise
                        else:
                            break
                    break
        finally:
            os.close(master_fd)
            # 先等待正常退出，避免见EOF直接kill导致竞态
            try:
                proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
                except OSError:
                    pass
                proc.wait()

        # communicate 避免对已可能关闭的 stdin 报异常
        stdout_data = b""
        try:
            if proc.stdin and not proc.stdin.closed:
                stdout_data, _ = proc.communicate(timeout=2)
            else:
                stdout_data = proc.stdout.read() if proc.stdout else b""
        except Exception:
            pass

        stderr_data = b"".join(stderr_chunks)
        return proc.returncode, stdout_data, stderr_data

def get_base_cli_args(port, tmp_cwd):
    return [
        "--provider", "custom",
        "--model", "test",
        "--base-url", f"http://127.0.0.1:{port}/v1",
        "--no-context",
        "--no-mcp",
        "--no-session",
        "--cwd", str(tmp_cwd),
    ]

def test_mock_success():
    print("[RUN] test_mock_success (-p working->done)")
    server, port = start_mock_server("success")
    try:
        with tempfile.TemporaryDirectory() as test_cwd:
            cli_args = get_base_cli_args(port, test_cwd) + ["-p", "hello"]
            ret, stdout, stderr = run_test_pty(cli_args, cwd=test_cwd)
            events = parse_osc_events(stderr)
            assert ret == 0, f"Expected 0, got {ret}, stdout={stdout.decode()}, stderr={stderr.decode()}"
            states = [e.get("state") for e in events]
            assert "working" in states, f"Expected 'working' in states: {states}"
            assert states[-1] == "done", f"Expected last state 'done', got: {states}"
            print("[PASS] test_mock_success")
    finally:
        server.shutdown()

def test_http_error():
    print("[RUN] test_http_error (HTTP 500 -> error)")
    server, port = start_mock_server("http_500")
    try:
        with tempfile.TemporaryDirectory() as test_cwd:
            cli_args = get_base_cli_args(port, test_cwd) + ["-p", "hello"]
            ret, stdout, stderr = run_test_pty(cli_args, cwd=test_cwd)
            events = parse_osc_events(stderr)
            assert ret != 0, f"Expected failure, got returncode 0"
            states = [e.get("state") for e in events]
            assert "working" in states, f"Expected 'working' in states: {states}"
            assert states[-1] == "error", f"Expected last state 'error', got: {states}"
            print("[PASS] test_http_error")
    finally:
        server.shutdown()

def test_sigint_idle():
    print("[RUN] test_sigint_idle (SIGINT -> last idle not error)")
    server, port = start_mock_server("hang")
    try:
        with tempfile.TemporaryDirectory() as test_cwd:
            cli_args = get_base_cli_args(port, test_cwd) + ["-p", "hello"]
            ret, stdout, stderr = run_test_pty(
                cli_args,
                trigger_event=server.request_event,
                sigint_on_trigger=True,
                cwd=test_cwd,
            )
            events = parse_osc_events(stderr)
            assert ret != 0, f"Expected cancelled exit code, got 0"
            states = [e.get("state") for e in events]
            assert "working" in states, f"Expected 'working' in states: {states}"
            assert states[-1] == "idle", f"Expected last state 'idle' (not error), got: {states}"
            print("[PASS] test_sigint_idle")
    finally:
        server.release_event.set()
        server.shutdown()

def test_stderr_pipe_no_osc():
    print("[RUN] test_stderr_pipe_no_osc (stderr pipe has no OSC)")
    server, port = start_mock_server("success")
    try:
        with tempfile.TemporaryDirectory() as home_dir, tempfile.TemporaryDirectory() as test_cwd:
            env = make_clean_env(home_dir)
            cli_args = get_base_cli_args(port, test_cwd) + ["-p", "hello"]
            proc = subprocess.run(
                [str(MIO_BIN)] + cli_args,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                cwd=test_cwd,
                env=env,
                timeout=10,
            )
            assert b"\x1b]7501;" not in proc.stderr, "stderr pipe must not contain OSC sequences"
            print("[PASS] test_stderr_pipe_no_osc")
    finally:
        server.shutdown()

def test_json_mode_no_osc_and_json_error():
    print("[RUN] test_json_mode_no_osc_and_json_error (--json stderr PTY has no OSC and stdout has JSON error)")
    server, port = start_mock_server("http_500")
    try:
        with tempfile.TemporaryDirectory() as test_cwd:
            cli_args = ["--json"] + get_base_cli_args(port, test_cwd) + ["-p", "hello"]
            ret, stdout, stderr = run_test_pty(cli_args, cwd=test_cwd)
            assert ret != 0, "Expected failure"
            events = parse_osc_events(stderr)
            assert len(events) == 0, f"Expected no OSC events in --json mode, got: {events}"
            # 验证 stdout 有 error json
            stdout_str = stdout.decode("utf-8")
            parsed = None
            for line in stdout_str.splitlines():
                line = line.strip()
                if line.startswith("{"):
                    try:
                        data = json.loads(line)
                        if data.get("type") == "error":
                            parsed = data
                            break
                    except Exception:
                        pass
            assert parsed is not None, f"Expected JSON error on stdout, got: {stdout_str}"
            print("[PASS] test_json_mode_no_osc_and_json_error")
    finally:
        server.shutdown()

def test_rpc_mode_closed_stdin():
    print("[RUN] test_rpc_mode_closed_stdin (--mode rpc closing stdin produces no OSC)")
    master_fd, slave_fd = pty.openpty()
    with tempfile.TemporaryDirectory() as home_dir, tempfile.TemporaryDirectory() as test_cwd:
        env = make_clean_env(home_dir)
        proc = subprocess.Popen(
            [str(MIO_BIN)] + get_base_cli_args(9, test_cwd) + ["--mode", "rpc"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=slave_fd,
            cwd=test_cwd,
            close_fds=True,
            env=env,
            start_new_session=True,
        )
        os.close(slave_fd)
        # 立即关闭 stdin
        proc.stdin.close()
        stderr_chunks = []
        try:
            start_t = time.time()
            while time.time() - start_t < 3.0:
                r, _, _ = select.select([master_fd], [], [], 0.05)
                if master_fd in r:
                    try:
                        chunk = os.read(master_fd, 4096)
                        if chunk:
                            stderr_chunks.append(chunk)
                        else:
                            break
                    except OSError as e:
                        if e.errno == errno.EIO:
                            break
                        raise
                if proc.poll() is not None:
                    # 读尽残留
                    while True:
                        r, _, _ = select.select([master_fd], [], [], 0.02)
                        if master_fd in r:
                            try:
                                chunk = os.read(master_fd, 4096)
                                if not chunk:
                                    break
                                stderr_chunks.append(chunk)
                            except OSError as e:
                                if e.errno == errno.EIO:
                                    break
                                raise
                        else:
                            break
                    break
        finally:
            os.close(master_fd)
            # 等正常退出并读完，处理 PTY EIO，不用 kill 当成功
            try:
                proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
                except OSError:
                    pass
                proc.wait()

        stderr_data = b"".join(stderr_chunks)
        events = parse_osc_events(stderr_data)
        assert proc.returncode == 0, f"RPC exited with {proc.returncode}: {stderr_data!r}"
        assert len(events) == 0, f"Expected no OSC events in RPC mode, got: {events}"
        print("[PASS] test_rpc_mode_closed_stdin")

def test_approve_blocked_and_done():
    print("[RUN] test_approve_blocked_and_done (--approve mock workspace write -> blocked -> y -> done)")
    server, port = start_mock_server("approve_workspace")
    try:
        with tempfile.TemporaryDirectory() as home_dir, tempfile.TemporaryDirectory() as tmp_dir:
            master_in, slave_in = pty.openpty()
            master_err, slave_err = pty.openpty()
            env = make_clean_env(home_dir)
            cli_args = ["--approve"] + get_base_cli_args(port, tmp_dir) + ["-p", "write file"]

            proc = subprocess.Popen(
                [str(MIO_BIN)] + cli_args,
                stdin=slave_in,
                stdout=subprocess.PIPE,
                stderr=slave_err,
                close_fds=True,
                env=env,
                cwd=tmp_dir,
                start_new_session=True,
            )
            os.close(slave_in)
            os.close(slave_err)

            stderr_chunks = []
            answered = False
            start_time = time.time()
            try:
                while True:
                    now = time.time()
                    if now - start_time > 10:
                        raise TimeoutError("Approval test timed out")

                    r, _, _ = select.select([master_err], [], [], 0.05)
                    if master_err in r:
                        try:
                            data = os.read(master_err, 4096)
                            if data:
                                stderr_chunks.append(data)
                                current_events = parse_osc_events(b"".join(stderr_chunks))
                                if not answered and any(e.get("state") == "blocked" for e in current_events):
                                    os.write(master_in, b"y\n")
                                    answered = True
                            else:
                                break
                        except OSError as e:
                            if e.errno == errno.EIO:
                                break
                            raise

                    if proc.poll() is not None:
                        # 读完残留
                        while True:
                            r, _, _ = select.select([master_err], [], [], 0.02)
                            if master_err in r:
                                try:
                                    data = os.read(master_err, 4096)
                                    if not data:
                                        break
                                    stderr_chunks.append(data)
                                except OSError as e:
                                    if e.errno == errno.EIO:
                                        break
                                    raise
                            else:
                                break
                        break
            finally:
                os.close(master_in)
                os.close(master_err)
                try:
                    proc.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    try:
                        os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
                    except OSError:
                        pass
                    proc.wait()

            events = parse_osc_events(b"".join(stderr_chunks))
            assert proc.returncode == 0, f"Expected 0, got {proc.returncode}, stderr={b''.join(stderr_chunks).decode()}"
            states = [e.get("state") for e in events]
            assert "blocked" in states, f"Expected blocked state in {states}"
            assert states[-1] == "done", f"Expected last state 'done', got {states}"
            # 验证文件是否已写入
            written_file = Path(tmp_dir) / "approved.txt"
            assert written_file.is_file(), "Expected approved.txt to be created"
            print("[PASS] test_approve_blocked_and_done")
    finally:
        server.shutdown()

def main():
    if not MIO_BIN.is_file():
        print(f"Error: binary {MIO_BIN} not found. Run cargo build first.", file=sys.stderr)
        sys.exit(1)

    test_mock_success()
    test_http_error()
    test_sigint_idle()
    test_stderr_pipe_no_osc()
    test_json_mode_no_osc_and_json_error()
    test_rpc_mode_closed_stdin()
    test_approve_blocked_and_done()
    print("\nAll OSC 7501 tests passed successfully!")

if __name__ == "__main__":
    main()
