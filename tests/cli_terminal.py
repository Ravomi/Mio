#!/usr/bin/env python3
"""离线 PTY 回归：终端交互保持简洁，且非交互入口不受影响。

覆盖点：
  1. 交互模式只输出 `mio> ` 提示符与流式文本：不进入 alternate screen、
     不输出 256/truecolor 彩色补全面板、启动时不主动清屏、不擦除滚动历史。
  2. Tab 补全与上下键历史仍可用；`/exit` 与 Ctrl-D 正常退出且退出码为 0。
  3. `/clear` 只有显式触发时才会发送清屏序列。
  4. `-p` / `--json` / `--mode rpc` 三个非交互入口行为不变（无 ANSI、无提示符）。

全部使用本地 mock：不访问真实 Provider、不依赖网络。
运行：python3 tests/cli_terminal.py [path/to/mio]
"""
import json
import os
import pty
import select
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_MIO_BIN = ROOT / "target" / "debug" / "mio"

# 花哨 UI 与备用屏特征序列。OSC 7501 状态上报是机器可读协议，不在禁用范围。
ALTERNATE_SCREEN = (
    b"\x1b[?1049h",
    b"\x1b[?1049l",
    b"\x1b[?1047h",
    b"\x1b[?1047l",
    b"\x1b[?1048h",
    b"\x1b[?47h",
    b"\x1b[?47l",
)
FANCY_COLOR = (
    b"\x1b[38;2;",
    b"\x1b[48;2;",
    b"\x1b[38;5;",
    b"\x1b[48;5;",
)
# 旧补全面板的选中标记与页脚。
FANCY_MARKERS = (b"\xe2\x96\xb8", b"\xe2\x80\xa6 (")
# 旧装饰性启动横幅片段。
FANCY_BANNER = (b"/help for commands", b"Tab completes", b"Mio \xc2\xb7")
SCROLLBACK_ERASE = (b"\x1b[3J",)


def assert_no_fancy(data: bytes, label: str):
    text = data.decode("utf-8", errors="replace")
    for seq in ALTERNATE_SCREEN:
        assert seq not in data, f"{label}: alternate screen {seq!r} in {text!r}"
    for seq in FANCY_COLOR:
        assert seq not in data, f"{label}: 彩色补全序列 {seq!r} in {text!r}"
    for seq in FANCY_MARKERS:
        assert seq not in data, f"{label}: 花哨面板标记 {seq!r} in {text!r}"
    for seq in SCROLLBACK_ERASE:
        assert seq not in data, f"{label}: 滚动历史被擦除 {seq!r} in {text!r}"


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
    env["MIO_CONFIG_DIR"] = str(Path(home_dir) / ".config" / "mio")
    env["MIO_API_KEY"] = "local-test-key"
    if extra_env:
        env.update(extra_env)
    return env


def make_sse_chunk(delta, finish_reason=None):
    return json.dumps(
        {
            "id": "test",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "test",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
        }
    )


def make_sse_response(delta, finish_reason):
    body = (
        f"data: {make_sse_chunk(delta, None)}\n\n"
        f"data: {make_sse_chunk({}, finish_reason)}\n\n"
        "data: [DONE]\n\n"
    )
    return (
        "HTTP/1.1 200 OK\r\n"
        "Content-Type: text/event-stream\r\n"
        f"Content-Length: {len(body)}\r\n"
        "Connection: close\r\n\r\n"
        f"{body}"
    ).encode("utf-8")


def read_http_request(sock: socket.socket, timeout=5.0):
    sock.settimeout(timeout)
    data = b""
    while b"\r\n\r\n" not in data:
        chunk = sock.recv(4096)
        if not chunk:
            break
        data += chunk
    return data


class MockServer:
    """单请求 loopback OpenAI 兼容 SSE 服务。"""

    def __init__(self):
        self.listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(4)
        self.port = self.listener.getsockname()[1]
        self.conns = []

    def base_url(self):
        return f"http://127.0.0.1:{self.port}/v1"

    def serve_one(self, content, timeout=10.0):
        deadline = time.time() + timeout
        while time.time() < deadline:
            ready, _, _ = select.select([self.listener], [], [], 0.05)
            if self.listener not in ready:
                continue
            conn, _ = self.listener.accept()
            self.conns.append(conn)
            read_http_request(conn)
            conn.sendall(make_sse_response({"role": "assistant", "content": content}, "stop"))
            conn.close()
            return
        raise TimeoutError("mock provider never received a request")

    def close(self):
        for sock in [self.listener, *self.conns]:
            try:
                sock.close()
            except Exception:
                pass
        self.conns.clear()


class Pty:
    """累积式 PTY 读取器，便于按出现次数断言。"""

    def __init__(self, master_fd):
        self.fd = master_fd
        self.buf = bytearray()
        self.eof = False

    def drain(self, timeout=0.2):
        ready, _, _ = select.select([self.fd], [], [], timeout)
        if self.fd not in ready:
            return False
        try:
            chunk = os.read(self.fd, 65536)
        except OSError:
            self.eof = True
            return False
        if not chunk:
            self.eof = True
            return False
        self.buf += chunk
        return True

    def expect(self, needle, timeout=5.0, count=1):
        deadline = time.monotonic() + timeout
        while self.buf.count(needle) < count:
            if time.monotonic() > deadline:
                raise AssertionError(
                    f"missing {needle!r} (x{count}) in {bytes(self.buf)!r}"
                )
            self.drain(0.05)

    def settle(self, idle=0.15, timeout=1.5):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if not self.drain(idle):
                return

    def write(self, data):
        os.write(self.fd, data)


def start_interactive(bin_path, home_dir, run_cwd):
    master_fd, slave_fd = pty.openpty()
    env = make_clean_env(home_dir.name, {"TERM": "xterm-256color"})
    args = [
        str(bin_path),
        "--no-context",
        "--no-mcp",
        "--no-session",
        "--provider",
        "custom",
        "--model",
        "test",
        # 交互用例不会真正发起请求；仅需通过本地端点校验。
        "--base-url",
        "http://127.0.0.1:9/v1",
    ]
    proc = subprocess.Popen(
        args,
        stdin=slave_fd,
        stdout=slave_fd,
        stderr=slave_fd,
        close_fds=True,
        env=env,
        cwd=run_cwd.name,
    )
    os.close(slave_fd)
    return proc, Pty(master_fd), master_fd


def stop(proc, master_fd):
    if proc.poll() is None:
        try:
            proc.terminate()
            proc.wait(timeout=1.0)
        except Exception:
            try:
                proc.kill()
                proc.wait(timeout=1.0)
            except Exception:
                pass
    try:
        os.close(master_fd)
    except Exception:
        pass


def case_interactive_simple(bin_path):
    """无花哨 UI、上下键历史、显式 /clear、/exit 正常退出。"""
    home_dir = tempfile.TemporaryDirectory()
    run_cwd = tempfile.TemporaryDirectory()
    proc = master_fd = None
    try:
        proc, pty_io, master_fd = start_interactive(bin_path, home_dir, run_cwd)
        pty_io.expect(b"mio> ")
        startup = bytes(pty_io.buf)
        assert_no_fancy(startup, "startup")
        for marker in FANCY_BANNER:
            assert marker not in startup, f"startup banner leaked: {startup!r}"
        assert b"\x1b[2J" not in startup, f"startup cleared the screen: {startup!r}"

        # /status 可运行；Up 键历史应该能再次提交同一条命令。
        pty_io.write(b"/status\n")
        pty_io.expect(b"Messages:")
        pty_io.settle()
        before_clear = bytes(pty_io.buf)
        assert b"\x1b[2J" not in before_clear, f"unexpected clear before /clear: {before_clear!r}"

        pty_io.write(b"\x1b[A")
        pty_io.settle(0.2)
        pty_io.write(b"\n")
        pty_io.expect(b"Messages:", count=2)

        # /clear 只有显式调用才清屏（只清当前视口，不动滚动历史）。
        pty_io.write(b"/clear\n")
        pty_io.expect(b"\x1b[2J")
        pty_io.settle()
        assert_no_fancy(bytes(pty_io.buf), "interactive")

        pty_io.write(b"/exit\n")
        assert proc.wait(timeout=5.0) == 0, "interactive /exit must exit 0"
        pty_io.settle()
        assert_no_fancy(bytes(pty_io.buf), "after-exit")
    finally:
        if proc is not None:
            stop(proc, master_fd)
        home_dir.cleanup()
        run_cwd.cleanup()


def case_tab_completion_and_ctrl_d(bin_path):
    """Tab 补全可用，Ctrl-D 在空提示符退出，且全程无备用屏。"""
    home_dir = tempfile.TemporaryDirectory()
    run_cwd = tempfile.TemporaryDirectory()
    proc = master_fd = None
    try:
        proc, pty_io, master_fd = start_interactive(bin_path, home_dir, run_cwd)
        pty_io.expect(b"mio> ")
        pty_io.write(b"/he\t")
        pty_io.expect(b"/help")
        assert_no_fancy(bytes(pty_io.buf), "tab")
        pty_io.write(b"\n")
        pty_io.expect(b"Usage: mio")
        pty_io.settle()
        pty_io.write(b"\x04")  # Ctrl-D 于空提示符
        assert proc.wait(timeout=5.0) == 0, "Ctrl-D must exit 0"
        pty_io.settle()
        assert_no_fancy(bytes(pty_io.buf), "ctrl-d")
    finally:
        if proc is not None:
            stop(proc, master_fd)
        home_dir.cleanup()
        run_cwd.cleanup()


def oneshot(bin_path, extra, content, home_dir, run_cwd, stdin=None):
    """运行一个非交互 mio 进程；`content` 为 None 时不启动 mock provider。"""
    server = MockServer()
    try:
        env = make_clean_env(home_dir.name, {"TERM": "dumb"})
        args = [
            str(bin_path),
            "--provider",
            "custom",
            "--model",
            "test",
            "--base-url",
            server.base_url(),
            "--no-context",
            "--no-mcp",
            "--no-session",
            *extra,
        ]
        if content is not None:
            worker = threading.Thread(
                target=server.serve_one, args=(content,), daemon=True
            )
            worker.start()
        return subprocess.run(
            args,
            input=stdin,
            capture_output=True,
            timeout=30,
            env=env,
            cwd=run_cwd.name,
        )
    finally:
        server.close()


def case_noninteractive_modes(bin_path):
    """-p / --json / --mode rpc 不回归：无 ANSI、无提示符、协议稳定。"""
    home_dir = tempfile.TemporaryDirectory()
    run_cwd = tempfile.TemporaryDirectory()
    try:
        answer = "terminal-mode-answer"

        # 1. 文本 -p：纯文本输出，无提示符、无 ANSI。
        text_run = oneshot(bin_path, ["-p", "hello"], answer, home_dir, run_cwd)
        assert text_run.returncode == 0, text_run.stderr.decode("utf-8", "replace")
        assert answer.encode() in text_run.stdout, text_run.stdout
        assert b"mio> " not in text_run.stdout, text_run.stdout
        assert b"\x1b" not in text_run.stdout, f"ANSI in text mode: {text_run.stdout!r}"

        # 2. --json：每行一个 JSON 事件。
        json_run = oneshot(bin_path, ["--json", "-p", "hello"], answer, home_dir, run_cwd)
        assert json_run.returncode == 0, json_run.stderr.decode("utf-8", "replace")
        events = [
            json.loads(line)
            for line in json_run.stdout.decode("utf-8").splitlines()
            if line.strip()
        ]
        assert any(e.get("type") == "text" for e in events), events
        assert any(e.get("type") == "done" for e in events), events
        assert b"mio> " not in json_run.stdout

        # 3. --mode rpc：get_state 请求-应答保持 JSONL 协议（无需模型调用，故不起 mock）。
        rpc_stdin = b'{"type":"get_state","id":"s1"}\n'
        rpc_run = oneshot(
            bin_path, ["--mode", "rpc"], None, home_dir, run_cwd, stdin=rpc_stdin
        )
        assert rpc_run.returncode == 0, rpc_run.stderr.decode("utf-8", "replace")
        responses = [
            json.loads(line)
            for line in rpc_run.stdout.decode("utf-8").splitlines()
            if line.strip()
        ]
        state = [
            r
            for r in responses
            if r.get("type") == "response"
            and r.get("command") == "get_state"
            and r.get("id") == "s1"
        ]
        assert state and state[0].get("success") is True, responses
        assert "sessionId" in state[0].get("data", {}), state
        assert b"mio> " not in rpc_run.stdout
    finally:
        home_dir.cleanup()
        run_cwd.cleanup()


def main():
    bin_path = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else DEFAULT_MIO_BIN.resolve()
    assert bin_path.exists(), f"mio binary not found: {bin_path}"
    cases = {
        "interactive_simple": case_interactive_simple,
        "tab_completion_and_ctrl_d": case_tab_completion_and_ctrl_d,
        "noninteractive_modes": case_noninteractive_modes,
    }
    selected = sys.argv[2:] or list(cases)
    for name in selected:
        assert name in cases, f"unknown case {name}; expected {sorted(cases)}"
        print(f"Running case: {name} against {bin_path.name} ...", end=" ", flush=True)
        try:
            cases[name](bin_path)
        except Exception as error:  # noqa: BLE001 - 测试脚本需要打印后重抛
            print("FAILED")
            raise AssertionError(f"{name}: {error}") from error
        print("PASSED")
    print(f"PASS: {len(selected)} CLI terminal cases")


if __name__ == "__main__":
    main()
