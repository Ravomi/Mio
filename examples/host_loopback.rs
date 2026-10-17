//! Complete, offline-runnable SDK host example.
//!
//! The example drives the public Rust SDK entry point
//! [`run_with_tools_tracked`](mio_coding_agent::run_with_tools_tracked) against a
//! loopback OpenAI-compatible mock and shows the real host contract end to end:
//!
//! * a real canonical [`TaskHandle`] the example itself registers and,
//!   for the cancel scenario, explicitly settles (`request_cancel` ->
//!   `finish(Cancelled)` -> `wait_settled`);
//! * a real parameter-bound approval: the host approves exactly one
//!   `write` bound to `result.txt` and refuses everything else;
//! * canonical tool-call lifecycle records written through the public
//!   `Journal::record_canonical_phase` API from `Host::on_tool_call_lifecycle`;
//! * real engine run scopes and task phases projected with
//!   `Journal::record_run_observation` / `Journal::project_task_canonical`;
//! * a real read whose returned text is the only source of the model-mocked
//!   write, plus a non-retryable failure path and a real cancellation while an
//!   HTTP request is in flight.
//!
//! Honest boundaries:
//!
//! * It never reads an environment credential. `Config::api_key` is a fixed
//!   local test token, the mock binds `127.0.0.1:0` only, and no request header
//!   is ever printed or persisted.
//! * It never spawns the CLI or a shell: the tool filter is exactly
//!   `read`/`write` (`.without_shell()`), and `Host::shell` only counts calls
//!   and fails.
//! * It does not treat the model's success text as acceptance; the caller/tests
//!   verify the real file, the real journal and the real ids.
//! * Unknown side effects are refused by
//!   `Host::confirm_unknown_side_effect`; the ordinary approval path is never
//!   reused as an unknown-effect confirmation.
//! * This is a host example, not the CLI: it reuses no CLI-private execution
//!   semantics and adds no GUI/HTTP product surface.
//!
//! Run it with:
//!
//! ```text
//! cargo run --example host_loopback -- --scenario success|model-error|cancel
//! ```
//!
//! The example prints exactly one JSON evidence summary on stdout (stderr is
//! reserved for warnings), and every temporary workspace is removed on drop.

use mio_coding_agent::{
    AgentEvent, CompletionCallObservation, Config, Host, HostFuture, Provider, ReadObservation,
    RunDiagnostic, RunObservation, RunObservationKind, RunOptions, RunTools, Services, ToolFilter,
    coordinator::Limits,
    host_task_sink,
    journal::{Journal, JournalFile, RunState},
    policy::{ApprovalBinding, NormalizedCall},
    run_with_tools_tracked,
    scheduling::Scheduler,
    task_lifecycle::{Settlement, TaskHandle, TaskId, TaskLifecycleTracker, TaskPhase, TaskRecord},
    tool_lifecycle::ToolCallRecord,
    unknown_effect::UnknownSideEffect,
};
use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Fixed, offline fixtures
// ---------------------------------------------------------------------------

/// Seed file the model is told to read.
pub const SEED_FILE: &str = "seed.txt";
/// File the host is willing to approve a write to.
pub const RESULT_FILE: &str = "result.txt";
/// Exact seed contents. A single line, so a line read returns it verbatim.
pub const SEED_TEXT: &str = "seed: the read result must be the only source of the write";
/// Fixed session identity the host reports to the engine.
pub const SESSION_ID: &str = "host-loopback-example-session";
/// Fixed local credential; never read from the environment.
pub const FIXED_CREDENTIAL: &str = "local-test-credential";
/// Model name used only by the loopback mock.
pub const MOCK_MODEL: &str = "local-test-model";
/// Hard model-call budget enforced by the real `Scheduler`.
pub const MODEL_REQUEST_LIMIT: u64 = 3;
/// Model tool-call id of the read in every scenario.
pub const READ_CALL_ID: &str = "call-read-1";
/// Model tool-call id of the write in the success scenario.
pub const WRITE_CALL_ID: &str = "call-write-1";
/// Final assistant text of the success scenario.
pub const FINAL_TEXT: &str = "final: derived write committed";
/// Provider body used by the model-error mock. Must never leak into any
/// diagnostic, error string or durable journal.
pub const MOCK_SECRET_BODY: &str = "mock-provider-body-must-not-leak";

const RUN_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const SERVER_DEADLINE: Duration = Duration::from_secs(60);
const JOIN_GRACE: Duration = Duration::from_secs(2);
const OBSERVE_GRACE: Duration = Duration::from_millis(150);
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 1024 * 1024;

const HELP: &str = "\
host_loopback: offline SDK host example (loopback model; no external API keys)

Usage:
  cargo run --example host_loopback -- [--scenario success|model-error|cancel] [--help]

Scenarios:
  success      real read -> model-derived write -> final answer (3 model requests)
  model-error  real read, then a non-retryable HTTP 400 and no write (2 requests)
  cancel       drop the run future while its HTTP request is in flight, then
               request_cancel + finish(Cancelled) + wait_settled (1 request)

The example binds 127.0.0.1 only, writes inside a private temporary workspace,
uses a fixed local credential, and prints one JSON evidence summary on stdout.
";

// ---------------------------------------------------------------------------
// Scenario
// ---------------------------------------------------------------------------

/// The three offline scenarios the example and its tests exercise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    /// Real read, then a write derived from the real read result, then a final
    /// answer: exactly three model requests.
    Success,
    /// Real read, then a non-retryable HTTP 400 from the mock: no write.
    ModelError,
    /// The run future is dropped while its real HTTP request is in flight.
    Cancel,
}

impl Scenario {
    /// Parse a `--scenario` value.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "success" => Ok(Self::Success),
            "model-error" => Ok(Self::ModelError),
            "cancel" => Ok(Self::Cancel),
            other => Err(format!(
                "unknown scenario '{other}'; expected success|model-error|cancel"
            )),
        }
    }

    /// Stable label used in the evidence summary.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::ModelError => "model-error",
            Self::Cancel => "cancel",
        }
    }

    /// Authoritative task phase expected after the scenario settles.
    pub const fn expected_phase(self) -> TaskPhase {
        match self {
            Self::Success => TaskPhase::Completed,
            Self::ModelError => TaskPhase::Failed,
            Self::Cancel => TaskPhase::Cancelled,
        }
    }

    fn prompt(self) -> &'static str {
        match self {
            Self::Success => "Read seed.txt, write the derived result to result.txt, then answer.",
            Self::ModelError => "Read seed.txt, then write the derived result to result.txt.",
            Self::Cancel => "Stream a long answer and keep streaming.",
        }
    }
}

// ---------------------------------------------------------------------------
// Hashing / derivation
// ---------------------------------------------------------------------------

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Deterministic content the model mock derives from the *actual* read result.
///
/// The derivation is a pure function of the returned text, so a test can
/// recompute the expectation from the seed without trusting the mock.
pub fn derive_write_content(read_output: &str) -> String {
    let trimmed = read_output.trim_end_matches(['\r', '\n']);
    let digest = sha256_hex(trimmed.as_bytes());
    format!("derived-from-read:{}:{trimmed}", &digest[..16])
}

// ---------------------------------------------------------------------------
// Loopback mock
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MockState {
    requests: AtomicUsize,
    read_output: Mutex<Option<String>>,
    derived_write: Mutex<Option<String>>,
}

struct MockServer {
    address: SocketAddr,
    state: Arc<MockState>,
    requests_rx: tokio::sync::mpsc::UnboundedReceiver<()>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockServer {
    /// Start a bounded loopback mock for `scenario`.
    fn start(scenario: Scenario) -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|error| format!("loopback mock bind failed: {error}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("loopback mock nonblocking failed: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("loopback mock address failed: {error}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(MockState::default());
        let (tx, requests_rx) = tokio::sync::mpsc::unbounded_channel();
        let thread_stop = Arc::clone(&stop);
        let thread_state = Arc::clone(&state);
        let handle = std::thread::spawn(move || {
            let deadline = Instant::now() + SERVER_DEADLINE;
            while !thread_stop.load(Ordering::SeqCst) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        let _ = socket.set_read_timeout(Some(REQUEST_TIMEOUT));
                        let _ = socket.set_write_timeout(Some(REQUEST_TIMEOUT));
                        let Some(request) = read_json_request(&mut socket) else {
                            continue;
                        };
                        thread_state.requests.fetch_add(1, Ordering::SeqCst);
                        let _ = tx.send(());
                        respond(scenario, &thread_state, &mut socket, &request);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => return,
                }
            }
        });
        Ok(Self {
            address,
            state,
            requests_rx,
            stop,
            handle: Some(handle),
        })
    }

    fn request_count(&self) -> usize {
        self.state.requests.load(Ordering::SeqCst)
    }

    fn read_output(&self) -> Option<String> {
        self.state.read_output.lock().unwrap().clone()
    }

    fn derived_write(&self) -> Option<String> {
        self.state.derived_write.lock().unwrap().clone()
    }

    /// Wait until the mock has really accepted one request. `false` means the
    /// server thread ended first.
    async fn next_request(&mut self) -> bool {
        self.requests_rx.recv().await.is_some()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let Some(handle) = self.handle.take() else {
            return;
        };
        let deadline = Instant::now() + JOIN_GRACE;
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if handle.is_finished() {
            let _ = handle.join();
        }
        // Otherwise the detached thread ends on its own bounded deadline; Drop
        // never blocks on a stuck handler.
    }
}

fn respond(scenario: Scenario, state: &MockState, socket: &mut TcpStream, request: &Value) {
    match scenario {
        Scenario::Success => match assistant_turn(request) {
            0 => {
                let delta = json!({
                    "role": "assistant",
                    "tool_calls": [tool_call(0, "read", READ_CALL_ID, json!({"path": SEED_FILE}))],
                });
                write_sse(socket, delta, json!("tool_calls"));
            }
            1 => {
                // The write content is derived from the real read result the
                // engine sent back, never hardcoded here.
                let read_output = read_output_from_request(request).unwrap_or_default();
                let derived = derive_write_content(&read_output);
                *state.read_output.lock().unwrap() = Some(read_output);
                *state.derived_write.lock().unwrap() = Some(derived.clone());
                let delta = json!({
                    "role": "assistant",
                    "tool_calls": [tool_call(
                        0,
                        "write",
                        WRITE_CALL_ID,
                        json!({"path": RESULT_FILE, "content": derived}),
                    )],
                });
                write_sse(socket, delta, json!("tool_calls"));
            }
            _ => write_text(socket, FINAL_TEXT),
        },
        Scenario::ModelError => {
            if assistant_turn(request) == 0 {
                let delta = json!({
                    "role": "assistant",
                    "tool_calls": [tool_call(0, "read", READ_CALL_ID, json!({"path": SEED_FILE}))],
                });
                write_sse(socket, delta, json!("tool_calls"));
            } else {
                // The read result really reached the mock; capture it before
                // the non-retryable failure so the evidence can show it.
                if let Some(read_output) = read_output_from_request(request) {
                    *state.read_output.lock().unwrap() = Some(read_output);
                }
                // A real, non-retryable client error.
                write_status(
                    socket,
                    400,
                    &json!({"error": {"message": MOCK_SECRET_BODY}}).to_string(),
                );
            }
        }
        Scenario::Cancel => stream_forever(socket),
    }
}

fn assistant_turn(request: &Value) -> usize {
    request["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .filter(|message| message["role"] == "assistant")
                .count()
        })
        .unwrap_or(0)
}

fn tool_result_texts(request: &Value) -> Vec<String> {
    let mut texts = Vec::new();
    let Some(messages) = request["messages"].as_array() else {
        return texts;
    };
    for message in messages {
        if message["role"] != "tool" {
            continue;
        }
        match &message["content"] {
            Value::String(text) => texts.push(text.clone()),
            Value::Array(parts) => {
                for part in parts {
                    if let Some(text) = part["text"].as_str() {
                        texts.push(text.to_owned());
                    }
                }
            }
            _ => {}
        }
    }
    texts
}

fn read_output_from_request(request: &Value) -> Option<String> {
    tool_result_texts(request)
        .into_iter()
        .rev()
        .find_map(|text| {
            serde_json::from_str::<Value>(&text).ok().and_then(|value| {
                value
                    .get("output")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
        })
}

fn tool_call(index: usize, name: &str, id: &str, arguments: Value) -> Value {
    json!({
        "index": index,
        "id": id,
        "type": "function",
        "function": {"name": name, "arguments": arguments.to_string()},
    })
}

fn sse_chunk(delta: Value, finish: Value) -> String {
    json!({
        "id": "host-loopback-mock",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": MOCK_MODEL,
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

fn write_status(socket: &mut TcpStream, status: u16, body: &str) {
    let reason = match status {
        400 => "Bad Request",
        _ => "Mock",
    };
    let _ = write!(
        socket,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.flush();
}

/// Stream chunks until the client drops the connection (bounded by the
/// socket write timeout and the server deadline).
fn stream_forever(socket: &mut TcpStream) {
    let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
    if socket.write_all(headers.as_bytes()).is_err() {
        return;
    }
    let text = "cancel-scenario-stream".repeat(64);
    loop {
        let frame_body = format!(
            "data: {}\n\n",
            sse_chunk(json!({"role": "assistant", "content": text}), Value::Null)
        );
        let frame = format!("{:x}\r\n{frame_body}\r\n", frame_body.len());
        if socket
            .write_all(frame.as_bytes())
            .and_then(|()| socket.flush())
            .is_err()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn read_json_request(socket: &mut TcpStream) -> Option<Value> {
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    while !header.ends_with(b"\r\n\r\n") {
        if header.len() > MAX_HEADER_BYTES {
            return None;
        }
        if socket.read_exact(&mut byte).is_err() {
            return None;
        }
        header.push(byte[0]);
    }
    let header_text = String::from_utf8(header).ok()?;
    let length = header_text
        .lines()
        .find_map(|line| {
            let lower = line.to_ascii_lowercase();
            lower
                .strip_prefix("content-length:")
                .map(|value| value.trim().to_owned())
        })
        .and_then(|value| value.parse::<usize>().ok())?;
    if length > MAX_BODY_BYTES {
        return None;
    }
    let mut body = vec![0u8; length];
    socket.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

// ---------------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------------

/// One recorded approval decision and the exact binding it was asked about.
#[derive(Clone, Debug, Serialize)]
pub struct ApprovalRecord {
    /// Whether the host approved the call.
    pub approved: bool,
    /// Approval operation label.
    pub operation: String,
    /// Human-readable target the engine displayed.
    pub target: String,
    /// Full parameter-bound approval binding.
    pub binding: ApprovalBinding,
}

/// The example host. It persists through the public journal APIs only; it
/// performs no I/O of its own beyond its in-memory record vectors.
pub struct ExampleHost {
    journal: Arc<Journal>,
    cwd: PathBuf,
    session: String,
    approvals: Mutex<Vec<ApprovalRecord>>,
    events: Mutex<Vec<AgentEvent>>,
    read_observations: Mutex<Vec<ReadObservation>>,
    canonical_calls: Mutex<Vec<ToolCallRecord>>,
    run_observations: Mutex<Vec<RunObservation>>,
    completions: Mutex<Vec<CompletionCallObservation>>,
    diagnostics: Mutex<Vec<RunDiagnostic>>,
    task_records: Mutex<Vec<TaskRecord>>,
    unknowns: Mutex<Vec<UnknownSideEffect>>,
    unknown_confirmations_refused: AtomicUsize,
    shell_calls: AtomicUsize,
}

impl ExampleHost {
    /// Build a host that projects task/run/tool evidence into `journal`.
    pub fn new(journal: Arc<Journal>, cwd: PathBuf) -> Self {
        Self {
            journal,
            cwd,
            session: SESSION_ID.to_owned(),
            approvals: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
            read_observations: Mutex::new(Vec::new()),
            canonical_calls: Mutex::new(Vec::new()),
            run_observations: Mutex::new(Vec::new()),
            completions: Mutex::new(Vec::new()),
            diagnostics: Mutex::new(Vec::new()),
            task_records: Mutex::new(Vec::new()),
            unknowns: Mutex::new(Vec::new()),
            unknown_confirmations_refused: AtomicUsize::new(0),
            shell_calls: AtomicUsize::new(0),
        }
    }

    /// Canonical workspace root the host was built for.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Every approval decision the engine asked for, in order.
    pub fn approvals(&self) -> Vec<ApprovalRecord> {
        self.approvals.lock().unwrap().clone()
    }

    /// Real agent events the engine emitted (`Text` for this example).
    pub fn events(&self) -> Vec<AgentEvent> {
        self.events.lock().unwrap().clone()
    }

    /// Real successful-read observations.
    pub fn read_observations(&self) -> Vec<ReadObservation> {
        self.read_observations.lock().unwrap().clone()
    }

    /// Every canonical tool-call lifecycle record the engine emitted.
    pub fn canonical_calls(&self) -> Vec<ToolCallRecord> {
        self.canonical_calls.lock().unwrap().clone()
    }

    /// Real bounded run observations.
    pub fn run_observations(&self) -> Vec<RunObservation> {
        self.run_observations.lock().unwrap().clone()
    }

    /// Completed provider calls.
    pub fn completions(&self) -> Vec<CompletionCallObservation> {
        self.completions.lock().unwrap().clone()
    }

    /// Classified failures.
    pub fn diagnostics(&self) -> Vec<RunDiagnostic> {
        self.diagnostics.lock().unwrap().clone()
    }

    /// Every canonical task transition the tracker projected.
    pub fn task_records(&self) -> Vec<TaskRecord> {
        self.task_records.lock().unwrap().clone()
    }

    /// Unknown side effects the engine recorded.
    pub fn unknowns(&self) -> Vec<UnknownSideEffect> {
        self.unknowns.lock().unwrap().clone()
    }

    /// How many unknown-effect confirmations the engine asked for and the host
    /// refused.
    pub fn unknown_confirmations_refused(&self) -> usize {
        self.unknown_confirmations_refused.load(Ordering::SeqCst)
    }

    /// How many times the engine asked the host to run a shell.
    pub fn shell_calls(&self) -> usize {
        self.shell_calls.load(Ordering::SeqCst)
    }
}

impl Host for ExampleHost {
    fn shell(&self, _command: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        self.shell_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err("host_loopback provides no shell; only read/write file tools are bound".to_string())
        })
    }

    fn on_event(&self, event: AgentEvent) {
        self.events.lock().unwrap().push(event);
    }

    fn session_id(&self) -> Option<String> {
        Some(self.session.clone())
    }

    /// Project the authoritative task phase onto the journal run that owns the
    /// real canonical task id.
    fn on_task_lifecycle(&self, record: &TaskRecord) -> Result<(), String> {
        self.task_records.lock().unwrap().push(record.clone());
        self.journal.project_task_canonical(
            record.task_id.as_str(),
            record.engine_run_id.as_ref().map(|id| id.as_str()),
            record.phase.as_str(),
        )
    }

    /// Persist the real run scope binding and observe scope lifecycle.
    fn on_run_observation(&self, observation: &RunObservation) {
        self.run_observations
            .lock()
            .unwrap()
            .push(observation.clone());
        if let Err(error) = self.journal.record_run_observation(observation) {
            eprintln!("host_loopback: journal run observation error: {error}");
        }
    }

    /// Persist the authoritative tool-call phase (and its set-once binding
    /// digest / real ids) through the public journal API.
    fn on_tool_call_lifecycle(&self, call: &ToolCallRecord) -> Result<(), String> {
        self.canonical_calls.lock().unwrap().push(call.clone());
        self.journal.record_canonical_phase(call)
    }

    fn on_read_observation(&self, observation: &ReadObservation) {
        self.read_observations
            .lock()
            .unwrap()
            .push(observation.clone());
    }

    fn on_completion_call(&self, call: &CompletionCallObservation) {
        self.completions.lock().unwrap().push(call.clone());
    }

    fn on_diagnostic(&self, diagnostic: &RunDiagnostic) {
        self.diagnostics.lock().unwrap().push(diagnostic.clone());
    }

    /// Approve exactly one parameter-bound `write` to `result.txt`; refuse any
    /// other operation, path, workspace or binding shape.
    fn approve_tool_binding(
        &self,
        operation: &str,
        target: &str,
        binding: &ApprovalBinding,
    ) -> HostFuture<Result<(), String>> {
        let operation = operation.to_owned();
        let target = target.to_owned();
        let approved = operation == "write"
            && target == RESULT_FILE
            && binding.tool == "write"
            && binding.operation == operation
            && Path::new(&binding.cwd) == self.cwd.as_path()
            && !binding.scope.is_empty()
            && !binding.params_sha256.is_empty()
            && binding
                .tool_call_id
                .as_deref()
                .map(|id| !id.is_empty())
                .unwrap_or(false);
        self.approvals.lock().unwrap().push(ApprovalRecord {
            approved,
            operation: operation.clone(),
            target: target.clone(),
            binding: binding.clone(),
        });
        Box::pin(async move {
            if approved {
                Ok(())
            } else {
                Err(format!(
                    "host_loopback refused '{operation}' on '{target}': only a parameter-bound write to {RESULT_FILE} is approved"
                ))
            }
        })
    }

    /// An unknown result is a different fact than "this call is allowed": the
    /// example never releases the unknown-effect gate from the ordinary
    /// approval path.
    fn confirm_unknown_side_effect(
        &self,
        call: &NormalizedCall,
        unknown: &[UnknownSideEffect],
    ) -> HostFuture<Result<(), String>> {
        self.unknown_confirmations_refused
            .fetch_add(1, Ordering::SeqCst);
        let message = format!(
            "host_loopback refuses to confirm {} unknown side effect(s) for '{}' -> '{}'; an unknown result is not an approval",
            unknown.len(),
            call.tool,
            call.display_target,
        );
        Box::pin(async move { Err(message) })
    }

    /// Record the unknown effect as an unknown fact. When the marker carries a
    /// real run scope, attach it to that scope's canonical call.
    fn on_unknown_side_effect(&self, effect: &UnknownSideEffect) -> Result<(), String> {
        self.unknowns.lock().unwrap().push(effect.clone());
        match effect.scope {
            Some(scope) => self.journal.record_canonical_unknown(
                &scope.to_string(),
                effect.tool_call_id.as_deref(),
                &format!(
                    "unknown {} side effect on {} ({})",
                    effect.reason.as_str(),
                    effect.operation,
                    effect.target
                ),
            ),
            // Outside a run scope there is no canonical call to attach to.
            None => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------
// Evidence
// ---------------------------------------------------------------------------

/// Everything the example really observed, derived from real files, real
/// callbacks and the real journal file.
#[derive(Clone, Debug, Serialize)]
pub struct Evidence {
    /// Scenario label.
    pub scenario: String,
    /// Temporary workspace root (removed after the example returns).
    pub workspace: String,
    /// Host session identity.
    pub session_id: String,
    /// Real canonical task id.
    pub task_id: String,
    /// Real engine run UUID attached by the engine.
    pub engine_run_id: Option<String>,
    /// Journal-local run id (distinct from the engine run UUID).
    pub journal_run_id: Option<String>,
    /// Real run UUID observed on `RunStarted`.
    pub scope_run_id: Option<String>,
    /// Authoritative terminal task phase.
    pub task_phase: TaskPhase,
    /// Whether the task ever really admitted (`Queued -> Running`).
    pub task_admitted: bool,
    /// Whether a cancellation was requested.
    pub task_cancel_requested: bool,
    /// Task phase observed immediately before the cancel-scenario drop.
    pub task_phase_at_cancel_drop: Option<TaskPhase>,
    /// Every real canonical task transition the tracker projected.
    pub task_transitions: Vec<TaskRecord>,
    /// Model requests the mock really served.
    pub model_requests: usize,
    /// Real scheduler snapshot.
    pub scheduler: Value,
    /// Real bounded run observations.
    pub run_observations: Vec<RunObservation>,
    /// Real successful-read observations.
    pub read_observations: Vec<ReadObservation>,
    /// Real agent events.
    pub events: Vec<AgentEvent>,
    /// Completed provider calls.
    pub completions: Vec<CompletionCallObservation>,
    /// Classified failures.
    pub diagnostics: Vec<RunDiagnostic>,
    /// Every approval decision.
    pub approvals: Vec<ApprovalRecord>,
    /// Every canonical lifecycle transition the host observed.
    pub tool_call_transitions: Vec<ToolCallRecord>,
    /// Authoritative merged canonical calls (from the journal file).
    pub canonical_calls: Vec<ToolCallRecord>,
    /// Unknown side effects recorded.
    pub unknown_recorded: Vec<UnknownSideEffect>,
    /// Unknown confirmations the host refused.
    pub unknown_confirmations_refused: usize,
    /// Shell invocations requested by the engine (must stay zero).
    pub shell_calls: usize,
    /// Seed text written before the run.
    pub seed_text: String,
    /// SHA-256 of the seed text.
    pub seed_sha256: String,
    /// The read result the model mock really saw.
    pub read_output: Option<String>,
    /// SHA-256 of that read result.
    pub read_output_sha256: Option<String>,
    /// The derived write content the mock really asked for.
    pub mock_derived_write: Option<String>,
    /// SHA-256 of the derived content.
    pub mock_derived_sha256: Option<String>,
    /// Whether `result.txt` exists after the run.
    pub result_exists: bool,
    /// Real `result.txt` contents, when present.
    pub result_content: Option<String>,
    /// SHA-256 of the real file contents.
    pub result_sha256: Option<String>,
    /// Byte length of the real file contents.
    pub result_len: Option<usize>,
    /// Whether the mock-derived content equals the real file contents.
    pub mock_derived_matches_disk: Option<bool>,
    /// Real journal file path.
    pub journal_path: String,
    /// Parsed real journal file.
    pub journal: JournalFile,
    /// Journal run state.
    pub journal_run_state: Option<RunState>,
    /// Journal canonical task phase.
    pub journal_canonical_phase: Option<String>,
    /// Journal engine run UUID.
    pub journal_engine_run_id: Option<String>,
    /// Journal canonical task id.
    pub journal_task_id: Option<String>,
    /// Journal bound engine scopes.
    pub journal_engine_scopes: Vec<String>,
    /// Canonical calls recorded in the journal.
    pub journal_call_count: usize,
    /// Legacy operation records in the journal (must stay empty for this run).
    pub journal_operation_count: usize,
    /// `request_cancel` outcome (cancel scenario).
    pub cancel_outcome: Option<String>,
    /// `finish` outcome (cancel scenario).
    pub finish_outcome: Option<String>,
    /// Assistant text the engine really delivered.
    pub final_text: String,
    /// Real SDK failure, when the scenario fails.
    pub error: Option<String>,
}

impl Evidence {
    /// Compact single-object evidence summary for stdout.
    ///
    /// Built as several smaller `json!` objects merged into one map: a single
    /// literal with this many keys exceeds the default macro recursion limit,
    /// both in the example crate and in the contract test crate that compiles
    /// this file in.
    pub fn summary(&self) -> Value {
        let mut summary = Map::new();
        let mut add = |part: Value| {
            if let Value::Object(map) = part {
                summary.extend(map);
            }
        };
        add(json!({
            "scenario": self.scenario,
            "workspace": self.workspace,
            "sessionId": self.session_id,
            "taskId": self.task_id,
            "engineRunId": self.engine_run_id,
            "journalRunId": self.journal_run_id,
            "scopeRunId": self.scope_run_id,
            "engineRunIdDiffersFromTaskId": self.engine_run_id.as_deref() != Some(self.task_id.as_str()),
            "taskPhase": self.task_phase,
            "taskAdmitted": self.task_admitted,
            "taskCancelRequested": self.task_cancel_requested,
            "taskPhaseAtCancelDrop": self.task_phase_at_cancel_drop,
            "taskTransitions": self.task_transitions.iter().map(|r| r.phase).collect::<Vec<_>>(),
            "modelRequests": self.model_requests,
            "completionCalls": self.completions.len(),
        }));
        add(json!({
            "readObservations": self.read_observations.len(),
            "approvals": self.approvals.iter().map(|a| json!({
                "operation": a.operation,
                "target": a.target,
                "approved": a.approved,
                "tool": a.binding.tool,
                "toolCallId": a.binding.tool_call_id,
                "callId": a.binding.call_id,
                "scope": a.binding.scope,
                "paramsSha256": a.binding.params_sha256,
            })).collect::<Vec<_>>(),
            "canonicalCalls": self.canonical_calls.iter().map(|c| json!({
                "tool": c.tool,
                "operation": c.operation,
                "target": c.target,
                "phase": c.phase.as_str(),
                "toolCallId": c.tool_call_id,
                "callId": c.call_id,
                "turn": c.turn,
                "effectId": c.effect_id,
                "paramsSha256": c.params_sha256,
            })).collect::<Vec<_>>(),
        }));
        add(json!({
            "seedSha256": self.seed_sha256,
            "readOutputSha256": self.read_output_sha256,
            "mockDerivedSha256": self.mock_derived_sha256,
            "resultExists": self.result_exists,
            "resultLen": self.result_len,
            "resultSha256": self.result_sha256,
            "mockDerivedMatchesDisk": self.mock_derived_matches_disk,
        }));
        add(json!({
            "journalPath": self.journal_path,
            "journalRunState": self.journal_run_state,
            "journalCanonicalPhase": self.journal_canonical_phase,
            "journalEngineRunId": self.journal_engine_run_id,
            "journalTaskId": self.journal_task_id,
            "journalEngineScopes": self.journal_engine_scopes,
            "journalCallCount": self.journal_call_count,
            "journalOperationCount": self.journal_operation_count,
            "shellCalls": self.shell_calls,
            "unknownRecorded": self.unknown_recorded.len(),
            "unknownConfirmationsRefused": self.unknown_confirmations_refused,
        }));
        add(json!({
            "diagnostics": self.diagnostics.iter().map(|d| json!({
                "category": d.category,
                "message": d.message,
                "retryable": d.retryable,
                "httpStatus": d.http_status,
            })).collect::<Vec<_>>(),
            "cancelOutcome": self.cancel_outcome,
            "finishOutcome": self.finish_outcome,
            "finalText": self.final_text,
            "error": self.error,
            "schedulerBudget": self.scheduler.get("budget").cloned().unwrap_or(Value::Null),
        }));
        Value::Object(summary)
    }
}

// ---------------------------------------------------------------------------
// Scenario runner (shared by the example `main` and the contract tests)
// ---------------------------------------------------------------------------

/// Run one scenario end to end and return real, verifiable evidence.
///
/// The temporary workspace and the loopback server are always cleaned up before
/// this returns; every observed value was read from a real file, a real
/// callback or the real journal before that cleanup.
pub async fn run_scenario(scenario: Scenario) -> Result<Evidence, String> {
    let workspace = tempfile::tempdir().map_err(|error| format!("temp workspace: {error}"))?;
    let cwd = workspace
        .path()
        .canonicalize()
        .map_err(|error| format!("canonical workspace: {error}"))?;
    std::fs::write(cwd.join(SEED_FILE), SEED_TEXT)
        .map_err(|error| format!("write {SEED_FILE}: {error}"))?;
    let seed_sha256 = sha256_hex(SEED_TEXT.as_bytes());

    let mut mock = MockServer::start(scenario)?;
    let config = Config {
        provider: Provider::Custom,
        base_url: format!("http://{}/v1", mock.address),
        model: MOCK_MODEL.to_owned(),
        api_key: FIXED_CREDENTIAL.to_owned(),
    };

    // The host persists through the public journal API; the tracker is wired to
    // the host's own `on_task_lifecycle` projection.
    let journal = Arc::new(Journal::open(&cwd, SESSION_ID));
    let host = Arc::new(ExampleHost::new(Arc::clone(&journal), cwd.clone()));
    let host_dyn: Arc<dyn Host> = host.clone();

    // A real, caller-owned canonical task. The example registers it (Queued)
    // before the run, so the run binds this exact task.
    let task_id = TaskId::new(Uuid::new_v4().to_string()).map_err(|error| error.to_string())?;
    let tracker = TaskLifecycleTracker::new(
        task_id.as_str().to_owned(),
        host_task_sink(Arc::clone(&host_dyn)),
    );
    let handle = TaskHandle::new(tracker, task_id.clone());
    journal.begin_run_with_task(Some(scenario.prompt()), Some(task_id.as_str()))?;
    handle
        .register(Some("host_loopback example task"))
        .map_err(|error| error.to_string())?;

    // A real shared scheduler with a hard model-call budget.
    let scheduler = Arc::new(Scheduler::new(
        Limits {
            max_model_calls: Some(MODEL_REQUEST_LIMIT),
            ..Limits::default()
        },
        None,
    )?);
    let options = RunOptions {
        scheduler: Some(Arc::clone(&scheduler)),
        instructions:
            "host_loopback: only the read and write file tools are available; the host approves a single write to result.txt"
                .to_owned(),
        max_turns: 6,
        task: Some(handle.clone()),
        ..RunOptions::default()
    };
    let tool_filter = ToolFilter::from_whitelist(&["read".to_owned(), "write".to_owned()])
        .map_err(|error| error.to_string())?
        .without_shell();
    let tools = RunTools {
        options,
        tool_filter,
        ..RunTools::default()
    };

    let mut error = None;
    let mut cancel_outcome = None;
    let mut finish_outcome = None;
    let mut task_phase_at_cancel_drop = None;
    let task_record: Option<TaskRecord>;

    match scenario {
        Scenario::Cancel => {
            let mut run = Box::pin(run_with_tools_tracked(
                config,
                Vec::new(),
                scenario.prompt(),
                &cwd,
                Arc::clone(&host_dyn),
                Services::default(),
                tools,
            ));
            // The run future must actually be polled while we wait for the
            // mock to accept its HTTP request; a bare await on the channel
            // would never let the engine make progress.
            tokio::select! {
                _outcome = &mut run => {
                    return Err(
                        "the cancel-scenario run finished on its own before the drop".into(),
                    );
                }
                accepted = tokio::time::timeout(RUN_TIMEOUT, mock.next_request()) => {
                    match accepted {
                        Ok(true) => {}
                        Ok(false) => {
                            return Err(
                                "the cancel-scenario mock server stopped before receiving a request"
                                    .into(),
                            );
                        }
                        Err(_) => {
                            return Err(
                                "the cancel-scenario model request was not dispatched within the bound"
                                    .into(),
                            );
                        }
                    }
                }
            }
            task_phase_at_cancel_drop = handle.snapshot().map(|record| record.phase);
            if task_phase_at_cancel_drop != Some(TaskPhase::Running) {
                return Err(format!(
                    "the cancel-scenario task was not really running at drop time: {task_phase_at_cancel_drop:?}"
                ));
            }
            drop(run);
            wait_for_run_finished(&host).await?;
            tokio::time::sleep(OBSERVE_GRACE).await;

            // The caller owns a dropped explicit task: it must cancel and
            // settle it itself.
            let outcome = handle
                .request_cancel(Some(
                    "host_loopback: run future dropped while the model request was in flight",
                ))
                .map_err(|error| error.to_string())?;
            cancel_outcome = Some(format!("{outcome:?}"));
            let finish = handle
                .finish(Settlement::Cancelled, Some("host_loopback cancel scenario"))
                .map_err(|error| error.to_string())?;
            finish_outcome = Some(format!("{finish:?}"));
            task_record = handle.wait_settled().await;
        }
        Scenario::Success | Scenario::ModelError => {
            let run = run_with_tools_tracked(
                config,
                Vec::new(),
                scenario.prompt(),
                &cwd,
                Arc::clone(&host_dyn),
                Services::default(),
                tools,
            );
            let outcome = tokio::time::timeout(RUN_TIMEOUT, run)
                .await
                .map_err(|_| "the SDK run exceeded its bounded timeout".to_string())?;
            match outcome {
                Ok((_history, record)) => task_record = Some(record),
                Err(message) => {
                    error = Some(message);
                    task_record = handle.snapshot();
                }
            }
        }
    }

    let task =
        task_record.ok_or_else(|| "the run returned no canonical task record".to_string())?;

    // Close the journal run with the real terminal state.
    let journal_state = match scenario.expected_phase() {
        TaskPhase::Completed => RunState::Completed,
        TaskPhase::Failed => RunState::Failed,
        TaskPhase::Cancelled => RunState::Cancelled,
        other => {
            return Err(format!(
                "scenario '{scenario:?}' cannot settle as {other:?}"
            ));
        }
    };
    journal.finish_run(journal_state)?;

    // Read the real workspace artifacts before the temporary directory drops.
    let result_path = host.cwd().join(RESULT_FILE);
    let result_exists = result_path.exists();
    let result_content = std::fs::read_to_string(&result_path).ok();
    let result_sha256 = result_content
        .as_deref()
        .map(|text| sha256_hex(text.as_bytes()));
    let result_len = result_content.as_ref().map(String::len);

    let read_output = mock.read_output();
    let read_output_sha256 = read_output
        .as_deref()
        .map(|text| sha256_hex(text.as_bytes()));
    let mock_derived_write = mock.derived_write();
    let mock_derived_sha256 = mock_derived_write
        .as_deref()
        .map(|text| sha256_hex(text.as_bytes()));
    let mock_derived_matches_disk = match (&mock_derived_write, &result_content) {
        (Some(derived), Some(disk)) => Some(derived == disk),
        _ => None,
    };
    let model_requests = mock.request_count();

    // Parse the durable journal this run really wrote.
    let journal_path = journal
        .path()
        .ok_or_else(|| "the example journal is disabled".to_string())?;
    let journal_text = std::fs::read_to_string(&journal_path)
        .map_err(|error| format!("read journal {}: {error}", journal_path.display()))?;
    let journal_file: JournalFile = serde_json::from_str(&journal_text)
        .map_err(|error| format!("parse journal {}: {error}", journal_path.display()))?;
    let journal_run = journal_file
        .runs
        .iter()
        .find(|run| run.task_id.as_deref() == Some(task_id.as_str()))
        .or_else(|| journal_file.runs.first());
    // Read every journal-derived value before moving the parsed journal into
    // the evidence.
    let journal_run_id = journal_run.map(|run| run.run_id.clone());
    let journal_run_state = journal_run.map(|run| run.state);
    let journal_canonical_phase = journal_run.and_then(|run| run.canonical_phase.clone());
    let journal_engine_run_id = journal_run.and_then(|run| run.engine_run_id.clone());
    let journal_task_id = journal_run.and_then(|run| run.task_id.clone());
    let journal_engine_scopes = journal_run
        .map(|run| run.engine_scopes.clone())
        .unwrap_or_default();
    let canonical_calls = journal_run.map(|run| run.calls.clone()).unwrap_or_default();
    let journal_call_count = journal_run.map(|run| run.calls.len()).unwrap_or(0);
    let journal_operation_count = journal_run.map(|run| run.operations.len()).unwrap_or(0);

    let events = host.events();
    let final_text: String = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();

    let evidence = Evidence {
        scenario: scenario.name().to_owned(),
        workspace: cwd.display().to_string(),
        session_id: SESSION_ID.to_owned(),
        task_id: task_id.as_str().to_owned(),
        engine_run_id: task.engine_run_id.as_ref().map(|id| id.as_str().to_owned()),
        journal_run_id,
        scope_run_id: host
            .run_observations()
            .iter()
            .find(|observation| observation.kind == RunObservationKind::RunStarted)
            .map(|observation| observation.run_id.to_string()),
        task_phase: task.phase,
        task_admitted: task.was_admitted(),
        task_cancel_requested: task.cancel_requested,
        task_phase_at_cancel_drop,
        task_transitions: host.task_records(),
        model_requests,
        scheduler: scheduler.snapshot(),
        run_observations: host.run_observations(),
        read_observations: host.read_observations(),
        events,
        completions: host.completions(),
        diagnostics: host.diagnostics(),
        approvals: host.approvals(),
        tool_call_transitions: host.canonical_calls(),
        canonical_calls,
        unknown_recorded: host.unknowns(),
        unknown_confirmations_refused: host.unknown_confirmations_refused(),
        shell_calls: host.shell_calls(),
        seed_text: SEED_TEXT.to_owned(),
        seed_sha256,
        read_output,
        read_output_sha256,
        mock_derived_write,
        mock_derived_sha256,
        result_exists,
        result_content,
        result_sha256,
        result_len,
        mock_derived_matches_disk,
        journal_path: journal_path.display().to_string(),
        journal_run_state,
        journal_canonical_phase,
        journal_engine_run_id,
        journal_task_id,
        journal_engine_scopes,
        journal_call_count,
        journal_operation_count,
        cancel_outcome,
        finish_outcome,
        final_text,
        error,
        journal: journal_file,
    };
    Ok(evidence)
}

async fn wait_for_run_finished(host: &ExampleHost) -> Result<(), String> {
    let deadline = Instant::now() + RUN_TIMEOUT;
    while Instant::now() < deadline {
        if host
            .run_observations()
            .iter()
            .any(|observation| observation.kind == RunObservationKind::RunFinished)
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Err("the dropped run did not emit its real RunFinished observation within the bound".into())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut scenario = Scenario::Success;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{HELP}");
                return Ok(());
            }
            "--scenario" => {
                let value = args
                    .next()
                    .ok_or("--scenario requires a value: success|model-error|cancel")?;
                scenario = Scenario::parse(&value)?;
            }
            other => return Err(format!("unknown argument '{other}'; try --help").into()),
        }
    }
    let evidence = run_scenario(scenario).await?;
    println!("{}", serde_json::to_string(&evidence.summary())?);
    Ok(())
}
