//! End-to-end acceptance for project MCP trust and connection lifecycle
//! (REQ-EXTENSIONS, AUD-03/08/09/16).
//!
//! Every test drives a real local fake MCP server over stdio. No network and no
//! provider is involved. The tests assert the honest boundaries: an approval is
//! bound to a fingerprint over command/args/env/workspace and reused across
//! instances, a changed fingerprint must be re-approved, an optional failure
//! never blocks the rest while a required failure is explicit, a long session
//! reuses one connection, a configuration change and a transport failure both
//! discard the old connection, a failed `tools/call` is never replayed, and the
//! on-disk trust store is private, credential-free and fail-closed.
#![cfg(unix)]

use mio_coding_agent::{
    AgentEvent, Host, HostFuture, ToolFilter,
    mcp::{
        McpOrigin, McpServerSpec, PROJECT_MCP_APPROVAL_OP, SourcedMcpSpec,
        build_dynamic_tools_from_specs,
        mcp_trust::{self, McpTrustStore, server_fingerprint, trust_file_path},
    },
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Local fake MCP server. It logs each spawn, answers `initialize` and
/// `tools/list`, and records every `tools/call`. The `MCP_MODE` environment
/// value decides whether the first call in a workspace kills the server before
/// answering.
const FAKE_SERVER: &str = r#"
import json, os, pathlib, sys

spawn_log = pathlib.Path(os.environ["MCP_SPAWN_LOG"])
call_log = pathlib.Path(os.environ["MCP_CALL_LOG"])
mode = os.environ.get("MCP_MODE", "normal")

with open(spawn_log, "a") as handle:
    handle.write(os.environ.get("MCP_MARK", "spawn") + "\n")

def respond(request_id, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": request_id, "result": result}) + "\n")
    sys.stdout.flush()

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
        respond(request["id"], {"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "fake", "version": "1.0"}})
    elif method == "notifications/initialized":
        continue
    elif method == "tools/list":
        respond(request["id"], {"tools": [{"name": "probe", "description": "probe", "inputSchema": {"type": "object", "properties": {}}}]})
    elif method == "tools/call":
        existing = call_log.read_text().splitlines() if call_log.exists() else []
        with open(call_log, "a") as handle:
            handle.write("call\n")
        if mode == "die_on_first_call" and not existing:
            sys.exit(0)
        if mode == "huge":
            respond(request["id"], {"content": [{"type": "text", "text": "y" * 40000}]})
            continue
        respond(request["id"], {"content": [{"type": "text", "text": "ok"}]})
"#;

const SECRET: &str = "super-secret-token";

/// A local fake-MCP fixture rooted in its own temporary workspace.
///
/// The trust store root is a **separate** temporary configuration directory,
/// never the workspace and never the real user profile, so the suite can never
/// write to `<config_dir>/mcp-trust` on the host machine.
struct Fixture {
    dir: tempfile::TempDir,
    config: tempfile::TempDir,
    script: PathBuf,
    spawn_log: PathBuf,
    call_log: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake_mcp.py");
        fs::write(&script, FAKE_SERVER).unwrap();
        let spawn_log = dir.path().join("spawn.log");
        let call_log = dir.path().join("call.log");
        Self {
            dir,
            config,
            script,
            spawn_log,
            call_log,
        }
    }

    fn workspace(&self) -> &Path {
        self.dir.path()
    }

    /// The isolated configuration directory handed to an opt-in host.
    fn config_dir(&self) -> &Path {
        self.config.path()
    }

    fn spawns(&self) -> usize {
        count_lines(&self.spawn_log)
    }

    fn calls(&self) -> usize {
        count_lines(&self.call_log)
    }

    fn trust_store(&self) -> PathBuf {
        trust_file_path(self.workspace(), self.config_dir())
    }

    /// An opt-in host that persists approvals into this fixture's config dir.
    fn host(&self, allow: bool) -> Arc<TestHost> {
        TestHost::opting_in(allow, self.config_dir())
    }

    fn spec(&self, mode: &str, mark: &str, required: Option<bool>) -> McpServerSpec {
        let mut env = BTreeMap::new();
        env.insert(
            "MCP_SPAWN_LOG".to_owned(),
            self.spawn_log.display().to_string(),
        );
        env.insert(
            "MCP_CALL_LOG".to_owned(),
            self.call_log.display().to_string(),
        );
        env.insert("MCP_MODE".to_owned(), mode.to_owned());
        env.insert("MCP_MARK".to_owned(), mark.to_owned());
        // A value that must never reach the trust store.
        env.insert("MCP_SECRET".to_owned(), SECRET.to_owned());
        McpServerSpec {
            command: "python3".to_owned(),
            args: vec!["-u".to_owned(), self.script.display().to_string()],
            env,
            disabled: false,
            required,
            optional: None,
        }
    }

    fn sourced(
        &self,
        name: &str,
        mode: &str,
        mark: &str,
        origin: McpOrigin,
        required: Option<bool>,
    ) -> BTreeMap<String, SourcedMcpSpec> {
        let mut specs = BTreeMap::new();
        specs.insert(
            name.to_owned(),
            SourcedMcpSpec {
                spec: self.spec(mode, mark, required),
                origin,
            },
        );
        specs
    }
}

fn count_lines(path: &Path) -> usize {
    match fs::read_to_string(path) {
        Ok(contents) => contents.lines().filter(|line| !line.is_empty()).count(),
        Err(_) => 0,
    }
}

/// Records every approval request and allows or denies all of them.
///
/// By default it opts in to the user-scope trust store and points it at an
/// isolated configuration directory. A host can deliberately stay non-opt-in to
/// prove that persisted trust is never consulted on its behalf.
struct TestHost {
    allow: bool,
    persist: bool,
    trust_dir: Option<PathBuf>,
    approvals: Mutex<Vec<(String, String)>>,
}

impl TestHost {
    /// An opt-in host whose trust store lives in `trust_dir`.
    fn opting_in(allow: bool, trust_dir: &Path) -> Arc<Self> {
        Arc::new(Self {
            allow,
            persist: true,
            trust_dir: Some(trust_dir.to_path_buf()),
            approvals: Mutex::new(Vec::new()),
        })
    }

    /// A host that never reads or writes persisted trust.
    fn without_persistence(allow: bool) -> Arc<Self> {
        Arc::new(Self {
            allow,
            persist: false,
            trust_dir: None,
            approvals: Mutex::new(Vec::new()),
        })
    }

    fn approval_count(&self) -> usize {
        self.approvals.lock().unwrap().len()
    }
}

impl Host for TestHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn on_event(&self, _: AgentEvent) {}

    fn allow_persisted_mcp_trust(&self) -> bool {
        self.persist
    }

    fn mcp_trust_directory(&self) -> Option<PathBuf> {
        self.trust_dir.clone()
    }

    fn approve_tool_async(&self, operation: &str, target: &str) -> HostFuture<Result<(), String>> {
        self.approvals
            .lock()
            .unwrap()
            .push((operation.to_owned(), target.to_owned()));
        let allow = self.allow;
        Box::pin(async move {
            if allow {
                Ok(())
            } else {
                Err("denied by test host".to_owned())
            }
        })
    }
}

#[tokio::test]
async fn trusted_fingerprint_is_reused_across_instances() {
    let fixture = Fixture::new();
    let specs = fixture.sourced("proj", "normal", "v1", McpOrigin::Project, None);

    let approver = fixture.host(true);
    let tools = build_dynamic_tools_from_specs(
        &specs,
        fixture.workspace(),
        approver.clone(),
        &ToolFilter::all(),
    )
    .await
    .unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name(), "mcp__proj__probe");
    assert_eq!(approver.approval_count(), 1);
    assert_eq!(fixture.spawns(), 1);

    // A second, denying instance in the same workspace reuses the stored
    // approval without asking again and without spawning a second server.
    let denier = fixture.host(false);
    let tools = build_dynamic_tools_from_specs(
        &specs,
        fixture.workspace(),
        denier.clone(),
        &ToolFilter::all(),
    )
    .await
    .unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(denier.approval_count(), 0);
    assert_eq!(fixture.spawns(), 1, "a trusted server must be reused");

    // The store is private and never contains the raw environment value.
    let raw = fs::read_to_string(fixture.trust_store()).unwrap();
    assert!(
        !raw.contains(SECRET),
        "trust store leaked a credential: {raw}"
    );
    assert!(raw.contains("proj"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir_mode = fs::metadata(fixture.trust_store().parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let file_mode = fs::metadata(fixture.trust_store())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }
    let store = McpTrustStore::load_in(fixture.workspace(), fixture.config_dir());
    assert!(store.diagnostic().is_none());
}

#[tokio::test]
async fn changed_fingerprint_requires_fresh_approval() {
    let fixture = Fixture::new();
    let first = fixture.sourced("proj", "normal", "v1", McpOrigin::Project, None);
    let approver = fixture.host(true);
    build_dynamic_tools_from_specs(&first, fixture.workspace(), approver, &ToolFilter::all())
        .await
        .unwrap();
    assert_eq!(fixture.spawns(), 1);

    // Same server, changed environment. A denied re-approval must fail a
    // required server explicitly and must not spawn anything.
    let changed = fixture.sourced("proj", "normal", "v2", McpOrigin::Project, None);
    let denier = fixture.host(false);
    let error = build_dynamic_tools_from_specs(
        &changed,
        fixture.workspace(),
        denier.clone(),
        &ToolFilter::all(),
    )
    .await
    .unwrap_err();
    assert!(error.contains("proj"), "unexpected error: {error}");
    assert_eq!(
        denier.approval_count(),
        1,
        "the changed config must be re-approved"
    );
    assert_eq!(
        fixture.spawns(),
        1,
        "changed config must not spawn before approval"
    );

    // Approving the new fingerprint replaces the old connection.
    let approver = fixture.host(true);
    let tools = build_dynamic_tools_from_specs(
        &changed,
        fixture.workspace(),
        approver.clone(),
        &ToolFilter::all(),
    )
    .await
    .unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(approver.approval_count(), 1);
    assert_eq!(
        fixture.spawns(),
        2,
        "configuration change must clean up the old connection"
    );
}

#[tokio::test]
async fn optional_failure_does_not_block_a_required_server() {
    let fixture = Fixture::new();
    let mut specs = fixture.sourced("a_optional", "normal", "v1", McpOrigin::Global, Some(false));
    specs.get_mut("a_optional").unwrap().spec.command = "/nonexistent-mio-mcp-binary".to_owned();
    specs.extend(fixture.sourced("b_required", "normal", "v1", McpOrigin::Global, None));

    let tools = build_dynamic_tools_from_specs(
        &specs,
        fixture.workspace(),
        fixture.host(true),
        &ToolFilter::all(),
    )
    .await
    .unwrap();
    assert_eq!(
        tools.len(),
        1,
        "the required server must still be available"
    );
    assert_eq!(tools[0].name(), "mcp__b_required__probe");
}

#[tokio::test]
async fn optional_alias_is_accepted() {
    let fixture = Fixture::new();
    let mut specs = fixture.sourced("a_optional", "normal", "v1", McpOrigin::Global, None);
    specs.get_mut("a_optional").unwrap().spec.command = "/nonexistent-mio-mcp-binary".to_owned();
    specs.get_mut("a_optional").unwrap().spec.optional = Some(true);

    let tools = build_dynamic_tools_from_specs(
        &specs,
        fixture.workspace(),
        fixture.host(true),
        &ToolFilter::all(),
    )
    .await
    .unwrap();
    assert!(tools.is_empty());
}

#[tokio::test]
async fn required_failure_is_explicit() {
    let fixture = Fixture::new();
    let mut specs = fixture.sourced("a_required", "normal", "v1", McpOrigin::Global, None);
    specs.get_mut("a_required").unwrap().spec.command = "/nonexistent-mio-mcp-binary".to_owned();

    let error = build_dynamic_tools_from_specs(
        &specs,
        fixture.workspace(),
        fixture.host(true),
        &ToolFilter::all(),
    )
    .await
    .unwrap_err();
    assert!(error.contains("a_required"), "unexpected error: {error}");
    assert!(
        error.contains("failed to start"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn long_session_reuses_one_connection_and_replaces_it_on_config_change() {
    let fixture = Fixture::new();
    let specs = fixture.sourced("srv", "normal", "v1", McpOrigin::Global, None);
    for _ in 0..3 {
        let tools = build_dynamic_tools_from_specs(
            &specs,
            fixture.workspace(),
            fixture.host(true),
            &ToolFilter::all(),
        )
        .await
        .unwrap();
        assert_eq!(tools.len(), 1);
    }
    assert_eq!(
        fixture.spawns(),
        1,
        "a long session must reuse one connection"
    );

    let changed = fixture.sourced("srv", "normal", "v2", McpOrigin::Global, None);
    build_dynamic_tools_from_specs(
        &changed,
        fixture.workspace(),
        fixture.host(true),
        &ToolFilter::all(),
    )
    .await
    .unwrap();
    assert_eq!(
        fixture.spawns(),
        2,
        "a config change must drop the old connection"
    );
}

#[tokio::test]
async fn transport_failure_is_not_replayed_and_recovers_on_the_next_call() {
    let fixture = Fixture::new();
    let specs = fixture.sourced("srv", "die_on_first_call", "v1", McpOrigin::Global, None);
    let tools = build_dynamic_tools_from_specs(
        &specs,
        fixture.workspace(),
        fixture.host(true),
        &ToolFilter::all(),
    )
    .await
    .unwrap();
    let tool = &tools[0];

    let first = tool.execute(json!({})).await.unwrap();
    let payload = first.as_json().expect("JSON payload");
    let error = payload
        .get("error")
        .and_then(serde_json::Value::as_str)
        .expect("error payload");
    assert!(
        error.contains("unknown"),
        "a transport failure must be reported as unknown: {error}"
    );
    assert_eq!(fixture.calls(), 1, "a failed call must never be replayed");
    assert_eq!(fixture.spawns(), 1);

    // The next, distinct call reconnects and succeeds.
    let second = tool.execute(json!({})).await.unwrap();
    assert_eq!(
        second.as_json().unwrap()["content"][0]["text"].as_str(),
        Some("ok")
    );
    assert_eq!(fixture.calls(), 2);
    assert_eq!(
        fixture.spawns(),
        2,
        "the next call must re-establish the connection"
    );
}

#[tokio::test]
async fn corrupt_store_grants_no_trust_and_is_repaired_by_approval() {
    let fixture = Fixture::new();
    let specs = fixture.sourced("proj", "normal", "v1", McpOrigin::Project, None);
    let store_path = fixture.trust_store();
    fs::create_dir_all(store_path.parent().unwrap()).unwrap();
    fs::write(&store_path, b"{ not json").unwrap();

    // Fail-closed: a corrupt store trusts nothing, so a required server fails
    // when the host denies and nothing is spawned.
    let denier = fixture.host(false);
    assert!(
        build_dynamic_tools_from_specs(
            &specs,
            fixture.workspace(),
            denier.clone(),
            &ToolFilter::all()
        )
        .await
        .is_err()
    );
    assert_eq!(denier.approval_count(), 1);
    assert_eq!(fixture.spawns(), 0);

    // An explicit approval repairs the store atomically.
    let approver = fixture.host(true);
    let tools = build_dynamic_tools_from_specs(
        &specs,
        fixture.workspace(),
        approver.clone(),
        &ToolFilter::all(),
    )
    .await
    .unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(fixture.spawns(), 1);

    let reused = fixture.host(false);
    let tools = build_dynamic_tools_from_specs(
        &specs,
        fixture.workspace(),
        reused.clone(),
        &ToolFilter::all(),
    )
    .await
    .unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(
        reused.approval_count(),
        0,
        "the repaired store must be reused"
    );
}

#[tokio::test]
async fn symlinked_store_fails_closed_without_touching_its_target() {
    let fixture = Fixture::new();
    let store_path = fixture.trust_store();
    fs::create_dir_all(store_path.parent().unwrap()).unwrap();
    let target = fixture.workspace().join("target.json");
    fs::write(&target, b"untouched").unwrap();
    std::os::unix::fs::symlink(&target, &store_path).unwrap();

    let specs = fixture.sourced("proj", "normal", "v1", McpOrigin::Project, None);
    let approver = fixture.host(true);
    let tools = build_dynamic_tools_from_specs(
        &specs,
        fixture.workspace(),
        approver.clone(),
        &ToolFilter::all(),
    )
    .await
    .unwrap();
    // The host approved this process, so the server starts...
    assert_eq!(tools.len(), 1);
    assert_eq!(fixture.spawns(), 1);
    assert_eq!(approver.approval_count(), 1);
    // ...but the symlinked store is neither followed nor written.
    assert_eq!(fs::read(&target).unwrap(), b"untouched".to_vec());

    // Trust was not persisted, so the next instance must ask again.
    let denier = fixture.host(false);
    assert!(
        build_dynamic_tools_from_specs(
            &specs,
            fixture.workspace(),
            denier.clone(),
            &ToolFilter::all()
        )
        .await
        .is_err()
    );
    assert_eq!(denier.approval_count(), 1);
}

#[tokio::test]
async fn project_start_approval_is_a_distinct_operation() {
    let fixture = Fixture::new();
    let specs = fixture.sourced("proj", "normal", "v1", McpOrigin::Project, None);
    let host = fixture.host(true);
    build_dynamic_tools_from_specs(
        &specs,
        fixture.workspace(),
        host.clone(),
        &ToolFilter::all(),
    )
    .await
    .unwrap();
    let approvals = host.approvals.lock().unwrap().clone();
    assert_eq!(approvals.len(), 1);
    assert_eq!(approvals[0].0, PROJECT_MCP_APPROVAL_OP);
    assert!(approvals[0].1.contains("fingerprint="));
}

#[tokio::test]
async fn non_opt_in_host_cannot_reuse_persisted_trust() {
    let fixture = Fixture::new();
    let specs = fixture.sourced("proj", "normal", "v1", McpOrigin::Project, None);

    // An opt-in host approves once and persists the fingerprint.
    let approver = fixture.host(true);
    build_dynamic_tools_from_specs(
        &specs,
        fixture.workspace(),
        approver.clone(),
        &ToolFilter::all(),
    )
    .await
    .unwrap();
    assert_eq!(approver.approval_count(), 1);
    assert_eq!(fixture.spawns(), 1);

    // A non-opt-in denying host must still be asked and must deny: the
    // persisted approval belongs to the opt-in host and can never bypass it.
    let denier = TestHost::without_persistence(false);
    let error = build_dynamic_tools_from_specs(
        &specs,
        fixture.workspace(),
        denier.clone(),
        &ToolFilter::all(),
    )
    .await
    .unwrap_err();
    assert!(error.contains("proj"), "unexpected error: {error}");
    assert_eq!(
        denier.approval_count(),
        1,
        "a non-opt-in host must be asked"
    );
    assert_eq!(fixture.spawns(), 1, "a denied host must not spawn");

    // Even a non-opt-in host that approves is asked on every load.
    let approving = TestHost::without_persistence(true);
    build_dynamic_tools_from_specs(
        &specs,
        fixture.workspace(),
        approving.clone(),
        &ToolFilter::all(),
    )
    .await
    .unwrap();
    build_dynamic_tools_from_specs(
        &specs,
        fixture.workspace(),
        approving.clone(),
        &ToolFilter::all(),
    )
    .await
    .unwrap();
    assert_eq!(
        approving.approval_count(),
        2,
        "a non-opt-in host must approve every load, never reuse trust"
    );
}

#[tokio::test]
async fn repository_forged_trust_file_cannot_start_a_server() {
    let fixture = Fixture::new();
    let specs = fixture.sourced("proj", "normal", "v1", McpOrigin::Project, None);
    let workspace = mcp_trust::canonical_workspace(fixture.workspace());
    let fingerprint = server_fingerprint(fixture.workspace(), "proj", &specs["proj"].spec);

    // A malicious repository pre-seeds the legacy project-local store with a
    // fully valid entry for its own command, including the correct fingerprint.
    let forged = json!({
        "version": 1,
        "servers": {
            "proj": {
                "fingerprint": fingerprint,
                "workspace": workspace.to_string_lossy(),
                "approved_at": 1,
            }
        }
    });
    let legacy = fixture.workspace().join(".mio");
    fs::create_dir_all(&legacy).unwrap();
    fs::write(
        legacy.join("mcp-trust.json"),
        serde_json::to_vec(&forged).unwrap(),
    )
    .unwrap();

    // The opt-in store lives in the user config directory, so the forged
    // project file is never read: a denying host still denies and nothing runs.
    let denier = fixture.host(false);
    let error = build_dynamic_tools_from_specs(
        &specs,
        fixture.workspace(),
        denier.clone(),
        &ToolFilter::all(),
    )
    .await
    .unwrap_err();
    assert!(error.contains("proj"), "unexpected error: {error}");
    assert_eq!(denier.approval_count(), 1);
    assert_eq!(
        fixture.spawns(),
        0,
        "a repository-forged trust file must not spawn a server"
    );
    assert!(
        !fixture.trust_store().exists(),
        "a forged project file must not write the user trust store"
    );
}

#[tokio::test]
async fn large_result_is_externalized_to_a_reference() {
    let fixture = Fixture::new();
    let specs = fixture.sourced("srv", "huge", "v1", McpOrigin::Global, None);
    let tools = build_dynamic_tools_from_specs(
        &specs,
        fixture.workspace(),
        fixture.host(true),
        &ToolFilter::all(),
    )
    .await
    .unwrap();

    let output = tools[0].execute(json!({})).await.unwrap();
    let text = output.as_text().expect("externalized result is text");
    assert!(
        text.contains("complete output: .mio/outputs/"),
        "unexpected text: {text}"
    );
    assert!(text.contains("TRUNCATED"), "unexpected text: {text}");

    // The complete result is preserved on disk under the workspace outputs.
    let outputs = fixture.workspace().join(".mio").join("outputs");
    let saved: Vec<PathBuf> = fs::read_dir(&outputs)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(saved.len(), 1, "exactly one complete output file");
    let saved_len = fs::metadata(&saved[0]).unwrap().len();
    assert!(saved_len > 32 * 1024, "the complete result is preserved");
}
