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
    AgentEvent, CompletionCallObservation, Config, DiagnosticCategory, Host, HostFuture, Message,
    Provider, RunDiagnostic, RunOptions, Services, ThinkingLevel, run_with_options,
    subagent::{Delegate, Executor, Status, SubagentManager, Task},
};
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
