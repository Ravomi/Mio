//! Native durable-recovery acceptance: one real CLI/RPC run that loses a real
//! native capture, refuses one new write, acknowledges the exact durable
//! evidence, and then runs exactly one new write.
//!
//! Everything in this test is real and in-process-distinct:
//!
//! * the actual `mio` CLI binary in `--mode rpc --approve`;
//! * a loopback OpenAI-compatible SSE mock that receives only the fixed local
//!   credential and never a real key;
//! * the production native `bash` supervision path
//!   (`shell::execute_with_policy` -> `shell::execute_with_timeout`), with a
//!   real `/bin/sh` child that first writes a sentinel file and then crosses
//!   the production 8 MiB complete-output bound via `head -c 8388609`;
//! * the production CLI `Host` with its durable
//!   `UnresolvedEffectLedger` (`.mio/unresolved-effects/effects.json`) and
//!   canonical `Journal`;
//! * the same `RpcApprovals` broker the CLI uses for parameter-bound tool
//!   approvals, reused for the unknown-side-effect confirmation.
//!
//! The scenario is deliberately narrow and asserts, in order:
//!
//! 1. the first real parameter-bound `shell` approval is passed through the
//!    broker, the real subprocess writes `sentinel.txt`, and the loss becomes a
//!    typed `native_capture` unknown recorded once in the durable ledger;
//! 2. a new model tool-call id that writes is refused when the separate
//!    unknown confirmation is rejected: no file is created and the ledger
//!    record stays `unknown`; a read in the same turn still runs;
//! 3. a second new id is admitted after the unknown confirmation is approved
//!    (the runner pre-resolves the exact snapshot), the ledger goes
//!    `unknown` -> `acknowledged` while the write still waits for its own
//!    binding approval, that binding approval is then approved separately, and
//!    exactly one write executes;
//! 4. after the run, the canonical shell call is still `unknown` with the real
//!    `native_capture` detail, the write is `executed`, the durable ledger key
//!    is the real run scope/call index (never a borrowed identity), and neither
//!    the canonical recovery response nor the durable ledger carries the raw command canary.
//!
//! Every wait, channel and child is bounded, the child is spawned in its own
//! process group and killed/reaped on drop, and the fake credential never
//! leaves loopback. See `QA/native-durable-contract-draft.md` for the contract
//! notes and the un-compiled assumptions of this draft.

#![cfg(unix)]

#[path = "../src/test_http.rs"]
mod test_http;

use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use uuid::Uuid;

const API_KEY: &str = "loopback-only-credential";
const MAX_EVENT_LINE: usize = 4 * 1024 * 1024;
const MAX_STDERR: usize = 256 * 1024;
const WAIT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Loopback model + real CLI process fixture
// ---------------------------------------------------------------------------

struct Harness {
    child: Child,
    events: mpsc::Receiver<Value>,
    requests: mpsc::Receiver<(TcpStream, Value, String)>,
    stderr: Arc<Mutex<Vec<u8>>>,
    stop: Arc<AtomicBool>,
    dir: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let accepting = Arc::clone(&stop);
        let (request_tx, requests) = mpsc::channel();
        std::thread::spawn(move || {
            while !accepting.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(15)))
                            .unwrap();
                        let (headers, request) = test_http::read_json(&mut stream);
                        if request_tx.send((stream, request, headers)).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("loopback accept failed: {error}"),
                }
            }
        });

        let mut command = Command::new(env!("CARGO_BIN_EXE_mio"));
        for (key, _) in std::env::vars_os() {
            let remove = {
                let name = key.to_string_lossy();
                name.starts_with("MIO_")
                    || name.contains("API_KEY")
                    || name.to_lowercase().contains("proxy")
            };
            if remove {
                command.env_remove(key);
            }
        }
        command
            .current_dir(dir.path())
            .env("MIO_CONFIG_DIR", dir.path())
            .env("MIO_API_KEY", API_KEY)
            .args([
                "--mode",
                "rpc",
                "--no-context",
                "--no-mcp",
                "--provider",
                "custom",
                "--model",
                "test",
                "--base-url",
                &format!("http://{address}/v1"),
                // The unknown-effect confirmation reuses the RPC approval
                // broker, which exists only in RPC mode with --approve.
                "--approve",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = command.spawn().unwrap();

        let stdout = child.stdout.take().expect("child stdout");
        let stderr = child.stderr.take().expect("child stderr");
        let (event_tx, events) = mpsc::channel();
        spawn_event_reader(stdout, event_tx);
        let captured = Arc::new(Mutex::new(Vec::new()));
        spawn_stderr_reader(stderr, Arc::clone(&captured));

        Self {
            child,
            events,
            requests,
            stderr: captured,
            stop,
            dir,
        }
    }

    fn send(&mut self, value: Value) {
        let stdin = self.child.stdin.as_mut().expect("child stdin");
        writeln!(stdin, "{value}").unwrap();
        stdin.flush().unwrap();
    }

    fn until(&self, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + WAIT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let event = self
                .events
                .recv_timeout(remaining)
                .expect("RPC event deadline");
            if predicate(&event) {
                return event;
            }
        }
    }

    fn next_request(&self) -> (TcpStream, Value, String) {
        self.requests
            .recv_timeout(WAIT)
            .expect("model request deadline")
    }

    fn approval(&self) -> Value {
        self.until(|event| event["type"] == "approval_request")
    }

    /// Answer one approval request and wait for its own ack response.
    fn resolve(&mut self, request: &str, approved: bool) {
        self.send(json!({
            "type": "approve_response",
            "request_id": request,
            "approved": approved,
        }));
        let response = self
            .until(|event| event["type"] == "response" && event["id"].as_str() == Some(request));
        assert_eq!(response["success"].as_bool(), Some(true), "{response}");
    }

    fn settled(&self, status: &str) {
        let event = self.until(|event| event["type"] == "agent_settled");
        assert_eq!(event["status"].as_str(), Some(status), "{event}");
    }

    fn get_state(&mut self, id: &str) -> Value {
        self.send(json!({"type": "get_state", "id": id}));
        self.until(|event| event["type"] == "response" && event["id"].as_str() == Some(id))
    }

    fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().unwrap()).into_owned()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Kill the process group this harness created, not only the direct
        // child, then reap it with a bounded wait so a hung turn can never
        // block the rest of the test run.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => break,
            }
        }
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Read JSONL RPC events from stdout with a hard per-line bound.
fn spawn_event_reader(stdout: std::process::ChildStdout, tx: mpsc::Sender<Value>) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line: Vec<u8> = Vec::new();
        loop {
            line.clear();
            let mut overlong = false;
            loop {
                let available = match reader.fill_buf() {
                    Ok(available) => available,
                    Err(_) => return,
                };
                if available.is_empty() {
                    if !line.is_empty() {
                        emit_event(&line, &tx);
                    }
                    return;
                }
                let newline = available.iter().position(|byte| *byte == b'\n');
                let take = newline.unwrap_or(available.len());
                let room = MAX_EVENT_LINE.saturating_sub(line.len());
                let copy = take.min(room);
                line.extend_from_slice(&available[..copy]);
                if copy < take {
                    overlong = true;
                }
                reader.consume(take + usize::from(newline.is_some()));
                if newline.is_some() || overlong {
                    break;
                }
            }
            assert!(!overlong, "RPC stdout line exceeded {MAX_EVENT_LINE} bytes");
            if line.iter().all(|byte| byte.is_ascii_whitespace()) {
                continue;
            }
            emit_event(&line, &tx);
        }
    });
}

fn emit_event(line: &[u8], tx: &mpsc::Sender<Value>) {
    let text = std::str::from_utf8(line).expect("RPC stdout must be UTF-8");
    let event = serde_json::from_str::<Value>(text.trim_end())
        .unwrap_or_else(|error| panic!("non-JSON RPC output ({error}): {text}"));
    if tx.send(event).is_err() {
        // The test already finished; nothing else to deliver.
    }
}

/// Drain stderr continuously into a bounded buffer (never an unbounded pipe).
fn spawn_stderr_reader(stderr: std::process::ChildStderr, sink: Arc<Mutex<Vec<u8>>>) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(read) => {
                    let mut guard = sink.lock().unwrap();
                    if guard.len() < MAX_STDERR {
                        let room = MAX_STDERR - guard.len();
                        guard.extend_from_slice(&chunk[..read.min(room)]);
                    }
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Loopback SSE encoding
// ---------------------------------------------------------------------------

fn sse_chunk(delta: Value, finish: Value) -> String {
    json!({
        "id": "chatcmpl-native-durable",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "test",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
    })
    .to_string()
}

fn reply_tool_calls(mut stream: TcpStream, calls: &[(&str, &str, Value)]) {
    let tool_calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, arguments))| {
            json!({
                "index": index,
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()}
            })
        })
        .collect();
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        sse_chunk(
            json!({"role": "assistant", "tool_calls": tool_calls}),
            Value::Null
        ),
        sse_chunk(json!({}), json!("tool_calls"))
    );
    write_response(&mut stream, &body);
}

fn reply_text(mut stream: TcpStream, text: &str) {
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        sse_chunk(json!({"role": "assistant", "content": text}), Value::Null),
        sse_chunk(json!({}), json!("stop"))
    );
    write_response(&mut stream, &body);
}

fn write_response(stream: &mut TcpStream, body: &str) {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// Durable-ledger and canonical-journal readers
// ---------------------------------------------------------------------------

fn read_ledger_text(root: &Path) -> String {
    let path = root.join(".mio/unresolved-effects/effects.json");
    let deadline = Instant::now() + WAIT;
    loop {
        if let Ok(text) = std::fs::read_to_string(&path) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "durable ledger {} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn ledger_value(root: &Path) -> Value {
    serde_json::from_str(&read_ledger_text(root)).expect("ledger JSON")
}

fn single_entry(ledger: &Value) -> &Value {
    let entries = ledger["entries"].as_array().expect("ledger entries");
    assert_eq!(entries.len(), 1, "{ledger}");
    &entries[0]
}

fn find_call<'a>(calls: &'a [Value], tool_call_id: &str) -> &'a Value {
    calls
        .iter()
        .find(|call| call["tool_call_id"].as_str() == Some(tool_call_id))
        .unwrap_or_else(|| panic!("no canonical call for '{tool_call_id}': {calls:?}"))
}

// ---------------------------------------------------------------------------
// The acceptance test
// ---------------------------------------------------------------------------

#[test]
fn native_capture_unknown_is_refused_then_acked_and_runs_one_new_write() {
    let mut harness = Harness::new();
    let root = harness.dir.path().canonicalize().unwrap();
    std::fs::write(root.join("present.txt"), "PRESENT-READ-OK").unwrap();
    let canary = format!("MIO-DURABLE-RAW-CANARY-{}", Uuid::new_v4().simple());

    // --- Turn 1: a real shell runs, then loses its capture. -----------------
    harness.send(json!({"type": "prompt", "id": "start", "message": "start"}));
    let started =
        harness.until(|event| event["type"] == "response" && event["id"].as_str() == Some("start"));
    assert_eq!(started["success"].as_bool(), Some(true), "{started}");

    let (stream, request, headers) = harness.next_request();
    assert_eq!(request["model"], "test");
    assert!(
        headers.contains("Bearer ") && headers.contains(API_KEY),
        "the loopback model must receive only the test credential: {headers}"
    );

    let shell_command =
        format!("printf sentinel > sentinel.txt; head -c 8388609 /dev/zero # {canary}");
    reply_tool_calls(
        stream,
        &[("call_shell", "bash", json!({"command": shell_command}))],
    );

    let shell_approval = harness.approval();
    assert_eq!(shell_approval["operation"].as_str(), Some("shell"));
    let shell_request = shell_approval["request_id"].as_str().unwrap().to_owned();
    let shell_binding = shell_approval["binding"].clone();
    assert_eq!(shell_binding["tool"].as_str(), Some("shell"));
    assert_eq!(shell_binding["entry"].as_str(), Some("builtin"));
    assert_eq!(shell_binding["toolCallId"].as_str(), Some("call_shell"));
    assert_eq!(shell_binding["cwd"].as_str(), root.to_str());
    assert_eq!(shell_binding["paramsSha256"].as_str().unwrap().len(), 64);
    let run_scope = shell_binding["scope"].as_str().unwrap().to_owned();
    assert!(Uuid::parse_str(&run_scope).is_ok(), "{run_scope}");
    let shell_call_id = shell_binding["callId"].as_str().unwrap().to_owned();
    assert_ne!(shell_call_id, "call_shell");
    // The approving host sees the real command; durable evidence must not.
    assert!(shell_approval["target"].as_str().unwrap().contains(&canary));
    harness.resolve(&shell_request, true);

    let (stream, _shell_turn, _headers) = harness.next_request();
    assert_eq!(
        std::fs::read_to_string(root.join("sentinel.txt")).unwrap(),
        "sentinel"
    );
    let ledger_text = read_ledger_text(&root);
    assert!(
        !ledger_text.contains(&canary),
        "the durable ledger must never copy the raw command"
    );
    let ledger = serde_json::from_str::<Value>(&ledger_text).unwrap();
    let entry = single_entry(&ledger);
    assert_eq!(entry["state"].as_str(), Some("unknown"));
    assert_eq!(entry["scope"].as_str(), Some(run_scope.as_str()));
    assert_eq!(entry["facts"][0]["operation"].as_str(), Some("shell"));
    assert_eq!(entry["facts"][0]["reason"].as_str(), Some("result_unknown"));
    assert_eq!(entry["facts_dropped"].as_u64().unwrap_or(0), 0);

    // --- Turn 2: a new id is refused at the unknown confirmation. -----------
    reply_tool_calls(
        stream,
        &[
            (
                "call_denied",
                "workspace",
                json!({"operation": "write", "path": "denied.txt", "value": "SHOULD-NOT-EXIST"}),
            ),
            ("call_read", "read", json!({"path": "present.txt"})),
        ],
    );
    let deny_confirmation = harness.approval();
    assert_eq!(
        deny_confirmation["operation"].as_str(),
        Some("confirm_unknown_side_effect")
    );
    let deny_request = deny_confirmation["request_id"].as_str().unwrap().to_owned();
    assert!(
        deny_confirmation["binding"].is_null(),
        "the unknown confirmation is not a tool binding: {deny_confirmation}"
    );
    harness.resolve(&deny_request, false);

    let (stream, denied_turn, _headers) = harness.next_request();
    assert!(
        !root.join("denied.txt").exists(),
        "a refused confirmation must not release the write"
    );
    let denied_body = denied_turn.to_string();
    assert!(
        denied_body.contains("Tool policy denied"),
        "the refusal must be model-visible: {denied_body}"
    );
    assert!(
        denied_body.contains("PRESENT-READ-OK"),
        "read-only tooling must stay available after an unknown: {denied_body}"
    );
    let ledger = ledger_value(&root);
    assert_eq!(single_entry(&ledger)["state"].as_str(), Some("unknown"));

    // --- Turn 3: the new id is confirmed, acked, and runs exactly once. -----
    reply_tool_calls(
        stream,
        &[(
            "call_final",
            "workspace",
            json!({"operation": "write", "path": "final.txt", "value": "MIO-DURABLE-FINAL"}),
        )],
    );
    let confirmation = harness.approval();
    assert_eq!(
        confirmation["operation"].as_str(),
        Some("confirm_unknown_side_effect")
    );
    let confirm_request = confirmation["request_id"].as_str().unwrap().to_owned();
    harness.resolve(&confirm_request, true);

    // The write's own binding approval can only be asked after the exact
    // pre-resolved snapshot was acknowledged; the write has not run yet.
    let write_approval = harness.approval();
    assert_eq!(write_approval["operation"].as_str(), Some("write"));
    let write_request = write_approval["request_id"].as_str().unwrap().to_owned();
    let write_binding = write_approval["binding"].clone();
    // The aggregate `workspace` tool dispatches to the concrete `write`
    // builtin: the binding names the real operation (`write`) while its
    // `entry` kind is `aggregate` (same shape as `tests/rpc.rs`).
    assert_eq!(write_binding["tool"].as_str(), Some("write"));
    assert_eq!(write_binding["entry"].as_str(), Some("aggregate"));
    assert_eq!(write_binding["operation"].as_str(), Some("write"));
    assert_eq!(write_binding["toolCallId"].as_str(), Some("call_final"));
    assert_eq!(write_binding["scope"].as_str(), Some(run_scope.as_str()));
    assert_ne!(
        write_binding["callId"].as_str(),
        write_binding["toolCallId"].as_str()
    );

    let ledger = ledger_value(&root);
    let entry = single_entry(&ledger);
    assert_eq!(entry["state"].as_str(), Some("acknowledged"));
    assert_eq!(entry["scope"].as_str(), Some(run_scope.as_str()));
    assert_eq!(entry["facts"][0]["reason"].as_str(), Some("result_unknown"));
    assert_eq!(
        entry["acknowledgement"]["snapshot_key"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    assert!(
        !root.join("final.txt").exists(),
        "the new write must still pass its own binding approval"
    );

    // Every approval used its own request id.
    let request_ids = [
        &shell_request,
        &deny_request,
        &confirm_request,
        &write_request,
    ];
    for (index, first) in request_ids.iter().enumerate() {
        for second in request_ids.iter().skip(index + 1) {
            assert_ne!(first, second, "each approval must use its own request id");
        }
    }
    harness.resolve(&write_request, true);

    let (stream, write_turn, _headers) = harness.next_request();
    assert_eq!(
        std::fs::read_to_string(root.join("final.txt")).unwrap(),
        "MIO-DURABLE-FINAL"
    );
    assert!(!root.join("denied.txt").exists());
    assert!(
        write_turn.to_string().contains("MIO-DURABLE-FINAL"),
        "the executed write must be model-visible"
    );
    reply_text(stream, "done");
    harness.settled("completed");

    // --- Canonical and durable evidence after the run. ----------------------
    let state = harness.get_state("recovery");
    let recovery = &state["data"]["recovery"];
    let calls = recovery["calls"].as_array().expect("canonical calls");

    let shell_call = find_call(calls, "call_shell");
    assert_eq!(shell_call["phase"].as_str(), Some("unknown"));
    assert_eq!(shell_call["operation"].as_str(), Some("shell"));
    assert_eq!(shell_call["tool"].as_str(), Some("shell"));
    assert_eq!(shell_call["scope"].as_str(), Some(run_scope.as_str()));
    assert_eq!(shell_call["target_redacted"].as_bool(), Some(true));
    assert!(
        !shell_call["target"].as_str().unwrap().contains(&canary),
        "the canonical shell target must never copy the raw command"
    );
    assert!(
        shell_call["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("native_capture"),
        "the canonical detail must name the real native capture loss: {shell_call}"
    );
    assert_eq!(shell_call["tool_call_id"].as_str(), Some("call_shell"));
    assert_eq!(shell_call["call_id"].as_str(), Some(shell_call_id.as_str()));
    assert_ne!(
        shell_call["call_id"].as_str(),
        shell_call["tool_call_id"].as_str()
    );

    let denied_call = find_call(calls, "call_denied");
    assert_eq!(denied_call["phase"].as_str(), Some("denied"));
    assert_eq!(denied_call["scope"].as_str(), Some(run_scope.as_str()));

    let read_call = find_call(calls, "call_read");
    assert_eq!(read_call["phase"].as_str(), Some("executed"));

    let final_call = find_call(calls, "call_final");
    assert_eq!(final_call["phase"].as_str(), Some("executed"));
    assert_eq!(final_call["scope"].as_str(), Some(run_scope.as_str()));
    assert_eq!(
        calls
            .iter()
            .filter(|call| {
                call["phase"].as_str() == Some("executed")
                    && call["operation"].as_str() == Some("write")
            })
            .count(),
        1,
        "exactly one new write may execute: {calls:?}"
    );

    let ledger_text = read_ledger_text(&root);
    assert!(!ledger_text.contains(&canary));
    let ledger = serde_json::from_str::<Value>(&ledger_text).unwrap();
    // Successful calls remain Resolved in the ledger until safe retention
    // pruning; find both real keys instead of discarding the new write's record.
    let entries = ledger["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "{ledger}");
    let entry = entries
        .iter()
        .find(|entry| {
            entry["scope"].as_str() == Some(run_scope.as_str())
                && entry["call_index"] == shell_call["call_index"]
        })
        .expect("the original shell key must retain its acknowledged evidence");
    let write_entry = entries
        .iter()
        .find(|entry| {
            entry["scope"].as_str() == Some(run_scope.as_str())
                && entry["call_index"] == final_call["call_index"]
        })
        .expect("the actual fresh write must retain its own confirmed record");
    assert_eq!(write_entry["state"].as_str(), Some("resolved"));
    assert_ne!(entry["call_index"], write_entry["call_index"]);
    assert_eq!(entry["state"].as_str(), Some("acknowledged"));
    assert_eq!(entry["scope"].as_str(), Some(run_scope.as_str()));
    assert_eq!(entry["call_index"], shell_call["call_index"]);
    assert!(
        entry["facts"]
            .as_array()
            .is_some_and(|facts| !facts.is_empty()),
        "the acknowledgement must preserve the original unknown facts: {entry}"
    );
    assert!(
        !serde_json::to_string(&state).unwrap().contains(&canary),
        "the canonical recovery response must not carry the raw shell command"
    );

    // --- Bounded logs never leak the credential or an auth header. ----------
    let stderr = harness.stderr_text();
    assert!(
        !stderr.contains(API_KEY),
        "stderr leaked the loopback credential: {stderr}"
    );
    assert!(
        !stderr.to_lowercase().contains("bearer"),
        "stderr leaked an auth header: {stderr}"
    );
}
