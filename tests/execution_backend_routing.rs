//! Routing and real-isolation tests for the pluggable execution backend.
//!
//! These tests drive the engine's own entry points - `shell`, declarative
//! plugin tools, MCP servers and subagents - and prove that the host
//! [`mio_coding_agent::Host::execution_policy`] actually reaches them:
//!
//! - a bubblewrap policy confines out-of-workspace reads and writes for shell,
//!   plugin and MCP processes;
//! - an unavailable backend fails closed instead of silently running the
//!   command unsandboxed;
//! - the MCP connection cache is keyed by the full policy, so changing the
//!   isolation or an explicit credential never reuses a live child;
//! - a delegated subagent inherits the parent policy;
//! - the default unrestricted policy keeps every entry point working.
//!
//! Real bubblewrap cases run on Linux when the helper exists and the kernel
//! allows unprivileged namespaces. Set `MIO_REQUIRE_BWRAP=1` to turn an
//! unverified sandbox into a failure instead of a skip.
#![cfg(unix)]

use mio_coding_agent::{
    AgentEvent, Config, Host, HostFuture, Message, RunOptions, ToolFilter,
    execution_backend::{
        BackendError, BackendProbe, BubblewrapBackend, BubblewrapPolicy, EnvironmentPolicy,
        ExecutionBackend, ExecutionPolicy, ExecutionRequest, FilesystemPolicy, HostPlatform,
        SupervisedCommand, linux_runtime_mounts,
    },
    mcp::{
        self, McpOrigin, McpServerSpec, SourcedMcpSpec, live_mcp_connection_count,
        shutdown_mcp_connections,
    },
    plugin, shell,
    subagent::{Delegate, Executor, SubagentManager, Task},
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Mutex as AsyncMutex;

/// A host that reports one execution policy and approves project MCP servers.
struct PolicyHost {
    policy: ExecutionPolicy,
    approvals: Mutex<Vec<String>>,
}

impl PolicyHost {
    fn new(policy: ExecutionPolicy) -> Arc<Self> {
        Arc::new(Self {
            policy,
            approvals: Mutex::new(Vec::new()),
        })
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

    fn approve_tool_async(&self, operation: &str, target: &str) -> HostFuture<Result<(), String>> {
        self.approvals
            .lock()
            .unwrap()
            .push(format!("{operation}:{target}"));
        Box::pin(async { Ok(()) })
    }
}

/// The ExplicitMounts runtime policy used by the CLI for a real sandbox.
fn runtime_policy() -> ExecutionPolicy {
    ExecutionPolicy::Bubblewrap(BubblewrapPolicy::default().with_filesystem(
        FilesystemPolicy::ExplicitMounts {
            read_only: linux_runtime_mounts(),
        },
    ))
}

/// Return a verified sandbox policy, or `None` when bubblewrap is unavailable
/// and the environment does not require it.
async fn verified_runtime_policy(workspace: &Path) -> Option<ExecutionPolicy> {
    let policy = runtime_policy();
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

fn unique_token() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    format!("mio-routing-{}-{nanos}", std::process::id())
}

fn install_plugin_tool(cwd: &Path, name: &str, script: &str) {
    install_plugin_command(
        cwd,
        name,
        vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()],
    );
}

fn install_plugin_command(cwd: &Path, name: &str, command: Vec<String>) {
    let directory = cwd.join(".mio").join("plugins").join(name);
    std::fs::create_dir_all(&directory).unwrap();
    let manifest = json!({
        "schema_version": 1,
        "name": name,
        "version": "0.1.0",
        "description": "routing fixture",
        "tools": [{
            "name": "probe",
            "description": "routing probe",
            "parameters": {"type": "object", "properties": {}},
            "command": command,
        }]
    });
    std::fs::write(
        directory.join("mio-plugin.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
}

fn mcp_specs(server: &str, script: &str, env: &[(&str, &str)]) -> BTreeMap<String, SourcedMcpSpec> {
    let mut environment = BTreeMap::new();
    for (name, value) in env {
        environment.insert((*name).to_owned(), (*value).to_owned());
    }
    let mut specs = BTreeMap::new();
    specs.insert(
        server.to_owned(),
        SourcedMcpSpec {
            spec: McpServerSpec {
                command: "python3".to_owned(),
                args: vec!["-u".to_owned(), "-c".to_owned(), script.to_owned()],
                env: environment,
                disabled: false,
                required: None,
                optional: None,
            },
            origin: McpOrigin::Global,
        },
    );
    specs
}

/// Minimal MCP server: run `preamble`, then serve `initialize` and
/// `tools/list`. The id is echoed from each request so a cached connection can
/// serve more than one request.
fn mcp_service_script(preamble: &str) -> String {
    format!(
        r#"{preamble}
import json
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
        result = {{}}
    elif method == "tools/list":
        result = {{"tools": [{{"name": "probe", "description": "", "inputSchema": {{}}}}]}}
    else:
        continue
    print(json.dumps({{"jsonrpc": "2.0", "id": request.get("id"), "result": result}}), flush=True)
"#
    )
}

/// MCP server that records one line per spawn, then serves the protocol.
fn mcp_spawn_script() -> String {
    mcp_service_script(
        "import pathlib\nwith open(\"mcp-spawns.txt\", \"a\", encoding=\"utf-8\") as handle:\n    handle.write(\"up\\n\")",
    )
}

/// MCP server that probes confinement, then serves the protocol.
fn mcp_isolation_script(token: &str) -> String {
    mcp_service_script(&format!(
        r#"import os
import pathlib

lines = []
lines.append("probe=" + os.environ.get("MIO_MCP_PROBE", ""))
lines.append("home=" + os.environ.get("HOME", ""))
try:
    pathlib.Path("/etc/{token}").write_text("x", encoding="utf-8")
    lines.append("etc-write=namespace")
except OSError:
    lines.append("etc-write=refused")
try:
    pathlib.Path("/etc/passwd").read_bytes()
    lines.append("passwd=readable")
except OSError:
    lines.append("passwd=hidden")
pathlib.Path("mcp-report.txt").write_text("\n".join(lines) + "\n", encoding="utf-8")"#
    ))
}

fn spawn_count(cwd: &Path) -> usize {
    std::fs::read_to_string(cwd.join("mcp-spawns.txt"))
        .map(|text| text.lines().filter(|line| !line.trim().is_empty()).count())
        .unwrap_or(0)
}

/// Extract the human-readable payload from a serialized `ToolOutput`, so
/// assertions are not confused by JSON escaping of the inner report.
fn tool_output_text(value: &serde_json::Value) -> String {
    let mut text = String::new();
    if let Some(items) = value.as_array() {
        for item in items {
            let Some(payload) = item.get("value") else {
                continue;
            };
            if let Some(output) = payload.get("output").and_then(serde_json::Value::as_str) {
                text.push_str(output);
            } else if let Some(payload) = payload.as_str() {
                text.push_str(payload);
            } else {
                text.push_str(&payload.to_string());
            }
        }
    }
    text
}

#[tokio::test]
async fn default_policy_keeps_every_entry_point_working() {
    let _guard = MCP_GUARD.lock().await;
    shutdown_mcp_connections();
    let cwd = tempfile::tempdir().unwrap();
    let policy = ExecutionPolicy::Unrestricted;
    let host = PolicyHost::new(policy.clone());
    assert_eq!(host.execution_policy().name(), "unrestricted");
    assert_eq!(policy.status().probe, BackendProbe::NotProbed);

    // Shell: the unrestricted branch keeps the legacy host behavior.
    let output = shell::execute_with_policy("printf direct-ok", cwd.path(), &policy)
        .await
        .unwrap();
    assert!(output.contains("direct-ok"), "{output}");

    // Plugin tool.
    install_plugin_tool(cwd.path(), "probe", "printf plugin-ok");
    let tools = plugin::load_plugin_tools(cwd.path(), host.clone(), &ToolFilter::all()).unwrap();
    assert_eq!(tools.len(), 1);
    let rendered = serde_json::to_string(&tools[0].execute(json!({})).await.unwrap()).unwrap();
    assert!(rendered.contains("plugin-ok"), "{rendered}");

    // MCP server.
    let specs = mcp_specs("probe", &mcp_service_script(""), &[("MIO_MCP_PROBE", "ok")]);
    let tools = mcp::build_dynamic_tools_from_specs(&specs, cwd.path(), host, &ToolFilter::all())
        .await
        .unwrap();
    assert_eq!(tools.len(), 1);
    shutdown_mcp_connections();
}

#[tokio::test]
async fn shell_with_policy_confines_reads_and_writes() {
    let cwd = tempfile::tempdir().unwrap();
    let Some(policy) = verified_runtime_policy(cwd.path()).await else {
        return;
    };
    let token = unique_token();
    let command = format!(
        "echo inside-ok > inside.txt && cat inside.txt; \
         if echo x > /etc/{token} 2>/dev/null; then echo etc-write=namespace; else echo etc-write=refused; fi; \
         if head -c 24 /etc/passwd >/dev/null 2>&1; then echo passwd=readable; else echo passwd=hidden; fi"
    );
    let output = shell::execute_with_policy(&command, cwd.path(), &policy)
        .await
        .unwrap();
    assert!(output.contains("inside-ok"), "{output}");
    assert!(output.contains("passwd=hidden"), "{output}");
    assert!(
        cwd.path().join("inside.txt").is_file(),
        "a workspace write must succeed"
    );
    assert!(
        !Path::new(&format!("/etc/{token}")).exists(),
        "the sandbox must not write through to the host /etc"
    );
}

#[tokio::test]
async fn plugin_tool_follows_the_host_policy() {
    let cwd = tempfile::tempdir().unwrap();
    let Some(policy) = verified_runtime_policy(cwd.path()).await else {
        return;
    };
    let token = unique_token();
    let script = format!(
        "echo \"args=$MIO_TOOL_ARGS\"; echo \"home=$HOME\"; \
         if echo x > \"/etc/{token}\" 2>/dev/null; then echo etc-write=namespace; else echo etc-write=refused; fi; \
         if head -c 24 /etc/passwd >/dev/null 2>&1; then echo passwd=readable; else echo passwd=hidden; fi; \
         echo inside-ok > plugin-inside.txt && cat plugin-inside.txt"
    );
    install_plugin_tool(cwd.path(), "probe", &script);
    let host = PolicyHost::new(policy);
    let tools = plugin::load_plugin_tools(cwd.path(), host, &ToolFilter::all()).unwrap();
    assert_eq!(tools.len(), 1);
    let outcome = tools[0].execute(json!({"probe": true})).await.unwrap();
    let rendered = tool_output_text(&serde_json::to_value(&outcome).unwrap());

    let canonical = std::fs::canonicalize(cwd.path()).unwrap();
    assert!(rendered.contains("args={\"probe\":true}"), "{rendered}");
    assert!(
        rendered.contains(&format!("home={}", canonical.display())),
        "{rendered}"
    );
    if let Ok(host_home) = std::env::var("HOME") {
        assert!(
            !rendered.contains(&format!("home={host_home}")),
            "the host HOME leaked into the sandbox: {rendered}"
        );
    }
    assert!(rendered.contains("passwd=hidden"), "{rendered}");
    assert!(rendered.contains("inside-ok"), "{rendered}");
    assert!(cwd.path().join("plugin-inside.txt").is_file());
    assert!(
        !Path::new(&format!("/etc/{token}")).exists(),
        "the plugin sandbox must not write through to the host /etc"
    );
}

#[tokio::test]
async fn workspace_local_program_runs_inside_explicit_mounts() {
    let cwd = tempfile::tempdir().unwrap();
    let Some(policy) = verified_runtime_policy(cwd.path()).await else {
        return;
    };
    let tool = cwd.path().join("local-tool.sh");
    std::fs::write(
        &tool,
        "#!/bin/sh\necho local-ok > local.txt && cat local.txt\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // A program inside the workspace must not be rejected by the read-only
    // mount coverage check, even though the workspace is not in that list.
    install_plugin_command(
        cwd.path(),
        "local",
        vec!["./local-tool.sh".to_owned(), "arg-one".to_owned()],
    );
    let host = PolicyHost::new(policy);
    let tools = plugin::load_plugin_tools(cwd.path(), host, &ToolFilter::all()).unwrap();
    let outcome = tools[0].execute(json!({})).await.unwrap();
    let rendered = tool_output_text(&serde_json::to_value(&outcome).unwrap());
    assert!(rendered.contains("local-ok"), "{rendered}");
    assert_eq!(
        std::fs::read_to_string(cwd.path().join("local.txt")).unwrap(),
        "local-ok\n"
    );
}

#[tokio::test]
async fn unavailable_backend_fails_closed_for_every_entry_point() {
    let workspace = tempfile::tempdir().unwrap();

    // The low-level backend refuses the missing helper outright.
    let backend = BubblewrapBackend::at("/nonexistent/mio-bwrap", HostPlatform::Linux);
    assert!(!backend.status().available);
    let request = ExecutionRequest::new("/bin/sh", workspace.path()).args(["-c", "echo no"]);
    assert!(matches!(
        backend.wrap(&request),
        Err(BackendError::Unavailable { .. })
    ));

    // An unsupported platform is refused just as explicitly.
    let backend = BubblewrapBackend::at("/usr/bin/bwrap", HostPlatform::Unsupported);
    assert!(!backend.status().available);
    assert!(matches!(
        backend.wrap(&request),
        Err(BackendError::Unavailable { .. })
    ));

    // The same policy reaches the engine entry points, which must not fall
    // back to running the command unsandboxed.
    let policy = ExecutionPolicy::Bubblewrap(
        BubblewrapPolicy::default().with_helper_program("/nonexistent/mio-bwrap"),
    );
    let status = policy.probe(workspace.path()).await;
    assert!(!status.available);
    assert!(matches!(status.probe, BackendProbe::Failed(_)));

    let host = PolicyHost::new(policy.clone());

    install_plugin_tool(workspace.path(), "blocked", "touch plugin-ran");
    let tools =
        plugin::load_plugin_tools(workspace.path(), host.clone(), &ToolFilter::all()).unwrap();
    let rendered = serde_json::to_string(&tools[0].execute(json!({})).await.unwrap()).unwrap();
    assert!(rendered.contains("bubblewrap"), "{rendered}");
    assert!(!workspace.path().join("plugin-ran").exists());

    assert!(
        shell::execute_with_policy("touch shell-ran", workspace.path(), &policy)
            .await
            .is_err()
    );
    assert!(!workspace.path().join("shell-ran").exists());

    let specs = mcp_specs("blocked", "touch mcp-ran", &[]);
    let error =
        mcp::build_dynamic_tools_from_specs(&specs, workspace.path(), host, &ToolFilter::all())
            .await
            .unwrap_err();
    assert!(error.contains("bubblewrap"), "{error}");
    assert!(!workspace.path().join("mcp-ran").exists());
}

#[test]
fn policy_fingerprint_covers_fields_without_leaking_values() {
    let base = runtime_policy();
    let with_secret = ExecutionPolicy::Bubblewrap(BubblewrapPolicy::default().with_environment(
        EnvironmentPolicy::Explicit(vec![("MIO_SECRET".to_owned(), "one".to_owned())]),
    ));
    let changed_secret = ExecutionPolicy::Bubblewrap(BubblewrapPolicy::default().with_environment(
        EnvironmentPolicy::Explicit(vec![("MIO_SECRET".to_owned(), "two".to_owned())]),
    ));

    assert_ne!(base.fingerprint(), with_secret.fingerprint());
    assert_ne!(with_secret.fingerprint(), changed_secret.fingerprint());
    assert_ne!(
        base.fingerprint(),
        ExecutionPolicy::Unrestricted.fingerprint()
    );
    assert!(
        !changed_secret.fingerprint().contains("two"),
        "the policy fingerprint must not expose environment values"
    );
    assert_eq!(base.fingerprint(), runtime_policy().fingerprint());
}

#[test]
fn unrestricted_helper_preserves_argv_and_explicit_environment() {
    let workspace = tempfile::tempdir().unwrap();
    let request = SupervisedCommand::new("/bin/sh", workspace.path())
        .args(["-c", "printf '%s' \"$MIO_PROBE\""])
        .env("MIO_PROBE", "explicit");
    let command = ExecutionPolicy::Unrestricted.command(&request).unwrap();
    assert_eq!(command.get_program(), std::ffi::OsStr::new("/bin/sh"));
    let arguments: Vec<_> = command.get_args().collect();
    assert_eq!(
        arguments,
        [
            std::ffi::OsStr::new("-c"),
            std::ffi::OsStr::new("printf '%s' \"$MIO_PROBE\"")
        ]
    );
    assert_eq!(command.get_current_dir(), Some(workspace.path()));
    assert!(command.get_envs().any(
        |(name, value)| name == "MIO_PROBE" && value == Some(std::ffi::OsStr::new("explicit"))
    ));
}

#[test]
fn sandboxed_supervisor_refuses_environment_inheritance() {
    let workspace = tempfile::tempdir().unwrap();
    // A sandbox policy that asks to inherit the whole host environment cannot
    // be honored; it fails instead of leaking the environment or falling back
    // to an unsandboxed command.
    let policy = ExecutionPolicy::Bubblewrap(
        BubblewrapPolicy::default().with_environment(EnvironmentPolicy::Inherit),
    );
    let request = SupervisedCommand::new("/bin/true", workspace.path());
    assert!(matches!(
        policy.command(&request),
        Err(BackendError::InvalidRequest(_))
    ));

    // An explicit Inherit on the command itself is refused as well.
    let policy = runtime_policy();
    let request = SupervisedCommand::new("/bin/true", workspace.path()).inherit_environment();
    assert!(matches!(
        policy.command(&request),
        Err(BackendError::InvalidRequest(_))
    ));
}

/// Serializes the process-wide MCP connection cache across this test binary.
static MCP_GUARD: AsyncMutex<()> = AsyncMutex::const_new(());

#[tokio::test]
async fn mcp_servers_follow_the_host_policy_and_cache_key() {
    let _guard = MCP_GUARD.lock().await;
    shutdown_mcp_connections();
    assert_eq!(live_mcp_connection_count(), 0);

    let cwd = tempfile::tempdir().unwrap();
    let Some(policy) = verified_runtime_policy(cwd.path()).await else {
        return;
    };
    let sandbox_host = PolicyHost::new(policy.clone());

    // Isolation: a sandboxed server cannot see the host /etc and its host-only
    // environment is replaced by the sandbox defaults plus spec.env.
    let token = unique_token();
    let isolation = mcp_specs(
        "isolated",
        &mcp_isolation_script(&token),
        &[("MIO_MCP_PROBE", "explicit-ok")],
    );
    let tools = mcp::build_dynamic_tools_from_specs(
        &isolation,
        cwd.path(),
        sandbox_host.clone(),
        &ToolFilter::all(),
    )
    .await
    .unwrap();
    assert_eq!(tools.len(), 1);
    let report = std::fs::read_to_string(cwd.path().join("mcp-report.txt"))
        .expect("the sandboxed MCP server must write its report into the workspace");
    let canonical = std::fs::canonicalize(cwd.path()).unwrap();
    assert!(report.contains("probe=explicit-ok"), "{report}");
    assert!(
        report.contains(&format!("home={}", canonical.display())),
        "{report}"
    );
    if let Ok(host_home) = std::env::var("HOME") {
        assert!(
            !report.contains(&format!("home={host_home}")),
            "the host HOME leaked into the sandbox: {report}"
        );
    }
    assert!(report.contains("passwd=hidden"), "{report}");
    assert!(
        !Path::new(&format!("/etc/{token}")).exists(),
        "the MCP sandbox must not write through to the host /etc"
    );
    assert_eq!(live_mcp_connection_count(), 1);

    // Same policy: the cached connection is reused, so no new spawn.
    let spawn_specs = mcp_specs("probe", &mcp_spawn_script(), &[]);
    mcp::build_dynamic_tools_from_specs(
        &spawn_specs,
        cwd.path(),
        sandbox_host.clone(),
        &ToolFilter::all(),
    )
    .await
    .unwrap();
    assert_eq!(spawn_count(cwd.path()), 1);
    mcp::build_dynamic_tools_from_specs(
        &spawn_specs,
        cwd.path(),
        sandbox_host.clone(),
        &ToolFilter::all(),
    )
    .await
    .unwrap();
    assert_eq!(
        spawn_count(cwd.path()),
        1,
        "a cached connection must not respawn"
    );

    // A different policy must not reuse the live child.
    let unrestricted_host = PolicyHost::new(ExecutionPolicy::Unrestricted);
    mcp::build_dynamic_tools_from_specs(
        &spawn_specs,
        cwd.path(),
        unrestricted_host.clone(),
        &ToolFilter::all(),
    )
    .await
    .unwrap();
    assert_eq!(
        spawn_count(cwd.path()),
        2,
        "a policy change must start a fresh MCP child"
    );
    assert_eq!(live_mcp_connection_count(), 2);
    mcp::build_dynamic_tools_from_specs(
        &spawn_specs,
        cwd.path(),
        unrestricted_host,
        &ToolFilter::all(),
    )
    .await
    .unwrap();
    assert_eq!(spawn_count(cwd.path()), 2);

    shutdown_mcp_connections();
    assert_eq!(live_mcp_connection_count(), 0);
}

struct RecordingExecutor {
    seen: Mutex<Vec<String>>,
}

impl Executor for RecordingExecutor {
    fn execute(&self, task: Task) -> HostFuture<Result<Vec<Message>, String>> {
        self.seen
            .lock()
            .unwrap()
            .push(task.host.execution_policy().name().to_owned());
        Box::pin(async { Ok(Vec::new()) })
    }
}

#[tokio::test]
async fn subagent_inherits_the_parent_execution_policy() {
    let cwd = tempfile::tempdir().unwrap();
    let recorded = Arc::new(RecordingExecutor {
        seen: Mutex::new(Vec::new()),
    });
    let config = Config {
        model: "routing-test-model".to_owned(),
        ..Config::default()
    };
    let manager = Arc::new(
        SubagentManager::with_executor(
            cwd.path(),
            config,
            RunOptions::default(),
            1,
            Duration::from_secs(30),
            recorded.clone(),
        )
        .unwrap(),
    );

    // A policy that cannot start a process is enough: this test only observes
    // what the child host reports.
    let policy = ExecutionPolicy::Bubblewrap(
        BubblewrapPolicy::default().with_helper_program("/nonexistent/mio-bwrap"),
    );
    let host = PolicyHost::new(policy);
    let snapshot = manager
        .delegate(
            Delegate {
                session: "child".to_owned(),
                goal: "check policy".to_owned(),
                prompt: "do nothing".to_owned(),
                path: None,
                read_only: true,
            },
            host,
        )
        .unwrap();
    let run_id = snapshot.run_id;

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = manager
            .status(run_id, Duration::from_millis(200))
            .await
            .unwrap();
        if !snapshot.status.is_active() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "child never settled: {snapshot:?}"
        );
    }
    assert_eq!(recorded.seen.lock().unwrap().as_slice(), ["bubblewrap"]);
}

#[test]
fn runtime_mounts_keep_loader_aliases_and_targets() {
    let mounts = linux_runtime_mounts();
    for mount in &mounts {
        assert!(mount.is_absolute(), "{} is not absolute", mount.display());
        assert_ne!(mount, &PathBuf::from("/"));
    }
    // On a merged-/usr host /bin is a symlink to usr/bin; both spellings must
    // be present so the dynamic loader path is preserved.
    if Path::new("/bin").is_symlink() {
        assert!(mounts.contains(&PathBuf::from("/bin")));
        assert!(mounts.contains(&PathBuf::from("/usr/bin")));
    }
    if Path::new("/lib64").is_symlink() {
        assert!(mounts.contains(&PathBuf::from("/lib64")));
        assert!(mounts.contains(&PathBuf::from("/usr/lib64")));
    }
    assert!(
        mounts.iter().any(|mount| mount == Path::new("/usr")),
        "the merged /usr tree must be mounted"
    );
}
