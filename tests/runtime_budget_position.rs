//! R13 end-to-end acceptance for the position of the trusted runtime budget
//! note at the real `run_with_options` model dispatch boundary.
//!
//! Every test starts a local mock HTTP server and drives real agent turns
//! through it. No external provider is contacted and no credential is read
//! from the environment: requests carry only the fixed `local-test` token this
//! file sets on [`Config`].
//!
//! The invariant under test: on the wire, the `[mio-budget]` system note is the
//! **last** message, after every current tool result. Before R13 the note sat
//! right after the preamble, which broke the reusable history prefix at the
//! second message on every turn. The note is still exactly one message, the
//! leading preamble stays first, every tool call/result pair stays complete and
//! adjacent, and the current user goal is preserved.

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
        "id": "chatcmpl-position",
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

fn role(message: &Value) -> &str {
    message["role"].as_str().unwrap_or_default()
}

fn text_of(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        // The OpenAI chat wire renders system content as an array of text
        // parts, so flatten those before checking the note marker.
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

/// Every model call really reaches the provider with one trusted runtime note
/// as the final wire message, after the current tool result; the leading
/// preamble stays first, the call/result pairs stay complete, and the reusable
/// history only grows. This is the genuine red/green check: the pre-R13 wire
/// carried the note at index 1, not last.
#[tokio::test]
async fn wire_note_is_the_last_message_after_each_current_tool_result() {
    let cap = 3u64;
    let scheduler = Arc::new(
        scheduling::Scheduler::new(
            coordinator::Limits {
                max_model_calls: Some(cap),
                ..coordinator::Limits::default()
            },
            None,
        )
        .unwrap(),
    );
    let server = MockServer::start(vec![
        tool_stream("call_pos_1", "read", json!({"path": "goal.txt"})),
        tool_stream("call_pos_2", "read", json!({"path": "goal.txt"})),
        text_stream("POSITION-GOAL-ANSWER"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("goal.txt"), "GOAL-KEEP-ME-VERBATIM\n").unwrap();
    let result = run_with_options(
        config_for(server.base_url()),
        Vec::new(),
        "POSITION-GOAL: read goal.txt twice, then answer",
        dir.path(),
        std::sync::Arc::new(TestHost),
        Services::default(),
        RunOptions {
            scheduler: Some(scheduler.clone()),
            ..RunOptions::default()
        },
    )
    .await
    .expect("the run must finish with its final answer");

    let requests = server.requests();
    assert_eq!(
        requests.len(),
        cap as usize,
        "the run must use exactly the admitted calls, never a hidden extra call"
    );
    let rendered = requests
        .iter()
        .map(|request| request.body.to_string())
        .collect::<Vec<_>>();

    let mut previous: Vec<Value> = Vec::new();
    for (index, request) in requests.iter().enumerate() {
        let messages = request.body["messages"]
            .as_array()
            .unwrap_or_else(|| panic!("call {index} must send messages: {}", request.body));
        assert!(!messages.is_empty(), "call {index} sent no messages");
        // R13 invariant: the note is the very last message.
        assert!(
            is_note(messages.last().unwrap()),
            "call {index}: the runtime note must be the last wire message: {}",
            rendered[index]
        );
        assert!(
            !is_note(&messages[1]),
            "call {index}: the note must no longer sit right after the preamble: {}",
            rendered[index]
        );
        // Exactly one note, ever.
        assert_eq!(
            messages.iter().filter(|message| is_note(message)).count(),
            1,
            "call {index} must carry exactly one note: {}",
            rendered[index]
        );
        // The leading preamble keeps its position.
        assert_eq!(
            role(&messages[0]),
            "system",
            "call {index} must keep its preamble first: {}",
            rendered[index]
        );
        assert!(
            !is_note(&messages[0]),
            "call {index}: the preamble is not the runtime note: {}",
            rendered[index]
        );
        // Every tool result is preceded by an assistant tool_call with the same
        // id, and no note separates the pair.
        let mut open: Vec<String> = Vec::new();
        for (position, message) in messages.iter().enumerate() {
            match role(message) {
                "assistant" => {
                    if let Some(calls) = message["tool_calls"].as_array() {
                        for call in calls {
                            let id = call["id"].as_str().unwrap_or_default().to_string();
                            assert!(
                                !open.contains(&id),
                                "call {index}: duplicate tool call {id}"
                            );
                            open.push(id);
                        }
                    }
                }
                "tool" => {
                    let id = message["tool_call_id"].as_str().unwrap_or_default();
                    let found = open
                        .iter()
                        .position(|candidate| candidate == id)
                        .unwrap_or_else(|| {
                            panic!("call {index}: orphan tool result {id}: {}", rendered[index])
                        });
                    open.remove(found);
                    assert!(
                        messages[position - 1]["role"] != "system",
                        "call {index}: a note must never sit between a call and its result"
                    );
                }
                "system" if !is_note(message) => {}
                "system" => {}
                _ => {}
            }
        }
        assert!(
            open.is_empty(),
            "call {index}: unanswered tool call {open:?}"
        );
        // The current user goal is preserved on every call.
        assert!(
            messages
                .iter()
                .any(|message| text_of(message).contains("POSITION-GOAL")),
            "call {index} must keep the current user goal: {}",
            rendered[index]
        );
        // The note counts down the original finite quota and keeps its caveats.
        let note = messages.last().unwrap();
        assert!(
            text_of(note).contains(&format!("local {}", cap - index as u64)),
            "call {index} must count down the real cap: {}",
            rendered[index]
        );
        assert!(
            text_of(note).contains("concurrency can lower it"),
            "call {index} must keep the concurrency caveat: {}",
            rendered[index]
        );
        // R14: the eligible final call is the only one whose protocol-level
        // `tool_choice` is narrowed to `none`; every earlier call keeps its
        // provider choice, while all calls still advertise the real tools.
        assert!(
            request.body["tools"]
                .as_array()
                .is_some_and(|tools| !tools.is_empty()),
            "call {index} must still advertise tools: {}",
            rendered[index]
        );
        if index + 1 == cap as usize {
            assert_eq!(
                request.body.get("tool_choice"),
                Some(&json!("none")),
                "the eligible final call must request the model's text answer: {}",
                rendered[index]
            );
        } else {
            assert!(
                request.body.get("tool_choice") != Some(&json!("none")),
                "call {index} must not be narrowed before the final slot: {}",
                rendered[index]
            );
        }
        // Removing the note leaves the canonical real messages that only grow
        // turn over turn: the relocation does not rewrite added history.
        let without_note = messages
            .iter()
            .filter(|message| !is_note(message))
            .cloned()
            .collect::<Vec<_>>();
        assert!(
            previous.len() < without_note.len(),
            "call {index}: real history must grow"
        );
        assert_eq!(
            without_note[..previous.len()],
            previous[..],
            "call {index}: the previously sent real messages must be byte-identical"
        );
        previous = without_note;
    }
    assert!(
        serde_json::to_string(&result)
            .unwrap()
            .contains("POSITION-GOAL-ANSWER"),
        "the last admitted call must still produce its normal answer"
    );
    assert_eq!(
        scheduler.snapshot()["budget"]["calls"].as_u64(),
        Some(cap),
        "all admitted calls are the original cap"
    );
}
