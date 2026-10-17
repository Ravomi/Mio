//! DES-20 fail-closed host and registration boundaries (REQ-ISOLATE).
//!
//! These regressions cover the gap between "a backend exists" and "a run that
//! asks for isolation cannot silently execute arbitrary host code":
//!
//! * a legacy host that only implements `Host::shell` refuses a restricted
//!   `shell_with_policy` and never invokes its legacy callback, while the
//!   unrestricted path still forwards to it;
//! * the same refusal reaches the engine through `Workspace`, so the shell tool
//!   stops before any host code runs;
//! * a run that requests isolation refuses arbitrary `extra_tools` before its
//!   first model request, with zero tool side effects;
//! * the `PolicyBoundTools` container refuses a run whose policy fingerprint
//!   does not match the policy the tools were loaded under;
//! * a real local plugin and stdio MCP server loaded through
//!   `load_policy_bound_tools` are accepted by a run under a verified
//!   bubblewrap policy, proving the restricted path is not an "everything off"
//!   stub.
//!
//! Real bubblewrap cases run when the helper exists and the kernel allows
//! unprivileged namespaces; `MIO_REQUIRE_BWRAP=1` turns an unverified sandbox
//! into a failure instead of a skip. No external provider or credential is
//! used: the requests carry only the fixed `local-test` token this file sets.
#![cfg(unix)]

#[path = "../src/test_http.rs"]
mod test_http;

use mio_agent::tool::DynamicTool;
use mio_coding_agent::execution_backend::{
    BackendProbe, BubblewrapPolicy, EnvironmentPolicy, ExecutionPolicy, FilesystemPolicy,
    linux_runtime_mounts,
};
use mio_coding_agent::mcp::{load_policy_bound_tools_from_specs, shutdown_mcp_connections};
use mio_coding_agent::{
    AgentEvent, Arguments, Config, Host, HostFuture, Provider, RunTools, Services, ToolFilter,
    Workspace, run_with_tools,
};
use mio_core::message::ToolName;
use mio_core::tool::{ToolExecutionError, ToolOutput};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Mutex as AsyncMutex;

const API_KEY: &str = "local-test";

// Read only this test workspace's declarations. Never discover or start a
// developer's global MCP servers as a side effect of a library regression.
async fn load_policy_bound_tools(
    cwd: &Path,
    host: Arc<dyn Host>,
    filter: &ToolFilter,
) -> Result<mio_coding_agent::mcp::PolicyBoundTools, String> {
    use mio_coding_agent::mcp::{McpConfigFile, McpOrigin, SourcedMcpSpec};
    let bytes = match std::fs::read(cwd.join(".mio/mcp.json")) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.to_string()),
    };
    let config: McpConfigFile = match bytes {
        Some(bytes) => serde_json::from_slice(&bytes).map_err(|error| error.to_string())?,
        None => serde_json::from_value(json!({"mcpServers": {}})).unwrap(),
    };
    let specs = config
        .mcp_servers
        .into_iter()
        .map(|(name, spec)| {
            (
                name,
                SourcedMcpSpec {
                    spec,
                    origin: McpOrigin::Project,
                },
            )
        })
        .collect();
    load_policy_bound_tools_from_specs(&specs, cwd, host, filter).await
}

// ---------------------------------------------------------------------------
// Local mock HTTP/SSE server
// ---------------------------------------------------------------------------

type Requests = Arc<Mutex<Vec<Value>>>;

struct MockServer {
    address: SocketAddr,
    requests: Requests,
}

impl MockServer {
    /// Serve `responses` in order, one HTTP/SSE reply per connection.
    fn start(responses: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let requests: Requests = Arc::new(Mutex::new(Vec::new()));
        let queue = Arc::new(Mutex::new(VecDeque::from(responses)));
        let captured = Arc::clone(&requests);
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
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
                    Err(error) => panic!("mock server accept failed: {error}"),
                }
            }
        });
        Self { address, requests }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    fn bodies(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

fn serve(mut socket: TcpStream, queue: Arc<Mutex<VecDeque<String>>>, captured: Requests) {
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

/// One assistant turn calling every `(call_id, tool_name, arguments)` entry.
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

// ---------------------------------------------------------------------------
// Hosts
// ---------------------------------------------------------------------------

/// A host written against the historical `Host` trait: it implements
/// `execution_policy` and the legacy `shell` callback only.
struct LegacyHost {
    policy: ExecutionPolicy,
    calls: Mutex<Vec<String>>,
}

impl LegacyHost {
    fn new(policy: ExecutionPolicy) -> Arc<Self> {
        Arc::new(Self {
            policy,
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Host for LegacyHost {
    fn execution_policy(&self) -> ExecutionPolicy {
        self.policy.clone()
    }

    fn shell(&self, command: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        self.calls.lock().unwrap().push(command.to_owned());
        Box::pin(async { Ok("legacy-shell-ran".to_owned()) })
    }

    fn on_event(&self, _event: AgentEvent) {}
}

/// The real CLI-style host shape: reports one policy and approves project
/// plugin/MCP starts, so a policy-bound load can complete.
struct PolicyHost {
    policy: ExecutionPolicy,
}

impl PolicyHost {
    fn new(policy: ExecutionPolicy) -> Arc<Self> {
        Arc::new(Self { policy })
    }
}

impl Host for PolicyHost {
    fn execution_policy(&self) -> ExecutionPolicy {
        self.policy.clone()
    }

    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn on_event(&self, _: AgentEvent) {}

    fn approve_tool_async(
        &self,
        _operation: &str,
        _target: &str,
    ) -> HostFuture<Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

// ---------------------------------------------------------------------------
// Policies and fixtures
// ---------------------------------------------------------------------------

/// A restricted policy that needs no helper for the fail-closed host path.
fn restricted_policy() -> ExecutionPolicy {
    ExecutionPolicy::Bubblewrap(BubblewrapPolicy::default().with_filesystem(
        FilesystemPolicy::ExplicitMounts {
            read_only: linux_runtime_mounts(),
        },
    ))
}

/// A second, distinguishable restricted policy (different fingerprint).
fn other_restricted_policy() -> ExecutionPolicy {
    ExecutionPolicy::Bubblewrap(BubblewrapPolicy::default().with_environment(
        EnvironmentPolicy::Explicit(vec![(
            "MIO_POLICY_MARKER".to_owned(),
            "different".to_owned(),
        )]),
    ))
}

/// Return a verified runtime policy, or `None` when bubblewrap is unavailable
/// and the environment does not require it.
async fn verified_runtime_policy(workspace: &Path) -> Option<ExecutionPolicy> {
    let policy = restricted_policy();
    let status = policy.probe(workspace).await;
    if status.probe == BackendProbe::Verified {
        return Some(policy);
    }
    let reason = status
        .unavailable_reason
        .unwrap_or_else(|| format!("{:?}", status.probe));
    if std::env::var_os("MIO_REQUIRE_BWRAP").is_some() {
        panic!("bubblewrap sandbox is required but not verified: {reason}");
    }
    eprintln!("skipping real sandbox test: {reason}");
    None
}

fn config(base_url: String) -> Config {
    Config {
        provider: Provider::Custom,
        base_url,
        model: "test".into(),
        api_key: API_KEY.into(),
    }
}

fn recording_extra(ran: Arc<AtomicUsize>, seen: Arc<Mutex<Option<Value>>>) -> DynamicTool {
    DynamicTool::new(
        ToolName::new("fixture_extra").expect("tool name"),
        "fixture extra tool",
        json!({"type": "object"}),
        move |arguments| {
            let ran = Arc::clone(&ran);
            let seen = Arc::clone(&seen);
            Box::pin(async move {
                ran.fetch_add(1, Ordering::SeqCst);
                *seen.lock().unwrap() = Some(arguments);
                Ok::<_, ToolExecutionError>(ToolOutput::text("ran"))
            })
        },
    )
}

fn write_plugin(root: &Path, name: &str, tools: Value) {
    let directory = root.join(".mio").join("plugins").join(name);
    std::fs::create_dir_all(&directory).unwrap();
    let manifest = json!({
        "schema_version": 1,
        "name": name,
        "version": "0.1.0",
        "description": "sandbox host-boundary fixture",
        "tools": tools,
    });
    std::fs::write(
        directory.join("mio-plugin.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
}

fn plugin_tool(name: &str, script: &str) -> Value {
    json!({
        "name": name,
        "description": "sandbox boundary plugin tool",
        "parameters": {"type": "object"},
        "command": ["sh", "-c", script],
    })
}

/// A real MCP server: answers `initialize` and `tools/list` with one tool.
fn mcp_service_script() -> String {
    r#"import json
import sys

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
        result = {}
    elif method == "tools/list":
        result = {"tools": [{"name": "probe", "description": "", "inputSchema": {"type": "object"}}]}
    elif method == "tools/call":
        result = {"content": [{"type": "text", "text": "mcp-ok"}]}
    else:
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": request.get("id"), "result": result}), flush=True)
"#
    .to_owned()
}

fn write_mcp_config(root: &Path, script: &str) {
    let config = json!({
        "mcpServers": {
            "probe": {"command": "python3", "args": ["-u", "-c", script]}
        }
    });
    let directory = root.join(".mio");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("mcp.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
}

fn assert_no_model_connection(listener: &TcpListener) {
    match listener.accept() {
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        other => panic!("expected zero model connections, got {other:?}"),
    }
}

static MCP_GUARD: AsyncMutex<()> = AsyncMutex::const_new(());

// ---------------------------------------------------------------------------
// 1. Restricted shell fails closed without calling the legacy host callback
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_legacy_host_refuses_a_restricted_shell_without_calling_its_legacy_callback() {
    let root = tempfile::tempdir().unwrap();
    let restricted = restricted_policy();
    let host = LegacyHost::new(restricted.clone());

    // Direct default-method check: restricted is refused, legacy callback
    // untouched; unrestricted still forwards.
    let refused = host
        .shell_with_policy("touch legacy-marker", root.path(), &restricted)
        .await;
    assert!(
        refused.is_err(),
        "a restricted shell must be refused: {refused:?}"
    );
    assert!(
        host.calls().is_empty(),
        "the legacy shell callback must never run for a restricted policy"
    );
    assert!(!root.path().join("legacy-marker").exists());

    let allowed = host
        .shell_with_policy("echo legacy", root.path(), &ExecutionPolicy::Unrestricted)
        .await
        .unwrap();
    assert_eq!(allowed, "legacy-shell-ran");
    assert_eq!(host.calls(), vec!["echo legacy".to_owned()]);

    // The Workspace shell entry point uses the same policy-aware method, so the
    // engine's own shell tool cannot reach `Host::shell` under a restricted
    // policy either.
    let workspace = Workspace::new(root.path(), host.clone()).unwrap();
    let denied = workspace
        .execute(Arguments::shell("touch workspace-marker"))
        .await;
    assert!(
        denied.is_err(),
        "the workspace shell path must refuse a restricted policy: {denied:?}"
    );
    assert_eq!(
        host.calls().len(),
        1,
        "the workspace refused before the legacy callback"
    );
    assert!(!root.path().join("workspace-marker").exists());

    // Positive control through the same Workspace path: unrestricted still
    // reaches the legacy callback.
    let open_host = LegacyHost::new(ExecutionPolicy::Unrestricted);
    let open_workspace = Workspace::new(root.path(), open_host.clone()).unwrap();
    let ran = open_workspace
        .execute(Arguments::shell("echo workspace"))
        .await
        .unwrap();
    assert_eq!(ran, "legacy-shell-ran");
    assert_eq!(host.calls().len(), 1);
    assert_eq!(open_host.calls(), vec!["echo workspace".to_owned()]);
}

// ---------------------------------------------------------------------------
// 2. Restricted run refuses arbitrary extra_tools before any request
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_restricted_run_refuses_arbitrary_extra_tools_before_any_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();

    let root = tempfile::tempdir().unwrap();
    let ran = Arc::new(AtomicUsize::new(0));
    let seen: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let extra = recording_extra(Arc::clone(&ran), Arc::clone(&seen));

    let host = LegacyHost::new(restricted_policy());
    let result = run_with_tools(
        config(format!("http://{address}/v1")),
        vec![],
        "go",
        root.path(),
        host,
        Services::default(),
        RunTools {
            tool_filter: ToolFilter::all(),
            extra_tools: vec![extra],
            ..RunTools::default()
        },
    )
    .await;

    let error = result.expect_err("a restricted run must refuse arbitrary extra tools");
    assert!(error.contains("arbitrary extra tool"), "{error}");
    assert!(
        error.contains("load_policy_bound_tools"),
        "the refusal must name the supported path: {error}"
    );
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "the refused tool must never run"
    );
    assert!(seen.lock().unwrap().is_none());
    assert_no_model_connection(&listener);
}

// ---------------------------------------------------------------------------
// 3. Policy-bound container refuses a mismatched run policy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn policy_bound_tools_refuse_a_mismatched_run_policy() {
    let root = tempfile::tempdir().unwrap();
    let canonical = root.path().canonicalize().unwrap();
    write_plugin(
        &canonical,
        "probe",
        json!([plugin_tool("emit", "touch bump.count")]),
    );

    // Load real plugin tools under one restricted policy. This does not need a
    // working backend because no subprocess is started at load time.
    let loaded_policy = restricted_policy();
    let loader = LegacyHost::new(loaded_policy.clone());
    let bound = load_policy_bound_tools(&canonical, loader, &ToolFilter::all())
        .await
        .expect("plugin tools must load without a backend");
    assert_eq!(
        bound.len(),
        1,
        "the container must carry the real loaded plugin tool"
    );
    assert_eq!(bound.names(), vec!["plugin__probe__emit".to_owned()]);
    assert_eq!(bound.policy_fingerprint(), loaded_policy.fingerprint());

    let other = other_restricted_policy();
    assert_ne!(
        loaded_policy.fingerprint(),
        other.fingerprint(),
        "the test needs two distinguishable policies"
    );

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();

    let result = run_with_tools(
        config(format!("http://{address}/v1")),
        vec![],
        "go",
        &canonical,
        LegacyHost::new(other),
        Services::default(),
        RunTools {
            tool_filter: ToolFilter::all(),
            policy_bound_tools: Some(bound),
            ..RunTools::default()
        },
    )
    .await;

    let error = result.expect_err("a policy mismatch must be refused");
    assert!(error.contains("policy-bound"), "{error}");
    assert!(error.contains("different execution policy"), "{error}");
    assert!(!canonical.join("bump.count").exists());
    assert_no_model_connection(&listener);
}

// ---------------------------------------------------------------------------
// 4. Real plugin + MCP container accepted by a restricted run
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_restricted_run_accepts_real_policy_bound_plugin_and_mcp_tools() {
    let _guard = MCP_GUARD.lock().await;
    shutdown_mcp_connections();

    let root = tempfile::tempdir().unwrap();
    let canonical = root.path().canonicalize().unwrap();
    let Some(policy) = verified_runtime_policy(&canonical).await else {
        return;
    };

    // A real declarative plugin tool that only runs inside the sandbox.
    write_plugin(
        &canonical,
        "probe",
        json!([plugin_tool(
            "emit",
            "echo plugin-ok > plugin-inside.txt && cat plugin-inside.txt"
        )]),
    );
    // A real project stdio MCP server.
    write_mcp_config(&canonical, &mcp_service_script());

    let host = PolicyHost::new(policy.clone());
    let bound = load_policy_bound_tools(&canonical, host.clone(), &ToolFilter::all())
        .await
        .expect("policy-bound plugin and MCP tools must load under a verified sandbox");
    let names = bound.names();
    assert!(
        names.iter().any(|name| name == "plugin__probe__emit"),
        "the plugin tool must be loaded: {names:?}"
    );
    assert!(
        names.iter().any(|name| name == "mcp__probe__probe"),
        "the MCP tool must be loaded: {names:?}"
    );

    let server = MockServer::start(vec![
        openai_tool_stream(&[
            ("call_1", "plugin__probe__emit", json!({})),
            ("call_2", "mcp__probe__probe", json!({})),
        ]),
        openai_text_stream("done"),
    ]);
    let result = run_with_tools(
        config(server.base_url()),
        vec![],
        "go",
        &canonical,
        host,
        Services::default(),
        RunTools {
            tool_filter: ToolFilter::all(),
            policy_bound_tools: Some(bound),
            ..RunTools::default()
        },
    )
    .await;
    assert!(result.is_ok(), "{result:?}");

    let bodies = server.bodies();
    assert!(bodies.len() >= 2, "the turn must reach the model twice");
    let advertised: Vec<String> = bodies[0]["tools"]
        .as_array()
        .expect("the request must advertise tools")
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
        .collect();
    assert!(
        advertised.contains(&"plugin__probe__emit".to_owned()),
        "the plugin tool must be advertised: {advertised:?}"
    );
    assert!(
        advertised.contains(&"mcp__probe__probe".to_owned()),
        "the MCP tool must be advertised: {advertised:?}"
    );

    let tool_result = bodies[1]["messages"]
        .as_array()
        .expect("the continuation must carry messages")
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "call_1")
        .expect("the plugin tool result must be in the continuation")["content"]
        .to_string();
    assert!(tool_result.contains("plugin-ok"), "{tool_result}");
    let mcp_result = bodies[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "call_2")
        .expect("the MCP tool result must be in the continuation")["content"]
        .to_string();
    assert!(mcp_result.contains("mcp-ok"), "{mcp_result}");
    assert_eq!(
        std::fs::read_to_string(canonical.join("plugin-inside.txt")).unwrap(),
        "plugin-ok\n"
    );

    shutdown_mcp_connections();
}

/// A host whose configuration briefly changes and then returns to its initial
/// policy. Merely checking before/after a batch while each loader independently
/// reads the host can otherwise label unrestricted handlers as restricted.
struct ChangingHost {
    policy_reads: AtomicUsize,
    legacy_calls: AtomicUsize,
}

impl ChangingHost {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            policy_reads: AtomicUsize::new(0),
            legacy_calls: AtomicUsize::new(0),
        })
    }
}

impl Host for ChangingHost {
    fn execution_policy(&self) -> ExecutionPolicy {
        if self.policy_reads.fetch_add(1, Ordering::SeqCst) == 1 {
            ExecutionPolicy::Unrestricted
        } else {
            restricted_policy()
        }
    }
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        self.legacy_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok("unrestricted callback".into()) })
    }
    fn on_event(&self, _: AgentEvent) {}
}

#[tokio::test]
async fn workspace_does_not_downgrade_its_captured_execution_policy() {
    let root = tempfile::tempdir().unwrap();
    let host = ChangingHost::new();
    let workspace = Workspace::new(root.path(), host.clone()).unwrap();
    assert!(
        workspace
            .execute(Arguments::shell("touch should-not-run"))
            .await
            .is_err()
    );
    assert_eq!(host.legacy_calls.load(Ordering::SeqCst), 0);
    assert!(!root.path().join("should-not-run").exists());
}

#[tokio::test]
async fn extension_batch_does_not_attest_a_policy_reread_by_individual_loaders() {
    let root = tempfile::tempdir().unwrap();
    write_plugin(
        root.path(),
        "probe",
        json!([plugin_tool("emit", "touch should-not-run")]),
    );
    let result =
        load_policy_bound_tools(root.path(), ChangingHost::new(), &ToolFilter::all()).await;
    assert!(matches!(result, Err(error) if error.contains("policy changed")));
    assert!(!root.path().join("should-not-run").exists());
}
