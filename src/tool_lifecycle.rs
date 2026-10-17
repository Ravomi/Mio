//! 工具调用权威状态机（DES-05）。
//!
//! 本模块是「一次真实引擎工具调用」唯一的状态定义与运行内跟踪器：
//!
//! ```text
//! Normalized -> Denied | Admitted -> Dispatched -> Executed | Failed | Unknown
//! ```
//!
//! - 纯状态定义 [`ToolCallPhase`] 与合法转移 [`ToolCallPhase::transition_allowed`]。
//!   非法转移一律拒绝；终态幂等（重复写入同一终态是空操作，绝不回退）。
//! - [`ToolCallTracker`] 是 Policy（准入）与最终 DispatchHook（派发）**同一运行内**
//!   共享的状态跟踪。`Admitted` 只表示通过 policy 准入，从不假称用户人工批准；
//!   用户审批在 [`ToolCallPhase::Admitted`] 之前由 policy 完成，拒绝写 `Denied`。
//! - `Denied` 必须发生在 `Dispatched` 之前且零副作用。`Dispatched` 只有在 intent
//!   成功持久化之后才允许交给 bus：持久化通过 [`Host::on_tool_call_lifecycle`] 的
//!   默认 `Result` 回调完成，SDK/引擎自身不做 I/O，由宿主决定是否落盘。
//! - 同一 `(scope, provider toolCallId)` 只允许一次 `Dispatched`；重复到达是协议
//!   错误并被拒绝，不重放。
//! - 崩溃恢复：`Dispatched` 未确认 -> `Unknown`；`Normalized`/`Admitted` 未派发 ->
//!   `NotDispatched`（绝不谎称「执行未知」）。显式的模型/策略拒绝是 `Denied`，
//!   不是未知副作用。
//!
//! 运行内跟踪器只是同一 run 的转移守卫；持久化的 canonical calls 由宿主写入
//! journal，才是跨进程恢复的权威。两者使用同一记录类型，不另造影子旁路。

use crate::Host;
use crate::shell::output_store::OutputReference;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

/// 单次运行保留的 canonical 调用上限。
///
/// 与 policy 的 provider id claim ledger 使用同一个上界（4096），这样纯粹检查认领
/// 上限的既有用例不会因为一个新的内存跟踪器而改变结果。跟踪器满员即 fail-closed
/// 拒绝新调用，绝不驱逐已经跟踪的调用去腾位置。写入 journal 时为文件体积另有更小的
/// 单 run 上限（`src/journal.rs::MAX_RUN_CALLS`），超出部分会被有界裁剪并在 journal
/// 里以 `dropped_calls` 标记为证据不完整。
pub const MAX_CANONICAL_CALLS: usize = 4096;

/// 单条 canonical 调用 target 的字符上限。
pub const MAX_CALL_TARGET_CHARS: usize = 512;
/// 单条 canonical 调用 detail 的字符上限。
pub const MAX_CALL_DETAIL_CHARS: usize = 512;

/// 一次工具调用的权威阶段。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallPhase {
    /// 最终参数规范化之后、审批之前的规范化记录。
    Normalized,
    /// 已通过 policy 准入（过滤、只读检查、未知副作用闸门、审批）。不假称用户批准。
    Admitted,
    /// 过滤拒绝、审批拒绝或派发前拒绝；零副作用，必在 `Dispatched` 之前。
    Denied,
    /// 真实派发给 bus；intent 已成功持久化。
    Dispatched,
    /// 观察到成功结果。
    Executed,
    /// 观察到工具失败（不保证零副作用）。
    Failed,
    /// 副作用可能发生但结果不可确认；绝不猜测为成功或失败。
    Unknown,
    /// 在被派发之前就被中断（崩溃/取消/run 结束）；明确不是「执行未知」。
    NotDispatched,
}

impl ToolCallPhase {
    /// 是否为终态。终态幂等且不可回退到非终态。
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Denied | Self::Executed | Self::Failed | Self::Unknown | Self::NotDispatched
        )
    }

    /// 是否已经真正派发（或派发之后）。
    pub const fn is_dispatched(self) -> bool {
        matches!(
            self,
            Self::Dispatched | Self::Executed | Self::Failed | Self::Unknown
        )
    }

    /// 是否允许 `from -> to`。相同状态永远幂等；终态只接受自身。
    pub const fn transition_allowed(from: Self, to: Self) -> bool {
        if from as u8 == to as u8 {
            return true;
        }
        matches!(
            (from, to),
            (
                Self::Normalized,
                Self::Admitted | Self::Denied | Self::NotDispatched
            ) | (
                Self::Admitted,
                Self::Dispatched | Self::Denied | Self::NotDispatched
            ) | (
                Self::Dispatched,
                Self::Executed | Self::Failed | Self::Unknown
            )
        )
    }

    /// 序列化用的稳定字符串。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normalized => "normalized",
            Self::Admitted => "admitted",
            Self::Denied => "denied",
            Self::Dispatched => "dispatched",
            Self::Executed => "executed",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
            Self::NotDispatched => "not_dispatched",
        }
    }
}

/// 一条 canonical 工具调用记录。
///
/// 只保存真实身份与有界元数据：绝不写原始参数、密钥或完整模型输出。`detail` 与
/// `output_references` 分别是有界的结果摘要与真实已提交外置输出引用。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallRecord {
    /// 运行内单调序号；与 `scope` 一起唯一标识一次调用。
    pub call_index: u64,
    /// 真实引擎 run scope（run UUID）。
    pub scope: String,
    /// 权威阶段。
    pub phase: ToolCallPhase,
    /// 规范化工具身份。
    pub tool: String,
    /// 审批 operation 标签。
    pub operation: String,
    /// 真实审批绑定使用的参数 SHA-256；绝不保存原始参数。旧式显式 SDK 调用
    /// （无审批绑定）保持为空，真实 `ToolPolicy` 路径必须携带与实际
    /// [`crate::policy::ApprovalBinding::params_sha256`] 完全一致的摘要。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub params_sha256: String,
    /// 有界的人类可读目标。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target: String,
    /// `target` 是否为脱敏摘要而不是字面目标。显式标记，避免把脱敏值误当完整证据。
    #[serde(default, skip_serializing_if = "is_false")]
    pub target_redacted: bool,
    /// 模型自己的 tool-call id（provider id），绝不填审批 attempt id。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// 审批 attempt id，与 `tool_call_id` 严格区分。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    /// 引擎 turn。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u64>,
    /// 派发器 mint 的 effect id。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_id: Option<u64>,
    /// 有界结果摘要（成功输出或失败原因）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// 真实已提交的外置输出引用。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_references: Vec<OutputReference>,
    /// 引用是否被有界截断；为真时证据不完整。
    #[serde(default)]
    pub references_truncated: bool,
    /// 首次记录的 Unix 秒。
    pub started: u64,
    /// 最近一次更新的 Unix 秒。
    pub updated: u64,
}

impl ToolCallRecord {
    /// 运行内定位键：同 run 内 `call_index` 单调。
    pub fn key(&self) -> (String, u64) {
        (self.scope.clone(), self.call_index)
    }
}

/// 状态机或持久化失败。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolLifecycleError {
    /// 跟踪器满员，拒绝新调用而不驱逐已跟踪调用。
    Full,
    /// 本 run 已经 fail-closed（一次持久化失败后不再派发新副作用）。
    FailClosed(String),
    /// 找不到匹配的 canonical 调用。
    NotFound,
    /// 非法转移。
    Illegal {
        /// 原状态。
        from: ToolCallPhase,
        /// 目标状态。
        to: ToolCallPhase,
    },
    /// 同一 `(scope, provider toolCallId)` 的重复派发。
    Duplicate(String),
    /// 宿主持久化失败。
    Persist(String),
}

impl std::fmt::Display for ToolLifecycleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => write!(
                formatter,
                "the tool call tracker is full ({MAX_CANONICAL_CALLS} calls); refusing a new call"
            ),
            Self::FailClosed(reason) => {
                write!(formatter, "the tool call tracker is fail-closed: {reason}")
            }
            Self::NotFound => write!(formatter, "no tracked tool call matches this dispatch"),
            Self::Illegal { from, to } => write!(
                formatter,
                "illegal tool call transition {} -> {}",
                from.as_str(),
                to.as_str()
            ),
            Self::Duplicate(id) => write!(
                formatter,
                "duplicate dispatch for tool call id '{id}': this run already dispatched it"
            ),
            Self::Persist(error) => write!(
                formatter,
                "the tool call state could not be persisted: {error}"
            ),
        }
    }
}

impl std::error::Error for ToolLifecycleError {}

struct TrackerState {
    calls: Vec<ToolCallRecord>,
    /// `(scope, provider toolCallId)` 摘要 -> call_index，用于重复派发拒绝。
    dispatched: HashMap<[u8; 32], u64>,
    next_index: u64,
    /// 已预留 call_index 但尚未提交的调用数；容量检查把它计入，
    /// 并发 `begin` 因此不会一起越过上界。
    pending: usize,
    /// 每个已跟踪调用自己的提交锁：同一调用的转移串行，不同调用互不阻塞。
    /// Host 持久化回调在持有该调用锁、但不持有 `state` 时执行，因此回调里
    /// 查询 [`ToolCallTracker::calls`] 不会死锁。
    locks: HashMap<u64, Arc<Mutex<()>>>,
}

/// 一次运行的 canonical 工具调用跟踪器。
///
/// 克隆共享同一份状态；Policy 与最终 DispatchHook 使用同一个实例。所有转移先经过
/// 合法转移校验，再通过 [`Host::on_tool_call_lifecycle`] 持久化，成功后才提交到内存。
pub struct ToolCallTracker {
    scope: String,
    host: Arc<dyn Host>,
    capacity: usize,
    state: Mutex<TrackerState>,
    fail_closed: Mutex<Option<String>>,
    latched: AtomicBool,
}

impl ToolCallTracker {
    /// 为一次运行创建跟踪器。
    pub fn new(scope: impl Into<String>, host: Arc<dyn Host>) -> Arc<Self> {
        Self::with_capacity(scope, host, MAX_CANONICAL_CALLS)
    }

    /// 指定容量的跟踪器（测试与更小的宿主）。
    pub fn with_capacity(
        scope: impl Into<String>,
        host: Arc<dyn Host>,
        capacity: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            scope: scope.into(),
            host,
            capacity,
            state: Mutex::new(TrackerState {
                calls: Vec::new(),
                dispatched: HashMap::new(),
                next_index: 0,
                pending: 0,
                locks: HashMap::new(),
            }),
            fail_closed: Mutex::new(None),
            latched: AtomicBool::new(false),
        })
    }

    /// 本跟踪器的 run scope。
    pub fn scope(&self) -> &str {
        &self.scope
    }

    /// 当前是否已经 fail-closed。
    pub fn is_fail_closed(&self) -> bool {
        self.latched.load(Ordering::SeqCst)
    }

    /// 已跟踪的调用（按 `call_index` 升序）。
    pub fn calls(&self) -> Vec<ToolCallRecord> {
        let mut calls = self.lock().calls.clone();
        calls.sort_by_key(|record| record.call_index);
        calls
    }

    /// 记录一次规范化调用（最终参数规范化后、审批前）。
    ///
    /// 显式 SDK 旧调用可能没有审批绑定，因此 `params_sha256` 为空，也不声明
    /// 目标脱敏。真实引擎路径应使用 [`Self::begin_normalized`]。
    pub fn begin(
        &self,
        tool: &str,
        operation: &str,
        target: &str,
        tool_call_id: Option<&str>,
        turn: Option<u64>,
        effect_id: Option<u64>,
    ) -> Result<u64, ToolLifecycleError> {
        self.begin_inner(
            tool,
            operation,
            target,
            false,
            "",
            tool_call_id,
            turn,
            effect_id,
        )
    }

    /// 记录一次规范化调用，并携带真实审批绑定的参数摘要与目标脱敏标记。
    ///
    /// `params_sha256` 必须与交给 [`Host::approve_tool_binding`] 的
    /// [`crate::policy::ApprovalBinding::params_sha256`] 完全一致；原始参数、插件
    /// 与 MCP 的 JSON 参数、密钥一律不进入记录。`target_redacted` 为真时，`target`
    /// 只是脱敏摘要而不是字面目标，审计不会把它误当完整证据。
    #[allow(clippy::too_many_arguments)]
    pub fn begin_normalized(
        &self,
        tool: &str,
        operation: &str,
        target: &str,
        target_redacted: bool,
        params_sha256: &str,
        tool_call_id: Option<&str>,
        turn: Option<u64>,
        effect_id: Option<u64>,
    ) -> Result<u64, ToolLifecycleError> {
        self.begin_inner(
            tool,
            operation,
            target,
            target_redacted,
            params_sha256,
            tool_call_id,
            turn,
            effect_id,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn begin_inner(
        &self,
        tool: &str,
        operation: &str,
        target: &str,
        target_redacted: bool,
        params_sha256: &str,
        tool_call_id: Option<&str>,
        turn: Option<u64>,
        effect_id: Option<u64>,
    ) -> Result<u64, ToolLifecycleError> {
        if let Some(reason) = self.fail_closed_reason() {
            return Err(ToolLifecycleError::FailClosed(reason));
        }
        // 预留 `call_index` 与容量槽位：不先提交记录，但把并发 `begin` 计入上界，
        // 也不让两个并发 `begin` 拿到同一个 `call_index`。
        let index = {
            let mut state = self.lock();
            if state.calls.len().saturating_add(state.pending) >= self.capacity {
                return Err(ToolLifecycleError::Full);
            }
            let index = state.next_index + 1;
            state.next_index = index;
            state.pending += 1;
            state
                .locks
                .entry(index)
                .or_insert_with(|| Arc::new(Mutex::new(())));
            index
        };
        let now = now_secs();
        let record = ToolCallRecord {
            call_index: index,
            scope: self.scope.clone(),
            phase: ToolCallPhase::Normalized,
            tool: bounded(tool, MAX_CALL_TARGET_CHARS),
            operation: bounded(operation, MAX_CALL_TARGET_CHARS),
            params_sha256: bounded(params_sha256, MAX_CALL_DETAIL_CHARS),
            target: bounded(target, MAX_CALL_TARGET_CHARS),
            target_redacted,
            tool_call_id: identity_opt(tool_call_id),
            call_id: None,
            turn,
            effect_id,
            detail: None,
            output_references: Vec::new(),
            references_truncated: false,
            started: now,
            updated: now,
        };
        // Host 持久化在 `state` 锁之外执行；持该调用的提交锁，使同一调用的提交串行。
        let lock = self.call_lock(index);
        let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Err(error) = self.persist(&record) {
            let mut state = self.lock();
            state.pending = state.pending.saturating_sub(1);
            return Err(error);
        }
        let mut state = self.lock();
        state.pending = state.pending.saturating_sub(1);
        let position = state.calls.partition_point(|call| call.call_index < index);
        state.calls.insert(position, record);
        Ok(index)
    }

    /// 记录审批 attempt id（不改变阶段）。
    pub fn set_call_id(&self, index: u64, call_id: &str) -> Result<(), ToolLifecycleError> {
        let call_id = identity_opt(Some(call_id));
        self.update(index, move |record| {
            record.call_id = call_id;
        })
    }

    /// 记录 policy 准入通过。
    pub fn admit(
        &self,
        index: u64,
        turn: Option<u64>,
        effect_id: Option<u64>,
    ) -> Result<(), ToolLifecycleError> {
        self.update(index, |record| {
            record.phase = ToolCallPhase::Admitted;
            if turn.is_some() {
                record.turn = turn;
            }
            if effect_id.is_some() {
                record.effect_id = effect_id;
            }
        })
    }

    /// 记录派发前拒绝（过滤/审批/落盘闸门），零副作用。
    pub fn deny(&self, index: u64, reason: &str) -> Result<(), ToolLifecycleError> {
        let reason = bounded(reason, MAX_CALL_DETAIL_CHARS);
        self.update(index, |record| {
            record.phase = ToolCallPhase::Denied;
            record.detail = Some(reason.clone());
        })
    }

    /// 记录真实派发。返回 `call_index`。
    ///
    /// 同一 `(scope, provider toolCallId)` 只允许一次；重复到达拒绝。持久化失败时
    /// 不提交内存状态并 latch fail-closed，调用方必须阻止 bus 派发。
    pub fn mark_dispatched(
        &self,
        effect_id: u64,
        tool_call_id: Option<&str>,
    ) -> Result<u64, ToolLifecycleError> {
        let index = {
            let state = self.lock();
            state
                .calls
                .iter()
                .find(|record| matches_dispatch(record, effect_id, tool_call_id))
                .map(|record| record.call_index)
                .ok_or(ToolLifecycleError::NotFound)?
        };
        let lock = self.call_lock(index);
        let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let candidate = {
            let mut state = self.lock();
            let position = state
                .calls
                .iter()
                .position(|record| record.call_index == index)
                .ok_or(ToolLifecycleError::NotFound)?;
            let mut candidate = state.calls[position].clone();
            if candidate.phase != ToolCallPhase::Dispatched
                && !ToolCallPhase::transition_allowed(candidate.phase, ToolCallPhase::Dispatched)
            {
                return Err(ToolLifecycleError::Illegal {
                    from: candidate.phase,
                    to: ToolCallPhase::Dispatched,
                });
            }
            // 重复判定与 claim 预留是 `state` 锁下的同一步，因此即使两个调用持有
            // 不同的提交锁，携带同一 provider id 的并发派发也只能成功一个。
            if let Some(tool_call_id) = tool_call_id {
                let key = claim_key(&self.scope, tool_call_id);
                if let Some(previous) = state.dispatched.get(&key) {
                    return Err(ToolLifecycleError::Duplicate(format!(
                        "tool call id '{tool_call_id}' was already dispatched as call {previous}"
                    )));
                }
                state.dispatched.insert(key, index);
            }
            candidate.phase = ToolCallPhase::Dispatched;
            candidate.effect_id = Some(effect_id);
            candidate.updated = now_secs();
            candidate
        };
        self.persist(&candidate)?;
        let mut state = self.lock();
        let position = state
            .calls
            .iter()
            .position(|record| record.call_index == index)
            .ok_or(ToolLifecycleError::NotFound)?;
        state.calls[position] = candidate;
        Ok(index)
    }

    /// 记录一次已派发调用的结果阶段（`Executed`/`Failed`/`Unknown`）。
    ///
    /// 终态幂等；从终态出发的不同阶段转移被拒绝，绝不回退。
    pub fn observe_outcome(
        &self,
        effect_id: u64,
        tool_call_id: Option<&str>,
        phase: ToolCallPhase,
        detail: Option<&str>,
    ) -> Result<(), ToolLifecycleError> {
        self.observe_outcome_with_references(effect_id, tool_call_id, phase, detail, &[], false)
    }

    /// [`Self::observe_outcome`] carrying the real committed output references.
    ///
    /// A terminal `Executed`/`Failed` canonical outcome therefore survives on
    /// the exact typed references the tool wrapper published, so managed output
    /// referenced by a large MCP/plugin result stays protected from cleanup
    /// even though the reference is minted after the tool-finished event.
    pub fn observe_outcome_with_references(
        &self,
        effect_id: u64,
        tool_call_id: Option<&str>,
        phase: ToolCallPhase,
        detail: Option<&str>,
        references: &[OutputReference],
        references_truncated: bool,
    ) -> Result<(), ToolLifecycleError> {
        debug_assert!(matches!(
            phase,
            ToolCallPhase::Executed | ToolCallPhase::Failed | ToolCallPhase::Unknown
        ));
        let index = {
            let state = self.lock();
            state
                .calls
                .iter()
                .find(|record| matches_dispatch(record, effect_id, tool_call_id))
                .map(|record| record.call_index)
                .ok_or(ToolLifecycleError::NotFound)?
        };
        let detail = detail.map(|value| bounded(value, MAX_CALL_DETAIL_CHARS));
        let (references, truncated) = bounded_call_references(references, references_truncated);
        self.update(index, move |record| {
            record.phase = phase;
            if detail.is_some() {
                record.detail = detail;
            }
            if record.effect_id.is_none() {
                record.effect_id = Some(effect_id);
            }
            if !references.is_empty() {
                record.output_references = references;
            }
            record.references_truncated |= truncated;
        })
    }

    /// 本 run 是否跟踪了该真实 provider tool-call id。
    ///
    /// DES-12 的整批外置只处理真实已派发的调用；历史里身份不明（resume 后不在本 run
    /// tracker 中）的结果不猜、不伪造。
    pub fn has_tool_call_id(&self, tool_call_id: &str) -> bool {
        self.lock()
            .calls
            .iter()
            .any(|record| record.tool_call_id.as_deref() == Some(tool_call_id))
    }

    /// 只补充真实已提交的外置输出引用，不改变权威阶段。
    ///
    /// DES-12 的整批外置在工具终态之后才产生引用：请求边界发现最新一批结果正文过大，
    /// 于是把正文外置并发布 typed 引用。此时 canonical 记录已经是终态，不能再做阶段
    /// 转移，只能把真实引用并进去，让 `Host` journal 的 canonical 记录保护清理。
    ///
    /// `run_scope` 必须与本 tracker 的 scope 一致；`tool_call_id` 必须能定位到真实
    /// 调用。scope 不匹配、找不到调用或持久化失败都返回 `Err`，调用方必须保守拒绝，
    /// 而不是伪造记录或静默丢弃引用。
    pub fn supplement_output_references(
        &self,
        run_scope: &str,
        tool_call_id: &str,
        references: &[OutputReference],
        references_truncated: bool,
    ) -> Result<(), ToolLifecycleError> {
        if run_scope != self.scope {
            return Err(ToolLifecycleError::NotFound);
        }
        if references.is_empty() && !references_truncated {
            return Ok(());
        }
        let index = {
            let state = self.lock();
            state
                .calls
                .iter()
                .find(|record| record.tool_call_id.as_deref() == Some(tool_call_id))
                .map(|record| record.call_index)
                .ok_or(ToolLifecycleError::NotFound)?
        };
        self.update(index, move |record| {
            let mut merged = record.output_references.clone();
            for reference in references {
                if !merged.contains(reference) {
                    merged.push(reference.clone());
                }
            }
            let (merged, extra_truncated) = bounded_call_references(&merged, references_truncated);
            record.output_references = merged;
            record.references_truncated |= extra_truncated;
        })
    }

    fn update<F>(&self, index: u64, mutate: F) -> Result<(), ToolLifecycleError>
    where
        F: FnOnce(&mut ToolCallRecord),
    {
        let lock = self.call_lock(index);
        let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let candidate = {
            let state = self.lock();
            let position = state
                .calls
                .iter()
                .position(|record| record.call_index == index)
                .ok_or(ToolLifecycleError::NotFound)?;
            let mut candidate = state.calls[position].clone();
            let previous = candidate.phase;
            mutate(&mut candidate);
            if candidate.phase != previous
                && !ToolCallPhase::transition_allowed(previous, candidate.phase)
            {
                return Err(ToolLifecycleError::Illegal {
                    from: previous,
                    to: candidate.phase,
                });
            }
            candidate.updated = now_secs();
            candidate
        };
        // Host 持久化在 `state` 锁之外执行。持该调用的提交锁，所以同一调用的
        // 并发转移被串行化；不同调用各自串行、互不拒绝。
        self.persist(&candidate)?;
        let mut state = self.lock();
        let position = state
            .calls
            .iter()
            .position(|record| record.call_index == index)
            .ok_or(ToolLifecycleError::NotFound)?;
        state.calls[position] = candidate;
        Ok(())
    }

    fn persist(&self, record: &ToolCallRecord) -> Result<(), ToolLifecycleError> {
        if self.is_fail_closed() {
            return Err(ToolLifecycleError::FailClosed(
                self.fail_closed_reason()
                    .unwrap_or_else(|| "a previous persistence failure".into()),
            ));
        }
        match self.host.on_tool_call_lifecycle(record) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.latch(&error);
                Err(ToolLifecycleError::Persist(error))
            }
        }
    }

    fn latch(&self, reason: &str) {
        if !self.latched.swap(true, Ordering::SeqCst) {
            *self
                .fail_closed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some(bounded(reason, MAX_CALL_DETAIL_CHARS));
        }
    }

    fn fail_closed_reason(&self) -> Option<String> {
        self.fail_closed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 取该调用自己的提交锁；同一调用串行，不同调用各自独立。
    fn call_lock(&self, index: u64) -> Arc<Mutex<()>> {
        let mut state = self.lock();
        state
            .locks
            .entry(index)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TrackerState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// `(scope, provider toolCallId)` 的摘要，与 policy 的 claim ledger 使用同一分隔约定。
fn claim_key(scope: &str, call_id: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(scope.as_bytes());
    hasher.update([0]);
    hasher.update(call_id.as_bytes());
    hasher.finalize().into()
}

fn bounded(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let mut out: String = value.chars().take(max_chars).collect();
    out.push_str("...");
    out
}

/// 单条 canonical 调用保留的最大真实输出引用数。
const MAX_CALL_REFERENCES: usize = crate::tool_identity::MAX_OUTPUT_REFERENCES_PER_CALL;

/// 有界保留真实引用；超出上界时标记证据不完整，绝不静默丢弃而谎称完整。
fn bounded_call_references(
    references: &[OutputReference],
    already_truncated: bool,
) -> (Vec<OutputReference>, bool) {
    let truncated = already_truncated || references.len() > MAX_CALL_REFERENCES;
    (
        references
            .iter()
            .take(MAX_CALL_REFERENCES)
            .cloned()
            .collect(),
        truncated,
    )
}

/// 一个 canonical 记录是否就是该 effect/`toolCallId` 对应的调用。
fn matches_dispatch(record: &ToolCallRecord, effect_id: u64, tool_call_id: Option<&str>) -> bool {
    if record.effect_id == Some(effect_id) {
        return true;
    }
    match (record.tool_call_id.as_deref(), tool_call_id) {
        (Some(known), Some(current)) => known == current,
        _ => false,
    }
}

/// 真实身份字段（scope/provider id/approval attempt id）原样保留，绝不截断成前缀，
/// 否则两个共享前缀的不同调用会被当成同一个身份。真实 provider id 已由 policy
/// 限制为 ≤512 字节，因此原样存储也是有界的。
fn identity_opt(value: Option<&str>) -> Option<String> {
    match value {
        Some(value) if !value.is_empty() => Some(value.to_owned()),
        _ => None,
    }
}

fn is_false(value: &bool) -> bool {
    !*value
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
    use crate::{AgentEvent, HostFuture};
    use std::{path::Path, sync::Mutex as StdMutex};

    #[derive(Default)]
    struct RecordingHost {
        records: StdMutex<Vec<ToolCallRecord>>,
        fail_writes: std::sync::atomic::AtomicBool,
    }

    impl Host for RecordingHost {
        fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
            Box::pin(async { Ok(String::new()) })
        }
        fn on_event(&self, _: AgentEvent) {}
        fn on_tool_call_lifecycle(&self, call: &ToolCallRecord) -> Result<(), String> {
            if self.fail_writes.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("disk full".into());
            }
            self.records.lock().unwrap().push(call.clone());
            Ok(())
        }
    }

    fn tracker(host: Arc<RecordingHost>) -> Arc<ToolCallTracker> {
        ToolCallTracker::new("run-1", host)
    }

    #[test]
    fn legal_transitions_are_exhaustive_and_terminal_states_are_idempotent() {
        use ToolCallPhase::*;
        for (from, to, expected) in [
            (Normalized, Admitted, true),
            (Normalized, Denied, true),
            (Normalized, NotDispatched, true),
            (Normalized, Dispatched, false),
            (Normalized, Executed, false),
            (Admitted, Dispatched, true),
            (Admitted, Denied, true),
            (Admitted, NotDispatched, true),
            (Admitted, Executed, false),
            (Dispatched, Executed, true),
            (Dispatched, Failed, true),
            (Dispatched, Unknown, true),
            (Dispatched, Denied, false),
            (Denied, Admitted, false),
            (Executed, Failed, false),
            (Unknown, Executed, false),
            (NotDispatched, Admitted, false),
        ] {
            assert_eq!(
                ToolCallPhase::transition_allowed(from, to),
                expected,
                "{from:?} -> {to:?}"
            );
        }
        for terminal in [Denied, Executed, Failed, Unknown, NotDispatched] {
            assert!(terminal.is_terminal());
            assert!(ToolCallPhase::transition_allowed(terminal, terminal));
        }
        // A real dispatch can only resolve to an observed outcome, never back
        // into a pre-dispatch refusal.
        for observed in [Executed, Failed, Unknown] {
            assert!(ToolCallPhase::transition_allowed(Dispatched, observed));
        }
        for refusal in [Denied, NotDispatched] {
            assert!(!ToolCallPhase::transition_allowed(Dispatched, refusal));
        }
    }

    #[test]
    fn a_call_records_every_stage_and_is_terminal_idempotent() {
        let host = Arc::new(RecordingHost::default());
        let tracker = tracker(host.clone());
        let index = tracker
            .begin("read", "read", "a.txt", Some("call-1"), Some(2), Some(9))
            .unwrap();
        tracker.set_call_id(index, "attempt-1").unwrap();
        tracker.admit(index, Some(2), Some(9)).unwrap();
        tracker.mark_dispatched(9, Some("call-1")).unwrap();
        tracker
            .observe_outcome(9, Some("call-1"), ToolCallPhase::Executed, Some("ok"))
            .unwrap();
        // Terminal idempotency: the same terminal again is a no-op.
        tracker
            .observe_outcome(9, Some("call-1"), ToolCallPhase::Executed, Some("ok"))
            .unwrap();
        // A different terminal is refused, never regressed.
        assert!(matches!(
            tracker.observe_outcome(9, Some("call-1"), ToolCallPhase::Failed, Some("no")),
            Err(ToolLifecycleError::Illegal { .. })
        ));
        let record = &tracker.calls()[0];
        assert_eq!(record.phase, ToolCallPhase::Executed);
        assert_eq!(record.call_id.as_deref(), Some("attempt-1"));
        assert_eq!(record.tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(record.detail.as_deref(), Some("ok"));
        assert!(!tracker.is_fail_closed());
    }

    #[test]
    fn a_duplicate_dispatch_is_rejected() {
        let host = Arc::new(RecordingHost::default());
        let tracker = tracker(host);
        let index = tracker
            .begin("read", "read", "a.txt", Some("call-1"), None, Some(1))
            .unwrap();
        tracker.admit(index, None, Some(1)).unwrap();
        tracker.mark_dispatched(1, Some("call-1")).unwrap();
        assert!(matches!(
            tracker.mark_dispatched(1, Some("call-1")),
            Err(ToolLifecycleError::Duplicate(_))
        ));
    }

    #[test]
    fn a_persist_failure_latches_fail_closed_and_never_claims_success() {
        let host = Arc::new(RecordingHost::default());
        let tracker = tracker(host.clone());
        let index = tracker
            .begin("write", "write", "a.txt", Some("call-1"), None, Some(1))
            .unwrap();
        tracker.admit(index, None, Some(1)).unwrap();
        host.fail_writes
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let error = tracker.mark_dispatched(1, Some("call-1")).unwrap_err();
        assert!(matches!(error, ToolLifecycleError::Persist(_)), "{error:?}");
        assert!(tracker.is_fail_closed());
        // A latched tracker refuses new states instead of pretending success.
        assert!(tracker.admit(index, None, Some(1)).is_err());
        assert!(matches!(
            tracker.begin("read", "read", "b.txt", Some("call-2"), None, None),
            Err(ToolLifecycleError::FailClosed(_))
        ));
    }

    fn output_reference() -> OutputReference {
        OutputReference {
            path: ".mio/outputs/abcd.stdout".into(),
            total_bytes: 11,
            stream: "stdout".into(),
            call_id: Some("call-1".into()),
            sha256: "b".repeat(64),
        }
    }

    /// A terminal outcome must carry the real committed output reference, so a
    /// managed output stays referenced from canonical evidence instead of only
    /// from model-visible text.
    #[test]
    fn a_terminal_outcome_carries_the_real_committed_reference() {
        let host = Arc::new(RecordingHost::default());
        let tracker = tracker(host.clone());
        let index = tracker
            .begin("read", "read", "a.txt", Some("call-1"), None, Some(1))
            .unwrap();
        tracker.admit(index, None, Some(1)).unwrap();
        tracker.mark_dispatched(1, Some("call-1")).unwrap();
        let reference = output_reference();
        tracker
            .observe_outcome_with_references(
                1,
                Some("call-1"),
                ToolCallPhase::Executed,
                Some("ok"),
                std::slice::from_ref(&reference),
                false,
            )
            .unwrap();
        let record = &tracker.calls()[0];
        assert_eq!(record.phase, ToolCallPhase::Executed);
        assert_eq!(record.output_references, vec![reference.clone()]);
        assert!(!record.references_truncated);
        assert!(
            host.records
                .lock()
                .unwrap()
                .iter()
                .any(|record| record.output_references == vec![reference.clone()]),
            "the host must observe the committed reference"
        );
    }

    #[test]
    fn over_bound_references_are_marked_incomplete_not_silently_dropped() {
        let host = Arc::new(RecordingHost::default());
        let tracker = tracker(host);
        let index = tracker
            .begin("read", "read", "a.txt", Some("call-1"), None, Some(1))
            .unwrap();
        tracker.admit(index, None, Some(1)).unwrap();
        tracker.mark_dispatched(1, Some("call-1")).unwrap();
        let references: Vec<OutputReference> = (0..MAX_CALL_REFERENCES + 1)
            .map(|index| OutputReference {
                path: format!(".mio/outputs/{index}.stdout"),
                total_bytes: 1,
                stream: "stdout".into(),
                call_id: Some("call-1".into()),
                sha256: "c".repeat(64),
            })
            .collect();
        tracker
            .observe_outcome_with_references(
                1,
                Some("call-1"),
                ToolCallPhase::Executed,
                None,
                &references,
                false,
            )
            .unwrap();
        let record = &tracker.calls()[0];
        assert_eq!(record.output_references.len(), MAX_CALL_REFERENCES);
        assert!(record.references_truncated);
    }
}
