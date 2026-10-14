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
    collections::BTreeMap,
    path::Path,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};

const MCP_TIMEOUT: Duration = Duration::from_secs(30);

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolInfo {
    pub server: String,
    pub name: String,
    pub qualified_name: String,
    pub description: String,
    pub input_schema: Value,
}

struct StdioTransport {
    _child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
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
                _child: child,
                stdin: BufWriter::new(stdin),
                stdout: BufReader::new(stdout),
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
        let msg = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let mut line = serde_json::to_string(&msg).map_err(|e| e.to_string())?;
        line.push('\n');
        let mut guard = self.transport.lock().await;
        guard
            .stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("MCP write failed ({}): {e}", self.server_name))?;
        guard
            .stdin
            .flush()
            .await
            .map_err(|e| format!("MCP flush failed ({}): {e}", self.server_name))
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let req_id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let msg = json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "method": method,
            "params": params,
        });
        let mut line = serde_json::to_string(&msg).map_err(|e| e.to_string())?;
        line.push('\n');

        let mut guard = self.transport.lock().await;
        tokio::time::timeout(MCP_TIMEOUT, async {
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

            let mut resp_line = String::new();
            loop {
                resp_line.clear();
                let n = guard
                    .stdout
                    .read_line(&mut resp_line)
                    .await
                    .map_err(|e| format!("MCP read failed ({}): {e}", self.server_name))?;
                if n == 0 {
                    return Err(format!(
                        "MCP server '{}' closed stdout unexpectedly",
                        self.server_name
                    ));
                }
                let trimmed = resp_line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(parsed) = serde_json::from_str::<Value>(trimmed) else {
                    continue;
                };
                if parsed.get("id").and_then(Value::as_u64) == Some(req_id) {
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
        let result = self.request("tools/list", json!({})).await?;
        let Some(tools) = result.get("tools").and_then(Value::as_array) else {
            return Ok(Vec::new());
        };
        let mut list = Vec::new();
        for t in tools {
            let Some(raw_name) = t.get("name").and_then(Value::as_str) else {
                continue;
            };
            let clean_tool = sanitize_identifier(raw_name);
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

/// Load merged MCP server specs from `<config_dir>/mcp.json` and `<cwd>/.mio/mcp.json`.
pub fn load_mcp_specs(cwd: &Path) -> Result<BTreeMap<String, McpServerSpec>, String> {
    let mut merged = BTreeMap::new();
    if let Ok(cfg_dir) = crate::profile::config_directory() {
        let global_path = cfg_dir.join("mcp.json");
        if global_path.is_file() {
            let content = std::fs::read_to_string(&global_path)
                .map_err(|e| format!("Failed to read {}: {e}", global_path.display()))?;
            let parsed: McpConfigFile = serde_json::from_str(&content)
                .map_err(|e| format!("Invalid MCP config {}: {e}", global_path.display()))?;
            merged.extend(parsed.mcp_servers);
        }
    }
    let local_path = cwd.join(".mio").join("mcp.json");
    if local_path.is_file() {
        let content = std::fs::read_to_string(&local_path)
            .map_err(|e| format!("Failed to read {}: {e}", local_path.display()))?;
        let parsed: McpConfigFile = serde_json::from_str(&content)
            .map_err(|e| format!("Invalid MCP config {}: {e}", local_path.display()))?;
        merged.extend(parsed.mcp_servers);
    }
    if let Ok(plugin_specs) = crate::plugin::load_plugin_mcp_specs(cwd) {
        merged.extend(plugin_specs);
    }
    merged.retain(|_, spec| !spec.disabled);
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
    Ok(dynamic_tools)
}

pub async fn build_dynamic_tools_from_specs(
    specs: &BTreeMap<String, McpServerSpec>,
    cwd: &Path,
    host: Arc<dyn Host>,
    filter: &ToolFilter,
) -> Result<Vec<DynamicTool>, String> {
    let mut dynamic_tools = Vec::new();
    for (server_name, spec) in specs {
        if spec.disabled {
            continue;
        }
        let client = McpClient::connect(server_name, spec, cwd).await?;
        let tools = client.list_tools().await?;
        for info in tools {
            if !filter.allows(&info.qualified_name) {
                continue;
            }
            let Ok(tool_name) = ToolName::new(info.qualified_name.clone()) else {
                continue;
            };
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

        let dyn_tools = build_dynamic_tools_from_specs(
            &specs,
            dir.path(),
            Arc::new(NoopHost),
            &ToolFilter::all(),
        )
        .await
        .unwrap();
        assert_eq!(dyn_tools.len(), 1);
        assert_eq!(dyn_tools[0].name(), "mcp__demo__ping");
    }
}
