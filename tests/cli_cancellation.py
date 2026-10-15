#!/usr/bin/env python3
import json
import os
import pty
import select
import signal
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_MIO_BIN = ROOT / "target" / "debug" / "mio"

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
    chunk = {
        "id": "test",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "test",
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason,
        }]
    }
    return json.dumps(chunk)

def make_sse_response(delta, finish_reason):
    c1 = make_sse_chunk(delta, None)
    c2 = make_sse_chunk({}, finish_reason)
    body = f"data: {c1}\n\ndata: {c2}\n\ndata: [DONE]\n\n"
    res = (
        f"HTTP/1.1 200 OK\r\n"
        f"Content-Type: text/event-stream\r\n"
        f"Content-Length: {len(body)}\r\n"
        f"Connection: close\r\n\r\n{body}"
    )
    return res.encode("utf-8")

def read_http_request(sock: socket.socket, timeout=5.0):
    sock.settimeout(timeout)
    data = b""
    while b"\r\n\r\n" not in data:
        chunk = sock.recv(4096)
        if not chunk:
            break
        data += chunk
    if b"\r\n\r\n" not in data:
        return {}, b""
    header_bytes, rest = data.split(b"\r\n\r\n", 1)
    lines = header_bytes.decode("utf-8", errors="replace").split("\r\n")
    headers = {}
    for line in lines[1:]:
        if ":" in line:
            k, v = line.split(":", 1)
            headers[k.strip().lower()] = v.strip()
    content_len = int(headers.get("content-length", 0))
    body = rest
    while len(body) < content_len:
        chunk = sock.recv(content_len - len(body))
        if not chunk:
            break
        body += chunk
    parsed = {}
    try:
        parsed = json.loads(body.decode("utf-8"))
    except Exception:
        pass
    return parsed, body

class MockServer:
    def __init__(self):
        self.listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(16)
        self.port = self.listener.getsockname()[1]
        self.conns = []

    def get_connection(self, timeout=5.0):
        deadline = time.time() + timeout
        while time.time() < deadline:
            r, _, _ = select.select([self.listener], [], [], 0.05)
            if self.listener in r:
                conn, _ = self.listener.accept()
                self.conns.append(conn)
                return conn
        raise TimeoutError("Timed out waiting for HTTP connection")

    def close(self):
        try:
            self.listener.close()
        except Exception:
            pass
        for c in self.conns:
            try:
                c.close()
            except Exception:
                pass
        self.conns.clear()

def assert_socket_closed(sock: socket.socket, timeout=5.0):
    deadline = time.time() + timeout
    sock.setblocking(False)
    while time.time() < deadline:
        try:
            data = sock.recv(1024)
            if len(data) == 0:
                return True
        except (BlockingIOError, InterruptedError):
            pass
        except (ConnectionResetError, BrokenPipeError):
            return True
        time.sleep(0.02)
    raise AssertionError("Socket remained open after deadline")

def drain_master(master_fd, total_timeout=1.0, idle_timeout=0.05):
    out = b""
    deadline = time.time() + total_timeout
    while time.time() < deadline:
        r, _, _ = select.select([master_fd], [], [], idle_timeout)
        if master_fd in r:
            try:
                chunk = os.read(master_fd, 4096)
                if not chunk:
                    break
                out += chunk
            except OSError:
                break
        else:
            break
    return out

def wait_for_output(master_fd, expected, timeout=5.0):
    output = b""
    deadline = time.monotonic() + timeout
    while expected not in output:
        remaining = deadline - time.monotonic()
        assert remaining > 0, f"Missing {expected!r}: {output!r}"
        ready, _, _ = select.select([master_fd], [], [], remaining)
        assert ready, f"Missing {expected!r}: {output!r}"
        output += os.read(master_fd, 4096)
    return output

def run_test(bin_path, case: str):
    server = MockServer()
    master_fd, slave_fd = pty.openpty()
    proc = None
    home_dir = tempfile.TemporaryDirectory()
    run_cwd = tempfile.TemporaryDirectory()
    try:
        base_url = f"http://127.0.0.1:{server.port}/v1"
        env = make_clean_env(home_dir.name, {
            "TERM": "dumb",
        })
        args = [
            str(bin_path),
            "--subagents",
            "--no-context",
            "--no-mcp",
            "--no-session",
            "--provider", "custom",
            "--model", "test",
            "--base-url", base_url,
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

        # 启动后稍等一下，让 mio 交互循环跑起来
        time.sleep(0.3)
        assert proc.poll() is None, "mio exited prematurely"

        # Invalid /thinking must leave the REPL usable; the valid setting must reach children.
        # Editing keys at an empty prompt must stay no-ops (DEL must not become text).
        os.write(master_fd, b"\x7f\x08\x0b\x15\x17/thinking invalid-level\n")
        wait_for_output(master_fd, b"Cannot set thinking level:")
        # Ctrl-U clears the line; Ctrl-W deletes the previous word.
        os.write(master_fd, b"discard\x15/thinking wrong\x17high\n")
        wait_for_output(master_fd, b"Thinking level set to high")
        # 发送第一个 prompt
        os.write(master_fd, b"delegate a task\n")

        # 收到第一个 parent HTTP 请求
        parent_conn1 = server.get_connection(timeout=5.0)
        first_req, _ = read_http_request(parent_conn1, timeout=5.0)
        assert first_req.get("reasoning_effort") == "high", first_req
        
        # 回复 parent：delegate child
        delegate_args = json.dumps({
            "operation": "delegate",
            "task": {
                "session": "child",
                "goal": "review",
                "prompt": "child-prompt",
                "read_only": True
            }
        })
        tool_call = {
            "name": "subagent",
            "arguments": delegate_args,
        }
        res_sse = make_sse_response(
            {"role": "assistant", "tool_calls": [{"index": 0, "id": "call-1", "type": "function", "function": tool_call}]},
            "tool_calls"
        )
        parent_conn1.sendall(res_sse)
        parent_conn1.close()

        # 现在有两个连接到达：child 请求 和 parent 后续延续请求
        # 接收这两个请求
        conn_a = server.get_connection(timeout=5.0)
        conn_b = server.get_connection(timeout=5.0)

        # 读取两个请求内容以区分哪个是 child
        req_a, _ = read_http_request(conn_a, timeout=5.0)
        req_b, _ = read_http_request(conn_b, timeout=5.0)
        assert req_a.get("reasoning_effort") == "high", req_a
        assert req_b.get("reasoning_effort") == "high", req_b

        # child 的 messages 中 role == 'user' 精确为 child-prompt (兼容 str 和 text 数组)
        def is_child_request(req):
            for m in req.get("messages", []):
                if m.get("role") != "user":
                    continue
                content = m.get("content")
                if isinstance(content, str):
                    if content == "child-prompt":
                        return True
                elif isinstance(content, list):
                    texts = []
                    for part in content:
                        if isinstance(part, dict) and part.get("type") == "text":
                            texts.append(part.get("text", ""))
                        elif isinstance(part, str):
                            texts.append(part)
                    if "".join(texts) == "child-prompt":
                        return True
            return False

        a_is_child = is_child_request(req_a)
        b_is_child = is_child_request(req_b)
        assert a_is_child != b_is_child, f"Expected exactly one child request, got a={a_is_child}, b={b_is_child}"

        if a_is_child:
            child_conn, parent_cont_conn = conn_a, conn_b
        else:
            child_conn, parent_cont_conn = conn_b, conn_a

        # 两个 case 分支
        if case == "sigint":
            # (a) 给 mio pid SIGINT，不是进程组
            os.kill(proc.pid, signal.SIGINT)
            assert_socket_closed(parent_cont_conn, timeout=5.0)
        elif case == "http_500":
            # (b) parent 后续请求回 500
            err_body = b'{"error":"internal mock error"}'
            resp_500 = (
                f"HTTP/1.1 500 Internal Server Error\r\n"
                f"Content-Type: application/json\r\n"
                f"Content-Length: {len(err_body)}\r\n"
                f"Connection: close\r\n\r\n".encode("utf-8") + err_body
            )
            parent_cont_conn.sendall(resp_500)
            parent_cont_conn.close()
        else:
            raise ValueError(f"Unknown case: {case}")

        # 断言 child socket 关闭
        assert_socket_closed(child_conn, timeout=5.0)

        # 断言 mio 进程仍然活着
        time.sleep(0.2)
        assert proc.poll() is None, f"mio process died unexpectedly (code={proc.returncode})"

        # 排空历史输出
        drain_master(master_fd, total_timeout=1.0, idle_timeout=0.05)

        # 用户重新发送 prompt
        os.write(master_fd, b"after cancellation\n")

        # 能够获得新 HTTP 请求
        new_conn = server.get_connection(timeout=5.0)
        new_req, _ = read_http_request(new_conn, timeout=5.0)

        # assert 新请求包含 user 消息 "after cancellation"
        def extract_user_messages(req):
            msgs = []
            for m in req.get("messages", []):
                if m.get("role") == "user":
                    content = m.get("content")
                    if isinstance(content, str):
                        msgs.append(content)
                    elif isinstance(content, list):
                        texts = []
                        for part in content:
                            if isinstance(part, dict) and part.get("type") == "text":
                                texts.append(part.get("text", ""))
                            elif isinstance(part, str):
                                texts.append(part)
                        msgs.append("".join(texts))
            return msgs

        user_msgs = extract_user_messages(new_req)
        assert any("after cancellation" in msg for msg in user_msgs), f"Expected 'after cancellation' in user messages, got: {user_msgs}"

        # 返回成功响应
        ok_sse = make_sse_response({"role": "assistant", "content": "recovered ok"}, "stop")
        new_conn.sendall(ok_sse)
        new_conn.close()

        # 读取 PTY 直到 recovered ok 出现，带 5s 总期限，并 assert 进程存活
        pty_deadline = time.time() + 5.0
        pty_output = b""
        while time.time() < pty_deadline:
            assert proc.poll() is None, f"mio died while awaiting recovered ok (code={proc.returncode})"
            chunk = drain_master(master_fd, total_timeout=0.2, idle_timeout=0.05)
            pty_output += chunk
            if b"recovered ok" in pty_output:
                break
        assert b"recovered ok" in pty_output, f"Timed out waiting for 'recovered ok' in PTY output: {pty_output.decode('utf-8', errors='replace')}"
        assert proc.poll() is None, "mio died after successful turn"

        return True
    finally:
        if proc is not None:
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
        server.close()
        home_dir.cleanup()
        run_cwd.cleanup()

if __name__ == "__main__":
    bin_to_test = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else DEFAULT_MIO_BIN.resolve()
    cases = sys.argv[2:] if len(sys.argv) > 2 else ["sigint", "http_500"]
    for c in cases:
        print(f"Running case: {c} against {bin_to_test.name} ...", end=" ", flush=True)
        try:
            run_test(bin_to_test, c)
            print("PASSED")
        except Exception as e:
            print("FAILED")
            raise e
