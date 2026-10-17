//! Offline, non-destructive product observation tests (AUD-15).
//!
//! Each test drives a real `run_with_options` turn against a local mock HTTP
//! server (or a closed local port for pre-network encoding failures). No
//! external provider is contacted and no credential is read: requests carry
//! only the fixed `local-test` token this file sets on `Config`.
//!
//! The tests assert the two `Host` observation callbacks carry real reported
//! usage and call identity, that unknown counters stay `None`, and that
//! diagnostics classify failures without leaking a provider body, URL,
//! header or source chain.

#[path = "../src/test_http.rs"]
mod test_http;

use mio_coding_agent::{
    AgentEvent, CompletionCallObservation, Config, DiagnosticCategory, ExecutionTiming,
    ExecutionTimingKind, ExecutionTimingStatus, Host, HostFuture, Message, Provider, RunDiagnostic,
    RunOptions, RunTools, Services, ThinkingLevel, run_with_options, run_with_tools,
    run_with_tools_message,
    subagent::{Delegate, Executor, Status, SubagentManager, Task},
};
use mio_core::message::ToolName;
use mio_core::message::{DocumentSourceKind, Image, ImageMediaType, UserContent};
use mio_core::tool::ToolExecutionError;
use serde_json::{Value, json};
use std::{
    io::Write,
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// A provider body must never escape into a diagnostic or the error string.
const SECRET: &str = "SECRET-LEAK-2f9c";

#[derive(Default)]
struct RecordingHost {
    completions: Mutex<Vec<CompletionCallObservation>>,
    diagnostics: Mutex<Vec<RunDiagnostic>>,
    events: Mutex<Vec<AgentEvent>>,
    timings: Mutex<Vec<ExecutionTiming>>,
}

impl Host for RecordingHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Err("unexpected shell".into()) })
    }

    fn on_event(&self, event: AgentEvent) {
        self.events.lock().unwrap().push(event);
    }

    fn on_completion_call(&self, call: &CompletionCallObservation) {
        self.completions.lock().unwrap().push(call.clone());
    }

    fn on_diagnostic(&self, diagnostic: &RunDiagnostic) {
        self.diagnostics.lock().unwrap().push(diagnostic.clone());
    }

    fn on_execution_timing(&self, timing: &ExecutionTiming) {
        self.timings.lock().unwrap().push(timing.clone());
    }
}

struct Response {
    status: u16,
    content_type: &'static str,
    headers: Vec<(&'static str, &'static str)>,
    body: Vec<u8>,
}

impl Response {
    fn sse_with_request_id(body: String, request_id: &'static str) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream",
            headers: vec![("x-request-id", request_id)],
            body: body.into_bytes(),
        }
    }

    fn json_status(status: u16, body: String) -> Self {
        Self {
            status,
            content_type: "application/json",
            headers: vec![("x-request-id", "req-error-1")],
            body: body.into_bytes(),
        }
    }
}

struct MockServer {
    address: SocketAddr,
}

impl MockServer {
    fn start(responses: Vec<Response>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut index = 0;
            while index < responses.len() && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        socket
                            .set_read_timeout(Some(Duration::from_secs(10)))
                            .unwrap();
                        socket
                            .set_write_timeout(Some(Duration::from_secs(10)))
                            .unwrap();
                        let _ = test_http::read_json(&mut socket);
                        write_response(&mut socket, &responses[index]);
                        index += 1;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("mock server accept failed: {error}"),
                }
            }
        });
        Self { address }
    }
}

fn write_response(socket: &mut TcpStream, response: &Response) {
    let reason = match response.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        408 => "Request Timeout",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Mock",
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        reason,
        response.content_type,
        response.body.len()
    );
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    socket.write_all(head.as_bytes()).unwrap();
    socket.write_all(&response.body).unwrap();
    socket.flush().unwrap();
}

/// Run one turn against a local mock and return the callbacks it produced.
async fn run_once(responses: Vec<Response>) -> (Result<Vec<Message>, String>, Arc<RecordingHost>) {
    let server = MockServer::start(responses);
    run_against(&format!("http://{}/v1", server.address)).await
}

async fn run_against(base_url: &str) -> (Result<Vec<Message>, String>, Arc<RecordingHost>) {
    let host = Arc::new(RecordingHost::default());
    let dir = tempfile::tempdir().unwrap();
    let result = run_with_options(
        Config {
            provider: Provider::Custom,
            model: "test".into(),
            base_url: base_url.to_owned(),
            api_key: "local-test".into(),
        },
        vec![],
        "hello",
        dir.path(),
        host.clone(),
        Services::default(),
        RunOptions::default(),
    )
    .await;
    (result, host)
}

fn openai_stream(id: &str, text: &str, usage: Option<(u64, u64, u64)>) -> String {
    let mut frames = vec![format!(
        "data: {}",
        json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "test",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": "stop"}]
        })
    )];
    if let Some((prompt, completion, total)) = usage {
        frames.push(format!(
            "data: {}",
            json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "test",
                "choices": [],
                "usage": {"prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": total}
            })
        ));
    }
    frames.push("data: [DONE]".to_owned());
    frames.join("\n\n") + "\n\n"
}

#[tokio::test]
async fn typed_sdk_image_reaches_the_provider_and_history() {
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+XcZUAAAAASUVORK5CYII=";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let (_, request) = test_http::read_json(&mut socket);
        let parts = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "user")
            .expect("user message")["content"]
            .as_array()
            .expect("typed parts");
        assert_eq!(parts[0]["text"], "describe image");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(
            parts[1]["image_url"]["url"],
            format!("data:image/png;base64,{PNG}")
        );
        write_response(
            &mut socket,
            &Response::sse_with_request_id(openai_stream("image-test", "a dot", None), "image-req"),
        );
    });
    let prompt = Message::User {
        content: vec![
            UserContent::text("describe image"),
            UserContent::Image(Image {
                data: DocumentSourceKind::Base64(PNG.into()),
                media_type: Some(ImageMediaType::PNG),
                detail: None,
                native: None,
            }),
        ],
    };
    let dir = tempfile::tempdir().unwrap();
    let history = run_with_tools_message(
        Config {
            provider: Provider::Custom,
            model: "gpt-4o".into(),
            base_url: format!("http://{address}/v1"),
            api_key: "local-test".into(),
        },
        vec![],
        prompt.clone(),
        dir.path(),
        Arc::new(RecordingHost::default()),
        Services::default(),
        RunTools {
            options: RunOptions {
                context_window: Some(128_000),
                ..RunOptions::default()
            },
            ..RunTools::default()
        },
    )
    .await
    .unwrap();
    server.join().unwrap();
    assert!(history.contains(&prompt));
}

/// An SSE turn that emits one or more tool calls for `tool_name`. The arguments
/// are serialized JSON, exactly as a provider streams them.
fn openai_tool_stream(id: &str, tool_name: &str, calls: &[(&str, Value)]) -> String {
    let tool_calls = calls
        .iter()
        .enumerate()
        .map(|(index, (call_id, arguments))| {
            json!({
                "index": index,
                "id": call_id,
                "type": "function",
                "function": {"name": tool_name, "arguments": arguments.to_string()}
            })
        })
        .collect::<Vec<_>>();
    let frames = [
        json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "test",
            "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": tool_calls}, "finish_reason": null}]
        })
        .to_string(),
        json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "test",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
        })
        .to_string(),
    ];
    frames
        .iter()
        .map(|frame| format!("data: {frame}"))
        .collect::<Vec<_>>()
        .join("\n\n")
        + "\n\ndata: [DONE]\n\n"
}

/// A host whose local shell sleeps, so a tool span has a measurable floor.
struct SlowToolHost {
    completions: Mutex<Vec<CompletionCallObservation>>,
    timings: Mutex<Vec<ExecutionTiming>>,
}

impl SlowToolHost {
    fn new() -> Self {
        Self {
            completions: Mutex::new(Vec::new()),
            timings: Mutex::new(Vec::new()),
        }
    }
}

impl Host for SlowToolHost {
    fn shell(&self, command: &str, _: &Path) -> HostFuture<Result<String, String>> {
        let delay = match command {
            "slow" => Duration::from_millis(60),
            "fast" => Duration::from_millis(10),
            _ => Duration::from_millis(0),
        };
        let command = command.to_owned();
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            Ok(format!("ran {command}"))
        })
    }

    fn on_event(&self, _: AgentEvent) {}

    fn on_completion_call(&self, call: &CompletionCallObservation) {
        self.completions.lock().unwrap().push(call.clone());
    }

    fn on_execution_timing(&self, timing: &ExecutionTiming) {
        self.timings.lock().unwrap().push(timing.clone());
    }
}

#[tokio::test]
async fn completion_observation_carries_reported_identity_and_usage() {
    let (result, host) = run_once(vec![Response::sse_with_request_id(
        openai_stream("chatcmpl-local", "ok", Some((11, 7, 18))),
        "req-local-1",
    )])
    .await;
    result.unwrap();

    let calls = host.completions.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.call_index, 0);
    assert!(!call.run_id.is_nil());
    assert_eq!(call.response_id.as_deref(), Some("chatcmpl-local"));
    assert_eq!(call.provider_request_id.as_deref(), Some("req-local-1"));
    assert_eq!(call.usage.input_tokens, Some(11));
    assert_eq!(call.usage.output_tokens, Some(7));
    assert_eq!(call.usage.total_tokens, Some(18));
    assert!(call.agent.is_none());
    assert!(host.diagnostics.lock().unwrap().is_empty());

    let json = serde_json::to_string(call).unwrap();
    assert!(json.contains(r#""finishReason":"stop""#), "{json}");
    assert!(!json.contains("raw"), "{json}");
    assert!(!json.contains("authorization"), "{json}");
}

#[tokio::test]
async fn unreported_usage_counters_stay_none() {
    let (result, host) = run_once(vec![Response::sse_with_request_id(
        openai_stream("chatcmpl-2", "ok", None),
        "req-local-2",
    )])
    .await;
    result.unwrap();

    let calls = host.completions.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].usage.input_tokens, None);
    assert_eq!(calls[0].usage.output_tokens, None);
    assert_eq!(calls[0].usage.total_tokens, None);
}

#[tokio::test]
async fn rate_limit_diagnostic_is_classified_and_redacted() {
    let body = json!({
        "error": {
            "message": SECRET,
            "type": "rate_limit_error",
            "url": "http://internal.example/v1/chat"
        }
    })
    .to_string();
    let (result, host) = run_once(vec![Response::json_status(429, body)]).await;
    let error = result.unwrap_err();
    assert!(!error.contains(SECRET), "{error}");

    let diagnostics = host.diagnostics.lock().unwrap();
    assert_eq!(diagnostics.len(), 1);
    let diagnostic = &diagnostics[0];
    assert_eq!(diagnostic.category, DiagnosticCategory::RateLimit);
    assert!(diagnostic.retryable);
    assert_eq!(diagnostic.http_status, Some(429));
    assert_eq!(diagnostic.message, "请求过于频繁或超出配额");
    assert!(diagnostic.agent.is_none());
    assert!(!diagnostic.run_id.is_nil());

    let json = serde_json::to_string(diagnostic).unwrap();
    for leaked in [SECRET, "internal.example", "http://", "rate_limit_error"] {
        assert!(
            !json.contains(leaked),
            "diagnostic leaked `{leaked}`: {json}"
        );
    }
}

#[tokio::test]
async fn statuses_map_to_safe_categories_and_retryability() {
    for (status, expected, retryable) in [
        (400u16, DiagnosticCategory::Request, false),
        (401, DiagnosticCategory::Authentication, false),
        (408, DiagnosticCategory::Network, true),
        (429, DiagnosticCategory::RateLimit, true),
        (500, DiagnosticCategory::Service, true),
    ] {
        let body = json!({"error": {"message": SECRET, "type": "provider_error"}}).to_string();
        let (result, host) = run_once(vec![Response::json_status(status, body)]).await;
        assert!(result.is_err(), "status {status}");
        let diagnostics = host.diagnostics.lock().unwrap();
        assert_eq!(diagnostics.len(), 1, "status {status}");
        let diagnostic = &diagnostics[0];
        assert_eq!(diagnostic.category, expected, "status {status}");
        assert_eq!(diagnostic.retryable, retryable, "status {status}");
        assert_eq!(diagnostic.http_status, Some(status), "status {status}");
        assert!(!diagnostic.message.contains(SECRET), "status {status}");
    }
}

#[tokio::test]
async fn context_budget_diagnostic_keeps_the_local_reason() {
    let host = Arc::new(RecordingHost::default());
    let dir = tempfile::tempdir().unwrap();
    let result = run_with_options(
        Config {
            provider: Provider::Custom,
            model: "test".into(),
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: "local-test".into(),
        },
        vec![],
        "hello",
        dir.path(),
        host.clone(),
        Services::default(),
        RunOptions {
            context_window: Some(1024),
            ..Default::default()
        },
    )
    .await;
    let error = result.unwrap_err();
    assert!(error.starts_with("Context budget:"), "{error}");

    let diagnostics = host.diagnostics.lock().unwrap();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].category, DiagnosticCategory::ContextBudget);
    assert!(diagnostics[0].message.starts_with("Context budget:"));
    assert!(!diagnostics[0].retryable);
    assert!(diagnostics[0].http_status.is_none());
}

#[tokio::test]
async fn unsupported_option_diagnostic_keeps_the_local_reason() {
    // Ollama's native wire rejects `xhigh` while the local catalog has no
    // entry for `test`, so the refusal happens at request encoding and the
    // closed port is never contacted.
    let host = Arc::new(RecordingHost::default());
    let dir = tempfile::tempdir().unwrap();
    let result = run_with_options(
        Config {
            provider: Provider::Ollama,
            model: "test".into(),
            base_url: "http://127.0.0.1:1".into(),
            api_key: "local-test".into(),
        },
        vec![],
        "hello",
        dir.path(),
        host.clone(),
        Services::default(),
        RunOptions {
            thinking: Some(ThinkingLevel::Xhigh),
            ..Default::default()
        },
    )
    .await;
    let error = result.unwrap_err();
    assert!(error.contains("xhigh"), "{error}");

    let diagnostics = host.diagnostics.lock().unwrap();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].category, DiagnosticCategory::Request);
    assert!(diagnostics[0].message.contains("xhigh"));
    assert!(!diagnostics[0].retryable);
    assert!(diagnostics[0].http_status.is_none());
    assert!(!diagnostics[0].message.contains("127.0.0.1"));
}

/// Emits observations from the child run's host, exactly as a real run would.
struct CallbackExecutor;

impl Executor for CallbackExecutor {
    fn execute(&self, mut task: Task) -> HostFuture<Result<Vec<Message>, String>> {
        Box::pin(async move {
            // Text must be captured by the child host, never forwarded inward.
            task.host
                .on_event(AgentEvent::Text("CHILD-ONLY-TEXT".to_owned()));
            task.host.on_completion_call(&CompletionCallObservation {
                run_id: uuid::Uuid::nil(),
                call_index: 3,
                usage: Default::default(),
                response_id: Some("child-resp".to_owned()),
                provider_request_id: None,
                finish_reason: None,
                agent: None,
            });
            task.host.on_diagnostic(&RunDiagnostic {
                run_id: uuid::Uuid::nil(),
                category: DiagnosticCategory::RateLimit,
                message: "child diagnostic".to_owned(),
                retryable: true,
                http_status: Some(429),
                agent: None,
            });
            task.history.push(Message::user(task.prompt));
            Ok(task.history)
        })
    }
}

#[tokio::test]
async fn subagent_callbacks_inherit_identity_without_leaking_text() {
    let dir = tempfile::tempdir().unwrap();
    let manager = SubagentManager::with_executor(
        dir.path(),
        Config {
            provider: Provider::Custom,
            model: "test".into(),
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: "local-test".into(),
        },
        RunOptions::default(),
        4,
        Duration::from_secs(10),
        Arc::new(CallbackExecutor),
    )
    .unwrap();
    let host = Arc::new(RecordingHost::default());
    let snapshot = manager
        .delegate(
            Delegate {
                session: "child".into(),
                goal: "goal".into(),
                prompt: "prompt".into(),
                path: None,
                read_only: true,
            },
            host.clone(),
        )
        .unwrap();
    let done = manager
        .status(snapshot.run_id, Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(done.status, Status::Completed);

    let calls = host.completions.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].run_id, snapshot.run_id);
    assert_eq!(calls[0].agent.as_deref(), Some("child"));
    assert_eq!(calls[0].call_index, 3);
    assert_eq!(calls[0].response_id.as_deref(), Some("child-resp"));

    let diagnostics = host.diagnostics.lock().unwrap();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].run_id, snapshot.run_id);
    assert_eq!(diagnostics[0].agent.as_deref(), Some("child"));

    // The child's text is captured in its snapshot, never sent to the parent.
    assert!(host.events.lock().unwrap().is_empty());
    assert!(done.output.contains("CHILD-ONLY-TEXT"));
}

#[tokio::test]
async fn diagnostic_serialization_has_no_forbidden_fields() {
    let body = json!({"error": {"message": SECRET}}).to_string();
    let (result, host) = run_once(vec![Response::json_status(500, body)]).await;
    assert!(result.is_err());
    let diagnostic = &host.diagnostics.lock().unwrap()[0];
    let value: Value = serde_json::to_value(diagnostic).unwrap();
    let object = value.as_object().unwrap();
    for forbidden in ["raw", "body", "headers", "sourceChain", "url", "apiKey"] {
        assert!(!object.contains_key(forbidden), "{forbidden}");
    }
}

/// One run, two same-named tool calls with different spans, then a final turn.
#[tokio::test]
async fn execution_timing_measures_model_and_tool_spans() {
    let server = MockServer::start(vec![
        Response::sse_with_request_id(
            openai_tool_stream(
                "chatcmpl-tools",
                "workspace",
                &[
                    (
                        "call_slow",
                        json!({"operation": "shell", "path": "", "value": "slow"}),
                    ),
                    (
                        "call_fast",
                        json!({"operation": "shell", "path": "", "value": "fast"}),
                    ),
                ],
            ),
            "req-tools-1",
        ),
        Response::sse_with_request_id(openai_stream("chatcmpl-done", "ok", None), "req-tools-2"),
    ]);
    let host = Arc::new(SlowToolHost::new());
    let dir = tempfile::tempdir().unwrap();
    let result = run_with_options(
        Config {
            provider: Provider::Custom,
            model: "test".into(),
            base_url: format!("http://{}/v1", server.address),
            api_key: "local-test".into(),
        },
        vec![],
        "run two commands",
        dir.path(),
        host.clone(),
        Services::default(),
        RunOptions::default(),
    )
    .await;
    assert!(result.is_ok(), "{result:?}");

    let timings = host.timings.lock().unwrap().clone();
    let completions = host.completions.lock().unwrap().clone();
    assert_eq!(completions.len(), 2, "{completions:?}");
    let run_id = completions[0].run_id;
    assert!(completions.iter().all(|call| call.run_id == run_id));

    let model: Vec<_> = timings
        .iter()
        .filter(|timing| timing.kind == ExecutionTimingKind::Model)
        .collect();
    let tool: Vec<_> = timings
        .iter()
        .filter(|timing| timing.kind == ExecutionTimingKind::Tool)
        .collect();
    assert_eq!(model.len(), 2, "{timings:?}");
    assert_eq!(tool.len(), 2, "{timings:?}");
    // Every observation carries this run's UUID, shared with the callbacks.
    assert!(
        timings.iter().all(|timing| timing.run_id == run_id),
        "{timings:?}"
    );
    assert!(
        model
            .iter()
            .all(|timing| timing.status == ExecutionTimingStatus::Success)
    );
    assert!(
        tool.iter()
            .all(|timing| timing.status == ExecutionTimingStatus::Success)
    );

    // Effect ids are the dispatcher's: four dispatches, four distinct ids. Two
    // calls of the same tool stay separate because the hook keys by effect id,
    // never by tool name.
    let mut ids: Vec<u64> = timings.iter().map(|timing| timing.effect_id).collect();
    assert!(ids.iter().all(|id| *id != 0), "{timings:?}");
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 4, "{timings:?}");

    // Spans are non-negative and cover the tool's own sleep: the 60 ms call
    // and the 10 ms call are not conflated or added together.
    let mut tool_ms: Vec<u64> = tool.iter().map(|timing| timing.duration_ms).collect();
    tool_ms.sort_unstable();
    // Loaded CI machines may delay either call; avoid an upper wall-time bound.
    assert!(tool_ms[0] >= 5, "{timings:?}");
    assert!(tool_ms[1] >= 50, "{timings:?}");

    // No argument, tool name, credential or workspace path is part of an
    // observation.
    let json = serde_json::to_string(&timings).unwrap();
    for leaked in [
        SECRET,
        "slow",
        "fast",
        "workspace",
        "local-test",
        dir.path().to_str().unwrap(),
    ] {
        assert!(!json.contains(leaked), "timing leaked `{leaked}`: {json}");
    }
}

#[tokio::test]
async fn failed_model_call_emits_no_execution_timing() {
    let body = json!({"error": {"message": SECRET}}).to_string();
    let (result, host) = run_once(vec![Response::json_status(500, body)]).await;
    assert!(result.is_err());
    // The completion never reached an outcome, so nothing is fabricated.
    assert!(host.timings.lock().unwrap().is_empty());
    assert_eq!(host.diagnostics.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn failed_tool_outcome_is_classified_as_error() {
    // A dynamic tool whose handler answers with an error: the engine turns it
    // into a failed `ToolResult`, so the measured status must be `error` while
    // the surrounding model calls stay `success`.
    let failing = mio_agent::tool::DynamicTool::new(
        ToolName::new("fixture_fail").expect("tool name"),
        "always fails",
        json!({"type": "object"}),
        |_| Box::pin(async { Err(ToolExecutionError::other("fixture failure")) }),
    );
    let server = MockServer::start(vec![
        Response::sse_with_request_id(
            openai_tool_stream("chatcmpl-fail", "fixture_fail", &[("call_fail", json!({}))]),
            "req-fail-1",
        ),
        Response::sse_with_request_id(
            openai_stream("chatcmpl-recovered", "recovered", None),
            "req-fail-2",
        ),
    ]);
    let host = Arc::new(RecordingHost::default());
    let dir = tempfile::tempdir().unwrap();
    let result = run_with_tools(
        Config {
            provider: Provider::Custom,
            model: "test".into(),
            base_url: format!("http://{}/v1", server.address),
            api_key: "local-test".into(),
        },
        vec![],
        "fail a tool",
        dir.path(),
        host.clone(),
        Services::default(),
        RunTools {
            extra_tools: vec![failing],
            ..RunTools::default()
        },
    )
    .await;
    assert!(result.is_ok(), "{result:?}");

    let timings = host.timings.lock().unwrap().clone();
    let tool: Vec<_> = timings
        .iter()
        .filter(|timing| timing.kind == ExecutionTimingKind::Tool)
        .collect();
    assert_eq!(tool.len(), 1, "{timings:?}");
    assert_eq!(tool[0].status, ExecutionTimingStatus::Error);
    assert!(
        timings
            .iter()
            .filter(|timing| timing.kind == ExecutionTimingKind::Model)
            .all(|timing| timing.status == ExecutionTimingStatus::Success),
        "{timings:?}"
    );
}

#[test]
fn execution_timing_serialization_is_exactly_scoped() {
    let timing = ExecutionTiming {
        run_id: uuid::Uuid::nil(),
        effect_id: 42,
        kind: ExecutionTimingKind::Tool,
        status: ExecutionTimingStatus::Error,
        duration_ms: 7,
    };
    let value = serde_json::to_value(&timing).unwrap();
    let object = value.as_object().unwrap();
    let keys: Vec<&str> = object.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        ["runId", "effectId", "kind", "status", "durationMs"],
        "field order and set are stable"
    );
    assert_eq!(value["kind"], json!("tool"));
    assert_eq!(value["status"], json!("error"));
    assert_eq!(value["durationMs"], json!(7));
    assert_eq!(value["effectId"], json!(42));
    assert_eq!(
        value["runId"],
        json!("00000000-0000-0000-0000-000000000000")
    );
    for forbidden in [
        "args",
        "arguments",
        "target",
        "reply",
        "raw",
        "body",
        "headers",
        "url",
        "apiKey",
        "api_key",
        "sourceChain",
    ] {
        assert!(!object.contains_key(forbidden), "{forbidden}");
    }
    let roundtrip: ExecutionTiming = serde_json::from_value(value).unwrap();
    assert_eq!(roundtrip, timing);

    let model = serde_json::to_value(ExecutionTiming {
        kind: ExecutionTimingKind::Model,
        status: ExecutionTimingStatus::Success,
        ..timing
    })
    .unwrap();
    assert_eq!(model["kind"], json!("model"));
    assert_eq!(model["status"], json!("success"));
}
