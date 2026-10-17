//! End-to-end acceptance for the unified tool admission gate (REQ-POLICY),
//! extending `tests/tool_policy.rs` to the real plugin and MCP entry points.
//!
//! Every test drives a real `run_with_tools` turn (or, where caller-owned hook
//! ordering must be pinned, a real `AgentBuilder` run) against a local loopback
//! HTTP/SSE server. Dynamic tools are not hand-made: they are loaded from a real
//! project plugin (`.mio/plugins/*`) and a real project MCP server
//! (`.mio/mcp.json` + `tests/fixtures/mcp_policy_server.py`) through the same
//! `load_dynamic_tools` entry the CLI runner uses, then dispatched through the
//! production `PolicyHook`/`ToolPolicy` at the engine's dispatch boundary.
//!
//! Honest boundaries:
//!
//! * A tool excluded by the run's filter is also dropped from the advertised
//!   tool set, so the engine never reaches the gate: it fails the turn closed
//!   as an invalid tool call. Those cases assert the load-time exclusion, the
//!   fail-closed run and the absence of any side effect, not a gate skip. The
//!   loaded-tool refusals (`read_only`) are the ones that dispatch through the
//!   gate while advertised and assert zero approval and zero execution.
//! * `run_with_tools` hard-codes the 120 s approval budget and exposes no hook
//!   injection, so two assertions cannot be reached through it. The approval
//!   timeout test builds a real agent with `AgentBuilder` and an explicit
//!   `ToolPolicy::with_approval_timeout`, and the "the gate is the last dispatch
//!   hook and binds the final, patched arguments" tests register a test patch
//!   hook before/after `PolicyHook`. Both still dispatch through the production
//!   gate and the production engine; only the surrounding wiring differs.
//! * The delegated-child regression verifies that `ChildHost` forwards the
//!   complete approval binding to the parent, including hosts that enforce
//!   authorization only through `approve_tool_binding`.
//!
//! No external provider is contacted and no credential is read from the
//! environment: the requests carry only the fixed `local-test` token this file
//! sets on `Config`.

#[path = "../src/test_http.rs"]
mod test_http;

use futures_util::StreamExt;
use mio_agent::AgentBuilder;
use mio_agent::agent::{
    AgentHook, DispatchAction, DispatchEvent, HookContext, StepEventKind, StreamingResult,
};
use mio_agent::tool::DynamicTool;
use mio_coding_agent::mcp::{PROJECT_MCP_APPROVAL_OP, load_dynamic_tools};
use mio_coding_agent::policy::{ApprovalBinding, EntryKind, PolicyContext, PolicyHook, ToolPolicy};
use mio_coding_agent::subagent::{Delegate, NativeExecutor, Status, SubagentManager};
use mio_coding_agent::{
    AgentEvent, Config, Host, HostFuture, Provider, RunOptions, RunTools, Services, ToolFilter,
    run_with_tools,
};
use mio_core::DynModel;
use mio_core::effect::EffectKind;
use mio_core::message::ToolName;
use mio_core::operation::Completion;
use mio_core::providers::openai::{OpenAIConfig, wire::Route};
use mio_core::tool::{ToolExecutionError, ToolOutput};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const API_KEY: &str = "local-test";

// ---------------------------------------------------------------------------
// Local mock HTTP/SSE server
// ---------------------------------------------------------------------------

/// Every JSON request body the mock server received, in arrival order.
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

fn body_text(body: &Value) -> String {
    serde_json::to_string(body).unwrap()
}

// ---------------------------------------------------------------------------
// Hosts
// ---------------------------------------------------------------------------

/// Records every bound approval `(operation, target, binding)`, plus the
/// unbound `approve_tool_async` calls (project MCP start approval and, as the
/// finding below shows, delegated child tool approvals).
#[derive(Default)]
struct GateHost {
    allow: bool,
    approvals: Mutex<Vec<(String, String, ApprovalBinding)>>,
    async_approvals: Mutex<Vec<(String, String)>>,
}

impl GateHost {
    fn allowing() -> Self {
        Self {
            allow: true,
            ..Self::default()
        }
    }

    fn bindings(&self) -> Vec<(String, String, ApprovalBinding)> {
        self.approvals.lock().unwrap().clone()
    }

    fn async_ops(&self) -> Vec<(String, String)> {
        self.async_approvals.lock().unwrap().clone()
    }
}

impl Host for GateHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn on_event(&self, _: AgentEvent) {}

    fn approve_tool_binding(
        &self,
        operation: &str,
        target: &str,
        binding: &ApprovalBinding,
    ) -> HostFuture<Result<(), String>> {
        self.approvals.lock().unwrap().push((
            operation.to_owned(),
            target.to_owned(),
            binding.clone(),
        ));
        let allow = self.allow;
        Box::pin(async move {
            if allow {
                Ok(())
            } else {
                Err("denied by test host".to_owned())
            }
        })
    }

    fn approve_tool_async(&self, operation: &str, target: &str) -> HostFuture<Result<(), String>> {
        self.async_approvals
            .lock()
            .unwrap()
            .push((operation.to_owned(), target.to_owned()));
        Box::pin(async { Ok(()) })
    }
}

/// Approves the first normalized parameter digest it sees for a tool and
/// refuses any later call whose digest changed.
#[derive(Default)]
struct PinHost {
    seen: Mutex<BTreeMap<String, String>>,
    approvals: Mutex<Vec<ApprovalBinding>>,
}

impl Host for PinHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn on_event(&self, _: AgentEvent) {}

    fn approve_tool_binding(
        &self,
        _operation: &str,
        _target: &str,
        binding: &ApprovalBinding,
    ) -> HostFuture<Result<(), String>> {
        let outcome: Result<(), String> = {
            let mut seen = self.seen.lock().unwrap();
            match seen.get(&binding.tool) {
                None => {
                    seen.insert(binding.tool.clone(), binding.params_sha256.clone());
                    Ok(())
                }
                Some(previous) if previous == &binding.params_sha256 => Ok(()),
                Some(_) => Err("binding changed after approval".to_owned()),
            }
        };
        self.approvals.lock().unwrap().push(binding.clone());
        Box::pin(async move { outcome })
    }
}

/// Counts approval requests and never answers, to drive the approval timeout.
#[derive(Default)]
struct PendingHost {
    bindings: Mutex<usize>,
}

impl PendingHost {
    fn approvals(&self) -> usize {
        *self.bindings.lock().unwrap()
    }
}

impl Host for PendingHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn on_event(&self, _: AgentEvent) {}

    fn approve_tool_binding(
        &self,
        _operation: &str,
        _target: &str,
        _binding: &ApprovalBinding,
    ) -> HostFuture<Result<(), String>> {
        *self.bindings.lock().unwrap() += 1;
        Box::pin(std::future::pending())
    }
}

// ---------------------------------------------------------------------------
// Dynamic tools
// ---------------------------------------------------------------------------

type SeenArgs = Arc<Mutex<Option<Value>>>;

/// A dynamic tool that records how often it ran and with which arguments.
fn recording_extra(ran: Arc<AtomicUsize>, seen: SeenArgs) -> DynamicTool {
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

fn openai_model(base_url: String) -> DynModel<Completion> {
    OpenAIConfig::new(API_KEY)
        .with_route(Route::Chat)
        .with_base_url(base_url)
        .client()
        .completion("test")
        .into()
}

/// Drains a real engine stream, failing the test on any stream error.
async fn drain(mut stream: StreamingResult) {
    while let Some(item) = stream.next().await {
        if let Err(error) = item {
            panic!("agent stream failed: {error}");
        }
    }
}

/// A dispatch hook that rewrites one tool call's arguments, like any earlier
/// argument-patching hook in a real run.
struct PatchHook {
    tool: &'static str,
    args: Value,
}

impl AgentHook for PatchHook {
    fn observes(&self, kind: StepEventKind) -> bool {
        matches!(kind, StepEventKind::ToolDispatch)
    }

    async fn on_dispatch(&self, _: &HookContext, event: DispatchEvent<'_>) -> DispatchAction {
        match event.kind {
            EffectKind::ToolCall { name, .. } if name.as_str() == self.tool => {
                DispatchAction::rewrite_tool_args(event.kind, self.args.clone())
            }
            _ => DispatchAction::proceed(),
        }
    }
}

// ---------------------------------------------------------------------------
// Project plugin and MCP fixtures
// ---------------------------------------------------------------------------

fn config(base_url: String) -> Config {
    Config {
        provider: Provider::Custom,
        base_url,
        model: "test".into(),
        api_key: API_KEY.into(),
    }
}

fn python_tool(name: &str, script: &str) -> Value {
    json!({
        "name": name,
        "description": format!("{name} policy fixture tool"),
        "parameters": {"type": "object"},
        "command": ["python3", "-c", script],
    })
}

/// Appends to `bump.count` and records the raw `MIO_TOOL_ARGS` in `bump.args`.
fn bump_tool() -> Value {
    python_tool(
        "bump",
        "import os,pathlib;pathlib.Path('bump.args').write_text(os.environ['MIO_TOOL_ARGS']);open('bump.count','a').write('bump'+chr(10))",
    )
}

/// Appends to `other.count`; must never run when `bump` is selected.
fn other_tool() -> Value {
    python_tool("other", "open('other.count','a').write('other'+chr(10))")
}

/// Appends to `read.count`; its short declared name is a real tool-filter token
/// (`read`), which lets a CLI `--exclude-tools read` reach the alias deny path.
fn alias_tool() -> Value {
    python_tool("read", "open('alias.count','a').write('read'+chr(10))")
}

fn write_plugin(root: &Path, name: &str, tools: Value) {
    let dir = root.join(".mio").join("plugins").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let manifest = json!({
        "schema_version": 1,
        "name": name,
        "version": "0.1.0",
        "description": format!("{name} tool-policy fixture"),
        "tools": tools,
    });
    std::fs::write(
        dir.join("mio-plugin.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

fn write_mcp_config(root: &Path, server: &Path) {
    let config = json!({
        "mcpServers": {
            "policyfs": {"command": "python3", "args": ["-u", server.to_str().unwrap()]}
        }
    });
    let dir = root.join(".mio");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mcp.json"),
        serde_json::to_string_pretty(&config).unwrap(),
    )
    .unwrap();
}

fn mcp_fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp_policy_server.py")
}

fn line_count(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

fn mcp_calls(root: &Path) -> Vec<Value> {
    let path = root.join("mcp_calls.log");
    if !path.exists() {
        return Vec::new();
    }
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn tool_names(tools: &[DynamicTool]) -> Vec<String> {
    tools
        .iter()
        .map(|tool| tool.name().as_str().to_owned())
        .collect()
}

/// The two arguments the child-run tests replay through the real engine.
fn workspace_write_and_read() -> String {
    openai_tool_stream(&[
        (
            "call_1",
            "workspace",
            json!({"operation": "write", "path": "guarded.txt", "value": "x"}),
        ),
        (
            "call_2",
            "workspace",
            json!({"operation": "read", "path": "present.txt"}),
        ),
    ])
}

async fn run_project(
    server: &MockServer,
    root: &Path,
    host: Arc<GateHost>,
    filter: ToolFilter,
    extra_tools: Vec<DynamicTool>,
) -> Result<Vec<mio_core::message::Message>, String> {
    run_with_tools(
        config(server.base_url()),
        vec![],
        "go",
        root,
        host,
        Services::default(),
        RunTools {
            tool_filter: filter,
            extra_tools,
            ..RunTools::default()
        },
    )
    .await
}

// ---------------------------------------------------------------------------
// Plugin entry point
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plugin_tool_routes_to_its_own_command_with_bound_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write_plugin(&root, "policyprobe", json!([bump_tool(), other_tool()]));
    let server = MockServer::start(vec![
        openai_tool_stream(&[(
            "call_1",
            "plugin__policyprobe__bump",
            json!({"value": "one"}),
        )]),
        openai_text_stream("done"),
    ]);
    let host = Arc::new(GateHost::allowing());
    let tools = load_dynamic_tools(&root, host.clone(), &ToolFilter::all())
        .await
        .unwrap();
    assert_eq!(
        tool_names(&tools),
        vec![
            "plugin__policyprobe__bump".to_owned(),
            "plugin__policyprobe__other".to_owned()
        ],
        "both declared plugin tools must load"
    );

    let result = run_project(&server, &root, host.clone(), ToolFilter::all(), tools).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(line_count(&root.join("bump.count")), 1);
    assert!(
        !root.join("other.count").exists(),
        "an unselected plugin tool must not run"
    );
    let written: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("bump.args")).unwrap()).unwrap();
    assert_eq!(
        written,
        json!({"value": "one"}),
        "the command must receive the dispatched arguments"
    );

    let bindings = host.bindings();
    assert_eq!(bindings.len(), 1);
    let (operation, target, binding) = &bindings[0];
    assert_eq!(binding.entry, EntryKind::Plugin);
    assert_eq!(operation, "plugin__policyprobe__bump");
    assert_eq!(binding.operation, "plugin__policyprobe__bump");
    assert_eq!(binding.cwd, root.display().to_string());
    assert_eq!(binding.params_sha256.len(), 64);
    assert_eq!(
        serde_json::from_str::<Value>(target).unwrap(),
        json!({"value": "one"}),
        "the approval must bind the arguments the command actually executed"
    );
}

#[tokio::test]
async fn repeated_plugin_dispatch_asks_again_with_a_fresh_call_id() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write_plugin(&root, "policyprobe", json!([bump_tool()]));
    let server = MockServer::start(vec![
        openai_tool_stream(&[(
            "call_1",
            "plugin__policyprobe__bump",
            json!({"value": "same"}),
        )]),
        openai_tool_stream(&[(
            "call_2",
            "plugin__policyprobe__bump",
            json!({"value": "same"}),
        )]),
        openai_text_stream("done"),
    ]);
    let host = Arc::new(GateHost::allowing());
    let tools = load_dynamic_tools(&root, host.clone(), &ToolFilter::all())
        .await
        .unwrap();

    let result = run_project(&server, &root, host.clone(), ToolFilter::all(), tools).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        line_count(&root.join("bump.count")),
        2,
        "each approved dispatch runs once"
    );
    let bindings = host.bindings();
    assert_eq!(bindings.len(), 2, "each dispatch must ask again");
    assert_eq!(
        bindings[0].2.params_sha256, bindings[1].2.params_sha256,
        "identical parameters keep the same normalized digest"
    );
    assert_ne!(
        bindings[0].2.call_id, bindings[1].2.call_id,
        "an approval is never reused for a later call"
    );
}

#[tokio::test]
async fn changed_plugin_arguments_are_refused_after_an_approval() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write_plugin(&root, "policyprobe", json!([bump_tool()]));
    let server = MockServer::start(vec![
        openai_tool_stream(&[(
            "call_1",
            "plugin__policyprobe__bump",
            json!({"value": "one"}),
        )]),
        openai_tool_stream(&[(
            "call_2",
            "plugin__policyprobe__bump",
            json!({"value": "two"}),
        )]),
        openai_text_stream("done"),
    ]);
    let host = Arc::new(PinHost::default());
    let tools = load_dynamic_tools(&root, host.clone(), &ToolFilter::all())
        .await
        .unwrap();

    let result = run_with_tools(
        config(server.base_url()),
        vec![],
        "go",
        &root,
        host.clone(),
        Services::default(),
        RunTools {
            tool_filter: ToolFilter::all(),
            extra_tools: tools,
            ..RunTools::default()
        },
    )
    .await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        line_count(&root.join("bump.count")),
        1,
        "the tampered second call must not execute"
    );
    let written: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("bump.args")).unwrap()).unwrap();
    assert_eq!(written, json!({"value": "one"}));

    let approvals = host.approvals.lock().unwrap().clone();
    assert_eq!(approvals.len(), 2);
    assert_ne!(
        approvals[0].params_sha256, approvals[1].params_sha256,
        "changed parameters must produce a different binding digest"
    );
    let bodies = server.bodies();
    assert!(bodies.len() >= 3);
    assert!(
        body_text(&bodies[2]).contains("binding changed after approval"),
        "the model-visible result must carry the refusal reason"
    );
}

#[tokio::test]
async fn read_only_run_denies_a_loaded_plugin_tool_without_running_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write_plugin(&root, "policyprobe", json!([bump_tool()]));
    let server = MockServer::start(vec![
        openai_tool_stream(&[("call_1", "plugin__policyprobe__bump", json!({"value": "x"}))]),
        openai_text_stream("done"),
    ]);
    let host = Arc::new(GateHost::allowing());
    // The plugin tool is advertised; the read-only gate must refuse it anyway.
    let tools = load_dynamic_tools(&root, host.clone(), &ToolFilter::all())
        .await
        .unwrap();
    assert_eq!(
        tool_names(&tools),
        vec!["plugin__policyprobe__bump".to_owned()]
    );

    let result = run_with_tools(
        config(server.base_url()),
        vec![],
        "go",
        &root,
        host.clone(),
        Services::default(),
        RunTools {
            read_only: true,
            tool_filter: ToolFilter::all(),
            extra_tools: tools,
            ..RunTools::default()
        },
    )
    .await;
    assert!(result.is_ok(), "{result:?}");
    assert!(
        !root.join("bump.count").exists(),
        "a read-only refusal must run no plugin command"
    );
    assert!(
        host.bindings().is_empty() && host.async_ops().is_empty(),
        "a read-only refusal happens before any approval request"
    );
    let bodies = server.bodies();
    assert!(bodies.len() >= 2);
    assert!(
        body_text(&bodies[1]).contains("read-only"),
        "the refusal must be model-visible"
    );
}

#[tokio::test]
async fn plugin_deny_on_qualified_or_short_alias_wins_before_execution() {
    // Short declared name (`read`) denied while the qualified name is allowed.
    run_alias_deny_case(Some("plugin__aliasprobe__read"), Some("read"), false).await;
    // Qualified name denied while the short alias is allowed.
    run_alias_deny_case(Some("read"), Some("plugin__aliasprobe__read"), false).await;
    // Positive control: the same short alias alone does load the tool.
    run_alias_deny_case(Some("read"), None, true).await;
}

async fn run_alias_deny_case(allowed: Option<&str>, excluded: Option<&str>, loads: bool) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write_plugin(&root, "aliasprobe", json!([alias_tool()]));
    let filter = ToolFilter::from_cli(allowed, excluded, false).unwrap();
    let host = Arc::new(GateHost::allowing());
    let tools = load_dynamic_tools(&root, host.clone(), &filter)
        .await
        .unwrap();
    assert_eq!(
        !tool_names(&tools).is_empty(),
        loads,
        "load outcome for allowed={allowed:?} excluded={excluded:?}"
    );

    let server = MockServer::start(vec![
        openai_tool_stream(&[("call_1", "plugin__aliasprobe__read", json!({"path": "x"}))]),
        openai_text_stream("done"),
    ]);
    let result = run_project(&server, &root, host, filter, tools).await;
    if loads {
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(line_count(&root.join("alias.count")), 1);
    } else {
        // The denied name is not advertised, so the engine fails the turn
        // closed before any dispatch and no command can run.
        assert!(
            result.is_err(),
            "a denied tool name must fail closed: {result:?}"
        );
        assert!(
            !root.join("alias.count").exists(),
            "a denied alias must not execute the plugin command"
        );
    }
}

#[tokio::test]
async fn normalized_plugin_name_collision_is_rejected_and_runs_no_tool() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write_plugin(
        &root,
        "a-b",
        json!([python_tool("c", "open('collide_one.count','a').write('1')")]),
    );
    write_plugin(
        &root,
        "a_b",
        json!([python_tool("c", "open('collide_two.count','a').write('2')")]),
    );
    let host = Arc::new(GateHost::allowing());
    let error = load_dynamic_tools(&root, host.clone(), &ToolFilter::all())
        .await
        .unwrap_err();
    assert!(error.contains("collision"), "unexpected error: {error}");
    assert!(
        error.contains("plugin__a_b__c"),
        "the collision must name the normalized tool: {error}"
    );
    assert!(
        !root.join("collide_one.count").exists() && !root.join("collide_two.count").exists(),
        "loading must reject the collision without running any command"
    );

    // The refused duplicate must not be silently routable to either command.
    let server = MockServer::start(vec![
        openai_tool_stream(&[("call_1", "plugin__a_b__c", json!({}))]),
        openai_text_stream("done"),
    ]);
    let result = run_project(&server, &root, host, ToolFilter::all(), vec![]).await;
    assert!(
        result.is_err(),
        "the normalized duplicate is not advertised, so the turn fails closed: {result:?}"
    );
    assert!(
        !root.join("collide_one.count").exists() && !root.join("collide_two.count").exists(),
        "no colliding command may run after the rejection"
    );
}

// ---------------------------------------------------------------------------
// MCP entry point
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mcp_tool_routes_to_the_server_and_binds_the_final_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write_mcp_config(&root, &mcp_fixture());
    let server = MockServer::start(vec![
        openai_tool_stream(&[(
            "call_1",
            "mcp__policyfs__record",
            json!({"path": "note.txt"}),
        )]),
        openai_text_stream("done"),
    ]);
    let host = Arc::new(GateHost::allowing());
    let tools = load_dynamic_tools(&root, host.clone(), &ToolFilter::all())
        .await
        .unwrap();
    let names = tool_names(&tools);
    assert!(
        names.contains(&"mcp__policyfs__record".to_owned()),
        "the MCP tool must load: {names:?}"
    );
    assert!(names.contains(&"mcp__policyfs__noop".to_owned()));

    let result = run_project(&server, &root, host.clone(), ToolFilter::all(), tools).await;
    assert!(result.is_ok(), "{result:?}");
    assert!(
        root.join("mcp_started.log").exists(),
        "the project MCP server must have been started"
    );
    let calls = mcp_calls(&root);
    assert_eq!(
        calls.len(),
        1,
        "exactly one tools/call must reach the server"
    );
    assert_eq!(calls[0]["name"], json!("record"));
    assert_eq!(calls[0]["arguments"], json!({"path": "note.txt"}));

    let bindings = host.bindings();
    assert_eq!(bindings.len(), 1);
    let (operation, target, binding) = &bindings[0];
    assert_eq!(binding.entry, EntryKind::Mcp);
    assert_eq!(operation, "mcp__policyfs__record");
    assert_eq!(binding.cwd, root.display().to_string());
    assert_eq!(binding.params_sha256.len(), 64);
    assert_eq!(
        serde_json::from_str::<Value>(target).unwrap(),
        json!({"path": "note.txt"})
    );
    let async_ops = host.async_ops();
    assert!(
        async_ops
            .iter()
            .any(|(operation, _)| operation == PROJECT_MCP_APPROVAL_OP),
        "starting a project MCP server must be explicitly authorized: {async_ops:?}"
    );
}

#[tokio::test]
async fn read_only_run_denies_a_loaded_mcp_tool_before_any_server_call() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write_mcp_config(&root, &mcp_fixture());
    let server = MockServer::start(vec![
        openai_tool_stream(&[(
            "call_1",
            "mcp__policyfs__record",
            json!({"path": "note.txt"}),
        )]),
        openai_text_stream("done"),
    ]);
    let host = Arc::new(GateHost::allowing());
    // The MCP tool is advertised; the read-only gate must refuse it anyway.
    let tools = load_dynamic_tools(&root, host.clone(), &ToolFilter::all())
        .await
        .unwrap();
    assert!(
        tool_names(&tools).contains(&"mcp__policyfs__record".to_owned()),
        "the MCP tool must load before the read-only dispatch"
    );

    let result = run_with_tools(
        config(server.base_url()),
        vec![],
        "go",
        &root,
        host.clone(),
        Services::default(),
        RunTools {
            read_only: true,
            tool_filter: ToolFilter::all(),
            extra_tools: tools,
            ..RunTools::default()
        },
    )
    .await;
    assert!(result.is_ok(), "{result:?}");
    assert!(
        root.join("mcp_started.log").exists(),
        "the server is started when its tools are loaded"
    );
    assert!(
        mcp_calls(&root).is_empty(),
        "a read-only refusal must never issue tools/call"
    );
    assert!(
        host.bindings().is_empty(),
        "a read-only refusal happens before approval"
    );
    let bodies = server.bodies();
    assert!(bodies.len() >= 2);
    assert!(
        body_text(&bodies[1]).contains("read-only"),
        "the refusal must be model-visible"
    );
}

#[tokio::test]
async fn excluded_mcp_tool_is_not_loaded_and_cannot_be_invoked() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write_mcp_config(&root, &mcp_fixture());
    let filter = ToolFilter::from_cli(None, Some("mcp__policyfs__record"), false).unwrap();
    let host = Arc::new(GateHost::allowing());
    let tools = load_dynamic_tools(&root, host.clone(), &filter)
        .await
        .unwrap();
    assert!(
        !tool_names(&tools).contains(&"mcp__policyfs__record".to_owned()),
        "the excluded MCP tool must not load"
    );

    let server = MockServer::start(vec![openai_tool_stream(&[(
        "call_1",
        "mcp__policyfs__record",
        json!({"path": "note.txt"}),
    )])]);
    let result = run_project(&server, &root, host.clone(), filter, tools).await;
    assert!(
        result.is_err(),
        "the excluded name is not advertised, so the turn fails closed: {result:?}"
    );
    assert!(
        root.join("mcp_started.log").exists(),
        "the server is still started even when its tools are filtered out"
    );
    assert!(
        mcp_calls(&root).is_empty(),
        "a denied MCP tool must never issue tools/call"
    );
}

// ---------------------------------------------------------------------------
// Approval binding scope and timeout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn binding_is_scoped_by_run_and_workspace_root() {
    let dir_a = tempfile::tempdir().unwrap();
    let root_a = dir_a.path().canonicalize().unwrap();
    write_plugin(&root_a, "policyprobe", json!([bump_tool()]));
    let dir_b = tempfile::tempdir().unwrap();
    let root_b = dir_b.path().canonicalize().unwrap();
    write_plugin(&root_b, "policyprobe", json!([bump_tool()]));

    let host = Arc::new(GateHost::allowing());
    let call = openai_tool_stream(&[(
        "call_1",
        "plugin__policyprobe__bump",
        json!({"value": "same"}),
    )]);
    for root in [&root_a, &root_b] {
        let server = MockServer::start(vec![call.clone(), openai_text_stream("done")]);
        let tools = load_dynamic_tools(root, host.clone(), &ToolFilter::all())
            .await
            .unwrap();
        let result = run_project(&server, root, host.clone(), ToolFilter::all(), tools).await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(line_count(&root.join("bump.count")), 1);
    }

    let bindings = host.bindings();
    assert_eq!(bindings.len(), 2);
    assert_eq!(bindings[0].2.cwd, root_a.display().to_string());
    assert_eq!(bindings[1].2.cwd, root_b.display().to_string());
    assert_ne!(
        bindings[0].2.scope, bindings[1].2.scope,
        "each run has its own scope"
    );
    assert_ne!(
        bindings[0].2.params_sha256, bindings[1].2.params_sha256,
        "the digest folds in the run context"
    );
}

#[tokio::test]
async fn approval_timeout_denies_before_the_handler_runs() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![
        openai_tool_stream(&[("call_1", "fixture_extra", json!({"value": "must-not-run"}))]),
        openai_text_stream("done"),
    ]);
    let ran = Arc::new(AtomicUsize::new(0));
    let seen: SeenArgs = Arc::new(Mutex::new(None));
    let host = Arc::new(PendingHost::default());
    let policy = ToolPolicy::new(
        ToolFilter::all(),
        false,
        host.clone(),
        PolicyContext::new(dir.path().to_string_lossy().to_string(), "run-timeout"),
    )
    .with_approval_timeout(Duration::from_millis(50));
    let agent = AgentBuilder::new(openai_model(server.base_url()))
        .preamble("policy test")
        .dynamic_tools(vec![recording_extra(ran.clone(), seen.clone())])
        .build();
    let stream = agent
        .prompt("go")
        .max_turns(4)
        .add_hook(PolicyHook::new(policy))
        .stream();
    drain(stream).await;

    assert_eq!(host.approvals(), 1, "the gate must request approval once");
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "an approval timeout must not run the handler"
    );
    assert!(
        seen.lock().unwrap().is_none(),
        "the handler must never observe arguments after a timeout"
    );
    let bodies = server.bodies();
    assert!(bodies.len() >= 2);
    assert!(
        body_text(&bodies[1]).contains("timed out"),
        "the model-visible result must name the timeout"
    );
}

// ---------------------------------------------------------------------------
// Hook ordering: the gate is the last dispatch hook
// ---------------------------------------------------------------------------

#[tokio::test]
async fn policy_hook_binds_the_final_patched_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![
        openai_tool_stream(&[("call_1", "fixture_extra", json!({"value": "original"}))]),
        openai_text_stream("done"),
    ]);
    let ran = Arc::new(AtomicUsize::new(0));
    let seen: SeenArgs = Arc::new(Mutex::new(None));
    let host = Arc::new(GateHost::allowing());
    let policy = ToolPolicy::new(
        ToolFilter::all(),
        false,
        host.clone(),
        PolicyContext::new(dir.path().to_string_lossy().to_string(), "run-patch"),
    );
    let agent = AgentBuilder::new(openai_model(server.base_url()))
        .preamble("policy test")
        .dynamic_tools(vec![recording_extra(ran.clone(), seen.clone())])
        .build();
    let stream = agent
        .prompt("go")
        .max_turns(4)
        .add_hook(PatchHook {
            tool: "fixture_extra",
            args: json!({"value": "patched"}),
        })
        .add_hook(PolicyHook::new(policy))
        .stream();
    drain(stream).await;

    assert_eq!(ran.load(Ordering::SeqCst), 1);
    assert_eq!(
        *seen.lock().unwrap(),
        Some(json!({"value": "patched"})),
        "the handler must run with the patched arguments"
    );
    let bindings = host.bindings();
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].2.entry, EntryKind::Extra);
    assert_eq!(
        serde_json::from_str::<Value>(&bindings[0].1).unwrap(),
        json!({"value": "patched"}),
        "the approval must bind the final, patched arguments, not the model's"
    );
}

#[tokio::test]
async fn policy_hook_before_a_patch_hook_would_bind_stale_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![
        openai_tool_stream(&[("call_1", "fixture_extra", json!({"value": "original"}))]),
        openai_text_stream("done"),
    ]);
    let ran = Arc::new(AtomicUsize::new(0));
    let seen: SeenArgs = Arc::new(Mutex::new(None));
    let host = Arc::new(GateHost::allowing());
    let policy = ToolPolicy::new(
        ToolFilter::all(),
        false,
        host.clone(),
        PolicyContext::new(dir.path().to_string_lossy().to_string(), "run-late-patch"),
    );
    let agent = AgentBuilder::new(openai_model(server.base_url()))
        .preamble("policy test")
        .dynamic_tools(vec![recording_extra(ran.clone(), seen.clone())])
        .build();
    // Deliberately the wrong order: the gate approves before the patch lands.
    let stream = agent
        .prompt("go")
        .max_turns(4)
        .add_hook(PolicyHook::new(policy))
        .add_hook(PatchHook {
            tool: "fixture_extra",
            args: json!({"value": "patched"}),
        })
        .stream();
    drain(stream).await;

    assert_eq!(ran.load(Ordering::SeqCst), 1);
    let approved = serde_json::from_str::<Value>(&host.bindings()[0].1).unwrap();
    assert_eq!(approved, json!({"value": "original"}));
    assert_eq!(*seen.lock().unwrap(), Some(json!({"value": "patched"})));
    assert_ne!(
        approved,
        seen.lock().unwrap().clone().unwrap(),
        "this is why the production gate must be registered last"
    );
}

// ---------------------------------------------------------------------------
// Delegated children
// ---------------------------------------------------------------------------

fn child_manager(root: &Path, base_url: String) -> SubagentManager {
    SubagentManager::with_executor(
        root,
        config(base_url),
        RunOptions::default(),
        1,
        Duration::from_secs(30),
        Arc::new(NativeExecutor),
    )
    .unwrap()
}

fn child_delegate(read_only: bool) -> Delegate {
    Delegate {
        session: "child".into(),
        goal: "attempt a guarded write".into(),
        prompt: "try the write, then read the present file".into(),
        path: None,
        read_only,
    }
}

#[tokio::test]
async fn child_cannot_widen_the_parent_tool_filter_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("present.txt"), "present-content").unwrap();
    let server = MockServer::start(vec![
        workspace_write_and_read(),
        openai_text_stream("child-done"),
    ]);
    let manager = child_manager(&root, server.base_url());
    // The parent ceiling allows the aggregate and read, but never a write.
    manager.set_permission_ceiling(
        ToolFilter::from_cli(Some("workspace"), Some("write"), false).unwrap(),
        false,
    );
    let host = Arc::new(GateHost::allowing());
    // The child asks for full access; the ceiling still has to narrow it.
    let snapshot = manager
        .delegate_with_options(child_delegate(false), host, None, ToolFilter::all())
        .unwrap();
    let done = manager
        .status(snapshot.run_id, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(done.status, Status::Completed, "{done:?}");
    assert!(
        !root.join("guarded.txt").exists(),
        "a child must not bypass the parent's write ceiling"
    );
    assert!(
        done.output.contains("child-done"),
        "the child run must complete: {:?}",
        done.output
    );
    let bodies = server.bodies();
    assert!(bodies.len() >= 2);
    let second = body_text(&bodies[1]);
    assert!(
        second.contains("present-content"),
        "the read the ceiling still allows must execute"
    );
    assert!(
        second.contains("Tool policy denied"),
        "the refused write must reach the model as a policy denial"
    );
}

#[tokio::test]
async fn read_only_ceiling_cannot_be_lifted_by_a_child_request() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("present.txt"), "present-content").unwrap();
    let server = MockServer::start(vec![
        workspace_write_and_read(),
        openai_text_stream("child-done"),
    ]);
    let manager = child_manager(&root, server.base_url());
    manager.set_permission_ceiling(ToolFilter::all(), true);
    let host = Arc::new(GateHost::allowing());
    let snapshot = manager
        .delegate_with_options(child_delegate(false), host, None, ToolFilter::all())
        .unwrap();
    let done = manager
        .status(snapshot.run_id, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(done.status, Status::Completed, "{done:?}");
    assert!(
        !root.join("guarded.txt").exists(),
        "a read-only ceiling cannot be lifted by read_only=false"
    );
    let bodies = server.bodies();
    assert!(bodies.len() >= 2);
    assert!(body_text(&bodies[1]).contains("Tool policy denied"));
}

/// The child must preserve the parent's parameter-bound approval policy.
#[tokio::test]
async fn child_dispatch_preserves_a_binding_aware_parent_host() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();

    // Parent-level control: the same host does receive and refuse a binding.
    let control_server = MockServer::start(vec![
        openai_tool_stream(&[(
            "call_1",
            "write",
            json!({"path": "guarded.txt", "value": "x"}),
        )]),
        openai_text_stream("done"),
    ]);
    let control_host = Arc::new(GateHost::default());
    let control = run_project(
        &control_server,
        &root,
        control_host.clone(),
        ToolFilter::all(),
        vec![],
    )
    .await;
    assert!(control.is_ok(), "{control:?}");
    assert!(
        !root.join("guarded.txt").exists(),
        "the parent-level write is refused through the binding-aware approval"
    );
    assert_eq!(control_host.bindings().len(), 1);
    assert_eq!(control_host.bindings()[0].2.tool, "write");

    // Child phase: the same bound policy also refuses the child write.
    let child_server = MockServer::start(vec![
        openai_tool_stream(&[(
            "call_1",
            "write",
            json!({"path": "child.json", "value": "x"}),
        )]),
        openai_text_stream("child-done"),
    ]);
    let manager = child_manager(&root, child_server.base_url());
    let child_host = Arc::new(GateHost::default());
    let snapshot = manager
        .delegate_with_options(
            child_delegate(false),
            child_host.clone(),
            None,
            ToolFilter::all(),
        )
        .unwrap();
    let done = manager
        .status(snapshot.run_id, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(done.status, Status::Completed, "{done:?}");
    assert_eq!(child_host.bindings().len(), 1);
    assert_eq!(child_host.bindings()[0].2.tool, "write");
    assert!(
        child_host
            .async_ops()
            .iter()
            .all(|(operation, _)| operation != "write")
    );
    assert!(
        !root.join("child.json").exists(),
        "a child's write cannot bypass bound approval"
    );
}
