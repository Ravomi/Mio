//! R14 end-to-end acceptance for the final-response reservation: on the real
//! `run_with_options` model dispatch boundary, the request for the one known
//! last remaining model call of a tool exchange carries the protocol-level
//! `tool_choice: "none"`, so the provider can return the model's own text
//! answer. The pre-admission quota snapshot that gates this is not an admission
//! promise: the real scheduler still decides whether the call runs.
//!
//! Every test drives real agent turns through a local mock HTTP server. No
//! external provider is contacted and no real credential is read: requests
//! carry only the fixed `local-test` token this file sets on [`Config`].
//!
//! The reservation is deliberately narrow. Only the eligible final call is
//! narrowed; every earlier call keeps its provider choice and every call still
//! advertises the identical tool definitions. A provider that ignores `none`
//! and returns another tool call is never auto-answered: the call really runs,
//! the original cap blocks the next API request, and the run fails with a
//! scheduling error instead of a fabricated success.

#[path = "../src/test_http.rs"]
mod test_http;

use mio_coding_agent::{
    AgentEvent, Config, Host, HostFuture, Provider, RunOptions, Services, coordinator,
    run_with_options, scheduling,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::Write,
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const API_KEY: &str = "local-test";
const NOTE_MARKER: &str = "[mio-budget]";

#[derive(Clone, Debug)]
struct RecordedRequest {
    body: Value,
}

struct MockState {
    requests: Mutex<Vec<RecordedRequest>>,
    script: Mutex<VecDeque<Value>>,
}

struct MockServer {
    address: SocketAddr,
    state: Arc<MockState>,
}

impl MockServer {
    fn start(script: Vec<Value>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let state = Arc::new(MockState {
            requests: Mutex::new(Vec::new()),
            script: Mutex::new(script.into_iter().collect()),
        });
        let thread_state = Arc::clone(&state);
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(60);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((socket, _)) => {
                        let state = Arc::clone(&thread_state);
                        std::thread::spawn(move || serve(socket, state));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("mock server accept failed: {error}"),
                }
            }
        });
        Self { address, state }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.state.requests.lock().unwrap().clone()
    }
}

fn serve(mut socket: TcpStream, state: Arc<MockState>) {
    let _ = socket.set_read_timeout(Some(Duration::from_secs(15)));
    let _ = socket.set_write_timeout(Some(Duration::from_secs(15)));
    let (_, body) = test_http::read_json(&mut socket);
    state
        .requests
        .lock()
        .unwrap()
        .push(RecordedRequest { body: body.clone() });
    let scripted = state.script.lock().unwrap().pop_front();
    let Some(Value::String(payload)) = scripted else {
        return;
    };
    let mut out = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    out.push_str(&payload);
    let _ = socket.write_all(out.as_bytes());
    let _ = socket.flush();
}

fn chunk(delta: Value, finish: Value) -> Value {
    json!({
        "id": "chatcmpl-reservation",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "local-test",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
    })
}

fn tool_stream(id: &str, name: &str, args: Value) -> Value {
    let tool_calls = json!([{
        "index": 0,
        "id": id,
        "type": "function",
        "function": {"name": name, "arguments": args.to_string()}
    }]);
    let rendered = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(
            json!({"role": "assistant", "tool_calls": tool_calls}),
            Value::Null
        ),
        chunk(json!({}), json!("tool_calls"))
    );
    Value::String(rendered)
}

fn text_stream(text: &str) -> Value {
    let rendered = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(json!({"role": "assistant", "content": text}), Value::Null),
        chunk(json!({}), json!("stop"))
    );
    Value::String(rendered)
}

struct TestHost;

impl Host for TestHost {
    fn shell(&self, _command: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }
    fn on_event(&self, _event: AgentEvent) {}
}

fn config_for(base_url: String) -> Config {
    Config {
        provider: Provider::Custom,
        base_url,
        model: "test".into(),
        api_key: API_KEY.into(),
    }
}

fn scheduler_with_cap(cap: u64) -> Arc<scheduling::Scheduler> {
    Arc::new(
        scheduling::Scheduler::new(
            coordinator::Limits {
                max_model_calls: Some(cap),
                ..coordinator::Limits::default()
            },
            None,
        )
        .unwrap(),
    )
}

fn role(message: &Value) -> &str {
    message["role"].as_str().unwrap_or_default()
}

fn text_of(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| part.get("text").and_then(Value::as_str).unwrap_or_default())
            .collect::<Vec<_>>()
            .join(""),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn is_note(message: &Value) -> bool {
    role(message) == "system" && text_of(message).starts_with(NOTE_MARKER)
}

/// Every tool call in the rendered body is answered once and nothing else is.
fn assert_tool_pairs_intact(messages: &[Value], rendered: &str) {
    let mut open: Vec<String> = Vec::new();
    for message in messages {
        match role(message) {
            "assistant" => {
                if let Some(calls) = message["tool_calls"].as_array() {
                    for call in calls {
                        let id = call["id"].as_str().unwrap_or_default().to_string();
                        assert!(!open.contains(&id), "duplicate tool call {id}: {rendered}");
                        open.push(id);
                    }
                }
            }
            "tool" => {
                let id = message["tool_call_id"].as_str().unwrap_or_default();
                let found = open
                    .iter()
                    .position(|candidate| candidate == id)
                    .unwrap_or_else(|| panic!("orphan tool result {id}: {rendered}"));
                open.remove(found);
            }
            _ => {}
        }
    }
    assert!(open.is_empty(), "unanswered tool call {open:?}: {rendered}");
}

/// With a two-tool-work turn before the final call, only the third (last
/// remaining) request is narrowed to `tool_choice: none`; earlier requests keep
/// their provider choice, every request keeps the identical tool definitions,
/// the note stays last with the final-call phase, and the provider's own text
/// answer still completes the run on exactly the original cap.
#[tokio::test]
async fn only_the_eligible_final_call_is_narrowed_and_the_model_answers() {
    let cap = 3u64;
    let server = MockServer::start(vec![
        tool_stream("call_res_1", "read", json!({"path": "goal.txt"})),
        tool_stream("call_res_2", "read", json!({"path": "goal.txt"})),
        text_stream("RESERVED-FINAL-TEXT-ANSWER"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("goal.txt"), "GOAL-KEEP-ME\n").unwrap();
    let result = run_with_options(
        config_for(server.base_url()),
        Vec::new(),
        "RESERVE-GOAL: read goal.txt twice, then answer",
        dir.path(),
        std::sync::Arc::new(TestHost),
        Services::default(),
        RunOptions {
            scheduler: Some(scheduler_with_cap(cap)),
            ..RunOptions::default()
        },
    )
    .await
    .expect("the final remaining call must return the model's real text answer");

    let requests = server.requests();
    assert_eq!(
        requests.len(),
        cap as usize,
        "the run must use exactly the original cap and never a hidden extra call"
    );
    let rendered = requests
        .iter()
        .map(|request| request.body.to_string())
        .collect::<Vec<_>>();

    let templates = requests[0].body["tools"].clone();
    assert!(
        templates.as_array().is_some_and(|tools| !tools.is_empty()),
        "the request must advertise real tools: {}",
        rendered[0]
    );
    for (index, request) in requests.iter().enumerate() {
        assert_eq!(
            request.body["tools"], templates,
            "call {index} must advertise the identical tool definitions: {}",
            rendered[index]
        );
        let messages = request.body["messages"]
            .as_array()
            .unwrap_or_else(|| panic!("call {index} must send messages: {}", rendered[index]));
        assert!(
            is_note(messages.last().unwrap()),
            "call {index}: the runtime note must stay last: {}",
            rendered[index]
        );
        assert!(
            messages
                .iter()
                .any(|message| text_of(message).contains("RESERVE-GOAL")),
            "call {index} must keep the user goal: {}",
            rendered[index]
        );
        assert_tool_pairs_intact(messages, &rendered[index]);
        if index + 1 == cap as usize {
            assert_eq!(
                request.body.get("tool_choice"),
                Some(&json!("none")),
                "the eligible final call must ask for the model's text answer: {}",
                rendered[index]
            );
            let note = text_of(messages.last().unwrap());
            assert!(
                note.contains("calls left: local 1 (min 1)"),
                "the final note must report one known call left: {note}"
            );
            assert!(
                note.contains("last call in this snapshot"),
                "the final note must keep the final-call phase: {note}"
            );
        } else {
            assert!(
                request.body.get("tool_choice") != Some(&json!("none")),
                "call {index} must not be narrowed before the final slot: {}",
                rendered[index]
            );
        }
    }
    // The model's own answer is reported; nothing was fabricated for it.
    assert!(
        serde_json::to_string(&result)
            .unwrap()
            .contains("RESERVED-FINAL-TEXT-ANSWER"),
        "the final text answer must be the model's own output"
    );
}

/// A provider that ignores `none` and returns another tool call is not
/// auto-answered and not stripped: the call really runs, and the original cap
/// blocks the next API request so the run fails as a scheduling error.
#[tokio::test]
async fn a_provider_that_ignores_none_is_blocked_by_the_original_cap() {
    let cap = 3u64;
    let server = MockServer::start(vec![
        tool_stream("call_ign_1", "read", json!({"path": "goal.txt"})),
        tool_stream("call_ign_2", "read", json!({"path": "goal.txt"})),
        // The third request already carried `tool_choice: none`; this provider
        // ignores it and asks for a write instead.
        tool_stream(
            "call_ign_3",
            "write",
            json!({"path": "ignored-tool-call.txt", "content": "the ignored call really ran"}),
        ),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("goal.txt"), "GOAL\n").unwrap();
    let error = run_with_options(
        config_for(server.base_url()),
        Vec::new(),
        "RESERVE-GOAL: keep working",
        dir.path(),
        std::sync::Arc::new(TestHost),
        Services::default(),
        RunOptions {
            scheduler: Some(scheduler_with_cap(cap)),
            ..RunOptions::default()
        },
    )
    .await
    .expect_err("a cap-exhausted run must fail, never fabricate a success");
    assert!(
        error.contains("Scheduling"),
        "the refusal must be attributed to scheduling, got: {error}"
    );
    let requests = server.requests();
    assert_eq!(
        requests.len(),
        cap as usize,
        "no request may pass the original cap"
    );
    assert_eq!(
        requests[cap as usize - 1].body.get("tool_choice"),
        Some(&json!("none")),
        "the final remaining request must still have been narrowed"
    );
    // The ignored tool call was neither stripped nor replaced by an answer: it
    // really executed and left its effect.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("ignored-tool-call.txt")).unwrap(),
        "the ignored call really ran",
        "the provider's tool call must run, not be silently dropped"
    );
}

/// The legacy one-slot budget keeps its first request Auto and is only refused
/// after that one real call; the reservation never applies to the initial slot.
#[tokio::test]
async fn the_initial_single_slot_budget_stays_auto_and_is_refused_after_one_call() {
    let server = MockServer::start(vec![tool_stream(
        "call_single",
        "read",
        json!({"path": "goal.txt"}),
    )]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("goal.txt"), "GOAL\n").unwrap();
    let error = run_with_options(
        config_for(server.base_url()),
        Vec::new(),
        "read goal.txt",
        dir.path(),
        std::sync::Arc::new(TestHost),
        Services::default(),
        RunOptions {
            scheduler: Some(scheduler_with_cap(1)),
            ..RunOptions::default()
        },
    )
    .await
    .expect_err("the second call must be refused by the one-slot budget");
    assert!(error.contains("Scheduling"), "got: {error}");
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "only one call is ever admitted");
    assert!(
        requests[0].body.get("tool_choice") != Some(&json!("none")),
        "the initial one-slot call must keep its provider choice, got: {}",
        requests[0].body
    );
}

/// The real CLI wire carries the same protocol field: with a dummy key, a
/// temporary HOME and a local SSE mock, the third and last request has
/// `tool_choice: none` and the run succeeds, while an ignoring provider is
/// stopped by the original cap with a non-zero exit. No real model is called.
#[test]
fn cli_wire_carries_none_on_the_final_call_and_respects_the_cap() {
    let cli = |base_url: &str, cwd: &Path, home: &Path, config: &Path| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mio"));
        cmd.env_remove("MIO_MODEL")
            .env_remove("MIO_BASE_URL")
            .env_remove("OPENAI_API_KEY")
            .env("HOME", home)
            .env("MIO_CONFIG_DIR", config)
            .env("MIO_PROVIDER", "custom")
            .env("MIO_API_KEY", "dummy-local-key")
            .args([
                "--cwd",
                cwd.to_str().unwrap(),
                "--model",
                "test",
                "--base-url",
                base_url,
                "--max-model-calls",
                "3",
                "--no-shared-limits",
                "--json",
                "--no-shell",
                "-p",
                "RESERVE-GOAL: read goal.txt twice, then answer",
            ]);
        cmd
    };
    let home = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();

    // Success: the model returns its own text answer on the narrowed final call.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("goal.txt"), "GOAL\n").unwrap();
    let server = MockServer::start(vec![
        tool_stream("call_cli_1", "read", json!({"path": "goal.txt"})),
        tool_stream("call_cli_2", "read", json!({"path": "goal.txt"})),
        text_stream("CLI-RESERVED-ANSWER"),
    ]);
    let output = cli(&server.base_url(), dir.path(), home.path(), config.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("CLI-RESERVED-ANSWER"),
        "the CLI must report the model's own answer: {stdout}"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 3, "exactly the original cap on the wire");
    for (index, request) in requests.iter().enumerate() {
        if index == 2 {
            assert_eq!(
                request.body.get("tool_choice"),
                Some(&json!("none")),
                "the CLI's final wire request must be narrowed: {}",
                request.body
            );
        } else {
            assert!(
                request.body.get("tool_choice") != Some(&json!("none")),
                "CLI call {index} must not be narrowed: {}",
                request.body
            );
        }
    }

    // Ignoring provider: the cap stops the fourth attempt and the CLI exits
    // non-zero instead of reporting a fabricated success.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("goal.txt"), "GOAL\n").unwrap();
    let server = MockServer::start(vec![
        tool_stream("call_cli_i1", "read", json!({"path": "goal.txt"})),
        tool_stream("call_cli_i2", "read", json!({"path": "goal.txt"})),
        tool_stream("call_cli_i3", "read", json!({"path": "goal.txt"})),
    ]);
    let output = cli(&server.base_url(), dir.path(), home.path(), config.path())
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "a cap-blocked run must exit non-zero: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        server.requests().len(),
        3,
        "the refused fourth call must never reach the wire"
    );
}
