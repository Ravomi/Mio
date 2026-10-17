//! Runner 状态机、CLI Host 实现与会话读取。

use mio_coding_agent::{
    AgentEvent, BlockedKind, Config, Host, HostFuture, Message, ProgramState, Provider, RunOptions,
    RunTools, Services, ThinkingLevel, ToolFilter,
    checkpoint::CheckpointStore,
    context::expand_at_files,
    dedup::{TaskKeyLedger, TaskKeyState},
    mcp::{PROJECT_MCP_APPROVAL_OP, load_policy_bound_tools},
    parse_model_spec,
    rpc::{list_catalog_models, resolve_provider_endpoint},
    run_with_tools_message,
    session::Session,
    subagent::SubagentManager,
    task_lifecycle::{
        Settlement, TaskHandle, TaskId, TaskIdentity, TaskLifecycleSink, TaskLifecycleTracker,
        TaskPhase, TaskRecord, scope_task,
    },
    tool_lifecycle::ToolCallPhase,
    unknown_effect::{UnknownReason, UnknownSideEffect},
    unresolved_effect::{
        CoverageCommit, DispatchAdmission, EvidenceResolution, MAX_SCOPE_CHARS,
        OutstandingRecovery, UnknownEffectInput, UnknownFactReason, UnresolvedEffectError,
        UnresolvedEffectKey, UnresolvedEffectLedger, UnresolvedEffectObservation,
        UnresolvedEffectState, UnresolvedSnapshotKey,
    },
};
use mio_core::message::{Image, UserContent};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::{
    journal::{self, Journal, RunState},
    json_output::{
        CorrelationFields, Envelope, JsonOutput, emit_or_print, emit_or_print_correlated,
    },
};

use super::approval::{RpcApprovals, StatusReporter};

/// Distinct operation label for the unknown-effect confirmation.
///
/// It is deliberately not a tool approval: it never writes a journal intent,
/// and an RPC reply releases only its own one-shot request. The label keeps the
/// confirmation out of the ordinary tool-approval stream.
pub(super) const CONFIRM_UNKNOWN_SIDE_EFFECT_OP: &str = "confirm_unknown_side_effect";

/// Upper bound on the number of workspace journal files one bounded legacy
/// migration scan inspects.
const MAX_JOURNAL_SCAN_FILES: usize = 1024;
/// Upper bound on the total journal bytes one bounded legacy migration scan
/// reads. Exceeding either bound fails closed rather than silently skipping a
/// journal (which could hide an unresolved side effect).
const MAX_JOURNAL_SCAN_BYTES: u64 = 64 * 1024 * 1024;
/// Upper bound on tracked run scopes used to decide whether a call belongs to a
/// still-live run.
const MAX_TRACKED_RUNS: usize = 4096;
/// Upper bound on the `(run scope, provider tool-call id) -> call_index`
/// correlation map. It never grows past this and never evicts a still-live
/// run's mapping; at the bound with only live runs, a new side-effect dispatch
/// is refused instead.
const MAX_TRACKED_DISPATCHES: usize = 8192;

/// One bounded legacy unknown collected for durable migration.
struct LegacyUnknownCandidate {
    key: UnresolvedEffectKey,
    operation: String,
    target: Option<String>,
    reason: UnknownFactReason,
}

/// Derive a distinct, bounded origin scope for a legacy journal operation.
///
/// The value comes only from the real journal `session_id` and `run_id`; no
/// engine run UUID, provider tool-call id or evidence id is invented. If the
/// real identity does not fit the ledger scope bound, the same real bytes are
/// folded into a domain-separated digest instead of truncating them (which
/// could collide).
fn legacy_origin_scope(session_id: &str, run_id: &str) -> String {
    let raw = format!("journal-legacy:{session_id}:{run_id}");
    if !raw.is_empty()
        && raw.chars().count() <= MAX_SCOPE_CHARS
        && !session_id.contains(':')
        && !run_id.contains(':')
        && !raw.chars().any(char::is_control)
    {
        return raw;
    }
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"mio-journal-legacy-origin-v1\0");
    // Length prefixes make the original identity tuple unambiguous even
    // when a legacy identifier itself contains ':' or a NUL separator.
    hasher.update((session_id.len() as u64).to_be_bytes());
    hasher.update(session_id.as_bytes());
    hasher.update((run_id.len() as u64).to_be_bytes());
    hasher.update(run_id.as_bytes());
    format!("journal-legacy:{:x}", hasher.finalize())
}

/// Push one deduplicated migration candidate.
fn push_legacy_candidate(
    candidates: &mut Vec<LegacyUnknownCandidate>,
    seen: &mut BTreeSet<(String, u64)>,
    key: UnresolvedEffectKey,
    operation: String,
    target: Option<String>,
    reason: UnknownFactReason,
) -> Result<(), ()> {
    if operation.trim().is_empty() {
        return Err(());
    }
    if seen.insert((key.scope.clone(), key.call_index)) {
        candidates.push(LegacyUnknownCandidate {
            key,
            operation,
            target,
            reason,
        });
    }
    Ok(())
}

/// Whether a canonical `(tool, target)` pair is a pure read.
///
/// Only the exact built-in read identities plus the precise subagent query
/// operations are reads. A subagent `spawn` or any other operation stays
/// write/external, and a qualified plugin/MCP label is never a read.
fn is_read_tool(tool: &str, target: &str) -> bool {
    match tool {
        "read" | "grep" | "find" | "ls" => true,
        // `classify` stores the subagent operation in the bounded display
        // target, not in `operation`.
        "subagent" => matches!(target, "status" | "snapshot" | "list"),
        _ => false,
    }
}

/// Whether a legacy canonical call (as raw journal JSON) is a pure read.
fn legacy_json_call_is_read(call: &serde_json::Value) -> bool {
    let tool = call.get("tool").and_then(|v| v.as_str()).unwrap_or("");
    let target = call.get("target").and_then(|v| v.as_str()).unwrap_or("");
    is_read_tool(tool, target)
}

/// Whether a legacy `operations` entry is a known pure read by its exact
/// built-in operation label.
fn legacy_operation_is_read(operation: &str) -> bool {
    matches!(operation, "read" | "grep" | "find" | "ls")
}

/// Bounded, read-only result of scanning the whole workspace's journals.
struct WorkspaceJournalScan {
    /// The real un-represented legacy unknowns worth migrating.
    candidates: Vec<LegacyUnknownCandidate>,
    /// Real dropped/truncated evidence was seen in some journal.
    loss: bool,
}

/// Bounded, human-readable identity of the unknown original calls, used only
/// for the host prompt. Identity matching always uses the typed marker and the
/// real dispatch correlation, never this text.
fn unknown_summary(unknown: &[UnknownSideEffect]) -> String {
    let mut parts = Vec::new();
    for effect in unknown.iter().take(4) {
        let mut identity = format!(
            "{} ({}, id={})",
            effect.operation,
            effect.reason.as_str(),
            effect.tool_call_id.as_deref().unwrap_or("unknown")
        );
        // A recovered unknown must be approved against the exact durable
        // ledger evidence the host can see: its real key and snapshot digest.
        if let Some((scope, index)) = effect.ledger_key() {
            identity.push_str(&format!(" ledger={scope}#{index}"));
            if let Some(snapshot) = effect.ledger_snapshot() {
                identity.push_str(&format!(" snapshot={snapshot}"));
            }
        }
        parts.push(identity);
    }
    if unknown.len() > 4 {
        parts.push(format!("and {} more", unknown.len() - 4));
    }
    let summary = parts.join("; ");
    if summary.chars().count() <= 1024 {
        return summary;
    }
    let mut end = 1024;
    while !summary.is_char_boundary(end) {
        end -= 1;
    }
    summary[..end].to_owned()
}

/// One unknown shown to the host, with the real durable-ledger key and the
/// exact snapshot digest that must be acknowledged.
#[derive(Clone, Debug)]
struct UnknownClaim {
    key: UnresolvedEffectKey,
    snapshot: UnresolvedSnapshotKey,
}

/// The durable-ledger claim behind a shown unknown, if it carries one.
///
/// A malformed or missing digest is treated as "no durable claim" rather than
/// a guessed identity: the host can still see the in-memory evidence, but only
/// a real, exactly-keyed record may be acknowledged.
fn unknown_claim(effect: &UnknownSideEffect) -> Option<UnknownClaim> {
    let (scope, call_index) = effect.ledger_key()?;
    let snapshot = effect.ledger_snapshot()?;
    Some(UnknownClaim {
        key: UnresolvedEffectKey::new(scope, call_index).ok()?,
        snapshot: snapshot.parse::<UnresolvedSnapshotKey>().ok()?,
    })
}

/// Whether a canonical dispatch is a read-only call.
///
/// Only the canonical built-in read identities are treated as reads, plus the
/// precise subagent query operations the authoritative policy classifier also
/// marks as reads (`status` / `snapshot` / `list`). A subagent `spawn` or any
/// other operation stays write/external: this is not a blanket "all subagent
/// calls are reads". Plugin and MCP calls keep their qualified names and are
/// conservatively write/external, so a ledger failure can never silently let an
/// unbounded effect through.
fn canonical_call_is_read(call: &mio_coding_agent::tool_lifecycle::ToolCallRecord) -> bool {
    is_read_tool(&call.tool, &call.target)
}

/// The durable fact reason for a typed run-time unknown reason.
fn ledger_fact_reason(reason: UnknownReason) -> UnknownFactReason {
    match reason {
        UnknownReason::McpTransport => UnknownFactReason::TransportLost,
        UnknownReason::PluginSupervision => UnknownFactReason::Interrupted,
        UnknownReason::InterruptedDispatch => UnknownFactReason::InterruptedDispatch,
        UnknownReason::MarkerEncoding => UnknownFactReason::Unclassified,
        // Native supervision losses are a typed unknown of their own: the
        // result could not be confirmed, so record it honestly as an unknown
        // result rather than pretending it was an MCP transport death or a
        // plugin supervision timeout.
        UnknownReason::NativeTimeout
        | UnknownReason::NativeWait
        | UnknownReason::NativeCapture
        | UnknownReason::HostSupervision => UnknownFactReason::ResultUnknown,
    }
}

/// The honest fence reason for a recovered durable record.
///
/// A recovered dispatch with no typed fact is `InterruptedDispatch`, never a
/// pretended MCP transport death or plugin supervision timeout.
fn recovered_fence_reason(reason: Option<UnknownFactReason>) -> UnknownReason {
    match reason {
        Some(UnknownFactReason::TransportLost) => UnknownReason::McpTransport,
        Some(UnknownFactReason::Interrupted | UnknownFactReason::InterruptedDispatch) => {
            UnknownReason::InterruptedDispatch
        }
        Some(UnknownFactReason::ResultUnknown | UnknownFactReason::Unclassified) | None => {
            UnknownReason::InterruptedDispatch
        }
    }
}

/// Build the run guard effect for one recovered `Unknown` record.
///
/// It keeps the real ledger key and snapshot digest; it invents no evidence id
/// and no run scope the ledger did not record.
fn recovered_unknown(entry: &OutstandingRecovery) -> UnknownSideEffect {
    UnknownSideEffect::recovered(
        entry
            .last_operation
            .clone()
            .unwrap_or_else(|| "interrupted_dispatch".to_owned()),
        recovered_fence_reason(entry.last_reason),
        entry.key.scope.clone(),
        entry.key.call_index,
        entry.snapshot_key.as_str(),
    )
}

pub(super) fn execution_status(
    policy: &mio_coding_agent::execution_backend::ExecutionPolicy,
) -> serde_json::Value {
    use mio_coding_agent::execution_backend::HostReadVisibility;
    let status = policy.status();
    let guarantees = status.guarantees;
    json!({
        "backend": status.name,
        "available": status.available,
        "unavailableReason": status.unavailable_reason,
        "program": status.program,
        "availabilityScope": "helper and platform check; OS may still refuse execution",
        "scope": "engine-started child processes only: the shell tool, declarative plugin tools, stdio MCP servers and delegated subagents",
        "entryPoints": ["shell", "plugin", "mcp_stdio", "subagent"],
        "outsideSandbox": [
            "arbitrary extra_tools and unknown host tools are refused before registration when the policy is not unrestricted",
            "the host Provider/model network client runs in the host process, not in a sandboxed child",
            "host Rust callbacks and the workspace path policy are not OS isolation",
        ],
        "hostReads": match guarantees.host_read_visibility {
            HostReadVisibility::FullHostAccess => "full_host_access",
            HostReadVisibility::HostRootReadOnly => "host_root_read_only",
            HostReadVisibility::ExplicitMountsOnly => "explicit_mounts_only",
        },
        "workspaceOnlyWrites": guarantees.workspace_only_writes,
        "networkUnshared": guarantees.network_unshared,
        "privateTmp": guarantees.private_tmp,
        "privatePid": guarantees.private_pid,
        "processExitCleanup": guarantees.process_exit_cleanup,
        "resourceLimits": guarantees.resource_limits,
        "caveats": status.caveats,
    })
}

/// Describe persisted history separately from the runner's in-memory history.
/// Reading recovery state never saves a session or replays a request.
pub(super) fn recovery_view(
    session_id: &str,
    cwd: &Path,
    path: Option<&Path>,
    evidence_field: &str,
) -> serde_json::Value {
    let mut view = json!({
        "sessionId": session_id,
        "sessionFile": path,
        "sessionSaved": false,
        "savedMessages": null,
        "sessionError": null,
        "evidenceField": evidence_field,
        "automaticReplay": false,
        "lastCompactionLoss": null,
    });
    if let Some(path) = path {
        match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => view["sessionError"] = json!(error.to_string()),
            Ok(_) => match Session::load(path) {
                Ok(saved) if saved.id.to_string() == session_id && saved.cwd == cwd => {
                    view["sessionSaved"] = json!(true);
                    view["savedMessages"] = json!(saved.history.len());
                    view["lastCompactionLoss"] = json!(saved.last_compaction_loss);
                }
                Ok(_) => view["sessionError"] = json!("Session identity or workspace mismatch"),
                Err(error) => view["sessionError"] = json!(error),
            },
        }
    }
    view
}

/// Bounded, read-only durable-ledger summary.
///
/// A damaged ledger reports its error instead of pretending to be an empty
/// safe set, and a disabled ledger reports zero without touching disk.
pub(super) fn ledger_summary(ledger: &UnresolvedEffectLedger) -> serde_json::Value {
    let mode = ledger.mode();
    match ledger.snapshot() {
        Ok(snapshot) => json!({
            "mode": mode.as_str(),
            "enabled": mode.is_writable(),
            "present": snapshot.present,
            "entries": snapshot.len(),
            "unknownCount": snapshot.unknown_count(),
            "outstandingCount": snapshot.outstanding_count(),
            "resolvedTrimmed": snapshot.resolved_trimmed(),
            "coverageInitialized": snapshot.coverage_initialized(),
            "coverageIncomplete": snapshot.coverage_incomplete(),
            "error": serde_json::Value::Null,
        }),
        Err(UnresolvedEffectError::Disabled) => json!({
            "mode": mode.as_str(),
            "enabled": false,
            "present": false,
            "entries": 0,
            "unknownCount": 0,
            "outstandingCount": 0,
            "resolvedTrimmed": 0,
            "coverageInitialized": false,
            "coverageIncomplete": false,
            "error": serde_json::Value::Null,
        }),
        Err(error) => json!({
            "mode": mode.as_str(),
            "enabled": mode.is_writable(),
            "present": serde_json::Value::Null,
            "entries": serde_json::Value::Null,
            "unknownCount": serde_json::Value::Null,
            "outstandingCount": serde_json::Value::Null,
            "resolvedTrimmed": serde_json::Value::Null,
            "coverageInitialized": serde_json::Value::Null,
            "coverageIncomplete": serde_json::Value::Null,
            "error": error.to_string(),
        }),
    }
}

/// Bounded, review-oriented listing of the exact durable records.
///
/// This is the inspection half of the maintenance path for a workspace whose
/// pending `Unknown` count exceeds the run fence: it lists the exact
/// `(scope, call_index)` and snapshot digest each record must be acknowledged
/// with, and marks which records are `acknowledgeable`. It never suggests or
/// performs deletion of the ledger.
pub(super) fn unresolved_effects_view(ledger: &UnresolvedEffectLedger) -> serde_json::Value {
    match ledger.snapshot() {
        Ok(snapshot) => {
            let entries: Vec<serde_json::Value> = snapshot
                .entries
                .iter()
                .map(|entry| {
                    let acknowledgeable = entry.record.state == UnresolvedEffectState::Unknown
                        && entry.record.facts_dropped == 0;
                    json!({
                        "scope": entry.record.scope,
                        "callIndex": entry.record.call_index,
                        "state": entry.record.state.as_str(),
                        "revision": entry.record.revision,
                        "facts": entry.record.facts.len(),
                        "factsDropped": entry.record.facts_dropped,
                        "complete": entry.record.facts_dropped == 0,
                        "acknowledgeable": acknowledgeable,
                        "snapshot": entry.snapshot_key.as_str(),
                    })
                })
                .collect();
            json!({
                "mode": ledger.mode().as_str(),
                "present": snapshot.present,
                "resolvedTrimmed": snapshot.resolved_trimmed(),
                "coverageInitialized": snapshot.coverage_initialized(),
                "coverageIncomplete": snapshot.coverage_incomplete(),
                "entries": entries,
                "error": serde_json::Value::Null,
            })
        }
        Err(UnresolvedEffectError::Disabled) => json!({
            "mode": "disabled",
            "present": false,
            "resolvedTrimmed": 0,
            "coverageInitialized": false,
            "coverageIncomplete": false,
            "entries": [],
            "error": serde_json::Value::Null,
        }),
        Err(error) => json!({
            "mode": ledger.mode().as_str(),
            "present": serde_json::Value::Null,
            "resolvedTrimmed": serde_json::Value::Null,
            "coverageInitialized": serde_json::Value::Null,
            "coverageIncomplete": serde_json::Value::Null,
            "entries": serde_json::Value::Null,
            "error": error.to_string(),
        }),
    }
}

/// Exactly acknowledge one durable record through the explicit maintenance
/// path.
///
/// This is the bounded recovery path for a pending `Unknown` count that
/// exceeds the run fence (or a fence that overflowed): the operator passes the
/// exact `(scope, call_index)` and snapshot digest they inspected, and only
/// that record is acknowledged. It refuses an incomplete record
/// (`facts_dropped > 0`), a stale digest, and any key it cannot parse. It never
/// touches the journal and never deletes evidence.
pub(super) fn acknowledge_unresolved_exact(
    ledger: &UnresolvedEffectLedger,
    scope: &str,
    call_index: u64,
    snapshot: &str,
    note: Option<&str>,
) -> Result<UnresolvedEffectObservation, String> {
    let key = UnresolvedEffectKey::new(scope, call_index).map_err(|error| error.to_string())?;
    let snapshot = snapshot
        .parse::<UnresolvedSnapshotKey>()
        .map_err(|error| error.to_string())?;
    match ledger.lookup(&key) {
        Ok(Some(observation)) => {
            if observation.record.state == UnresolvedEffectState::Acknowledged {
                return Ok(observation);
            }
            if observation.record.state != UnresolvedEffectState::Unknown {
                return Err(format!(
                    "refusing to acknowledge {}: it is {} in the unresolved effect ledger",
                    key,
                    observation.record.state.as_str()
                ));
            }
            if observation.record.facts_dropped > 0 {
                return Err(format!(
                    "refusing to acknowledge {}: its evidence is incomplete ({} facts dropped) and can never be confirmed from an incomplete snapshot",
                    key, observation.record.facts_dropped
                ));
            }
        }
        Ok(None) => {
            return Err(format!(
                "refusing to acknowledge {}: no durable unresolved effect record exists",
                key
            ));
        }
        Err(UnresolvedEffectError::Disabled) => {
            return Err("the unresolved effect ledger is disabled".into());
        }
        Err(error) => return Err(error.to_string()),
    }
    ledger
        .acknowledge(&key, &snapshot, note)
        .map_err(|error| error.to_string())
}

/// Persist an acknowledgement for exactly the shown snapshot digest.
///
/// New evidence appended after the host was shown a digest makes that digest
/// stale: the acknowledgement is refused instead of silently covering evidence
/// the host never saw. Nothing here re-snapshots and acks "everything".
fn persist_unknown_claims(
    ledger: &UnresolvedEffectLedger,
    claims: &[UnknownClaim],
    note: &str,
) -> Result<(), String> {
    for claim in claims {
        match ledger.lookup(&claim.key) {
            Ok(Some(observation))
                if matches!(
                    observation.record.state,
                    UnresolvedEffectState::Acknowledged | UnresolvedEffectState::Resolved
                ) =>
            {
                continue;
            }
            Ok(Some(observation)) if observation.record.state == UnresolvedEffectState::Unknown => {
            }
            Ok(Some(observation)) => {
                return Err(format!(
                    "refusing to acknowledge {}: it is {} in the unresolved effect ledger",
                    claim.key,
                    observation.record.state.as_str()
                ));
            }
            Ok(None) => {
                return Err(format!(
                    "refusing to acknowledge {}: no durable unresolved effect record exists",
                    claim.key
                ));
            }
            Err(UnresolvedEffectError::Disabled) => continue,
            Err(error) => {
                return Err(format!(
                    "cannot read the unresolved effect ledger before acknowledging {}: {error}",
                    claim.key
                ));
            }
        }
        match ledger.acknowledge(&claim.key, &claim.snapshot, Some(note)) {
            Ok(_) => {}
            Err(UnresolvedEffectError::StaleConfirmation) => {
                return Err(format!(
                    "refusing to acknowledge {}: new unknown evidence appeared after the host was shown snapshot {}; a stale confirmation must never acknowledge newer evidence",
                    claim.key, claim.snapshot
                ));
            }
            Err(UnresolvedEffectError::IncompleteEvidence) => {
                return Err(format!(
                    "refusing to acknowledge {}: its evidence is incomplete (facts were dropped) and can never be confirmed from an incomplete snapshot; the unknown and its evidence are preserved",
                    claim.key
                ));
            }
            Err(UnresolvedEffectError::IllegalTransition {
                from: UnresolvedEffectState::Acknowledged | UnresolvedEffectState::Resolved,
                ..
            }) => {}
            Err(UnresolvedEffectError::Disabled) => {}
            Err(error) => {
                return Err(format!(
                    "cannot persist the acknowledgement for {}: {error}",
                    claim.key
                ));
            }
        }
    }
    Ok(())
}

pub(super) struct CliHost {
    pub(super) json: bool,
    /// 有界非阻塞 JSONL 传输；仅 JSON/RPC 模式存在（纯文本模式不创建）。
    pub(super) output: Option<JsonOutput>,
    pub(super) no_shell: bool,
    pub(super) execution_policy: mio_coding_agent::execution_backend::ExecutionPolicy,
    pub(super) approve: bool,
    pub(super) trust_project_mcp: bool,
    pub(super) approved_project_mcp: Arc<Mutex<BTreeSet<String>>>,
    pub(super) reporter: Arc<StatusReporter>,
    pub(super) steering_queue: Mutex<VecDeque<String>>,
    pub(super) rpc_approvals: Option<Arc<RpcApprovals>>,
    pub(super) journal: Arc<Journal>,
    /// 跨重启未决副作用账本。`--no-session` 时完全禁用（零额外落盘）。
    pub(super) ledger: Arc<UnresolvedEffectLedger>,
    /// 该 host 的 journal 与账本所属的工作区根。用于有界的整工作区旧
    /// journal 迁移扫描，不按当前 session 会话裁剪。
    pub(super) workspace: PathBuf,
    /// 整个进程（跨回合）共享的未知副作用闸门。每回合开始时从账本恢复并
    /// 播种；子代理沿用父级同一个 `Arc`，不自行播种。回合之间会被替换成一个
    /// **新**的闸门（真实 top-run 生命周期），旧的 run-scope 别名随之死亡。
    pub(super) unknown_fence: Mutex<Arc<mio_coding_agent::unknown_effect::UnknownEffectFence>>,
    /// 本回合真实取得派发权的账本主键（含规范化操作标签）。回合结束时，仍是
    /// `Outstanding` 的记录会被保守转为 `Unknown`；只覆盖本回合自己的真实
    /// `(scope, call_index)`，绝不升级别的进程/子代理仍在执行的调用。
    pub(super) turn_claims: Mutex<Vec<(UnresolvedEffectKey, String)>>,
    /// `(run scope, provider tool_call_id)` -> canonical `call_index`，用于把
    /// 工具体内发布的类型化 unknown 关联回真实账本主键。只清理已被真实
    /// `RunFinished` 观测确认为结束的 run，绝不在新回合擦掉仍在运行的子 run。
    pub(super) dispatched_calls: Mutex<BTreeMap<(String, String), u64>>,
    /// 来自真实 `RunObservation` 的 run 存活性：`true` = 观测到 `RunStarted`
    /// 且尚未观测到终态。只有真正结束的 run 的遗留派发才会被 finalize。
    pub(super) run_states: Mutex<BTreeMap<String, bool>>,
    /// run 存活表已满且**全部为活跃**：无法再不驱逐活跃 scope 地追踪新 scope。
    /// 此时新的写/外部派发 fail-closed（绝不 clear 表、也绝不偷偷增长）。
    pub(super) run_tracking_saturated: AtomicBool,
    /// `--output-schema PATH` 解析出的最终答复 JSON Schema。默认 `None`，
    /// 此时 [`Host::output_schema`] 走默认实现，旧 CLI/RPC 行为不变。
    pub(super) output_schema: Option<mio_coding_agent::OutputSchema>,
}

impl CliHost {
    /// 始终交互式确认（不受 `--approve` 影响），用于项目 MCP 启动授权。
    fn interactive_approval(
        &self,
        operation: &str,
        target: &str,
        prompt: &str,
    ) -> Result<(), String> {
        let msg = format!("{operation} {target}");
        self.reporter.report(
            ProgramState::Blocked {
                kind: BlockedKind::Permission,
            },
            Some(msg.trim()),
        );
        eprint!("{prompt}");
        let _ = io::stderr().flush();
        let mut answer = String::new();
        let res = io::stdin()
            .read_line(&mut answer)
            .map_err(|e| format!("Failed to read approval input: {e}"));
        self.reporter.report(ProgramState::Working, None);
        res?;
        if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            Ok(())
        } else {
            Err(format!("User rejected tool operation '{operation}'"))
        }
    }

    /// 项目 MCP 配置启动授权（AUD-03）：fail-closed，与工具审批分离。
    ///
    /// 授权绑定具体服务器指纹（命令、参数、环境变量），配置变化会重新请求；
    /// 不会只凭 cwd 布尔值长期信任。`--trust-project-mcp` 或 RPC 审批代理可放行，
    /// 否则必须交互确认；非交互环境直接拒绝。
    fn approve_project_mcp(&self, target: &str) -> HostFuture<Result<(), String>> {
        if self.trust_project_mcp {
            return Box::pin(std::future::ready(Ok(())));
        }
        if self.approved_project_mcp.lock().unwrap().contains(target) {
            return Box::pin(std::future::ready(Ok(())));
        }
        if let Some(broker) = self.rpc_approvals.clone() {
            let cache = self.approved_project_mcp.clone();
            let target_owned = target.to_owned();
            return Box::pin(async move {
                broker
                    .request(PROJECT_MCP_APPROVAL_OP, &target_owned)
                    .await
                    .map(|()| {
                        cache.lock().unwrap().insert(target_owned);
                    })
            });
        }
        if self.json || !io::stdin().is_terminal() {
            return Box::pin(std::future::ready(Err(format!(
                "Project MCP server start requires authorization; no interactive terminal or RPC approval broker is available. Re-run with --trust-project-mcp to authorize: {target}"
            ))));
        }
        let res = self.interactive_approval(
            PROJECT_MCP_APPROVAL_OP,
            target,
            &format!("\nStart project MCP server? {target} [y/N]: "),
        );
        Box::pin(std::future::ready(match res {
            Ok(()) => {
                self.approved_project_mcp
                    .lock()
                    .unwrap()
                    .insert(target.to_owned());
                Ok(())
            }
            Err(error) => Err(error),
        }))
    }

    // JSON/RPC 模式下所有事件都经有界传输；流式增量可丢弃，其余事件 fail-closed。
    fn emit_agent_event(&self, event: AgentEvent) {
        // 工具开始/结束是真实派发的边界：身份取当前执行任务上由工具 wrapper
        // 绑定的真实 `ToolIdentity`（引擎 dispatch 的真实 run/turn/callId/effectId），
        // 再结合该 run 已观测的 session/subagent/parent。没有 scope 就诚实 null，
        // 绝不按派发顺序 peek/pop 配对，更不用工具名或参数 hash 冒充 ID。
        let correlation = match &event {
            AgentEvent::ToolStarted { .. } | AgentEvent::ToolFinished { .. } => {
                self.tool_correlation()
            }
            _ => self.current_correlation(),
        };
        let lossy = matches!(event, AgentEvent::Text(_));
        let value = serde_json::to_value(&event).expect("event serialization");
        if lossy {
            if let Some(output) = &self.output {
                output.emit_lossy_correlated(value, &correlation);
            } else {
                let _ = emit_or_print_correlated(&None, value, false, &correlation);
            }
        } else {
            let _ = emit_or_print_correlated(&self.output, value, false, &correlation);
        }
    }

    /// 最近一次顶层运行的真实关联字段。
    fn current_correlation(&self) -> CorrelationFields {
        self.output
            .as_ref()
            .map(JsonOutput::correlation)
            .unwrap_or_default()
    }

    /// 指定真实 run UUID 的关联字段。
    fn correlation_for_run(&self, run_id: uuid::Uuid) -> CorrelationFields {
        self.output
            .as_ref()
            .map(|output| output.correlation_for_run(run_id))
            .unwrap_or_default()
    }

    /// 当前任务真实工具身份对应的关联字段。
    ///
    /// 身份由工具 wrapper 在 call 期间绑定到 task-local；没有真实绑定（直接
    /// 调用、引擎未提供 scope）时返回诚实 null，而不是猜一个 run/turn/callId。
    fn tool_correlation(&self) -> CorrelationFields {
        let identity = mio_coding_agent::tool_identity::current();
        self.output
            .as_ref()
            .map(|output| output.correlation_for_tool_identity(identity.as_ref()))
            .unwrap_or_default()
    }

    fn approval_correlation(&self, scope: &str, tool_call_id: Option<&str>) -> CorrelationFields {
        self.output
            .as_ref()
            .map(|output| output.approval_correlation(scope, tool_call_id))
            .unwrap_or_default()
    }

    /// Permanently fail the run guard closed for write/external effects.
    ///
    /// Read tooling stays usable: the guard only refuses new side effects, it
    /// never turns a read into a refusal and never fabricates unknown evidence.
    /// The latch survives the per-turn guard swap because it is re-applied to
    /// the fresh guard from the previous one.
    fn mark_guard_persist_failed(&self) {
        self.guard().mark_persist_failed();
    }

    /// The guard for the current top-level run.
    fn guard(&self) -> Arc<mio_coding_agent::unknown_effect::UnknownEffectFence> {
        self.unknown_fence
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Record the observed liveness of one real engine run from a
    /// `RunObservation`. A `RunStarted` marks it live, a terminal observation
    /// marks it ended. The map is bounded: only entries for runs that really
    /// ended are dropped to make room. When the bound is already held by live
    /// runs it is **never** cleared and never grown; instead the host records
    /// that tracking is saturated so a new write/external dispatch fails closed
    /// rather than being finalized from a missing liveness observation.
    fn note_run_liveness(&self, scope: &str, live: bool) {
        let mut states = self
            .run_states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !states.contains_key(scope) {
            if states.len() >= MAX_TRACKED_RUNS {
                states.retain(|_, live| *live);
            }
            if states.len() >= MAX_TRACKED_RUNS {
                // All bounded slots are live: refuse to evict a live scope.
                // An untracked live run simply cannot be finalized; writes are
                // refused while saturated (see `run_tracking_saturated`).
                self.run_tracking_saturated.store(true, Ordering::SeqCst);
                return;
            }
        }
        states.insert(scope.to_owned(), live);
        // Capacity is available again once we are below the bound (or the map
        // is no longer all-live). Recompute from the real state.
        let saturated = states.len() >= MAX_TRACKED_RUNS && states.values().all(|live| *live);
        self.run_tracking_saturated
            .store(saturated, Ordering::SeqCst);
    }

    /// Whether the bounded run-liveness registry cannot track a new live run
    /// without evicting a live one. New write/external dispatches fail closed
    /// while true; reads stay available.
    fn run_tracking_saturated(&self) -> bool {
        self.run_tracking_saturated.load(Ordering::SeqCst)
    }

    /// Snapshot the scopes that really ended, so callers can decide what to
    /// forget without holding the liveness lock and the correlation-map lock at
    /// the same time (which could deadlock against the reverse order).
    fn ended_run_scopes(&self) -> BTreeSet<String> {
        self.run_states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|(_, live)| !**live)
            .map(|(scope, _)| scope.clone())
            .collect()
    }

    /// Whether the run scope is known to have ended (a terminal observation was
    /// seen). A scope with no observation is not "ended": finalization stays
    /// conservative and never marks a possibly-live run as unknown.
    fn run_has_ended(&self, scope: &str) -> bool {
        self.run_states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(scope)
            .copied()
            == Some(false)
    }

    /// Recover durable unknowns, migrate legacy journal unknowns, and seed a
    /// fresh per-turn guard.
    ///
    /// Called once per turn before the engine runs. A positively live
    /// `Outstanding` owner is never upgraded (the dispatch may still be in
    /// flight); an uncertain or dead owner is conservatively upgraded to
    /// `Unknown` by the ledger and seeded as `interrupted_dispatch`. A ledger
    /// that cannot be read leaves the guard permanently fail-closed for
    /// write/external effects, while reads stay usable.
    ///
    /// Every turn gets a **new** guard (the real top-run lifecycle): a
    /// long-lived session cannot accumulate acknowledged history until the
    /// bounded fence overflows, and the previous run scope's alias dies with
    /// the old guard so the bounded scope registry is pruned. Children still
    /// share the parent's `Arc` for the duration of the run.
    pub(super) fn recover_and_seed_guard(&self) {
        // Only a run that really ended may have its dispatch correlation
        // forgotten. A still-live child that outlives the parent turn keeps its
        // mapping so its later typed unknown can still be journaled against the
        // real ledger key; clearing the whole map here would erase it. The
        // ended set is snapshotted first so the liveness lock and the map lock
        // are never held at the same time.
        let ended = self.ended_run_scopes();
        self.dispatched_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|(scope, _), _| !ended.contains(scope));
        self.turn_claims
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|(key, _)| !ended.contains(&key.scope));

        // The durable-write failure latch is a property of the workspace, not
        // of one turn, so carry it onto the fresh guard.
        let sticky = self.guard().persist_failed();
        let fence = mio_coding_agent::unknown_effect::UnknownEffectFence::new();
        if sticky {
            fence.mark_persist_failed();
        }

        let mut ledger_failed = false;
        // Migrate first so a legacy journal `Unknown` seeded into the ledger is
        // picked up by this turn's recovery and blocks the same turn.
        if self.migrate_legacy_journal_unknowns(false).is_err() {
            ledger_failed = true;
        }
        match self.ledger.recover_outstanding() {
            Ok(report) => {
                for entry in report
                    .entries
                    .iter()
                    .filter(|entry| entry.state == UnresolvedEffectState::Unknown)
                {
                    fence.mark(recovered_unknown(entry));
                }
            }
            // --no-session keeps the ledger disabled: no disk I/O, no refusal.
            Err(UnresolvedEffectError::Disabled) => {}
            // A damaged or unreadable ledger must never look like "no unknown".
            Err(_) => ledger_failed = true,
        }
        if ledger_failed {
            fence.mark_persist_failed();
        }
        *self
            .unknown_fence
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = fence;
    }

    /// Migrate durable unknown evidence that predates this run's ledger.
    ///
    /// The scan is bounded and read-only against the journals, and it covers
    /// the whole workspace rather than just the current session: an old
    /// session's `Dispatched` canonical call or legacy unknown operation must
    /// not be silently bypassed by simply starting a new session. A canonical
    /// call with a real `(scope, call_index)` is migrated under that exact real
    /// key. A legacy operation that no canonical call represents is migrated
    /// under a distinct origin derived from its real journal identity
    /// (`session_id` + `run_id` + `op_id`), so it can still be inspected and
    /// acknowledged exactly; no engine run UUID, provider id or evidence id is
    /// invented. Pure reads are skipped: they never carry a side effect and
    /// must never fence a later write.
    ///
    /// Coverage is the durable, authoritative answer to "has every pre-existing
    /// unknown in the whole workspace been migrated?":
    ///
    /// - Before the epoch is committed, the scan must be complete and free of
    ///   loss/corruption, and every real unknown must be written, before the
    ///   ledger atomically marks coverage initialized in the same `flock`
    ///   transaction. Any loss, corruption or partial migration persists the
    ///   monotonic `coverage_incomplete` latch and fails closed: no fabricated
    ///   `(scope, call_index)` stands in for missing evidence and no normal
    ///   acknowledgement can clear it.
    /// - After the epoch is committed, every new runtime side effect already
    ///   went through the ledger before dispatch, so normal journal retention
    ///   truncation is **not** evidence and never becomes an unknown. The scan
    ///   still imports any visible un-represented old unknown under its real
    ///   key, and a corrupt/oversized source still fails closed.
    ///
    /// A disabled or read-only ledger performs no I/O at all, so `--no-session`
    /// and plain recovery queries stay zero-write.
    ///
    /// `commit_empty` controls the no-evidence case. Turn-start recovery passes
    /// `false` so a text-only turn never creates a ledger file just because it
    /// ran; write admission passes `true` so the empty workspace still commits
    /// a durable "complete, nothing to migrate" epoch before the first side
    /// effect.
    fn migrate_legacy_journal_unknowns(&self, commit_empty: bool) -> Result<(), ()> {
        // Nothing can be migrated without a writable ledger, and a read-only
        // query must never create a record. `--no-session` uses a disabled
        // ledger and therefore performs zero extra disk I/O.
        if !self.ledger.mode().is_writable() {
            return Ok(());
        }
        let coverage = self.ledger.coverage().map_err(|_| ())?;
        if coverage.incomplete {
            return Err(());
        }
        let scan = match self.scan_workspace_journals() {
            Ok(scan) => scan,
            Err(()) => {
                // An unreadable/corrupt/oversized source can never prove that no
                // unknown was skipped: latch the durable fail-closed state.
                let _ = self.ledger.mark_coverage_incomplete();
                return Err(());
            }
        };
        let inputs = Self::legacy_inputs(scan.candidates);
        if coverage.initialized {
            // Post-epoch: retention truncation is authoritative-ledger
            // territory and is not treated as evidence. Import only the real
            // visible un-represented unknowns.
            self.ledger.import_unknowns(&inputs).map_err(|_| ())?;
            return Ok(());
        }
        if inputs.is_empty() && !scan.loss && !commit_empty {
            // Nothing to migrate and no real side effect is being admitted yet:
            // leave the epoch uncommitted so a read-only/text-only turn does not
            // create a ledger file. A later write admission commits it.
            return Ok(());
        }
        let complete = !scan.loss;
        match self
            .ledger
            .commit_initial_coverage(&inputs, complete)
            .map_err(|_| ())?
        {
            CoverageCommit::Initialized => Ok(()),
            // A concurrent process committed the epoch first; import our own
            // scan's real unknowns so its evidence is never dropped.
            CoverageCommit::AlreadyInitialized => {
                self.ledger.import_unknowns(&inputs).map_err(|_| ())?;
                Ok(())
            }
            CoverageCommit::MarkedIncomplete | CoverageCommit::AlreadyIncomplete => Err(()),
        }
    }

    /// Commit the workspace coverage epoch before admitting a real write.
    ///
    /// Fast-paths a durable epoch or latch. Otherwise it runs the bounded scan
    /// and commits, including the empty-but-complete case, so the admission
    /// check inside `claim_dispatch` sees durable proof rather than this
    /// process's memory.
    fn ensure_coverage(&self) -> Result<(), String> {
        if !self.ledger.mode().is_writable() {
            return Ok(());
        }
        let coverage = self.ledger.coverage().map_err(|error| {
            format!(
                "the workspace coverage epoch could not be read; refusing a new side effect: {error}"
            )
        })?;
        if coverage.incomplete {
            return Err(
                "the workspace coverage epoch is latched incomplete; refusing a new side effect until a complete migration is possible"
                    .into(),
            );
        }
        if coverage.initialized {
            return Ok(());
        }
        self.migrate_legacy_journal_unknowns(true).map_err(|()| {
            "the workspace coverage epoch could not be proven complete; refusing a new side effect"
                .to_string()
        })
    }

    /// Convert scanned candidates into validated ledger inputs.
    fn legacy_inputs(
        candidates: Vec<LegacyUnknownCandidate>,
    ) -> Vec<(UnresolvedEffectKey, UnknownEffectInput)> {
        candidates
            .into_iter()
            .map(|candidate| {
                let mut input = UnknownEffectInput::new(candidate.operation, candidate.reason);
                if let Some(target) = candidate.target {
                    input = input.with_target(target);
                }
                (candidate.key, input)
            })
            .collect()
    }

    /// Bounded, read-only scan of every journal in the workspace.
    ///
    /// `Err` means the scan could not prove that no unknown was skipped
    /// (unreadable directory/file, non-regular source, parse/version error,
    /// malformed identity, or a scan bound exceeded). `Ok(loss = true)` means a
    /// real dropped/truncated count was seen. Nothing is written here.
    fn scan_workspace_journals(&self) -> Result<WorkspaceJournalScan, ()> {
        let dir = self.workspace.join(journal::JOURNAL_DIR);
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            // No journal directory is a complete scan with no candidates.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(WorkspaceJournalScan {
                    candidates: Vec::new(),
                    loss: false,
                });
            }
            Err(_) => return Err(()),
        };
        let mut files: Vec<PathBuf> = Vec::new();
        let mut total_bytes = 0u64;
        for entry in entries {
            let entry = entry.map_err(|_| ())?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            if files.len() >= MAX_JOURNAL_SCAN_FILES {
                return Err(());
            }
            let metadata = std::fs::symlink_metadata(&path).map_err(|_| ())?;
            if !metadata.is_file() {
                return Err(());
            }
            total_bytes = total_bytes.saturating_add(metadata.len());
            if total_bytes > MAX_JOURNAL_SCAN_BYTES {
                return Err(());
            }
            files.push(path);
        }
        files.sort();
        let mut candidates: Vec<LegacyUnknownCandidate> = Vec::new();
        let mut seen: BTreeSet<(String, u64)> = BTreeSet::new();
        let mut loss = false;
        for path in &files {
            loss |= self.collect_legacy_journal_unknowns(path, &mut candidates, &mut seen)?;
        }
        Ok(WorkspaceJournalScan { candidates, loss })
    }

    /// Bounded, read-only collection of unknown evidence from one journal file.
    ///
    /// The file is parsed directly so a legacy (`v1`) operation keeps its real
    /// `op_id`; anything unreadable, oversized, version-mismatched or malformed
    /// fails closed. Nothing is written here.
    fn collect_legacy_journal_unknowns(
        &self,
        path: &Path,
        candidates: &mut Vec<LegacyUnknownCandidate>,
        seen: &mut BTreeSet<(String, u64)>,
    ) -> Result<bool, ()> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(_) => return Err(()),
        };
        if !metadata.is_file() || metadata.len() > journal::MAX_JOURNAL_FILE_SIZE as u64 {
            return Err(());
        }
        let bytes = std::fs::read(path).map_err(|_| ())?;
        if bytes.len() > journal::MAX_JOURNAL_FILE_SIZE {
            return Err(());
        }
        let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| ())?;
        let version = value.get("version").and_then(|v| v.as_u64()).ok_or(())?;
        if version != journal::JOURNAL_VERSION as u64
            && version != journal::JOURNAL_VERSION_V1 as u64
        {
            return Err(());
        }
        let session_id = value
            .get("session_id")
            .and_then(|v| v.as_str())
            .ok_or(())?
            .to_owned();
        let runs = value.get("runs").and_then(|v| v.as_array()).ok_or(())?;
        // Real dropped/truncated evidence is not a call identity: it is missing
        // evidence that must fail the coverage epoch closed, never a fabricated
        // `(scope, call_index)` a normal acknowledgement could clear.
        let mut loss = false;

        // Every canonical key that carries a provider tool-call id is
        // "represented" regardless of phase: a pending legacy intent whose
        // canonical call is already terminal must not be resurrected.
        let mut covered: BTreeSet<(String, String)> = BTreeSet::new();
        for run in runs {
            let Some(calls) = run.get("calls").and_then(|v| v.as_array()) else {
                continue;
            };
            for call in calls {
                if let Some(tool_call_id) = call.get("tool_call_id").and_then(|v| v.as_str())
                    && let Some(scope) = call.get("scope").and_then(|v| v.as_str())
                {
                    covered.insert((scope.to_owned(), tool_call_id.to_owned()));
                }
            }
        }

        for run in runs {
            let run_id = run.get("run_id").and_then(|v| v.as_str()).ok_or(())?;
            if let Some(calls) = run.get("calls").and_then(|v| v.as_array()) {
                for call in calls {
                    let phase = call.get("phase").and_then(|v| v.as_str()).unwrap_or("");
                    if phase != "dispatched" && phase != "unknown" {
                        continue;
                    }
                    // A pure read is never a write-side unknown and must not
                    // fence a later write.
                    if legacy_json_call_is_read(call) {
                        continue;
                    }
                    let scope = call.get("scope").and_then(|v| v.as_str()).ok_or(())?;
                    let call_index = call.get("call_index").and_then(|v| v.as_u64()).ok_or(())?;
                    let key =
                        UnresolvedEffectKey::new(scope.to_owned(), call_index).map_err(|_| ())?;
                    push_legacy_candidate(
                        candidates,
                        seen,
                        key,
                        call.get("operation")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_owned(),
                        call.get("target")
                            .and_then(|v| v.as_str())
                            .map(str::to_owned),
                        if phase == "unknown" {
                            UnknownFactReason::ResultUnknown
                        } else {
                            UnknownFactReason::InterruptedDispatch
                        },
                    )?;
                }
            }
            if let Some(operations) = run.get("operations").and_then(|v| v.as_array()) {
                for operation in operations {
                    let status = operation
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if status != "pending" && status != "unknown" {
                        continue;
                    }
                    // A pure built-in read operation is skipped for the same
                    // reason as a read canonical call.
                    if legacy_operation_is_read(
                        operation
                            .get("operation")
                            .and_then(|v| v.as_str())
                            .unwrap_or(""),
                    ) {
                        continue;
                    }
                    let scope = operation
                        .get("correlation")
                        .and_then(|c| c.get("scope"))
                        .and_then(|v| v.as_str());
                    let tool_call_id = operation
                        .get("correlation")
                        .and_then(|c| c.get("tool_call_id"))
                        .and_then(|v| v.as_str());
                    let represented = scope.is_some_and(|scope| {
                        tool_call_id
                            .is_some_and(|id| covered.contains(&(scope.to_owned(), id.to_owned())))
                    });
                    if represented {
                        continue;
                    }
                    let op_id = operation.get("op_id").and_then(|v| v.as_u64()).ok_or(())?;
                    if op_id == 0 {
                        return Err(());
                    }
                    let key =
                        UnresolvedEffectKey::new(legacy_origin_scope(&session_id, run_id), op_id)
                            .map_err(|_| ())?;
                    push_legacy_candidate(
                        candidates,
                        seen,
                        key,
                        operation
                            .get("operation")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_owned(),
                        operation
                            .get("target")
                            .and_then(|v| v.as_str())
                            .map(str::to_owned),
                        if status == "unknown" {
                            UnknownFactReason::ResultUnknown
                        } else {
                            UnknownFactReason::InterruptedDispatch
                        },
                    )?;
                }
            }
            // A run that dropped canonical calls cannot be fully represented.
            // Record it as a real loss marker for the coverage epoch.
            let dropped_calls = run
                .get("dropped_calls")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            if dropped_calls > 0 {
                loss = true;
            }
        }
        let dropped_runs = value
            .get("dropped_runs")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let truncated_operations = value
            .get("truncated_operations")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if dropped_runs > 0 || truncated_operations > 0 {
            loss = true;
        }
        Ok(loss)
    }

    /// Conservatively resolve this turn's own leftover dispatch claims.
    ///
    /// A same-process dispatch whose owner is still live would otherwise stay
    /// `Outstanding` forever: recovery only upgrades a non-live owner, and a
    /// later write is never blocked by an `Outstanding` record. At the end of
    /// the turn, every call whose run really ended and did not resolve is
    /// genuinely unconfirmed, so it is recorded as `Unknown` with its real
    /// operation and `(scope, call_index)`. A still-live child (including one
    /// that outlives the parent turn) keeps its claim: only a run the host
    /// observed as ended is finalized, never a possibly-live one.
    pub(super) fn finalize_turn_dispatches(&self) {
        let claims = std::mem::take(
            &mut *self
                .turn_claims
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        let mut retained = Vec::new();
        for (key, operation) in claims {
            if !self.run_has_ended(&key.scope) {
                // Not known to have ended (no terminal observation): keep the
                // claim so a still-running child is never marked Unknown.
                retained.push((key, operation));
                continue;
            }
            match self.ledger.lookup(&key) {
                Ok(Some(observation))
                    if observation.record.state == UnresolvedEffectState::Outstanding =>
                {
                    let input = UnknownEffectInput::new(operation, UnknownFactReason::Interrupted);
                    match self.ledger.record_unknown(&key, input) {
                        Ok(_) => {}
                        Err(UnresolvedEffectError::Disabled) => {}
                        Err(error) => {
                            self.mark_guard_persist_failed();
                            eprintln!(
                                "Warning: unresolved effect ledger could not record the interrupted dispatch for {key}: {error}"
                            );
                        }
                    }
                }
                _ => {}
            }
        }
        *self
            .turn_claims
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = retained;
    }

    /// Remember the canonical `call_index` for a real dispatch so a tool body's
    /// typed unknown can be linked back to its durable ledger key.
    ///
    /// Returns `false` when the bounded map is already full of mappings for
    /// live runs: it never grows past [`MAX_TRACKED_DISPATCHES`] and never
    /// evicts a live run's correlation. The caller must then refuse the new
    /// write/external dispatch instead of admitting an untracked correlation.
    fn note_dispatched_call(
        &self,
        scope: &str,
        tool_call_id: Option<&str>,
        call_index: u64,
    ) -> bool {
        let Some(tool_call_id) = tool_call_id else {
            // No provider tool-call id: there is no correlation to remember, but
            // the canonical `Unknown` fallback still records the durable key, so
            // this is not a missing-correlation refusal.
            return true;
        };
        // Snapshot the ended scopes before taking the map lock so the two
        // locks are never held at the same time (that ordering could deadlock
        // against the reverse order).
        let at_bound = match self.dispatched_calls.lock() {
            Ok(map) => map.len() >= MAX_TRACKED_DISPATCHES,
            Err(_) => true,
        };
        let ended = at_bound.then(|| self.ended_run_scopes());
        let mut map = self
            .dispatched_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // The tracker already caps one run's calls. Only ever drop mappings for
        // runs that really ended: never clear the whole map, which would erase a
        // still-live child's correlation and break its later unknown journaling.
        if let Some(ended) = ended {
            map.retain(|(scope, _), _| !ended.contains(scope));
            if map.len() >= MAX_TRACKED_DISPATCHES {
                return false;
            }
        }
        map.insert((scope.to_owned(), tool_call_id.to_owned()), call_index);
        true
    }

    /// The real `call_index` a currently-running tool body was dispatched as.
    fn dispatched_call_index(&self, scope: &str, tool_call_id: &str) -> Option<u64> {
        self.dispatched_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&(scope.to_owned(), tool_call_id.to_owned()))
            .copied()
    }

    /// Whether a shown claim is already settled in the durable ledger and so
    /// can be reused without asking again.
    fn claim_is_settled(&self, claim: &UnknownClaim) -> bool {
        matches!(
            self.ledger.lookup(&claim.key),
            Ok(Some(observation))
                if matches!(
                    observation.record.state,
                    UnresolvedEffectState::Acknowledged | UnresolvedEffectState::Resolved
                )
        )
    }

    /// Resolve one displayed unknown to exactly one durable claim *before* the
    /// host is asked.
    ///
    /// A recovered record already carries its exact durable key and the
    /// snapshot digest the host was shown, so those semantics are kept verbatim
    /// (a malformed pair is refused rather than guessed). A live runtime marker
    /// (MCP/plugin transport or supervision) carries no ledger key: it must be
    /// resolved through its real `evidence_id` to exactly one persisted record,
    /// and that record's current snapshot digest is the one the host will
    /// acknowledge. A marker with no real evidence id, no matching record, more
    /// than one matching record, or a record with dropped facts fails closed:
    /// the host is never asked to confirm evidence that cannot be bound exactly.
    fn resolve_unknown_claim(&self, effect: &UnknownSideEffect) -> Result<UnknownClaim, String> {
        if effect.ledger_key().is_some() || effect.ledger_snapshot().is_some() {
            return unknown_claim(effect).ok_or_else(|| {
                format!(
                    "the unknown side effect '{}' has a malformed durable-ledger identity; refusing to confirm a claim that cannot be bound exactly",
                    effect.operation
                )
            });
        }
        let Some(evidence_id) = effect.evidence_id else {
            return Err(format!(
                "the unknown side effect '{}' carries neither a durable-ledger key nor a real evidence id; there is nothing to confirm and the new side effect stays refused",
                effect.operation
            ));
        };
        match self.ledger.resolve_evidence(evidence_id) {
            Ok(EvidenceResolution::Unique(record)) => {
                if let Some(scope) = effect.scope
                    && record.key.scope != scope.to_string()
                {
                    return Err(format!(
                        "the unknown side effect '{}' (evidence {evidence_id}) does not match its real run scope; refusing to confirm another run's evidence",
                        effect.operation
                    ));
                }
                if record.facts_dropped > 0 {
                    return Err(format!(
                        "the unknown side effect '{}' (evidence {evidence_id}) has incomplete durable evidence ({} facts dropped); it can never be confirmed from an incomplete snapshot",
                        effect.operation, record.facts_dropped
                    ));
                }
                Ok(UnknownClaim {
                    key: record.key,
                    snapshot: record.snapshot_key,
                })
            }
            Ok(EvidenceResolution::Missing) => Err(format!(
                "the unknown side effect '{}' (evidence {evidence_id}) has no durable unresolved-effect record; refusing to confirm evidence the ledger never persisted",
                effect.operation
            )),
            Ok(EvidenceResolution::Ambiguous(count)) => Err(format!(
                "the unknown side effect '{}' (evidence {evidence_id}) matches {count} durable records; an ambiguous claim can never be confirmed exactly",
                effect.operation
            )),
            Err(UnresolvedEffectError::Disabled) => Err(format!(
                "the unknown side effect '{}' cannot be confirmed because the unresolved effect ledger is disabled; the new side effect stays refused",
                effect.operation
            )),
            Err(error) => Err(format!(
                "cannot read the unresolved effect ledger to resolve the unknown side effect '{}' (evidence {evidence_id}): {error}",
                effect.operation
            )),
        }
    }

    /// Resolve every displayed unknown, failing closed on the first one that
    /// cannot be bound to exactly one durable record.
    fn resolve_unknown_claims(
        &self,
        unknown: &[UnknownSideEffect],
    ) -> Result<Vec<UnknownClaim>, String> {
        unknown
            .iter()
            .map(|effect| self.resolve_unknown_claim(effect))
            .collect()
    }

    /// Claim or advance the durable ledger for one canonical lifecycle record.
    ///
    /// `Dispatched` claims the real `(scope, call_index)` before the bus runs
    /// and, for a write/external call, re-checks the whole workspace's durable
    /// pending unknowns in the same locked transaction so evidence added by
    /// another process after this turn's seed still refuses the dispatch. A
    /// terminal `Executed`/`Failed` resolves it; a published typed `Unknown`
    /// is recorded as `Unknown` (never downgraded to `Executed`). A ledger
    /// failure never blocks a read-only call, but trips the guard and refuses a
    /// write/external dispatch so no side effect runs without durable evidence.
    ///
    /// Returns `true` when this call newly claimed an `Outstanding` intent that
    /// the caller must retract if a later pre-dispatch step fails.
    fn wire_ledger_lifecycle(
        &self,
        call: &mio_coding_agent::tool_lifecycle::ToolCallRecord,
    ) -> Result<bool, String> {
        // DES-09 scope: a pure read carries no side effect. It never claims or
        // records durable dispatch state and never fences a later write; the
        // canonical journal still records its real phase/result honestly.
        if canonical_call_is_read(call) {
            return Ok(false);
        }
        if call.phase == ToolCallPhase::Dispatched && self.run_tracking_saturated() {
            return Err(
                "refusing a new side effect: the bounded run-liveness registry is full of live runs and cannot track this dispatch safely"
                    .into(),
            );
        }
        let key = match UnresolvedEffectKey::new(call.scope.clone(), call.call_index) {
            Ok(key) => key,
            Err(error) => {
                self.mark_guard_persist_failed();
                return Err(format!(
                    "the canonical call {} has an invalid unresolved-effect key: {error}",
                    call.call_index
                ));
            }
        };
        match call.phase {
            ToolCallPhase::Dispatched => {
                // Commit (or latch) the whole-workspace coverage epoch in its
                // own `flock` transaction before the admission claim checks it.
                self.ensure_coverage()?;
                match self.ledger.claim_dispatch(&key, true) {
                    Ok(DispatchAdmission::Acquired) => {
                        if !self.note_dispatched_call(
                            &call.scope,
                            call.tool_call_id.as_deref(),
                            call.call_index,
                        ) {
                            // The bounded correlation map holds only live scopes:
                            // retract the never-dispatched claim instead of leaving
                            // an untracked outstanding record behind.
                            let _ = self.ledger.release_undispatched(&key);
                            return Err(
                            "refusing a new side effect: the bounded dispatch-correlation map is full of live runs; the un-dispatched claim was retracted"
                                .into(),
                        );
                        }
                        self.turn_claims
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push((key, call.operation.clone()));
                        Ok(true)
                    }
                    Ok(DispatchAdmission::Existing(state)) => Err(format!(
                        "refusing to re-dispatch {}: the unresolved effect ledger already records it as {}",
                        key,
                        state.as_str()
                    )),
                    Ok(DispatchAdmission::BlockedByUnknown { pending }) => Err(format!(
                        "refusing to dispatch '{}': the unresolved effect ledger still holds {pending} unacknowledged unknown side effect(s); the host must confirm each exact durable snapshot before a new side effect can run",
                        call.operation
                    )),
                    Ok(DispatchAdmission::BlockedByCoverage {
                        initialized,
                        incomplete,
                    }) => Err(format!(
                        "refusing to dispatch '{}': the workspace coverage epoch is not proven complete (initialized={initialized}, incomplete={incomplete}); a pre-existing unknown may still be un-migrated and no ordinary acknowledgement can clear that",
                        call.operation
                    )),
                    Err(UnresolvedEffectError::Disabled) => Ok(false),
                    Err(error) => {
                        self.mark_guard_persist_failed();
                        Err(format!(
                            "the unresolved effect ledger could not record the dispatch {}; refusing '{}': {error}",
                            key, call.operation
                        ))
                    }
                }
            }
            ToolCallPhase::Executed | ToolCallPhase::Failed => {
                match self.ledger.resolve_confirmed(&key) {
                    Ok(_) => {}
                    Err(UnresolvedEffectError::Disabled) => {}
                    // An unknown is never downgraded to resolved.
                    Err(UnresolvedEffectError::IllegalTransition { .. }) => {}
                    Err(_) => self.mark_guard_persist_failed(),
                }
                // The record is terminal now; its claim no longer needs to be
                // finalized at turn end, so drop it immediately.
                self.forget_turn_claim(&key);
                self.forget_dispatched_call(&call.scope, call.tool_call_id.as_deref());
                Ok(false)
            }
            ToolCallPhase::Unknown => {
                // The typed path already wrote the durable `Unknown`; this is
                // the engine-driven fallback for a published marker with no
                // host callback. An existing `Unknown`/terminal record is left
                // untouched, never reclassified.
                match self.ledger.lookup(&key) {
                    Ok(Some(observation))
                        if observation.record.state != UnresolvedEffectState::Outstanding => {}
                    Ok(_) => {
                        let input = UnknownEffectInput::new(
                            call.operation.clone(),
                            UnknownFactReason::ResultUnknown,
                        );
                        let input = if call.target.is_empty() {
                            input
                        } else {
                            input.with_target(call.target.clone())
                        };
                        match self.ledger.record_unknown(&key, input) {
                            Ok(_) => {}
                            Err(UnresolvedEffectError::Disabled) => {}
                            Err(error) => {
                                self.mark_guard_persist_failed();
                                eprintln!(
                                    "Warning: unresolved effect ledger could not record the unknown for {key}: {error}"
                                );
                            }
                        }
                    }
                    Err(UnresolvedEffectError::Disabled) => {}
                    Err(_) => self.mark_guard_persist_failed(),
                }
                // `Unknown` is terminal for this claim: drop it now.
                self.forget_turn_claim(&key);
                self.forget_dispatched_call(&call.scope, call.tool_call_id.as_deref());
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    /// Retract a claimed `Outstanding` intent that provably never dispatched
    /// because the canonical journal write failed.
    fn retract_undispatched_claim(&self, call: &mio_coding_agent::tool_lifecycle::ToolCallRecord) {
        let Ok(key) = UnresolvedEffectKey::new(call.scope.clone(), call.call_index) else {
            return;
        };
        let _ = self.ledger.release_undispatched(&key);
        self.forget_turn_claim(&key);
        self.forget_dispatched_call(&call.scope, call.tool_call_id.as_deref());
    }

    /// Settle a claim whose canonical `Dispatched` journal record is already
    /// persisted, but whose call provably never reached the bus because a later
    /// pre-dispatch host step failed.
    ///
    /// The intent cannot be retracted from the journal (`Dispatched` has no
    /// legal transition back to `NotDispatched`), so deleting the ledger claim
    /// would let a later workspace scan resurrect it as a false `Unknown`.
    /// A terminal `Resolved` records the same fact honestly: no side effect
    /// occurred and the call is not an unknown effect. Real side effects are
    /// never overwritten because nothing was dispatched.
    fn settle_undispatched_claim(&self, call: &mio_coding_agent::tool_lifecycle::ToolCallRecord) {
        let Ok(key) = UnresolvedEffectKey::new(call.scope.clone(), call.call_index) else {
            return;
        };
        match self.ledger.resolve_confirmed(&key) {
            Ok(_) => {}
            Err(UnresolvedEffectError::Disabled) => {}
            Err(UnresolvedEffectError::IllegalTransition { .. }) => {}
            Err(_) => self.mark_guard_persist_failed(),
        }
        self.forget_turn_claim(&key);
        self.forget_dispatched_call(&call.scope, call.tool_call_id.as_deref());
    }

    /// Drop one claimed key from this turn's pending dispatch list.
    fn forget_turn_claim(&self, key: &UnresolvedEffectKey) {
        self.turn_claims
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|(claim, _)| claim != key);
    }

    /// Forget the real dispatch correlation for one settled call.
    ///
    /// Only called once the call reached a terminal phase or its intent was
    /// retracted: a still-live child's mapping must survive so its later typed
    /// unknown can still be journaled against the real ledger key.
    fn forget_dispatched_call(&self, scope: &str, tool_call_id: Option<&str>) {
        let Some(tool_call_id) = tool_call_id else {
            return;
        };
        self.dispatched_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&(scope.to_owned(), tool_call_id.to_owned()));
    }

    /// Bounded, read-only durable-ledger status for RPC `get_state`.
    ///
    /// A damaged ledger reports its error instead of pretending to be an empty
    /// safe set.
    pub(super) fn ledger_status(&self) -> serde_json::Value {
        ledger_summary(&self.ledger)
    }
}

impl Host for CliHost {
    fn output_schema(&self) -> Option<mio_coding_agent::OutputSchema> {
        self.output_schema.clone()
    }

    fn execution_policy(&self) -> mio_coding_agent::execution_backend::ExecutionPolicy {
        self.execution_policy.clone()
    }

    fn allow_persisted_mcp_trust(&self) -> bool {
        true
    }

    fn on_tool_result(&self, operation: &str, target: &str, result: &Result<String, String>) {
        // The canonical phase is driven by the engine's own outcome boundary;
        // this path only enriches the record with the exact result detail and
        // committed output references, and never changes the phase. A `Denied`
        // or `Unknown` call is therefore never rewritten into `Failed`.
        let Some(identity) = mio_coding_agent::tool_identity::current() else {
            // Engine-less call: keep the legacy operation path only when the
            // journal has an active run.
            if let Err(error) = self.journal.record_result(operation, target, result) {
                eprintln!("Warning: failed to record execution evidence for {operation}: {error}");
            }
            return;
        };
        let detail = match result {
            Ok(text) => text.clone(),
            Err(error) => error.clone(),
        };
        let references = mio_coding_agent::tool_identity::current_output_reference_collector()
            .map(|collector| collector.snapshot())
            .unwrap_or_default();
        if let Err(error) = self.journal.record_canonical_detail(
            &identity.run_id().to_string(),
            Some(identity.tool_call_id()),
            &detail,
            &references,
        ) {
            eprintln!(
                "Warning: failed to record canonical execution evidence for {operation}: {error}"
            );
        }
    }

    fn on_completion_call(&self, call: &mio_coding_agent::CompletionCallObservation) {
        if self.json {
            let correlation = self.correlation_for_run(call.run_id);
            let _ = emit_or_print_correlated(
                &self.output,
                json!({"type": "completion_call", "data": call}),
                false,
                &correlation,
            );
        }
    }

    fn on_diagnostic(&self, diagnostic: &mio_coding_agent::RunDiagnostic) {
        if self.json {
            let correlation = self.correlation_for_run(diagnostic.run_id);
            let _ = emit_or_print_correlated(
                &self.output,
                json!({"type": "diagnostic", "data": diagnostic}),
                false,
                &correlation,
            );
        }
    }

    fn on_context_compaction(
        &self,
        observation: &mio_coding_agent::compact::ContextCompactionObservation,
    ) -> Result<(), String> {
        self.journal.record_context_compaction(observation)?;
        if self.json {
            let correlation = self.correlation_for_run(observation.run_id);
            emit_or_print_correlated(
                &self.output,
                json!({"type": "context_compaction", "data": observation}),
                false,
                &correlation,
            )?;
        }
        Ok(())
    }

    fn on_execution_timing(&self, timing: &mio_coding_agent::ExecutionTiming) {
        if self.json {
            let correlation = self.correlation_for_run(timing.run_id);
            let _ = emit_or_print_correlated(
                &self.output,
                json!({"type": "execution_timing", "data": timing}),
                false,
                &correlation,
            );
        }
    }

    fn shell(&self, command: &str, cwd: &Path) -> HostFuture<Result<String, String>> {
        let (command, cwd, disabled) = (command.to_owned(), cwd.to_owned(), self.no_shell);
        let policy = self.execution_policy.clone();
        Box::pin(async move {
            if disabled {
                return Err("Shell disabled by --no-shell".into());
            }
            mio_coding_agent::shell::execute_with_policy(&command, &cwd, &policy).await
        })
    }

    /// Supervised shell entry point used by [`mio_coding_agent::Workspace`].
    ///
    /// The requested policy must match this host's configured policy, so a run
    /// cannot ask for one isolation level while the host is wired for another.
    /// A match still goes through the host's own `shell`, which keeps the
    /// `--no-shell` refusal and the already-sandboxed `execute_with_policy`
    /// implementation. The engine never runs a native fallback that could
    /// bypass `--no-shell`.
    fn shell_with_policy(
        &self,
        command: &str,
        cwd: &Path,
        policy: &mio_coding_agent::execution_backend::ExecutionPolicy,
    ) -> HostFuture<Result<String, String>> {
        if *policy != self.execution_policy {
            let configured_name = self.execution_policy.name();
            let requested_name = policy.name();
            let configured = self.execution_policy.fingerprint();
            let requested = policy.fingerprint();
            let message = format!(
                "Shell policy mismatch: this host is configured for execution policy '{configured_name}' ({configured}) but the run requested '{requested_name}' ({requested}); refusing to run the command"
            );
            return Box::pin(async move { Err(message) });
        }
        self.shell(command, cwd)
    }

    fn on_event(&self, event: AgentEvent) {
        // 证据记录与输出模式无关：json/rpc 模式也必须落盘。
        if let AgentEvent::ToolFinished {
            operation,
            target,
            result,
        } = &event
        {
            self.on_tool_result(operation, target, result);
        }
        if self.json {
            self.emit_agent_event(event);
        } else {
            match event {
                AgentEvent::Text(text) => {
                    self.reporter
                        .report(ProgramState::Working, Some("Responding"));
                    print!("{text}");
                    if let Err(e) = io::stdout().flush() {
                        eprintln!("Output error: {e}");
                    }
                }
                AgentEvent::ToolStarted { operation, target } => {
                    let msg = format!("{operation} {target}");
                    self.reporter
                        .report(ProgramState::Working, Some(msg.trim()));
                    eprintln!("\n[{operation}] {target}")
                }
                AgentEvent::ToolFinished {
                    operation, result, ..
                } => {
                    self.reporter
                        .report(ProgramState::Working, Some("Waiting for model"));
                    if let Err(error) = result {
                        eprintln!("[{operation} failed] {error}");
                    }
                }
            }
        }
    }

    fn on_reasoning(&self, run_id: uuid::Uuid, part: usize, text: &str) {
        if !self.json {
            return;
        }
        let correlation = self.correlation_for_run(run_id);
        let value = json!({"type": "reasoning", "data": {"part": part, "text": text}});
        if let Some(output) = &self.output {
            output.emit_lossy_correlated(value, &correlation);
        } else {
            let _ = emit_or_print_correlated(&None, value, false, &correlation);
        }
    }

    fn poll_steering(&self) -> Option<String> {
        let mut guard = self.steering_queue.lock().unwrap();
        guard.pop_front()
    }

    fn approve_tool(&self, operation: &str, target: &str) -> Result<(), String> {
        if !self.approve {
            return Ok(());
        }
        if self.json || !io::stdin().is_terminal() {
            return Err(format!(
                "Tool '{operation}' ({target}) requires interactive approval (--approve)"
            ));
        }
        self.interactive_approval(
            operation,
            target,
            &format!("\nApprove [{operation}] {target}? [y/N]: "),
        )
    }

    fn approve_tool_async(&self, operation: &str, target: &str) -> HostFuture<Result<(), String>> {
        if operation == PROJECT_MCP_APPROVAL_OP {
            return self.approve_project_mcp(target);
        }
        // 副作用之前持久化 intent；journal 写入失败必须阻止该操作。
        if let Err(error) = self.journal.record_intent(operation, target) {
            return Box::pin(std::future::ready(Err(format!(
                "Execution journal unavailable; refusing '{operation}' ({target}): {error}"
            ))));
        }
        if let Some(broker) = &self.rpc_approvals {
            broker.request(operation, target)
        } else {
            let res = self.approve_tool(operation, target);
            Box::pin(std::future::ready(res))
        }
    }

    fn approve_tool_binding(
        &self,
        operation: &str,
        target: &str,
        binding: &mio_coding_agent::policy::ApprovalBinding,
    ) -> HostFuture<Result<(), String>> {
        // 项目 MCP 启动授权是独立的宿主路径（不是模型工具调用），保持原有特殊逻辑。
        if operation == PROJECT_MCP_APPROVAL_OP {
            return self.approve_tool_async(operation, target);
        }
        // The canonical `Normalized` record (including this approval attempt's
        // `callId`) is persisted by the policy gate before this approval is
        // requested, so the host no longer writes a separate legacy intent for
        // an engine tool call.
        if let Some(broker) = &self.rpc_approvals {
            let correlation =
                self.approval_correlation(&binding.scope, binding.tool_call_id.as_deref());
            // The pending request retains this immutable dispatch binding. A reply
            // can release only its own one-shot receiver, never a later tool call.
            broker.request_bound(operation, target, Some(binding), Some(&correlation))
        } else {
            let res = self.approve_tool(operation, target);
            Box::pin(std::future::ready(res))
        }
    }

    /// Explicit host confirmation for a new write/external dispatch while an
    /// earlier call's side effect is unknown.
    ///
    /// This is deliberately separate from [`Host::approve_tool_binding`]: it is
    /// not a tool approval, it never writes a journal intent, and it does not
    /// release the normal parameter-bound approval that still follows. For a
    /// recovered unknown it acknowledges exactly the shown durable-ledger key
    /// and snapshot digest; an already acknowledged record is reused without
    /// asking again, and a stale digest is refused rather than acknowledging
    /// newer evidence. RPC reuses the bounded approval broker (same pending
    /// limit and timeout); a non-interactive JSON run has no way to answer, so
    /// it refuses.
    fn confirm_unknown_side_effect(
        &self,
        call: &mio_coding_agent::policy::NormalizedCall,
        unknown: &[mio_coding_agent::unknown_effect::UnknownSideEffect],
    ) -> HostFuture<Result<(), String>> {
        let summary = unknown_summary(unknown);
        // The exact durable claim behind every displayed unknown is resolved
        // BEFORE the host is asked. A live runtime marker carries only its real
        // `evidence_id`, so this is what turns it into exactly one
        // `(scope, call_index)` key and the exact snapshot digest that will be
        // acknowledged. Nothing is re-resolved after the approval, so new
        // evidence appearing while the prompt waits makes the acknowledgement
        // stale instead of silently covering it.
        let claims = match self.resolve_unknown_claims(unknown) {
            Ok(claims) => claims,
            Err(error) => return Box::pin(std::future::ready(Err(error))),
        };
        let claims_for_ack = claims.clone();
        let all_settled =
            !claims.is_empty() && claims.iter().all(|claim| self.claim_is_settled(claim));
        if all_settled {
            return Box::pin(async { Ok(()) });
        }
        if let Some(broker) = &self.rpc_approvals {
            let broker = broker.clone();
            let ledger = self.ledger.clone();
            return Box::pin(async move {
                broker
                    .request(CONFIRM_UNKNOWN_SIDE_EFFECT_OP, &summary)
                    .await?;
                persist_unknown_claims(
                    &ledger,
                    &claims_for_ack,
                    "confirmed unknown-side-effect site",
                )
            });
        }
        if self.json || !io::stdin().is_terminal() {
            return Box::pin(std::future::ready(Err(format!(
                "an earlier call's side effect is unknown ({summary}); confirming a new '{}' ({}) requires an interactive terminal or an RPC approval broker",
                call.operation, call.display_target
            ))));
        }
        let res = self.interactive_approval(
            CONFIRM_UNKNOWN_SIDE_EFFECT_OP,
            &summary,
            &format!(
                "\nAn earlier call's side effect is unknown ({summary}). Allow the new [{}] {}? [y/N]: ",
                call.operation, call.display_target
            ),
        );
        match res {
            Ok(()) => Box::pin(std::future::ready(persist_unknown_claims(
                &self.ledger,
                &claims_for_ack,
                "confirmed unknown-side-effect site",
            ))),
            Err(error) => Box::pin(std::future::ready(Err(error))),
        }
    }

    /// The unknown-effect fence that gates this run's write/external
    /// dispatches (DES-09).
    ///
    /// The CLI host owns one process-wide guard, seeded from the durable ledger
    /// at the start of every turn, so a recovered unknown keeps blocking new
    /// side effects across restarts and new sessions. Delegated children reuse
    /// this same `Arc` and never re-seed it.
    fn unknown_effect_fence(
        &self,
    ) -> Option<Arc<mio_coding_agent::unknown_effect::UnknownEffectFence>> {
        Some(self.guard())
    }

    /// Persist one authoritative tool-call lifecycle record and, under the
    /// negotiated v1 envelope, surface it as an optional event.
    ///
    /// The event is emitted only when the client negotiated v1, so a legacy
    /// client's stream is never silently changed. An output-queue error is
    /// propagated to the tracker so a disconnected or saturated output channel
    /// cannot admit a later tool dispatch.
    fn on_tool_call_lifecycle(
        &self,
        call: &mio_coding_agent::tool_lifecycle::ToolCallRecord,
    ) -> Result<(), String> {
        if call.phase == ToolCallPhase::Dispatched {
            // DES-09: claim the real `(scope, call_index)` in the cross-restart
            // ledger, and re-check the whole workspace's durable unknowns in
            // the same locked transaction, BEFORE the canonical journal
            // records `Dispatched`. A refusal must not leave a journal
            // `Dispatched` for a call that never reached the bus, which a later
            // recovery would misreport as `Unknown`. A read-only call is
            // recorded conservatively too, but a ledger failure never blocks a
            // read; a write/external failure fails the dispatch closed.
            let claimed = self.wire_ledger_lifecycle(call)?;
            if let Err(error) = self.journal.record_canonical_phase(call) {
                if claimed {
                    // The call provably never dispatched: retract the pure
                    // outstanding intent so recovery cannot report a false
                    // `Unknown` for it.
                    self.retract_undispatched_claim(call);
                }
                return Err(error);
            }
            if let Some(output) = &self.output
                && output.is_failed()
            {
                // The canonical journal already persisted `Dispatched`, but the
                // pre-dispatch host step failed, so the bus never ran. Settle
                // the claim as not-unknown instead of leaving a false Unknown.
                if claimed {
                    self.settle_undispatched_claim(call);
                }
                return Err(output
                    .failure()
                    .unwrap_or_else(|| "JSON output transport failed".to_owned()));
            }
        } else {
            self.journal.record_canonical_phase(call)?;
            self.wire_ledger_lifecycle(call)?;
        }
        if let Some(output) = &self.output
            && output.is_failed()
        {
            return Err(output
                .failure()
                .unwrap_or_else(|| "JSON output transport failed".to_owned()));
        }
        if let Some(output) = &self.output
            && output.protocol_state().envelope == Envelope::V1
        {
            let correlation = CorrelationFields {
                session_id: output.session_id(),
                run_id: uuid::Uuid::parse_str(&call.scope).ok(),
                turn_id: call.turn,
                tool_call_id: call.tool_call_id.clone(),
                subagent_id: None,
                parent_run_id: None,
            };
            if let Err(error) = emit_or_print_correlated(
                &self.output,
                json!({"type": "tool_call_lifecycle", "data": call}),
                false,
                &correlation,
            ) {
                // A queue-failure on this very event happens after both durable
                // writes. For a `Dispatched` call the engine still never reached
                // the bus, so reconcile to a terminal not-unknown record rather
                // than a later false `Unknown`.
                if call.phase == ToolCallPhase::Dispatched {
                    self.settle_undispatched_claim(call);
                }
                return Err(error);
            }
        }
        Ok(())
    }

    /// Record a real unknown result as `Unknown`, never as `Failed` or
    /// `Executed`. A persistence failure is returned so the tool fails closed
    /// and refuses later side effects.
    ///
    /// The durable ledger is written before this returns. The canonical
    /// `call_index` comes from the real dispatch this tool body is running
    /// under; if no real dispatch identity is available the engine-driven
    /// fallback in [`Self::wire_ledger_lifecycle`] records `Unknown` from the
    /// canonical record instead, and no id is invented here.
    fn on_unknown_side_effect(
        &self,
        effect: &mio_coding_agent::unknown_effect::UnknownSideEffect,
    ) -> Result<(), String> {
        let detail = format!("result unknown ({})", effect.reason.as_str());
        let journal_result = match mio_coding_agent::tool_identity::current() {
            Some(identity) => self.journal.record_canonical_unknown(
                &identity.run_id().to_string(),
                Some(identity.tool_call_id()),
                &detail,
            ),
            None => self
                .journal
                .record_unknown_result(&effect.operation, &effect.target, &detail),
        };
        if let Some(identity) = mio_coding_agent::tool_identity::current()
            && let Some(call_index) =
                self.dispatched_call_index(&identity.run_id().to_string(), identity.tool_call_id())
            && let Ok(key) = UnresolvedEffectKey::new(identity.run_id().to_string(), call_index)
        {
            let input = UnknownEffectInput::new(
                effect.operation.clone(),
                ledger_fact_reason(effect.reason),
            )
            .with_optional_evidence_id(effect.evidence_id);
            let input = if effect.target.is_empty() {
                input
            } else {
                input.with_target(effect.target.clone())
            };
            match self.ledger.record_unknown(&key, input) {
                Ok(_) => {}
                Err(UnresolvedEffectError::Disabled) => {}
                Err(error) => {
                    self.mark_guard_persist_failed();
                    return Err(format!(
                        "the unresolved effect ledger could not record the unknown for {}: {error}",
                        key
                    ));
                }
            }
        }
        journal_result
    }

    /// The host's own persisted session identity, when one exists.
    fn session_id(&self) -> Option<String> {
        self.output.as_ref().and_then(JsonOutput::session_id)
    }

    /// Record a real run observation and, in the v1 envelope, surface the run's
    /// real start/finish so a client can locate the run without guessing.
    fn on_run_observation(&self, observation: &mio_coding_agent::RunObservation) {
        // Scope observations are the public canonical-journal API: a real SDK
        // host binds scopes exactly like this CLI adapter. A binding failure is
        // reported here and re-enforced at the next canonical write, which fails
        // closed rather than routing evidence to the wrong run.
        if let Err(error) = self.journal.record_run_observation(observation) {
            eprintln!("Warning: failed to record run scope evidence: {error}");
        }
        // Track real run liveness before any output-mode early return: the
        // dispatch-ownership decision must not depend on JSON mode.
        match observation.kind {
            mio_coding_agent::RunObservationKind::RunStarted => {
                self.note_run_liveness(&observation.run_id.to_string(), true);
            }
            mio_coding_agent::RunObservationKind::RunFinished => {
                self.note_run_liveness(&observation.run_id.to_string(), false);
            }
            _ => {}
        }
        if !self.json {
            return;
        }
        let Some(output) = &self.output else {
            return;
        };
        output.observe(observation);
        // Evaluation consumes ordinary --json as well as the v1 envelope.
        // Report only the post-admission dispatch, never ModelPrepared.
        if observation.kind == mio_coding_agent::RunObservationKind::ModelDispatched {
            let correlation = output.correlation_for_run(observation.run_id);
            let _ = output.emit_critical_correlated(
                json!({"type": "run_observation", "data": observation}),
                &correlation,
            );
            return;
        }
        if output.protocol_state().envelope != Envelope::V1 {
            return;
        }
        if matches!(
            observation.kind,
            mio_coding_agent::RunObservationKind::RunStarted
                | mio_coding_agent::RunObservationKind::RunFinished
        ) {
            let correlation = output.correlation_for_run(observation.run_id);
            let _ = output.emit_critical_correlated(
                json!({"type": "run_observation", "data": observation}),
                &correlation,
            );
        }
    }

    /// 发出一条真实 read 观测；仅 JSON/RPC 模式，事件只带工作区相对路径与实际
    /// 返回范围，正文不进入事件。
    ///
    /// 关联字段来自该次 read 真实绑定的 scope（run/turn/toolCallId/effectId），
    /// 子代理转发后还带真实父子身份；引擎未提供的字段保持 null。
    fn on_read_observation(&self, observation: &mio_coding_agent::ReadObservation) {
        if !self.json {
            return;
        }
        let Some(output) = &self.output else {
            return;
        };
        let correlation = self.tool_correlation();
        let _ = output.emit_correlated(
            json!({"type": "read_observation", "data": observation}),
            &correlation,
        );
    }
}

pub(super) fn session_list(
    directory: &Path,
) -> Result<Vec<mio_coding_agent::session::SessionMetadata>, String> {
    if !directory.try_exists().map_err(|e| e.to_string())? {
        return Ok(Vec::new());
    }
    let (list, warnings) = Session::list(directory)?;
    for warning in warnings {
        eprintln!("Warning: {warning}");
    }
    // Session timestamps use seconds. File timestamps distinguish saves made
    // within the same second, so a freshly forked task is selected reliably.
    let mut dated = list
        .into_iter()
        .map(|session| {
            let modified = std::fs::metadata(&session.file_path)
                .and_then(|metadata| metadata.modified())
                .map_err(|e| e.to_string())?;
            Ok((modified, session))
        })
        .collect::<Result<Vec<_>, String>>()?;
    dated.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.id.cmp(&b.1.id)));
    Ok(dated.into_iter().map(|(_, session)| session).collect())
}

pub(super) fn print_sessions(directory: &Path, json: bool) -> Result<(), String> {
    print_sessions_with(directory, json, &None)
}

/// `--list-sessions --json` 的有界传输版本；纯文本模式仍用 [`print_sessions`]。
pub(super) fn print_sessions_json(
    directory: &Path,
    output: &Option<JsonOutput>,
) -> Result<(), String> {
    print_sessions_with(directory, output.is_some(), output)
}

fn print_sessions_with(
    directory: &Path,
    json: bool,
    output: &Option<JsonOutput>,
) -> Result<(), String> {
    for session in session_list(directory)? {
        if json {
            let _ = emit_or_print(
                output,
                serde_json::json!({"type":"session", "data":session}),
                false,
            );
        } else if let Some(name) = &session.name {
            println!(
                "{}  [{}]  {} messages  {}",
                session.id,
                name,
                session.message_count,
                session.file_path.display()
            );
        } else {
            println!(
                "{}  {} messages  {}",
                session.id,
                session.message_count,
                session.file_path.display()
            );
        }
    }
    Ok(())
}

pub(super) fn print_models(query: Option<&str>, output: &Option<JsonOutput>) {
    let entries = list_catalog_models(query);
    if output.is_some() {
        for entry in entries {
            let _ = emit_or_print(output, json!({"type": "model", "data": entry}), false);
        }
    } else {
        for entry in entries {
            let status = if entry.configured { "ready" } else { "unset" };
            let thinking = if entry.supports_thinking {
                "thinking"
            } else {
                "standard"
            };
            println!(
                "{}/{}  [{}] ({}, {})",
                entry.provider, entry.model, status, thinking, entry.env_var
            );
        }
    }
}

/// Project one canonical task's live records into the CLI's existing evidence.
///
/// The tracker is the single writer of task phase; this sink is a pure
/// projection. It attaches the real engine run UUID and canonical task id to
/// the active journal run, mirrors the phase into the dedup ledger (for the
/// one task this turn claimed), and never invents a phase for old data. The
/// callback runs outside the tracker's state lock, so a snapshot query here is
/// deadlock-free.
pub(super) struct CliTaskProjection {
    host: Arc<CliHost>,
    dedup: Arc<TaskKeyLedger>,
    /// The dedup ledger's task id for this turn; `None` for an untracked turn.
    dedup_task: Option<String>,
}

impl CliTaskProjection {
    pub(super) fn new(
        host: Arc<CliHost>,
        dedup: Arc<TaskKeyLedger>,
        dedup_task: Option<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            host,
            dedup,
            dedup_task,
        })
    }

    fn project(&self, record: &TaskRecord) -> Result<(), String> {
        let task_id = record.task_id.as_str();
        // Route strictly by the real canonical task id (and its real engine run
        // UUID): a queued sibling whose journal run has not opened yet is left
        // unknown instead of writing its phase onto whichever run happens to be
        // active, and a child never overwrites another run's phase.
        self.host.journal.project_task_canonical(
            task_id,
            record.engine_run_id.as_ref().map(|run| run.as_str()),
            record.phase.as_str(),
        )?;
        if self.dedup_task.as_deref() == Some(task_id) {
            match record.phase {
                TaskPhase::Running => self.dedup.begin(task_id)?,
                TaskPhase::Completed => self.dedup.finish(task_id, TaskKeyState::Completed)?,
                TaskPhase::Failed => self.dedup.finish(task_id, TaskKeyState::Failed)?,
                TaskPhase::Cancelled => self.dedup.finish(task_id, TaskKeyState::Cancelled)?,
                // No legacy mapping exists for Queued (already recorded by
                // `claim`) or TimedOut (never faked as Failed).
                TaskPhase::Queued | TaskPhase::TimedOut => {}
            }
            self.dedup.record_canonical(
                task_id,
                record.phase.as_str(),
                record.engine_run_id.as_ref().map(|run| run.as_str()),
            )?;
        }
        Ok(())
    }
}

impl TaskLifecycleSink for CliTaskProjection {
    fn on_task_lifecycle(&self, record: &TaskRecord) -> Result<(), String> {
        self.project(record)
    }
}

/// 创建并登记一个 canonical 任务（`Queued`）。
///
/// 这是全进程唯一的 canonical 句柄构造点：`Runner::new_task_handle` 与 RPC
/// 入队期的 `QueuedHandleFactory` 都复用它，绝不出现第二套任务构造。
/// `dedup_task` 是去重账本的真实 task id（经 `--dedup-key` / RPC dedup_key
/// 到达）；否则生成一个独立的新 id。它绝不从 engine run UUID 推导。
///
/// 取 `&Arc` 而非 `&mut Runner`，因此活动 RPC 回合的 future 独占 `&mut Runner`
/// 时，入队处理器仍可经共享 factory 登记句柄。
pub(super) fn create_task_handle(
    host: &Arc<CliHost>,
    dedup: &Arc<TaskKeyLedger>,
    dedup_task: Option<String>,
) -> Result<TaskHandle, String> {
    let raw = dedup_task
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let task_id = TaskId::new(raw).map_err(|error| error.to_string())?;
    let projection = CliTaskProjection::new(host.clone(), dedup.clone(), dedup_task);
    let tracker = TaskLifecycleTracker::new(task_id.as_str().to_string(), projection.clone());
    tracker
        .register(TaskIdentity::new(task_id.clone()), None)
        .map_err(|error| error.to_string())?;
    Ok(TaskHandle::new(tracker, task_id))
}

pub(super) struct Runner {
    pub(super) config: Config,
    pub(super) provider_name: String,
    pub(super) connections: BTreeMap<String, Config>,
    pub(super) options: RunOptions,
    pub(super) tool_filter: ToolFilter,
    pub(super) no_mcp: bool,
    pub(super) no_session: bool,
    pub(super) host: Arc<CliHost>,
    pub(super) session: Session,
    pub(super) path: PathBuf,
    pub(super) subagents: Option<Arc<SubagentManager>>,
    pub(super) antigravity: Option<mio_coding_agent::antigravity::Antigravity>,
    pub(super) journal: Arc<Journal>,
    pub(super) checkpoints: Arc<CheckpointStore>,
    pub(super) dedup: Arc<TaskKeyLedger>,
}

impl Runner {
    pub(super) fn set_model(
        &mut self,
        spec: &str,
        thinking: Option<ThinkingLevel>,
    ) -> Result<(), String> {
        let (provider, model, inline_thinking) = parse_model_spec(spec);
        if model.trim().is_empty() || model.len() > 1024 || model.ends_with('/') {
            return Err("Model must be nonempty and at most 1024 bytes".into());
        }
        let name = provider.unwrap_or_else(|| self.provider_name.clone());
        let name = if name == "gemini" {
            "google".to_owned()
        } else {
            name
        };
        let mut config = if name == self.provider_name {
            self.config.clone()
        } else if let Some(saved) = self.connections.get(&name) {
            saved.clone()
        } else {
            let explicit_base = if name == "custom" {
                std::env::var("MIO_BASE_URL").ok()
            } else {
                None
            };
            let (provider, base_url, api_key) = resolve_provider_endpoint(&name, explicit_base)?;
            Config {
                provider,
                base_url,
                api_key,
                model: model.clone(),
            }
        };
        config.model = model;
        let account = if config.provider == Provider::Antigravity {
            Some(match self.antigravity.clone() {
                Some(account) => account,
                None => mio_coding_agent::antigravity::Antigravity::from_profile(
                    mio_coding_agent::profile::config_directory()?,
                )?,
            })
        } else {
            None
        };
        let mut options = self.options.clone();
        if let Some(level) = thinking.or(inline_thinking) {
            options.thinking = Some(level);
        }
        mio_coding_agent::validate_model_thinking(&config, options.thinking)?;
        mio_coding_agent::validate_model_temperature(
            &config,
            options.temperature,
            options.thinking,
            Some(&name),
        )?;
        options.catalog_provider = Some(name.clone());
        mio_coding_agent::context_budget::ContextBudget::resolve(&config, &options)?;
        if let Some(manager) = self.subagents.as_mut() {
            Arc::get_mut(manager)
                .ok_or("Cannot switch a shared or active subagent manager")?
                .reconfigure(config.clone(), options.clone(), account.clone())?;
        }
        // Commit the entire connection only after all validation succeeds.
        self.connections.insert(name.clone(), config.clone());
        self.config = config;
        self.provider_name = name;
        self.options = options;
        self.antigravity = account;
        Ok(())
    }

    /// 切换后续回合是否在 `write` / `edit` 前保存 pre-edit 快照（默认关闭）。
    pub(super) fn set_checkpoints(&mut self, enabled: bool) -> Result<(), String> {
        if enabled && !self.checkpoints.is_enabled() {
            return Err(
                "File checkpoints require a session; --no-session keeps them disabled".into(),
            );
        }
        let mut options = self.options.clone();
        options.checkpoints = enabled.then(|| self.checkpoints.clone());
        if let Some(manager) = self.subagents.as_mut() {
            Arc::get_mut(manager)
                .ok_or("Cannot switch a shared or active subagent manager")?
                .reconfigure(
                    self.config.clone(),
                    options.clone(),
                    self.antigravity.clone(),
                )?;
        }
        self.options = options;
        Ok(())
    }

    pub(super) async fn rewind_checkpoint(
        &self,
        selector: &str,
    ) -> Result<mio_coding_agent::checkpoint::RewindOutcome, String> {
        let shared = self
            .options
            .scheduler
            .as_ref()
            .and_then(|scheduler| scheduler.shared.clone());
        rewind_file(
            &self.session.cwd,
            self.checkpoints.clone(),
            self.host.clone(),
            self.tool_filter.clone(),
            shared,
            self.subagents.clone(),
            selector,
        )
        .await
    }

    /// 当前回合是否启用文件检查点。
    pub(super) fn checkpoints_enabled(&self) -> bool {
        self.options.checkpoints.is_some()
    }

    pub(super) fn commit_session(
        &mut self,
        mut candidate: Session,
        path: PathBuf,
    ) -> Result<(), String> {
        if !self.no_session {
            let parent = path.parent().ok_or("Invalid session path")?;
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Cannot create session directory: {e}"))?;
            candidate.save(&path)?;
        }
        let session_changed = candidate.id != self.session.id;
        self.options.pinned_context = candidate.pinned_context.clone();
        self.session = candidate;
        self.path = path;
        if session_changed {
            self.journal
                .rebind(&self.session.cwd, &self.session.id.to_string());
        }
        // 关联 ID 只在真实存在时附上；会话切换后同步给输出封套。
        if let Some(output) = &self.host.output {
            output.set_session_id(Some(self.session.id.to_string()));
        }
        Ok(())
    }

    pub(super) async fn turn(&mut self, prompt: &str) -> Result<(), String> {
        let (_, result) = self.run_turn(prompt).await;
        result
    }

    /// 执行一个回合并返回其终态，供任务 key 账本记录。
    ///
    /// 未绑定任务时内部创建一个真实 canonical 任务，因此所有 CLI 回合都有
    /// 真实 task id（与 engine run UUID 独立）。
    pub(super) async fn run_turn(&mut self, prompt: &str) -> (RunState, Result<(), String>) {
        // A turn whose canonical task cannot be registered must not run: it
        // would have no real tracking handle, so no Provider request is made.
        let handle = match self.new_task_handle(None) {
            Ok(handle) => handle,
            Err(error) => {
                self.host.reporter.report(ProgramState::Error, Some(&error));
                return (
                    RunState::Failed,
                    Err(format!("cannot register the canonical task: {error}")),
                );
            }
        };
        self.run_turn_with_task(prompt, Some(handle)).await
    }

    /// 创建并登记一个 canonical 任务（`Queued`），委托给全进程唯一构造点
    /// [`create_task_handle`]。
    ///
    /// `dedup_task` 是去重账本的真实 task id（经 `--dedup-key` / RPC dedup_key
    /// 到达）；否则生成一个独立的新 id。它绝不从 engine run UUID 推导。
    pub(super) fn new_task_handle(&self, dedup_task: Option<String>) -> Result<TaskHandle, String> {
        create_task_handle(&self.host, &self.dedup, dedup_task)
    }

    /// 执行一个已绑定 canonical 任务的回合。
    ///
    /// 引擎只在真实调度许可处准入（`Queued -> Running`）；这里只负责在回合
    /// 结束时从同一结果结算终态。取消时先记录取消请求，终态由真实 worker
    /// 回收后落地。
    pub(super) async fn run_turn_with_task(
        &mut self,
        prompt: &str,
        handle: Option<TaskHandle>,
    ) -> (RunState, Result<(), String>) {
        self.run_turn_with_task_images(prompt, Vec::new(), handle)
            .await
    }

    pub(super) async fn run_turn_with_task_images(
        &mut self,
        prompt: &str,
        images: Vec<Image>,
        handle: Option<TaskHandle>,
    ) -> (RunState, Result<(), String>) {
        self.host.reporter.cancelled.store(false, Ordering::SeqCst);
        self.host
            .reporter
            .report(ProgramState::Working, Some("Waiting for model"));
        let task_id = handle
            .as_ref()
            .map(|handle| handle.task_id().as_str().to_string());
        if let Err(error) = self
            .journal
            .begin_run_with_task(Some(prompt), task_id.as_deref())
        {
            eprintln!(
                "Warning: execution evidence journal unavailable ({error}); side-effecting tools will be refused."
            );
        }
        // DES-09: recover durable unknowns and seed the process-wide guard
        // before this turn's dispatches. A live owner is never upgraded; an
        // uncertain or dead one stays blocking as a recovered unknown.
        self.host.recover_and_seed_guard();
        // The task-local scope makes the engine attach its real run UUID and
        // lets workspace/shell count real workers against the same task. An
        // unbound turn runs without any task scope and changes nothing.
        let turn = async {
            if let Some(handle) = handle.clone() {
                scope_task(handle, self.turn_inner_images(prompt, &images)).await
            } else {
                self.turn_inner_images(prompt, &images).await
            }
        };
        let mut cancelled = false;
        // Whether the cancellation path itself completed cleanly. A failed
        // worker cleanup is a real failure and must never be reported as idle.
        let mut cancelled_cleanly = false;
        let res = tokio::select! {
            res = turn => res,
            signal = tokio::signal::ctrl_c() => {
                cancelled = true;
                if let Err(e) = signal {
                    Err(e.to_string())
                } else if let Err(cleanup_err) = cancel_children(self.subagents.as_ref()).await {
                    self.host.reporter.report(ProgramState::Error, Some(&cleanup_err));
                    Err(cleanup_err)
                } else {
                    self.host.reporter.cancelled.store(true, Ordering::SeqCst);
                    cancelled_cleanly = true;
                    Err(
                        "Cancelled; previous session retained. File changes are not rolled back."
                            .into(),
                    )
                }
            }
        };
        // AUD-13: 回合结束时保留证据。已执行操作保持 Executed，未记录结果的 intent 标记为 Unknown。
        let run_state = if cancelled {
            RunState::Cancelled
        } else if res.is_ok() {
            RunState::Completed
        } else {
            RunState::Failed
        };
        let mut settled = true;
        if let Some(handle) = &handle {
            if cancelled {
                let _ = handle.request_cancel(Some("user cancelled the turn"));
            }
            let settlement = match run_state {
                RunState::Completed => Settlement::Completed,
                RunState::Cancelled => Settlement::Cancelled,
                RunState::Active | RunState::Failed | RunState::Interrupted => Settlement::Failed,
            };
            if let Err(error) = handle.finish(settlement, None) {
                eprintln!("Warning: canonical task settlement failed: {error}");
            }
            // Wait for the real workers (shell reapers, blocking readers, live
            // child scopes) to settle before the terminal phase becomes durable
            // and before a done/idle state is reported. `wait_settled` also
            // returns the last committed record after a latched persistence
            // failure, so a broken sink cannot hang the turn; a non-terminal
            // record is reported honestly instead of as a false settlement.
            if let Err(error) = wait_for_settlement(handle).await {
                settled = false;
                eprintln!("Warning: {error}");
            }
        }
        if let Err(error) = self.journal.finish_run(run_state) {
            eprintln!("Warning: failed to finalize execution evidence: {error}");
        }
        // DES-09: a same-process dispatch left `Outstanding` by a cancelled or
        // unfinished turn is genuinely unconfirmed. Its owner is still live, so
        // recovery would never upgrade it; convert only this turn's own claims
        // to `Unknown` so a later write is blocked until the host confirms.
        self.host.finalize_turn_dispatches();
        if cancelled {
            // No idle may be reported before the real workers (shell reapers,
            // blocking readers, live child scopes) settled AND the cleanup
            // itself succeeded. A broken sink or a failed cleanup is reported
            // honestly as an error instead of a false idle.
            if !settled || !cancelled_cleanly {
                let error = if cancelled_cleanly {
                    "Cancelled, but the canonical task did not settle; refusing to report a false idle state."
                } else {
                    "Cancelled, but the worker cleanup did not complete; refusing to report a false idle state."
                };
                self.host.reporter.report(ProgramState::Error, Some(error));
                return (run_state, Err(error.to_owned()));
            }
            // The real settlement is confirmed; only now is an idle report
            // honest.
            self.host.reporter.report(ProgramState::Idle, None);
            return (run_state, res);
        }
        if !settled {
            let error = "The turn ended, but its canonical task did not settle; reporting an error instead of a false completion.";
            self.host.reporter.report(ProgramState::Error, Some(error));
            return (run_state, Err(error.to_owned()));
        }
        match res {
            Ok(()) => {
                self.host.reporter.report(ProgramState::Done, None);
                (run_state, Ok(()))
            }
            Err(e) => {
                if let Err(cleanup_err) = cancel_children(self.subagents.as_ref()).await {
                    self.host
                        .reporter
                        .report(ProgramState::Error, Some(&cleanup_err));
                    return (run_state, Err(cleanup_err));
                }
                self.host.reporter.report(ProgramState::Error, Some(&e));
                (run_state, Err(e))
            }
        }
    }

    async fn turn_inner_images(&mut self, prompt: &str, images: &[Image]) -> Result<(), String> {
        let expanded_prompt = expand_at_files(&self.session.cwd, prompt)?;
        let mut content = vec![UserContent::text(expanded_prompt)];
        content.extend(images.iter().cloned().map(UserContent::Image));
        let prompt_message = Message::User { content };
        // HOST-PREPARATION LIFETIME (deliberately not a bound engine worker).
        //
        // The canonical task is still `Queued` here: it is admitted only at the
        // first real model/tool dispatch inside `run_with_tools`, while the
        // project MCP servers and plugins loaded below are started *before* that
        // permit exists. These helper processes are host preparation: their
        // lifetime is exactly the returned `PolicyBoundTools`, they are dropped
        // (and their stdio children reaped) when this future returns or is
        // cancelled, and they are never counted as a `TaskWorkerGuard` because
        // the task has not been admitted yet. A preparation failure therefore
        // settles the still-`Queued` task as `Failed` (never a fake `Running`),
        // and cancellation settles it as `Cancelled`, with those real helper
        // processes released by the drop of this future.
        //
        // Engine-loaded plugin and MCP tools are bound to this host's policy
        // through the unforgeable container, so a restricted run can register
        // them while refusing arbitrary `extra_tools`.
        let policy_bound_tools = if self.no_mcp {
            None
        } else {
            Some(
                load_policy_bound_tools(&self.session.cwd, self.host.clone(), &self.tool_filter)
                    .await?,
            )
        };
        let history = run_with_tools_message(
            self.config.clone(),
            self.session.history.clone(),
            prompt_message,
            &self.session.cwd,
            self.host.clone(),
            Services {
                antigravity: self.antigravity.as_ref(),
                ..Services::default()
            },
            RunTools {
                options: self.options.clone(),
                read_only: false,
                subagents: self.subagents.clone(),
                tool_filter: self.tool_filter.clone(),
                extra_tools: Vec::new(),
                policy_bound_tools,
            },
        )
        .await?;
        if let Some(manager) = &self.subagents {
            let active: Vec<_> = manager
                .list()
                .into_iter()
                .filter(|run| run.status.is_active())
                .collect();
            if !active.is_empty() {
                for run in active {
                    manager.kill(run.run_id).await?;
                }
                return Err("Parent ended before collecting child tasks; unfinished children cancelled. File changes are not rolled back.".into());
            }
        }
        let mut candidate = self.session.clone();
        candidate.history = history;
        self.commit_session(candidate, self.path.clone())?;
        if self.host.json {
            let _ = emit_or_print(
                &self.host.output,
                serde_json::json!({
                    "type": "done",
                    "session": if self.no_session { serde_json::Value::Null } else { json!(self.path) },
                    "messages": self.session.history.len()
                }),
                true,
            );
        } else {
            println!();
            if !self.no_session {
                eprintln!("Session: {}", self.path.display());
            }
        }
        Ok(())
    }
}

/// 打开工作区级文件检查点存储。`--no-session` 与 journal 一样保持禁用（零磁盘写入）。
pub(super) fn open_checkpoints(
    no_session: bool,
    cwd: &Path,
) -> Result<Arc<CheckpointStore>, String> {
    if no_session {
        return Ok(Arc::new(CheckpointStore::disabled()));
    }
    Ok(Arc::new(CheckpointStore::open(cwd)?))
}

pub(super) fn open_journal(no_session: bool, cwd: &Path, session_id: &str) -> Arc<Journal> {
    if no_session {
        return Arc::new(Journal::disabled());
    }
    let journal = Arc::new(Journal::open(cwd, session_id));
    journal.mark_interrupted();
    journal
}

/// Open the workspace-level cross-restart unresolved-effect ledger.
///
/// `--no-session` keeps it fully disabled so an ephemeral run performs zero
/// extra disk writes and claims no durable memory.
pub(super) fn open_ledger(no_session: bool, cwd: &Path) -> Arc<UnresolvedEffectLedger> {
    if no_session {
        return Arc::new(UnresolvedEffectLedger::disabled());
    }
    Arc::new(UnresolvedEffectLedger::persistent(cwd))
}

/// `--output-schema PATH` 原始文件字节上限。
///
/// 手写 schema 文件常带缩进，原始字节可能明显大于紧凑序列化结果，因此这里的
/// 读盘上限高于引擎检查的 32 KiB **序列化**上限。两层都保留：这里只防止无界
/// 读盘，32 KiB 仍由引擎在任何模型请求之前判定。
pub(super) const MAX_OUTPUT_SCHEMA_BYTES: u64 = 256 * 1024;

/// 有界读取并解析 `--output-schema PATH`。
///
/// 相对路径按工作区根解析，与 `--export` / `--shared-limits-dir` 的既有约定一致。
/// 叶子符号链接、非普通文件、超限文件、非法 JSON 以及不能构成 JSON Schema
/// （既不是对象也不是布尔）的内容一律拒绝。每次失败都在模型请求之前作为明确
/// 错误返回，绝不静默降级成“没有 schema”。
pub(super) fn load_output_schema(
    cwd: &Path,
    path: &Path,
) -> Result<mio_coding_agent::OutputSchema, String> {
    use std::io::Read;
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let metadata = std::fs::symlink_metadata(&resolved).map_err(|error| {
        format!(
            "--output-schema cannot access {}: {error}",
            resolved.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "--output-schema {} must be a regular file, not a symlink or special file",
            resolved.display()
        ));
    }
    if metadata.len() > MAX_OUTPUT_SCHEMA_BYTES {
        return Err(format!(
            "--output-schema {} is {} bytes, above the {MAX_OUTPUT_SCHEMA_BYTES} byte read limit",
            resolved.display(),
            metadata.len()
        ));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A file can be swapped for a FIFO after symlink_metadata. O_NONBLOCK
        // keeps that race from blocking startup; fstat below checks the opened
        // object rather than trusting the earlier path lookup.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(&resolved).map_err(|error| {
        format!(
            "--output-schema cannot open {}: {error}",
            resolved.display()
        )
    })?;
    let opened = file.metadata().map_err(|error| {
        format!(
            "--output-schema cannot inspect {}: {error}",
            resolved.display()
        )
    })?;
    if !opened.is_file() {
        return Err(format!(
            "--output-schema {} must be a regular file, not a symlink or special file",
            resolved.display()
        ));
    }
    if opened.len() > MAX_OUTPUT_SCHEMA_BYTES {
        return Err(format!(
            "--output-schema {} is {} bytes, above the {MAX_OUTPUT_SCHEMA_BYTES} byte read limit",
            resolved.display(),
            opened.len()
        ));
    }
    let mut bytes = Vec::new();
    (&file)
        .take(MAX_OUTPUT_SCHEMA_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            format!(
                "--output-schema cannot read {}: {error}",
                resolved.display()
            )
        })?;
    if bytes.len() as u64 > MAX_OUTPUT_SCHEMA_BYTES {
        return Err(format!(
            "--output-schema {} is larger than the {MAX_OUTPUT_SCHEMA_BYTES} byte read limit",
            resolved.display()
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "--output-schema {} is not valid JSON: {error}",
            resolved.display()
        )
    })?;
    mio_coding_agent::OutputSchema::try_from(value).map_err(|error| {
        format!(
            "--output-schema {} is not a valid JSON Schema: {error}",
            resolved.display()
        )
    })
}

/// 以人类可读形式打印恢复证据中的操作列表。
pub(super) fn print_recovered_ops(label: &str, operations: &[journal::RecoveredOperation]) {
    for op in operations {
        let detail = op
            .detail
            .as_deref()
            .map(|detail| format!(", {detail}"))
            .unwrap_or_default();
        println!(
            "  {label}: {} {} (turn {}, run {}{detail})",
            op.operation, op.target, op.turn, op.run_id
        );
    }
}

/// 以人类可读形式打印恢复证据中的 canonical 工具调用列表。
///
/// 所有文本字段都经 `escape_debug`，所以目标或详情里的终端控制字符不会注入终端。
pub(super) fn print_recovered_calls(calls: &[journal::RecoveredToolCall]) {
    for call in calls {
        let redacted = if call.target_redacted {
            " (redacted target)"
        } else {
            ""
        };
        let tool_call_id = call
            .tool_call_id
            .as_deref()
            .map(|id| format!(", tool_call_id {}", id.escape_debug()))
            .unwrap_or_default();
        let detail = call
            .detail
            .as_deref()
            .map(|detail| format!(", {}", detail.escape_debug()))
            .unwrap_or_default();
        println!(
            "  {}: {} {} (turn {}, run {}{}{}{})",
            call.phase.as_str(),
            call.tool.escape_debug(),
            call.target.escape_debug(),
            call.turn,
            call.run_id.escape_debug(),
            redacted,
            tool_call_id,
            detail,
        );
    }
}

/// Wait for a turn's canonical task to reach a real terminal phase.
///
/// This is the ordering barrier for the terminal report: no `done` or `idle`
/// state may be reported until this returns `Ok`. `wait_settled` also returns
/// the last committed record after a latched persistence failure, so this never
/// hangs; a non-terminal record is an error instead of a false settlement.
pub(super) async fn wait_for_settlement(
    handle: &mio_coding_agent::task_lifecycle::TaskHandle,
) -> Result<(), String> {
    match handle.wait_settled().await {
        Some(record) if record.phase.is_terminal() => Ok(()),
        other => {
            let phase = other
                .map(|record| record.phase.as_str().to_owned())
                .unwrap_or_else(|| "untracked".to_owned());
            Err(format!(
                "canonical task did not settle (phase {phase}); refusing to report a false completion"
            ))
        }
    }
}

pub(super) async fn cancel_children(manager: Option<&Arc<SubagentManager>>) -> Result<(), String> {
    if let Some(manager) = manager {
        for run in manager
            .list()
            .into_iter()
            .filter(|run| run.status.is_active())
        {
            manager.kill(run.run_id).await?;
        }
    }
    Ok(())
}

/// Rewind through the same workspace leases and approval path as writes.
pub(super) async fn rewind_file(
    cwd: &Path,
    checkpoints: Arc<CheckpointStore>,
    host: Arc<dyn Host>,
    filter: ToolFilter,
    shared: Option<mio_coding_agent::shared_limits::SharedLimits>,
    subagents: Option<Arc<SubagentManager>>,
    selector: &str,
) -> Result<mio_coding_agent::checkpoint::RewindOutcome, String> {
    let mut workspace = mio_coding_agent::Workspace::new(cwd, host)?
        .with_tool_filter(filter)
        .with_checkpoints(checkpoints);
    if let Some(shared) = shared {
        workspace = workspace.with_shared_limits(shared);
    }
    if let Some(subagents) = subagents {
        workspace = workspace.with_subagents(subagents)?;
    }
    workspace.rewind_checkpoint(selector).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_view_reports_only_matching_persisted_history() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let mut session = Session::new(&cwd).unwrap();
        let path = cwd.join("session.json");
        let id = session.id.to_string();
        let missing = recovery_view(&id, &cwd, Some(&path), "data");
        assert_eq!(missing["sessionSaved"], false);
        assert!(missing["savedMessages"].is_null());
        session.save(&path).unwrap();
        let saved = recovery_view(&id, &cwd, Some(&path), "recovery");
        assert_eq!(saved["sessionSaved"], true);
        assert_eq!(saved["savedMessages"], 0);
        assert_eq!(saved["automaticReplay"], false);
        let mismatch = recovery_view("different-session", &cwd, Some(&path), "data");
        assert_eq!(mismatch["sessionSaved"], false);
        assert!(
            mismatch["sessionError"]
                .as_str()
                .unwrap()
                .contains("mismatch")
        );
        let disabled = recovery_view(&id, &cwd, None, "recovery");
        assert!(disabled["sessionFile"].is_null());
        assert!(disabled["savedMessages"].is_null());
        std::fs::write(&path, "corrupt").unwrap();
        let corrupt = recovery_view(&id, &cwd, Some(&path), "data");
        assert_eq!(corrupt["sessionSaved"], false);
        assert!(corrupt["sessionError"].is_string());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "corrupt");
    }

    #[test]
    fn latest_session_uses_save_order_even_when_session_seconds_match() {
        let dir = tempfile::tempdir().unwrap();
        let mut original = Session::new(dir.path()).unwrap();
        let mut fork = original.fork();
        let a = dir.path().join("a.json");
        let b = dir.path().join("b.json");
        original.save(&a).unwrap();
        fork.save(&b).unwrap();
        for (path, nanos) in [(&a, 1), (&b, 2)] {
            let times = std::fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_nanos(nanos));
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(times)
                .unwrap();
        }
        let list = session_list(dir.path()).unwrap();
        assert_eq!(list[0].id, fork.id);
    }

    fn test_host(
        root: &Path,
        session_id: &str,
        no_session: bool,
        trust_project_mcp: bool,
    ) -> Arc<CliHost> {
        Arc::new(CliHost {
            json: true,
            output: None,
            no_shell: false,
            execution_policy: mio_coding_agent::execution_backend::ExecutionPolicy::Unrestricted,
            approve: false,
            trust_project_mcp,
            approved_project_mcp: Arc::new(Mutex::new(BTreeSet::new())),
            reporter: Arc::new(StatusReporter::new(false)),
            steering_queue: Mutex::new(VecDeque::new()),
            rpc_approvals: None,
            journal: open_journal(no_session, root, session_id),
            ledger: open_ledger(no_session, root),
            workspace: root.to_path_buf(),
            unknown_fence: Mutex::new(mio_coding_agent::unknown_effect::UnknownEffectFence::new()),
            turn_claims: Mutex::new(Vec::new()),
            dispatched_calls: Mutex::new(BTreeMap::new()),
            run_states: Mutex::new(BTreeMap::new()),
            run_tracking_saturated: AtomicBool::new(false),
            output_schema: None,
        })
    }

    /// Commit a complete empty coverage epoch so write dispatches in a unit
    /// test exercise the real admission path instead of the pre-epoch refusal.
    fn seed_coverage(host: &CliHost) {
        host.ledger.commit_initial_coverage(&[], true).unwrap();
    }

    #[test]
    fn failed_output_transport_refuses_canonical_admission_for_legacy_and_v1_clients() {
        use mio_coding_agent::tool_lifecycle::ToolCallPhase;
        for envelope in [Envelope::Legacy, Envelope::V1] {
            let dir = tempfile::tempdir().unwrap();
            let mut host = test_host(dir.path(), "output-failure", false, false);
            host.journal.begin_run().unwrap();
            let output = JsonOutput::new();
            let mut protocol = output.protocol_state();
            protocol.initialized = envelope == Envelope::V1;
            protocol.envelope = envelope;
            output.set_protocol(protocol);
            // Oversized events fail before enqueueing; no payload reaches stdout.
            assert!(
                output
                    .emit(json!({"payload": "x".repeat(2 * 1024 * 1024)}))
                    .is_err()
            );
            assert!(output.is_failed());
            Arc::get_mut(&mut host).unwrap().output = Some(output);
            let tracker = mio_coding_agent::tool_lifecycle::ToolCallTracker::new(
                "real-output-failure-scope",
                host.clone(),
            );
            let result = tracker.begin("write", "write", "a.txt", Some("call-1"), Some(1), Some(1));
            assert!(
                result.is_err(),
                "{envelope:?}: failed transport must refuse admission"
            );
            assert!(tracker.is_fail_closed());
            assert!(tracker.calls().is_empty(), "no admitted or dispatched call");
            host.journal.finish_run(RunState::Failed).unwrap();
            let report = host.journal.recovery_report().unwrap();
            assert_eq!(report.calls.len(), 1);
            assert_eq!(report.calls[0].phase, ToolCallPhase::NotDispatched);
            assert!(
                report.unknown_operations.is_empty(),
                "transport refusal is not an unknown tool effect"
            );
        }
    }

    /// A digest that must never reach the journal: only real correlation ids
    /// are evidence, never the binding's parameter digest.
    const BINDING_DIGEST_MARKER: &str = "binding-digest-must-not-enter-the-journal";

    fn bound_write(scope: &str, tool_call_id: &str) -> mio_coding_agent::policy::ApprovalBinding {
        mio_coding_agent::policy::ApprovalBinding {
            entry: mio_coding_agent::policy::EntryKind::Builtin,
            tool: "write".into(),
            operation: "write".into(),
            cwd: "/work".into(),
            scope: scope.into(),
            params_sha256: BINDING_DIGEST_MARKER.into(),
            call_id: uuid::Uuid::new_v4().to_string(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    fn canonical_call(
        scope: &str,
        call_index: u64,
        phase: mio_coding_agent::tool_lifecycle::ToolCallPhase,
        tool_call_id: Option<&str>,
        call_id: Option<&str>,
    ) -> mio_coding_agent::tool_lifecycle::ToolCallRecord {
        mio_coding_agent::tool_lifecycle::ToolCallRecord {
            call_index,
            scope: scope.into(),
            phase,
            tool: "write".into(),
            operation: "write".into(),
            params_sha256: "0".repeat(64),
            target: "a.txt".into(),
            target_redacted: false,
            tool_call_id: tool_call_id.map(str::to_owned),
            call_id: call_id.map(str::to_owned),
            turn: None,
            effect_id: None,
            detail: None,
            output_references: Vec::new(),
            references_truncated: false,
            started: 0,
            updated: 0,
        }
    }

    /// Emit a real run lifecycle observation for `scope` so the host tracks it.
    fn note_run(host: &CliHost, scope: &str, live: bool) {
        use mio_coding_agent::{RunObservation, RunObservationKind};
        host.on_run_observation(&RunObservation {
            run_id: uuid::Uuid::parse_str(scope).unwrap(),
            kind: if live {
                RunObservationKind::RunStarted
            } else {
                RunObservationKind::RunFinished
            },
            session_id: None,
            subagent_id: None,
            parent_run_id: None,
            turn_id: None,
            tool_call_id: None,
            effect_id: None,
            status: None,
        });
    }

    #[tokio::test]
    async fn journal_records_executed_and_unknown_operations() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path(), "s1", false, false);
        host.journal.begin_run().unwrap();
        host.approve_tool_async("write", "a.txt").await.unwrap();
        host.on_event(AgentEvent::ToolFinished {
            operation: "write".into(),
            target: "a.txt".into(),
            result: Ok("ok".into()),
        });
        // 取消时尚未返回结果的操作必须保留为 unknown。
        host.approve_tool_async("bash", "sleep 99").await.unwrap();
        host.journal.finish_run(RunState::Cancelled).unwrap();

        let reloaded = Journal::open(dir.path(), "s1");
        let report = reloaded.recovery_report().unwrap();
        assert_eq!(report.executed_operations, 1);
        assert_eq!(report.unknown_operations.len(), 1);
        assert_eq!(report.unknown_operations[0].operation, "bash");
        assert!(report.has_unfinished());
    }

    /// A real transport unknown is journaled `Unknown` (never `Failed`), and the
    /// CLI host's confirmation still refuses when the shown marker cannot be
    /// bound to a persisted durable record (here the live unknown never reached
    /// the ledger because the fixture skipped the dispatch claim).
    #[tokio::test]
    async fn a_real_unknown_is_journaled_unknown_and_json_confirmation_refuses() {
        use mio_coding_agent::tool_lifecycle::ToolCallPhase;
        use mio_coding_agent::unknown_effect::{UnknownReason, UnknownSideEffect};
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path(), "unknown-guard", false, false);
        host.journal.begin_run().unwrap();
        let run_id = uuid::Uuid::new_v4();
        // A dispatched canonical call whose transport died: it is `Unknown`, and
        // never `Failed` or `Executed`.
        host.journal
            .record_canonical_phase(&canonical_call(
                &run_id.to_string(),
                1,
                ToolCallPhase::Dispatched,
                Some("call-unknown"),
                None,
            ))
            .unwrap();
        let effect = UnknownSideEffect::new(
            "mcp__srv__tool",
            "{}",
            UnknownReason::McpTransport,
            Some("call-unknown".into()),
        );
        let recorded = mio_coding_agent::tool_identity::scope(
            Some(mio_coding_agent::tool_identity::ToolIdentity::new(
                run_id,
                "call-unknown",
                1,
                5,
            )),
            {
                let host = host.clone();
                let effect = effect.clone();
                async move { host.on_unknown_side_effect(&effect) }
            },
        )
        .await;
        assert!(recorded.is_ok(), "recording a real unknown must succeed");

        let report = host.journal.recovery_report().unwrap();
        assert_eq!(report.calls.len(), 1, "{report:?}");
        assert_eq!(report.calls[0].phase, ToolCallPhase::Unknown, "{report:?}");
        assert!(
            report.failed_operations.is_empty(),
            "a transport death is unknown, never failed: {report:?}"
        );
        assert!(report.has_unfinished(), "{report:?}");

        // Non-interactive JSON mode has no way to answer: refuse.
        let call = mio_coding_agent::policy::NormalizedCall {
            entry: mio_coding_agent::policy::EntryKind::Mcp,
            tool: "mcp__srv__tool".into(),
            operation: "mcp__srv__tool".into(),
            display_target: "{}".into(),
            side_effect: mio_coding_agent::policy::SideEffect::External,
            requires_approval: true,
            aliases: vec!["mcp__srv__tool".into()],
            params_sha256: "0".repeat(64),
        };
        let refused = host
            .confirm_unknown_side_effect(&call, std::slice::from_ref(&effect))
            .await
            .unwrap_err();
        assert!(refused.contains("unknown"), "{refused}");
    }

    #[tokio::test]
    async fn journal_write_failure_blocks_side_effects() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".mio")).unwrap();
        std::fs::write(dir.path().join(".mio/journal"), "not a directory").unwrap();
        let host = test_host(dir.path(), "s2", false, false);
        let _ = host.journal.begin_run();
        let err = host.approve_tool_async("write", "a.txt").await.unwrap_err();
        assert!(
            err.contains("Execution journal unavailable"),
            "unexpected error: {err}"
        );
        // 证据持久化失败必须被报告，而不是静默通过。
        assert!(
            host.journal
                .recovery_report()
                .unwrap()
                .journal_error
                .is_some()
        );
    }

    #[tokio::test]
    async fn canonical_binding_keeps_real_identity_and_never_the_digest() {
        use mio_coding_agent::tool_lifecycle::ToolCallPhase;
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path(), "bound", false, false);
        host.journal.begin_run().unwrap();
        let run_id = uuid::Uuid::new_v4();
        let binding = bound_write(&run_id.to_string(), "call-bound");
        let mut record = canonical_call(
            &binding.scope,
            1,
            ToolCallPhase::Admitted,
            binding.tool_call_id.as_deref(),
            Some(binding.call_id.as_str()),
        );
        host.journal.record_canonical_phase(&record).unwrap();

        // The canonical record carries the real scope / tool-call id / approval
        // attempt id before any side effect, and never the binding digest.
        let path = journal::journal_path(dir.path(), "bound");
        let stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let calls = stored["runs"][0]["calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1, "{stored}");
        assert_eq!(calls[0]["phase"], "admitted", "{stored}");
        assert_eq!(calls[0]["scope"], binding.scope, "{stored}");
        assert_eq!(calls[0]["tool_call_id"], "call-bound", "{stored}");
        assert_eq!(calls[0]["call_id"], binding.call_id, "{stored}");
        assert!(
            !stored.to_string().contains(BINDING_DIGEST_MARKER),
            "a journal record must not carry binding digests or raw parameters"
        );

        // A later dispatch updates only the phase and the real dispatch ids.
        record.phase = ToolCallPhase::Dispatched;
        record.turn = Some(3);
        record.effect_id = Some(7);
        host.journal.record_canonical_phase(&record).unwrap();
        let stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let call = &stored["runs"][0]["calls"][0];
        assert_eq!(call["phase"], "dispatched", "{stored}");
        assert_eq!(call["turn"], 3, "{stored}");
        assert_eq!(call["effect_id"], 7, "{stored}");
    }

    #[tokio::test]
    async fn canonical_phase_write_is_refused_when_the_journal_cannot_persist() {
        use mio_coding_agent::tool_lifecycle::ToolCallPhase;
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path(), "bound-failure", false, false);
        host.journal.begin_run().unwrap();
        // 让 journal 文件路径变成目录：落盘必然失败。
        let path = journal::journal_path(dir.path(), "bound-failure");
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let err = host
            .journal
            .record_canonical_phase(&canonical_call(
                "some-scope",
                1,
                ToolCallPhase::Normalized,
                Some("call-x"),
                None,
            ))
            .unwrap_err();
        assert!(!err.is_empty(), "a refused write must report a reason");
        assert!(path.is_dir(), "a refused write must write nothing");
    }

    #[tokio::test]
    async fn no_session_bound_approval_writes_no_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path(), "no-session-bound", true, false);
        assert!(host.journal.begin_run().is_ok());
        let binding = bound_write("some-scope", "call-y");
        assert!(
            host.approve_tool_binding("write", "a.txt", &binding)
                .await
                .is_ok()
        );
        assert!(
            host.journal
                .record_result("write", "a.txt", &Ok("ok".into()))
                .is_ok()
        );
        assert!(host.journal.finish_run(RunState::Completed).is_ok());
        assert!(!dir.path().join(".mio").exists());
    }

    #[test]
    fn no_session_checkpoints_are_disabled_and_opening_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let disabled = open_checkpoints(true, dir.path()).unwrap();
        assert!(!disabled.is_enabled());
        assert!(
            disabled
                .record_pre_edit("write", "a.txt", None, b"x")
                .unwrap()
                .is_none()
        );
        let enabled = open_checkpoints(false, dir.path()).unwrap();
        assert!(enabled.is_enabled());
        assert!(enabled.list().unwrap().is_empty());
        assert!(
            !dir.path().join(".mio").exists(),
            "opening a checkpoint store must not create files"
        );
    }

    #[test]
    fn no_session_journal_is_disabled_and_creates_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let journal = open_journal(true, dir.path(), "s3");
        assert!(!journal.is_enabled());
        assert!(journal.path().is_none());
        assert!(journal.begin_run().is_ok());
        assert!(journal.record_intent("write", "a.txt").is_ok());
        assert!(journal.finish_run(RunState::Completed).is_ok());
        assert!(journal.recovery_report().is_none());
        assert!(!dir.path().join(".mio").exists());
    }

    #[tokio::test]
    async fn project_mcp_start_requires_explicit_trust() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path(), "s4", false, false);
        let err = host
            .approve_tool_async(PROJECT_MCP_APPROVAL_OP, "server=proj; command=sh")
            .await
            .unwrap_err();
        assert!(err.contains("requires authorization"), "unexpected: {err}");

        let trusted = test_host(dir.path(), "s5", false, true);
        assert!(
            trusted
                .approve_tool_async(PROJECT_MCP_APPROVAL_OP, "server=proj; command=sh")
                .await
                .is_ok()
        );
    }

    /// The order the Ctrl-C branch depends on: waiting for a cancelled turn's
    /// real worker must not resolve (and therefore no idle may be reported)
    /// before the worker guard is actually reclaimed.
    #[tokio::test]
    async fn cancelled_settlement_waits_for_the_real_worker_before_idle() {
        use mio_coding_agent::task_lifecycle::{
            ActiveLease, FinishOutcome, NullTaskSink, Settlement, TaskId, TaskLifecycleTracker,
            TaskPhase,
        };
        let tracker = TaskLifecycleTracker::new("cancel-order", Arc::new(NullTaskSink));
        let task_id = TaskId::new("cancel-order-task").unwrap();
        let handle =
            mio_coding_agent::task_lifecycle::TaskHandle::new(tracker.clone(), task_id.clone());
        handle.register(None).unwrap();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let permit =
            ActiveLease::from_semaphore_permit(semaphore.clone().try_acquire_owned().unwrap());
        tracker.admit(&task_id, permit.as_ref()).unwrap();
        // A real worker (the same guard shell reapers and blocking readers use)
        // stays unreclaimed while the settlement wait runs.
        let guard = handle.begin_worker().unwrap();
        assert_eq!(handle.snapshot().unwrap().workers, 1);

        let waiter_handle = handle.clone();
        let waiter = tokio::spawn(async move { wait_for_settlement(&waiter_handle).await });
        handle
            .request_cancel(Some("user cancelled the turn"))
            .unwrap();
        assert!(matches!(
            handle.finish(Settlement::Cancelled, None).unwrap(),
            FinishOutcome::Deferred { workers: 1, .. }
        ));

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            !waiter.is_finished(),
            "settlement must not resolve before the real worker is reclaimed"
        );

        guard.release();
        let result = waiter.await.unwrap();
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(handle.snapshot().unwrap().phase, TaskPhase::Cancelled);
    }

    /// A latched persistence failure can never commit a terminal phase. The
    /// settlement wait must report that honestly so the caller reports an error
    /// instead of a false idle.
    #[tokio::test]
    async fn a_latched_persistence_failure_never_reports_a_settlement() {
        use mio_coding_agent::task_lifecycle::{
            ActiveLease, Settlement, TaskId, TaskLifecycleSink, TaskLifecycleTracker,
        };
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
        let fail = Arc::new(AtomicBool::new(false));
        let fail_in_sink = fail.clone();
        let sink: Arc<dyn TaskLifecycleSink> = Arc::new(
            move |_record: &mio_coding_agent::task_lifecycle::TaskRecord| {
                if fail_in_sink.load(AtomicOrdering::SeqCst) {
                    Err("disk full".to_string())
                } else {
                    Ok(())
                }
            },
        );
        let tracker = TaskLifecycleTracker::new("fail-closed-order", sink);
        let task_id = TaskId::new("fail-closed-order-task").unwrap();
        let handle =
            mio_coding_agent::task_lifecycle::TaskHandle::new(tracker.clone(), task_id.clone());
        handle.register(None).unwrap();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let permit =
            ActiveLease::from_semaphore_permit(semaphore.clone().try_acquire_owned().unwrap());
        tracker.admit(&task_id, permit.as_ref()).unwrap();

        // The sink latches on the terminal write, so the deferred terminal can
        // never commit.
        fail.store(true, AtomicOrdering::SeqCst);
        let _ = handle.finish(Settlement::Cancelled, None);
        let result = wait_for_settlement(&handle).await;
        assert!(
            result.is_err(),
            "a latched persistence failure must never look settled: {result:?}"
        );
    }

    // --- DES-09 ledger wiring ---

    #[test]
    fn recovered_reason_is_honest_and_never_a_pretended_transport() {
        assert_eq!(
            recovered_fence_reason(None),
            UnknownReason::InterruptedDispatch
        );
        assert_eq!(
            recovered_fence_reason(Some(UnknownFactReason::ResultUnknown)),
            UnknownReason::InterruptedDispatch
        );
        assert_eq!(
            recovered_fence_reason(Some(UnknownFactReason::InterruptedDispatch)),
            UnknownReason::InterruptedDispatch
        );
        assert_eq!(
            recovered_fence_reason(Some(UnknownFactReason::TransportLost)),
            UnknownReason::McpTransport
        );
    }

    #[tokio::test]
    async fn ledger_claims_dispatch_resolves_and_records_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path(), "ledger-lifecycle", false, false);
        seed_coverage(&host);
        host.journal.begin_run().unwrap();
        let scope = uuid::Uuid::new_v4().to_string();
        let key = UnresolvedEffectKey::new(scope.clone(), 1).unwrap();

        let dispatched = canonical_call(&scope, 1, ToolCallPhase::Dispatched, Some("call-1"), None);
        host.on_tool_call_lifecycle(&dispatched).unwrap();
        assert_eq!(
            host.ledger.lookup(&key).unwrap().unwrap().record.state,
            UnresolvedEffectState::Outstanding
        );
        // Same real key dispatched twice must be refused, never replayed.
        assert!(host.on_tool_call_lifecycle(&dispatched).is_err());

        host.on_tool_call_lifecycle(&canonical_call(
            &scope,
            1,
            ToolCallPhase::Executed,
            Some("call-1"),
            None,
        ))
        .unwrap();
        assert_eq!(
            host.ledger.lookup(&key).unwrap().unwrap().record.state,
            UnresolvedEffectState::Resolved
        );

        let second = UnresolvedEffectKey::new(scope.clone(), 2).unwrap();
        host.on_tool_call_lifecycle(&canonical_call(
            &scope,
            2,
            ToolCallPhase::Dispatched,
            Some("call-2"),
            None,
        ))
        .unwrap();
        // A typed unknown must never be downgraded to Executed.
        host.on_tool_call_lifecycle(&canonical_call(
            &scope,
            2,
            ToolCallPhase::Unknown,
            Some("call-2"),
            None,
        ))
        .unwrap();
        let unknown = host.ledger.lookup(&second).unwrap().unwrap();
        assert_eq!(unknown.record.state, UnresolvedEffectState::Unknown);
        host.on_tool_call_lifecycle(&canonical_call(
            &scope,
            2,
            ToolCallPhase::Executed,
            Some("call-2"),
            None,
        ))
        .unwrap();
        assert_eq!(
            host.ledger.lookup(&second).unwrap().unwrap().record.state,
            UnresolvedEffectState::Unknown,
            "an unknown record must not regress to resolved"
        );
    }

    #[tokio::test]
    async fn recovered_unknowns_seed_the_guard_and_ack_the_exact_digest() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path(), "ledger-seed", false, false);
        let key = UnresolvedEffectKey::new(uuid::Uuid::new_v4().to_string(), 1).unwrap();
        host.ledger
            .record_unknown(
                &key,
                UnknownEffectInput::new("mcp__srv__write", UnknownFactReason::TransportLost),
            )
            .unwrap();
        let stored = host.ledger.lookup(&key).unwrap().unwrap();

        host.recover_and_seed_guard();
        let fence = host.unknown_effect_fence().expect("host guard");
        assert_eq!(fence.len(), 1);
        let mut seeded_all = fence.snapshot();
        let seeded = seeded_all.pop().unwrap();
        assert_eq!(seeded.ledger_key(), Some((key.scope.as_str(), 1)));
        assert_eq!(seeded.ledger_snapshot(), Some(stored.snapshot_key.as_str()));
        assert_eq!(seeded.reason, UnknownReason::McpTransport);
        assert!(
            seeded.evidence_id.is_none(),
            "recovered evidence must not invent an id"
        );

        // Re-seeding the same real ledger key is idempotent.
        host.recover_and_seed_guard();
        assert_eq!(host.unknown_effect_fence().unwrap().len(), 1);

        let claim = unknown_claim(&seeded).unwrap();
        persist_unknown_claims(&host.ledger, std::slice::from_ref(&claim), "test").unwrap();
        assert_eq!(
            host.ledger.lookup(&key).unwrap().unwrap().record.state,
            UnresolvedEffectState::Acknowledged
        );

        // New evidence appended after the shown digest makes it stale: the
        // stale confirmation must be refused rather than acknowledge it all.
        let key2 = UnresolvedEffectKey::new(uuid::Uuid::new_v4().to_string(), 1).unwrap();
        host.ledger
            .record_unknown(
                &key2,
                UnknownEffectInput::new("write", UnknownFactReason::ResultUnknown),
            )
            .unwrap();
        let shown = host.ledger.lookup(&key2).unwrap().unwrap();
        host.ledger
            .record_unknown(
                &key2,
                UnknownEffectInput::new("write", UnknownFactReason::ResultUnknown)
                    .with_evidence_id(uuid::Uuid::new_v4()),
            )
            .unwrap();
        let stale = persist_unknown_claims(
            &host.ledger,
            &[UnknownClaim {
                key: key2.clone(),
                snapshot: shown.snapshot_key,
            }],
            "test",
        );
        assert!(stale.is_err(), "stale digest must be refused: {stale:?}");
        assert!(
            stale.unwrap_err().contains("stale"),
            "the refusal must name the stale digest"
        );
        assert_eq!(
            host.ledger.lookup(&key2).unwrap().unwrap().record.state,
            UnresolvedEffectState::Unknown,
            "a refused stale confirmation must leave the record pending"
        );
    }

    /// The policy view of a new write that must ask for unknown-effect
    /// confirmation.
    fn runtime_unknown_call(
        operation: &str,
        target: &str,
    ) -> mio_coding_agent::policy::NormalizedCall {
        mio_coding_agent::policy::NormalizedCall {
            entry: mio_coding_agent::policy::EntryKind::Mcp,
            tool: operation.into(),
            operation: operation.into(),
            display_target: target.into(),
            side_effect: mio_coding_agent::policy::SideEffect::External,
            requires_approval: true,
            aliases: vec![operation.into()],
            params_sha256: "0".repeat(64),
        }
    }

    /// Wait for exactly one real pending broker request and return its id.
    async fn wait_for_broker_request(broker: &Arc<RpcApprovals>) -> String {
        for _ in 0..20_000 {
            if let Some(id) = broker.pending_request_ids().into_iter().next() {
                return id;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        panic!("the broker emitted no approval request");
    }

    /// Persist a live runtime `Unknown` exactly the way an MCP/plugin transport
    /// or supervision failure does: claim the canonical dispatch first, then
    /// publish the typed marker under its real tool identity and trip the run
    /// fence with the same marker (including the same real `evidence_id`).
    async fn persist_live_runtime_unknown(
        host: &Arc<CliHost>,
        run_id: uuid::Uuid,
        tool_call_id: &str,
        call_index: u64,
        operation: &str,
    ) -> UnknownSideEffect {
        let scope = run_id.to_string();
        host.on_tool_call_lifecycle(&canonical_call(
            &scope,
            call_index,
            ToolCallPhase::Dispatched,
            Some(tool_call_id),
            None,
        ))
        .expect("the live dispatch must claim its durable intent");
        let effect = UnknownSideEffect::new(
            operation,
            "{}",
            UnknownReason::McpTransport,
            Some(tool_call_id.into()),
        );
        mio_coding_agent::tool_identity::scope(
            Some(mio_coding_agent::tool_identity::ToolIdentity::new(
                run_id,
                tool_call_id,
                call_index as usize,
                5,
            )),
            {
                let host = host.clone();
                let effect = effect.clone();
                async move { host.on_unknown_side_effect(&effect) }
            },
        )
        .await
        .expect("the live unknown must persist");
        host.unknown_effect_fence()
            .expect("host guard")
            .mark(effect.clone());
        effect
    }

    /// Real same-process loopback: a live runtime unknown is denied, a separate
    /// broker approval acknowledges exactly the persisted evidence, and a new
    /// id then dispatches exactly once. No restart is involved.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_runtime_unknown_is_denied_then_broker_approval_admits_one_new_id() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut host = test_host(&root, "live-confirm", false, false);
        host.journal.begin_run().unwrap();
        seed_coverage(&host);
        let broker = Arc::new(RpcApprovals::new(std::time::Duration::from_secs(30)));
        Arc::get_mut(&mut host).unwrap().rpc_approvals = Some(broker.clone());

        let run_id = uuid::Uuid::new_v4();
        let scope = run_id.to_string();
        let effect =
            persist_live_runtime_unknown(&host, run_id, "call-1", 1, "mcp__srv__record").await;
        let evidence_id = effect
            .evidence_id
            .expect("a live marker carries a real evidence id");

        // The effect really produced a partial side effect before its outcome
        // became unknown; the sentinel on disk is that real result.
        std::fs::write(root.join("sentinel.txt"), "written-before-unknown").unwrap();

        let key1 = UnresolvedEffectKey::new(scope.clone(), 1).unwrap();
        let shown = host.ledger.lookup(&key1).unwrap().unwrap();
        assert_eq!(shown.record.state, UnresolvedEffectState::Unknown);
        assert_eq!(shown.record.facts.len(), 1);
        assert_eq!(shown.record.facts[0].evidence_id, Some(evidence_id));

        // A new id cannot dispatch while the live unknown is unconfirmed.
        let denied = host
            .on_tool_call_lifecycle(&canonical_call(
                &scope,
                2,
                ToolCallPhase::Dispatched,
                Some("call-2"),
                None,
            ))
            .unwrap_err();
        assert!(denied.contains("unknown"), "{denied}");
        assert!(!root.join("guarded.txt").exists());

        // A separate host confirmation round-trips through the real broker.
        let call = runtime_unknown_call("workspace", "write guarded.txt");
        let confirm = tokio::spawn({
            let host = host.clone();
            let effect = effect.clone();
            async move {
                host.confirm_unknown_side_effect(&call, std::slice::from_ref(&effect))
                    .await
            }
        });
        let request_id = wait_for_broker_request(&broker).await;
        assert!(!request_id.is_empty());
        broker.resolve(&request_id, true).unwrap();
        confirm.await.unwrap().unwrap();

        let acked = host.ledger.lookup(&key1).unwrap().unwrap();
        assert_eq!(acked.record.state, UnresolvedEffectState::Acknowledged);
        assert_eq!(
            acked.snapshot_key, shown.snapshot_key,
            "the acknowledgement binds the exact snapshot the host was shown"
        );

        // The new id now dispatches exactly once, without any restart.
        std::fs::write(root.join("guarded.txt"), "executed-once").unwrap();
        host.on_tool_call_lifecycle(&canonical_call(
            &scope,
            2,
            ToolCallPhase::Dispatched,
            Some("call-2"),
            None,
        ))
        .expect("the acknowledged unknown must release the new id");
        let replay = host
            .on_tool_call_lifecycle(&canonical_call(
                &scope,
                2,
                ToolCallPhase::Dispatched,
                Some("call-2"),
                None,
            ))
            .unwrap_err();
        assert!(replay.contains("already records"), "{replay}");
        host.on_tool_call_lifecycle(&canonical_call(
            &scope,
            2,
            ToolCallPhase::Executed,
            Some("call-2"),
            None,
        ))
        .unwrap();
    }

    /// New evidence for the same durable key while the confirmation prompt is
    /// still waiting makes the approval stale: the ack is refused and the
    /// record stays `Unknown`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn new_evidence_while_the_prompt_waits_invalidates_the_approval() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut host = test_host(&root, "live-stale", false, false);
        host.journal.begin_run().unwrap();
        seed_coverage(&host);
        let broker = Arc::new(RpcApprovals::new(std::time::Duration::from_secs(30)));
        Arc::get_mut(&mut host).unwrap().rpc_approvals = Some(broker.clone());

        let run_id = uuid::Uuid::new_v4();
        let scope = run_id.to_string();
        let effect =
            persist_live_runtime_unknown(&host, run_id, "call-1", 1, "mcp__srv__record").await;
        let key1 = UnresolvedEffectKey::new(scope.clone(), 1).unwrap();
        let shown = host.ledger.lookup(&key1).unwrap().unwrap();

        let call = runtime_unknown_call("workspace", "write guarded.txt");
        let confirm = tokio::spawn({
            let host = host.clone();
            let effect = effect.clone();
            async move {
                host.confirm_unknown_side_effect(&call, std::slice::from_ref(&effect))
                    .await
            }
        });
        let request_id = wait_for_broker_request(&broker).await;
        // The prompt is already waiting with the shown snapshot; new evidence
        // arrives before the host answers.
        host.ledger
            .record_unknown(
                &key1,
                UnknownEffectInput::new("mcp__srv__record", UnknownFactReason::TransportLost)
                    .with_evidence_id(uuid::Uuid::new_v4()),
            )
            .unwrap();
        broker.resolve(&request_id, true).unwrap();

        let refused = confirm.await.unwrap().unwrap_err();
        assert!(refused.contains("stale"), "{refused}");
        let current = host.ledger.lookup(&key1).unwrap().unwrap();
        assert_eq!(current.record.state, UnresolvedEffectState::Unknown);
        assert_ne!(current.snapshot_key, shown.snapshot_key);
    }

    /// Missing, ambiguous or incomplete evidence can never be confirmed, and is
    /// refused before the host is asked (no broker request is emitted).
    #[tokio::test]
    async fn unconfirmable_runtime_evidence_fails_closed_without_prompting() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut host = test_host(&root, "live-failclosed", false, false);
        host.journal.begin_run().unwrap();
        seed_coverage(&host);
        let broker = Arc::new(RpcApprovals::new(std::time::Duration::from_secs(30)));
        Arc::get_mut(&mut host).unwrap().rpc_approvals = Some(broker.clone());
        let call = runtime_unknown_call("workspace", "write guarded.txt");

        // Missing: a live marker whose evidence the ledger never persisted.
        let missing = UnknownSideEffect::new(
            "mcp__srv__tool",
            "{}",
            UnknownReason::McpTransport,
            Some("call-x".into()),
        );
        let error = host
            .confirm_unknown_side_effect(&call, std::slice::from_ref(&missing))
            .await
            .unwrap_err();
        assert!(error.contains("no durable"), "{error}");
        assert_eq!(
            broker.pending_count(),
            0,
            "an unconfirmable claim must never prompt"
        );

        // Ambiguous: the same real evidence id persisted on two records.
        let run_id = uuid::Uuid::new_v4();
        let scope = run_id.to_string();
        let effect =
            persist_live_runtime_unknown(&host, run_id, "call-1", 1, "mcp__srv__record").await;
        let evidence_id = effect.evidence_id.unwrap();
        let other_key = UnresolvedEffectKey::new(scope.clone(), 2).unwrap();
        host.ledger
            .record_unknown(
                &other_key,
                UnknownEffectInput::new("mcp__srv__record", UnknownFactReason::TransportLost)
                    .with_evidence_id(evidence_id),
            )
            .unwrap();
        let error = host
            .confirm_unknown_side_effect(&call, std::slice::from_ref(&effect))
            .await
            .unwrap_err();
        assert!(error.contains("ambiguous"), "{error}");
        assert_eq!(broker.pending_count(), 0);

        // Incomplete: a record whose facts were dropped can never be confirmed.
        let incomplete_dir = tempfile::tempdir().unwrap();
        let incomplete_root = incomplete_dir.path().canonicalize().unwrap();
        let mut incomplete_host = test_host(&incomplete_root, "live-incomplete", false, false);
        incomplete_host.journal.begin_run().unwrap();
        seed_coverage(&incomplete_host);
        let incomplete_broker = Arc::new(RpcApprovals::new(std::time::Duration::from_secs(30)));
        Arc::get_mut(&mut incomplete_host).unwrap().rpc_approvals = Some(incomplete_broker.clone());
        let run_id2 = uuid::Uuid::new_v4();
        let scope2 = run_id2.to_string();
        let effect2 = persist_live_runtime_unknown(
            &incomplete_host,
            run_id2,
            "call-1",
            1,
            "mcp__srv__record",
        )
        .await;
        let evidence2 = effect2.evidence_id.unwrap();
        let key2 = UnresolvedEffectKey::new(scope2, 1).unwrap();
        let mut input =
            UnknownEffectInput::new("mcp__srv__record", UnknownFactReason::TransportLost);
        for _ in 0..mio_coding_agent::unresolved_effect::MAX_FACTS_PER_RECORD {
            input = input.with_evidence_id(uuid::Uuid::new_v4());
            incomplete_host
                .ledger
                .record_unknown(&key2, input.clone())
                .unwrap();
        }
        let record = incomplete_host.ledger.lookup(&key2).unwrap().unwrap();
        assert!(
            record.record.facts_dropped > 0,
            "the record must be incomplete"
        );
        assert!(
            record
                .record
                .facts
                .iter()
                .any(|fact| fact.evidence_id == Some(evidence2)),
            "the bounded record must retain the original live evidence"
        );
        let error = incomplete_host
            .confirm_unknown_side_effect(&call, std::slice::from_ref(&effect2))
            .await
            .unwrap_err();
        assert!(error.contains("incomplete"), "{error}");
        assert_eq!(incomplete_broker.pending_count(), 0);
    }

    /// JSON mode has no broker: even with real, resolvable evidence the
    /// confirmation refuses instead of confirming by omission.
    #[tokio::test]
    async fn json_mode_confirmation_refuses_with_real_resolvable_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let host = test_host(&root, "live-json", false, false);
        host.journal.begin_run().unwrap();
        seed_coverage(&host);

        let run_id = uuid::Uuid::new_v4();
        let effect =
            persist_live_runtime_unknown(&host, run_id, "call-1", 1, "mcp__srv__record").await;
        let call = runtime_unknown_call("workspace", "write guarded.txt");
        let refused = host
            .confirm_unknown_side_effect(&call, std::slice::from_ref(&effect))
            .await
            .unwrap_err();
        assert!(
            refused.contains("interactive terminal") || refused.contains("RPC approval broker"),
            "{refused}"
        );
    }

    #[tokio::test]
    async fn live_evidence_cannot_be_confirmed_under_another_real_scope() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path(), "live-scope", false, false);
        host.journal.begin_run().unwrap();
        seed_coverage(&host);
        let run_id = uuid::Uuid::new_v4();
        let mut effect =
            persist_live_runtime_unknown(&host, run_id, "call-1", 1, "mcp__srv__record").await;
        effect.scope = Some(run_id);
        host.resolve_unknown_claim(&effect).unwrap();
        effect.scope = Some(uuid::Uuid::new_v4());
        let error = host.resolve_unknown_claim(&effect).unwrap_err();
        assert!(
            error.contains("does not match its real run scope"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn ledger_failure_blocks_writes_but_never_reads() {
        let dir = tempfile::tempdir().unwrap();
        let ledger_dir = dir.path().join(".mio/unresolved-effects");
        std::fs::create_dir_all(&ledger_dir).unwrap();
        std::fs::write(ledger_dir.join("effects.json"), "{ not json").unwrap();
        let host = test_host(dir.path(), "ledger-corrupt", false, false);
        host.journal.begin_run().unwrap();
        // Recovery detects the damaged ledger and fails the guard closed for
        // side effects (before any tool call).
        host.recover_and_seed_guard();
        let scope = uuid::Uuid::new_v4().to_string();

        let mut read_call = canonical_call(&scope, 1, ToolCallPhase::Dispatched, Some("r"), None);
        read_call.tool = "read".into();
        read_call.operation = "read".into();
        host.on_tool_call_lifecycle(&read_call)
            .expect("a damaged ledger must never block a read");
        assert!(
            host.unknown_effect_fence().unwrap().persist_failed(),
            "the damaged ledger must fail the guard closed for side effects"
        );

        let write_call = canonical_call(&scope, 2, ToolCallPhase::Dispatched, Some("w"), None);
        assert!(
            host.on_tool_call_lifecycle(&write_call).is_err(),
            "a write must be refused when the ledger cannot record it"
        );
        assert!(host.ledger.snapshot().is_err());
    }

    #[tokio::test]
    async fn no_session_ledger_is_disabled_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path(), "no-session", true, false);
        assert!(
            !host.ledger.mode().is_writable(),
            "an ephemeral run has no durable ledger"
        );
        let scope = uuid::Uuid::new_v4().to_string();
        let dispatched = canonical_call(&scope, 1, ToolCallPhase::Dispatched, Some("c"), None);
        host.on_tool_call_lifecycle(&dispatched).unwrap();
        host.recover_and_seed_guard();
        assert!(
            !dir.path().join(".mio/unresolved-effects").exists(),
            "--no-session must perform zero extra disk writes"
        );
    }

    #[tokio::test]
    async fn durable_admission_refuses_a_write_and_leaves_no_journal_dispatched() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path(), "durable-admission", false, false);
        seed_coverage(&host);
        host.journal.begin_run().unwrap();

        // A different real run scope records an Unknown after this host's guard
        // would normally have been seeded.
        let blocker = UnresolvedEffectKey::new(uuid::Uuid::new_v4().to_string(), 7).unwrap();
        host.ledger
            .record_unknown(
                &blocker,
                UnknownEffectInput::new("mcp__srv__write", UnknownFactReason::TransportLost),
            )
            .unwrap();

        let scope = uuid::Uuid::new_v4().to_string();
        let key = UnresolvedEffectKey::new(scope.clone(), 1).unwrap();
        let write = canonical_call(&scope, 1, ToolCallPhase::Dispatched, Some("w"), None);
        let error = host
            .on_tool_call_lifecycle(&write)
            .expect_err("a write must be refused while a durable unknown is unacknowledged");
        assert!(error.contains("unacknowledged"), "{error}");
        assert!(
            host.ledger.lookup(&key).unwrap().is_none(),
            "a refused dispatch must not be claimed"
        );
        let report = host.journal.recovery_report().unwrap();
        assert!(
            report
                .calls
                .iter()
                .all(|call| call.scope != scope || call.call_index != 1),
            "no canonical Dispatched may be left for a refused call"
        );

        // Confirming the exact shown snapshot lets the same write continue.
        let shown = host.ledger.lookup(&blocker).unwrap().unwrap();
        host.ledger
            .acknowledge(&blocker, &shown.snapshot_key, None)
            .unwrap();
        host.on_tool_call_lifecycle(&write)
            .expect("an acknowledged exact snapshot lets a new write continue");
        assert_eq!(
            host.ledger.lookup(&key).unwrap().unwrap().record.state,
            UnresolvedEffectState::Outstanding
        );
    }

    #[tokio::test]
    async fn a_failed_journal_write_retracts_the_undispatched_claim() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path(), "retract", false, false);
        seed_coverage(&host);
        // No begin_run: the canonical journal write must fail after the ledger
        // claim, and the never-dispatched intent must be retracted instead of
        // left to become a false `Unknown` on a later recovery.
        let scope = uuid::Uuid::new_v4().to_string();
        let write = canonical_call(&scope, 1, ToolCallPhase::Dispatched, Some("w"), None);
        assert!(host.on_tool_call_lifecycle(&write).is_err());
        assert!(
            host.ledger
                .lookup(&UnresolvedEffectKey::new(scope, 1).unwrap())
                .unwrap()
                .is_none(),
            "a claim that provably never dispatched must be retracted"
        );
    }

    #[tokio::test]
    async fn turn_finalize_only_finalizes_truly_ended_runs() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path(), "turn-finalize", false, false);
        seed_coverage(&host);
        host.journal.begin_run().unwrap();
        let top = uuid::Uuid::new_v4().to_string();
        let child = uuid::Uuid::new_v4().to_string();
        note_run(&host, &top, true);
        note_run(&host, &child, true);
        let mine = canonical_call(&top, 1, ToolCallPhase::Dispatched, Some("mine"), None);
        host.on_tool_call_lifecycle(&mine).unwrap();
        let child_call = canonical_call(&child, 1, ToolCallPhase::Dispatched, Some("child"), None);
        host.on_tool_call_lifecycle(&child_call).unwrap();

        // A dispatch from another scope (a live sibling/process) was not
        // claimed this turn and must stay Outstanding.
        let other = UnresolvedEffectKey::new(uuid::Uuid::new_v4().to_string(), 1).unwrap();
        host.ledger.record_dispatched(&other).unwrap();

        // The top run ends; the child is still live.
        note_run(&host, &top, false);
        host.finalize_turn_dispatches();
        assert_eq!(
            host.ledger
                .lookup(&UnresolvedEffectKey::new(top.clone(), 1).unwrap())
                .unwrap()
                .unwrap()
                .record
                .state,
            UnresolvedEffectState::Unknown,
            "an ended run's unconfirmed dispatch becomes Unknown"
        );
        assert_eq!(
            host.ledger
                .lookup(&UnresolvedEffectKey::new(child.clone(), 1).unwrap())
                .unwrap()
                .unwrap()
                .record
                .state,
            UnresolvedEffectState::Outstanding,
            "a still-live child run must never be finalized as Unknown"
        );
        assert_eq!(
            host.dispatched_call_index(&child, "child"),
            Some(1),
            "a live child's dispatch mapping must survive a new top turn"
        );

        // A new top turn recovers without erasing the live child's mapping.
        host.recover_and_seed_guard();
        assert_eq!(host.dispatched_call_index(&child, "child"), Some(1));
        assert_eq!(
            host.ledger
                .lookup(&UnresolvedEffectKey::new(child.clone(), 1).unwrap())
                .unwrap()
                .unwrap()
                .record
                .state,
            UnresolvedEffectState::Outstanding,
            "recovery must not upgrade a live child"
        );

        // Once the child really ends, its leftover claim is finalized.
        note_run(&host, &child, false);
        host.finalize_turn_dispatches();
        assert_eq!(
            host.ledger
                .lookup(&UnresolvedEffectKey::new(child, 1).unwrap())
                .unwrap()
                .unwrap()
                .record
                .state,
            UnresolvedEffectState::Unknown,
            "a truly ended child's leftover dispatch is finalized"
        );
    }

    #[tokio::test]
    async fn a_new_fact_for_the_same_key_refreshes_the_shown_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path(), "same-key-refresh", false, false);
        let key = UnresolvedEffectKey::new(uuid::Uuid::new_v4().to_string(), 1).unwrap();
        host.ledger
            .record_unknown(
                &key,
                UnknownEffectInput::new("write", UnknownFactReason::Interrupted)
                    .with_evidence_id(uuid::Uuid::new_v4()),
            )
            .unwrap();
        host.recover_and_seed_guard();
        let first = host
            .unknown_effect_fence()
            .unwrap()
            .snapshot()
            .pop()
            .unwrap();

        // A new fact for the same real key changes the evidence version.
        host.ledger
            .record_unknown(
                &key,
                UnknownEffectInput::new("write", UnknownFactReason::Interrupted)
                    .with_evidence_id(uuid::Uuid::new_v4()),
            )
            .unwrap();
        host.recover_and_seed_guard();
        let second = host
            .unknown_effect_fence()
            .unwrap()
            .snapshot()
            .pop()
            .unwrap();
        assert_eq!(second.ledger_key(), Some((key.scope.as_str(), 1)));
        assert_ne!(second.ledger_snapshot(), first.ledger_snapshot());

        // The earlier digest is stale; only the exact new one can be confirmed.
        let stale = acknowledge_unresolved_exact(
            &host.ledger,
            &key.scope,
            1,
            first.ledger_snapshot().unwrap(),
            None,
        );
        assert!(
            stale.is_err(),
            "an old digest must not confirm new evidence"
        );
        acknowledge_unresolved_exact(
            &host.ledger,
            &key.scope,
            1,
            second.ledger_snapshot().unwrap(),
            None,
        )
        .unwrap();
    }

    #[tokio::test]
    async fn repeated_seeding_is_bounded_and_acked_history_never_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path(), "seed-bound", false, false);

        // 65 different acknowledged records must not make a later turn overflow
        // its fresh guard or refuse an unrelated write.
        for index in 0..65u64 {
            let key = UnresolvedEffectKey::new(format!("acked-{index}"), 1).unwrap();
            let observation = host
                .ledger
                .record_unknown(
                    &key,
                    UnknownEffectInput::new("write", UnknownFactReason::Interrupted),
                )
                .unwrap();
            host.ledger
                .acknowledge(&key, &observation.snapshot_key, None)
                .unwrap();
        }
        let live = UnresolvedEffectKey::new(uuid::Uuid::new_v4().to_string(), 1).unwrap();
        host.ledger
            .record_unknown(
                &live,
                UnknownEffectInput::new("write", UnknownFactReason::Interrupted),
            )
            .unwrap();
        for _ in 0..200 {
            host.recover_and_seed_guard();
            let fence = host.unknown_effect_fence().unwrap();
            assert_eq!(fence.len(), 1, "only the live unknown is seeded");
            assert!(!fence.overflowed(), "a fresh guard must not overflow");
        }

        let shown = host.ledger.lookup(&live).unwrap().unwrap();
        host.ledger
            .acknowledge(&live, &shown.snapshot_key, None)
            .unwrap();
        host.recover_and_seed_guard();
        assert!(host.unknown_effect_fence().unwrap().is_empty());
        host.journal.begin_run().unwrap();
        let scope = uuid::Uuid::new_v4().to_string();
        host.on_tool_call_lifecycle(&canonical_call(
            &scope,
            1,
            ToolCallPhase::Dispatched,
            Some("w"),
            None,
        ))
        .expect("acknowledged history must not refuse a new write");
    }

    #[tokio::test]
    async fn fence_overflow_has_an_exact_maintenance_ack_path() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path(), "overflow-maintenance", false, false);
        let overflow = mio_coding_agent::unknown_effect::MAX_UNKNOWN_EFFECTS + 1;
        let mut keys = Vec::new();
        for index in 0..overflow {
            let key = UnresolvedEffectKey::new(format!("pending-{index}"), 1).unwrap();
            let observation = host
                .ledger
                .record_unknown(
                    &key,
                    UnknownEffectInput::new("write", UnknownFactReason::Interrupted),
                )
                .unwrap();
            keys.push((key, observation.snapshot_key));
        }

        // The run guard overflows and refuses: a normal confirmation cannot
        // release an incomplete snapshot.
        host.recover_and_seed_guard();
        assert!(host.unknown_effect_fence().unwrap().overflowed());

        // The maintenance path inspects and acknowledges each exact key, so the
        // operator can recover without deleting evidence.
        let view = unresolved_effects_view(&host.ledger);
        let entries = view["entries"].as_array().cloned().unwrap_or_default();
        assert_eq!(
            entries.len(),
            overflow,
            "inspection lists every exact durable key"
        );
        for entry in &entries {
            assert!(entry["acknowledgeable"].as_bool().unwrap_or(false));
            assert_eq!(entry["snapshot"].as_str().unwrap().len(), 64);
        }
        for (key, snapshot_key) in &keys {
            acknowledge_unresolved_exact(
                &host.ledger,
                &key.scope,
                1,
                snapshot_key.as_str(),
                Some("operator reviewed the exact durable record"),
            )
            .unwrap();
        }
        host.recover_and_seed_guard();
        let fence = host.unknown_effect_fence().unwrap();
        assert!(fence.is_empty(), "the recovered guard is no longer tripped");
        assert!(!fence.overflowed());
    }

    #[tokio::test]
    async fn a_legacy_canonical_unknown_is_migrated_with_its_real_key() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path(), "legacy-migrate", false, false);
        // Simulate an older Mio journal: a canonical call left `Dispatched` in
        // a previous run with no ledger record of it.
        host.journal.begin_run().unwrap();
        let scope = uuid::Uuid::new_v4().to_string();
        host.journal
            .record_canonical_phase(&canonical_call(
                &scope,
                1,
                ToolCallPhase::Dispatched,
                Some("call-1"),
                None,
            ))
            .unwrap();
        host.journal.finish_run(RunState::Interrupted).unwrap();
        assert!(host.ledger.snapshot().unwrap().is_empty());

        host.recover_and_seed_guard();
        let migrated = host
            .ledger
            .lookup(&UnresolvedEffectKey::new(scope.clone(), 1).unwrap())
            .unwrap()
            .expect("the legacy canonical unknown must be migrated, not ignored");
        assert_eq!(migrated.record.state, UnresolvedEffectState::Unknown);

        // The migrated real key blocks a new write in the same transaction.
        host.journal.begin_run().unwrap();
        let write = canonical_call(
            &uuid::Uuid::new_v4().to_string(),
            1,
            ToolCallPhase::Dispatched,
            Some("w"),
            None,
        );
        assert!(
            host.on_tool_call_lifecycle(&write).is_err(),
            "a migrated legacy unknown must refuse a new write"
        );
    }

    #[tokio::test]
    async fn a_legacy_unknown_without_a_canonical_key_gets_a_recoverable_origin() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path(), "legacy-v1", false, false);
        host.journal.begin_run().unwrap();
        host.journal.record_intent("write", "a.txt").unwrap();
        host.journal.finish_run(RunState::Interrupted).unwrap();
        assert!(host.ledger.snapshot().unwrap().is_empty());

        host.recover_and_seed_guard();
        // The legacy operation is migrated under a distinct origin derived from
        // its real journal identity: not silently ignored, and not permanently
        // fail-closed with no usable recovery action.
        let snapshot = host.ledger.snapshot().unwrap();
        assert_eq!(snapshot.len(), 1, "{snapshot:?}");
        let entry = &snapshot.entries[0];
        assert_eq!(entry.record.state, UnresolvedEffectState::Unknown);
        assert!(
            entry.record.scope.starts_with("journal-legacy:legacy-v1:"),
            "{}",
            entry.record.scope
        );
        assert!(
            !host.unknown_effect_fence().unwrap().persist_failed(),
            "a recoverable migration must not permanently fail closed"
        );

        // A new write is refused while the legacy unknown is unacknowledged.
        host.journal.begin_run().unwrap();
        let write = canonical_call(
            &uuid::Uuid::new_v4().to_string(),
            1,
            ToolCallPhase::Dispatched,
            Some("w"),
            None,
        );
        assert!(host.on_tool_call_lifecycle(&write).is_err());

        // The exact maintenance path acknowledges the legacy origin by its real
        // key + snapshot, after which a new write is allowed.
        let key = entry.record.key();
        acknowledge_unresolved_exact(
            &host.ledger,
            &key.scope,
            key.call_index,
            entry.snapshot_key.as_str(),
            Some("operator reviewed the legacy evidence"),
        )
        .unwrap();
        host.on_tool_call_lifecycle(&write)
            .expect("an acked legacy unknown lets a new write continue");
    }

    #[tokio::test]
    async fn a_new_session_migrates_another_sessions_old_unknown() {
        let dir = tempfile::tempdir().unwrap();
        // Session A leaves a `Dispatched` canonical call in its own journal.
        let session_a = test_host(dir.path(), "session-a", false, false);
        session_a.journal.begin_run().unwrap();
        let old_scope = uuid::Uuid::new_v4().to_string();
        session_a
            .journal
            .record_canonical_phase(&canonical_call(
                &old_scope,
                1,
                ToolCallPhase::Dispatched,
                Some("call-a"),
                None,
            ))
            .unwrap();
        session_a.journal.finish_run(RunState::Interrupted).unwrap();
        assert!(session_a.ledger.snapshot().unwrap().is_empty());

        // A brand new session B starts. Its own journal says nothing, but the
        // bounded whole-workspace scan must still surface A's unknown.
        let session_b = test_host(dir.path(), "session-b", false, false);
        session_b.recover_and_seed_guard();
        let snapshot = session_b.ledger.snapshot().unwrap();
        assert_eq!(snapshot.len(), 1, "{snapshot:?}");
        assert_eq!(snapshot.entries[0].record.scope, old_scope);
        session_b.journal.begin_run().unwrap();
        let write = canonical_call(
            &uuid::Uuid::new_v4().to_string(),
            1,
            ToolCallPhase::Dispatched,
            Some("w"),
            None,
        );
        assert!(
            session_b.on_tool_call_lifecycle(&write).is_err(),
            "a new session must not bypass an old session's durable unknown"
        );
    }

    #[tokio::test]
    async fn subagent_query_calls_are_reads_and_survive_a_damaged_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let ledger_dir = dir.path().join(".mio/unresolved-effects");
        std::fs::create_dir_all(&ledger_dir).unwrap();
        std::fs::write(ledger_dir.join("effects.json"), "{ not json").unwrap();
        let host = test_host(dir.path(), "subagent-read", false, false);
        host.journal.begin_run().unwrap();
        let scope = uuid::Uuid::new_v4().to_string();

        for (index, operation) in ["status", "snapshot", "list"].into_iter().enumerate() {
            let mut query = canonical_call(
                &scope,
                index as u64 + 1,
                ToolCallPhase::Dispatched,
                Some(operation),
                None,
            );
            query.tool = "subagent".into();
            query.operation = "subagent".into();
            query.target = operation.into();
            host.on_tool_call_lifecycle(&query).unwrap_or_else(|error| {
                panic!("subagent {operation} is a read and must survive: {error}")
            });
        }

        // A subagent spawn is write/external and must be refused by the damaged
        // ledger: this is a precise classifier, not blanket all-subagent-read.
        let mut spawn = canonical_call(&scope, 10, ToolCallPhase::Dispatched, Some("spawn"), None);
        spawn.tool = "subagent".into();
        spawn.operation = "subagent".into();
        spawn.target = "spawn".into();
        assert!(host.on_tool_call_lifecycle(&spawn).is_err());
    }

    #[tokio::test]
    async fn dispatched_boundary_output_failure_leaves_no_false_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = test_host(dir.path(), "dispatch-output-failure", false, false);
        seed_coverage(&host);
        host.journal.begin_run().unwrap();
        let output = JsonOutput::new();
        assert!(
            output
                .emit(json!({"payload": "x".repeat(2 * 1024 * 1024)}))
                .is_err()
        );
        assert!(output.is_failed());
        Arc::get_mut(&mut host).unwrap().output = Some(output);

        let scope = uuid::Uuid::new_v4().to_string();
        let key = UnresolvedEffectKey::new(scope.clone(), 1).unwrap();
        let write = canonical_call(&scope, 1, ToolCallPhase::Dispatched, Some("w"), None);
        assert!(
            host.on_tool_call_lifecycle(&write).is_err(),
            "the transport failure aborts the dispatch"
        );

        // The canonical journal really did persist `Dispatched`...
        let report = host.journal.recovery_report().unwrap();
        assert!(
            report
                .calls
                .iter()
                .any(|call| call.scope == scope && call.phase == ToolCallPhase::Dispatched)
        );
        // ...but the ledger records the honest not-unknown fact.
        assert_eq!(
            host.ledger.lookup(&key).unwrap().unwrap().record.state,
            UnresolvedEffectState::Resolved,
            "a never-bus-dispatched call must not be left Outstanding/Unknown"
        );

        // A later recovery must not resurrect it as a false Unknown.
        host.recover_and_seed_guard();
        let snapshot = host.ledger.snapshot().unwrap();
        assert_eq!(snapshot.unknown_count(), 0, "{snapshot:?}");
        assert!(host.unknown_effect_fence().unwrap().is_empty());
    }

    #[test]
    fn maintenance_ack_refuses_incomplete_and_stale_records() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = open_ledger(false, dir.path());
        let key = UnresolvedEffectKey::new("legacy", 1).unwrap();
        let mut input = UnknownEffectInput::new("write", UnknownFactReason::Interrupted);
        ledger.record_unknown(&key, input.clone()).unwrap();
        // Force facts_dropped > 0 by pushing past the per-record fact cap.
        for _ in 0..mio_coding_agent::unresolved_effect::MAX_FACTS_PER_RECORD {
            input = input.with_evidence_id(uuid::Uuid::new_v4());
            ledger.record_unknown(&key, input.clone()).unwrap();
        }
        let current = ledger.lookup(&key).unwrap().unwrap();
        assert!(current.record.facts_dropped > 0);
        let error = acknowledge_unresolved_exact(
            &ledger,
            &key.scope,
            1,
            current.snapshot_key.as_str(),
            None,
        )
        .unwrap_err();
        assert!(error.contains("incomplete"), "{error}");
        assert_eq!(
            ledger.lookup(&key).unwrap().unwrap().record.state,
            UnresolvedEffectState::Unknown
        );
        // A stale digest is refused too.
        let stale = "0".repeat(64);
        assert!(acknowledge_unresolved_exact(&ledger, &key.scope, 1, &stale, None).is_err());
    }

    // --- Whole-workspace coverage epoch ---

    /// Plant a well-formed journal that records real journal retention loss.
    fn plant_lossy_journal(root: &Path, session_id: &str, dropped_runs: u64) {
        let journal_dir = root.join(journal::JOURNAL_DIR);
        std::fs::create_dir_all(&journal_dir).unwrap();
        let value = json!({
            "version": journal::JOURNAL_VERSION,
            "session_id": session_id,
            "updated": 0,
            "dropped_runs": dropped_runs,
            "truncated_operations": 0,
            "runs": [],
        });
        std::fs::write(
            journal_dir.join(format!("{session_id}.json")),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn a_first_write_commits_empty_coverage_without_a_prior_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path(), "fresh-write", false, false);
        host.journal.begin_run().unwrap();
        // Turn-start recovery on an empty, loss-free workspace must not create
        // a ledger just because a turn ran.
        host.recover_and_seed_guard();
        assert!(
            !dir.path().join(".mio/unresolved-effects").exists(),
            "a text-only turn must not create the ledger"
        );
        // The first real write commits the durable empty epoch in its own flock.
        let scope = uuid::Uuid::new_v4().to_string();
        host.on_tool_call_lifecycle(&canonical_call(
            &scope,
            1,
            ToolCallPhase::Dispatched,
            Some("w"),
            None,
        ))
        .expect("the first write commits coverage and is admitted");
        assert!(host.ledger.coverage().unwrap().initialized);
        assert!(
            dir.path()
                .join(".mio/unresolved-effects/effects.json")
                .is_file()
        );
    }

    #[tokio::test]
    async fn pre_epoch_truncation_latches_and_no_normal_ack_releases_it() {
        let dir = tempfile::tempdir().unwrap();
        plant_lossy_journal(dir.path(), "old-session", 3);
        let host = test_host(dir.path(), "fresh", false, false);
        host.journal.begin_run().unwrap();
        host.recover_and_seed_guard();

        // The epoch is not committed; instead a durable latch records the loss.
        let coverage = host.ledger.coverage().unwrap();
        assert!(!coverage.initialized, "{coverage:?}");
        assert!(coverage.incomplete, "{coverage:?}");
        assert!(
            host.unknown_effect_fence().unwrap().persist_failed(),
            "pre-epoch loss must fail the guard closed"
        );
        // No fabricated `journal_truncation` record exists to acknowledge.
        let view = unresolved_effects_view(&host.ledger);
        assert!(
            view["entries"].as_array().unwrap().is_empty(),
            "missing evidence must not become an ackable record: {view}"
        );

        // A new write is refused by the coverage gate.
        let write = canonical_call(
            &uuid::Uuid::new_v4().to_string(),
            1,
            ToolCallPhase::Dispatched,
            Some("w"),
            None,
        );
        let error = host.on_tool_call_lifecycle(&write).unwrap_err();
        assert!(error.contains("coverage"), "{error}");

        // A read stays available.
        let mut read = canonical_call(
            &uuid::Uuid::new_v4().to_string(),
            1,
            ToolCallPhase::Dispatched,
            Some("r"),
            None,
        );
        read.tool = "read".into();
        read.operation = "read".into();
        host.on_tool_call_lifecycle(&read)
            .expect("reads stay available under an incomplete epoch");

        // Not even a later "complete" claim can clear the latch.
        assert_eq!(
            host.ledger.commit_initial_coverage(&[], true).unwrap(),
            CoverageCommit::AlreadyIncomplete
        );
        assert!(host.ledger.coverage().unwrap().incomplete);
    }

    #[tokio::test]
    async fn post_epoch_retention_truncation_never_blocks_65_plus_normal_turns() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path(), "steady", false, false);
        // Commit the epoch before any retention truncation exists.
        seed_coverage(&host);
        assert!(host.ledger.coverage().unwrap().initialized);

        let mut saw_dropped = false;
        for turn in 0..70u64 {
            host.journal.begin_run().unwrap();
            host.recover_and_seed_guard();
            let value: serde_json::Value = serde_json::from_slice(
                &std::fs::read(journal::journal_path(dir.path(), "steady")).unwrap(),
            )
            .unwrap();
            if value["dropped_runs"].as_u64().unwrap_or(0) > 0 {
                saw_dropped = true;
            }
            // A normal write is admitted: post-epoch retention truncation is not
            // evidence and must not become a blocking unknown.
            let scope = uuid::Uuid::new_v4().to_string();
            host.on_tool_call_lifecycle(&canonical_call(
                &scope,
                1,
                ToolCallPhase::Dispatched,
                Some("w"),
                None,
            ))
            .unwrap_or_else(|error| panic!("turn {turn} was falsely blocked: {error}"));
            host.on_tool_call_lifecycle(&canonical_call(
                &scope,
                1,
                ToolCallPhase::Executed,
                Some("w"),
                None,
            ))
            .unwrap();
            host.journal.finish_run(RunState::Completed).unwrap();
        }
        assert!(
            saw_dropped,
            "the test must actually reach real journal retention truncation"
        );
        assert!(host.ledger.coverage().unwrap().initialized);
        assert!(!host.ledger.coverage().unwrap().incomplete);
    }

    // --- Pure reads never fence writes ---

    #[tokio::test]
    async fn a_pure_read_never_fences_a_later_write() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path(), "read-unknown", false, false);
        seed_coverage(&host);
        host.journal.begin_run().unwrap();
        let scope = uuid::Uuid::new_v4().to_string();

        // A read dispatched and then interrupted keeps its journal phase but
        // must not create any durable effect-ledger record.
        let mut read = canonical_call(
            &scope,
            1,
            ToolCallPhase::Dispatched,
            Some("call-read"),
            None,
        );
        read.tool = "read".into();
        read.operation = "read".into();
        host.on_tool_call_lifecycle(&read).unwrap();
        assert!(
            host.ledger.snapshot().unwrap().is_empty(),
            "a pure read must not claim the effect ledger"
        );
        host.journal.finish_run(RunState::Interrupted).unwrap();

        // Migration must recognize the known pure read and not fence a write.
        host.recover_and_seed_guard();
        assert!(host.ledger.snapshot().unwrap().is_empty());
        assert!(!host.unknown_effect_fence().unwrap().persist_failed());
        host.journal.begin_run().unwrap();
        let write_scope = uuid::Uuid::new_v4().to_string();
        host.on_tool_call_lifecycle(&canonical_call(
            &write_scope,
            1,
            ToolCallPhase::Dispatched,
            Some("w"),
            None,
        ))
        .expect("a pure read must never fence a later write");
    }

    #[tokio::test]
    async fn an_interrupted_write_still_requires_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path(), "write-unknown", false, false);
        seed_coverage(&host);
        host.journal.begin_run().unwrap();
        let scope = uuid::Uuid::new_v4().to_string();
        host.on_tool_call_lifecycle(&canonical_call(
            &scope,
            1,
            ToolCallPhase::Dispatched,
            Some("call-write"),
            None,
        ))
        .unwrap();
        // The run is observed to end without confirming the write.
        note_run(&host, &scope, false);
        host.finalize_turn_dispatches();
        let key = UnresolvedEffectKey::new(scope.clone(), 1).unwrap();
        assert_eq!(
            host.ledger.lookup(&key).unwrap().unwrap().record.state,
            UnresolvedEffectState::Unknown
        );
        host.recover_and_seed_guard();

        host.journal.begin_run().unwrap();
        let other = uuid::Uuid::new_v4().to_string();
        assert!(
            host.on_tool_call_lifecycle(&canonical_call(
                &other,
                1,
                ToolCallPhase::Dispatched,
                Some("w2"),
                None,
            ))
            .is_err(),
            "a real interrupted write must still block a later write"
        );
    }

    // --- Bounded run/dispatch correlation capacity ---

    #[tokio::test]
    async fn saturated_run_tracking_refuses_writes_but_not_reads() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path(), "run-capacity", false, false);
        seed_coverage(&host);
        // A real read still needs the real canonical journal to have an active
        // run; the saturation refusal must come from run tracking, not from a
        // missing journal run. This is a fixture setup, not a relaxed gate.
        host.journal.begin_run().unwrap();
        // Saturate the host's bounded run-liveness registry directly. Emitting
        // MAX_TRACKED_RUNS real `RunStarted` observations would also bind that
        // many scopes to the single journal run, overflowing its separate
        // per-run scope-binding cap and latching the journal fail-closed for
        // unbound scopes; the read would then fail for an unrelated reason. The
        // real observation path stays covered by the other run-tracking tests.
        let mut scopes = Vec::new();
        for _ in 0..MAX_TRACKED_RUNS {
            let scope = uuid::Uuid::new_v4().to_string();
            host.note_run_liveness(&scope, true);
            scopes.push(scope);
        }
        assert!(
            host.run_tracking_saturated(),
            "a registry full of live runs is saturated"
        );
        // The map is full of live runs: a new live scope cannot be tracked
        // without evicting a live one, so side effects fail closed.
        host.note_run_liveness(&uuid::Uuid::new_v4().to_string(), true);
        assert!(host.run_tracking_saturated());
        assert!(
            host.on_tool_call_lifecycle(&canonical_call(
                &uuid::Uuid::new_v4().to_string(),
                1,
                ToolCallPhase::Dispatched,
                Some("w"),
                None,
            ))
            .is_err(),
            "writes must fail closed when live-run tracking is saturated"
        );
        let mut read = canonical_call(
            &uuid::Uuid::new_v4().to_string(),
            1,
            ToolCallPhase::Dispatched,
            Some("r"),
            None,
        );
        read.tool = "read".into();
        read.operation = "read".into();
        host.on_tool_call_lifecycle(&read)
            .expect("reads stay available while saturated");

        // One run really ending frees capacity and un-saturates the host.
        host.note_run_liveness(&scopes[0], false);
        assert!(!host.run_tracking_saturated());
        host.on_tool_call_lifecycle(&canonical_call(
            &uuid::Uuid::new_v4().to_string(),
            1,
            ToolCallPhase::Dispatched,
            Some("w-after"),
            None,
        ))
        .expect("a freed run slot lets a new write through");
    }

    #[tokio::test]
    async fn dispatch_correlation_capacity_never_exceeds_the_bound() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path(), "dispatch-capacity", false, false);
        let scope = uuid::Uuid::new_v4().to_string();
        note_run(&host, &scope, true);
        for index in 0..MAX_TRACKED_DISPATCHES {
            assert!(host.note_dispatched_call(
                &scope,
                Some(&format!("id-{index}")),
                index as u64 + 1,
            ));
        }
        assert_eq!(
            host.dispatched_calls.lock().unwrap().len(),
            MAX_TRACKED_DISPATCHES
        );
        assert!(
            !host.note_dispatched_call(&scope, Some("overflow"), 999),
            "the map must never grow past the bound"
        );
        assert_eq!(
            host.dispatched_calls.lock().unwrap().len(),
            MAX_TRACKED_DISPATCHES,
            "a refused correlation must not grow the map"
        );
    }

    #[tokio::test]
    async fn capacity_pruning_drops_ended_runs_and_keeps_live_children() {
        let dir = tempfile::tempdir().unwrap();
        let host = test_host(dir.path(), "dispatch-live", false, false);
        let live = uuid::Uuid::new_v4().to_string();
        let ended = uuid::Uuid::new_v4().to_string();
        note_run(&host, &live, true);
        note_run(&host, &ended, true);
        assert!(host.note_dispatched_call(&live, Some("live"), 1));
        assert!(host.note_dispatched_call(&ended, Some("ended"), 1));
        note_run(&host, &ended, false);

        let filler = uuid::Uuid::new_v4().to_string();
        note_run(&host, &filler, true);
        for index in 0..(MAX_TRACKED_DISPATCHES - 2) {
            assert!(host.note_dispatched_call(
                &filler,
                Some(&format!("f-{index}")),
                index as u64 + 1
            ));
        }
        assert_eq!(
            host.dispatched_calls.lock().unwrap().len(),
            MAX_TRACKED_DISPATCHES
        );

        // A new live mapping prunes the ended run's mapping only.
        let newcomer = uuid::Uuid::new_v4().to_string();
        note_run(&host, &newcomer, true);
        assert!(host.note_dispatched_call(&newcomer, Some("new"), 1));
        let map = host.dispatched_calls.lock().unwrap();
        assert!(map.contains_key(&(live.clone(), "live".to_owned())));
        assert!(map.contains_key(&(newcomer.clone(), "new".to_owned())));
        assert!(!map.contains_key(&(ended.clone(), "ended".to_owned())));
        assert_eq!(map.len(), MAX_TRACKED_DISPATCHES);
    }
    #[test]
    fn legacy_origin_identity_tuples_never_alias_on_separators() {
        assert_ne!(
            legacy_origin_scope("a:b", "c"),
            legacy_origin_scope("a", "b:c")
        );
        assert_ne!(
            legacy_origin_scope("a\0b", "c"),
            legacy_origin_scope("a", "b\0c")
        );
        assert_eq!(
            legacy_origin_scope("legacy-v1", "run-1"),
            "journal-legacy:legacy-v1:run-1"
        );
        for (session, run) in [("a:b", "c"), ("a\0b", "c")] {
            let scope = legacy_origin_scope(session, run);
            assert!(scope.starts_with("journal-legacy:"));
            assert!(scope.chars().count() <= MAX_SCOPE_CHARS);
            assert!(!scope.chars().any(char::is_control));
        }
    }
}
