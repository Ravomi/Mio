//! 持久化执行证据 journal（AUD-13）。
//!
//! 目标：在失败、取消或进程重启后，仍能区分「已执行」「执行失败」「结果未知」的
//! 副作用操作。设计约束：
//!
//! - **独立、版本化**：与 session 分离，使用独立的 `version` 字段与文件。
//! - **有大小限制**：限制运行数、每轮操作数与文件字节数，超出时丢弃最旧记录并计数。
//! - **Unix 0600**：journal 目录 0700、文件 0600，原子替换写入。
//! - **副作用之前持久化 intent**：`record_intent` 在操作真正执行前落盘；写入失败时
//!   返回 `Err`，由调用方阻止副作用。`record_bound_intent` 额外记录真实 dispatch
//!   关联（engine run scope、provider toolCallId、审批 attempt callId），并发同名同
//!   目标调用因此按各自的真实身份配对，而不是按完成顺序；旧文件缺少这些字段时仍按
//!   无绑定记录处理。
//! - **不自动重放**：本模块只记录证据，绝不据此重新执行历史工具，也不声称回滚文件。
//! - **无会话模式零落盘**：`Journal::disabled()` 的所有写入都是空操作。
//!
//! 通过 `#[path]` 被 `bin/mio.rs` 引用，避免与并行修改 `lib.rs` 的变更冲突。

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use tempfile::NamedTempFile;

use crate::shell::output_store::OutputReference;
/// Re-exported so `src/output_store.rs` can name the canonical phase through
/// the journal module, which is the module registered at the test-crate root by
/// the `#[path]`-compiled output-cleanup tests.
pub use crate::tool_lifecycle::ToolCallPhase;
use crate::tool_lifecycle::ToolCallRecord;

/// Current journal data format version.
///
/// v1 recorded only run-level interruption and tool-level `Pending -> Executed /
/// Failed / Unknown`. v2 adds the bounded `calls` canonical tool-call state
/// machine (`Normalized -> Denied | Admitted -> Dispatched -> Executed | Failed
/// | Unknown`, plus `NotDispatched` for an interrupted pre-dispatch call). A v1
/// file still loads and reports faithfully with an empty `calls` list: no phase
/// is fabricated for it. The first real write upgrades the file to v2 and
/// preserves the original legacy `operations` evidence; a reader that does not
/// know v2 must fail closed (it does, because the version check rejects it).
/// Queries stay read-only and never upgrade.
pub const JOURNAL_VERSION: u32 = 2;
/// The previous, still readable journal format version.
pub const JOURNAL_VERSION_V1: u32 = 1;
/// 相对于工作区根目录的 journal 目录。
pub const JOURNAL_DIR: &str = ".mio/journal";
/// 单个 journal 文件的字节上限。
pub const MAX_JOURNAL_FILE_SIZE: usize = 512 * 1024;
/// 保留的最大运行记录数，超出丢弃最旧记录。
pub const MAX_JOURNAL_RUNS: usize = 64;
/// 单次运行保留的最大操作数，超出丢弃最旧操作。
pub const MAX_RUN_OPERATIONS: usize = 512;
/// 单次运行保留的最大 canonical 调用数；超出时丢弃最旧记录并计入 `dropped_calls`。
pub const MAX_RUN_CALLS: usize = 512;
/// 单次运行最多绑定的真实 engine run scope 数；超出时 fail-closed 拒绝新绑定，
/// 绝不静默丢弃仍在用的 scope 证据。
pub const MAX_RUN_SCOPES: usize = 512;
/// 单个操作目标字段的最大字符数。
pub const MAX_TARGET_CHARS: usize = 512;
/// 单条结果摘要的最大字符数。
pub const MAX_DETAIL_CHARS: usize = 512;

/// 一次 Agent 运行（通常对应一个用户回合）的终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// 进程仍在运行或意外中断。
    Active,
    /// 回合正常完成。
    Completed,
    /// 回合以错误结束（例如模型请求失败）。
    Failed,
    /// 回合被用户取消。
    Cancelled,
    /// 进程在回合结束前退出，重启时标记。
    Interrupted,
}

/// 副作用操作的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpStatus {
    /// 已持久化 intent，但尚未记录结果。
    Pending,
    /// 已执行并记录结果。
    Executed,
    /// 已执行但返回错误。
    Failed,
    /// 无法确认结果（进程中断、取消或结果未落盘）。
    Unknown,
}

/// A side-effecting operation's real dispatch correlation.
///
/// Every field is optional and only ever filled from a value the caller really
/// has. `scope` is the real engine run UUID the call was dispatched under,
/// `tool_call_id` is the provider's own tool-call id (kept strictly separate
/// from the approval attempt's `call_id`), and `turn`/`effect_id` are filled in
/// from the real dispatch identity when the result is recorded. Nothing here is
/// derived from the operation name, the target or completion order, and `None`
/// always means "unknown", never a fabricated value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationCorrelation {
    /// Real engine run scope (the run UUID) the call was dispatched under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// The provider's own tool-call id, never the approval attempt id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// The approval attempt's id, independent of `tool_call_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    /// Real engine turn, recorded with the result from the dispatch identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u64>,
    /// The dispatcher's real effect id, recorded with the result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_id: Option<u64>,
}

impl OperationCorrelation {
    /// The correlation a real approval binding proves: the run scope, the
    /// provider's tool-call id and the approval attempt id.
    ///
    /// A binding carries no turn or effect id, so both stay `None` here instead
    /// of being guessed from the operation, target or dispatch order.
    pub fn from_binding(binding: &crate::policy::ApprovalBinding) -> Self {
        Self {
            scope: bounded(&binding.scope),
            tool_call_id: binding.tool_call_id.as_deref().and_then(bounded),
            call_id: bounded(&binding.call_id),
            turn: None,
            effect_id: None,
        }
    }

    /// True when no real correlation id was recorded.
    pub fn is_none(&self) -> bool {
        self.scope.is_none()
            && self.tool_call_id.is_none()
            && self.call_id.is_none()
            && self.turn.is_none()
            && self.effect_id.is_none()
    }

    /// True when this record is pinned to a real dispatch/approval binding.
    ///
    /// A bound record is completed only by a result that carries the same real
    /// scope and tool-call id; operation/target alone never completes it.
    fn is_bound(&self) -> bool {
        self.scope.is_some() || self.tool_call_id.is_some() || self.call_id.is_some()
    }
}

/// 单个副作用操作的证据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationRecord {
    pub op_id: u64,
    pub operation: String,
    pub target: String,
    pub status: OpStatus,
    pub started: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Real dispatch correlation. A journal written before this field existed
    /// loads as an empty correlation and keeps its operation/target semantics.
    #[serde(default, skip_serializing_if = "OperationCorrelation::is_none")]
    pub correlation: OperationCorrelation,
}

/// 单次运行的证据记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    pub turn: u64,
    pub started: u64,
    pub state: RunState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<u64>,
    pub operations: Vec<OperationRecord>,
    /// Authoritative canonical tool calls (DES-05). A v1 file has none, and the
    /// recovery report never fabricates a phase for such a file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<ToolCallRecord>,
    /// How many canonical calls were dropped by the bounded limit.
    #[serde(default)]
    pub dropped_calls: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    #[serde(default)]
    pub goal_truncated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_compactions: Vec<crate::compact::ContextCompactionObservation>,
    #[serde(default)]
    pub dropped_context_compactions: u64,
    /// Canonical task identity projected from the authoritative tracker. A
    /// journal written before this field existed loads as `None`; readers keep
    /// it unknown instead of deriving an id from the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Real engine run UUID, attached once by the tracker projection. Distinct
    /// from the journal's own `run_id`; legacy files keep `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_run_id: Option<String>,
    /// Canonical task phase projected from the tracker. Legacy files keep
    /// `None` and never have a phase fabricated for them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_phase: Option<String>,
    /// Real engine run scopes bound to this journal run: the run's own
    /// top-level engine UUID plus every delegated child scope observed under
    /// it. Each entry only ever comes from a real `RunObservation::RunStarted`
    /// (or the tracker's real engine UUID), never from a tool name, target or
    /// completion order. A file written before this field existed loads with an
    /// empty list, so a v1 journal never fabricates a scope.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub engine_scopes: Vec<String>,
}

/// Context audit associated with its durable journal run, which is distinct
/// from the engine observation's run UUID.
#[derive(Debug, Clone, Serialize)]
pub struct RecoveredContextCompaction {
    pub journal_run_id: String,
    pub turn: u64,
    pub observation: crate::compact::ContextCompactionObservation,
}

/// journal 文件的持久化结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalFile {
    pub version: u32,
    pub session_id: String,
    pub updated: u64,
    #[serde(default)]
    pub dropped_runs: u64,
    #[serde(default)]
    pub truncated_operations: u64,
    pub runs: Vec<RunRecord>,
    /// Real engine scopes that were observed but could not be bound to any
    /// journal run. A recovered journal keeps refusing to route them to an
    /// unrelated active run. A v1 file has none and keeps its legacy behaviour.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unbound_scopes: Vec<String>,
}

/// 恢复报告中单条操作。
#[derive(Debug, Clone, Serialize)]
pub struct RecoveredOperation {
    pub run_id: String,
    pub turn: u64,
    pub operation: String,
    pub target: String,
    pub status: OpStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Real dispatch correlation observed for this operation, when the record
    /// carries one. Legacy operations report an empty correlation.
    #[serde(default, skip_serializing_if = "OperationCorrelation::is_none")]
    pub correlation: OperationCorrelation,
    pub recorded_at: u64,
}

/// 恢复报告中单条 canonical 工具调用。
#[derive(Debug, Clone, Serialize)]
pub struct RecoveredToolCall {
    pub run_id: String,
    pub turn: u64,
    pub call_index: u64,
    pub phase: ToolCallPhase,
    pub scope: String,
    pub tool: String,
    pub operation: String,
    /// 真实审批绑定的参数 SHA-256；旧式无绑定记录为空。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub params_sha256: String,
    pub target: String,
    /// `target` 是否为脱敏摘要。
    #[serde(default, skip_serializing_if = "is_false")]
    pub target_redacted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_references: Vec<OutputReference>,
    pub references_truncated: bool,
    pub recorded_at: u64,
}

/// Recorded request for a turn that did not complete. This is historical
/// evidence, not permission to replay the request or its tools.
#[derive(Debug, Clone, Serialize)]
pub struct RecoveredGoal {
    pub run_id: String,
    pub turn: u64,
    pub state: RunState,
    pub goal: Option<String>,
    pub truncated: bool,
}

/// 查询/启动时返回的恢复证据摘要。
#[derive(Debug, Clone, Default, Serialize)]
pub struct RecoveryReport {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal_error: Option<String>,
    pub interrupted_runs: usize,
    pub executed_operations: usize,
    pub executed: Vec<RecoveredOperation>,
    pub failed_operations: Vec<RecoveredOperation>,
    pub unknown_operations: Vec<RecoveredOperation>,
    pub dropped_runs: u64,
    pub truncated_operations: u64,
    /// How many canonical calls the writer dropped from the bounded file.
    #[serde(default)]
    pub dropped_calls: u64,
    /// Canonical calls read from a v2 journal; empty (never fabricated) for v1.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<RecoveredToolCall>,
    pub unfinished_goals: Vec<RecoveredGoal>,
    pub context_compactions: Vec<RecoveredContextCompaction>,
    pub dropped_context_compactions: u64,
}

impl RecoveryReport {
    /// 是否存在需要用户注意的未完成运行或结果未知调用。
    ///
    /// `NotDispatched` 与 `Denied` 都不是未知副作用，绝不计入未完成。
    pub fn has_unfinished(&self) -> bool {
        self.interrupted_runs > 0
            || !self.unknown_operations.is_empty()
            || self.calls.iter().any(|call| {
                matches!(
                    call.phase,
                    ToolCallPhase::Dispatched | ToolCallPhase::Unknown
                )
            })
    }

    /// 单行人类可读摘要（明确声明不重放、不回滚）。
    pub fn summary_line(&self) -> String {
        format!(
            "{} interrupted run(s) and {} operation(s) with unknown results. \
             Mio will not replay these operations; file changes are not rolled back. \
             Inspect evidence with --recovery.",
            self.interrupted_runs,
            self.unknown_operations.len()
        )
    }
}

struct JournalData {
    /// 工作区根，与 `path` 同时确定，用于在落盘区间取 shared 输出租约。
    root: PathBuf,
    path: PathBuf,
    file: JournalFile,
    active: Option<String>,
    load_error: Option<String>,
    /// Real engine run scopes that started but have not reported a real
    /// `RunFinished` in this process. Only a scope in this set is withheld from
    /// finalization; process-crash recovery (`mark_interrupted`) ignores it.
    live_scopes: BTreeSet<String>,
    /// Real engine scopes that were actually observed but whose owner could not
    /// be proven (unknown parent, or a binding persistence failure). Routing
    /// such a scope to the current active run would attach a child's evidence
    /// to an unrelated turn, so they fail closed instead. The set is mirrored
    /// into the journal file so a reopened journal still refuses to guess an
    /// owner; it is cleared only by the scope's real `RunFinished` or by
    /// process-crash recovery.
    unbound_scopes: BTreeSet<String>,
    /// Set once the unbound-scope set could not retain a newly observed scope
    /// (capacity). From then on no scope may fall back to the active run,
    /// because ownership can no longer be proven for every observation. This
    /// is the bounded fail-closed state, never silent evidence loss.
    unbound_overflow: bool,
}

/// 执行证据 journal。`inner == None` 表示禁用（无会话模式），所有写入为空操作。
pub struct Journal {
    inner: Mutex<Option<JournalData>>,
}

impl Journal {
    /// 无会话模式：不创建、不写入任何文件。
    pub fn disabled() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    /// 打开指定会话的 journal。损坏或版本不符的证据保留原样，禁止后续覆盖。
    pub fn open(root: &Path, session_id: &str) -> Self {
        let path = journal_path(root, session_id);
        let (file, load_error) = load_file(&path, session_id);
        Self {
            inner: Mutex::new(Some(JournalData {
                root: root.to_path_buf(),
                path,
                unbound_scopes: file_unbound_scopes(&file),
                file,
                active: None,
                load_error,
                live_scopes: BTreeSet::new(),
                unbound_overflow: false,
            })),
        }
    }

    /// 是否启用持久化。
    pub fn is_enabled(&self) -> bool {
        self.inner.lock().unwrap().is_some()
    }

    /// journal 文件路径（禁用时为 `None`）。
    pub fn path(&self) -> Option<PathBuf> {
        self.inner
            .lock()
            .unwrap()
            .as_ref()
            .map(|data| data.path.clone())
    }

    /// 切换到另一个会话的 journal，并返回其恢复报告。
    pub fn rebind(&self, root: &Path, session_id: &str) -> Option<RecoveryReport> {
        {
            let mut guard = self.inner.lock().unwrap();
            let data = guard.as_mut()?;
            let path = journal_path(root, session_id);
            let (file, load_error) = load_file(&path, session_id);
            data.root = root.to_path_buf();
            data.path = path;
            data.unbound_scopes = file_unbound_scopes(&file);
            data.file = file;
            data.active = None;
            data.load_error = load_error;
            data.live_scopes.clear();
            data.unbound_overflow = false;
        }
        self.mark_interrupted()
    }

    /// 开始一次运行。写入失败时返回 `Err`，调用方应提示并阻止副作用操作。
    pub fn begin_run(&self) -> Result<(), String> {
        self.begin_run_with_goal(None)
    }

    /// Persist the request before the model dispatch, including a visible
    /// truncation marker when its text exceeds the recovery summary budget.
    pub fn begin_run_with_goal(&self, goal: Option<&str>) -> Result<(), String> {
        self.begin_run_with_task(goal, None)
    }

    /// Begin a run that already knows its canonical task identity.
    ///
    /// The legacy `run_id` stays a journal-local id so existing recovery
    /// readers are unchanged; the canonical `task_id` is separate evidence
    /// projected from the authoritative tracker, never derived from the run.
    pub fn begin_run_with_task(
        &self,
        goal: Option<&str>,
        task_id: Option<&str>,
    ) -> Result<(), String> {
        let mut guard = self.inner.lock().unwrap();
        let Some(data) = guard.as_mut() else {
            return Ok(());
        };
        if let Some(active) = data.active.clone() {
            let mut candidate = data.file.clone();
            if let Some(run) = candidate.runs.iter_mut().find(|r| r.run_id == active) {
                // A previous turn that never finalized: withhold any still-live
                // child scope instead of resolving evidence a real child still
                // owns. The binding keeps late child results routed to it.
                let now = now_secs();
                run.state = RunState::Interrupted;
                run.ended = Some(now);
                resolve_run_evidence(
                    run,
                    now,
                    Some(&data.live_scopes),
                    "a later run began before the operation result was recorded",
                );
            }
            let _ = persist(data, &candidate);
        }
        let turn = data.file.runs.iter().map(|r| r.turn).max().unwrap_or(0) + 1;
        let run_id = uuid::Uuid::new_v4().to_string();
        let mut candidate = data.file.clone();
        candidate.runs.push(RunRecord {
            run_id: run_id.clone(),
            turn,
            started: now_secs(),
            state: RunState::Active,
            ended: None,
            operations: Vec::new(),
            calls: Vec::new(),
            dropped_calls: 0,
            goal: goal.map(|goal| truncate(goal, 4096)),
            goal_truncated: goal.is_some_and(|goal| goal.chars().count() > 4096),
            context_compactions: Vec::new(),
            dropped_context_compactions: 0,
            task_id: task_id.map(|task_id| truncate(task_id, MAX_TARGET_CHARS)),
            engine_run_id: None,
            canonical_phase: None,
            engine_scopes: Vec::new(),
        });
        let enforced = persist(data, &candidate)?;
        data.file = enforced;
        data.active = Some(run_id);
        Ok(())
    }

    /// Attach the real engine run UUID and (optionally) the canonical task id
    /// to the active run.
    ///
    /// A projection may arrive before the run is opened (or after it ended), so
    /// having no active run is deliberately a no-op, not a persistence error.
    /// Once an engine run id exists it is set-once: a different value is
    /// refused rather than silently rewriting the run's identity.
    pub fn attach_run_identity(
        &self,
        engine_run_id: &str,
        task_id: Option<&str>,
    ) -> Result<(), String> {
        let mut guard = self.inner.lock().unwrap();
        let Some(data) = guard.as_mut() else {
            return Ok(());
        };
        let Some(run_id) = current_run(data) else {
            return Ok(());
        };
        let mut candidate = data.file.clone();
        let Some(run) = candidate.runs.iter_mut().find(|r| r.run_id == run_id) else {
            return Ok(());
        };
        if let Some(task_id) = task_id {
            let task_id = truncate(task_id, MAX_TARGET_CHARS);
            if run.task_id.as_deref() != Some(task_id.as_str()) {
                run.task_id = Some(task_id);
            }
        }
        let existing = run.engine_run_id.take();
        match existing {
            Some(existing) if existing == engine_run_id => {
                run.engine_run_id = Some(existing);
            }
            Some(_) => {
                return Err("the active run already has a different real engine run id".into());
            }
            None => run.engine_run_id = Some(truncate(engine_run_id, MAX_TARGET_CHARS)),
        }
        data.file = persist(data, &candidate)?;
        Ok(())
    }

    /// Project the canonical task phase onto the active run without touching
    /// the legacy `state`. No active run is a no-op; an unknown phase is
    /// refused so no fabricated phase can enter durable evidence.
    pub fn set_canonical_phase(&self, phase: &str) -> Result<(), String> {
        if !is_canonical_phase(phase) {
            return Err(format!("'{phase}' is not a canonical task phase"));
        }
        let mut guard = self.inner.lock().unwrap();
        let Some(data) = guard.as_mut() else {
            return Ok(());
        };
        let Some(run_id) = current_run(data) else {
            return Ok(());
        };
        let mut candidate = data.file.clone();
        let Some(run) = candidate.runs.iter_mut().find(|r| r.run_id == run_id) else {
            return Ok(());
        };
        if run.canonical_phase.as_deref() != Some(phase) {
            run.canonical_phase = Some(phase.to_owned());
            data.file = persist(data, &candidate)?;
        }
        Ok(())
    }

    /// Project a canonical task phase (and set-once engine run UUID) onto the
    /// journal run that already owns the real task id.
    ///
    /// This is the CLI/DSK adapter's projection entry: the authoritative task
    /// tracker is the single writer of phase, and this method only ever updates
    /// the run whose `task_id` matches. A queued sibling task whose run has not
    /// been opened yet is left unknown instead of having its phase written onto
    /// whatever run happens to be active (which would pollute another turn).
    /// The engine UUID is set-once: a different real id is refused, never
    /// silently replaced.
    pub fn project_task_canonical(
        &self,
        task_id: &str,
        engine_run_id: Option<&str>,
        phase: &str,
    ) -> Result<(), String> {
        if !is_canonical_phase(phase) {
            return Err(format!("'{phase}' is not a canonical task phase"));
        }
        let mut guard = self.inner.lock().unwrap();
        let Some(data) = guard.as_mut() else {
            return Ok(());
        };
        // Route strictly by the real canonical task id. Having no matching run
        // (a task queued before its turn opens) is an honest no-op.
        let Some(run_id) = data
            .file
            .runs
            .iter()
            .rev()
            .find(|run| run.task_id.as_deref() == Some(task_id))
            .map(|run| run.run_id.clone())
        else {
            return Ok(());
        };
        let mut candidate = data.file.clone();
        let Some(run) = candidate.runs.iter_mut().find(|r| r.run_id == run_id) else {
            return Ok(());
        };
        if let Some(engine_run_id) = engine_run_id {
            let engine_run_id = truncate(engine_run_id, MAX_TARGET_CHARS);
            match run.engine_run_id.as_deref() {
                Some(existing) if existing == engine_run_id => {}
                Some(_) => {
                    return Err("the journal run already has a different real engine run id".into());
                }
                None => run.engine_run_id = Some(engine_run_id.clone()),
            }
            if !run
                .engine_scopes
                .iter()
                .any(|scope| scope == &engine_run_id)
            {
                if run.engine_scopes.len() >= MAX_RUN_SCOPES {
                    return Err(
                        "the journal run cannot bind more real engine scopes; refusing to drop live scope evidence"
                            .into(),
                    );
                }
                run.engine_scopes.push(engine_run_id);
            }
        }
        if run.canonical_phase.as_deref() != Some(phase) {
            run.canonical_phase = Some(phase.to_owned());
        }
        data.file = persist(data, &candidate)?;
        Ok(())
    }

    /// Record one real engine run lifecycle observation and bind its scope to
    /// the journal run that really owns it.
    ///
    /// This is the public scope-observation entry an SDK [`Host`](crate::Host)
    /// can consume exactly like the CLI adapter does; the CLI is only one
    /// caller. A `RunStarted` with a real `parent_run_id` binds the child scope
    /// to the run that owns the parent scope, even when another top turn is
    /// active or no top turn is active at all. A `RunFinished` finalizes only
    /// that scope's still-pending evidence: a live sibling scope is never
    /// touched. Nothing here infers an owner from a tool name, target or order.
    pub fn record_run_observation(
        &self,
        observation: &crate::RunObservation,
    ) -> Result<(), String> {
        use crate::run_observation::RunObservationKind;
        let scope = observation.run_id.to_string();
        let mut guard = self.inner.lock().unwrap();
        let Some(data) = guard.as_mut() else {
            return Ok(());
        };
        match observation.kind {
            RunObservationKind::RunStarted => {
                let owner = match observation.parent_run_id {
                    Some(parent) => scope_owner(data, &parent.to_string()),
                    None => current_run(data),
                };
                // An unknown parent or no active run cannot own this scope. A
                // real observation must never be guessed onto an unrelated
                // run: remember the scope so every later canonical dispatch for
                // it fails closed, and report the binding failure now.
                let Some(owner) = owner else {
                    remember_unbound_scope(data, &scope)?;
                    return Err(format!(
                        "run scope {scope} has no provable owner; refusing to bind it to an unrelated run"
                    ));
                };
                let mut candidate = data.file.clone();
                let Some(run) = candidate.runs.iter_mut().find(|r| r.run_id == owner) else {
                    remember_unbound_scope(data, &scope)?;
                    return Err(format!(
                        "run scope {scope} lost its owning run; refusing to bind it elsewhere"
                    ));
                };
                if run.engine_run_id.is_none() {
                    run.engine_run_id = Some(truncate(&scope, MAX_TARGET_CHARS));
                }
                if !run.engine_scopes.iter().any(|bound| bound == &scope) {
                    if run.engine_scopes.len() >= MAX_RUN_SCOPES {
                        remember_unbound_scope(data, &scope)?;
                        return Err(
                            "the journal run cannot bind more real engine scopes; refusing to drop live scope evidence"
                                .into(),
                        );
                    }
                    run.engine_scopes.push(truncate(&scope, MAX_TARGET_CHARS));
                }
                let enforced = match persist(data, &candidate) {
                    Ok(enforced) => enforced,
                    Err(error) => {
                        // The binding did not become durable: keep the scope
                        // fail-closed so a later dispatch cannot silently fall
                        // back to whichever run is active then.
                        let _ = remember_unbound_scope(data, &scope);
                        return Err(error);
                    }
                };
                data.file = enforced;
                data.live_scopes.insert(scope.clone());
                data.unbound_scopes.remove(&scope);
                Ok(())
            }
            RunObservationKind::RunFinished => {
                data.live_scopes.remove(&scope);
                // The scope really ended, so its fail-closed routing guard is
                // no longer needed; write the cleared set back when it changed.
                let cleared_unbound = data.unbound_scopes.remove(&scope);
                let Some(owner) = scope_owner(data, &scope) else {
                    if cleared_unbound {
                        let candidate = data.file.clone();
                        data.file = persist(data, &candidate)?;
                    }
                    return Ok(());
                };
                let mut candidate = data.file.clone();
                let Some(run) = candidate.runs.iter_mut().find(|r| r.run_id == owner) else {
                    return Ok(());
                };
                let now = now_secs();
                for op in run.operations.iter_mut() {
                    if op.status == OpStatus::Pending
                        && op.correlation.scope.as_deref() == Some(scope.as_str())
                    {
                        op.status = OpStatus::Unknown;
                        op.finished = Some(now);
                        op.detail = Some("result not recorded before the run ended".into());
                    }
                }
                for call in run.calls.iter_mut() {
                    if call.scope == scope {
                        resolve_canonical(call, now);
                    }
                }
                let enforced = persist(data, &candidate)?;
                data.file = enforced;
                Ok(())
            }
            // Prepared/dispatched/outcome observations carry no scope binding.
            _ => Ok(()),
        }
    }

    /// Persist a bounded metadata-only context audit before its request is
    /// dispatched. A failure must be propagated instead of silently losing it.
    pub fn record_context_compaction(
        &self,
        observation: &crate::compact::ContextCompactionObservation,
    ) -> Result<(), String> {
        let mut guard = self.inner.lock().unwrap();
        let Some(data) = guard.as_mut() else {
            return Ok(());
        };
        let run_id =
            current_run(data).ok_or("Execution journal has no active run for context audit")?;
        let mut candidate = data.file.clone();
        let run = candidate
            .runs
            .iter_mut()
            .find(|run| run.run_id == run_id)
            .ok_or("Execution journal lost the active context audit run")?;
        const MAX_AUDITS: usize = 4;
        if run.context_compactions.len() >= MAX_AUDITS {
            run.context_compactions.remove(0);
            run.dropped_context_compactions = run.dropped_context_compactions.saturating_add(1);
        }
        run.context_compactions.push(observation.clone());
        data.file = persist(data, &candidate)?;
        Ok(())
    }

    /// 在副作用发生之前持久化 intent。失败时返回 `Err`，调用方不得继续执行。
    ///
    /// Legacy entry point: the record carries operation/target only and is
    /// completed only by a result that also has no real dispatch identity.
    pub fn record_intent(&self, operation: &str, target: &str) -> Result<(), String> {
        self.record_intent_with(operation, target, OperationCorrelation::default())
    }

    /// Persist an intent pinned to the real approval binding before the side
    /// effect runs (DES-05 correlation).
    ///
    /// The record carries the binding's real run scope, the provider's
    /// tool-call id and the approval attempt id, so concurrent calls to the
    /// same tool with the same target are told apart by their own dispatch
    /// identity instead of by completion order. A binding cannot know a turn or
    /// effect id, so both stay `None` until the real result is recorded. A write
    /// failure returns `Err` and the caller must not execute the operation; a
    /// disabled (`--no-session`) journal still writes nothing.
    pub fn record_bound_intent(
        &self,
        operation: &str,
        target: &str,
        binding: &crate::policy::ApprovalBinding,
    ) -> Result<(), String> {
        // Identity is compared exactly, unlike display text. Truncating two
        // different ids to one prefix could complete the wrong operation.
        if self.is_enabled() {
            for (name, value) in [
                ("scope", Some(binding.scope.as_str())),
                ("call_id", Some(binding.call_id.as_str())),
                ("tool_call_id", binding.tool_call_id.as_deref()),
            ] {
                if let Some(value) = value
                    && (value.trim().is_empty()
                        || value.len() > MAX_TARGET_CHARS
                        || value.chars().any(char::is_control))
                {
                    return Err(format!(
                        "invalid journal correlation {name}; refusing operation"
                    ));
                }
            }
        }
        self.record_intent_with(
            operation,
            target,
            OperationCorrelation::from_binding(binding),
        )
    }

    fn record_intent_with(
        &self,
        operation: &str,
        target: &str,
        correlation: OperationCorrelation,
    ) -> Result<(), String> {
        let mut guard = self.inner.lock().unwrap();
        let Some(data) = guard.as_mut() else {
            return Ok(());
        };
        let Some(run_id) = current_run(data) else {
            return Err(
                "Execution journal has no active run; refusing side-effecting operation".into(),
            );
        };
        let operation = truncate(operation, MAX_TARGET_CHARS);
        let target = truncate(target, MAX_TARGET_CHARS);
        let mut candidate = data.file.clone();
        let Some(run) = candidate.runs.iter_mut().find(|r| r.run_id == run_id) else {
            return Err("Execution journal lost the active run; refusing operation".into());
        };
        if run.operations.len() >= MAX_RUN_OPERATIONS {
            run.operations.remove(0);
            candidate.truncated_operations += 1;
        }
        let op_id = run.operations.last().map(|op| op.op_id).unwrap_or(0) + 1;
        run.operations.push(OperationRecord {
            op_id,
            operation,
            target,
            status: OpStatus::Pending,
            started: now_secs(),
            finished: None,
            detail: None,
            correlation,
        });
        let enforced = persist(data, &candidate)?;
        data.file = enforced;
        Ok(())
    }

    /// 记录副作用操作的结果。匹配不到 intent 时静默忽略（纯读取操作不产生证据）。
    ///
    /// The match is driven by the real dispatch identity the engine wrappers
    /// bind to the executing task. When that identity exists, only a record
    /// with the same real scope and provider tool-call id is completed, and its
    /// real turn/effect id are filled in. A result without a real identity (a
    /// direct, engine-less call) may only complete a legacy record that carries
    /// no binding at all, so an operation/target coincidence can never complete
    /// another call that is already pinned to its own dispatch. Records already
    /// in a terminal state are never revisited, so a repeated result is a
    /// no-op rather than a regression.
    pub fn record_result(
        &self,
        operation: &str,
        target: &str,
        result: &Result<String, String>,
    ) -> Result<(), String> {
        let (status, detail) = match result {
            Ok(output) => (OpStatus::Executed, truncate(output, MAX_DETAIL_CHARS)),
            Err(error) => (OpStatus::Failed, truncate(error, MAX_DETAIL_CHARS)),
        };
        self.record_terminal(operation, target, status, detail)
    }

    /// Record that a dispatched call's result could not be confirmed (DES-09).
    ///
    /// This is the only way an intent becomes `Unknown` **before** the run
    /// ends: a real transport death or a supervision timeout is recorded as
    /// unknown, never as `Failed`, and the reason is a typed marker the caller
    /// already classified. Matching is identical to [`Self::record_result`]: a
    /// result with real dispatch identity completes only its own pending,
    /// exactly-correlated record, and a record already in a terminal state is
    /// never revisited. A persistence failure returns `Err` so the caller can
    /// fail closed and refuse later side effects.
    pub fn record_unknown_result(
        &self,
        operation: &str,
        target: &str,
        detail: &str,
    ) -> Result<(), String> {
        self.record_terminal(
            operation,
            target,
            OpStatus::Unknown,
            truncate(detail, MAX_DETAIL_CHARS),
        )
    }

    /// The shared completion path for [`Self::record_result`] and
    /// [`Self::record_unknown_result`].
    fn record_terminal(
        &self,
        operation: &str,
        target: &str,
        status: OpStatus,
        detail: String,
    ) -> Result<(), String> {
        let identity = crate::tool_identity::current();
        let scope = identity
            .as_ref()
            .and_then(|identity| bounded(&identity.run_id().to_string()));
        let mut guard = self.inner.lock().unwrap();
        let Some(data) = guard.as_mut() else {
            return Ok(());
        };
        let run_id = match scope.as_deref() {
            Some(scope) => route_run(data, scope),
            None => current_run(data),
        };
        let Some(run_id) = run_id else {
            return Ok(());
        };
        let operation = truncate(operation, MAX_TARGET_CHARS);
        let target = truncate(target, MAX_TARGET_CHARS);
        let tool_call_id = identity
            .as_ref()
            .and_then(|identity| bounded(identity.tool_call_id()));
        let mut candidate = data.file.clone();
        let Some(run) = candidate.runs.iter_mut().find(|r| r.run_id == run_id) else {
            return Ok(());
        };
        let Some(index) = run.operations.iter().rposition(|op| {
            if op.status != OpStatus::Pending {
                return false;
            }
            match (scope.as_deref(), tool_call_id.as_deref()) {
                (Some(scope), Some(tool_call_id)) => {
                    op.correlation.scope.as_deref() == Some(scope)
                        && op.correlation.tool_call_id.as_deref() == Some(tool_call_id)
                }
                // No real identity: only an unbound legacy record is a match.
                // A bound record has its own dispatch and must not be completed
                // by an operation/target coincidence.
                _ => !op.correlation.is_bound() && op.operation == operation && op.target == target,
            }
        }) else {
            return Ok(());
        };
        let op = &mut run.operations[index];
        op.status = status;
        op.finished = Some(now_secs());
        op.detail = if detail.is_empty() {
            None
        } else {
            Some(detail)
        };
        if let Some(identity) = &identity {
            // The approval binding cannot know these; the real dispatch can.
            op.correlation.turn = identity.turn_id();
            op.correlation.effect_id = Some(identity.effect_id());
        }
        let enforced = persist(data, &candidate)?;
        data.file = enforced;
        Ok(())
    }

    /// Persist one authoritative canonical tool-call transition (DES-05).
    ///
    /// The record is matched by `(scope, call_index)` inside the active run. A
    /// new call is inserted (its `Normalized` record); an existing one merges
    /// identity and bounded metadata and applies the transition. A terminal
    /// phase is never regressed: if two independent writers race (the host's
    /// `Unknown` and the engine's `Failed`, say), the first terminal wins and
    /// the later write is an idempotent no-op. A persistence failure returns
    /// `Err`, so the caller refuses the dispatch instead of pretending success.
    /// A disabled (`--no-session`) journal writes nothing and reports `Ok`.
    pub fn record_canonical_phase(&self, record: &ToolCallRecord) -> Result<(), String> {
        let mut guard = self.inner.lock().unwrap();
        let Some(data) = guard.as_mut() else {
            return Ok(());
        };
        let Some(run_id) = route_run(data, &record.scope) else {
            return Err(
                "Execution journal has no active run; refusing canonical tool call evidence".into(),
            );
        };
        let incoming = bounded_call(record);
        let mut candidate = data.file.clone();
        let run = candidate
            .runs
            .iter_mut()
            .find(|run| run.run_id == run_id)
            .ok_or(
                "Execution journal lost the active run; refusing canonical tool call evidence",
            )?;
        let position = run.calls.iter().position(|call| {
            call.scope == incoming.scope && call.call_index == incoming.call_index
        });
        match position {
            Some(position) => merge_canonical(&mut run.calls[position], &incoming)?,
            None => {
                // Capacity is enforced only by `enforce_limits`, which refuses
                // to drop a canonical call that belongs to a live child scope.
                // Trimming the oldest call here would silently delete live
                // evidence and let the write slip past that protection.
                run.calls.push(incoming);
            }
        }
        let enforced = persist(data, &candidate)?;
        data.file = enforced;
        Ok(())
    }

    /// Attach the real, bounded result detail and committed output references
    /// to an already-recorded call without ever changing its phase.
    ///
    /// This is the engine-less entry the host calls from `on_tool_result`; it
    /// never turns a `Denied` or `Unknown` call into `Failed` and never
    /// regresses a terminal phase. A missing record is a bounded no-op because
    /// the bounded journal may have dropped it; the drop is itself reported via
    /// `dropped_calls`.
    pub fn record_canonical_detail(
        &self,
        scope: &str,
        tool_call_id: Option<&str>,
        detail: &str,
        references: &[OutputReference],
    ) -> Result<(), String> {
        let Some(tool_call_id) = tool_call_id else {
            return Ok(());
        };
        let mut guard = self.inner.lock().unwrap();
        let Some(data) = guard.as_mut() else {
            return Ok(());
        };
        let Some(run_id) = route_run(data, scope) else {
            return Ok(());
        };
        let mut candidate = data.file.clone();
        let Some(run) = candidate.runs.iter_mut().find(|run| run.run_id == run_id) else {
            return Ok(());
        };
        let Some(call) = run
            .calls
            .iter_mut()
            .find(|call| call.scope == scope && call.tool_call_id.as_deref() == Some(tool_call_id))
        else {
            return Ok(());
        };
        if call.phase != ToolCallPhase::Denied {
            call.detail = Some(truncate(detail, MAX_DETAIL_CHARS));
        }
        if !references.is_empty() {
            call.references_truncated |= references.len() > MAX_CALL_REFERENCES;
            call.output_references = bounded_references(references);
        }
        call.updated = now_secs();
        let enforced = persist(data, &candidate)?;
        data.file = enforced;
        Ok(())
    }

    /// Record a dispatched call whose side effect could not be confirmed.
    ///
    /// This is the durable `Unknown` writer the plugin/MCP unknown path calls
    /// before the tool-finished event. A missing matching call under an active
    /// run returns `Err` so the caller's unknown-effect fence marks the failure
    /// and refuses later side effects; a disabled journal is a no-op `Ok`.
    pub fn record_canonical_unknown(
        &self,
        scope: &str,
        tool_call_id: Option<&str>,
        detail: &str,
    ) -> Result<(), String> {
        let mut guard = self.inner.lock().unwrap();
        let Some(data) = guard.as_mut() else {
            return Ok(());
        };
        let Some(run_id) = route_run(data, scope) else {
            return Err(
                "Execution journal has no active run; refusing unknown-effect evidence".into(),
            );
        };
        let mut candidate = data.file.clone();
        let run = candidate
            .runs
            .iter_mut()
            .find(|run| run.run_id == run_id)
            .ok_or("Execution journal lost the active run; refusing unknown-effect evidence")?;
        let Some(call) = run.calls.iter_mut().find(|call| {
            call.scope == scope
                && match (call.tool_call_id.as_deref(), tool_call_id) {
                    (Some(known), Some(current)) => known == current,
                    _ => false,
                }
        }) else {
            return Err(format!(
                "no canonical tool call matches the unknown side effect (scope {scope})"
            ));
        };
        if call.phase == ToolCallPhase::Dispatched {
            call.phase = ToolCallPhase::Unknown;
        }
        if call.detail.is_none() {
            call.detail = Some(truncate(detail, MAX_DETAIL_CHARS));
        }
        call.updated = now_secs();
        let enforced = persist(data, &candidate)?;
        data.file = enforced;
        Ok(())
    }

    /// 结束当前运行。仍处于 `Pending` 的操作一律标记为 `Unknown`。
    pub fn finish_run(&self, state: RunState) -> Result<(), String> {
        let mut guard = self.inner.lock().unwrap();
        let Some(data) = guard.as_mut() else {
            return Ok(());
        };
        let Some(run_id) = current_run(data) else {
            return Ok(());
        };
        let mut candidate = data.file.clone();
        let Some(run) = candidate.runs.iter_mut().find(|r| r.run_id == run_id) else {
            return Ok(());
        };
        // Only the run this instance started is still Active here. A terminal
        // run is never rewritten, which keeps finish idempotent and prevents a
        // late finish from rolling a terminal state back.
        if run.state != RunState::Active {
            data.active = None;
            return Ok(());
        }
        let now = now_secs();
        // Only finalize evidence whose real scope ended. A live child scope that
        // outlives this top turn keeps its real `Dispatched` evidence and is
        // finalized later by its own `RunFinished` (or by crash recovery).
        resolve_run_evidence(
            run,
            now,
            Some(&data.live_scopes),
            "result not recorded before the run ended",
        );
        run.state = state;
        run.ended = Some(now);
        let enforced = persist(data, &candidate)?;
        data.file = enforced;
        data.active = None;
        Ok(())
    }

    /// 将上次进程遗留的 `Active` 运行标记为 `Interrupted`，并返回恢复报告。
    pub fn mark_interrupted(&self) -> Option<RecoveryReport> {
        let mut guard = self.inner.lock().unwrap();
        let data = guard.as_mut()?;
        let mut candidate = data.file.clone();
        let mut changed = false;
        for run in candidate.runs.iter_mut() {
            if run.state == RunState::Active {
                interrupt_run(run);
                changed = true;
            }
        }
        // Process-crash recovery ends every real scope, so the unbound-scope
        // guard is cleared too (and the cleared set is persisted).
        if !data.unbound_scopes.is_empty() || data.unbound_overflow {
            data.unbound_scopes.clear();
            data.unbound_overflow = false;
            changed = true;
        }
        if changed && let Ok(enforced) = persist(data, &candidate) {
            data.file = enforced;
        }
        data.active = None;
        data.live_scopes.clear();
        Some(build_report(&data.file, data.load_error.clone()))
    }

    /// 只读查询当前恢复证据，不修改文件。
    pub fn recovery_report(&self) -> Option<RecoveryReport> {
        let guard = self.inner.lock().unwrap();
        let data = guard.as_ref()?;
        Some(build_report(&data.file, data.load_error.clone()))
    }
}

/// 依据根目录与会话 ID 计算 journal 路径。
pub fn journal_path(root: &Path, session_id: &str) -> PathBuf {
    root.join(JOURNAL_DIR)
        .join(format!("{}.json", sanitize(session_id)))
}

fn current_run(data: &JournalData) -> Option<String> {
    // An on-disk Active record may belong to a crashed or another process.
    // Only a successfully persisted begin_run in this instance grants access.
    data.active.clone()
}

/// The unbound scopes a journal file carries, as an ordered set.
fn file_unbound_scopes(file: &JournalFile) -> BTreeSet<String> {
    file.unbound_scopes.iter().cloned().collect()
}

/// Remember a real, observed scope whose owner could not be proven.
///
/// The set is bounded by the same real-scope limit the journal uses. On
/// overflow the journal latches a fail-closed state instead of forgetting the
/// scope (which would let a later dispatch fall back to the active run).
fn remember_unbound_scope(data: &mut JournalData, scope: &str) -> Result<(), String> {
    if data.unbound_scopes.contains(scope) {
        return Ok(());
    }
    if data.unbound_scopes.len() >= MAX_RUN_SCOPES {
        data.unbound_overflow = true;
        return Err(
            "too many observed scopes without a provable owner; refusing to route evidence to an unrelated run"
                .into(),
        );
    }
    data.unbound_scopes.insert(scope.to_owned());
    Ok(())
}

/// The journal run a real engine scope is bound to, if any.
///
/// A scope is bound only by a real `RunStarted`/tracker projection; this never
/// matches on the tool name, target or completion order.
fn scope_owner(data: &JournalData, scope: &str) -> Option<String> {
    data.file.runs.iter().rev().find_map(|run| {
        if run.engine_run_id.as_deref() == Some(scope)
            || run.engine_scopes.iter().any(|bound| bound == scope)
        {
            Some(run.run_id.clone())
        } else {
            None
        }
    })
}

/// Route a real engine scope to its owning journal run.
///
/// A bound scope always routes to its real owner, even when another top turn is
/// active or no top turn is active at all. A scope that was really observed but
/// could not be bound must fail closed: guessing that an unrelated active run
/// owns it would attach a child's evidence to the wrong turn. Only a scope this
/// instance never observed keeps the legacy single-active-run fallback, so an
/// engine-less writer's evidence still lands in the run it started.
fn route_run(data: &JournalData, scope: &str) -> Option<String> {
    if let Some(owner) = scope_owner(data, scope) {
        return Some(owner);
    }
    if data.unbound_overflow || data.unbound_scopes.contains(scope) {
        return None;
    }
    current_run(data)
}

fn interrupt_run(run: &mut RunRecord) {
    let now = now_secs();
    run.state = RunState::Interrupted;
    run.ended = Some(now);
    // A real process crash ends every scope, so crash recovery finalizes all
    // outstanding evidence regardless of the in-process live set.
    resolve_run_evidence(
        run,
        now,
        None,
        "process ended before the operation result was recorded",
    );
}

/// Resolve a run's outstanding evidence, withholding any scope in `live`.
///
/// `live` is `None` for a real process crash, where no scope can still be
/// running. A live scope keeps its real non-terminal phase so a later real
/// result is not falsely reported as `Unknown`.
fn resolve_run_evidence(
    run: &mut RunRecord,
    now: u64,
    live: Option<&BTreeSet<String>>,
    op_detail: &str,
) {
    let is_live = |scope: Option<&str>| match (live, scope) {
        (Some(live), Some(scope)) => live.contains(scope),
        _ => false,
    };
    for op in run.operations.iter_mut() {
        if op.status == OpStatus::Pending && !is_live(op.correlation.scope.as_deref()) {
            op.status = OpStatus::Unknown;
            op.finished = Some(now);
            op.detail = Some(op_detail.to_owned());
        }
    }
    for call in run.calls.iter_mut() {
        if !is_live(Some(call.scope.as_str())) {
            resolve_canonical(call, now);
        }
    }
}

/// Resolve a call left non-terminal by an interrupted or finished run.
///
/// A dispatched call with no confirmed result becomes `Unknown`; a call that
/// was normalized or admitted but never dispatched becomes `NotDispatched`
/// (explicitly not an unknown side effect). A call already in a terminal phase
/// is left exactly as it is: a denial is never rewritten into `Unknown`.
fn resolve_canonical(call: &mut ToolCallRecord, now: u64) {
    let detail = match call.phase {
        ToolCallPhase::Dispatched => {
            call.phase = ToolCallPhase::Unknown;
            "result not recorded before the run ended"
        }
        ToolCallPhase::Normalized | ToolCallPhase::Admitted => {
            call.phase = ToolCallPhase::NotDispatched;
            "interrupted before dispatch"
        }
        _ => return,
    };
    call.updated = now;
    if call.detail.is_none() {
        call.detail = Some(detail.to_owned());
    }
}

fn build_report(file: &JournalFile, load_error: Option<String>) -> RecoveryReport {
    let mut report = RecoveryReport {
        session_id: Some(file.session_id.clone()),
        journal_error: load_error,
        dropped_runs: file.dropped_runs,
        truncated_operations: file.truncated_operations,
        ..RecoveryReport::default()
    };
    for run in &file.runs {
        report.dropped_calls = report.dropped_calls.saturating_add(run.dropped_calls);
        report.dropped_context_compactions = report
            .dropped_context_compactions
            .saturating_add(run.dropped_context_compactions);
        report
            .context_compactions
            .extend(run.context_compactions.iter().cloned().map(|observation| {
                RecoveredContextCompaction {
                    journal_run_id: run.run_id.clone(),
                    turn: run.turn,
                    observation,
                }
            }));
        if run.state != RunState::Completed {
            report.unfinished_goals.push(RecoveredGoal {
                run_id: run.run_id.clone(),
                turn: run.turn,
                state: run.state,
                goal: run.goal.clone(),
                truncated: run.goal_truncated,
            });
        }
        if matches!(run.state, RunState::Active | RunState::Interrupted) {
            report.interrupted_runs += 1;
        }
        for op in &run.operations {
            match op.status {
                OpStatus::Pending | OpStatus::Unknown => {
                    report.unknown_operations.push(recovered(run, op));
                }
                OpStatus::Failed => report.failed_operations.push(recovered(run, op)),
                OpStatus::Executed => {
                    report.executed_operations += 1;
                    report.executed.push(recovered(run, op));
                }
            }
        }
        for call in &run.calls {
            report.calls.push(RecoveredToolCall {
                run_id: run.run_id.clone(),
                turn: run.turn,
                call_index: call.call_index,
                phase: call.phase,
                scope: call.scope.clone(),
                tool: call.tool.clone(),
                operation: call.operation.clone(),
                params_sha256: call.params_sha256.clone(),
                target: call.target.clone(),
                target_redacted: call.target_redacted,
                tool_call_id: call.tool_call_id.clone(),
                call_id: call.call_id.clone(),
                detail: call.detail.clone(),
                output_references: call.output_references.clone(),
                references_truncated: call.references_truncated,
                recorded_at: call.updated,
            });
        }
    }
    report
}

/// The most committed output references one canonical call keeps.
const MAX_CALL_REFERENCES: usize = crate::tool_identity::MAX_OUTPUT_REFERENCES_PER_CALL;

/// Bound a canonical call's textual fields and reference list.
fn bounded_call(record: &ToolCallRecord) -> ToolCallRecord {
    let mut bounded = record.clone();
    bounded.tool = truncate(&bounded.tool, MAX_TARGET_CHARS);
    bounded.operation = truncate(&bounded.operation, MAX_TARGET_CHARS);
    bounded.target = truncate(&bounded.target, MAX_TARGET_CHARS);
    if let Some(detail) = &bounded.detail {
        bounded.detail = Some(truncate(detail, MAX_DETAIL_CHARS));
    }
    if bounded.output_references.len() > MAX_CALL_REFERENCES {
        bounded.output_references.truncate(MAX_CALL_REFERENCES);
        bounded.references_truncated = true;
    }
    for reference in &mut bounded.output_references {
        if let Some(call_id) = &reference.call_id {
            reference.call_id = bounded_opt(call_id);
        }
    }
    bounded
}

/// Bound a batch of output references to the per-call limit.
fn bounded_references(references: &[OutputReference]) -> Vec<OutputReference> {
    references
        .iter()
        .take(MAX_CALL_REFERENCES)
        .cloned()
        .map(|mut reference| {
            if let Some(call_id) = &reference.call_id {
                reference.call_id = bounded_opt(call_id);
            }
            reference
        })
        .collect()
}

fn bounded_opt(value: &str) -> Option<String> {
    if value.is_empty() {
        return None;
    }
    Some(truncate(value, MAX_TARGET_CHARS))
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Merge an incoming canonical phase into an existing record without ever
/// regressing or rewriting a terminal state.
fn merge_canonical(existing: &mut ToolCallRecord, incoming: &ToolCallRecord) -> Result<(), String> {
    if existing.phase != incoming.phase {
        if existing.phase.is_terminal() {
            // Two independent writers can race (host `Unknown` vs engine
            // `Failed`); the first terminal state stands and the later write is
            // only an idempotent merge of any metadata it carries.
            merge_canonical_metadata(existing, incoming);
            return Ok(());
        }
        if !ToolCallPhase::transition_allowed(existing.phase, incoming.phase) {
            return Err(format!(
                "illegal canonical tool call transition {} -> {}",
                existing.phase.as_str(),
                incoming.phase.as_str()
            ));
        }
        existing.phase = incoming.phase;
    }
    merge_canonical_metadata(existing, incoming);
    Ok(())
}

fn merge_canonical_metadata(existing: &mut ToolCallRecord, incoming: &ToolCallRecord) {
    // The approval digest is set once, from the real binding, and never
    // rewritten by a later state writer.
    if existing.params_sha256.is_empty() && !incoming.params_sha256.is_empty() {
        existing.params_sha256 = incoming.params_sha256.clone();
    }
    existing.target_redacted |= incoming.target_redacted;
    if incoming.tool_call_id.is_some() {
        existing.tool_call_id = incoming.tool_call_id.clone();
    }
    if incoming.call_id.is_some() {
        existing.call_id = incoming.call_id.clone();
    }
    if incoming.turn.is_some() {
        existing.turn = incoming.turn;
    }
    if incoming.effect_id.is_some() {
        existing.effect_id = incoming.effect_id;
    }
    if incoming.detail.is_some() {
        existing.detail = incoming.detail.clone();
    }
    if !incoming.output_references.is_empty() {
        existing.output_references = incoming.output_references.clone();
    }
    existing.references_truncated |= incoming.references_truncated;
    existing.updated = now_secs();
}

fn recovered(run: &RunRecord, op: &OperationRecord) -> RecoveredOperation {
    RecoveredOperation {
        run_id: run.run_id.clone(),
        turn: run.turn,
        operation: op.operation.clone(),
        target: op.target.clone(),
        status: op.status,
        detail: op.detail.clone(),
        correlation: op.correlation.clone(),
        recorded_at: op.finished.unwrap_or(op.started),
    }
}

fn load_file(path: &Path, session_id: &str) -> (JournalFile, Option<String>) {
    let fresh = || JournalFile {
        version: JOURNAL_VERSION,
        session_id: session_id.to_owned(),
        updated: now_secs(),
        dropped_runs: 0,
        truncated_operations: 0,
        runs: Vec::new(),
        unbound_scopes: Vec::new(),
    };
    let bytes = (|| -> std::io::Result<Vec<u8>> {
        validate_locations(path, false).map_err(std::io::Error::other)?;
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file() || metadata.len() > MAX_JOURNAL_FILE_SIZE as u64 {
            return Err(std::io::Error::other(
                "journal must be a regular file within its size limit",
            ));
        }
        let mut bytes = Vec::new();
        fs::File::open(path)?
            .take((MAX_JOURNAL_FILE_SIZE + 1) as u64)
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    })();
    match bytes {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (fresh(), None),
        Err(error) => (
            fresh(),
            Some(format!(
                "cannot read execution journal {}: {error}",
                path.display()
            )),
        ),
        Ok(bytes) if bytes.len() > MAX_JOURNAL_FILE_SIZE => (
            fresh(),
            Some(format!(
                "execution journal {} exceeds {MAX_JOURNAL_FILE_SIZE} bytes",
                path.display()
            )),
        ),
        Ok(bytes) => match serde_json::from_slice::<JournalFile>(&bytes) {
            Ok(file)
                if matches!(file.version, JOURNAL_VERSION | JOURNAL_VERSION_V1)
                    && file.session_id == session_id =>
            {
                // A v1 file is read faithfully (empty `calls`, its legacy
                // `operations` intact). The first real write upgrades it.
                (file, None)
            }
            Ok(file) if file.session_id != session_id => (
                fresh(),
                Some("execution journal session identity does not match".into()),
            ),
            Ok(file) => (
                fresh(),
                Some(format!(
                    "unsupported execution journal version {} (expected {JOURNAL_VERSION})",
                    file.version
                )),
            ),
            Err(error) => (
                fresh(),
                Some(format!(
                    "corrupt execution journal {}: {error}",
                    path.display()
                )),
            ),
        },
    }
}

fn persist(data: &JournalData, candidate: &JournalFile) -> Result<JournalFile, String> {
    if let Some(error) = &data.load_error {
        return Err(format!(
            "Execution journal requires inspection; existing evidence was preserved: {error}"
        ));
    }
    // Hold the shared output lease for the whole candidate-to-disk interval.
    // When `.mio/outputs` does not exist this creates nothing; when an exclusive
    // cleanup lease is active the write is refused before any temp file exists,
    // so the previous journal file stays byte-for-byte unchanged.
    let _output_guard = match crate::shell::output_store::OutputStore::guard_existing(
        data.root.as_path(),
    ) {
        Ok(lease) => lease,
        Err(error) => {
            return Err(format!(
                "Execution journal write refused: the workspace output lease is unavailable: {error}"
            ));
        }
    };
    let Some(dir) = data.path.parent() else {
        return Err("Execution journal path has no parent directory".into());
    };
    validate_locations(&data.path, true)?;
    set_private_dir(dir)?;

    let mut enforced = candidate.clone();
    enforce_limits(&mut enforced, &data.live_scopes)?;
    // Keep the durable unbound-scope guard in sync with this instance: a
    // reopened journal must still refuse to guess an owner for an observed
    // scope whose binding failed.
    enforced.unbound_scopes = data.unbound_scopes.iter().cloned().collect();
    // Queries never rewrite; the first real write upgrades a legacy v1 file to
    // the current format while preserving its original evidence.
    enforced.version = JOURNAL_VERSION;
    let mut payload = serde_json::to_vec_pretty(&enforced)
        .map_err(|error| format!("cannot serialize execution journal: {error}"))?;
    loop {
        if payload.len() <= MAX_JOURNAL_FILE_SIZE || enforced.runs.len() <= 1 {
            break;
        }
        // Never trim a run that still owns a live child scope: dropping it would
        // lose required evidence. Trim an unrelated run instead, and fail closed
        // below when no safe candidate exists.
        let Some(position) = enforced
            .runs
            .iter()
            .position(|run| !run_owns_live_scope(run, &data.live_scopes))
        else {
            break;
        };
        enforced.runs.remove(position);
        enforced.dropped_runs += 1;
        payload = serde_json::to_vec_pretty(&enforced)
            .map_err(|error| format!("cannot serialize execution journal: {error}"))?;
    }
    if payload.len() > MAX_JOURNAL_FILE_SIZE {
        return Err(format!(
            "execution journal exceeds {MAX_JOURNAL_FILE_SIZE} bytes even after trimming"
        ));
    }

    let mut temp = NamedTempFile::new_in(dir).map_err(|error| {
        format!(
            "cannot create journal tempfile in {}: {error}",
            dir.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("cannot set journal mode 0600: {error}"))?;
    }
    temp.write_all(&payload)
        .map_err(|error| format!("cannot write execution journal: {error}"))?;
    temp.as_file()
        .sync_all()
        .map_err(|error| format!("cannot sync execution journal: {error}"))?;
    temp.persist(&data.path).map_err(|error| {
        format!(
            "cannot persist execution journal to {}: {error}",
            data.path.display()
        )
    })?;
    Ok(enforced)
}

fn enforce_limits(file: &mut JournalFile, live: &BTreeSet<String>) -> Result<(), String> {
    while file.runs.len() > MAX_JOURNAL_RUNS {
        let Some(position) = file
            .runs
            .iter()
            .position(|run| !run_owns_live_scope(run, live))
        else {
            return Err(
                "execution journal capacity cannot retain every run that owns a live child scope; refusing to drop required evidence"
                    .into(),
            );
        };
        file.runs.remove(position);
        file.dropped_runs += 1;
    }
    let mut truncated = 0u64;
    for run in &mut file.runs {
        while run.operations.len() > MAX_RUN_OPERATIONS {
            run.operations.remove(0);
            truncated += 1;
        }
        while run.calls.len() > MAX_RUN_CALLS {
            let Some(position) = run
                .calls
                .iter()
                .position(|call| !live.contains(&call.scope))
            else {
                return Err(
                    "execution journal capacity cannot retain every canonical call for a live child scope; refusing to drop required evidence"
                        .into(),
                );
            };
            run.calls.remove(position);
            run.dropped_calls = run.dropped_calls.saturating_add(1);
        }
        for call in &mut run.calls {
            *call = bounded_call(call);
        }
        for op in &mut run.operations {
            op.operation = truncate(&op.operation, MAX_TARGET_CHARS);
            op.target = truncate(&op.target, MAX_TARGET_CHARS);
            if let Some(detail) = &op.detail {
                let trimmed = truncate(detail, MAX_DETAIL_CHARS);
                op.detail = Some(trimmed);
            }
            let bounded_correlation = OperationCorrelation {
                scope: op.correlation.scope.as_deref().and_then(bounded),
                tool_call_id: op.correlation.tool_call_id.as_deref().and_then(bounded),
                call_id: op.correlation.call_id.as_deref().and_then(bounded),
                turn: op.correlation.turn,
                effect_id: op.correlation.effect_id,
            };
            op.correlation = bounded_correlation;
        }
    }
    file.truncated_operations += truncated;
    Ok(())
}

/// Whether a journal run still owns a scope that is live in this process.
fn run_owns_live_scope(run: &RunRecord, live: &BTreeSet<String>) -> bool {
    live.iter().any(|scope| {
        run.engine_run_id.as_deref() == Some(scope.as_str())
            || run.engine_scopes.iter().any(|bound| bound == scope)
    })
}

// Reject static symlinks in the two product-managed directories and the final
// file. As with workspace paths, this is not an OS sandbox against rename races.
fn validate_locations(path: &Path, create: bool) -> Result<(), String> {
    let dir = path.parent().ok_or("invalid journal directory")?;
    let product = dir.parent().ok_or("invalid journal root")?;
    for directory in [product, dir] {
        if create {
            match fs::create_dir(directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(format!(
                        "cannot create journal directory {}: {error}",
                        directory.display()
                    ));
                }
            }
        }
        match fs::symlink_metadata(directory) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err("journal directories must be real directories, not symlinks".into());
            }
            Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err("journal must be a regular file, not a symlink".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(unix)]
fn set_private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|error| error.to_string())
}

#[cfg(not(unix))]
fn set_private_dir(_dir: &Path) -> Result<(), String> {
    Ok(())
}

/// The canonical task phases the journal accepts as a real projection.
fn is_canonical_phase(phase: &str) -> bool {
    matches!(
        phase,
        "queued" | "running" | "completed" | "failed" | "cancelled" | "timed_out"
    )
}

/// A bounded, non-empty correlation id. Empty input stays `None`, never an
/// empty string, so an absent id is never mistaken for a recorded one.
fn bounded(value: &str) -> Option<String> {
    if value.is_empty() {
        return None;
    }
    Some(truncate(value, MAX_TARGET_CHARS))
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let mut out: String = value.chars().take(max_chars).collect();
    out.push_str("...");
    out
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_audits_are_bounded_persisted_and_queryable_without_source_content() {
        let root = tempfile::tempdir().unwrap();
        let history = vec![
            crate::Message::user("private-source-must-not-enter-journal".repeat(100)),
            crate::Message::assistant("old finding"),
            crate::Message::user("recent question"),
            crate::Message::assistant("recent answer"),
        ];
        let report = crate::compact::compact_history(&history, 2, None)
            .1
            .loss_report
            .unwrap();
        let observation = crate::compact::ContextCompactionObservation {
            run_id: uuid::Uuid::new_v4(),
            turn: Some(1),
            agent: None,
            kind: crate::compact::CompactionKind::Request,
            report,
        };
        let journal = Journal::open(root.path(), "audit-session");
        journal.begin_run().unwrap();
        for _ in 0..6 {
            journal.record_context_compaction(&observation).unwrap();
        }
        journal.finish_run(RunState::Failed).unwrap();
        let restored = Journal::open(root.path(), "audit-session");
        let report = restored.recovery_report().unwrap();
        assert_eq!(report.context_compactions.len(), 4);
        assert_eq!(report.dropped_context_compactions, 2);
        assert_eq!(report.context_compactions[0].observation, observation);
        assert!(!report.context_compactions[0].journal_run_id.is_empty());
        let stored = fs::read_to_string(journal_path(root.path(), "audit-session")).unwrap();
        assert!(!stored.contains("private-source-must-not-enter-journal"));
        assert!(
            Journal::disabled()
                .record_context_compaction(&observation)
                .is_ok()
        );
    }

    fn read_file(root: &Path, session_id: &str) -> JournalFile {
        let path = journal_path(root, session_id);
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap()
    }

    #[test]
    fn records_intent_result_and_verifies_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path(), "sess-1");
        journal.begin_run().unwrap();
        journal.record_intent("write", "a.txt").unwrap();
        journal
            .record_result("write", "a.txt", &Ok("wrote 3 bytes".to_string()))
            .unwrap();
        journal.finish_run(RunState::Completed).unwrap();

        let file = read_file(dir.path(), "sess-1");
        assert_eq!(file.version, JOURNAL_VERSION);
        assert_eq!(file.runs.len(), 1);
        assert_eq!(file.runs[0].state, RunState::Completed);
        assert_eq!(file.runs[0].operations.len(), 1);
        assert_eq!(file.runs[0].operations[0].status, OpStatus::Executed);

        let report = Journal::open(dir.path(), "sess-1")
            .mark_interrupted()
            .unwrap();
        assert_eq!(report.executed_operations, 1);
        assert!(!report.has_unfinished());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_path = journal_path(dir.path(), "sess-1");
            assert_eq!(
                fs::metadata(&file_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(file_path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn failed_turn_keeps_executed_evidence_and_marks_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path(), "sess-2");
        journal.begin_run().unwrap();
        journal.record_intent("write", "good.txt").unwrap();
        journal
            .record_result("write", "good.txt", &Ok("ok".to_string()))
            .unwrap();
        journal.record_intent("bash", "sleep 99").unwrap();
        journal.finish_run(RunState::Failed).unwrap();

        let report = journal.recovery_report().unwrap();
        assert_eq!(report.executed_operations, 1);
        assert_eq!(report.unknown_operations.len(), 1);
        assert_eq!(report.unknown_operations[0].operation, "bash");
        assert!(report.has_unfinished());
        // 已执行操作保留为 Executed，不会被改成 Unknown。
        let file = read_file(dir.path(), "sess-2");
        assert_eq!(file.runs[0].operations[0].status, OpStatus::Executed);
        assert_eq!(file.runs[0].operations[1].status, OpStatus::Unknown);
        assert_eq!(file.runs[0].state, RunState::Failed);
    }

    #[test]
    fn cancelled_turn_marks_pending_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path(), "sess-3");
        journal.begin_run().unwrap();
        journal.record_intent("bash", "long-running").unwrap();
        journal.finish_run(RunState::Cancelled).unwrap();
        let report = journal.recovery_report().unwrap();
        assert_eq!(report.unknown_operations.len(), 1);
        assert_eq!(report.unknown_operations[0].status, OpStatus::Unknown);
        assert!(report.has_unfinished());
    }

    #[test]
    fn restart_marks_active_run_interrupted_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        {
            let journal = Journal::open(dir.path(), "sess-4");
            journal.begin_run().unwrap();
            journal.record_intent("write", "partial.txt").unwrap();
            // 模拟进程崩溃：不调用 finish_run。
        }
        let reopened = Journal::open(dir.path(), "sess-4");
        assert!(reopened.recovery_report().unwrap().has_unfinished());
        let report = reopened.mark_interrupted().unwrap();
        assert_eq!(report.interrupted_runs, 1);
        assert_eq!(report.unknown_operations.len(), 1);
        let file = read_file(dir.path(), "sess-4");
        assert_eq!(file.runs[0].state, RunState::Interrupted);
        assert_eq!(file.runs[0].operations[0].status, OpStatus::Unknown);
    }

    #[test]
    fn write_failure_blocks_intent() {
        let dir = tempfile::tempdir().unwrap();
        // `.mio/journal` 是普通文件，无法作为目录写入。
        fs::create_dir_all(dir.path().join(".mio")).unwrap();
        fs::write(dir.path().join(".mio/journal"), "not a directory").unwrap();
        let journal = Journal::open(dir.path(), "sess-5");
        let _ = journal.begin_run();
        let err = journal.record_intent("write", "a.txt").unwrap_err();
        assert!(!err.is_empty());
        // 失败被记录为加载/写入诊断，而不是静默成功。
        let report = journal.recovery_report().unwrap();
        assert!(report.journal_error.is_some());
    }

    #[test]
    fn disabled_journal_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::disabled();
        assert!(!journal.is_enabled());
        assert!(journal.path().is_none());
        assert!(journal.begin_run().is_ok());
        assert!(journal.record_intent("write", "a.txt").is_ok());
        assert!(journal.finish_run(RunState::Completed).is_ok());
        assert!(journal.recovery_report().is_none());
        assert!(!dir.path().join(".mio").exists());
    }

    #[test]
    fn size_limit_drops_oldest_runs() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path(), "sess-6");
        for _ in 0..(MAX_JOURNAL_RUNS + 5) {
            journal.begin_run().unwrap();
            journal.finish_run(RunState::Completed).unwrap();
        }
        let file = read_file(dir.path(), "sess-6");
        assert!(file.runs.len() <= MAX_JOURNAL_RUNS);
        assert!(file.dropped_runs >= 5);
    }

    #[test]
    fn long_targets_and_details_are_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path(), "sess-7");
        journal.begin_run().unwrap();
        let huge = "x".repeat(MAX_TARGET_CHARS * 4);
        journal.record_intent("write", &huge).unwrap();
        journal
            .record_result("write", &huge, &Err(huge.clone()))
            .unwrap();
        journal.finish_run(RunState::Completed).unwrap();
        let file = read_file(dir.path(), "sess-7");
        let op = &file.runs[0].operations[0];
        assert!(op.target.chars().count() <= MAX_TARGET_CHARS + 3);
        assert_eq!(op.status, OpStatus::Failed);
        assert!(
            op.detail
                .as_ref()
                .is_some_and(|d| d.chars().count() <= MAX_DETAIL_CHARS + 3)
        );
    }

    #[test]
    fn bound_and_legacy_intents_round_trip_without_fabricated_correlation() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path(), "corr");
        journal.begin_run().unwrap();
        let binding = crate::policy::ApprovalBinding {
            entry: crate::policy::EntryKind::Builtin,
            tool: "write".into(),
            operation: "write".into(),
            cwd: "/work".into(),
            scope: uuid::Uuid::new_v4().to_string(),
            params_sha256: "digest-must-not-be-recorded".into(),
            call_id: uuid::Uuid::new_v4().to_string(),
            tool_call_id: Some("call-1".into()),
        };
        journal
            .record_bound_intent("write", "a.txt", &binding)
            .unwrap();
        journal.record_intent("bash", "legacy command").unwrap();
        journal
            .record_intent("bash", &"c".repeat(MAX_TARGET_CHARS * 2))
            .unwrap();
        journal.finish_run(RunState::Cancelled).unwrap();

        let file = read_file(dir.path(), "corr");
        let bound = &file.runs[0].operations[0];
        assert_eq!(bound.status, OpStatus::Unknown);
        assert_eq!(
            bound.correlation.scope.as_deref(),
            Some(binding.scope.as_str())
        );
        assert_eq!(bound.correlation.tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(
            bound.correlation.call_id.as_deref(),
            Some(binding.call_id.as_str())
        );
        // A binding cannot prove a turn or effect id: both stay absent.
        assert_eq!(bound.correlation.turn, None);
        assert_eq!(bound.correlation.effect_id, None);
        // The legacy record keeps its historical shape, and long ids are bounded.
        let legacy = &file.runs[0].operations[1];
        assert!(legacy.correlation.is_none());
        let oversized = &file.runs[0].operations[2];
        assert!(oversized.target.chars().count() <= MAX_TARGET_CHARS + 3);
        let stored = fs::read_to_string(journal_path(dir.path(), "corr")).unwrap();
        assert!(!stored.contains("digest-must-not-be-recorded"));
        assert!(!stored.contains("params_sha256"));
    }

    #[test]
    fn corrupt_journal_is_preserved_and_blocks_new_side_effects() {
        let dir = tempfile::tempdir().unwrap();
        let path = journal_path(dir.path(), "sess-8");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{ not json").unwrap();
        let journal = Journal::open(dir.path(), "sess-8");
        assert!(journal.recovery_report().unwrap().journal_error.is_some());
        assert!(journal.begin_run().is_err());
        assert!(journal.record_intent("write", "a.txt").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn oversized_or_mismatched_journals_are_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = journal_path(dir.path(), "bounded");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, vec![b'x'; MAX_JOURNAL_FILE_SIZE + 1]).unwrap();
        let journal = Journal::open(dir.path(), "bounded");
        assert!(journal.begin_run().is_err());
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            (MAX_JOURNAL_FILE_SIZE + 1) as u64
        );
        let good = Journal::open(dir.path(), "source");
        good.begin_run().unwrap();
        good.finish_run(RunState::Completed).unwrap();
        let original = fs::read(journal_path(dir.path(), "source")).unwrap();
        fs::write(&path, &original).unwrap();
        let mismatched = Journal::open(dir.path(), "bounded");
        assert!(mismatched.begin_run().is_err());
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_journal_directory_never_changes_external_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".mio")).unwrap();
        std::os::unix::fs::symlink(external.path(), dir.path().join(".mio/journal")).unwrap();
        let journal = Journal::open(dir.path(), "linked");
        assert!(journal.begin_run().is_err());
        assert_eq!(fs::read_dir(external.path()).unwrap().count(), 0);
    }

    #[test]
    fn recovery_preserves_the_interrupted_request_and_reports_goal_loss() {
        let root = tempfile::tempdir().unwrap();
        let journal = Journal::open(root.path(), "goal-session");
        let goal = format!(
            "Preserve user edits and finish the migration. {}",
            "界".repeat(5000)
        );
        journal.begin_run_with_goal(Some(&goal)).unwrap();
        drop(journal);
        let restored = Journal::open(root.path(), "goal-session");
        restored.mark_interrupted();
        let report = restored.recovery_report().unwrap();
        assert_eq!(report.unfinished_goals.len(), 1);
        let recovered = &report.unfinished_goals[0];
        assert_eq!(recovered.state, RunState::Interrupted);
        assert!(
            recovered
                .goal
                .as_deref()
                .unwrap()
                .starts_with("Preserve user edits")
        );
        assert!(recovered.truncated);
        assert!(recovered.goal.as_ref().unwrap().chars().count() <= 4099);
    }
}
