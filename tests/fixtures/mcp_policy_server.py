#!/usr/bin/env python3
"""Minimal stdio MCP server used by the tool-policy end-to-end tests.

It speaks JSON-RPC 2.0 over stdin/stdout only (no network, no credentials) and
records what it receives into files under the process working directory:

* ``mcp_started.log`` is appended on ``initialize`` so a test can prove the
  project server was really spawned, even when every one of its tools is denied.
* ``mcp_calls.log`` is appended on ``tools/call`` so a test can prove which
  tool names and arguments actually reached the server.

The working directory is whatever the MCP client sets (the run's canonical
workspace root), so the relative log paths land in the test's temp dir.
"""

import json
import os
import sys


def send(payload):
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stdout.flush()


def append(path, record):
    with open(os.path.join(os.getcwd(), path), "a", encoding="utf-8") as handle:
        handle.write(json.dumps(record) + "\n")


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        request = json.loads(line)
    except ValueError:
        continue
    method = request.get("method")
    request_id = request.get("id")
    if method == "initialize":
        append("mcp_started.log", {"server": "policyfs"})
        send(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "serverInfo": {"name": "policyfs", "version": "0.0.0"},
                },
            }
        )
    elif method == "notifications/initialized":
        continue
    elif method == "tools/list":
        send(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "tools": [
                        {
                            "name": "record",
                            "description": "append the arguments to mcp_calls.log",
                            "inputSchema": {"type": "object"},
                        },
                        {
                            "name": "noop",
                            "description": "do nothing",
                            "inputSchema": {"type": "object"},
                        },
                    ]
                },
            }
        )
    elif method == "tools/call":
        params = request.get("params") or {}
        append(
            "mcp_calls.log",
            {"name": params.get("name"), "arguments": params.get("arguments")},
        )
        send(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {"content": [{"type": "text", "text": "recorded"}]},
            }
        )
    elif request_id is not None:
        send(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": -32601, "message": "unknown method"},
            }
        )
