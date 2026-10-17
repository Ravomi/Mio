//! End-to-end acceptance for the unified tool admission gate (REQ-POLICY).
//!
//! Every test drives a real `run_with_tools` turn against a local mock HTTP
//! server. No external provider is contacted and no credential is read: the
//! requests carry only the fixed `local-test` token this file sets on `Config`.
//!
//! The tests assert the honest boundaries: a denial at the dispatch boundary
//! leaves no file and never runs an `extra_tools` handler, an approval is
//! requested again for every dispatch instead of being reused, a read-only run
//! refuses unknown side effects without prompting, a dynamic tool may not
//! shadow a built-in name, and a delegated child can only narrow the parent's
//! permission set. A real provider tool-call id is single use within a run: a
//! repeated id is refused before any approval, and a refused, denied or
//! cancelled attempt never releases its id for a replay.

#[path = "../src/test_http.rs"]
mod test_http;

use mio_agent::tool::DynamicTool;
use mio_coding_agent::policy::{
    ApprovalBinding, PolicyContext, TOOL_CALL_CLAIMS_CAPACITY, ToolPolicy,
};
use mio_coding_agent::{
    AgentEvent, Config, Host, HostFuture, Message, Provider, RunOptions, RunTools, Services,
    ToolFilter, run_with_tools,
    subagent::{Delegate, Executor, Status, SubagentManager, Task},
};
use mio_core::message::ToolName;
use mio_core::tool::{ToolExecutionError, ToolOutput};
use serde_json::{Value, json};
use std::{
    io::Write,
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

const API_KEY: &str = "local-test";

/// What the capture executor observed about the task it was handed.
type Captured = Arc<Mutex<Option<(bool, bool, bool)>>>;

// ---------------------------------------------------------------------------
// Local mock HTTP server
// ---------------------------------------------------------------------------

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
}

struct MockServer {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl MockServer {
    fn start(responses: Vec<Response>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
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
                        let (_, body) = test_http::read_json(&mut socket);
                        captured.lock().unwrap().push(body);
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

    /// Every JSON request body the mock received, in arrival order.
    fn bodies(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

fn write_response(socket: &mut TcpStream, response: &Response) {
    let head = format!(
        "HTTP/1.1 {} OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        response.content_type,
        response.body.len()
    );
    socket.write_all(head.as_bytes()).unwrap();
    socket.write_all(&response.body).unwrap();
    socket.flush().unwrap();
}

fn openai_stream(id: &str, text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "test",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": "stop"}]
        })
    )
}

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

// ---------------------------------------------------------------------------
// Hosts and tools
// ---------------------------------------------------------------------------

/// A host that records every bound approval and answers with a fixed verdict.
#[derive(Default)]
struct GateHost {
    allow: bool,
    approvals: Mutex<Vec<String>>,
}

impl GateHost {
    fn allowing() -> Self {
        Self {
            allow: true,
            ..Self::default()
        }
    }

    fn approval_count(&self) -> usize {
        self.approvals.lock().unwrap().len()
    }
}

impl Host for GateHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Err("shell is not expected in this test".into()) })
    }

    fn on_event(&self, _: AgentEvent) {}

    fn approve_tool_binding(
        &self,
        operation: &str,
        target: &str,
        binding: &ApprovalBinding,
    ) -> HostFuture<Result<(), String>> {
        self.approvals
            .lock()
            .unwrap()
            .push(format!("{operation}:{target}:{}", binding.params_sha256));
        let allow = self.allow;
        Box::pin(async move {
            if allow {
                Ok(())
            } else {
                Err("denied by test host".into())
            }
        })
    }
}

fn config(base_url: String) -> Config {
    Config {
        provider: Provider::Custom,
        base_url,
        model: "test".into(),
        api_key: API_KEY.into(),
    }
}

/// A dynamic tool that counts how many times its handler actually ran.
fn counting_extra(ran: Arc<AtomicUsize>) -> DynamicTool {
    DynamicTool::new(
        ToolName::new("fixture_extra").expect("tool name"),
        "fixture extra tool",
        json!({"type": "object"}),
        move |_| {
            let ran = ran.clone();
            Box::pin(async move {
                ran.fetch_add(1, Ordering::SeqCst);
                Ok::<_, ToolExecutionError>(ToolOutput::text("ran"))
            })
        },
    )
}

async fn run(
    base_url: String,
    dir: &Path,
    host: Arc<GateHost>,
    tools: RunTools,
) -> Result<Vec<Message>, String> {
    run_with_tools(
        config(base_url),
        vec![],
        "go",
        dir,
        host,
        Services::default(),
        tools,
    )
    .await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn extra_tool_shadowing_a_builtin_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let shadow = DynamicTool::new(
        ToolName::new("read").expect("tool name"),
        "shadow",
        json!({"type": "object"}),
        |_| Box::pin(async { Ok::<_, ToolExecutionError>(ToolOutput::text("shadow")) }),
    );
    let host = Arc::new(GateHost::default());
    // The conflict is rejected before any provider call, so the port is unused.
    let error = run(
        "http://127.0.0.1:1/v1".into(),
        dir.path(),
        host,
        RunTools {
            extra_tools: vec![shadow],
            ..RunTools::default()
        },
    )
    .await
    .unwrap_err();
    assert!(error.contains("conflict"), "{error}");
}

#[tokio::test]
async fn denied_builtin_write_leaves_no_file_and_reasks_each_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("guarded.txt");
    let server = MockServer::start(vec![
        Response::sse(openai_tool_stream(
            "c1",
            "workspace",
            &[(
                "call_1",
                json!({"operation":"write","path":"guarded.txt","value":"one"}),
            )],
        )),
        Response::sse(openai_tool_stream(
            "c2",
            "workspace",
            &[(
                "call_2",
                json!({"operation":"write","path":"guarded.txt","value":"one"}),
            )],
        )),
        Response::sse(openai_stream("c3", "done")),
    ]);
    let host = Arc::new(GateHost::default());
    let result = run(
        format!("http://{}/v1", server.address),
        dir.path(),
        host.clone(),
        RunTools::default(),
    )
    .await;
    assert!(result.is_ok(), "{result:?}");
    assert!(!target.exists(), "a denied write must not create the file");
    assert_eq!(
        host.approval_count(),
        2,
        "each dispatch must ask again rather than reuse an approval"
    );
}

#[tokio::test]
async fn extra_tool_handler_never_runs_when_denied() {
    let dir = tempfile::tempdir().unwrap();
    let ran = Arc::new(AtomicUsize::new(0));
    let server = MockServer::start(vec![
        Response::sse(openai_tool_stream(
            "e1",
            "fixture_extra",
            &[("call_1", json!({}))],
        )),
        Response::sse(openai_stream("e2", "done")),
    ]);
    let host = Arc::new(GateHost::default());
    let result = run(
        format!("http://{}/v1", server.address),
        dir.path(),
        host.clone(),
        RunTools {
            extra_tools: vec![counting_extra(ran.clone())],
            ..RunTools::default()
        },
    )
    .await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(ran.load(Ordering::SeqCst), 0, "denied handler must not run");
    assert_eq!(host.approval_count(), 1);
}

#[tokio::test]
async fn read_only_run_denies_extra_tools_without_approval() {
    let dir = tempfile::tempdir().unwrap();
    let ran = Arc::new(AtomicUsize::new(0));
    let server = MockServer::start(vec![
        Response::sse(openai_tool_stream(
            "r1",
            "fixture_extra",
            &[("call_1", json!({}))],
        )),
        Response::sse(openai_stream("r2", "done")),
    ]);
    let host = Arc::new(GateHost::allowing());
    let result = run(
        format!("http://{}/v1", server.address),
        dir.path(),
        host.clone(),
        RunTools {
            read_only: true,
            extra_tools: vec![counting_extra(ran.clone())],
            ..RunTools::default()
        },
    )
    .await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(ran.load(Ordering::SeqCst), 0);
    assert_eq!(
        host.approval_count(),
        0,
        "a read-only refusal happens before approval"
    );
}

#[tokio::test]
async fn shell_alias_and_aggregate_operation_are_denied_by_the_filter() {
    let host = Arc::new(GateHost::allowing());
    let policy = ToolPolicy::new(
        ToolFilter::from_cli(None, Some("bash"), false).unwrap(),
        false,
        host,
        PolicyContext::new("/tmp", "run-1"),
    );
    assert!(policy.admit("shell", r#"{"command":"x"}"#).await.is_err());
    assert!(
        policy
            .admit("workspace", r#"{"operation":"shell","value":"x"}"#)
            .await
            .is_err()
    );
    assert!(policy.admit("read", r#"{"path":"a"}"#).await.is_ok());
}

#[tokio::test]
async fn subagent_permission_ceiling_is_monotonic_and_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let captured: Captured = Arc::new(Mutex::new(None));

    struct Capture(Captured);
    impl Executor for Capture {
        fn execute(&self, task: Task) -> HostFuture<Result<Vec<Message>, String>> {
            let captured = self.0.clone();
            Box::pin(async move {
                *captured.lock().unwrap() = Some((
                    task.read_only,
                    task.tool_filter.allows("write"),
                    task.tool_filter.allows("read"),
                ));
                Ok(vec![])
            })
        }
    }

    let manager = SubagentManager::with_executor(
        dir.path(),
        Config {
            model: "test".into(),
            ..Config::default()
        },
        RunOptions::default(),
        1,
        Duration::from_secs(5),
        Arc::new(Capture(captured.clone())),
    )
    .unwrap();

    // A read-only ceiling with a read-only tool whitelist.
    manager.set_permission_ceiling(
        ToolFilter::from_whitelist(&["read".to_owned()]).unwrap(),
        true,
    );

    // A child asking for full access and writable mode must still be narrowed.
    let run = manager
        .delegate_with_options(
            Delegate {
                session: "child".into(),
                goal: "g".into(),
                prompt: "p".into(),
                path: None,
                read_only: false,
            },
            Arc::new(GateHost::allowing()),
            None,
            ToolFilter::all(),
        )
        .unwrap();
    let done = manager
        .status(run.run_id, Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(done.status, Status::Completed);
    assert_eq!(
        *captured.lock().unwrap(),
        Some((true, false, true)),
        "a read-only ceiling must not be lifted by the child"
    );

    // Installing a broader ceiling later cannot widen the installed one.
    manager.set_permission_ceiling(ToolFilter::all(), false);
    let run = manager
        .delegate_with_options(
            Delegate {
                session: "child-two".into(),
                goal: "g".into(),
                prompt: "p".into(),
                path: None,
                read_only: false,
            },
            Arc::new(GateHost::allowing()),
            None,
            ToolFilter::all(),
        )
        .unwrap();
    let done = manager
        .status(run.run_id, Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(done.status, Status::Completed);
    assert_eq!(
        *captured.lock().unwrap(),
        Some((true, false, true)),
        "ceilings compose monotonically"
    );

    // Equal local rules must not hide a stricter incoming outer ceiling.
    manager.set_permission_ceiling(ToolFilter::all().narrowed_by(&ToolFilter::none()), false);
    let run = manager
        .delegate_with_options(
            Delegate {
                session: "child-three".into(),
                goal: "g".into(),
                prompt: "p".into(),
                path: None,
                read_only: false,
            },
            Arc::new(GateHost::allowing()),
            None,
            ToolFilter::all(),
        )
        .unwrap();
    let done = manager
        .status(run.run_id, Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(done.status, Status::Completed);
    assert_eq!(*captured.lock().unwrap(), Some((true, false, false)));
}

// ---------------------------------------------------------------------------
// Single-use provider tool-call ids (DES05)
// ---------------------------------------------------------------------------

/// A host whose bound approval never settles, modelling a cancelled dispatch.
struct PendingHost;

impl Host for PendingHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Err("shell is not expected in this test".into()) })
    }

    fn on_event(&self, _: AgentEvent) {}

    fn approve_tool_binding(
        &self,
        _: &str,
        _: &str,
        _: &ApprovalBinding,
    ) -> HostFuture<Result<(), String>> {
        Box::pin(std::future::pending())
    }
}

fn tool_policy(host: Arc<dyn Host>, scope: &str, read_only: bool) -> ToolPolicy {
    ToolPolicy::new(
        ToolFilter::all(),
        read_only,
        host,
        PolicyContext::new("/work", scope),
    )
}

/// The same provider id in a later turn must not run the tool a second time:
/// the duplicate is refused before approval, and the model sees the refusal.
#[tokio::test]
async fn a_duplicate_provider_tool_call_id_runs_the_side_effect_at_most_once() {
    let dir = tempfile::tempdir().unwrap();
    let ran = Arc::new(AtomicUsize::new(0));
    let server = MockServer::start(vec![
        Response::sse(openai_tool_stream(
            "dup-1",
            "fixture_extra",
            &[("call-same", json!({"step": 1}))],
        )),
        Response::sse(openai_tool_stream(
            "dup-2",
            "fixture_extra",
            &[("call-same", json!({"step": 2}))],
        )),
        Response::sse(openai_stream("dup-3", "done")),
    ]);
    let host = Arc::new(GateHost::allowing());
    let result = run(
        format!("http://{}/v1", server.address),
        dir.path(),
        host.clone(),
        RunTools {
            extra_tools: vec![counting_extra(ran.clone())],
            ..RunTools::default()
        },
    )
    .await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "a repeated provider tool-call id must not run the handler twice"
    );
    assert_eq!(
        host.approval_count(),
        1,
        "the duplicate is refused before another approval is requested"
    );
    let bodies = server.bodies();
    assert!(bodies.len() >= 3, "one request per turn: {bodies:?}");
    let last = bodies.last().expect("the final request must be captured");
    assert!(
        last.to_string().contains("duplicate tool call id"),
        "the model must see the protocol refusal: {last}"
    );
}

/// Eight real concurrent dispatches of one id: exactly one may claim it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_dispatches_of_one_id_admit_exactly_one() {
    let host = Arc::new(GateHost::allowing());
    let policy = tool_policy(host.clone(), "run-concurrent", false);
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let policy = policy.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            let _ = barrier.wait().await;
            policy
                .admit_with_call_id("fixture_extra", r#"{"a":1}"#, Some("call-shared"))
                .await
        }));
    }
    let mut admitted = 0usize;
    let mut duplicated = 0usize;
    for task in tasks {
        match task.await.expect("the dispatch task must not panic") {
            Ok(_) => admitted += 1,
            Err(denial) if denial.reason().contains("duplicate tool call id") => duplicated += 1,
            Err(other) => panic!("unexpected refusal: {other}"),
        }
    }
    assert_eq!(admitted, 1, "only one concurrent dispatch may claim the id");
    assert_eq!(duplicated, 7);
    assert_eq!(policy.claim_count(), 1);
    assert_eq!(
        host.approval_count(),
        1,
        "only the winning claim asks for approval"
    );
}

/// Every clone of a run's gate shares one ledger.
#[tokio::test]
async fn cloned_gates_share_one_ledger() {
    let policy = tool_policy(Arc::new(GateHost::allowing()), "run-clone", false);
    let clone = policy.clone();
    policy
        .admit_with_call_id("read", r#"{"path":"a"}"#, Some("call-shared"))
        .await
        .unwrap();
    let denial = clone
        .admit_with_call_id("read", r#"{"path":"a"}"#, Some("call-shared"))
        .await
        .unwrap_err();
    assert!(
        denial.reason().contains("duplicate tool call id"),
        "{denial}"
    );
    assert_eq!(policy.claim_count(), 1);
    assert_eq!(clone.claim_count(), 1);
}

/// The key is `(scope, tool_call_id)`: another run may see the same id.
#[tokio::test]
async fn the_same_id_is_independent_across_scopes() {
    let host = Arc::new(GateHost::allowing());
    let first = tool_policy(host.clone(), "run-a", false);
    let second = tool_policy(host.clone(), "run-b", false);
    for policy in [&first, &second] {
        assert!(
            policy
                .admit_with_call_id("read", r#"{"path":"a"}"#, Some("call-same"))
                .await
                .is_ok(),
            "a fresh scope must accept the id"
        );
        assert_eq!(policy.claim_count(), 1);
    }
    let replay = first
        .admit_with_call_id("read", r#"{"path":"a"}"#, Some("call-same"))
        .await
        .unwrap_err();
    assert!(
        replay.reason().contains("duplicate tool call id"),
        "{replay}"
    );
    assert_eq!(second.claim_count(), 1, "another scope keeps its own entry");
}

/// A refused attempt keeps its id: the replay is a protocol refusal, and the
/// host is never asked twice for one id.
#[tokio::test]
async fn a_refused_attempt_does_not_release_the_id() {
    let host = Arc::new(GateHost::default());
    let policy = tool_policy(host.clone(), "run-refused", false);
    assert!(
        policy
            .admit_with_call_id("fixture_extra", r#"{"a":1}"#, Some("call-refused"))
            .await
            .is_err()
    );
    assert_eq!(host.approval_count(), 1);
    let replay = policy
        .admit_with_call_id("fixture_extra", r#"{"a":1}"#, Some("call-refused"))
        .await
        .unwrap_err();
    assert!(
        replay.reason().contains("duplicate tool call id"),
        "{replay}"
    );
    assert_eq!(host.approval_count(), 1, "a refused id is never re-asked");
}

/// Dropping a dispatch while its approval is pending does not free the id.
#[tokio::test]
async fn a_cancelled_attempt_does_not_release_the_id() {
    let policy = tool_policy(Arc::new(PendingHost), "run-cancelled", false);
    let cancelled = tokio::time::timeout(
        Duration::from_millis(20),
        policy.admit_with_call_id("fixture_extra", r#"{"a":1}"#, Some("call-cancelled")),
    )
    .await;
    assert!(cancelled.is_err(), "the approval is still pending");
    let replay = policy
        .admit_with_call_id("fixture_extra", r#"{"a":1}"#, Some("call-cancelled"))
        .await
        .unwrap_err();
    assert!(
        replay.reason().contains("duplicate tool call id"),
        "{replay}"
    );
    assert_eq!(policy.claim_count(), 1);
    assert_eq!(policy.denial_count(), 1, "only the replay is refused");
}

/// The ledger is bounded: at capacity new ids fail closed instead of evicting
/// an id that was already admitted.
#[tokio::test]
async fn the_ledger_is_bounded_and_fails_closed_without_eviction() {
    assert_eq!(
        TOOL_CALL_CLAIMS_CAPACITY, 4096,
        "the bound is explicit and documented"
    );
    let policy = tool_policy(Arc::new(GateHost::allowing()), "run-capacity", false);
    for index in 0..TOOL_CALL_CLAIMS_CAPACITY {
        let id = format!("call-{index}");
        policy
            .admit_with_call_id("read", r#"{"path":"a"}"#, Some(id.as_str()))
            .await
            .unwrap_or_else(|denial| panic!("{id} must fit: {denial}"));
    }
    assert_eq!(policy.claim_count(), TOOL_CALL_CLAIMS_CAPACITY);
    let overflow = policy
        .admit_with_call_id("read", r#"{"path":"a"}"#, Some("call-overflow"))
        .await
        .unwrap_err();
    assert!(
        overflow.reason().contains("full"),
        "a full ledger refuses new ids: {overflow}"
    );
    let oldest = policy
        .admit_with_call_id("read", r#"{"path":"a"}"#, Some("call-0"))
        .await
        .unwrap_err();
    assert!(
        oldest.reason().contains("duplicate tool call id"),
        "the oldest entry is still remembered: {oldest}"
    );
    assert_eq!(
        policy.claim_count(),
        TOOL_CALL_CLAIMS_CAPACITY,
        "a refused id never evicts or claims a slot"
    );
}

/// A malformed id fails closed and claims nothing; `None` keeps the direct SDK
/// path unchanged instead of fabricating a provider id.
#[tokio::test]
async fn a_malformed_provider_id_is_refused_without_claiming_a_slot() {
    let policy = tool_policy(Arc::new(GateHost::allowing()), "run-bad-id", false);
    let too_long = "x".repeat(4096);
    for bad in ["", "   ", "bad\nid", too_long.as_str()] {
        let denial = policy
            .admit_with_call_id("read", r#"{"path":"a"}"#, Some(bad))
            .await
            .unwrap_err();
        assert!(
            denial.reason().contains("refusing the call"),
            "{bad:?} must fail closed: {denial}"
        );
    }
    assert_eq!(policy.claim_count(), 0, "a refused id claims no slot");
    policy.admit("read", r#"{"path":"a"}"#).await.unwrap();
    policy.admit("read", r#"{"path":"a"}"#).await.unwrap();
    assert_eq!(policy.claim_count(), 0, "None never fabricates an id");
}
