#!/usr/bin/env python3
"""Opt-in installed-Pi wire test, using only a local mock and pi-delegate.

Run from the root Codex environment (not recursively from a delegated Pi):
  python3 -B tests/pi_sampling_wire.py
This requires the locally installed Pi and the authorized delegation wrapper;
it does not contact a Provider or read the user's Pi credentials/settings.
"""
import argparse
import http.server
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--delegate", type=Path,
                        default=Path("/home/waya/.local/bin/pi-delegate"))
    args = parser.parse_args()
    if os.environ.get("PI_DELEGATE_ACTIVE"):
        parser.error("run outside delegated Pi; recursive delegation is forbidden")
    requests = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            length = int(self.headers.get("Content-Length", "0"))
            if not 0 < length <= 1024 * 1024:
                self.send_error(413)
                return
            requests.append((self.path, json.loads(self.rfile.read(length))))
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            chunks = [
                {"id": "local", "object": "chat.completion.chunk", "created": 0,
                 "model": "deepseek-flash", "choices": [{"index": 0,
                  "delta": {"role": "assistant", "content": "LOCAL_OK"},
                  "finish_reason": None}]},
                {"id": "local", "object": "chat.completion.chunk", "created": 0,
                 "model": "deepseek-flash", "choices": [{"index": 0,
                  "delta": {}, "finish_reason": "stop"}],
                 "usage": {"prompt_tokens": 10, "completion_tokens": 2,
                           "total_tokens": 12}},
            ]
            for chunk in chunks:
                self.wfile.write(("data: " + json.dumps(chunk) + "\n\n").encode())
            self.wfile.write(b"data: [DONE]\n\n")

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="mio-pi-wire-") as temporary:
            root = Path(temporary)
            home = root / "pi-home"
            home.mkdir(mode=0o700)
            models = {"providers": {"deepseek": {
                "baseUrl": f"http://127.0.0.1:{server.server_port}/v1",
                "apiKey": "local-test-only",
            }}}
            (home / "models.json").write_text(json.dumps(models))
            environment = {
                key: value for key, value in os.environ.items()
                if not key.startswith(("PI_", "MIO_"))
                and not any(word in key.upper() for word in ("API_KEY", "TOKEN", "SECRET"))
            }
            environment["PI_CODING_AGENT_DIR"] = str(home)
            command = [str(args.delegate), "--owner", "codex", "--heavy", "--",
                       "--mode", "json", "-p", "--no-session", "--no-tools",
                       "--no-mcp", "--no-extensions", "--no-context-files",
                       "--no-skills", "--no-prompt-templates",
                       "-e", str(ROOT / "evals/adapters/pi-fixed-sampling.ts"),
                       "--provider", "deepseek", "--model", "deepseek-flash",
                       "--thinking", "off", "Respond LOCAL_OK."]
            process = subprocess.Popen(command, cwd=root, env=environment,
                                       stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                       stderr=subprocess.PIPE, start_new_session=True)
            active_started = None
            while True:
                try:
                    stdout, stderr = process.communicate(timeout=1)
                    break
                except subprocess.TimeoutExpired as pending:
                    # pi-delegate may legitimately queue for an arbitrary time.
                    # Start the engine deadline only after Node/Pi has actually
                    # replaced the Python wrapper, or emitted startup output.
                    try:
                        comm = Path(f"/proc/{process.pid}/comm").read_text().strip()
                    except OSError:
                        comm = ""
                    if active_started is None and (
                            requests or pending.output or comm in ("pi", "node")):
                        active_started = time.monotonic()
                    if active_started is not None and time.monotonic() - active_started > 120:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.communicate()
                        raise AssertionError("active Pi did not finish within 120s (queue excluded)")
            # Never print raw output: even an unexpected configuration failure
            # should not copy any ambient credential into this test's log.
            assert process.returncode == 0, f"Pi exited {process.returncode}; raw logs discarded"
            assert len(requests) == 1, f"expected one local request, saw {len(requests)}"
            path, body = requests[0]
            assert path == "/v1/chat/completions", path
            assert body.get("model") == "deepseek-flash"
            assert body.get("temperature") == 0
            assert body.get("max_tokens") == 8192
            assert "max_completion_tokens" not in body
            assert b"LOCAL_OK" in stdout, "mock reply absent from Pi output"
            assert b"pi-fixed-sampling: refusing" not in stderr
            print("PASS: installed Pi extension sends temperature=0, max_tokens=8192 to loopback")
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
