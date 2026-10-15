//! Native Model Context Protocol (MCP) stdio client and DynamicTool bridge.
//!
//! Discovers MCP servers from `.mio/mcp.json` and the user configuration directory,
//! negotiates JSON-RPC 2.0 over stdio (`initialize`, `tools/list`, `tools/call`),
//! and exposes tools as `mcp__<server>__<tool>` [`DynamicTool`] instances.

use crate::{AgentEvent, Host, ToolFilter};
use mio_core::{
    message::ToolName,
    tool::{DynamicTool, ToolOutput},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncWriteExt, BufReader, BufWriter},
    process::{ChildStdin, ChildStdout, Command},
    sync::Mutex,
};

use crate::rpc::read_bounded_line;

const MCP_TIMEOUT: Duration = Duration::from_secs(30);
/// Hard limit for a single JSON-RPC frame in either direction.
const MAX_MCP_FRAME_BYTES: usize = 1024 * 1024;
/// Hard limit for `mcp.json` configuration files.
const MAX_MCP_CONFIG_BYTES: usize = 1024 * 1024;
/// Maximum number of `tools/list` pages followed per server.
const MAX_MCP_TOOL_PAGES: usize = 20;
/// Maximum number of tools accepted from a single server.
const MAX_MCP_TOOLS: usize = 1000;

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

pub struct McpClient {
    server_name: String,
    transport: Mutex<StdioTransport>,
    next_id: AtomicU64,
}

impl McpClient {
    pub async fn connect(
        server_name: &str,
        spec: &McpServerSpec,
        cwd: &Path,
    ) -> Result<Arc<Self>, String> {
        let mut cmd = Command::new(&spec.command);
        cmd.args(&spec.args)
            .current_dir(cwd)
            .envs(&spec.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.as_std_mut().process_group(0);
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

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let req_id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut line = self.encode_frame(json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "method": method,
            "params": params,
        }))?;
        line.push('\n');

        tokio::time::timeout(MCP_TIMEOUT, async {
            let mut guard = self.transport.lock().await;
            if guard.interrupted {
                return Err("MCP connection was interrupted; reconnect before retrying".into());
            }
            // Cancellation can interrupt a write or consume part of a frame.
            // Reject reuse unless a complete matching response was received.
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

            loop {
                let frame = read_bounded_line(&mut guard.stdout, MAX_MCP_FRAME_BYTES)
                    .await
                    .map_err(|e| format!("MCP server '{}': {e}", self.server_name))?;
                let Some(trimmed) = frame else {
                    return Err(format!(
                        "MCP server '{}' closed stdout unexpectedly",
                        self.server_name
                    ));
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
                        return Err(format!("MCP error ({}): {err}", self.server_name));
                    }
                    return Ok(parsed.get("result").cloned().unwrap_or(Value::Null));
                }
            }
        })
        .await
        .map_err(|_| format!("MCP request '{method}' timed out on '{}'", self.server_name))?
    }

    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>, String> {
        let mut list = Vec::new();
        // Raw name -> normalized name, used to detect non-injective sanitization.
        let mut seen_names: BTreeMap<String, String> = BTreeMap::new();
        let mut seen_cursors: BTreeSet<String> = BTreeSet::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0usize;
        loop {
            pages += 1;
            if pages > MAX_MCP_TOOL_PAGES {
                return Err(format!(
                    "MCP server '{}' exceeded {MAX_MCP_TOOL_PAGES} tools/list pages",
                    self.server_name
                ));
            }
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let result = self.request("tools/list", params).await?;
            if let Some(tools) = result.get("tools").and_then(Value::as_array) {
                for t in tools {
                    let Some(raw_name) = t.get("name").and_then(Value::as_str) else {
                        continue;
                    };
                    let clean_tool = sanitize_identifier(raw_name);
                    if let Some(previous) = seen_names.get(&clean_tool)
                        && previous != raw_name
                    {
                        return Err(format!(
                            "MCP server '{}' exposes tools '{previous}' and '{raw_name}' that normalize to the same name '{clean_tool}'",
                            self.server_name
                        ));
                    }
                    if seen_names.contains_key(&clean_tool) {
                        // Identical raw name repeated across pages: skip.
                        continue;
                    }
                    seen_names.insert(clean_tool.clone(), raw_name.to_owned());
                    if list.len() >= MAX_MCP_TOOLS {
                        return Err(format!(
                            "MCP server '{}' exposes more than {MAX_MCP_TOOLS} tools",
                            self.server_name
                        ));
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
                        return Err(format!(
                            "MCP server '{}' repeated tools/list cursor '{next}'",
                            self.server_name
                        ));
                    }
                    cursor = Some(next.to_owned());
                }
                _ => break,
            }
        }
        Ok(list)
    }

    pub async fn call_tool(&self, tool_name: &str, arguments: Value) -> Result<Value, String> {
        self.request(
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

/// 生成项目 MCP 服务器的稳定指纹（命令、参数、环境变量）。
///
/// 配置一旦变化指纹即变化，因此授权绑定具体内容，而不是仅凭 cwd 长期信任。
fn project_server_fingerprint(server_name: &str, spec: &McpServerSpec) -> String {
    let mut env_pairs: Vec<String> = spec
        .env
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    env_pairs.sort();
    let raw = format!(
        "server={server_name}; command={}; args=[{}]; env=[{}]",
        spec.command,
        spec.args.join(" "),
        env_pairs.join(" ")
    );
    if raw.chars().count() <= 1024 {
        raw
    } else {
        let mut truncated: String = raw.chars().take(1024).collect();
        truncated.push_str("...");
        truncated
    }
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
    let local_path = cwd.join(".mio").join("mcp.json");
    if local_path.is_file() {
        let content = crate::context::read_bounded_utf8(&local_path, MAX_MCP_CONFIG_BYTES)?;
        let parsed: McpConfigFile = serde_json::from_str(&content)
            .map_err(|e| format!("Invalid MCP config {}: {e}", local_path.display()))?;
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
    if filter.is_disabled() {
        return Ok(Vec::new());
    }
    let mut dynamic_tools = crate::plugin::load_plugin_tools(cwd, host.clone(), filter)?;
    let specs = load_mcp_specs(cwd)?;
    if !specs.is_empty() {
        let mcp_tools = build_dynamic_tools_from_specs(&specs, cwd, host, filter).await?;
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

pub async fn build_dynamic_tools_from_specs(
    specs: &BTreeMap<String, SourcedMcpSpec>,
    cwd: &Path,
    host: Arc<dyn Host>,
    filter: &ToolFilter,
) -> Result<Vec<DynamicTool>, String> {
    let mut dynamic_tools = Vec::new();
    // `specs` keys are distinct, but different server names can sanitize to the
    // same qualified prefix; reject the collision instead of losing a tool.
    let mut owners: BTreeMap<String, String> = BTreeMap::new();
    for (server_name, sourced) in specs {
        let spec = &sourced.spec;
        if spec.disabled {
            continue;
        }
        // AUD-03：项目配置命令必须在任何发现/过滤逻辑之前获得明确授权。
        // 这里在 connect（即 spawn）之前调用审批，因此无法借 `--tools read` 绕过。
        if sourced.origin == McpOrigin::Project {
            let fingerprint = project_server_fingerprint(server_name, spec);
            if let Err(error) = host
                .approve_tool_async(PROJECT_MCP_APPROVAL_OP, &fingerprint)
                .await
            {
                eprintln!("Mio: project MCP server '{server_name}' was not started: {error}");
                continue;
            }
        }
        let client = McpClient::connect(server_name, spec, cwd).await?;
        let tools = client.list_tools().await?;
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
            let client_clone = client.clone();
            let host_clone = host.clone();
            let qualified = info.qualified_name.clone();
            let raw_name = info.name.clone();
            let dyn_tool = DynamicTool::new_with_context(
                tool_name,
                info.description,
                info.input_schema,
                move |_ctx, args| {
                    let client = client_clone.clone();
                    let host = host_clone.clone();
                    let qualified = qualified.clone();
                    let raw_name = raw_name.clone();
                    Box::pin(async move {
                        let target = args.to_string();
                        host.on_event(AgentEvent::ToolStarted {
                            operation: qualified.clone(),
                            target: target.clone(),
                        });
                        let outcome = match host.approve_tool_async(&qualified, &target).await {
                            Ok(()) => client.call_tool(&raw_name, args).await,
                            Err(err) => Err(err),
                        };
                        host.on_event(AgentEvent::ToolFinished {
                            operation: qualified,
                            target,
                            result: outcome.as_ref().map(Value::to_string).map_err(Clone::clone),
                        });
                        let mut payload = match outcome {
                            Ok(val) => val,
                            Err(err) => json!({ "error": err }),
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
                        Ok(ToolOutput::json(payload))
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
        let tools =
            build_dynamic_tools_from_specs(&specs, dir.path(), host.clone(), &ToolFilter::all())
                .await
                .unwrap();
        assert!(tools.is_empty());
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
        // 只允许读取工具：审批仍必须先发生。
        let filter = ToolFilter::from_cli(Some("read"), None, false).unwrap();
        let tools = build_dynamic_tools_from_specs(&specs, dir.path(), host.clone(), &filter)
            .await
            .unwrap();
        assert!(tools.is_empty());
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
        let base = McpServerSpec {
            command: "sh".to_owned(),
            args: vec!["-c".to_owned(), "true".to_owned()],
            env: BTreeMap::new(),
            disabled: false,
        };
        assert_eq!(
            project_server_fingerprint("s", &base),
            project_server_fingerprint("s", &base.clone())
        );
        let mut changed_args = base.clone();
        changed_args.args.push("extra".to_owned());
        assert_ne!(
            project_server_fingerprint("s", &base),
            project_server_fingerprint("s", &changed_args)
        );
        let mut changed_env = base.clone();
        changed_env.env.insert("TOKEN".to_owned(), "x".to_owned());
        assert_ne!(
            project_server_fingerprint("s", &base),
            project_server_fingerprint("s", &changed_env)
        );
    }
}
