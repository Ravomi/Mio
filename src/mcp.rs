//! Native Model Context Protocol (MCP) stdio client and DynamicTool bridge.
//!
//! Discovers MCP servers from `.mio/mcp.json` and the user configuration directory,
//! negotiates JSON-RPC 2.0 over stdio (`initialize`, `tools/list`, `tools/call`),
//! and exposes tools as `mcp__<server>__<tool>` [`DynamicTool`] instances.

use crate::execution_backend::{ExecutionPolicy, SupervisedCommand};
use crate::unknown_effect::{UnknownReason, UnknownSideEffect};
use crate::{AgentEvent, Host, ToolFilter};
use mio_core::{
    message::ToolName,
    tool::{DynamicTool, ToolOutput},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncWriteExt, BufReader, BufWriter},
    process::{ChildStdin, ChildStdout, Command},
    sync::Mutex,
};

use crate::rpc::read_bounded_line;

/// Persisted project-scope MCP approval store (fingerprint trust store).
///
/// Declared here with an explicit `#[path]` so the module compiles without a
/// second declaration in `lib.rs`. Do not register it in both places.
#[path = "mcp_trust.rs"]
pub mod mcp_trust;

const MCP_TIMEOUT: Duration = Duration::from_secs(30);
/// Hard limit for a single JSON-RPC frame in either direction.
const MAX_MCP_FRAME_BYTES: usize = 1024 * 1024;
/// Hard limit for `mcp.json` configuration files.
const MAX_MCP_CONFIG_BYTES: usize = 1024 * 1024;
/// Maximum number of `tools/list` pages followed per server.
const MAX_MCP_TOOL_PAGES: usize = 20;
/// Maximum number of tools accepted from a single server.
const MAX_MCP_TOOLS: usize = 1000;
/// Maximum number of simultaneously cached MCP connections in this process.
///
/// The cache is bounded and evicts the least recently used connection, whose
/// process group is then killed. This is what keeps long-session reuse safe.
const MAX_MCP_CONNECTIONS: usize = 64;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpConfigFile {
    #[serde(default, rename = "mcpServers")]
    pub mcp_servers: BTreeMap<String, McpServerSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerSpec {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub disabled: bool,
    /// Whether the run must abort when this server cannot start.
    ///
    /// `None` (the default) keeps the historical behavior: a configured server
    /// is required. `Some(false)` makes it optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    /// Convenience alias for `required = false`.
    ///
    /// When `optional` is `Some(true)` it wins; `None` keeps the historical
    /// required default, so existing configurations behave unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
}

impl McpServerSpec {
    /// Whether a startup or discovery failure must fail the whole tool load.
    ///
    /// A spec that sets neither field is required, exactly like before the
    /// field existed (fresh-project compatibility).
    pub fn is_required(&self) -> bool {
        if self.optional == Some(true) {
            return false;
        }
        self.required.unwrap_or(true)
    }
}

/// 项目配置命令的信任来源。项目作用域的 MCP 服务器会执行仓库中的命令，
/// 因此在启动前必须获得宿主明确授权（AUD-03）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpOrigin {
    /// 用户级配置（`<config_dir>/mcp.json`）。
    Global,
    /// 项目配置（`.mio/mcp.json`）或项目插件的 MCP 声明。
    Project,
}

/// MCP 服务器 spec 及其信任来源。
#[derive(Debug, Clone)]
pub struct SourcedMcpSpec {
    pub spec: McpServerSpec,
    pub origin: McpOrigin,
}

/// 启动项目 MCP 服务器前请求宿主授权的操作名。
pub const PROJECT_MCP_APPROVAL_OP: &str = "mcp_server_start";

#[cfg(all(test, unix))]
mod lifecycle_tests {
    use super::*;

    #[tokio::test]
    async fn interrupted_requests_require_reconnect_and_drop_kills_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let script = r#"
import json, pathlib, subprocess, sys
child = subprocess.Popen(['sleep', '30'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
pathlib.Path('child.pid').write_text(str(child.pid))
for line in sys.stdin:
    request = json.loads(line)
    if request.get('method') == 'initialize':
        print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':{'protocolVersion':'2024-11-05','capabilities':{}}}), flush=True)
    elif request.get('method') == 'tools/list':
        pathlib.Path('request.started').touch()
"#;
        let spec = McpServerSpec {
            command: "python3".into(),
            args: vec!["-u".into(), "-c".into(), script.into()],
            env: BTreeMap::new(),
            disabled: false,
            required: None,
            optional: None,
        };
        let client = McpClient::connect("lifecycle", &spec, dir.path())
            .await
            .unwrap();
        let pid: i32 = std::fs::read_to_string(dir.path().join("child.pid"))
            .unwrap()
            .parse()
            .unwrap();
        {
            let request = client.list_tools();
            tokio::pin!(request);
            tokio::select! {
                result = &mut request => panic!("request unexpectedly completed: {result:?}"),
                _ = async {
                    tokio::time::timeout(Duration::from_secs(5), async {
                        while !dir.path().join("request.started").exists() {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                    }).await.unwrap();
                } => {}
            }
        }
        let error = tokio::time::timeout(Duration::from_secs(1), client.list_tools())
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.contains("interrupted"));
        drop(client);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                // SAFETY: signal 0 only checks the test-created descendant.
                if unsafe { libc::kill(pid, 0) } != 0 {
                    break;
                }
                #[cfg(target_os = "linux")]
                if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    && stat
                        .rsplit_once(") ")
                        .is_some_and(|(_, tail)| tail.starts_with('Z'))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolInfo {
    pub server: String,
    pub name: String,
    pub qualified_name: String,
    pub description: String,
    pub input_schema: Value,
}

struct StdioTransport {
    _child: crate::shell::Process,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    interrupted: bool,
}

/// Why one MCP request failed.
///
/// The split decides whether a connection may be reused. `Transport` means the
/// connection is unusable and must be discarded without replaying the request;
/// `Response` means the peer or protocol answered and the connection stays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpRequestError {
    /// Connection-level failure: write, flush, read, EOF, timeout or
    /// cancellation. The request must not be replayed.
    Transport(String),
    /// The peer or protocol rejected this request. The connection is healthy.
    Response(String),
}

impl McpRequestError {
    /// The human-readable reason.
    pub fn message(&self) -> &str {
        match self {
            Self::Transport(message) | Self::Response(message) => message,
        }
    }

    fn into_message(self) -> String {
        match self {
            Self::Transport(message) | Self::Response(message) => message,
        }
    }
}

impl std::fmt::Display for McpRequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message())
    }
}

pub struct McpClient {
    server_name: String,
    transport: Mutex<StdioTransport>,
    next_id: AtomicU64,
    /// Set once a transport failure or cancellation made the connection
    /// unusable, so the registry reconnects instead of reusing it.
    dead: AtomicBool,
}

impl McpClient {
    /// Connect with the legacy unrestricted policy.
    ///
    /// This keeps existing callers and tests working unchanged. A host that
    /// wants its MCP servers isolated uses [`McpClient::connect_with_policy`].
    pub async fn connect(
        server_name: &str,
        spec: &McpServerSpec,
        cwd: &Path,
    ) -> Result<Arc<Self>, String> {
        Self::connect_with_policy(server_name, spec, cwd, &ExecutionPolicy::Unrestricted).await
    }

    /// Connect and initialize one MCP server under an execution policy.
    ///
    /// `spec.command` and `spec.args` are kept verbatim, `spec.env` is passed as
    /// explicit configuration (a sandboxed policy never inherits the host
    /// environment), and the child runs in `cwd`. When the policy cannot run,
    /// this returns an error without starting the server, so there is no silent
    /// fallback to an unsandboxed command.
    pub async fn connect_with_policy(
        server_name: &str,
        spec: &McpServerSpec,
        cwd: &Path,
        policy: &ExecutionPolicy,
    ) -> Result<Arc<Self>, String> {
        let request = SupervisedCommand::new(&spec.command, cwd)
            .args(spec.args.iter().cloned())
            .envs(
                spec.env
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone())),
            );
        let native = policy
            .command(&request)
            .map_err(|error| format!("Failed to spawn MCP server '{server_name}': {error}"))?;
        let mut cmd = Command::from(native);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.as_std_mut().process_group(0);
        }
        // Linux parent-death signal: a crashed or killed Mio process must not
        // leave MCP servers behind, because the process-wide cache means the
        // child is usually not dropped before exit.
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt;
            // Capture the parent pid before the fork so the child can detect
            // that the parent already exited between `fork` and `prctl`. In
            // that window `PR_SET_PDEATHSIG` is installed too late and the
            // signal never fires, which would leak an orphaned server.
            let parent_pid = std::process::id() as libc::pid_t;
            // SAFETY: `pre_exec` runs in the forked child before `exec`; the
            // closure only calls the async-signal-safe `prctl` and `getppid`.
            unsafe {
                cmd.as_std_mut().pre_exec(move || {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::getppid() != parent_pid {
                        return Err(std::io::Error::other(
                            "parent process exited before MCP server exec",
                        ));
                    }
                    Ok(())
                });
            }
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn MCP server '{server_name}': {e}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("MCP server '{server_name}' stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("MCP server '{server_name}' stdout unavailable"))?;

        let client = Arc::new(Self {
            server_name: sanitize_identifier(server_name),
            transport: Mutex::new(StdioTransport {
                _child: crate::shell::Process::new(child),
                stdin: BufWriter::new(stdin),
                stdout: BufReader::new(stdout),
                interrupted: false,
            }),
            next_id: AtomicU64::new(1),
            dead: AtomicBool::new(false),
        });

        client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "mio-coding-agent",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            )
            .await?;
        client
            .notify("notifications/initialized", json!({}))
            .await?;

        Ok(client)
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let mut line = self.encode_frame(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))?;
        line.push('\n');
        tokio::time::timeout(MCP_TIMEOUT, async {
            let mut guard = self.transport.lock().await;
            if guard.interrupted {
                return Err("MCP connection was interrupted; reconnect before retrying".into());
            }
            guard.interrupted = true;
            guard
                .stdin
                .write_all(line.as_bytes())
                .await
                .map_err(|e| format!("MCP write failed ({}): {e}", self.server_name))?;
            guard
                .stdin
                .flush()
                .await
                .map_err(|e| format!("MCP flush failed ({}): {e}", self.server_name))?;
            guard.interrupted = false;
            Ok(())
        })
        .await
        .map_err(|_| {
            format!(
                "MCP notification '{method}' timed out on '{}'",
                self.server_name
            )
        })?
    }

    fn encode_frame(&self, msg: Value) -> Result<String, String> {
        let line = serde_json::to_string(&msg).map_err(|e| e.to_string())?;
        if line.len() > MAX_MCP_FRAME_BYTES {
            return Err(format!(
                "MCP request frame to '{}' exceeds {MAX_MCP_FRAME_BYTES} bytes",
                self.server_name
            ));
        }
        Ok(line)
    }

    /// Whether the connection can serve another request.
    ///
    /// A connection is unusable once a transport failure or a cancelled request
    /// interrupted its framing. Callers must discard it instead of replaying the
    /// failed request.
    pub fn is_usable(&self) -> bool {
        if self.dead.load(Ordering::SeqCst) {
            return false;
        }
        match self.transport.try_lock() {
            Ok(guard) => !guard.interrupted,
            // A request is in flight; the connection is still claimed.
            Err(_) => true,
        }
    }

    /// Send one request and return its result as a plain error string.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        self.request_classified(method, params)
            .await
            .map_err(McpRequestError::into_message)
    }

    async fn request_classified(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, McpRequestError> {
        let req_id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut line = self
            .encode_frame(json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "method": method,
                "params": params,
            }))
            .map_err(McpRequestError::Response)?;
        line.push('\n');

        let outcome = tokio::time::timeout(MCP_TIMEOUT, async {
            let mut guard = self.transport.lock().await;
            if guard.interrupted {
                return Err(McpRequestError::Transport(format!(
                    "MCP connection to '{}' was interrupted; reconnect before retrying",
                    self.server_name
                )));
            }
            // Cancellation can interrupt a write or consume part of a frame.
            // Reject reuse unless a complete matching response was received.
            guard.interrupted = true;
            guard.stdin.write_all(line.as_bytes()).await.map_err(|e| {
                McpRequestError::Transport(format!("MCP write failed ({}): {e}", self.server_name))
            })?;
            guard.stdin.flush().await.map_err(|e| {
                McpRequestError::Transport(format!("MCP flush failed ({}): {e}", self.server_name))
            })?;

            loop {
                let frame = read_bounded_line(&mut guard.stdout, MAX_MCP_FRAME_BYTES)
                    .await
                    .map_err(|e| {
                        McpRequestError::Transport(format!(
                            "MCP server '{}': {e}",
                            self.server_name
                        ))
                    })?;
                let Some(trimmed) = frame else {
                    return Err(McpRequestError::Transport(format!(
                        "MCP server '{}' closed stdout unexpectedly",
                        self.server_name
                    )));
                };
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(parsed) = serde_json::from_str::<Value>(&trimmed) else {
                    continue;
                };
                if parsed.get("id").and_then(Value::as_u64) == Some(req_id) {
                    guard.interrupted = false;
                    if let Some(err) = parsed.get("error") {
                        return Err(McpRequestError::Response(format!(
                            "MCP error ({}): {err}",
                            self.server_name
                        )));
                    }
                    return Ok(parsed.get("result").cloned().unwrap_or(Value::Null));
                }
            }
        })
        .await;

        match outcome {
            Ok(result) => {
                if result.is_err() {
                    self.dead.store(true, Ordering::SeqCst);
                }
                result
            }
            Err(_) => {
                self.dead.store(true, Ordering::SeqCst);
                Err(McpRequestError::Transport(format!(
                    "MCP request '{method}' timed out on '{}'",
                    self.server_name
                )))
            }
        }
    }

    /// List every tool the server exposes, following pagination.
    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>, String> {
        self.list_tools_classified()
            .await
            .map_err(McpRequestError::into_message)
    }

    async fn list_tools_classified(&self) -> Result<Vec<McpToolInfo>, McpRequestError> {
        let mut list = Vec::new();
        // Raw name -> normalized name, used to detect non-injective sanitization.
        let mut seen_names: BTreeMap<String, String> = BTreeMap::new();
        let mut seen_cursors: BTreeSet<String> = BTreeSet::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0usize;
        loop {
            pages += 1;
            if pages > MAX_MCP_TOOL_PAGES {
                return Err(McpRequestError::Response(format!(
                    "MCP server '{}' exceeded {MAX_MCP_TOOL_PAGES} tools/list pages",
                    self.server_name
                )));
            }
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let result = self.request_classified("tools/list", params).await?;
            if let Some(tools) = result.get("tools").and_then(Value::as_array) {
                for t in tools {
                    let Some(raw_name) = t.get("name").and_then(Value::as_str) else {
                        continue;
                    };
                    let clean_tool = sanitize_identifier(raw_name);
                    if let Some(previous) = seen_names.get(&clean_tool)
                        && previous != raw_name
                    {
                        return Err(McpRequestError::Response(format!(
                            "MCP server '{}' exposes tools '{previous}' and '{raw_name}' that normalize to the same name '{clean_tool}'",
                            self.server_name
                        )));
                    }
                    if seen_names.contains_key(&clean_tool) {
                        // Identical raw name repeated across pages: skip.
                        continue;
                    }
                    seen_names.insert(clean_tool.clone(), raw_name.to_owned());
                    if list.len() >= MAX_MCP_TOOLS {
                        return Err(McpRequestError::Response(format!(
                            "MCP server '{}' exposes more than {MAX_MCP_TOOLS} tools",
                            self.server_name
                        )));
                    }
                    let qualified_name = format!("mcp__{}__{}", self.server_name, clean_tool);
                    let description = t
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("MCP tool")
                        .to_owned();
                    let input_schema = t
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                    list.push(McpToolInfo {
                        server: self.server_name.clone(),
                        name: raw_name.to_owned(),
                        qualified_name,
                        description,
                        input_schema,
                    });
                }
            }
            match result.get("nextCursor").and_then(Value::as_str) {
                Some(next) if !next.is_empty() => {
                    if !seen_cursors.insert(next.to_owned()) {
                        return Err(McpRequestError::Response(format!(
                            "MCP server '{}' repeated tools/list cursor '{next}'",
                            self.server_name
                        )));
                    }
                    cursor = Some(next.to_owned());
                }
                _ => break,
            }
        }
        Ok(list)
    }

    /// Call one tool. Transport failures are returned as-is; callers decide
    /// whether to discard the connection.
    pub async fn call_tool(&self, tool_name: &str, arguments: Value) -> Result<Value, String> {
        self.call_tool_classified(tool_name, arguments)
            .await
            .map_err(McpRequestError::into_message)
    }

    async fn call_tool_classified(
        &self,
        tool_name: &str,
        arguments: Value,
    ) -> Result<Value, McpRequestError> {
        self.request_classified(
            "tools/call",
            json!({
                "name": tool_name,
                "arguments": arguments,
            }),
        )
        .await
    }
}

fn sanitize_identifier(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Cache key for one live MCP connection.
///
/// The configuration fingerprint and the full execution-policy fingerprint are
/// both part of the key, so a configuration or isolation change simply misses
/// the cache; `acquire_connection` also prunes the superseded entry so its child
/// process group is reaped immediately. Two runs that differ only by policy -
/// including explicit environment values - must never share a live child.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ConnectionKey {
    workspace: PathBuf,
    server: String,
    fingerprint: String,
    /// [`ExecutionPolicy::fingerprint`], credential-free but covering values.
    policy_fingerprint: String,
}

struct CachedConnection {
    client: Arc<McpClient>,
    last_used: u64,
}

#[derive(Default)]
struct ConnectionRegistry {
    entries: BTreeMap<ConnectionKey, CachedConnection>,
    clock: u64,
}

impl ConnectionRegistry {
    fn get_live(&mut self, key: &ConnectionKey) -> Option<Arc<McpClient>> {
        let live = self
            .entries
            .get(key)
            .is_some_and(|entry| entry.client.is_usable());
        if !live {
            // Dropping the entry drops the client, killing its process group.
            self.entries.remove(key);
            return None;
        }
        self.clock += 1;
        let now = self.clock;
        let entry = self.entries.get_mut(key)?;
        entry.last_used = now;
        Some(entry.client.clone())
    }

    fn insert(&mut self, key: ConnectionKey, client: Arc<McpClient>) {
        self.clock += 1;
        self.entries.insert(
            key,
            CachedConnection {
                client,
                last_used: self.clock,
            },
        );
        while self.entries.len() > MAX_MCP_CONNECTIONS {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone());
            match oldest {
                Some(key) => {
                    self.entries.remove(&key);
                }
                None => break,
            }
        }
    }
}

fn connection_registry() -> &'static StdMutex<ConnectionRegistry> {
    static REGISTRY: OnceLock<StdMutex<ConnectionRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| StdMutex::new(ConnectionRegistry::default()))
}

/// Number of live MCP connections currently cached in this process.
///
/// Exposed for diagnostics and tests; it is not a session or run metric.
pub fn live_mcp_connection_count() -> usize {
    match connection_registry().lock() {
        Ok(registry) => registry.entries.len(),
        Err(_) => 0,
    }
}

/// Drop every cached connection, killing the cached child process groups.
///
/// Eviction, the cache bound and the Linux parent-death signal already clean up
/// in normal operation; this is for diagnostics and tests that want a clean
/// slate.
pub fn shutdown_mcp_connections() {
    if let Ok(mut registry) = connection_registry().lock() {
        registry.entries.clear();
    }
}

/// Remove any cached connection for this workspace and server whose fingerprint
/// or execution policy no longer matches, so a configuration or isolation change
/// cannot keep the old command running.
fn prune_superseded(key: &ConnectionKey) {
    if let Ok(mut registry) = connection_registry().lock() {
        registry.entries.retain(|candidate, _| {
            !(candidate.workspace == key.workspace
                && candidate.server == key.server
                && (candidate.fingerprint != key.fingerprint
                    || candidate.policy_fingerprint != key.policy_fingerprint))
        });
    }
}

fn evict_connection(key: &ConnectionKey) {
    if let Ok(mut registry) = connection_registry().lock() {
        registry.entries.remove(key);
    }
}

/// Return a live client for this key, connecting (and caching) one when needed.
///
/// Connection establishment (`initialize`) is safe to retry; a failed
/// `tools/call` never reaches this path, so it is never replayed.
async fn acquire_connection(
    key: &ConnectionKey,
    spec: &McpServerSpec,
    cwd: &Path,
    policy: &ExecutionPolicy,
) -> Result<Arc<McpClient>, String> {
    prune_superseded(key);
    let cached = match connection_registry().lock() {
        Ok(mut registry) => registry.get_live(key),
        Err(_) => None,
    };
    if let Some(client) = cached {
        return Ok(client);
    }
    let client = McpClient::connect_with_policy(&key.server, spec, cwd, policy).await?;
    if let Ok(mut registry) = connection_registry().lock() {
        if let Some(existing) = registry.get_live(key) {
            // Another task connected first; keep that one and drop ours.
            return Ok(existing);
        }
        registry.insert(key.clone(), client.clone());
    }
    Ok(client)
}

/// List tools, reconnecting once when the transport itself failed.
///
/// `tools/list` is read-only, so a single reconnect is safe. `tools/call` never
/// uses this helper, because its side effects are unbounded.
async fn list_tools_with_reconnect(
    key: &ConnectionKey,
    client: &Arc<McpClient>,
    spec: &McpServerSpec,
    cwd: &Path,
    policy: &ExecutionPolicy,
) -> Result<Vec<McpToolInfo>, String> {
    match client.list_tools_classified().await {
        Ok(tools) => Ok(tools),
        Err(McpRequestError::Response(error)) => Err(error),
        Err(McpRequestError::Transport(error)) => {
            evict_connection(key);
            let fresh = acquire_connection(key, spec, cwd, policy)
                .await
                .map_err(|reconnect| format!("{error}; reconnect failed: {reconnect}"))?;
            fresh.list_tools_classified().await.map_err(|error| {
                format!("{error}; reconnected once and did not retry a second time")
            })
        }
    }
}

/// One MCP tool exposed as a dynamic tool, resolved lazily per invocation so a
/// dead connection is replaced on the next call instead of replaying a failed
/// one.
#[derive(Clone)]
struct McpToolRoute {
    key: ConnectionKey,
    spec: McpServerSpec,
    cwd: PathBuf,
    raw_name: String,
    policy: ExecutionPolicy,
}

/// Why one MCP tool call did not return normally.
///
/// The split is typed, never parsed from the message: a peer that answered
/// (`Known`) is a provable failure, while a transport death (`Unknown`) may
/// have reached the server and must be recorded as an unknown side effect.
#[derive(Debug, Clone, PartialEq, Eq)]
enum McpCallFailure {
    /// The server or protocol answered, or the call never left this process.
    Known(String),
    /// The transport died after the request may have reached the server.
    Unknown(String),
}

impl McpCallFailure {
    fn message(&self) -> &str {
        match self {
            Self::Known(message) | Self::Unknown(message) => message,
        }
    }
}

async fn invoke_mcp_tool(route: &McpToolRoute, arguments: Value) -> Result<Value, McpCallFailure> {
    // Acquiring a connection happens before any request is written, so a
    // failure here provably did not call the tool.
    let client = acquire_connection(&route.key, &route.spec, &route.cwd, &route.policy)
        .await
        .map_err(McpCallFailure::Known)?;
    match client
        .call_tool_classified(&route.raw_name, arguments)
        .await
    {
        Ok(value) => Ok(value),
        Err(McpRequestError::Response(error)) => Err(McpCallFailure::Known(error)),
        Err(McpRequestError::Transport(error)) => {
            // The request may have reached the server before the transport died.
            // Discard the connection and report the call as unknown rather than
            // replaying it.
            evict_connection(&route.key);
            Err(McpCallFailure::Unknown(format!(
                "{error}; the call was not retried and any side effect is unknown"
            )))
        }
    }
}

fn load_project_mcp_config(cwd: &Path) -> Result<Option<McpConfigFile>, String> {
    let canonical_cwd = cwd
        .canonicalize()
        .map_err(|e| format!("Failed to canonicalize cwd {}: {e}", cwd.display()))?;
    let local_path = canonical_cwd.join(".mio").join("mcp.json");
    if !local_path.is_file() {
        return Ok(None);
    }
    let content =
        crate::context::read_workspace_bounded(&canonical_cwd, &local_path, MAX_MCP_CONFIG_BYTES)?;
    let parsed = serde_json::from_str(&content)
        .map_err(|e| format!("Invalid MCP config {}: {e}", local_path.display()))?;
    Ok(Some(parsed))
}

/// Load merged MCP server specs from `<config_dir>/mcp.json` and `<cwd>/.mio/mcp.json`.
pub fn load_mcp_specs(cwd: &Path) -> Result<BTreeMap<String, SourcedMcpSpec>, String> {
    let mut merged = BTreeMap::new();
    if let Ok(cfg_dir) = crate::profile::config_directory() {
        let global_path = cfg_dir.join("mcp.json");
        if global_path.is_file() {
            let content = crate::context::read_bounded_utf8(&global_path, MAX_MCP_CONFIG_BYTES)?;
            let parsed: McpConfigFile = serde_json::from_str(&content)
                .map_err(|e| format!("Invalid MCP config {}: {e}", global_path.display()))?;
            merged.extend(parsed.mcp_servers.into_iter().map(|(name, spec)| {
                (
                    name,
                    SourcedMcpSpec {
                        spec,
                        origin: McpOrigin::Global,
                    },
                )
            }));
        }
    }
    if let Some(parsed) = load_project_mcp_config(cwd)? {
        merged.extend(parsed.mcp_servers.into_iter().map(|(name, spec)| {
            (
                name,
                SourcedMcpSpec {
                    spec,
                    origin: McpOrigin::Project,
                },
            )
        }));
    }
    if let Ok(plugin_specs) = crate::plugin::load_plugin_mcp_specs(cwd) {
        merged.extend(plugin_specs.into_iter().map(|(name, spec)| {
            (
                name,
                SourcedMcpSpec {
                    spec,
                    origin: McpOrigin::Project,
                },
            )
        }));
    }
    merged.retain(|_, sourced| !sourced.spec.disabled);
    Ok(merged)
}

/// Connect to all configured MCP servers and build filtered `DynamicTool` instances
/// (including declarative tools bundled in installed Mio plugins).
pub async fn load_dynamic_tools(
    cwd: &Path,
    host: Arc<dyn Host>,
    filter: &ToolFilter,
) -> Result<Vec<DynamicTool>, String> {
    let policy = host.execution_policy();
    load_dynamic_tools_with_policy(cwd, host, filter, policy).await
}

async fn load_dynamic_tools_with_policy(
    cwd: &Path,
    host: Arc<dyn Host>,
    filter: &ToolFilter,
    policy: ExecutionPolicy,
) -> Result<Vec<DynamicTool>, String> {
    if filter.is_disabled() {
        return Ok(Vec::new());
    }
    let specs = load_mcp_specs(cwd)?;
    load_dynamic_tools_from_specs_with_policy(&specs, cwd, host, filter, policy).await
}

async fn load_dynamic_tools_from_specs_with_policy(
    specs: &BTreeMap<String, SourcedMcpSpec>,
    cwd: &Path,
    host: Arc<dyn Host>,
    filter: &ToolFilter,
    policy: ExecutionPolicy,
) -> Result<Vec<DynamicTool>, String> {
    if filter.is_disabled() {
        return Ok(Vec::new());
    }
    let mut dynamic_tools =
        crate::plugin::load_plugin_tools_with_policy(cwd, host.clone(), filter, policy.clone())?;
    if !specs.is_empty() {
        let mcp_tools =
            build_dynamic_tools_from_specs_with_policy(specs, cwd, host, filter, policy).await?;
        dynamic_tools.extend(mcp_tools);
    }
    // No external identity may be registered twice: a silent override would
    // make tool routing dependent on load order.
    let mut seen = BTreeSet::new();
    for tool in &dynamic_tools {
        if !seen.insert(tool.name().as_str().to_owned()) {
            return Err(format!(
                "Dynamic tool name collision: '{}' is registered more than once",
                tool.name()
            ));
        }
    }
    Ok(dynamic_tools)
}

/// Engine-loaded plugin and stdio MCP tools bound to the execution policy they
/// were loaded under.
///
/// The container is deliberately unforgeable from outside this crate: its
/// fields are private and only the native loaders
/// [`load_policy_bound_tools`] and [`load_policy_bound_tools_from_specs`] can construct it. A host cannot hand the engine an arbitrary
/// `DynamicTool` and label it "policy-bound", because the engine re-checks the
/// recorded policy fingerprint against the run's own policy before the tools
/// are registered. There is no public constructor for arbitrary tools and no
/// mutable access to the tool list.
pub struct PolicyBoundTools {
    tools: Vec<DynamicTool>,
    policy_fingerprint: String,
}

impl PolicyBoundTools {
    /// The execution-policy fingerprint these tools were loaded under.
    pub fn policy_fingerprint(&self) -> &str {
        &self.policy_fingerprint
    }

    /// Number of tools in the container.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the container holds no tools.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Names of the tools in the container, in load order.
    pub fn names(&self) -> Vec<String> {
        self.tools
            .iter()
            .map(|tool| tool.name().as_str().to_owned())
            .collect()
    }

    /// Consume the container and return its tools, but only when `policy` still
    /// matches the policy captured at load time.
    ///
    /// This is the crate-internal unwrap the engine uses. A mismatch is refused
    /// instead of registering tools whose subprocesses would run under a
    /// different policy than the run requested. Keeping the comparison here
    /// means a caller cannot unwrap the tools without passing the live policy.
    pub(crate) fn into_tools_for(
        self,
        policy: &ExecutionPolicy,
    ) -> Result<Vec<DynamicTool>, String> {
        let run_fingerprint = policy.fingerprint();
        if run_fingerprint != self.policy_fingerprint {
            return Err(format!(
                "policy-bound tools were loaded under a different execution policy (loaded {}, run {}); refusing to register them",
                self.policy_fingerprint, run_fingerprint
            ));
        }
        Ok(self.tools)
    }
}

/// Load plugin and stdio MCP tools and bind them to the host's execution
/// policy.
///
/// The host's policy is read before and after the load and both fingerprints
/// must be identical, so tools can never be attributed to a policy that changed
/// mid-load. Prefer this over [`load_dynamic_tools`] whenever a run requests
/// isolation: `RunTools::extra_tools` supplied directly by a host is refused
/// under a non-`Unrestricted` policy, while this container is accepted after
/// the engine re-checks the fingerprint against the run policy.
pub async fn load_policy_bound_tools(
    cwd: &Path,
    host: Arc<dyn Host>,
    filter: &ToolFilter,
) -> Result<PolicyBoundTools, String> {
    let specs = if filter.is_disabled() {
        BTreeMap::new()
    } else {
        load_mcp_specs(cwd)?
    };
    load_policy_bound_tools_from_specs(&specs, cwd, host, filter).await
}

/// Load native plugin and stdio MCP tools using explicitly supplied server
/// declarations, without reading global MCP configuration. All handlers share
/// one immutable execution-policy snapshot. This never accepts arbitrary Rust
/// tools; each server is started by the same policy-aware native supervisor.
pub async fn load_policy_bound_tools_from_specs(
    specs: &BTreeMap<String, SourcedMcpSpec>,
    cwd: &Path,
    host: Arc<dyn Host>,
    filter: &ToolFilter,
) -> Result<PolicyBoundTools, String> {
    let policy = host.execution_policy();
    let policy_fingerprint = policy.fingerprint();
    let tools =
        load_dynamic_tools_from_specs_with_policy(specs, cwd, host.clone(), filter, policy).await?;
    let after_fingerprint = host.execution_policy().fingerprint();
    if after_fingerprint != policy_fingerprint {
        return Err(format!(
            "execution policy changed while plugin and MCP tools were loading (before {}, after {}); refusing to register them",
            policy_fingerprint, after_fingerprint
        ));
    }
    Ok(PolicyBoundTools {
        tools,
        policy_fingerprint,
    })
}

pub async fn build_dynamic_tools_from_specs(
    specs: &BTreeMap<String, SourcedMcpSpec>,
    cwd: &Path,
    host: Arc<dyn Host>,
    filter: &ToolFilter,
) -> Result<Vec<DynamicTool>, String> {
    let policy = host.execution_policy();
    build_dynamic_tools_from_specs_with_policy(specs, cwd, host, filter, policy).await
}

async fn build_dynamic_tools_from_specs_with_policy(
    specs: &BTreeMap<String, SourcedMcpSpec>,
    cwd: &Path,
    host: Arc<dyn Host>,
    filter: &ToolFilter,
    policy: ExecutionPolicy,
) -> Result<Vec<DynamicTool>, String> {
    let mut dynamic_tools = Vec::new();
    // `specs` keys are distinct, but different server names can sanitize to the
    // same qualified prefix; reject the collision instead of losing a tool.
    let mut owners: BTreeMap<String, String> = BTreeMap::new();
    let workspace = mcp_trust::canonical_workspace(cwd);
    // The engine starts every server from this load under the same policy the
    // host reported. Its fingerprint is part of the cache key so a policy or
    // credential change never reuses a live child.
    let policy_fingerprint = policy.fingerprint();
    // AUD-03: only a host that explicitly opts in may read or write the
    // user-scope trust store. A host that does not opt in must approve every
    // project server start itself, so an approval granted by another host — or
    // a repository-forged workspace store — is never reusable across hosts.
    let mut trust = if host.allow_persisted_mcp_trust() {
        let store = match host.mcp_trust_directory() {
            Some(directory) => mcp_trust::McpTrustStore::load_in(&workspace, &directory),
            None => mcp_trust::McpTrustStore::load(&workspace),
        };
        if let Some(diagnostic) = store.diagnostic() {
            eprintln!(
                "Mio: MCP trust store {}: {diagnostic}",
                store.path().display()
            );
        }
        Some(store)
    } else {
        None
    };
    for (server_name, sourced) in specs {
        let spec = &sourced.spec;
        if spec.disabled {
            continue;
        }
        let required = spec.is_required();
        let fingerprint = mcp_trust::server_fingerprint(&workspace, server_name, spec);
        // AUD-03：项目配置命令必须在任何发现/过滤逻辑之前获得明确授权，
        // 并且在任何 spawn 之前。这里在 connect 之前调用审批，因此无法借
        // `--tools read` 绕过；只有 opt-in 宿主才复用用户级信任库中的已批准
        // 指纹，非 opt-in 宿主每次都必须自己批准。
        if sourced.origin == McpOrigin::Project
            && !trust
                .as_ref()
                .is_some_and(|store| store.is_trusted(server_name, &fingerprint))
        {
            let target =
                mcp_trust::server_display_target(&workspace, server_name, spec, &fingerprint);
            match host
                .approve_tool_async(PROJECT_MCP_APPROVAL_OP, &target)
                .await
            {
                Ok(()) => {
                    if let Some(store) = trust.as_mut()
                        && let Err(error) = store.record(server_name, &fingerprint)
                    {
                        eprintln!(
                            "Mio: approved project MCP server '{server_name}' could not persist its trust: {error}"
                        );
                    }
                }
                Err(error) => {
                    if required {
                        return Err(format!(
                            "project MCP server '{server_name}' was not approved: {error}"
                        ));
                    }
                    eprintln!(
                        "Mio: optional project MCP server '{server_name}' was not approved ({error}); continuing without it"
                    );
                    continue;
                }
            }
        }

        let key = ConnectionKey {
            workspace: workspace.clone(),
            server: server_name.clone(),
            fingerprint,
            policy_fingerprint: policy_fingerprint.clone(),
        };
        let client = match acquire_connection(&key, spec, cwd, &policy).await {
            Ok(client) => client,
            Err(error) => {
                let message = format!("MCP server '{server_name}' failed to start: {error}");
                if required {
                    return Err(message);
                }
                eprintln!("Mio: optional {message}; continuing without it");
                continue;
            }
        };
        let tools = match list_tools_with_reconnect(&key, &client, spec, cwd, &policy).await {
            Ok(tools) => tools,
            Err(error) => {
                let message = format!("MCP server '{server_name}' failed to list tools: {error}");
                if required {
                    return Err(message);
                }
                eprintln!("Mio: optional {message}; continuing without it");
                continue;
            }
        };
        for info in tools {
            if let Some(previous) = owners.insert(info.qualified_name.clone(), server_name.clone())
            {
                return Err(format!(
                    "MCP tool name collision: '{}' is provided by both servers '{previous}' and '{server_name}'",
                    info.qualified_name
                ));
            }
            if !filter.allows(&info.qualified_name) {
                continue;
            }
            let tool_name = ToolName::new(info.qualified_name.clone())
                .map_err(|_| format!("MCP tool name '{}' is empty", info.qualified_name))?;
            let route = McpToolRoute {
                key: key.clone(),
                spec: spec.clone(),
                cwd: cwd.to_path_buf(),
                raw_name: info.name.clone(),
                policy: policy.clone(),
            };
            let host_clone = host.clone();
            let qualified = info.qualified_name.clone();
            let dyn_tool = DynamicTool::new_with_context(
                tool_name,
                info.description,
                info.input_schema,
                move |ctx, args| {
                    let route = route.clone();
                    let host = host_clone.clone();
                    let qualified = qualified.clone();
                    Box::pin(async move {
                        let target = args.to_string();
                        host.on_event(AgentEvent::ToolStarted {
                            operation: qualified.clone(),
                            target: target.clone(),
                        });
                        // Admission (tool filter and approval) still happens once
                        // at the run's dispatch boundary before this handler runs.
                        // Reusing a cached connection never repeats or bypasses
                        // that per-dispatch gate.
                        let outcome = invoke_mcp_tool(&route, args).await;
                        // A real transport death is a typed unknown, not an error
                        // string: the request may have reached the server, so the
                        // run's fence is tripped before the event is emitted and
                        // the durable record is written as `Unknown`, never
                        // `Executed` or `Failed`.
                        if let Err(McpCallFailure::Unknown(_)) = &outcome {
                            let unknown = UnknownSideEffect::new(
                                qualified.clone(),
                                target.clone(),
                                UnknownReason::McpTransport,
                                crate::tool_identity::current_tool_call_id(),
                            );
                            let fence = crate::unknown_effect::mark_unknown(ctx, unknown.clone());
                            if host.on_unknown_side_effect(&unknown).is_err()
                                && let Some(fence) = &fence
                            {
                                fence.mark_persist_failed();
                            }
                        }
                        let event_result: Result<String, String> = match &outcome {
                            Ok(value) => Ok(value.to_string()),
                            Err(failure) => Err(failure.message().to_owned()),
                        };
                        host.on_event(AgentEvent::ToolFinished {
                            operation: qualified,
                            target,
                            result: event_result,
                        });
                        let mut payload = match outcome {
                            Ok(val) => val,
                            Err(McpCallFailure::Known(err)) | Err(McpCallFailure::Unknown(err)) => {
                                json!({ "error": err })
                            }
                        };
                        if let Some(steer) = host.poll_steering()
                            && !steer.trim().is_empty()
                        {
                            if let Some(obj) = payload.as_object_mut() {
                                obj.insert("steering".into(), json!(steer.trim()));
                            } else {
                                payload = json!({"result": payload, "steering": steer.trim()});
                            }
                        }
                        // A large MCP result is preserved as a bounded preview
                        // plus a workspace-relative reference instead of
                        // flooding the model context. Small results are
                        // returned unchanged as structured JSON; a result over
                        // the 8 MiB hard limit fails the call.
                        let rendered =
                            serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_owned());
                        match crate::shell::externalize_output(&rendered, &route.cwd).await {
                            Ok(text) if text != rendered => Ok(ToolOutput::text(text)),
                            Ok(_) => Ok(ToolOutput::json(payload)),
                            Err(error) => Ok(ToolOutput::json(json!({ "error": error }))),
                        }
                    })
                },
            );
            dynamic_tools.push(dyn_tool);
        }
    }
    Ok(dynamic_tools)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[cfg(unix)]
    #[test]
    fn project_mcp_config_does_not_follow_external_link() {
        use std::os::unix::fs::symlink;

        let (workspace, outside) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mio_dir = workspace.path().join(".mio");
        std::fs::create_dir(&mio_dir).unwrap();
        let outside_config = outside.path().join("mcp.json");
        std::fs::write(&outside_config, "{\"mcpServers\":{}}").unwrap();
        symlink(&outside_config, mio_dir.join("mcp.json")).unwrap();

        let error = load_project_mcp_config(workspace.path()).unwrap_err();
        assert!(error.contains("mcp.json"), "unexpected error: {error}");
    }

    struct NoopHost;
    impl Host for NoopHost {
        fn shell(&self, _: &str, _: &Path) -> crate::HostFuture<Result<String, String>> {
            Box::pin(async { Ok(String::new()) })
        }
        fn on_event(&self, _: AgentEvent) {}
    }

    /// 可配置的项目 MCP 授权宿主，记录收到的审批请求。
    struct GatedHost {
        allow: bool,
        approvals: std::sync::Mutex<Vec<(String, String)>>,
    }
    impl GatedHost {
        fn new(allow: bool) -> Self {
            Self {
                allow,
                approvals: std::sync::Mutex::new(Vec::new()),
            }
        }
    }
    impl Host for GatedHost {
        fn shell(&self, _: &str, _: &Path) -> crate::HostFuture<Result<String, String>> {
            Box::pin(async { Ok(String::new()) })
        }
        fn on_event(&self, _: AgentEvent) {}
        fn approve_tool_async(
            &self,
            operation: &str,
            target: &str,
        ) -> crate::HostFuture<Result<(), String>> {
            self.approvals
                .lock()
                .unwrap()
                .push((operation.to_owned(), target.to_owned()));
            let allow = self.allow;
            Box::pin(async move {
                if allow {
                    Ok(())
                } else {
                    Err("project MCP start denied by test host".to_owned())
                }
            })
        }
    }

    /// 项目作用域的 MCP 服务器，spawn 时会创建 `$MARKER` 文件。
    fn project_specs(marker: &Path) -> BTreeMap<String, SourcedMcpSpec> {
        let script = r#"
touch "$MARKER"
while IFS= read -r line; do
  if echo "$line" | grep -q '"method":"initialize"'; then
    echo '{"jsonrpc":"2.0","id":1,"result":{}}'
  elif echo "$line" | grep -q '"method":"tools/list"'; then
    echo '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"probe","description":"","inputSchema":{}}]}}'
  fi
done
"#;
        let mut env = BTreeMap::new();
        env.insert("MARKER".to_owned(), marker.display().to_string());
        let mut specs = BTreeMap::new();
        specs.insert(
            "proj".to_owned(),
            SourcedMcpSpec {
                spec: McpServerSpec {
                    command: "sh".to_owned(),
                    args: vec!["-c".to_owned(), script.to_owned()],
                    env,
                    disabled: false,
                    required: None,
                    optional: None,
                },
                origin: McpOrigin::Project,
            },
        );
        specs
    }

    #[tokio::test]
    async fn test_project_mcp_requires_approval_before_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("spawned");
        let specs = project_specs(&marker);
        let host = Arc::new(GatedHost::new(false));
        // A denied project server is required by default, so the load fails
        // explicitly instead of silently dropping the server.
        let error =
            build_dynamic_tools_from_specs(&specs, dir.path(), host.clone(), &ToolFilter::all())
                .await
                .unwrap_err();
        assert!(error.contains("proj"), "unexpected error: {error}");
        assert!(
            !marker.exists(),
            "denied project MCP server must not spawn its command"
        );
        let approvals = host.approvals.lock().unwrap().clone();
        assert_eq!(approvals.len(), 1);
        assert_eq!(approvals[0].0, PROJECT_MCP_APPROVAL_OP);
        assert!(approvals[0].1.contains("proj"));
    }

    #[tokio::test]
    async fn test_denied_optional_project_mcp_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("spawned");
        let mut specs = project_specs(&marker);
        specs.get_mut("proj").unwrap().spec.optional = Some(true);
        let host = Arc::new(GatedHost::new(false));
        let tools =
            build_dynamic_tools_from_specs(&specs, dir.path(), host.clone(), &ToolFilter::all())
                .await
                .unwrap();
        assert!(tools.is_empty());
        assert!(!marker.exists());
        assert_eq!(host.approvals.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_trusted_project_mcp_server_starts_and_lists_tools() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("spawned");
        let specs = project_specs(&marker);
        let host = Arc::new(GatedHost::new(true));
        let tools =
            build_dynamic_tools_from_specs(&specs, dir.path(), host.clone(), &ToolFilter::all())
                .await
                .unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "mcp__proj__probe");
        assert!(marker.exists());
        assert_eq!(host.approvals.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_project_mcp_filter_cannot_bypass_approval() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("spawned");
        let specs = project_specs(&marker);
        let host = Arc::new(GatedHost::new(false));
        // 只允许读取工具：审批仍必须先发生，且拒绝后不 spawn。
        let filter = ToolFilter::from_cli(Some("read"), None, false).unwrap();
        let error = build_dynamic_tools_from_specs(&specs, dir.path(), host.clone(), &filter)
            .await
            .unwrap_err();
        assert!(error.contains("proj"), "unexpected error: {error}");
        assert!(!marker.exists());
        assert_eq!(host.approvals.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_mcp_stdio_handshake_list_and_call() {
        let dir = tempfile::tempdir().unwrap();
        let script = r#"
while IFS= read -r line; do
  if echo "$line" | grep -q '"method":"initialize"'; then
    echo '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"mock","version":"1.0"}}}'
  elif echo "$line" | grep -q '"method":"tools/list"'; then
    echo '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"ping","description":"Ping tool","inputSchema":{"type":"object","properties":{"msg":{"type":"string"}}}}]}}'
  elif echo "$line" | grep -q '"method":"tools/call"'; then
    echo '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"pong"}]}}'
  fi
done
"#;
        let mut specs = BTreeMap::new();
        specs.insert(
            "demo".to_owned(),
            McpServerSpec {
                command: "sh".to_owned(),
                args: vec!["-c".to_owned(), script.to_owned()],
                env: BTreeMap::new(),
                disabled: false,
                required: None,
                optional: None,
            },
        );

        let client = McpClient::connect("demo", &specs["demo"], dir.path())
            .await
            .unwrap();
        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].qualified_name, "mcp__demo__ping");

        let res = client
            .call_tool("ping", json!({"msg": "hello"}))
            .await
            .unwrap();
        assert_eq!(res["content"][0]["text"], "pong");

        let sourced: BTreeMap<String, SourcedMcpSpec> = specs
            .iter()
            .map(|(name, spec)| {
                (
                    name.clone(),
                    SourcedMcpSpec {
                        spec: spec.clone(),
                        origin: McpOrigin::Global,
                    },
                )
            })
            .collect();
        let dyn_tools = build_dynamic_tools_from_specs(
            &sourced,
            dir.path(),
            Arc::new(NoopHost),
            &ToolFilter::all(),
        )
        .await
        .unwrap();
        assert_eq!(dyn_tools.len(), 1);
        assert_eq!(dyn_tools[0].name(), "mcp__demo__ping");
    }

    #[tokio::test]
    async fn test_read_bounded_line_rejects_and_drains_oversized_frames() {
        let mut ok = BufReader::new(&b"abc\ndef\n"[..]);
        assert_eq!(
            crate::rpc::read_bounded_line(&mut ok, 16).await.unwrap(),
            Some("abc".to_owned())
        );
        assert_eq!(
            crate::rpc::read_bounded_line(&mut ok, 16).await.unwrap(),
            Some("def".to_owned())
        );
        assert_eq!(
            crate::rpc::read_bounded_line(&mut ok, 16).await.unwrap(),
            None
        );

        let mut oversized = BufReader::new(&b"0123456789\nnext\n"[..]);
        let err = crate::rpc::read_bounded_line(&mut oversized, 8)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("exceeds"), "unexpected error: {err}");
        // The stream stays framed: the following line is still readable.
        assert_eq!(
            crate::rpc::read_bounded_line(&mut oversized, 8)
                .await
                .unwrap(),
            Some("next".to_owned())
        );
    }

    fn make_spec(script: String) -> BTreeMap<String, McpServerSpec> {
        let mut specs = BTreeMap::new();
        specs.insert(
            "fixture".to_owned(),
            McpServerSpec {
                command: "sh".to_owned(),
                args: vec!["-c".to_owned(), script],
                env: BTreeMap::new(),
                disabled: false,
                required: None,
                optional: None,
            },
        );
        specs
    }

    #[tokio::test]
    async fn test_mcp_tools_list_follows_pagination() {
        let dir = tempfile::tempdir().unwrap();
        let script = r#"
while IFS= read -r line; do
  if echo "$line" | grep -q '"method":"initialize"'; then
    echo '{"jsonrpc":"2.0","id":1,"result":{}}'
  elif echo "$line" | grep -q '"method":"tools/list"'; then
    if echo "$line" | grep -q '"cursor":"page2"'; then
      echo '{"jsonrpc":"2.0","id":3,"result":{"tools":[{"name":"second","description":"","inputSchema":{}}]}}'
    else
      echo '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"first","description":"","inputSchema":{}}],"nextCursor":"page2"}}'
    fi
  fi
done
"#;
        let specs = make_spec(script.to_owned());
        let client = McpClient::connect("fixture", &specs["fixture"], dir.path())
            .await
            .unwrap();
        let tools = client.list_tools().await.unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["first", "second"]);
    }

    #[tokio::test]
    async fn test_mcp_normalized_tool_name_collision_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let script = r#"
while IFS= read -r line; do
  if echo "$line" | grep -q '"method":"initialize"'; then
    echo '{"jsonrpc":"2.0","id":1,"result":{}}'
  elif echo "$line" | grep -q '"method":"tools/list"'; then
    echo '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"a.b","description":"","inputSchema":{}},{"name":"a_b","description":"","inputSchema":{}}]}}'
  fi
done
"#;
        let specs = make_spec(script.to_owned());
        let client = McpClient::connect("fixture", &specs["fixture"], dir.path())
            .await
            .unwrap();
        let err = client.list_tools().await.unwrap_err();
        assert!(err.contains("normalize"), "unexpected error: {err}");
        assert!(err.contains("a.b") && err.contains("a_b"));
    }

    #[tokio::test]
    async fn test_mcp_outgoing_oversized_frame_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let script = r#"
while IFS= read -r line; do
  if echo "$line" | grep -q '"method":"initialize"'; then
    echo '{"jsonrpc":"2.0","id":1,"result":{}}'
  fi
done
"#;
        let specs = make_spec(script.to_owned());
        let client = McpClient::connect("fixture", &specs["fixture"], dir.path())
            .await
            .unwrap();
        let huge = "x".repeat(MAX_MCP_FRAME_BYTES + 1);
        let err = client
            .call_tool("probe", json!({ "blob": huge }))
            .await
            .unwrap_err();
        assert!(err.contains("exceeds"), "unexpected error: {err}");
    }

    #[test]
    fn project_mcp_fingerprint_changes_with_config() {
        let dir = tempfile::tempdir().unwrap();
        let base = McpServerSpec {
            command: "sh".to_owned(),
            args: vec!["-c".to_owned(), "true".to_owned()],
            env: BTreeMap::new(),
            disabled: false,
            required: None,
            optional: None,
        };
        assert_eq!(
            mcp_trust::server_fingerprint(dir.path(), "s", &base),
            mcp_trust::server_fingerprint(dir.path(), "s", &base.clone())
        );
        let mut changed_args = base.clone();
        changed_args.args.push("extra".to_owned());
        assert_ne!(
            mcp_trust::server_fingerprint(dir.path(), "s", &base),
            mcp_trust::server_fingerprint(dir.path(), "s", &changed_args)
        );
        let mut changed_env = base.clone();
        changed_env.env.insert("TOKEN".to_owned(), "x".to_owned());
        assert_ne!(
            mcp_trust::server_fingerprint(dir.path(), "s", &base),
            mcp_trust::server_fingerprint(dir.path(), "s", &changed_env)
        );
    }

    #[test]
    fn required_defaults_to_true_and_optional_alias_wins() {
        let mut spec = McpServerSpec {
            command: "sh".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            disabled: false,
            required: None,
            optional: None,
        };
        assert!(spec.is_required());
        spec.required = Some(false);
        assert!(!spec.is_required());
        spec.required = Some(true);
        assert!(spec.is_required());
        spec.optional = Some(true);
        assert!(!spec.is_required());
    }
}
