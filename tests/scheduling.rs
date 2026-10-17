//! End-to-end acceptance tests for the scheduling admission hook at the real
//! `run_with_options` model/tool dispatch boundary.
//!
//! Every test starts a local mock HTTP server and drives real agent turns
//! through it. No external provider is contacted and no credential is read
//! from the environment: requests carry only the fixed `local-test` token this
//! file sets on [`Config`]. A single [`Scheduler`] is shared across concurrent
//! runs (and, in one test, across a parent run and a delegated child run).
//!
//! The tests assert behavior at the honest boundaries:
//!
//! * shared model concurrency queues a second run and cancellation frees the
//!   queued waiter / the active slot,
//! * model-call and total-token budgets refuse dispatch before any request is
//!   sent,
//! * reported usage settles exactly and unknown usage is charged the
//!   conservative reservation,
//! * a real 429 with `Retry-After` publishes a cooldown for that endpoint and
//!   credential only, never replays an executed tool, and does not block a
//!   different provider key,
//! * a real heavy tool (`bash`) is bounded by the heavy quota and cancellation
//!   releases the slot,
//! * `Scheduler::snapshot` / `Coordinator::snapshot` expose no key or raw
//!   endpoint,
//! * a `Scheduler` backed by a shared root enforces the cross-process model
//!   quota and isolates provider cooldowns by hash.

#[path = "../src/test_http.rs"]
mod test_http;

use mio_coding_agent::subagent::{Delegate, SubagentManager};
use mio_coding_agent::{
    AgentEvent, Config, DiagnosticCategory, Host, HostFuture, Message, Provider, RunDiagnostic,
    RunOptions, Services, coordinator, run_with_options, scheduling,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::Write,
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};
use tokio::{sync::watch, task::JoinHandle};

const API_KEY: &str = "local-test";

// ---------------------------------------------------------------------------
// Local mock HTTP server
// ---------------------------------------------------------------------------

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

#[derive(Clone)]
struct Response {
    status: u16,
    content_type: &'static str,
    extra_headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl Response {
    fn sse(body: String) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream",
            extra_headers: Vec::new(),
            body: body.into_bytes(),
        }
    }

    fn json_error(status: u16, retry_after: Option<&str>, body: &str) -> Self {
        let mut extra_headers = Vec::new();
        if let Some(value) = retry_after {
            extra_headers.push(("retry-after", value.to_owned()));
        }
        Self {
            status,
            content_type: "application/json",
            extra_headers,
            body: body.as_bytes().to_vec(),
        }
    }
}

/// One scripted reply. A held reply is not written until the test releases the
/// server gate, which lets a test prove a request reached the provider and is
/// still in flight.
#[derive(Clone)]
struct Scripted {
    response: Response,
    hold: bool,
}

impl Scripted {
    fn immediate(response: Response) -> Self {
        Self {
            response,
            hold: false,
        }
    }

    fn held(response: Response) -> Self {
        Self {
            response,
            hold: true,
        }
    }
}

struct MockState {
    requests: Mutex<Vec<RecordedRequest>>,
    script: Mutex<VecDeque<Scripted>>,
    released: Mutex<bool>,
    release_cv: Condvar,
}

struct MockServer {
    address: SocketAddr,
    state: Arc<MockState>,
}

impl MockServer {
    fn start(script: Vec<Scripted>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let state = Arc::new(MockState {
            requests: Mutex::new(Vec::new()),
            script: Mutex::new(script.into_iter().collect()),
            released: Mutex::new(false),
            release_cv: Condvar::new(),
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

    fn request_count(&self) -> usize {
        self.state.requests.lock().unwrap().len()
    }

    /// Release every held reply. Held connections whose client went away fail
    /// to write, which is ignored.
    fn release(&self) {
        *self.state.released.lock().unwrap() = true;
        self.state.release_cv.notify_all();
    }

    /// Poll until at least `count` requests have been recorded.
    async fn wait_for_requests(&self, count: usize, label: &str) {
        wait_until(
            &format!("{label}: {count} request(s)"),
            Duration::from_secs(10),
            || self.request_count() >= count,
        )
        .await;
    }
}

fn serve(mut socket: TcpStream, state: Arc<MockState>) {
    let _ = socket.set_read_timeout(Some(Duration::from_secs(15)));
    let _ = socket.set_write_timeout(Some(Duration::from_secs(15)));
    let (header, body) = test_http::read_json(&mut socket);
    let mut lines = header.lines();
    let mut request_line = lines.next().unwrap_or_default().split_whitespace();
    let method = request_line.next().unwrap_or_default().to_owned();
    let path = request_line.next().unwrap_or_default().to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    state.requests.lock().unwrap().push(RecordedRequest {
        method,
        path,
        headers,
        body,
    });
    let scripted = state.script.lock().unwrap().pop_front();
    let Some(scripted) = scripted else {
        return;
    };
    if scripted.hold {
        let mut released = state.released.lock().unwrap();
        while !*released {
            let (guard, timeout) = state
                .release_cv
                .wait_timeout(released, Duration::from_secs(20))
                .unwrap();
            released = guard;
            if timeout.timed_out() {
                break;
            }
        }
    }
    let response = &scripted.response;
    let reason = match response.status {
        200 => "OK",
        429 => "Too Many Requests",
        _ => "Mock",
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        reason,
        response.content_type,
        response.body.len()
    );
    for (name, value) in &response.extra_headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    if socket.write_all(head.as_bytes()).is_err() {
        return;
    }
    let _ = socket.write_all(&response.body);
    let _ = socket.flush();
}

// ---------------------------------------------------------------------------
// OpenAI-compatible stream fixtures
// ---------------------------------------------------------------------------

fn chunk(delta: Value, finish: Value, usage: Option<Value>) -> Value {
    let mut value = json!({
        "id": "chatcmpl-test",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "local-test",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
    });
    if let Some(usage) = usage {
        value["usage"] = usage;
    }
    value
}

fn openai_text_stream(text: &str, usage: Option<Value>) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(
            json!({"role": "assistant", "content": text}),
            Value::Null,
            None
        ),
        chunk(json!({}), json!("stop"), usage)
    )
}

fn openai_tool_stream(name: &str, args: Value, usage: Option<Value>) -> String {
    let tool_calls = json!([{
        "index": 0,
        "id": "call_1",
        "type": "function",
        "function": {"name": name, "arguments": args.to_string()}
    }]);
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(
            json!({"role": "assistant", "tool_calls": tool_calls}),
            Value::Null,
            None
        ),
        chunk(json!({}), json!("tool_calls"), usage)
    )
}

fn text_response(text: &str) -> Response {
    Response::sse(openai_text_stream(text, None))
}

fn text_response_usage(text: &str, total: u64) -> Response {
    Response::sse(openai_text_stream(
        text,
        Some(json!({
            "prompt_tokens": total,
            "completion_tokens": 0,
            "total_tokens": total
        })),
    ))
}

// ---------------------------------------------------------------------------
// Host, scheduler helpers, and polling
// ---------------------------------------------------------------------------

/// A shell gate that keeps a real tool call in flight until released.
struct ShellGate {
    released: watch::Sender<bool>,
}

impl ShellGate {
    fn new() -> Arc<Self> {
        let (released, _) = watch::channel(false);
        Arc::new(Self { released })
    }

    fn release(&self) {
        self.released.send_replace(true);
    }

    async fn wait(&self) {
        let mut receiver = self.released.subscribe();
        while !*receiver.borrow_and_update() {
            if receiver.changed().await.is_err() {
                break;
            }
        }
    }
}

struct TestHost {
    shells: Arc<Mutex<Vec<String>>>,
    diagnostics: Arc<Mutex<Vec<RunDiagnostic>>>,
    shell_gate: Option<Arc<ShellGate>>,
}

impl TestHost {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            shells: Arc::new(Mutex::new(Vec::new())),
            diagnostics: Arc::new(Mutex::new(Vec::new())),
            shell_gate: None,
        })
    }

    fn with_shell_gate(gate: Arc<ShellGate>) -> Arc<Self> {
        Arc::new(Self {
            shells: Arc::new(Mutex::new(Vec::new())),
            diagnostics: Arc::new(Mutex::new(Vec::new())),
            shell_gate: Some(gate),
        })
    }

    fn shell_count(&self) -> usize {
        self.shells.lock().unwrap().len()
    }

    fn diagnostics(&self) -> Vec<RunDiagnostic> {
        self.diagnostics.lock().unwrap().clone()
    }
}

impl Host for TestHost {
    fn shell(&self, command: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        self.shells.lock().unwrap().push(command.to_owned());
        let gate = self.shell_gate.clone();
        let command = command.to_owned();
        Box::pin(async move {
            if let Some(gate) = gate {
                gate.wait().await;
            }
            Ok(format!("ran: {command}"))
        })
    }

    fn on_event(&self, _event: AgentEvent) {}

    fn on_diagnostic(&self, diagnostic: &RunDiagnostic) {
        self.diagnostics.lock().unwrap().push(diagnostic.clone());
    }
}

fn limits(mutate: impl FnOnce(&mut coordinator::Limits)) -> coordinator::Limits {
    let mut limits = coordinator::Limits::default();
    mutate(&mut limits);
    limits
}

fn config_for(base_url: String) -> Config {
    Config {
        provider: Provider::Custom,
        base_url,
        model: "test".into(),
        api_key: API_KEY.into(),
    }
}

fn spawn_run(
    config: Config,
    dir: PathBuf,
    prompt: &str,
    host: Arc<TestHost>,
    scheduler: Arc<scheduling::Scheduler>,
) -> JoinHandle<Result<Vec<Message>, String>> {
    let prompt = prompt.to_owned();
    tokio::spawn(async move {
        run_with_options(
            config,
            Vec::new(),
            &prompt,
            &dir,
            host,
            Services::default(),
            RunOptions {
                scheduler: Some(scheduler),
                ..RunOptions::default()
            },
        )
        .await
    })
}

async fn wait_until(label: &str, timeout: Duration, condition: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if condition() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {label}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn active_model(snapshot: &Value) -> u64 {
    snapshot["model"]["active"].as_u64().unwrap()
}

fn queued_model(snapshot: &Value) -> u64 {
    snapshot["model"]["queued"].as_u64().unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Two independent runs sharing one `Scheduler` share the local model slot.
/// Holding the first request proves the second is queued (and never dispatched)
/// and that cancelling the queued waiter releases it; cancelling the active run
/// then frees the slot for a third run.
#[tokio::test]
async fn shared_scheduler_model_slot_queues_and_cancellation_releases() {
    let scheduler = Arc::new(
        scheduling::Scheduler::new(
            limits(|limits| {
                limits.model = 1;
                limits.provider_model = 1;
            }),
            None,
        )
        .unwrap(),
    );
    let server = MockServer::start(vec![
        Scripted::held(text_response("first")),
        Scripted::immediate(text_response("second")),
        Scripted::immediate(text_response("third")),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let config = config_for(server.base_url());

    let run1 = spawn_run(
        config.clone(),
        dir.path().to_path_buf(),
        "one",
        TestHost::new(),
        scheduler.clone(),
    );
    server.wait_for_requests(1, "first run dispatch").await;
    wait_until("first run active", Duration::from_secs(5), || {
        active_model(&scheduler.snapshot()) == 1
    })
    .await;

    let run2 = spawn_run(
        config.clone(),
        dir.path().to_path_buf(),
        "two",
        TestHost::new(),
        scheduler.clone(),
    );
    wait_until("second run queued", Duration::from_secs(5), || {
        queued_model(&scheduler.snapshot()) == 1
    })
    .await;
    assert_eq!(
        server.request_count(),
        1,
        "a queued run must not reach the provider"
    );

    run2.abort();
    let cancelled = run2.await.expect_err("second run was cancelled");
    assert!(cancelled.is_cancelled());
    wait_until("queued waiter released", Duration::from_secs(5), || {
        queued_model(&scheduler.snapshot()) == 0
    })
    .await;

    // The active run still owns the only slot until it is cancelled.
    run1.abort();
    let cancelled = run1.await.expect_err("first run was cancelled");
    assert!(cancelled.is_cancelled());
    wait_until("active slot released", Duration::from_secs(5), || {
        active_model(&scheduler.snapshot()) == 0
    })
    .await;

    let run3 = spawn_run(
        config,
        dir.path().to_path_buf(),
        "three",
        TestHost::new(),
        scheduler.clone(),
    );
    let result = tokio::time::timeout(Duration::from_secs(10), run3)
        .await
        .expect("third run finished")
        .expect("third run task joined");
    result.expect("third run succeeds after the slot was freed");
    assert!(
        server.request_count() >= 2,
        "third run reached the provider"
    );
    server.release();
}

/// A parent run and a delegated child run share one scheduler: the child holds
/// the only model slot, the parent queues, and killing the child releases it.
#[tokio::test]
async fn subagent_and_parent_share_one_scheduler_model_slot() {
    let scheduler = Arc::new(
        scheduling::Scheduler::new(
            limits(|limits| {
                limits.model = 1;
                limits.provider_model = 1;
            }),
            None,
        )
        .unwrap(),
    );
    let server = MockServer::start(vec![
        Scripted::held(text_response("child")),
        Scripted::immediate(text_response("parent")),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let config = config_for(server.base_url());
    let manager = SubagentManager::new(
        dir.path(),
        config.clone(),
        RunOptions {
            scheduler: Some(scheduler.clone()),
            ..RunOptions::default()
        },
    )
    .unwrap();

    let child = manager
        .delegate(
            Delegate {
                session: "child".into(),
                goal: "answer".into(),
                prompt: "hello from the child".into(),
                path: None,
                read_only: true,
            },
            TestHost::new(),
        )
        .unwrap();
    server.wait_for_requests(1, "child dispatch").await;
    wait_until("child holds the model slot", Duration::from_secs(5), || {
        active_model(&scheduler.snapshot()) == 1
    })
    .await;

    let parent = spawn_run(
        config,
        dir.path().to_path_buf(),
        "hello from the parent",
        TestHost::new(),
        scheduler.clone(),
    );
    wait_until(
        "parent queued behind the child",
        Duration::from_secs(5),
        || queued_model(&scheduler.snapshot()) == 1,
    )
    .await;
    assert_eq!(server.request_count(), 1);

    let killed = manager.kill(child.run_id).await.unwrap();
    assert!(!killed.status.is_active());

    let result = tokio::time::timeout(Duration::from_secs(10), parent)
        .await
        .expect("parent finished")
        .expect("parent task joined");
    result.expect("parent succeeds after the child released the slot");
    wait_until("slot released", Duration::from_secs(5), || {
        active_model(&scheduler.snapshot()) == 0 && queued_model(&scheduler.snapshot()) == 0
    })
    .await;
    assert!(server.request_count() >= 2);
    server.release();
}

/// The model-call budget refuses the second completion before it can be sent.
#[tokio::test]
async fn model_call_budget_blocks_before_request() {
    let scheduler = Arc::new(
        scheduling::Scheduler::new(
            limits(|limits| {
                limits.max_model_calls = Some(1);
            }),
            None,
        )
        .unwrap(),
    );
    let server = MockServer::start(vec![Scripted::immediate(Response::sse(
        openai_tool_stream("read", json!({"path": "note.txt"}), None),
    ))]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("note.txt"), "budget-fixture").unwrap();

    let host = TestHost::new();
    let result = run_with_options(
        config_for(server.base_url()),
        Vec::new(),
        "read the note",
        dir.path(),
        host.clone(),
        Services::default(),
        RunOptions {
            scheduler: Some(scheduler.clone()),
            ..RunOptions::default()
        },
    )
    .await;

    assert!(result.is_err(), "budget-exhausted run must fail");
    assert!(
        result.unwrap_err().contains("Scheduling"),
        "refusal is attributed to scheduling"
    );
    assert!(
        host.diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.category == DiagnosticCategory::Scheduling),
        "the refusal is classified as a scheduling diagnostic"
    );
    assert_eq!(
        server.request_count(),
        1,
        "the refused call must never be dispatched"
    );
    let snapshot = scheduler.snapshot();
    assert_eq!(snapshot["budget"]["calls"].as_u64(), Some(1));
    assert_eq!(snapshot["model"]["active"].as_u64(), Some(0));
    assert_eq!(snapshot["model"]["queued"].as_u64(), Some(0));
}

/// Reported usage settles exactly; unknown usage is charged the conservative
/// reservation rather than zero. The snapshot also never exposes the key or the
/// raw endpoint.
#[tokio::test]
async fn usage_settles_exactly_and_unknown_usage_is_charged() {
    let scheduler =
        Arc::new(scheduling::Scheduler::new(coordinator::Limits::default(), None).unwrap());
    let server = MockServer::start(vec![
        Scripted::immediate(text_response_usage("reported", 1_234)),
        Scripted::immediate(text_response("unknown")),
        Scripted::immediate(text_response("unknown")),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let config = config_for(server.base_url());

    run_with_options(
        config.clone(),
        Vec::new(),
        "first",
        dir.path(),
        TestHost::new(),
        Services::default(),
        RunOptions {
            scheduler: Some(scheduler.clone()),
            ..RunOptions::default()
        },
    )
    .await
    .unwrap();
    let after_reported = scheduler.snapshot();
    assert_eq!(
        after_reported["budget"]["tokensUsed"].as_u64(),
        Some(1_234),
        "reported usage settles to the reported total"
    );

    let mut deltas = Vec::new();
    for _ in 0..2 {
        let before = scheduler.snapshot()["budget"]["tokensUsed"]
            .as_u64()
            .unwrap();
        run_with_options(
            config.clone(),
            Vec::new(),
            "second",
            dir.path(),
            TestHost::new(),
            Services::default(),
            RunOptions {
                scheduler: Some(scheduler.clone()),
                ..RunOptions::default()
            },
        )
        .await
        .unwrap();
        let after = scheduler.snapshot()["budget"]["tokensUsed"]
            .as_u64()
            .unwrap();
        deltas.push(after - before);
    }
    assert!(
        deltas.iter().all(|delta| *delta > 0),
        "unknown usage must never be charged zero: {deltas:?}"
    );
    assert_eq!(
        deltas[0], deltas[1],
        "identical requests reserve the same conservative amount"
    );

    let snapshot = serde_json::to_string(&scheduler.snapshot()).unwrap();
    let coordinator_snapshot = format!("{:?}", scheduler.coordinator.snapshot());
    for rendered in [&snapshot, &coordinator_snapshot] {
        assert!(!rendered.contains(API_KEY), "snapshot leaked the key");
        assert!(
            !rendered.contains(&server.address.to_string()),
            "snapshot leaked the raw endpoint: {rendered}"
        );
    }
    // The only identifiers are opaque hashes.
    let active = scheduler.snapshot()["model"]["activeByProviderHash"].clone();
    if let Some(map) = active.as_object() {
        for key in map.keys() {
            assert_eq!(key.len(), 64, "provider identity must be a SHA-256 digest");
            assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }
    assert_eq!(server.requests().len(), 3);
    let first = &server.requests()[0];
    assert_eq!(first.method, "POST");
    assert_eq!(first.path, "/v1/chat/completions");
    assert_eq!(first.body["model"], json!("test"));
    let expected_authorization = format!("Bearer {API_KEY}");
    assert_eq!(
        first.header("authorization"),
        Some(expected_authorization.as_str())
    );
}

/// Real reported usage above the token budget blocks the next completion
/// before it is dispatched.
#[tokio::test]
async fn token_budget_blocks_after_real_usage_settlement() {
    let scheduler = Arc::new(
        scheduling::Scheduler::new(
            limits(|limits| {
                limits.max_total_tokens = Some(200_000);
            }),
            None,
        )
        .unwrap(),
    );
    let server = MockServer::start(vec![Scripted::immediate(Response::sse(
        openai_tool_stream(
            "read",
            json!({"path": "note.txt"}),
            Some(json!({
                "prompt_tokens": 500_000,
                "completion_tokens": 1,
                "total_tokens": 500_000
            })),
        ),
    ))]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("note.txt"), "budget-fixture").unwrap();

    let host = TestHost::new();
    let result = run_with_options(
        config_for(server.base_url()),
        Vec::new(),
        "read the note",
        dir.path(),
        host.clone(),
        Services::default(),
        RunOptions {
            scheduler: Some(scheduler.clone()),
            ..RunOptions::default()
        },
    )
    .await;

    assert!(result.is_err(), "over-budget run must fail");
    assert!(
        host.diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.category == DiagnosticCategory::Scheduling),
        "the refused call is classified as scheduling"
    );
    assert_eq!(
        server.request_count(),
        1,
        "the second call must be refused before dispatch"
    );
    let snapshot = scheduler.snapshot();
    assert_eq!(snapshot["budget"]["tokensUsed"].as_u64(), Some(500_000));
    assert_eq!(snapshot["budget"]["maxTokens"].as_u64(), Some(200_000));
    assert_eq!(snapshot["budget"]["blocked"].as_bool(), Some(true));
}

/// A real 429 with `Retry-After` publishes a cooldown scoped to the endpoint +
/// credential, never replays the executed tool, and leaves a different provider
/// key unblocked.
#[tokio::test]
async fn rate_limit_cools_down_one_provider_without_replay() {
    let scheduler =
        Arc::new(scheduling::Scheduler::new(coordinator::Limits::default(), None).unwrap());
    let slow = MockServer::start(vec![
        Scripted::immediate(Response::sse(openai_tool_stream(
            "bash",
            json!({"command": "echo one"}),
            None,
        ))),
        Scripted::immediate(Response::json_error(
            429,
            Some("2"),
            r#"{"error":{"message":"rate limited"}}"#,
        )),
        Scripted::immediate(text_response("recovered")),
    ]);
    let other = MockServer::start(vec![Scripted::immediate(text_response("other provider"))]);
    let dir = tempfile::tempdir().unwrap();
    let limited = config_for(slow.base_url());

    let host = TestHost::new();
    let result = run_with_options(
        limited.clone(),
        Vec::new(),
        "run a command",
        dir.path(),
        host.clone(),
        Services::default(),
        RunOptions {
            scheduler: Some(scheduler.clone()),
            ..RunOptions::default()
        },
    )
    .await;
    assert!(result.is_err(), "429 surfaces as a run failure");
    assert_eq!(
        host.shell_count(),
        1,
        "the executed tool must not be replayed after a 429"
    );
    assert_eq!(slow.request_count(), 2, "no automatic retry request");
    assert!(
        host.diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.http_status == Some(429)),
        "the run reports the 429 diagnostic"
    );

    let cooldown = scheduler.snapshot()["model"]["cooldownMs"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    assert_eq!(cooldown.len(), 1, "exactly the limited provider is cooled");
    let remaining = cooldown.values().next().unwrap().as_u64().unwrap();
    assert!(
        (1_000..=3_000).contains(&remaining),
        "Retry-After: 2 must set a ~2s cooldown, got {remaining}ms"
    );
    for key in cooldown.keys() {
        assert_eq!(key.len(), 64, "cooldown key is an opaque hash");
        assert!(!key.contains(API_KEY));
    }

    // A different provider key (different endpoint) is not blocked.
    let other_started = Instant::now();
    let other_result = run_with_options(
        config_for(other.base_url()),
        Vec::new(),
        "other",
        dir.path(),
        TestHost::new(),
        Services::default(),
        RunOptions {
            scheduler: Some(scheduler.clone()),
            ..RunOptions::default()
        },
    )
    .await;
    other_result.expect("a different provider key is not blocked");
    assert!(
        other_started.elapsed() < Duration::from_millis(1_000),
        "a different provider key must dispatch immediately"
    );
    assert_eq!(other.request_count(), 1);

    // The same provider key waits out the cooldown before its next request.
    let same_started = Instant::now();
    let same_result = run_with_options(
        limited,
        Vec::new(),
        "retry later",
        dir.path(),
        TestHost::new(),
        Services::default(),
        RunOptions {
            scheduler: Some(scheduler),
            ..RunOptions::default()
        },
    )
    .await;
    same_result.expect("the cooled provider recovers after Retry-After");
    assert!(
        same_started.elapsed() >= Duration::from_millis(1_000),
        "the same provider key must wait for the cooldown, waited {:?}",
        same_started.elapsed()
    );
    assert_eq!(slow.request_count(), 3);
}

/// A real heavy tool (`bash`) is bounded by the heavy quota; a queued heavy
/// dispatch does not execute, and cancellation releases the waiter.
#[tokio::test]
async fn heavy_tool_quota_queues_and_cancellation_releases() {
    let gate = ShellGate::new();
    let scheduler = Arc::new(
        scheduling::Scheduler::new(
            limits(|limits| {
                limits.heavy = 1;
                limits.model = 4;
                limits.provider_model = 4;
            }),
            None,
        )
        .unwrap(),
    );
    let tool_call = || {
        Scripted::immediate(Response::sse(openai_tool_stream(
            "bash",
            json!({"command": "echo heavy"}),
            None,
        )))
    };
    let server = MockServer::start(vec![
        tool_call(),
        tool_call(),
        Scripted::immediate(text_response("done")),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let config = config_for(server.base_url());

    let host1 = TestHost::with_shell_gate(gate.clone());
    let run1 = spawn_run(
        config.clone(),
        dir.path().to_path_buf(),
        "heavy one",
        host1.clone(),
        scheduler.clone(),
    );
    wait_until("first heavy tool active", Duration::from_secs(5), || {
        scheduler.snapshot()["heavy"]["active"].as_u64() == Some(1)
    })
    .await;
    assert_eq!(host1.shell_count(), 1);

    let host2 = TestHost::with_shell_gate(gate.clone());
    let run2 = spawn_run(
        config,
        dir.path().to_path_buf(),
        "heavy two",
        host2.clone(),
        scheduler.clone(),
    );
    wait_until("second heavy tool queued", Duration::from_secs(5), || {
        scheduler.snapshot()["heavy"]["queued"].as_u64() == Some(1)
    })
    .await;
    assert_eq!(
        host2.shell_count(),
        0,
        "a queued heavy tool must not execute"
    );

    run2.abort();
    assert!(
        run2.await
            .expect_err("second heavy run cancelled")
            .is_cancelled()
    );
    wait_until("heavy waiter released", Duration::from_secs(5), || {
        scheduler.snapshot()["heavy"]["queued"].as_u64() == Some(0)
    })
    .await;
    assert_eq!(
        scheduler.snapshot()["heavy"]["active"].as_u64(),
        Some(1),
        "the first heavy slot is still held"
    );

    gate.release();
    let result = tokio::time::timeout(Duration::from_secs(10), run1)
        .await
        .expect("first heavy run finished")
        .expect("first heavy task joined");
    result.expect("the heavy run completes after release");
    wait_until("heavy slot released", Duration::from_secs(5), || {
        scheduler.snapshot()["heavy"]["active"].as_u64() == Some(0)
    })
    .await;
    assert_eq!(host1.shell_count(), 1);
}

/// A `Scheduler` backed by a shared root enforces the cross-process model
/// concurrency and counts waiters; cancelling the waiter releases it.
#[tokio::test]
async fn shared_root_enforces_cross_process_model_quota() {
    let root = tempfile::tempdir().unwrap();
    let scheduler = Arc::new(
        scheduling::Scheduler::new(
            limits(|limits| {
                // Local limits are wider than the shared policy, so the shared
                // quota is the only thing that can queue a run.
                limits.model = 4;
                limits.provider_model = 4;
            }),
            Some(root.path()),
        )
        .unwrap(),
    );
    let server = MockServer::start(vec![
        Scripted::held(text_response("a")),
        Scripted::held(text_response("b")),
        Scripted::immediate(text_response("c")),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let config = config_for(server.base_url());

    let run1 = spawn_run(
        config.clone(),
        dir.path().to_path_buf(),
        "shared one",
        TestHost::new(),
        scheduler.clone(),
    );
    let run2 = spawn_run(
        config.clone(),
        dir.path().to_path_buf(),
        "shared two",
        TestHost::new(),
        scheduler.clone(),
    );
    server
        .wait_for_requests(2, "both shared runs dispatched")
        .await;
    let shared = scheduler.snapshot()["shared"].clone();
    assert_eq!(shared["modelLimit"].as_u64(), Some(2));
    assert_eq!(scheduler.snapshot()["waitingSharedModel"].as_u64(), Some(0));

    let run3 = spawn_run(
        config,
        dir.path().to_path_buf(),
        "shared three",
        TestHost::new(),
        scheduler.clone(),
    );
    wait_until(
        "third run queued on the shared quota",
        Duration::from_secs(5),
        || scheduler.snapshot()["waitingSharedModel"].as_u64() == Some(1),
    )
    .await;
    assert_eq!(
        server.request_count(),
        2,
        "the shared-quota waiter must not reach the provider"
    );

    run3.abort();
    assert!(
        run3.await
            .expect_err("shared waiter cancelled")
            .is_cancelled()
    );
    wait_until("shared waiter released", Duration::from_secs(5), || {
        scheduler.snapshot()["waitingSharedModel"].as_u64() == Some(0)
    })
    .await;

    server.release();
    for run in [run1, run2] {
        tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .expect("shared run finished")
            .expect("shared task joined")
            .expect("shared run succeeds");
    }
    let rendered = serde_json::to_string(&scheduler.snapshot()).unwrap();
    assert!(!rendered.contains(API_KEY));
    assert!(!rendered.contains(&server.address.to_string()));
}

/// In shared mode a 429 publishes a cooldown keyed by the provider hash only,
/// and a different provider key is not blocked by it.
#[tokio::test]
async fn shared_root_isolates_provider_cooldowns() {
    let root = tempfile::tempdir().unwrap();
    let scheduler = Arc::new(
        scheduling::Scheduler::new(coordinator::Limits::default(), Some(root.path())).unwrap(),
    );
    let cooled = MockServer::start(vec![Scripted::immediate(Response::json_error(
        429,
        Some("2"),
        r#"{"error":{"message":"rate limited"}}"#,
    ))]);
    let healthy = MockServer::start(vec![Scripted::immediate(text_response("healthy"))]);
    let dir = tempfile::tempdir().unwrap();

    let limited = config_for(cooled.base_url());
    let result = run_with_options(
        limited.clone(),
        Vec::new(),
        "hit the limit",
        dir.path(),
        TestHost::new(),
        Services::default(),
        RunOptions {
            scheduler: Some(scheduler.clone()),
            ..RunOptions::default()
        },
    )
    .await;
    assert!(result.is_err());

    let snapshot = scheduler.snapshot();
    let cooldowns = snapshot["shared"]["cooldowns"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(cooldowns.len(), 1, "one shared cooldown published");
    let hash = cooldowns[0]["providerHash"].as_str().unwrap();
    assert_eq!(hash.len(), 64, "shared cooldown key is a hash");
    assert!(!cooldowns[0].to_string().contains(API_KEY));

    let started = Instant::now();
    run_with_options(
        config_for(healthy.base_url()),
        Vec::new(),
        "different provider",
        dir.path(),
        TestHost::new(),
        Services::default(),
        RunOptions {
            scheduler: Some(scheduler),
            ..RunOptions::default()
        },
    )
    .await
    .expect("a different provider key is not blocked by the shared cooldown");
    assert!(
        started.elapsed() < Duration::from_millis(1_000),
        "the healthy provider must not wait on another provider's cooldown"
    );
    assert_eq!(healthy.request_count(), 1);
}

/// A shared budget seeded from local flags persists in the shared root: reported
/// usage settles into the durable ledger, a second scheduler (restart / another
/// process) inherits it, and the next completion is refused before any request
/// reaches the provider.
#[tokio::test]
async fn shared_budget_persists_across_scheduler_restart_and_blocks_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![Scripted::immediate(text_response_usage(
        "first", 1_000_000,
    ))]);
    let dir = tempfile::tempdir().unwrap();
    let config = config_for(server.base_url());
    let budget_limits = || {
        limits(|limits| {
            limits.max_total_tokens = Some(1_000_000);
        })
    };

    let first = Arc::new(
        scheduling::Scheduler::new(budget_limits(), Some(root.path())).expect("shared root"),
    );
    run_with_options(
        config.clone(),
        Vec::new(),
        "first",
        dir.path(),
        TestHost::new(),
        Services::default(),
        RunOptions {
            scheduler: Some(first.clone()),
            ..RunOptions::default()
        },
    )
    .await
    .expect("first run settles inside the budget");
    let snapshot = first.snapshot();
    assert_eq!(
        snapshot["shared"]["budget"]["maxTokens"].as_u64(),
        Some(1_000_000)
    );
    assert_eq!(
        snapshot["shared"]["budget"]["tokensUsed"].as_u64(),
        Some(1_000_000)
    );
    assert_eq!(snapshot["shared"]["budget"]["calls"].as_u64(), Some(1));
    assert_eq!(
        snapshot["shared"]["budget"]["tokensReserved"].as_u64(),
        Some(0)
    );

    // A second scheduler on the same root inherits the durable policy and usage.
    let second = Arc::new(
        scheduling::Scheduler::new(budget_limits(), Some(root.path())).expect("shared root"),
    );
    assert_eq!(
        second.snapshot()["shared"]["budget"]["tokensUsed"].as_u64(),
        Some(1_000_000),
        "settled usage must survive a scheduler restart"
    );

    let host = TestHost::new();
    let result = run_with_options(
        config,
        Vec::new(),
        "second",
        dir.path(),
        host.clone(),
        Services::default(),
        RunOptions {
            scheduler: Some(second.clone()),
            ..RunOptions::default()
        },
    )
    .await;
    assert!(
        result.is_err(),
        "the persisted budget must refuse the next call"
    );
    assert_eq!(
        server.request_count(),
        1,
        "the refused completion must never reach the provider"
    );
    assert!(
        host.diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.category == DiagnosticCategory::Scheduling),
        "the shared-budget refusal is classified as scheduling"
    );
}

/// A wider local budget never expands an already-recorded shared budget; an
/// unset local budget inherits it, and a narrower one may still narrow it.
#[tokio::test]
async fn wider_local_budget_does_not_expand_the_shared_policy() {
    let root = tempfile::tempdir().unwrap();
    assert!(
        scheduling::Scheduler::new(
            limits(|limits| {
                limits.max_total_tokens = Some(200_000);
            }),
            Some(root.path()),
        )
        .is_ok()
    );
    assert!(
        scheduling::Scheduler::new(
            limits(|limits| {
                limits.max_total_tokens = Some(500_000);
            }),
            Some(root.path()),
        )
        .is_err(),
        "a wider local budget must fail closed against the recorded shared policy"
    );
    let inherited =
        scheduling::Scheduler::new(coordinator::Limits::default(), Some(root.path())).unwrap();
    assert_eq!(
        inherited.snapshot()["shared"]["budget"]["maxTokens"].as_u64(),
        Some(200_000),
        "an unset local budget inherits the recorded shared budget"
    );
    let narrowed = scheduling::Scheduler::new(
        limits(|limits| {
            limits.max_total_tokens = Some(50_000);
        }),
        Some(root.path()),
    )
    .unwrap();
    assert_eq!(
        narrowed.snapshot()["shared"]["budget"]["maxTokens"].as_u64(),
        Some(50_000)
    );
}

/// Cancelling a run whose completion was already dispatched charges the shared
/// reservation instead of refunding it, so cancellation cannot bypass the
/// cross-process budget.
#[tokio::test]
async fn cancelled_shared_budget_reservation_is_not_refunded() {
    let root = tempfile::tempdir().unwrap();
    let scheduler = Arc::new(
        scheduling::Scheduler::new(
            limits(|limits| {
                limits.max_total_tokens = Some(10_000_000);
            }),
            Some(root.path()),
        )
        .unwrap(),
    );
    let server = MockServer::start(vec![Scripted::held(text_response("held"))]);
    let dir = tempfile::tempdir().unwrap();
    let run = spawn_run(
        config_for(server.base_url()),
        dir.path().to_path_buf(),
        "held",
        TestHost::new(),
        scheduler.clone(),
    );
    server.wait_for_requests(1, "held dispatch").await;
    wait_until("shared reservation visible", Duration::from_secs(5), || {
        scheduler.snapshot()["shared"]["budget"]["tokensReserved"]
            .as_u64()
            .unwrap_or(0)
            > 0
    })
    .await;

    run.abort();
    let _ = run.await;
    wait_until("shared reservation settled", Duration::from_secs(5), || {
        scheduler.snapshot()["shared"]["budget"]["tokensReserved"].as_u64() == Some(0)
    })
    .await;
    let used = scheduler.snapshot()["shared"]["budget"]["tokensUsed"]
        .as_u64()
        .unwrap_or(0);
    assert!(
        used > 0,
        "a cancelled in-flight reservation must be charged, not refunded"
    );
    server.release();
}

/// The same OpenAI stream shape as [`openai_tool_stream`], but with an explicit
/// call id so one test can make several distinct tool turns in a single run.
fn openai_tool_stream_with_id(id: &str, name: &str, args: Value, usage: Option<Value>) -> String {
    let tool_calls = json!( [{
        "index": 0,
        "id": id,
        "type": "function",
        "function": {"name": name, "arguments": args.to_string()}
    }]);
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(
            json!({"role": "assistant", "tool_calls": tool_calls}),
            Value::Null,
            None
        ),
        chunk(json!({}), json!("tool_calls"), usage)
    )
}

/// A finite model-call cap is visible to the model on every real outgoing
/// request: each carries a small trusted runtime note whose remaining count
/// shrinks as admitted calls are counted. The run still uses the original cap
/// and keeps its last admitted call for a successful final answer, without any
/// hidden extra call.
#[tokio::test]
async fn runtime_budget_note_tracks_the_cap_and_keeps_the_last_call_for_the_answer() {
    let cap = 3u64;
    let scheduler = Arc::new(
        scheduling::Scheduler::new(limits(|limits| limits.max_model_calls = Some(cap)), None)
            .unwrap(),
    );
    let server = MockServer::start(vec![
        Scripted::immediate(Response::sse(openai_tool_stream_with_id(
            "call_note_1",
            "read",
            json!({"path": "note.txt"}),
            None,
        ))),
        Scripted::immediate(Response::sse(openai_tool_stream_with_id(
            "call_note_2",
            "read",
            json!({"path": "note.txt"}),
            None,
        ))),
        Scripted::immediate(text_response("final answer")),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("note.txt"), "fixture").unwrap();

    let result = run_with_options(
        config_for(server.base_url()),
        Vec::new(),
        "read the note twice, then answer",
        dir.path(),
        TestHost::new(),
        Services::default(),
        RunOptions {
            scheduler: Some(scheduler.clone()),
            ..RunOptions::default()
        },
    )
    .await
    .expect("the last admitted call must still produce the final answer");
    assert!(!result.is_empty());

    let requests = server.requests();
    assert_eq!(
        requests.len(),
        cap as usize,
        "the run must use the original cap and never a hidden extra call"
    );
    let body = |index: usize| serde_json::to_string(&requests[index].body).unwrap();
    let first = body(0);
    let second = body(1);
    let third = body(2);
    for (index, rendered) in [&first, &second, &third].iter().enumerate() {
        assert!(
            rendered.contains("[mio-budget]"),
            "call {index} must carry the trusted runtime note: {rendered}"
        );
        assert_eq!(
            rendered.matches("[mio-budget]").count(),
            1,
            "call {index} must carry exactly one non-accumulated note: {rendered}"
        );
        assert!(
            rendered.contains("pre-admission snapshot incl. this call, not a guarantee"),
            "call {index} must state the snapshot is pre-admission and not a guarantee: {rendered}"
        );
        assert!(
            rendered.contains("not a guarantee or authorization"),
            "call {index} must keep the not-an-authorization caveat: {rendered}"
        );
        assert!(
            rendered.contains("concurrency can lower it."),
            "call {index} must keep the concurrency caveat: {rendered}"
        );
    }
    // The guidance is phase-aware on the real known minimum, not one generic
    // sentence: 3 keeps the mid remainder guidance, 2 asks for verification
    // only, and 1 states this snapshot's last call. The old blanket
    // "save one call to answer last" assertion is replaced by the stronger
    // per-phase assertions below; the generic wording now applies only at six
    // or more known calls left (or an unknown remainder).
    assert!(
        first.contains("calls left: local 3 (min 3)"),
        "first call must see the full cap: {first}"
    );
    assert!(
        first.contains(
            "make the needed changes from known facts; keep calls for verification and the answer"
        ),
        "first call must see the mid-remainder phase guidance: {first}"
    );
    assert!(
        !first.contains("last call in this snapshot"),
        "the first call is not the final call: {first}"
    );
    assert!(
        second.contains("calls left: local 2 (min 2)"),
        "second call must see one started call: {second}"
    );
    assert!(
        second.contains(
            "use this call for necessary verification only; answer next call and report gaps"
        ),
        "second call must see the verify-only phase guidance: {second}"
    );
    assert!(
        !second.contains("last call in this snapshot"),
        "the second call is not the final call: {second}"
    );
    assert!(
        third.contains("calls left: local 1 (min 1)"),
        "the last call must see exactly one remaining call for the answer: {third}"
    );
    assert!(
        third.contains("last call in this snapshot"),
        "the last call must see the final-call phase: {third}"
    );
    assert!(
        third.contains("state done/not-done/unverified; no tools; do not claim unverified success"),
        "the last call must be told to answer now without claiming unverified success: {third}"
    );
    assert!(
        !third.contains("save one call to answer last"),
        "the last call must not get the generic remainder guidance: {third}"
    );
    // R14: the eligible final call is the only one whose protocol-level
    // `tool_choice` is narrowed to `none`, asking the provider for the model's
    // own text answer; every earlier call keeps its provider choice. The same
    // real request still advertises every tool on every call, and the final text
    // turn still completes normally. The note never erases a tool definition.
    for (index, request) in requests.iter().enumerate() {
        let tools = request.body["tools"].as_array().unwrap_or_else(|| {
            panic!(
                "call {index} must still advertise its tool definitions: {}",
                request.body
            )
        });
        assert!(
            !tools.is_empty(),
            "call {index} must not lose its tool definitions: {}",
            request.body
        );
        if index + 1 == cap as usize {
            assert_eq!(
                request.body.get("tool_choice"),
                Some(&json!("none")),
                "the eligible final call must request the model's text answer: {}",
                request.body
            );
        } else {
            assert!(
                request.body.get("tool_choice") != Some(&json!("none")),
                "call {index} must not be narrowed before the final slot: {}",
                request.body
            );
        }
    }
    assert_eq!(
        requests[0].body["tools"], requests[1].body["tools"],
        "tool definitions must not be tightened between calls"
    );
    assert_eq!(
        requests[1].body["tools"], requests[2].body["tools"],
        "tool definitions must not be tightened between calls"
    );
    assert_eq!(
        requests[0].body.get("tool_choice"),
        requests[1].body.get("tool_choice"),
        "tool_choice must stay unchanged before the final slot"
    );
    assert!(
        serde_json::to_string(&result)
            .unwrap()
            .contains("final answer"),
        "the last admitted call must still produce its normal text answer"
    );
    assert_eq!(
        scheduler.snapshot()["budget"]["calls"].as_u64(),
        Some(cap),
        "all admitted calls are the original cap"
    );
}

/// The phase note is guidance only: a model that ignores the final-call hint
/// and asks for yet another tool turn is still stopped by the original finite
/// cap. Nothing auto-answers, retries or fabricates a success on its behalf.
#[tokio::test]
async fn runtime_budget_note_cannot_override_the_cap_when_tools_continue() {
    let cap = 3u64;
    let scheduler = Arc::new(
        scheduling::Scheduler::new(limits(|limits| limits.max_model_calls = Some(cap)), None)
            .unwrap(),
    );
    let server = MockServer::start(vec![
        Scripted::immediate(Response::sse(openai_tool_stream_with_id(
            "call_phase_1",
            "read",
            json!({"path": "note.txt"}),
            None,
        ))),
        Scripted::immediate(Response::sse(openai_tool_stream_with_id(
            "call_phase_2",
            "read",
            json!({"path": "note.txt"}),
            None,
        ))),
        // The third request already saw "last call in this snapshot" and still
        // answered with a tool call instead of the requested final text.
        Scripted::immediate(Response::sse(openai_tool_stream_with_id(
            "call_phase_3",
            "read",
            json!({"path": "note.txt"}),
            None,
        ))),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("note.txt"), "fixture").unwrap();
    let host = TestHost::new();
    let result = run_with_options(
        config_for(server.base_url()),
        Vec::new(),
        "keep reading even after the last-call hint",
        dir.path(),
        host.clone(),
        Services::default(),
        RunOptions {
            scheduler: Some(scheduler.clone()),
            ..RunOptions::default()
        },
    )
    .await;

    assert!(
        result.is_err(),
        "the original cap must stop the run instead of auto-answering"
    );
    assert!(
        result.unwrap_err().contains("Scheduling"),
        "refusal is still attributed to scheduling admission"
    );
    assert!(
        host.diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.category == DiagnosticCategory::Scheduling),
        "the refusal is classified as a scheduling diagnostic"
    );
    assert_eq!(
        server.request_count(),
        cap as usize,
        "the hint must not admit a hidden extra call"
    );
    assert_eq!(
        scheduler.snapshot()["budget"]["calls"].as_u64(),
        Some(cap),
        "the admitted calls are still exactly the original cap"
    );
    let third = serde_json::to_string(&server.requests()[2].body).unwrap();
    assert!(
        third.contains("last call in this snapshot"),
        "the final admitted call really did see the final-call guidance: {third}"
    );
    assert!(
        third.contains("no tools; do not claim unverified success"),
        "the final admitted call really did see the no-more-tools and no-unverified-success guidance: {third}"
    );
}
