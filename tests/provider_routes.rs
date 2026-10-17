//! Offline provider routing contract tests for AUD-04 / REQ-PROVIDER.
//!
//! Every test starts a local mock HTTP server and drives a real agent turn
//! through it. No external provider is contacted, and no credential is read
//! from the environment: requests carry only the fixed `local-test` token
//! this file sets on `Config`.
//!
//! The Anthropic, Google (Gemini) and Ollama presets must speak their native
//! protocols (`/v1/messages`, `/v1beta/models/…:streamGenerateContent`,
//! `/api/chat`) with native authentication and thinking parameters. The
//! OpenAI-compatible vendors and `custom` keep the Chat Completions route;
//! `custom-responses` uses the Responses route.

#[path = "../src/test_http.rs"]
mod test_http;

use mio_coding_agent::rpc::{list_catalog_models, resolve_provider_endpoint};
use mio_coding_agent::{
    AgentEvent, Config, Host, HostFuture, Message, Provider, RunOptions, RunTools, Services,
    ThinkingLevel, context_budget::ContextBudget, parse_model_spec, run_with_options,
    run_with_tools_message, subagent::SubagentManager, validate_model_thinking,
};
use mio_core::message::{DocumentSourceKind, Image, ImageMediaType, Text, UserContent};
use serde_json::{Value, json};
use std::{
    io::Write,
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Content the mock `read` tool round-trips; the second request must carry it.
const FIXTURE: &str = "provider-route-fixture";
const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+XcZUAAAAASUVORK5CYII=";

#[derive(Default)]
struct RecordingHost(Mutex<Vec<AgentEvent>>);

impl Host for RecordingHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Err("unexpected shell".into()) })
    }

    fn on_event(&self, event: AgentEvent) {
        self.0.lock().unwrap().push(event);
    }
}

#[derive(Clone, Debug)]
struct RecordedRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Value,
}

impl RecordedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    fn sse(body: String) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream",
            body: body.into_bytes(),
        }
    }

    fn ndjson(body: String) -> Self {
        Self {
            status: 200,
            content_type: "application/x-ndjson",
            body: body.into_bytes(),
        }
    }
}

/// A one-connection-at-a-time local server that replays `responses` in order.
struct MockServer {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl MockServer {
    fn start(responses: Vec<Response>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
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
                        let request = read_request(&mut socket);
                        recorded.lock().unwrap().push(request);
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
        Self { address, requests }
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

fn read_request(socket: &mut TcpStream) -> RecordedRequest {
    let (header, body) = test_http::read_json(socket);
    let mut lines = header.lines();
    let mut request_line = lines.next().unwrap_or_default().split_whitespace();
    let method = request_line.next().unwrap_or_default().to_owned();
    let path = request_line.next().unwrap_or_default().to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    RecordedRequest {
        method,
        path,
        headers,
        body,
    }
}

fn write_response(socket: &mut TcpStream, response: &Response) {
    let reason = match response.status {
        200 => "OK",
        _ => "Mock",
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        reason,
        response.content_type,
        response.body.len()
    );
    socket.write_all(head.as_bytes()).unwrap();
    socket.write_all(&response.body).unwrap();
    socket.flush().unwrap();
}

/// Drive one agent turn against a local mock. `base_suffix` is appended to the
/// mock's address so the test can prove compatibility-path normalization.
async fn drive(
    provider: Provider,
    base_suffix: &str,
    responses: Vec<Response>,
) -> (Result<Vec<Message>, String>, MockServer, Arc<RecordingHost>) {
    let server = MockServer::start(responses);
    let host = Arc::new(RecordingHost::default());
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("note.txt"), FIXTURE).unwrap();
    let result = run_with_options(
        Config {
            provider,
            model: "test".into(),
            base_url: format!("http://{}{}", server.address, base_suffix),
            api_key: "local-test".into(),
        },
        vec![],
        "Read note.txt and report the result",
        dir.path(),
        host.clone(),
        Services::default(),
        RunOptions {
            thinking: Some(ThinkingLevel::High),
            ..Default::default()
        },
    )
    .await;
    (result, server, host)
}

async fn drive_image(
    provider: Provider,
    model: &str,
    base_suffix: &str,
    response: Response,
) -> (Result<Vec<Message>, String>, MockServer) {
    let server = MockServer::start(vec![response]);
    let dir = tempfile::tempdir().unwrap();
    let prompt = Message::User {
        content: vec![
            UserContent::Text(Text::new("describe the image")),
            UserContent::Image(Image {
                data: DocumentSourceKind::Base64(PNG.into()),
                media_type: Some(ImageMediaType::PNG),
                detail: None,
                native: None,
            }),
        ],
    };
    let result = run_with_tools_message(
        Config {
            provider,
            model: model.into(),
            base_url: format!("http://{}{}", server.address, base_suffix),
            api_key: "local-test".into(),
        },
        vec![],
        prompt,
        dir.path(),
        Arc::new(RecordingHost::default()),
        Services::default(),
        RunTools {
            options: RunOptions {
                // Ollama's catalog has no context size for gemma3. Supply the
                // local mock's actual capacity instead of relaxing the image
                // reserve used by production admission.
                context_window: Some(131_072),
                ..RunOptions::default()
            },
            ..RunTools::default()
        },
    )
    .await;
    (result, server)
}

// ---- Header-free stream fixtures -------------------------------------------

fn anthropic_text_stream(text: &str) -> String {
    [
        "event: message_start".to_owned(),
        r#"data: {"message":{"id":"msg_1","model":"test","role":"assistant","type":"message","content":[],"stop_reason":null,"usage":{"input_tokens":10,"output_tokens":1}},"type":"message_start"}"#.to_owned(),
        String::new(),
        "event: content_block_start".to_owned(),
        r#"data: {"content_block":{"text":"","type":"text"},"index":0,"type":"content_block_start"}"#.to_owned(),
        String::new(),
        "event: content_block_delta".to_owned(),
        format!(r#"data: {{"delta":{{"text":"{text}","type":"text_delta"}},"index":0,"type":"content_block_delta"}}"#),
        String::new(),
        "event: content_block_stop".to_owned(),
        r#"data: {"index":0,"type":"content_block_stop"}"#.to_owned(),
        String::new(),
        "event: message_delta".to_owned(),
        r#"data: {"delta":{"stop_reason":"end_turn","stop_sequence":null},"type":"message_delta","usage":{"output_tokens":4}}"#.to_owned(),
        String::new(),
        "event: message_stop".to_owned(),
        r#"data: {"type":"message_stop"}"#.to_owned(),
        String::new(),
    ]
    .join("\n")
}

fn anthropic_tool_stream() -> String {
    [
        "event: message_start".to_owned(),
        r#"data: {"message":{"id":"msg_1","model":"test","role":"assistant","type":"message","content":[],"stop_reason":null,"usage":{"input_tokens":10,"output_tokens":1}},"type":"message_start"}"#.to_owned(),
        String::new(),
        "event: content_block_start".to_owned(),
        r#"data: {"content_block":{"id":"toolu_1","input":{},"name":"read","type":"tool_use"},"index":0,"type":"content_block_start"}"#.to_owned(),
        String::new(),
        "event: content_block_delta".to_owned(),
        r#"data: {"delta":{"partial_json":"{\"path\":\"note.txt\"}","type":"input_json_delta"},"index":0,"type":"content_block_delta"}"#.to_owned(),
        String::new(),
        "event: content_block_stop".to_owned(),
        r#"data: {"index":0,"type":"content_block_stop"}"#.to_owned(),
        String::new(),
        "event: message_delta".to_owned(),
        r#"data: {"delta":{"stop_reason":"tool_use","stop_sequence":null},"type":"message_delta","usage":{"output_tokens":5}}"#.to_owned(),
        String::new(),
        "event: message_stop".to_owned(),
        r#"data: {"type":"message_stop"}"#.to_owned(),
        String::new(),
    ]
    .join("\n")
}

fn gemini_text_stream(text: &str) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "candidates": [{
                "content": {"parts": [{"text": text}], "role": "model"},
                "finishReason": "STOP",
                "index": 0
            }],
            "modelVersion": "test",
            "responseId": "r1",
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 4, "totalTokenCount": 14}
        })
    )
}

fn gemini_tool_stream() -> String {
    format!(
        "data: {}\n\n",
        json!({
            "candidates": [{
                "content": {
                    "parts": [{"functionCall": {"name": "read", "args": {"path": "note.txt"}}}],
                    "role": "model"
                },
                "finishReason": "STOP",
                "index": 0
            }],
            "modelVersion": "test",
            "responseId": "r1",
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5, "totalTokenCount": 15}
        })
    )
}

fn ollama_text_stream(text: &str) -> String {
    format!(
        "{}\n{}\n",
        json!({"model":"test","message":{"role":"assistant","content":text},"done":false}),
        json!({"model":"test","message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","prompt_eval_count":10,"eval_count":4})
    )
}

fn ollama_tool_stream() -> String {
    format!(
        "{}\n{}\n",
        json!({
            "model": "test",
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "function": {"name": "read", "arguments": {"path": "note.txt"}}
                }]
            },
            "done": false
        }),
        json!({"model":"test","message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","prompt_eval_count":10,"eval_count":5})
    )
}

fn openai_text_stream(text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({
            "id": "chatcmpl-1",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "test",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": "stop"}]
        })
    )
}

// ---- Routing and capability unit checks ------------------------------------

#[test]
fn presets_route_to_matching_protocols() {
    for (name, expected, expected_base) in [
        (
            "anthropic",
            Provider::Anthropic,
            "https://api.anthropic.com",
        ),
        (
            "google",
            Provider::Google,
            "https://generativelanguage.googleapis.com",
        ),
        (
            "gemini",
            Provider::Google,
            "https://generativelanguage.googleapis.com",
        ),
        ("ollama", Provider::Ollama, "http://127.0.0.1:11434"),
        ("openai", Provider::Custom, "https://api.openai.com/v1"),
        (
            "openrouter",
            Provider::Custom,
            "https://openrouter.ai/api/v1",
        ),
        ("groq", Provider::Custom, "https://api.groq.com/openai/v1"),
        ("mistral", Provider::Custom, "https://api.mistral.ai/v1"),
        ("xai", Provider::Custom, "https://api.x.ai/v1"),
    ] {
        let (provider, base, _) = resolve_provider_endpoint(name, None).unwrap();
        assert_eq!(provider, expected, "provider preset `{name}`");
        assert_eq!(base, expected_base, "base url for `{name}`");
    }

    let (provider, base, _) =
        resolve_provider_endpoint("anthropic", Some("http://127.0.0.1:9/gateway".to_owned()))
            .unwrap();
    assert_eq!(provider, Provider::Anthropic);
    assert_eq!(base, "http://127.0.0.1:9/gateway");
}

#[test]
fn catalog_reports_native_base_urls() {
    let catalog = list_catalog_models(None);
    assert!(catalog.iter().any(|entry| {
        entry.provider == "anthropic"
            && entry.base_url == "https://api.anthropic.com"
            && entry.model.starts_with("claude")
    }));
    assert!(catalog.iter().any(|entry| {
        entry.provider == "google" && entry.base_url == "https://generativelanguage.googleapis.com"
    }));
    assert!(
        catalog.iter().any(|entry| {
            entry.provider == "ollama" && entry.base_url == "http://127.0.0.1:11434"
        })
    );
}

#[test]
fn provider_serde_round_trips_for_model_switching() {
    for (provider, name) in [
        (Provider::Anthropic, "anthropic"),
        (Provider::Google, "google"),
        (Provider::Ollama, "ollama"),
        (Provider::Custom, "custom"),
        (Provider::CustomResponses, "custom-responses"),
    ] {
        let encoded = serde_json::to_string(&provider).unwrap();
        assert_eq!(encoded, format!("\"{name}\""));
        assert_eq!(
            serde_json::from_str::<Provider>(&encoded).unwrap(),
            provider
        );
    }
}

#[test]
fn model_spec_shorthand_selects_native_providers() {
    for (spec, expected) in [
        ("anthropic/claude-sonnet-4-5:high", "anthropic"),
        ("google/gemini-2.5-pro:high", "google"),
        ("gemini/gemini-2.5-flash", "gemini"),
        ("ollama/qwen3", "ollama"),
    ] {
        let (provider, model, _) = parse_model_spec(spec);
        assert_eq!(provider.as_deref(), Some(expected), "{spec}");
        assert!(!model.is_empty(), "{spec}");
    }
}

#[test]
fn context_budget_reads_native_catalog_windows() {
    for (provider, model, minimum) in [
        (Provider::Anthropic, "claude-sonnet-4-5", 200_000),
        (Provider::Google, "gemini-2.5-pro", 1_000_000),
    ] {
        let budget = ContextBudget::resolve(
            &Config {
                provider,
                base_url: String::new(),
                model: model.into(),
                api_key: String::new(),
            },
            &RunOptions::default(),
        )
        .unwrap();
        assert_eq!(budget.source, "catalog", "{provider:?} {model}");
        assert!(budget.context_window >= minimum, "{provider:?} {model}");
    }
}

#[test]
fn native_thinking_validation_uses_the_matching_catalog() {
    let config = |provider, model: &str| Config {
        provider,
        base_url: String::new(),
        model: model.into(),
        api_key: String::new(),
    };
    // Budget-only models refuse an effort level locally, before any request.
    assert!(
        validate_model_thinking(
            &config(Provider::Anthropic, "claude-sonnet-4-5"),
            Some(ThinkingLevel::High),
        )
        .is_err()
    );
    assert!(
        validate_model_thinking(
            &config(Provider::Google, "gemini-2.5-pro"),
            Some(ThinkingLevel::High),
        )
        .is_err()
    );
    // Levels the catalog lists pass without a network round trip.
    assert!(
        validate_model_thinking(
            &config(Provider::Anthropic, "claude-opus-4-6"),
            Some(ThinkingLevel::High),
        )
        .is_ok()
    );
    assert!(
        validate_model_thinking(
            &config(Provider::Google, "gemini-2.5-flash"),
            Some(ThinkingLevel::Off),
        )
        .is_ok()
    );
}

#[test]
fn subagents_accept_native_providers_and_switch_models() {
    let dir = tempfile::tempdir().unwrap();
    for provider in [Provider::Anthropic, Provider::Google, Provider::Ollama] {
        let config = Config {
            provider,
            base_url: "http://127.0.0.1:1".into(),
            model: "test".into(),
            api_key: "local-test".into(),
        };
        let mut manager =
            SubagentManager::new(dir.path(), config.clone(), RunOptions::default()).unwrap();
        // Model switching must rebuild the native executor without a network call.
        manager
            .reconfigure(config, RunOptions::default(), None)
            .unwrap();
    }
}

// ---- Local HTTP contracts --------------------------------------------------

#[tokio::test]
async fn anthropic_preset_speaks_native_messages() {
    let (result, server, _) = drive(
        Provider::Anthropic,
        "/v1",
        vec![
            Response::sse(anthropic_tool_stream()),
            Response::sse(anthropic_text_stream("done")),
        ],
    )
    .await;
    result.unwrap();
    let requests = server.requests();
    assert!(
        requests.len() >= 2,
        "expected a tool round trip: {requests:?}"
    );

    let first = &requests[0];
    assert_eq!(first.method, "POST");
    assert_eq!(first.path, "/v1/messages");
    assert_eq!(first.header("x-api-key"), Some("local-test"));
    assert!(
        first
            .header("anthropic-version")
            .is_some_and(|value| !value.is_empty()),
        "anthropic-version header: {:?}",
        first.headers
    );
    assert_eq!(first.body["stream"], json!(true));
    assert!(first.body["max_tokens"].as_u64().is_some());
    assert_eq!(first.body["thinking"]["type"], "adaptive");
    assert_eq!(first.body["output_config"]["effort"], "high");
    assert!(
        first.body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "read"),
        "read tool definition: {}",
        first.body["tools"]
    );
    assert!(
        requests[1].body.to_string().contains(FIXTURE),
        "second request should carry the tool result: {}",
        requests[1].body
    );
}

#[tokio::test]
async fn google_preset_speaks_native_generate_content() {
    let (result, server, _) = drive(
        Provider::Google,
        "/v1beta/openai",
        vec![
            Response::sse(gemini_tool_stream()),
            Response::sse(gemini_text_stream("done")),
        ],
    )
    .await;
    result.unwrap();
    let requests = server.requests();
    assert!(
        requests.len() >= 2,
        "expected a tool round trip: {requests:?}"
    );

    let first = &requests[0];
    assert_eq!(first.method, "POST");
    assert!(
        first
            .path
            .starts_with("/v1beta/models/test:streamGenerateContent"),
        "native path with compatibility suffix stripped: {}",
        first.path
    );
    assert!(first.path.contains("alt=sse"), "path: {}", first.path);
    assert!(
        first.path.contains("key=local-test"),
        "Gemini authenticates by query key: {}",
        first.path
    );
    assert_eq!(
        first.body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "high"
    );
    let declarations = first.body["tools"][0]["functionDeclarations"]
        .as_array()
        .expect("functionDeclarations");
    assert!(
        declarations.iter().any(|tool| tool["name"] == "read"),
        "read tool definition: {}",
        first.body["tools"]
    );
    assert!(
        requests[1].body.to_string().contains(FIXTURE),
        "second request should carry the tool result: {}",
        requests[1].body
    );
}

#[tokio::test]
async fn ollama_preset_speaks_native_chat() {
    let (result, server, _) = drive(
        Provider::Ollama,
        "/v1",
        vec![
            Response::ndjson(ollama_tool_stream()),
            Response::ndjson(ollama_text_stream("done")),
        ],
    )
    .await;
    result.unwrap();
    let requests = server.requests();
    assert!(
        requests.len() >= 2,
        "expected a tool round trip: {requests:?}"
    );

    let first = &requests[0];
    assert_eq!(first.method, "POST");
    assert_eq!(first.path, "/api/chat");
    assert_eq!(
        first.header("authorization"),
        Some("Bearer local-test"),
        "ollama token: {:?}",
        first.headers
    );
    assert_eq!(first.body["stream"], json!(true));
    assert_eq!(first.body["think"], "high");
    assert!(
        first.body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "read"),
        "read tool definition: {}",
        first.body["tools"]
    );
    assert!(
        requests[1].body.to_string().contains(FIXTURE),
        "second request should carry the tool result: {}",
        requests[1].body
    );
}

#[tokio::test]
async fn native_image_routes_send_inline_bytes_in_their_own_wire_shapes() {
    let (result, server) = drive_image(
        Provider::Anthropic,
        "claude-sonnet-4-5",
        "/v1",
        Response::sse(anthropic_text_stream("done")),
    )
    .await;
    result.unwrap();
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/v1/messages");
    let blocks = requests[0].body["messages"][0]["content"]
        .as_array()
        .unwrap();
    let image = blocks.iter().find(|part| part["type"] == "image").unwrap();
    assert_eq!(image["source"]["type"], "base64");
    assert_eq!(image["source"]["media_type"], "image/png");
    assert_eq!(image["source"]["data"], PNG);

    let (result, server) = drive_image(
        Provider::Google,
        "gemini-2.5-pro",
        "/v1beta/openai",
        Response::sse(gemini_text_stream("done")),
    )
    .await;
    result.unwrap();
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]
            .path
            .starts_with("/v1beta/models/gemini-2.5-pro:streamGenerateContent")
    );
    let parts = requests[0].body["contents"][0]["parts"].as_array().unwrap();
    let image = parts
        .iter()
        .find(|part| part.get("inlineData").is_some())
        .unwrap();
    assert_eq!(image["inlineData"]["mimeType"], "image/png");
    assert_eq!(image["inlineData"]["data"], PNG);

    let (result, server) = drive_image(
        Provider::Ollama,
        "gemma3",
        "/v1",
        Response::ndjson(ollama_text_stream("done")),
    )
    .await;
    result.unwrap();
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/api/chat");
    let user = requests[0].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "user")
        .unwrap();
    assert_eq!(user["images"][0], PNG);
}

#[tokio::test]
async fn custom_provider_keeps_openai_chat_semantics() {
    let (result, server, _) = drive(
        Provider::Custom,
        "/v1",
        vec![Response::sse(openai_text_stream("done"))],
    )
    .await;
    result.unwrap();
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let first = &requests[0];
    assert_eq!(first.path, "/v1/chat/completions");
    assert_eq!(first.header("authorization"), Some("Bearer local-test"));
    assert_eq!(first.body["stream"], json!(true));
    assert_eq!(first.body["reasoning_effort"], "high");
    assert_eq!(first.body["model"], "test");
}

#[tokio::test]
async fn custom_responses_uses_responses_route_without_storage() {
    let (_, server, _) = drive(
        Provider::CustomResponses,
        "/v1",
        vec![Response::sse(String::new())],
    )
    .await;
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let first = &requests[0];
    assert_eq!(first.path, "/v1/responses");
    assert_eq!(first.header("authorization"), Some("Bearer local-test"));
    assert_eq!(first.body["store"], false);
    assert_eq!(first.body["stream"], true);
}
