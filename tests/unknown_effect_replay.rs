//! Same-run unknown-side-effect guard regression (DES-09, partial).
//!
//! This suite drives the **real** engine loop (`run_with_tools`) against a
//! loopback OpenAI mock and a real stdio MCP server, and proves the same-run
//! contract only:
//!
//! * a `tools/call` whose transport died is counted **once** as a typed
//!   unknown (journal `Unknown`, never `Failed`/`Executed`) and is never
//!   replayed;
//! * after that unknown, a new write/external dispatch is refused whatever the
//!   tool, the parameters or the provider tool-call id, while `read` still
//!   works;
//! * an explicit host `confirm_unknown_side_effect` releases the new dispatch,
//!   which still needs its own new tool-call id and its normal bound approval;
//! * a delegated child run shares the parent run's fence.
//!
//! It deliberately does **not** claim a restart-persistent fence: the fence is
//! per run tree and dies with the run. Durable `Unknown` evidence across a
//! crash is the existing `finish_run`/`interrupted` path, not this guard. An
//! engine-less direct tool call carries no dispatch identity, so it has no run
//! guard at all: that is asserted honestly in `mcp_lifecycle`, not here.
//!
//! No external provider is contacted and no credential is read from the
//! environment; the loopback mock only receives the fixed `local-test` token.

#![cfg(unix)]

#[path = "../src/test_http.rs"]
mod test_http;

use mio_coding_agent::journal::{Journal, RunState};
use mio_coding_agent::mcp::{
    McpOrigin, McpServerSpec, SourcedMcpSpec, load_policy_bound_tools_from_specs,
};
use mio_coding_agent::policy::{NormalizedCall, PolicyContext, ToolPolicy};
use mio_coding_agent::subagent::{Delegate, NativeExecutor, Status, SubagentManager};
use mio_coding_agent::tool_lifecycle::{ToolCallPhase, ToolCallRecord};
use mio_coding_agent::unknown_effect::{UnknownReason, UnknownSideEffect};
use mio_coding_agent::unresolved_effect::{
    EvidenceResolution, UnknownEffectInput, UnknownFactReason, UnresolvedEffectKey,
    UnresolvedEffectLedger, UnresolvedEffectState,
};
use mio_coding_agent::{
    AgentEvent, Config, Host, HostFuture, Provider, RunObservation, RunObservationKind,
    RunObservationStatus, RunOptions, RunTools, Services, ToolFilter, run_with_tools,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

const API_KEY: &str = "local-test";
const SESSION: &str = "unknown-effect";

// ---------------------------------------------------------------------------
// Loopback OpenAI/SSE mock (one response per request)
// ---------------------------------------------------------------------------

struct MockServer {
    address: SocketAddr,
    bodies: Arc<Mutex<Vec<Value>>>,
}

impl MockServer {
    fn start(responses: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let bodies: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let queue = Arc::new(Mutex::new(VecDeque::from(responses)));
        let captured = Arc::clone(&bodies);
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(60);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((socket, _)) => {
                        let queue = Arc::clone(&queue);
                        let captured = Arc::clone(&captured);
                        std::thread::spawn(move || serve(socket, queue, captured));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("mock accept failed: {error}"),
                }
            }
        });
        Self { address, bodies }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().unwrap().clone()
    }
}

fn serve(
    mut socket: TcpStream,
    queue: Arc<Mutex<VecDeque<String>>>,
    captured: Arc<Mutex<Vec<Value>>>,
) {
    let _ = socket.set_read_timeout(Some(Duration::from_secs(15)));
    let _ = socket.set_write_timeout(Some(Duration::from_secs(15)));
    let (_, body) = test_http::read_json(&mut socket);
    captured.lock().unwrap().push(body);
    let response = queue
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or_else(|| openai_text_stream("mock-exhausted"));
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.len()
    );
    if socket.write_all(head.as_bytes()).is_err() {
        return;
    }
    let _ = socket.write_all(response.as_bytes());
    let _ = socket.flush();
}

fn openai_text_stream(text: &str) -> String {
    let frame = json!({
        "id": "chatcmpl-test",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "test",
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": "stop"}]
    })
    .to_string();
    format!("data: {frame}\n\ndata: [DONE]\n\n")
}

fn openai_tool_stream(calls: &[(&str, &str, Value)]) -> String {
    let tool_calls = calls
        .iter()
        .enumerate()
        .map(|(index, (call_id, name, arguments))| {
            json!({
                "index": index,
                "id": call_id,
                "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()}
            })
        })
        .collect::<Vec<_>>();
    let frames = [
        json!({
            "id": "chatcmpl-test",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "test",
            "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": tool_calls}, "finish_reason": Value::Null}]
        })
        .to_string(),
        json!({
            "id": "chatcmpl-test",
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

fn body_text(body: &Value) -> String {
    serde_json::to_string(body).unwrap()
}

// ---------------------------------------------------------------------------
// Real stdio fake MCP server, embedded so this suite stays self-contained
// ---------------------------------------------------------------------------

/// Speaks JSON-RPC over stdio, appends each `tools/call` to `mcp_calls.log`, and
/// in `die_first` mode exits on the first call of a fresh process (before
/// answering), which is a real transport death.
const FAKE_MCP: &str = r#"
import json, os, sys
mode = sys.argv[1] if len(sys.argv) > 1 else "normal"
call_log = os.path.join(os.getcwd(), "mcp_calls.log")
def send(payload):
    sys.stdout.write(json.dumps(payload) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        request = json.loads(line)
    except ValueError:
        continue
    method = request.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": request["id"], "result": {"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "fake", "version": "1.0"}}})
    elif method == "notifications/initialized":
        continue
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": request["id"], "result": {"tools": [{"name": "record", "description": "record", "inputSchema": {"type": "object"}}]}})
    elif method == "tools/call":
        existing = open(call_log).read().splitlines() if os.path.exists(call_log) else []
        with open(call_log, "a") as handle:
            handle.write("call\n")
        if mode == "die_first" and not existing:
            sys.exit(0)
        send({"jsonrpc": "2.0", "id": request["id"], "result": {"content": [{"type": "text", "text": "recorded"}]}})
    elif request.get("id") is not None:
        send({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32601, "message": "unknown method"}})
"#;

fn fake_specs(mode: &str) -> BTreeMap<String, SourcedMcpSpec> {
    BTreeMap::from([(
        "fake".to_owned(),
        SourcedMcpSpec {
            spec: McpServerSpec {
                command: "python3".into(),
                args: vec!["-u".into(), "-c".into(), FAKE_MCP.into(), mode.into()],
                env: BTreeMap::new(),
                disabled: false,
                required: Some(true),
                optional: None,
            },
            origin: McpOrigin::Global,
        },
    )])
}

fn mcp_calls(root: &Path) -> usize {
    std::fs::read_to_string(root.join("mcp_calls.log"))
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

fn config(base_url: String) -> Config {
    Config {
        provider: Provider::Custom,
        base_url,
        model: "test".into(),
        api_key: API_KEY.into(),
    }
}

// ---------------------------------------------------------------------------
// Host that journals exactly like the CLI host
// ---------------------------------------------------------------------------

struct JournalHost {
    journal: Option<Arc<Journal>>,
    confirm: bool,
    confirm_failure: bool,
    persist_failure: bool,
    confirms: Mutex<Vec<(String, String)>>,
    unknown_calls: Mutex<Vec<UnknownSideEffect>>,
    observations: Mutex<Vec<RunObservation>>,
}

impl JournalHost {
    fn with_journal(journal: Arc<Journal>) -> Self {
        Self {
            journal: Some(journal),
            confirm: false,
            confirm_failure: false,
            persist_failure: false,
            confirms: Mutex::new(Vec::new()),
            unknown_calls: Mutex::new(Vec::new()),
            observations: Mutex::new(Vec::new()),
        }
    }

    fn plain() -> Self {
        Self {
            journal: None,
            confirm: false,
            confirm_failure: false,
            persist_failure: false,
            confirms: Mutex::new(Vec::new()),
            unknown_calls: Mutex::new(Vec::new()),
            observations: Mutex::new(Vec::new()),
        }
    }

    fn confirms(&self) -> Vec<(String, String)> {
        self.confirms.lock().unwrap().clone()
    }

    fn unknown_calls(&self) -> Vec<UnknownSideEffect> {
        self.unknown_calls.lock().unwrap().clone()
    }
}

impl Host for JournalHost {
    fn on_run_observation(&self, observation: &RunObservation) {
        self.observations.lock().unwrap().push(observation.clone());
    }

    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn on_event(&self, event: AgentEvent) {
        if let AgentEvent::ToolFinished {
            operation,
            target,
            result,
        } = &event
        {
            self.on_tool_result(operation, target, result);
        }
    }

    fn approve_tool_binding(
        &self,
        operation: &str,
        target: &str,
        binding: &mio_coding_agent::policy::ApprovalBinding,
    ) -> HostFuture<Result<(), String>> {
        if let Some(journal) = &self.journal
            && let Err(error) = journal.record_bound_intent(operation, target, binding)
        {
            return Box::pin(std::future::ready(Err(error)));
        }
        Box::pin(std::future::ready(Ok(())))
    }

    fn approve_tool_async(&self, _: &str, _: &str) -> HostFuture<Result<(), String>> {
        Box::pin(std::future::ready(Ok(())))
    }

    fn on_tool_result(&self, operation: &str, target: &str, result: &Result<String, String>) {
        if let Some(journal) = &self.journal {
            let _ = journal.record_result(operation, target, result);
        }
    }

    fn on_unknown_side_effect(&self, effect: &UnknownSideEffect) -> Result<(), String> {
        self.unknown_calls.lock().unwrap().push(effect.clone());
        if self.persist_failure {
            return Err("simulated unknown-evidence persistence failure".into());
        }
        match &self.journal {
            Some(journal) => {
                journal.record_unknown_result(&effect.operation, &effect.target, "result unknown")
            }
            None => Ok(()),
        }
    }

    fn confirm_unknown_side_effect(
        &self,
        call: &NormalizedCall,
        _unknown: &[UnknownSideEffect],
    ) -> HostFuture<Result<(), String>> {
        self.confirms
            .lock()
            .unwrap()
            .push((call.operation.clone(), call.display_target.clone()));
        if self.confirm_failure {
            return Box::pin(std::future::ready(Err(
                "host refused unknown-effect confirmation".into(),
            )));
        }
        let allow = self.confirm;
        Box::pin(async move {
            if allow {
                Ok(())
            } else {
                Err("host refused unknown-effect confirmation".into())
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Run helpers
// ---------------------------------------------------------------------------

async fn run_with_mcp(
    root: &Path,
    server: &MockServer,
    host: Arc<dyn Host>,
    mode: &str,
) -> Result<Vec<mio_coding_agent::Message>, String> {
    let bound = load_policy_bound_tools_from_specs(
        &fake_specs(mode),
        root,
        host.clone(),
        &ToolFilter::all(),
    )
    .await
    .expect("the policy-bound MCP tools must load");
    run_with_tools(
        config(server.base_url()),
        vec![],
        "go",
        root,
        host,
        Services::default(),
        RunTools {
            tool_filter: ToolFilter::all(),
            policy_bound_tools: Some(bound),
            ..RunTools::default()
        },
    )
    .await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// One transport death is one typed unknown; new same-params / changed-params /
/// other-write dispatches cannot bypass it, and `read` still works.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transport_death_is_unknown_and_blocks_new_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("present.txt"), "present-content").unwrap();

    let server = MockServer::start(vec![
        openai_tool_stream(&[("call_1", "mcp__fake__record", json!({"a": 1}))]),
        // Same parameters, new id, plus a changed-parameter call: both are
        // blocked by the run fence, so the server is never called again.
        openai_tool_stream(&[
            ("call_2", "mcp__fake__record", json!({"a": 1})),
            ("call_2b", "mcp__fake__record", json!({"a": 2})),
        ]),
        // A different write: blocked too.
        openai_tool_stream(&[(
            "call_3",
            "workspace",
            json!({"operation": "write", "path": "guarded.txt", "value": "x"}),
        )]),
        // A read is still allowed and must really run.
        openai_tool_stream(&[("call_4", "read", json!({"path": "present.txt"}))]),
        openai_text_stream("done"),
    ]);

    let journal = Arc::new(Journal::open(&root, SESSION));
    journal.begin_run().unwrap();
    let host = Arc::new(JournalHost::with_journal(journal.clone()));
    let result = run_with_mcp(&root, &server, host.clone(), "die_first").await;
    assert!(result.is_ok(), "{result:?}");
    journal.finish_run(RunState::Completed).unwrap();

    assert_eq!(mcp_calls(&root), 1, "the dead call must never be replayed");
    assert!(
        !root.join("guarded.txt").exists(),
        "an unknown must block a new write"
    );
    let bodies = server.bodies();
    assert_eq!(bodies.len(), 5, "one model request per turn: {bodies:?}");
    assert!(
        body_text(&bodies[4]).contains("present-content"),
        "the allowed read must have executed: {}",
        body_text(&bodies[4])
    );

    let unknown = host.unknown_calls();
    assert_eq!(unknown.len(), 1, "one typed unknown: {unknown:?}");
    assert_eq!(unknown[0].reason, UnknownReason::McpTransport);
    let observations = host.observations.lock().unwrap();
    let outcome = observations
        .iter()
        .find(|event| {
            event.kind == RunObservationKind::ToolOutcome
                && event.tool_call_id.as_deref() == Some("call_1")
        })
        .expect("the actual MCP transport failure must have a correlated outcome");
    assert_eq!(
        outcome.status,
        Some(RunObservationStatus::Error),
        "a legacy Ok JSON error payload must not be observed as successful execution"
    );
    assert_eq!(unknown[0].scope, Some(outcome.run_id));
    assert!(unknown[0].evidence_id.is_some_and(|id| !id.is_nil()));

    let report = journal.recovery_report().unwrap();
    assert_eq!(
        report.unknown_operations.len(),
        1,
        "the journal must record exactly one unknown: {report:?}"
    );
    assert!(
        report.failed_operations.is_empty(),
        "a transport death must never be journaled as Failed: {report:?}"
    );
    assert_eq!(
        report.unknown_operations[0].operation, "mcp__fake__record",
        "{report:?}"
    );
}

/// An explicit host confirmation releases one new dispatch; it still needs a
/// new tool-call id and its normal bound approval, and only that one write runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_host_confirmation_releases_a_new_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();

    let server = MockServer::start(vec![
        openai_tool_stream(&[("call_1", "mcp__fake__record", json!({"a": 1}))]),
        openai_tool_stream(&[(
            "call_2",
            "workspace",
            json!({"operation": "write", "path": "guarded.txt", "value": "x"}),
        )]),
        openai_text_stream("done"),
    ]);

    let journal = Arc::new(Journal::open(&root, SESSION));
    journal.begin_run().unwrap();
    let mut host = JournalHost::with_journal(journal.clone());
    host.confirm = true;
    let host = Arc::new(host);
    let result = run_with_mcp(&root, &server, host.clone(), "die_first").await;
    assert!(result.is_ok(), "{result:?}");
    journal.finish_run(RunState::Completed).unwrap();

    assert_eq!(mcp_calls(&root), 1);
    assert_eq!(
        host.confirms().len(),
        1,
        "the confirmation is asked exactly once for the one new side effect"
    );
    assert_eq!(host.confirms()[0].0, "write");
    assert!(
        root.join("guarded.txt").exists(),
        "an explicitly confirmed write must run"
    );
    let report = journal.recovery_report().unwrap();
    assert!(
        report.executed.iter().any(|op| op.operation == "write"),
        "the confirmed write must be recorded as Executed: {report:?}"
    );
}

/// A denied confirmation never releases the new side effect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_confirmation_keeps_the_guard() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();

    let server = MockServer::start(vec![
        openai_tool_stream(&[("call_1", "mcp__fake__record", json!({"a": 1}))]),
        openai_tool_stream(&[(
            "call_2",
            "workspace",
            json!({"operation": "write", "path": "guarded.txt", "value": "x"}),
        )]),
        openai_text_stream("done"),
    ]);

    let journal = Arc::new(Journal::open(&root, SESSION));
    journal.begin_run().unwrap();
    let mut host = JournalHost::with_journal(journal.clone());
    host.confirm = true;
    host.confirm_failure = true;
    let host = Arc::new(host);
    let result = run_with_mcp(&root, &server, host.clone(), "die_first").await;
    assert!(result.is_ok(), "{result:?}");
    journal.finish_run(RunState::Completed).unwrap();

    assert_eq!(host.confirms().len(), 1);
    assert!(
        !root.join("guarded.txt").exists(),
        "a refused confirmation must not release the write"
    );
}

/// A delegated child run shares the parent run's fence: a write is refused there
/// too, while its read still runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_run_shares_the_parent_fence() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("present.txt"), "present-content").unwrap();

    let server = MockServer::start(vec![
        openai_tool_stream(&[
            (
                "call_1",
                "workspace",
                json!({"operation": "write", "path": "guarded.txt", "value": "x"}),
            ),
            ("call_2", "read", json!({"path": "present.txt"})),
        ]),
        openai_text_stream("child-done"),
    ]);

    let manager = SubagentManager::with_executor(
        &root,
        config(server.base_url()),
        RunOptions::default(),
        1,
        Duration::from_secs(30),
        Arc::new(NativeExecutor),
    )
    .unwrap();

    // The parent run's fence, exactly as its own ToolPolicy would resolve it.
    let parent_run = Uuid::new_v4();
    let fence = mio_coding_agent::unknown_effect::fence_for_scope(&parent_run.to_string());
    fence.mark(UnknownSideEffect::new(
        "mcp__fake__record",
        "{}",
        UnknownReason::McpTransport,
        None,
    ));

    let host: Arc<dyn Host> = Arc::new(JournalHost::plain());
    let snapshot = manager
        .delegate_child(
            Delegate {
                session: "child".into(),
                goal: "attempt a guarded write".into(),
                prompt: "try the write, then read the present file".into(),
                path: None,
                read_only: false,
            },
            host,
            Some(parent_run),
        )
        .unwrap();
    let done = manager
        .status(snapshot.run_id, Duration::from_secs(15))
        .await
        .unwrap();
    assert_eq!(done.status, Status::Completed, "{done:?}");
    assert!(
        !root.join("guarded.txt").exists(),
        "a child must inherit the parent's unknown-effect fence"
    );
    let bodies = server.bodies();
    assert_eq!(bodies.len(), 2, "{bodies:?}");
    assert!(
        body_text(&bodies[1]).contains("present-content"),
        "the child's read must still run: {}",
        body_text(&bodies[1])
    );
    assert!(
        body_text(&bodies[1]).contains("Tool policy denied"),
        "the child's blocked write must be model-visible: {}",
        body_text(&bodies[1])
    );
}

// ---------------------------------------------------------------------------
// Direct admission gate: the guard does not depend on publishing the marker
// ---------------------------------------------------------------------------

struct DirectHost {
    confirm: bool,
}

impl Host for DirectHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn on_event(&self, _: AgentEvent) {}

    fn confirm_unknown_side_effect(
        &self,
        _call: &NormalizedCall,
        _unknown: &[UnknownSideEffect],
    ) -> HostFuture<Result<(), String>> {
        let allow = self.confirm;
        Box::pin(async move {
            if allow {
                Ok(())
            } else {
                Err("unknown side effect: denied".into())
            }
        })
    }
}

fn direct_policy(confirm: bool) -> ToolPolicy {
    ToolPolicy::new(
        ToolFilter::all(),
        false,
        Arc::new(DirectHost { confirm }),
        PolicyContext::new("/work", "direct-run"),
    )
}

#[tokio::test]
async fn direct_fence_mark_blocks_writes_without_any_marker_encoding() {
    let policy = direct_policy(false);
    // Trip the fence directly: no tool result, no context, no encoding step.
    assert!(policy.unknown_fence().mark(UnknownSideEffect::new(
        "mcp__srv__tool",
        "{}",
        UnknownReason::MarkerEncoding,
        None,
    )));

    assert!(
        policy.admit("read", r#"{"path":"a.txt"}"#).await.is_ok(),
        "reads must stay available"
    );
    let write = policy
        .admit(
            "workspace",
            r#"{"operation":"write","path":"a.txt","value":"x"}"#,
        )
        .await
        .unwrap_err();
    assert!(
        write.reason().contains("unknown"),
        "the denial must name the unknown fence: {}",
        write.reason()
    );
    // A different write with different parameters cannot bypass it either.
    assert!(
        policy
            .admit("write", r#"{"path":"other.txt","value":"y"}"#)
            .await
            .is_err()
    );
    assert_eq!(policy.unknown_fence().len(), 1);
}

/// A confirmation that never answers times out at the approval deadline and
/// still refuses: a cancelled or silent confirmation never leaks the guard.
struct PendingConfirmHost;

impl Host for PendingConfirmHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn on_event(&self, _: AgentEvent) {}

    fn confirm_unknown_side_effect(
        &self,
        _call: &NormalizedCall,
        _unknown: &[UnknownSideEffect],
    ) -> HostFuture<Result<(), String>> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn a_silent_confirmation_times_out_and_keeps_the_guard() {
    let policy = ToolPolicy::new(
        ToolFilter::all(),
        false,
        Arc::new(PendingConfirmHost),
        PolicyContext::new("/work", "timeout-run"),
    )
    .with_approval_timeout(Duration::from_millis(1));
    policy.unknown_fence().mark(UnknownSideEffect::new(
        "mcp__srv__tool",
        "{}",
        UnknownReason::McpTransport,
        None,
    ));
    let denied = policy
        .admit("write", r#"{"path":"a.txt","value":"x"}"#)
        .await
        .unwrap_err();
    assert!(
        denied.reason().contains("timed out"),
        "a silent confirmation must time out closed: {}",
        denied.reason()
    );
    assert!(policy.admit("read", r#"{"path":"a.txt"}"#).await.is_ok());
}

#[tokio::test]
async fn direct_confirmation_releases_and_persist_failure_fails_closed() {
    let host = Arc::new(DirectHost { confirm: true });
    let policy = ToolPolicy::new(
        ToolFilter::all(),
        false,
        host,
        PolicyContext::new("/work", "direct-confirm"),
    );
    policy.unknown_fence().mark(UnknownSideEffect::new(
        "mcp__srv__tool",
        "{}",
        UnknownReason::McpTransport,
        None,
    ));
    assert!(
        policy
            .admit("write", r#"{"path":"a.txt","value":"x"}"#)
            .await
            .is_ok(),
        "an explicit confirmation releases the write"
    );

    // Once the durable unknown record could not be written, no confirmation
    // can release a later side effect.
    policy.unknown_fence().mark_persist_failed();
    let denied = policy
        .admit("write", r#"{"path":"b.txt","value":"y"}"#)
        .await
        .unwrap_err();
    assert!(
        denied.reason().contains("persist"),
        "a persistence failure must fail closed: {}",
        denied.reason()
    );
    assert!(policy.admit("read", r#"{"path":"c.txt"}"#).await.is_ok());
}

/// A host that never confirms keeps the default refusal, and the confirmation
/// flow never becomes a cached approval: each new write asks again.
#[tokio::test]
async fn default_host_denies_and_never_caches_a_confirmation() {
    let host = Arc::new(JournalHost::plain());
    let policy = ToolPolicy::new(
        ToolFilter::all(),
        false,
        host,
        PolicyContext::new("/work", "default-confirm"),
    );
    policy.unknown_fence().mark(UnknownSideEffect::new(
        "mcp__srv__tool",
        "{}",
        UnknownReason::McpTransport,
        None,
    ));
    assert!(
        policy
            .admit("write", r#"{"path":"a.txt","value":"x"}"#)
            .await
            .is_err()
    );
    assert!(
        policy
            .admit("write", r#"{"path":"a.txt","value":"x"}"#)
            .await
            .is_err()
    );
    // The MCP entry is denied too, and its rejection is not a tool approval.
    assert!(policy.admit("mcp__srv__tool", r#"{}"#).await.is_err());
}

// ---------------------------------------------------------------------------
// Durable live confirmation: real engine loopback -> ledger ack -> new id runs
// ---------------------------------------------------------------------------

/// A host that persists each live typed unknown into the real workspace ledger,
/// linked back to its canonical dispatch key exactly like the CLI host, and
/// resolves a confirmation through the real `evidence_id` before acking the
/// exact persisted snapshot.
struct DurableHost {
    journal: Arc<Journal>,
    ledger: Arc<UnresolvedEffectLedger>,
    /// `(run scope, provider tool-call id) -> canonical call_index`
    dispatched: Mutex<BTreeMap<(String, String), u64>>,
    /// Per-run dispatch counter, mirroring the engine's canonical call_index.
    next_index: Mutex<BTreeMap<String, u64>>,
    /// Whether the confirmation approves; when false it refuses.
    confirm: bool,
    /// Inject one new fact for the same key while the confirmation waits, so
    /// the snapshot captured before the host answered is stale.
    stale_on_confirm: bool,
    confirms: Mutex<Vec<String>>,
    acked: Mutex<Vec<String>>,
}

impl DurableHost {
    fn new(journal: Arc<Journal>, ledger: Arc<UnresolvedEffectLedger>) -> Self {
        Self {
            journal,
            ledger,
            dispatched: Mutex::new(BTreeMap::new()),
            next_index: Mutex::new(BTreeMap::new()),
            confirm: true,
            stale_on_confirm: false,
            confirms: Mutex::new(Vec::new()),
            acked: Mutex::new(Vec::new()),
        }
    }

    fn key_for(&self, effect: &UnknownSideEffect) -> Result<UnresolvedEffectKey, String> {
        let scope = effect
            .scope
            .ok_or_else(|| "the live marker carries no real run scope".to_owned())?
            .to_string();
        let tool_call_id = effect
            .tool_call_id
            .clone()
            .ok_or_else(|| "the live marker carries no real tool-call id".to_owned())?;
        let index = self
            .dispatched
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&(scope.clone(), tool_call_id))
            .copied()
            .ok_or_else(|| "no canonical dispatch key for the live marker".to_owned())?;
        UnresolvedEffectKey::new(scope, index).map_err(|error| error.to_string())
    }
}

impl Host for DurableHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn on_event(&self, event: AgentEvent) {
        if let AgentEvent::ToolFinished {
            operation,
            target,
            result,
        } = &event
        {
            let _ = self.journal.record_result(operation, target, result);
        }
    }

    fn approve_tool_binding(
        &self,
        operation: &str,
        target: &str,
        binding: &mio_coding_agent::policy::ApprovalBinding,
    ) -> HostFuture<Result<(), String>> {
        if let Err(error) = self.journal.record_bound_intent(operation, target, binding) {
            return Box::pin(std::future::ready(Err(error)));
        }
        Box::pin(std::future::ready(Ok(())))
    }

    fn on_tool_call_lifecycle(&self, call: &ToolCallRecord) -> Result<(), String> {
        if call.phase == ToolCallPhase::Dispatched {
            let index = {
                let mut next = self
                    .next_index
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let slot = next.entry(call.scope.clone()).or_insert(0);
                *slot = slot.saturating_add(1);
                *slot
            };
            if let Some(tool_call_id) = &call.tool_call_id {
                self.dispatched
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert((call.scope.clone(), tool_call_id.clone()), index);
            }
        }
        Ok(())
    }

    fn on_unknown_side_effect(&self, effect: &UnknownSideEffect) -> Result<(), String> {
        let key = self.key_for(effect)?;
        let mut input =
            UnknownEffectInput::new(effect.operation.clone(), UnknownFactReason::TransportLost)
                .with_optional_evidence_id(effect.evidence_id);
        if !effect.target.is_empty() {
            input = input.with_target(effect.target.clone());
        }
        self.ledger
            .record_unknown(&key, input)
            .map_err(|error| error.to_string())?;
        self.journal
            .record_unknown_result(&effect.operation, &effect.target, "result unknown")
    }

    fn confirm_unknown_side_effect(
        &self,
        call: &NormalizedCall,
        unknown: &[UnknownSideEffect],
    ) -> HostFuture<Result<(), String>> {
        self.confirms
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(call.operation.clone());
        if !self.confirm {
            return Box::pin(std::future::ready(Err(
                "host refused unknown-effect confirmation".into(),
            )));
        }
        // Resolve every displayed runtime evidence id to exactly one durable
        // record BEFORE answering, exactly like the CLI host.
        let mut claims = Vec::new();
        for effect in unknown {
            let Some(evidence_id) = effect.evidence_id else {
                return Box::pin(std::future::ready(Err(
                    "the displayed unknown carries no real evidence id".into(),
                )));
            };
            match self.ledger.resolve_evidence(evidence_id) {
                Ok(EvidenceResolution::Unique(record)) if record.facts_dropped == 0 => {
                    claims.push((record.key, record.snapshot_key));
                }
                Ok(EvidenceResolution::Unique(_)) => {
                    return Box::pin(std::future::ready(Err(
                        "the displayed unknown has incomplete durable evidence".into(),
                    )));
                }
                Ok(EvidenceResolution::Missing) => {
                    return Box::pin(std::future::ready(Err(
                        "the displayed unknown has no durable record".into(),
                    )));
                }
                Ok(EvidenceResolution::Ambiguous(_)) => {
                    return Box::pin(std::future::ready(Err(
                        "the displayed unknown is ambiguous in the durable ledger".into(),
                    )));
                }
                Err(error) => {
                    return Box::pin(std::future::ready(Err(error.to_string())));
                }
            }
        }
        if self.stale_on_confirm {
            for (key, _) in &claims {
                let _ = self.ledger.record_unknown(
                    key,
                    UnknownEffectInput::new("mcp__fake__record", UnknownFactReason::TransportLost)
                        .with_evidence_id(uuid::Uuid::new_v4()),
                );
            }
        }
        let mut acked = Vec::new();
        for (key, snapshot) in claims {
            match self
                .ledger
                .acknowledge(&key, &snapshot, Some("live confirmation"))
            {
                Ok(_) => acked.push(key.to_string()),
                Err(error) => {
                    return Box::pin(std::future::ready(Err(error.to_string())));
                }
            }
        }
        *self
            .acked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = acked;
        Box::pin(std::future::ready(Ok(())))
    }
}

/// A real engine loopback: an MCP transport death is a live durable `Unknown`,
/// the new write id is denied, a separate host confirmation acks exactly the
/// persisted evidence, and the write then runs exactly once. No restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_live_unknown_is_denied_then_confirmation_acks_and_new_id_runs_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();

    let server = MockServer::start(vec![
        openai_tool_stream(&[("call_1", "mcp__fake__record", json!({"a": 1}))]),
        openai_tool_stream(&[(
            "call_2",
            "workspace",
            json!({"operation": "write", "path": "guarded.txt", "value": "x"}),
        )]),
        openai_text_stream("done"),
    ]);

    let journal = Arc::new(Journal::open(&root, SESSION));
    journal.begin_run().unwrap();
    let ledger = Arc::new(UnresolvedEffectLedger::persistent(&root));
    let host = Arc::new(DurableHost::new(journal.clone(), ledger.clone()));
    let result = run_with_mcp(&root, &server, host.clone(), "die_first").await;
    assert!(result.is_ok(), "{result:?}");
    journal.finish_run(RunState::Completed).unwrap();

    assert_eq!(mcp_calls(&root), 1, "the dead call must never be replayed");
    assert!(
        root.join("guarded.txt").exists(),
        "the confirmed write must run exactly once"
    );
    assert_eq!(host.confirms.lock().unwrap().len(), 1);
    let acked = host.acked.lock().unwrap().clone();
    assert_eq!(
        acked.len(),
        1,
        "exactly one durable record was acknowledged"
    );
    let snapshot = ledger.snapshot().unwrap();
    assert_eq!(snapshot.unknown_count(), 0, "{snapshot:?}");
    assert!(
        snapshot
            .entries
            .iter()
            .all(|entry| { entry.record.state == UnresolvedEffectState::Acknowledged })
    );
}

/// New real evidence for the same durable key while the host is answering makes
/// the captured snapshot stale: the ack is refused and the new write stays
/// denied.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_live_confirmation_is_invalidated_by_new_evidence_while_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();

    let server = MockServer::start(vec![
        openai_tool_stream(&[("call_1", "mcp__fake__record", json!({"a": 1}))]),
        openai_tool_stream(&[(
            "call_2",
            "workspace",
            json!({"operation": "write", "path": "guarded.txt", "value": "x"}),
        )]),
        openai_text_stream("done"),
    ]);

    let journal = Arc::new(Journal::open(&root, SESSION));
    journal.begin_run().unwrap();
    let ledger = Arc::new(UnresolvedEffectLedger::persistent(&root));
    let mut durable = DurableHost::new(journal.clone(), ledger.clone());
    durable.stale_on_confirm = true;
    let host = Arc::new(durable);
    let result = run_with_mcp(&root, &server, host.clone(), "die_first").await;
    assert!(result.is_ok(), "{result:?}");
    journal.finish_run(RunState::Completed).unwrap();

    assert!(
        !root.join("guarded.txt").exists(),
        "a stale approval must never release the write"
    );
    let snapshot = ledger.snapshot().unwrap();
    assert_eq!(snapshot.unknown_count(), 1, "{snapshot:?}");
    assert_eq!(
        snapshot.entries[0].record.state,
        UnresolvedEffectState::Unknown
    );
    assert!(
        snapshot.entries[0].record.facts.len() >= 2,
        "the new evidence must be preserved: {snapshot:?}"
    );
}
