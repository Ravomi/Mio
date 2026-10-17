//! DES-05 runtime regression: real CLI/RPC -> Policy -> journal -> tool-task
//! identity under concurrency.
//!
//! The offline `tests/journal_correlation.rs` proves the journal's pairing rules
//! by handing it `ToolIdentity::new` / `ApprovalBinding` values directly. That
//! is a useful unit-level baseline, but it never exercises the real chain that
//! mints those values. This file closes that gap with a real engine:
//!
//! 1. A real `mio --mode rpc` subprocess (`CARGO_BIN_EXE_mio`) runs a real
//!    provider loop against a loopback HTTP mock with a fixed `local-test`
//!    credential. `HOME` and `MIO_CONFIG_DIR` point at the temporary workspace,
//!    so no user MCP server or real provider is reachable.
//! 2. The session/journal are enabled (no `--no-session`); only the fixture's
//!    declarative plugin is installed. `--approve` keeps the real approval
//!    broker in the path so the approval attempt `callId` is a real value the
//!    client can compare against the journal.
//! 3. The model emits two calls to the same plugin tool in one turn. The two
//!    provider ids are distinct, but the arguments are byte-identical, so the
//!    journal `operation`/`target` of both records are the same. The plugin
//!    decides its role by atomically creating a workspace file: the first
//!    executor waits for `release`, the second returns `fast-result`
//!    immediately. The client writes `release` only after it really receives
//!    the fast `tool_finished` event, so completion order is inverted by real
//!    event order, never by a fixed sleep.
//! 4. Assertions pair records by the real provider `toolCallId` carried by the
//!    v1 events, then read the real journal JSON and require each record's
//!    `correlation.scope` to be the event `runId`, its `turn`/`effect_id` to be
//!    the real dispatch identity, its `call_id` to be the approval attempt that
//!    the client actually resolved (never the model tool-call id), and its
//!    `detail` to match the result the same provider id produced. Dispatch
//!    order is never used to guess a pairing.
//!
//! Scope honesty: the RPC v1 envelope does not publish a per-tool `effectId`,
//! so the journal `effect_id` is asserted to be a real, non-zero, distinct live
//! dispatcher id that does not collide with the observed model-dispatch effect
//! ids; it is not reconstructed from event order. The second test proves a
//! denied approval is recorded as the authoritative `Denied` phase (before
//! dispatch, zero side effect) and is never rewritten into `Unknown` at run
//! end.
//!
//! The test is deterministic and bounded: every socket has a timeout, the
//! client has an event deadline, the mock server thread is stopped and joined,
//! and the child is EOF'd, waited on and killed within a grace period. There is
//! no skip path: a broken fixture fails.

#[path = "../src/test_http.rs"]
mod test_http;

use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

/// Qualified plugin tool name: `plugin__<plugin>__<tool>`.
const PLUGIN: &str = "plugin__probe__emit";
/// Deliberately neutral provider ids: the fixture, not the id, decides which
/// call becomes the waiting one, so a passing test cannot be built on assuming
/// that a given id is the slow or the fast call.
const ID_ONE: &str = "call_one";
const ID_TWO: &str = "call_two";
const FAST: &str = "fast-result";
const SLOW: &str = "slow-result";
/// Client-side ceiling for one real event; the fixture's own 15s self-limit must
/// stay below it.
const CLIENT_EVENT_DEADLINE: Duration = Duration::from_secs(30);
/// Bounded wait before a child is force-killed.
const CHILD_GRACE: Duration = Duration::from_secs(3);
/// Bounded wait while joining the mock server / RPC reader thread.
const THREAD_GRACE: Duration = Duration::from_secs(2);

/// A sequential loopback mock. Each accepted connection is read once and the
/// handler writes the response itself.
struct MockServer {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let deadline = Instant::now() + THREAD_GRACE;
            while !handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
    }
}

fn mock_server<F>(handler: F) -> MockServer
where
    F: Fn(&mut TcpStream, &Value) + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let handle = std::thread::spawn(move || {
        // A hung test must not leave a permanent accept loop behind.
        let deadline = Instant::now() + Duration::from_secs(90);
        while !thread_stop.load(Ordering::SeqCst) && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut socket, _)) => {
                    socket
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    socket
                        .set_write_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    let request = test_http::read_json(&mut socket).1;
                    handler(&mut socket, &request);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(_) => return,
            }
        }
    });
    MockServer {
        address,
        stop,
        handle: Some(handle),
    }
}

fn sse_chunk(delta: Value, finish: Value) -> String {
    json!({
        "id": "test",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "test",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    })
    .to_string()
}

fn write_sse(socket: &mut TcpStream, delta: Value, finish: Value) {
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        sse_chunk(delta, Value::Null),
        sse_chunk(json!({}), finish)
    );
    let _ = write!(
        socket,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.flush();
}

fn write_text(socket: &mut TcpStream, text: &str) {
    write_sse(
        socket,
        json!({"role": "assistant", "content": text}),
        json!("stop"),
    );
}

fn tool_call(index: usize, name: &str, id: &str, arguments: Value) -> Value {
    json!({
        "index": index,
        "id": id,
        "type": "function",
        "function": {"name": name, "arguments": arguments.to_string()},
    })
}

/// Install a declarative plugin whose two calls share one tool name, one command
/// and byte-identical arguments. The role is decided by `O_CREAT | O_EXCL`, so
/// it comes from real execution, not from the arguments or the dispatch order.
fn write_probe_plugin(cwd: &Path) {
    let directory = cwd.join(".mio").join("plugins").join("probe");
    std::fs::create_dir_all(&directory).unwrap();
    let script = r##"
import os
import sys
import time

try:
    descriptor = os.open(".probe-role", os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
except FileExistsError:
    # The second executor is the fast one: it never waits for a human.
    sys.stdout.write("fast-result")
    sys.stdout.flush()
    sys.exit(0)

os.close(descriptor)
# Self-limited: the fixture can never outlive the test's own event deadline.
deadline = time.monotonic() + 15.0
while time.monotonic() < deadline:
    if os.path.exists("release"):
        sys.stdout.write("slow-result")
        sys.stdout.flush()
        sys.exit(0)
    time.sleep(0.02)
sys.exit(3)
"##;
    let manifest = json!({
        "schema_version": 1,
        "name": "probe",
        "version": "0.1.0",
        "description": "parallel same-argument tool-call identity fixture",
        "tools": [{
            "name": "emit",
            "description": "emit a role-derived result",
            "parameters": {
                "type": "object",
                "properties": {"tag": {"type": "string"}},
                "additionalProperties": true,
            },
            "command": ["python3", "-c", script],
        }],
    });
    std::fs::write(
        directory.join("mio-plugin.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
}

/// Real `mio --mode rpc` subprocess. Drop closes stdin (the RPC loop treats EOF
/// as a bounded shutdown), waits within a grace period, then force-kills, and
/// joins the reader thread within its own bound.
struct RpcClient {
    child: Child,
    events: mpsc::Receiver<Value>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Drop for RpcClient {
    fn drop(&mut self) {
        let _ = self.child.stdin.take();
        let deadline = Instant::now() + CHILD_GRACE;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        if let Some(reader) = self.reader.take() {
            let deadline = Instant::now() + THREAD_GRACE;
            while !reader.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if reader.is_finished() {
                let _ = reader.join();
            }
        }
    }
}

impl RpcClient {
    /// Start a session-enabled, journal-enabled engine. `--no-session` and
    /// `--no-mcp` are deliberately absent: the first would disable the journal,
    /// the second would also disable plugin loading. Isolation instead comes
    /// from clearing inherited `MIO_*` / key / proxy overrides and pointing
    /// `HOME` and `MIO_CONFIG_DIR` at the temporary workspace.
    fn start(root: &Path, address: SocketAddr) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mio"));
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if name.starts_with("MIO_")
                || name.contains("API_KEY")
                || name.to_lowercase().contains("proxy")
            {
                command.env_remove(key);
            }
        }
        let mut child = command
            .current_dir(root)
            .env("HOME", root)
            .env("MIO_CONFIG_DIR", root)
            .env("MIO_API_KEY", "local-test-credential")
            .args([
                "--mode",
                "rpc",
                "--no-context",
                "--no-shared-limits",
                "--approve",
                "--tool-concurrency",
                "2",
                "--provider",
                "custom",
                "--model",
                "test",
                "--base-url",
                &format!("http://{address}/v1"),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel::<Value>();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(&line) {
                    Ok(event) => {
                        if tx.send(event).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        // Never drop protocol corruption silently.
                        if tx
                            .send(json!({
                                "type": "__invalid_json__",
                                "error": error.to_string(),
                                "raw": line,
                            }))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            child,
            events: rx,
            reader: Some(reader),
        }
    }

    fn send(&mut self, value: Value) {
        let stdin = self.child.stdin.as_mut().expect("rpc stdin");
        writeln!(stdin, "{value}").unwrap();
        stdin.flush().unwrap();
    }

    /// Read events until the predicate holds; a deadline is a failure, never an
    /// unbounded wait.
    fn collect_until(&self, predicate: impl Fn(&Value) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + CLIENT_EVENT_DEADLINE;
        let mut seen = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "RPC event deadline: {seen:?}");
            let event = self
                .events
                .recv_timeout(remaining)
                .unwrap_or_else(|_| panic!("RPC event deadline: {seen:?}"));
            let matched = predicate(&event);
            seen.push(event);
            if matched {
                return seen;
            }
        }
    }

    fn close_stdin(&mut self) {
        let _ = self.child.stdin.take();
    }
}

/// Pump events into `seen`, answering every real approval request inline, until
/// `done` holds for a non-approval event.
fn pump_until(
    client: &mut RpcClient,
    seen: &mut Vec<Value>,
    approve: bool,
    done: impl Fn(&Value) -> bool,
) {
    loop {
        let batch =
            client.collect_until(|event| event["type"] == "approval_request" || done(event));
        let mut reached = false;
        for event in &batch {
            if event["type"] == "approval_request" {
                let request_id = event["request_id"].as_str().expect("approval request_id");
                client.send(json!({
                    "type": "approve_response",
                    "request_id": request_id,
                    "approved": approve,
                }));
            } else if done(event) {
                reached = true;
            }
        }
        seen.extend(batch);
        if reached {
            return;
        }
    }
}

/// The successful text of a plugin `tool_finished` event, if this is one.
fn plugin_finished_text(event: &Value) -> Option<String> {
    if event["type"] != "tool_finished" || event["data"]["operation"] != PLUGIN {
        return None;
    }
    event["data"]["result"]
        .get("Ok")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// The raw result text of a `tool_finished` event, panicking when it failed.
fn finish_text(event: &Value) -> String {
    event["data"]["result"]
        .get("Ok")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| panic!("plugin call must succeed: {event}"))
}

/// The two provider ids the fixture uses, sorted for order-independent
/// comparison.
fn fixture_ids() -> Vec<&'static str> {
    let mut ids = vec![ID_ONE, ID_TWO];
    ids.sort_unstable();
    ids
}

/// Read the workspace's single real session journal as raw JSON.
fn read_single_journal(root: &Path) -> Value {
    let directory = root.join(".mio").join("journal");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("journal directory {}: {error}", directory.display()))
        .map(|entry| entry.expect("journal directory entry").path())
        .filter(|path| path.extension().and_then(|extension| extension.to_str()) == Some("json"))
        .collect();
    files.sort();
    assert_eq!(
        files.len(),
        1,
        "expected exactly one session journal, got {files:?}"
    );
    let bytes = std::fs::read(&files[0]).expect("read journal");
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("journal {} is not JSON: {error}", files[0].display()))
}

#[test]
fn rpc_same_args_plugin_calls_bind_journal_to_real_dispatch_identity() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write_probe_plugin(&root);
    assert!(
        !root.join(".mio").join("mcp.json").exists(),
        "the fixture must not configure MCP"
    );

    let turn = Mutex::new(0usize);
    let mock = mock_server(move |socket, _request| {
        let mut turn = turn.lock().unwrap();
        if *turn == 0 {
            // Same tool, distinct provider ids, byte-identical arguments.
            let delta = json!({
                "role": "assistant",
                "tool_calls": [
                    tool_call(0, PLUGIN, ID_ONE, json!({"tag": "hold"})),
                    tool_call(1, PLUGIN, ID_TWO, json!({"tag": "hold"})),
                ],
            });
            write_sse(socket, delta, json!("tool_calls"));
        } else {
            write_text(socket, "done");
        }
        *turn += 1;
    });

    let mut client = RpcClient::start(&root, mock.address);
    let mut seen = Vec::new();

    client.send(json!({"type": "initialize", "id": "i1", "schema_versions": [1]}));
    seen.extend(client.collect_until(|event| event["id"] == "i1"));
    let negotiated = seen.last().expect("initialize response").clone();
    assert_eq!(negotiated["success"], true, "{negotiated}");
    assert_eq!(negotiated["data"]["envelope"], "v1", "{negotiated}");

    client.send(json!({"type": "prompt", "id": "p1", "message": "run both probes"}));
    seen.extend(client.collect_until(|event| event["id"] == "p1"));
    assert_eq!(seen.last().unwrap()["success"], true);

    // Wait for the real fast result (answering both real approval requests).
    // Only after that event does the client write `release`, so the slow call's
    // completion depends on the observed event, not on a sleep.
    pump_until(&mut client, &mut seen, true, |event| {
        plugin_finished_text(event).as_deref() == Some(FAST) || event["type"] == "agent_settled"
    });
    assert!(
        seen.iter()
            .any(|event| plugin_finished_text(event).as_deref() == Some(FAST)),
        "the fast call must report a real result: {seen:?}"
    );
    std::fs::write(root.join("release"), b"go").unwrap();
    if !seen.iter().any(|event| event["type"] == "agent_settled") {
        pump_until(&mut client, &mut seen, true, |event| {
            event["type"] == "agent_settled"
        });
    }

    client.close_stdin();
    drop(client);
    drop(mock);

    assert!(
        seen.iter().all(|event| event["type"] != "__invalid_json__"),
        "the RPC stream must stay valid JSON: {seen:?}"
    );

    // Real v1 run identity: the journal `scope` is this UUID.
    let run_started = seen
        .iter()
        .find(|event| event["type"] == "run_observation" && event["data"]["kind"] == "run_started")
        .expect("v1 run_started observation");
    let run_id = run_started["runId"]
        .as_str()
        .expect("run_started must carry a real runId")
        .to_owned();
    assert!(
        uuid::Uuid::parse_str(&run_id).is_ok(),
        "runId must be a real UUID: {run_id}"
    );
    let session_id = run_started["sessionId"]
        .as_str()
        .expect("v1 run_started must carry the real session id")
        .to_owned();
    assert!(
        uuid::Uuid::parse_str(&session_id).is_ok(),
        "sessionId must be a real UUID: {session_id}"
    );
    assert_eq!(run_started["subagentId"], Value::Null, "{run_started}");
    assert_eq!(run_started["parentRunId"], Value::Null, "{run_started}");

    // Locate the two plugin calls purely by their real provider ids.
    let mut start_by_id: BTreeMap<String, &Value> = BTreeMap::new();
    let mut finish_by_id: BTreeMap<String, &Value> = BTreeMap::new();
    for event in &seen {
        if event["data"]["operation"] != PLUGIN {
            continue;
        }
        if event["type"] == "tool_started" {
            let id = event["toolCallId"]
                .as_str()
                .expect("started toolCallId")
                .to_owned();
            assert!(
                start_by_id.insert(id, event).is_none(),
                "duplicate tool_started: {event}"
            );
        }
        if event["type"] == "tool_finished" {
            let id = event["toolCallId"]
                .as_str()
                .expect("finished toolCallId")
                .to_owned();
            assert!(
                finish_by_id.insert(id, event).is_none(),
                "duplicate tool_finished: {event}"
            );
        }
    }
    assert_eq!(start_by_id.len(), 2, "{seen:?}");
    assert_eq!(finish_by_id.len(), 2, "{seen:?}");
    let mut ids: Vec<&str> = finish_by_id.keys().map(String::as_str).collect();
    ids.sort_unstable();
    assert_eq!(ids, fixture_ids(), "{seen:?}");

    let fast_id = finish_by_id
        .iter()
        .find(|(_, event)| finish_text(event) == FAST)
        .map(|(id, _)| id.clone())
        .expect("one call must produce fast-result");
    let slow_id = finish_by_id
        .iter()
        .find(|(_, event)| finish_text(event) == SLOW)
        .map(|(id, _)| id.clone())
        .expect("one call must produce slow-result");
    assert_ne!(fast_id, slow_id);

    let fast_pos = seen
        .iter()
        .position(|event| {
            event["type"] == "tool_finished"
                && event["toolCallId"].as_str() == Some(fast_id.as_str())
        })
        .expect("fast tool_finished");
    let slow_pos = seen
        .iter()
        .position(|event| {
            event["type"] == "tool_finished"
                && event["toolCallId"].as_str() == Some(slow_id.as_str())
        })
        .expect("slow tool_finished");
    assert!(
        fast_pos < slow_pos,
        "the fast call must finish first; correlation cannot rely on dispatch order: {seen:?}"
    );

    // Real approvals: `scope` is the run UUID, `callId` is independent.
    let approvals: Vec<&Value> = seen
        .iter()
        .filter(|event| event["type"] == "approval_request")
        .collect();
    assert_eq!(approvals.len(), 2, "{seen:?}");
    let mut approval_call_id_by_id: BTreeMap<String, String> = BTreeMap::new();
    for request in &approvals {
        let binding = &request["binding"];
        assert_eq!(binding["scope"], json!(run_id), "{request}");
        assert_eq!(request["runId"], json!(run_id), "{request}");
        assert_eq!(request["toolCallId"], binding["toolCallId"], "{request}");
        assert_eq!(binding["entry"], "plugin", "{request}");
        let tool_call_id = binding["toolCallId"]
            .as_str()
            .expect("binding.toolCallId")
            .to_owned();
        let call_id = binding["callId"]
            .as_str()
            .expect("binding.callId")
            .to_owned();
        assert_ne!(
            call_id, tool_call_id,
            "the approval attempt id must not impersonate the provider tool-call id: {request}"
        );
        assert_ne!(call_id, run_id, "{request}");
        assert!(
            uuid::Uuid::parse_str(&call_id).is_ok(),
            "the approval attempt id must be a real UUID: {request}"
        );
        assert!(
            approval_call_id_by_id
                .insert(tool_call_id, call_id)
                .is_none(),
            "duplicate approval request: {request}"
        );
    }
    assert_eq!(approval_call_id_by_id.len(), 2, "{approvals:?}");

    // The real journal JSON on disk. Engine calls are authoritative canonical
    // `calls`, not the legacy `operations` list.
    let journal = read_single_journal(&root);
    assert_eq!(journal["version"], 2, "{journal}");
    assert_eq!(journal["session_id"], json!(session_id), "{journal}");
    let runs = journal["runs"].as_array().expect("journal runs");
    assert_eq!(runs.len(), 1, "{journal}");
    let run = &runs[0];
    assert_eq!(run["state"], "completed", "{run}");
    assert!(run["ended"].is_u64(), "{run}");
    assert_eq!(
        run["operations"].as_array().map(Vec::len).unwrap_or(0),
        0,
        "engine calls must not also write legacy operations: {run}"
    );
    let calls = run["calls"].as_array().expect("journal canonical calls");
    assert_eq!(calls.len(), 2, "{run}");

    let mut call_by_id: BTreeMap<String, &Value> = BTreeMap::new();
    for call in calls {
        let id = call["tool_call_id"]
            .as_str()
            .expect("journal canonical tool_call_id")
            .to_owned();
        assert!(
            call_by_id.insert(id, call).is_none(),
            "duplicate journal canonical call: {call}"
        );
    }
    assert_eq!(call_by_id.len(), 2, "{calls:?}");
    let mut call_ids: Vec<&str> = call_by_id.keys().map(String::as_str).collect();
    call_ids.sort_unstable();
    assert_eq!(call_ids, fixture_ids(), "{calls:?}");

    for id in [fast_id.as_str(), slow_id.as_str()] {
        let call = call_by_id
            .get(id)
            .unwrap_or_else(|| panic!("no canonical journal call for provider id {id}: {calls:?}"));
        let start = start_by_id.get(id).expect("tool_started");
        let finish = finish_by_id.get(id).expect("tool_finished");
        let expected_detail = finish_text(finish);

        assert_eq!(call["operation"], PLUGIN, "{call}");
        assert_eq!(call["phase"], "executed", "{call}");
        assert_eq!(
            call["detail"].as_str(),
            Some(expected_detail.as_str()),
            "the detail must be the result of this provider id: {call} {finish}"
        );

        // `scope` is the real run, `call_id` is the resolved approval attempt,
        // `tool_call_id` is the model's own id. They are not interchangeable.
        assert_eq!(call["scope"], json!(run_id), "{call}");
        assert_eq!(
            call["call_id"].as_str().map(str::to_owned),
            approval_call_id_by_id.get(id).cloned(),
            "the canonical call must record the approval attempt the client resolved: {call}"
        );
        assert_ne!(call["call_id"], call["tool_call_id"], "{call}");
        assert_ne!(call["call_id"], json!(run_id), "{call}");

        // turn/effect come from the real dispatch identity, not from order.
        assert_eq!(call["turn"], start["turnId"], "{call} {start}");
        assert_eq!(call["turn"], finish["turnId"], "{call} {finish}");
        assert!(call["turn"].is_u64(), "{call}");
        assert!(
            call["effect_id"].as_u64().is_some_and(|effect| effect > 0),
            "a real dispatch has a non-zero effect id: {call}"
        );

        assert_eq!(start["runId"], json!(run_id), "{start}");
        assert_eq!(finish["runId"], json!(run_id), "{finish}");
        assert_eq!(start["turnId"], finish["turnId"], "{start} {finish}");
        assert_eq!(start["subagentId"], Value::Null, "{start}");
        assert_eq!(finish["subagentId"], Value::Null, "{finish}");
        assert_eq!(start["parentRunId"], Value::Null, "{start}");
        assert_eq!(finish["parentRunId"], Value::Null, "{finish}");
    }

    let fast_call = call_by_id.get(&fast_id).expect("fast call");
    let slow_call = call_by_id.get(&slow_id).expect("slow call");

    // Identical operation/target must not be how the two records are told
    // apart; only the real provider id is.
    assert_eq!(fast_call["target"], slow_call["target"], "{calls:?}");
    assert!(
        call_by_id.values().all(|call| call["operation"] == PLUGIN),
        "{calls:?}"
    );

    // The two dispatches are distinct; their effect ids come from the live
    // dispatcher and never collide with the observed model-dispatch effects.
    assert_eq!(
        fast_call["turn"], slow_call["turn"],
        "both calls belong to the same engine turn: {calls:?}"
    );
    assert_ne!(fast_call["effect_id"], slow_call["effect_id"], "{calls:?}");
    assert_ne!(fast_call["call_id"], slow_call["call_id"], "{calls:?}");
    let model_effects: Vec<u64> = seen
        .iter()
        .filter(|event| {
            event["type"] == "run_observation" && event["data"]["kind"] == "model_dispatched"
        })
        .filter_map(|event| event["data"]["effectId"].as_u64())
        .collect();
    assert_eq!(model_effects.len(), 2, "{seen:?}");
    for call in call_by_id.values() {
        let effect = call["effect_id"].as_u64().unwrap();
        assert!(
            !model_effects.contains(&effect),
            "a tool effect id must not be a model effect id: {effect} in {model_effects:?}"
        );
    }

    // Both completions and the run finish belong to the same real run.
    let completions: Vec<&Value> = seen
        .iter()
        .filter(|event| event["type"] == "completion_call")
        .collect();
    assert_eq!(completions.len(), 2, "{seen:?}");
    for completion in &completions {
        assert_eq!(completion["runId"], json!(run_id), "{completion}");
    }
    let run_finished = seen
        .iter()
        .find(|event| event["type"] == "run_observation" && event["data"]["kind"] == "run_finished")
        .expect("v1 run_finished observation");
    assert_eq!(run_finished["runId"], json!(run_id), "{run_finished}");
    assert_eq!(run_finished["data"]["status"], "success", "{run_finished}");
    let settled = seen
        .iter()
        .find(|event| event["type"] == "agent_settled")
        .expect("agent_settled");
    assert_eq!(settled["status"], "completed", "{settled}");
}

/// A sequential deny path: the canonical call is persisted as `Normalized`
/// before approval, the operator refuses, the call never dispatches, and the
/// authoritative phase is `Denied` (zero side effect). It is explicitly never
/// rewritten into `Unknown` at run end: a policy/approval refusal is a denial,
/// not an unknown side effect.
#[test]
fn a_denied_approval_is_recorded_as_denied_and_never_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();

    let turn = Mutex::new(0usize);
    let mock = mock_server(move |socket, _request| {
        let mut turn = turn.lock().unwrap();
        if *turn == 0 {
            let delta = json!({
                "role": "assistant",
                "tool_calls": [tool_call(
                    0,
                    "write",
                    "call-denied",
                    json!({"path": "out.txt", "value": "x"}),
                )],
            });
            write_sse(socket, delta, json!("tool_calls"));
        } else {
            write_text(socket, "done");
        }
        *turn += 1;
    });

    let mut client = RpcClient::start(&root, mock.address);
    let mut seen = Vec::new();
    client.send(json!({"type": "initialize", "id": "i1", "schema_versions": [1]}));
    seen.extend(client.collect_until(|event| event["id"] == "i1"));
    client.send(json!({"type": "prompt", "id": "p1", "message": "write out.txt"}));
    seen.extend(client.collect_until(|event| event["id"] == "p1"));

    pump_until(&mut client, &mut seen, false, |event| {
        event["type"] == "agent_settled"
    });

    client.close_stdin();
    drop(client);
    drop(mock);

    assert!(
        seen.iter().all(|event| event["type"] != "__invalid_json__"),
        "the RPC stream must stay valid JSON: {seen:?}"
    );

    let request = seen
        .iter()
        .find(|event| event["type"] == "approval_request")
        .expect("the write call must request approval")
        .clone();
    let binding = &request["binding"];
    assert_eq!(binding["toolCallId"], "call-denied", "{request}");
    assert_eq!(binding["scope"], request["runId"], "{request}");
    assert_ne!(
        binding["callId"], binding["toolCallId"],
        "the approval attempt id must not impersonate the provider tool-call id: {request}"
    );
    let call_id = binding["callId"].as_str().unwrap().to_owned();
    let scope = binding["scope"].as_str().unwrap().to_owned();
    assert!(uuid::Uuid::parse_str(&call_id).is_ok(), "{request}");

    // A denial is before dispatch: no tool events and no side effect.
    assert!(
        !seen.iter().any(|event| {
            (event["type"] == "tool_started" || event["type"] == "tool_finished")
                && event["data"]["operation"] == "write"
        }),
        "a denied call must never start or finish a handler: {seen:?}"
    );
    assert!(
        !root.join("out.txt").exists(),
        "a denied write must not touch the workspace"
    );

    let journal = read_single_journal(&root);
    let runs = journal["runs"].as_array().expect("journal runs");
    assert_eq!(runs.len(), 1, "{journal}");
    let run = &runs[0];
    assert_eq!(run["state"], "completed", "{run}");
    assert_eq!(
        run["operations"].as_array().map(Vec::len).unwrap_or(0),
        0,
        "a denied engine call must not also write a legacy operation: {run}"
    );
    let calls = run["calls"].as_array().expect("journal canonical calls");
    assert_eq!(calls.len(), 1, "{run}");
    let call = &calls[0];
    assert_eq!(call["operation"], "write", "{call}");
    // The denial is recorded as `Denied` at the policy boundary, never as a
    // still-pending intent that the run end would turn into `Unknown`.
    assert_eq!(call["phase"], "denied", "{call}");
    assert_eq!(call["scope"], json!(scope), "{call}");
    assert_eq!(call["tool_call_id"], "call-denied", "{call}");
    assert_eq!(call["call_id"], json!(call_id), "{call}");
}
