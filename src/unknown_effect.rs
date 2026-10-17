//! Typed "unknown side effect" evidence and the run-shared fail-closed fence
//! (DES-09).
//!
//! A dispatched call whose result cannot be confirmed is *not* a failure: the
//! effect may already have happened. This module keeps that fact typed instead
//! of inferring it from an error string:
//!
//! * [`UnknownSideEffect`] is a typed, serializable marker a tool publishes
//!   into its dispatch result through [`mio_core::tool::ToolContext::insert_result`].
//!   [`UnknownReason`] is a closed enum, so a real transport death is never
//!   confused with a peer that provably answered.
//! * [`UnknownEffectFence`] is the bounded, run-shared counter of those
//!   markers. One run tree shares one `Arc` fence, so an unknown that a parent
//!   run observed blocks new write/external dispatches in that run and in its
//!   delegated children until the host explicitly confirms.
//!
//! The fence only ever *adds* evidence. It never claims a rollback, never
//! releases a claim, and never turns "unknown" into "succeeded". When the
//! durable record cannot be written, [`UnknownEffectFence::mark_persist_failed`]
//! makes every later write/external dispatch fail closed.
//!
//! Every observation is read through one atomic [`UnknownEffectEvidence`]
//! snapshot carrying a monotonic `revision`. An admission gate captures one
//! snapshot and re-verifies the revision after each `await`, so an earlier
//! confirmation or a normal approval can never authorize a call that a *new*
//! unknown has since made unsafe. A snapshot is "complete" only while the
//! fence has not overflowed, the durable record was written, and the run scope
//! is actually tracked: an incomplete or unavailable guard always fails closed
//! instead of approving from evidence the host never saw.

use mio_core::tool::{ContextValue, ToolContext};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    future::Future,
    sync::{Arc, Mutex, OnceLock, Weak},
};

/// Longest operation/target string kept in one marker, in characters.
pub const MAX_UNKNOWN_FIELD_CHARS: usize = 512;

/// Most distinct unknown effects one run's fence remembers before it reports
/// overflow. The fence never evicts: once full it stays tripped and keeps
/// counting, so a flood cannot make an older unknown disappear.
pub const MAX_UNKNOWN_EFFECTS: usize = 64;

/// Most run scopes the fence registry tracks at once. It is a bounded cache of
/// weak handles: a live run always holds its own `Arc`, so only dead aliases
/// are pruned and a live scope is never evicted. Once the bound is reached with
/// live scopes, a new scope resolves an explicitly unavailable guard instead of
/// an empty, executable fence.
pub const MAX_FENCE_SCOPES: usize = 4096;

/// Why a dispatched call's side effect could not be confirmed.
///
/// Every variant is a typed fact the engine observed; none is parsed back out
/// of a human-readable error string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownReason {
    /// The stdio MCP transport died after the request may have reached the
    /// server. The call is never replayed.
    McpTransport,
    /// A supervised plugin call ended without a confirmable result because its
    /// supervision timeout fired while it may have been running.
    PluginSupervision,
    /// A dispatch was recovered from durable evidence across a restart (or a
    /// run ended) before any typed result was ever observed. This is a real,
    /// honest reason of its own: it is never reported as an MCP transport
    /// death or a plugin supervision timeout it was not.
    InterruptedDispatch,
    /// A supervised native subprocess (the workspace `shell`/`bash` tool) lost
    /// its result to a supervision timeout after it may have been running.
    NativeTimeout,
    /// A supervised native subprocess's exit status could not be waited for, so
    /// it may have run without a confirmable result.
    NativeWait,
    /// A supervised native subprocess's captured output was lost (a stream read
    /// failed or crossed the complete-output bound) after it may have run.
    NativeCapture,
    /// A `Workspace` outer timeout cancelled a custom [`crate::Host`] shell
    /// future. The engine cannot know whether that host already spawned a
    /// process, so the result is conservatively unknown.
    HostSupervision,
    /// The typed marker could not be published into the dispatch context; the
    /// direct fence mark still tripped the guard.
    MarkerEncoding,
}

impl UnknownReason {
    /// Stable machine-readable label, never derived from an error string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::McpTransport => "mcp_transport",
            Self::PluginSupervision => "plugin_supervision",
            Self::InterruptedDispatch => "interrupted_dispatch",
            Self::NativeTimeout => "native_timeout",
            Self::NativeWait => "native_wait",
            Self::NativeCapture => "native_capture",
            Self::HostSupervision => "host_supervision",
            Self::MarkerEncoding => "marker_encoding",
        }
    }
}

/// One typed, bounded record of a call whose effect is unknown.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnknownSideEffect {
    /// Identity of this observed fact, independent of every engine/tool id.
    /// Clones retain it; older serialized markers remain unidentified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_id: Option<uuid::Uuid>,
    /// Real engine run that observed this effect, absent outside a run scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<uuid::Uuid>,
    /// The canonical operation that was dispatched (`mcp__srv__tool`, `write`).
    pub operation: String,
    /// A bounded human-readable target, never used for identity matching.
    pub target: String,
    /// The typed reason the result is unknown.
    pub reason: UnknownReason,
    /// The model's own tool-call id, when the engine supplied one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Real durable-ledger scope this unknown was recovered from, when it came
    /// from the cross-restart ledger instead of this run. It is the true ledger
    /// key, never a fabricated run id, and stays `None` for an in-run unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger_scope: Option<String>,
    /// Real durable-ledger `call_index` for [`UnknownSideEffect::ledger_scope`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger_call_index: Option<u64>,
    /// Exact durable-ledger snapshot digest the host must acknowledge. A host
    /// acknowledges this precise evidence version, never a re-derived one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger_snapshot_key: Option<String>,
}

impl UnknownSideEffect {
    /// Build a bounded marker.
    pub fn new(
        operation: impl Into<String>,
        target: impl Into<String>,
        reason: UnknownReason,
        tool_call_id: Option<String>,
    ) -> Self {
        Self {
            evidence_id: Some(uuid::Uuid::new_v4()),
            scope: crate::tool_identity::current()
                .map(|identity| identity.run_id())
                .filter(|scope| !scope.is_nil()),
            operation: bounded(operation.into()),
            target: bounded(target.into()),
            reason,
            tool_call_id: tool_call_id.map(bounded),
            ledger_scope: None,
            ledger_call_index: None,
            ledger_snapshot_key: None,
        }
    }

    /// Build a marker recovered from the durable ledger.
    ///
    /// It keeps the real ledger key and the exact snapshot digest, and it never
    /// invents an `evidence_id` or a run scope the ledger did not record.
    pub fn recovered(
        operation: impl Into<String>,
        reason: UnknownReason,
        ledger_scope: impl Into<String>,
        ledger_call_index: u64,
        ledger_snapshot_key: impl Into<String>,
    ) -> Self {
        Self {
            evidence_id: None,
            scope: None,
            operation: bounded(operation.into()),
            target: String::new(),
            reason,
            tool_call_id: None,
            ledger_scope: Some(ledger_scope.into()),
            ledger_call_index: Some(ledger_call_index),
            ledger_snapshot_key: Some(ledger_snapshot_key.into()),
        }
    }

    /// The real durable-ledger `(scope, call_index)` this unknown came from, if
    /// any. Used to deduplicate re-seeding of the same recovered record; it is
    /// never an invented identity.
    pub fn ledger_key(&self) -> Option<(&str, u64)> {
        self.ledger_scope.as_deref().zip(self.ledger_call_index)
    }

    /// The exact ledger snapshot digest the host must acknowledge, if any.
    pub fn ledger_snapshot(&self) -> Option<&str> {
        self.ledger_snapshot_key.as_deref()
    }
}

impl ContextValue for UnknownSideEffect {
    const KEY: &'static str = "mio.unknown_side_effect";
}

fn bounded(value: String) -> String {
    if value.chars().count() <= MAX_UNKNOWN_FIELD_CHARS {
        return value;
    }
    let mut end = MAX_UNKNOWN_FIELD_CHARS;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[derive(Default)]
struct FenceInner {
    unknown: Vec<UnknownSideEffect>,
    overflow: u64,
    persist_failed: bool,
    unavailable: bool,
    revision: u64,
}

/// One atomic view of a fence's evidence, taken under the fence's own lock.
///
/// The snapshot carries the real records and a monotonic
/// [`UnknownEffectEvidence::revision`]. A gate captures one snapshot, hands its
/// records to the host, and compares the revision again after every `await`:
/// an equal revision proves no new unknown (and no new fail-closed state) was
/// recorded in between.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownEffectEvidence {
    unknown: Vec<UnknownSideEffect>,
    overflow: u64,
    persist_failed: bool,
    unavailable: bool,
    revision: u64,
}

impl UnknownEffectEvidence {
    /// Monotonic evidence version; it only ever increases.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The bounded records this snapshot holds, oldest first.
    pub fn unknowns(&self) -> &[UnknownSideEffect] {
        &self.unknown
    }

    /// Number of distinct records this snapshot holds.
    pub fn unknown_count(&self) -> usize {
        self.unknown.len()
    }

    /// Whether any unknown (including an omitted, overflowed one) was seen.
    pub fn has_unknown(&self) -> bool {
        !self.unknown.is_empty() || self.overflow > 0
    }

    /// Whether at least one record was dropped after saturation.
    pub fn overflowed(&self) -> bool {
        self.overflow > 0
    }

    /// Whether a durable unknown record could not be written.
    pub fn persist_failed(&self) -> bool {
        self.persist_failed
    }

    /// Whether the run scope could not be tracked by the bounded registry.
    pub fn unavailable(&self) -> bool {
        self.unavailable
    }

    /// Whether this evidence alone permanently refuses a new write/external
    /// dispatch, whatever a host says: an incomplete snapshot (overflow), a
    /// lost durable record, or an untracked scope. A host can never confirm an
    /// unknown it could not be shown.
    pub fn is_fail_closed(&self) -> bool {
        self.overflow > 0 || self.persist_failed || self.unavailable
    }

    /// Whether `other` describes the same evidence version.
    pub fn same_revision(&self, other: &Self) -> bool {
        self.revision == other.revision
    }
}

/// The bounded, run-shared unknown-effect fence.
///
/// Cloning the `Arc` shares the evidence: a parent run and its delegated
/// children gate on the same records. The fence is monotonic for the length of
/// a run: nothing here removes a record or clears `persist_failed`.
#[derive(Default)]
pub struct UnknownEffectFence {
    inner: Mutex<FenceInner>,
}

impl UnknownEffectFence {
    /// A new, empty fence.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Record one unknown effect. Returns `true` when it was newly recorded.
    ///
    /// The fence is bounded: past [`MAX_UNKNOWN_EFFECTS`] distinct records it
    /// keeps counting (`overflow`) and stays tripped rather than dropping an
    /// older unknown.
    pub fn mark(&self, effect: UnknownSideEffect) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(evidence_id) = effect.evidence_id
            && inner
                .unknown
                .iter()
                .any(|record| record.evidence_id == Some(evidence_id))
        {
            return false;
        }
        // A recovered record is identified by its real durable-ledger key: the
        // same record seeded on a later turn is the same fact, not a new
        // unknown, and re-seeding it must not grow the fence or trip overflow.
        // But the same key can carry a *new evidence version*: a record that
        // gained a fact after the host was shown an earlier digest. Key-only
        // dedup would swallow that new evidence and keep a confirmation of the
        // old snapshot alive, so refresh the stored record and advance the
        // revision instead of dropping it.
        if let Some(position) = effect.ledger_key().and_then(|(scope, index)| {
            inner.unknown.iter().position(|record| {
                record.ledger_scope.as_deref() == Some(scope)
                    && record.ledger_call_index == Some(index)
            })
        }) {
            if inner.unknown[position].ledger_snapshot_key == effect.ledger_snapshot_key {
                return false;
            }
            inner.unknown[position] = effect;
            inner.revision = inner.revision.saturating_add(1);
            return true;
        }
        let newly_recorded = if inner.unknown.len() >= MAX_UNKNOWN_EFFECTS {
            inner.overflow = inner.overflow.saturating_add(1);
            false
        } else {
            inner.unknown.push(effect);
            true
        };
        // Every accepted or omitted record advances the evidence version, so a
        // gate that re-reads it after an `await` can prove no new unknown was
        // added while it was waiting.
        inner.revision = inner.revision.saturating_add(1);
        newly_recorded
    }

    /// Fail closed from now on: the durable unknown record could not be
    /// written, so no later write/external dispatch may proceed.
    pub fn mark_persist_failed(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !inner.persist_failed {
            inner.persist_failed = true;
            inner.revision = inner.revision.saturating_add(1);
        }
    }

    /// Whether the bounded scope registry could not track this run, so the
    /// fence is an explicitly unavailable guard rather than an empty,
    /// executable one. It fabricates no unknown evidence; it only refuses.
    pub fn unavailable(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unavailable
    }

    /// One atomic evidence snapshot with its monotonic revision.
    pub fn evidence(&self) -> UnknownEffectEvidence {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        UnknownEffectEvidence {
            unknown: inner.unknown.clone(),
            overflow: inner.overflow,
            persist_failed: inner.persist_failed,
            unavailable: inner.unavailable,
            revision: inner.revision,
        }
    }

    /// An explicitly unavailable guard for a scope the bounded registry could
    /// not track. Writes fail closed; reads stay available for inspection; and
    /// no unknown is invented to justify the refusal.
    pub(crate) fn unavailable_guard() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(FenceInner {
                unavailable: true,
                revision: 1,
                ..FenceInner::default()
            }),
        })
    }

    /// Whether any unknown effect was observed, including overflow.
    pub fn has_unknown(&self) -> bool {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        !inner.unknown.is_empty() || inner.overflow > 0
    }

    /// Whether a durable unknown record could not be persisted.
    pub fn persist_failed(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .persist_failed
    }

    /// Number of distinct records currently held.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unknown
            .len()
    }

    /// Whether no distinct record is currently held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether at least one distinct unknown was dropped after saturation.
    pub fn overflowed(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .overflow
            > 0
    }

    /// A bounded snapshot of the recorded unknowns, oldest first.
    ///
    /// This is a plain read with no evidence version. An admission gate that
    /// must prove the evidence did not change across an `await` uses
    /// [`UnknownEffectFence::evidence`] instead.
    pub fn snapshot(&self) -> Vec<UnknownSideEffect> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unknown
            .clone()
    }
}

#[derive(Default)]
struct FenceRegistry {
    entries: BTreeMap<String, Weak<UnknownEffectFence>>,
}

fn registry() -> &'static Mutex<FenceRegistry> {
    static REGISTRY: OnceLock<Mutex<FenceRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(FenceRegistry::default()))
}

/// The shared fence for one real run scope (the run UUID as a string).
///
/// A live run holds the strong `Arc`; the registry keeps only a weak handle,
/// so it can never keep a finished run's fence alive. Reach for the same scope
/// string from a tool body and from the run's admission gate and both observe
/// the same fence.
///
/// The registry is bounded, but it is never allowed to evict a live scope: a
/// run's children must keep resolving the run's real fence. Only dead weak
/// handles are pruned. Once the bound is reached with live scopes, a new scope
/// gets an explicitly [`UnknownEffectFence::unavailable`] guard, which fails
/// write/external dispatches closed instead of pretending to be empty.
pub fn fence_for_scope(scope: &str) -> Arc<UnknownEffectFence> {
    let mut registry = registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(fence) = registry.entries.get(scope).and_then(Weak::upgrade) {
        return fence;
    }
    // A finished run's alias is dead; drop only those. A live run holds its own
    // `Arc`, so pruning can never remove a running scope's fence.
    registry.entries.retain(|_, weak| weak.strong_count() > 0);
    if registry.entries.len() >= MAX_FENCE_SCOPES {
        // The bound is reached with live scopes. Hand back an unavailable
        // guard rather than an empty executable fence.
        return UnknownEffectFence::unavailable_guard();
    }
    let fence = UnknownEffectFence::new();
    registry
        .entries
        .insert(scope.to_owned(), Arc::downgrade(&fence));
    fence
}

/// Point one run scope at an existing fence.
///
/// A delegated child shares its parent's fence; registering the alias under the
/// child's own scope lets a child's tools (loaded lazily at dispatch) resolve
/// the same fence. The alias is bounded by the same registry: when the bound is
/// already held by live scopes, the alias is refused instead of evicting a live
/// scope. That never clears an existing parent/child protection: the caller
/// already holds `fence` directly, and the untracked child scope resolves an
/// unavailable, fail-closed guard.
///
/// Returns `true` when the scope now resolves `fence`. `false` means the
/// association could not be saved, and the caller must fail closed for that
/// scope rather than gate it on a fence its own tools cannot reach.
#[must_use]
pub fn alias_scope(scope: &str, fence: &Arc<UnknownEffectFence>) -> bool {
    let mut registry = registry().lock().unwrap_or_else(|e| e.into_inner());
    registry.entries.retain(|_, weak| weak.strong_count() > 0);
    if !registry.entries.contains_key(scope) && registry.entries.len() >= MAX_FENCE_SCOPES {
        return false;
    }
    registry
        .entries
        .insert(scope.to_owned(), Arc::downgrade(fence));
    true
}

/// The fence for the run the current tool body executes in.
///
/// `None` when the engine supplied no real dispatch identity (a direct,
/// engine-less call): absence is reported, never filled with a guess, and such
/// a call has no run guard to trip.
pub fn current_run_fence() -> Option<Arc<UnknownEffectFence>> {
    crate::tool_identity::current().map(|identity| fence_for_scope(&identity.run_id().to_string()))
}

/// Publish a typed unknown marker into `context` and trip the current run's
/// fence.
///
/// The direct fence mark is authoritative: even when publishing the marker
/// fails, the run guard is tripped. Returns the fence when one was reached so
/// the caller can report the durable record and mark a persistence failure.
pub fn mark_unknown(
    context: &mut ToolContext,
    effect: UnknownSideEffect,
) -> Option<Arc<UnknownEffectFence>> {
    if context.insert_result(effect.clone()).is_err()
        && let Some(fence) = current_run_fence()
    {
        fence.mark(UnknownSideEffect::new(
            effect.operation.clone(),
            effect.target.clone(),
            UnknownReason::MarkerEncoding,
            effect.tool_call_id.clone(),
        ));
    }
    let fence = current_run_fence();
    if let Some(fence) = &fence {
        fence.mark(effect);
    }
    fence
}

/// 单次工具执行内，原生监督路径最多记录的原生 unknown 条数。一次调用正常只产生一条，
/// 触及时是保守拒绝（fence 已经点亮），而不是静默丢弃。
pub const MAX_UNKNOWN_EFFECTS_PER_CALL: usize = 8;

/// 原生 unknown 收集失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnknownCollectorError {
    /// 超过单次调用的条数上限。
    Bounds(String),
}

impl std::fmt::Display for UnknownCollectorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bounds(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for UnknownCollectorError {}

/// 一次工具执行内、由**原生监督路径**（`shell` 子进程与 `Workspace` 外层超时）填写的
/// unknown 收集器。
///
/// 这些路径运行在真实工具体内，却没有 [`ToolContext`] 可用；收集器通过
/// [`scope_unknown_effects`] 绑定到执行任务，由 [`crate::tool_identity::ScopedTool`] 在
/// 工具体返回后写入真实 [`ToolContext`]。MCP/plugin 已经直接调用
/// [`crate::Host::on_unknown_side_effect`]，因此不经过该收集器，不会被重复上报。
#[derive(Clone, Default)]
pub struct UnknownEffectCollector {
    unknown: Arc<Mutex<Vec<UnknownSideEffect>>>,
}

impl UnknownEffectCollector {
    /// 新建一个空收集器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 发布一条原生 unknown。超过单次上限时 fail-closed 返回 `Err`；此时调用方仍旧
    /// 已经点亮了 run fence，后续副作用不会被放行。
    pub fn try_publish(&self, effect: UnknownSideEffect) -> Result<(), UnknownCollectorError> {
        let mut collected = self.lock();
        if collected.len() >= MAX_UNKNOWN_EFFECTS_PER_CALL {
            return Err(UnknownCollectorError::Bounds(format!(
                "a single tool call already recorded {MAX_UNKNOWN_EFFECTS_PER_CALL} native unknown effects; refusing to drop one"
            )));
        }
        collected.push(effect);
        Ok(())
    }

    /// 取走当前记录的原生 unknown；调用方负责写进真实 dispatch context。
    pub fn take(&self) -> Vec<UnknownSideEffect> {
        std::mem::take(&mut *self.lock())
    }

    /// 当前记录的克隆快照。
    pub fn snapshot(&self) -> Vec<UnknownSideEffect> {
        self.lock().clone()
    }

    /// 当前记录条数。
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// 当前是否没有记录。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<UnknownSideEffect>> {
        self.unknown
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

tokio::task_local! {
    /// 绑定到执行任务的原生 unknown 收集器；每个工具调用绑定自己的一个。
    static UNKNOWN_EFFECT_COLLECTOR: UnknownEffectCollector;
}

/// 在 `future` 存续期间把一个独立收集器绑定到当前任务。
///
/// 每次工具执行都绑定一个**新的**收集器，所以同一个任务上并发或嵌套调用不会共享、
/// 也不会继承外层的收集器。
pub async fn scope_unknown_effects<F: Future>(
    collector: UnknownEffectCollector,
    future: F,
) -> F::Output {
    UNKNOWN_EFFECT_COLLECTOR.scope(collector, future).await
}

/// 当前任务上由工具执行包装器绑定的原生 unknown 收集器；不在任何工具执行内时为 `None`。
pub fn current_unknown_effect_collector() -> Option<UnknownEffectCollector> {
    UNKNOWN_EFFECT_COLLECTOR.try_with(Clone::clone).ok()
}

/// 原生监督路径发现“进程可能已运行、但结果无法确认”时调用。
///
/// 它做两件真实的事：先用当前真实 dispatch 身份点亮 run fence，再把 typed marker
/// 交给每 call 收集器，由包装器写进真实 [`ToolContext`]，让 canonical 终态读成
/// `Unknown`。直接、无引擎的调用既没有 fence 也没有收集器，因此保持原有 `String`
/// 语义，不虚构身份。
pub fn publish_supervision_unknown(effect: UnknownSideEffect) -> Option<Arc<UnknownEffectFence>> {
    let fence = current_run_fence();
    if let Some(fence) = &fence {
        fence.mark(effect.clone());
    }
    if let Some(collector) = current_unknown_effect_collector()
        && collector.try_publish(effect.clone()).is_err()
    {
        // The bounded collector refused the record. That refusal is never
        // swallowed: the run guard fails closed from here on (no later write or
        // external dispatch, because the host cannot be shown complete evidence),
        // and the dropped per-call marker is recorded as typed fence evidence.
        // The native path yields at most one loss per stream, so this branch is
        // defensive unless a future caller exceeds the per-call bound.
        if let Some(fence) = &fence {
            fence.mark_persist_failed();
            fence.mark(UnknownSideEffect::new(
                effect.operation.clone(),
                effect.target.clone(),
                UnknownReason::MarkerEncoding,
                effect.tool_call_id.clone(),
            ));
        }
    }
    fence
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fence_is_bounded_and_stays_tripped_after_overflow() {
        let fence = UnknownEffectFence::new();
        assert!(!fence.has_unknown());
        for index in 0..MAX_UNKNOWN_EFFECTS {
            assert!(fence.mark(UnknownSideEffect::new(
                format!("op{index}"),
                "target",
                UnknownReason::McpTransport,
                None,
            )));
        }
        assert_eq!(fence.len(), MAX_UNKNOWN_EFFECTS);
        assert!(
            !fence.mark(UnknownSideEffect::new(
                "extra",
                "target",
                UnknownReason::McpTransport,
                None,
            )),
            "the fence must not evict an older record"
        );
        assert!(fence.overflowed());
        assert!(fence.has_unknown());
    }

    #[test]
    fn duplicate_marks_do_not_grow_or_reclassify() {
        let fence = UnknownEffectFence::new();
        let effect = UnknownSideEffect::new("op", "target", UnknownReason::McpTransport, None);
        assert!(fence.mark(effect.clone()));
        assert!(!fence.mark(effect));
        assert_eq!(fence.len(), 1);
    }

    #[test]
    fn separate_unknowns_with_the_same_tool_id_do_not_collapse() {
        let fence = UnknownEffectFence::new();
        let mut first = UnknownSideEffect::new(
            "mcp__probe__write",
            "same target",
            UnknownReason::McpTransport,
            Some("call_1".into()),
        );
        first.scope = Some(uuid::Uuid::from_u128(1));
        let mut second = UnknownSideEffect::new(
            "mcp__probe__write",
            "same target",
            UnknownReason::McpTransport,
            Some("call_1".into()),
        );
        second.scope = Some(uuid::Uuid::from_u128(2));
        assert!(fence.mark(first.clone()));
        let earlier = fence.evidence();
        assert!(fence.mark(second));
        assert!(!earlier.same_revision(&fence.evidence()));
        assert_eq!(fence.len(), 2);
        assert!(
            !fence.mark(first),
            "a clone of the same fact is still idempotent"
        );
    }

    #[test]
    fn old_unknown_markers_do_not_gain_invented_identity() {
        let old: UnknownSideEffect = serde_json::from_value(serde_json::json!({
            "operation":"mcp__probe__write", "target":"same target",
            "reason":"mcp_transport", "tool_call_id":"call_1"
        }))
        .unwrap();
        assert!(old.scope.is_none());
        assert!(old.evidence_id.is_none());
        let fence = UnknownEffectFence::new();
        assert!(fence.mark(old.clone()));
        let first = fence.evidence();
        assert!(fence.mark(old));
        assert!(
            !first.same_revision(&fence.evidence()),
            "unidentified observations cannot safely be assumed to be duplicates"
        );
    }

    #[test]
    fn scoped_fences_are_shared_and_isolated_by_scope() {
        let first = fence_for_scope("run-a");
        assert!(Arc::ptr_eq(&first, &fence_for_scope("run-a")));
        assert!(!Arc::ptr_eq(&first, &fence_for_scope("run-b")));
        let alias = fence_for_scope("run-child");
        let registered = alias_scope("run-child", &first);
        assert!(registered, "an alias under the bound must be registered");
        assert!(Arc::ptr_eq(&first, &fence_for_scope("run-child")));
        assert_eq!(alias.len(), 0);
    }

    #[test]
    fn persist_failure_is_monotonic() {
        let fence = UnknownEffectFence::new();
        assert!(!fence.persist_failed());
        fence.mark_persist_failed();
        assert!(fence.persist_failed());
    }

    #[test]
    fn evidence_revision_advances_on_every_new_record_and_fail_closed_state() {
        let fence = UnknownEffectFence::new();
        let effect = UnknownSideEffect::new("op", "target", UnknownReason::McpTransport, None);
        let start = fence.evidence();
        assert_eq!(start.revision(), 0);
        assert!(fence.mark(effect.clone()));
        let marked = fence.evidence();
        assert!(!start.same_revision(&marked));
        assert!(marked.has_unknown());
        // A duplicate adds nothing and must not advance the revision.
        assert!(!fence.mark(effect));
        assert!(marked.same_revision(&fence.evidence()));
        // Overflow and a persistence failure are evidence changes too.
        for index in 0..MAX_UNKNOWN_EFFECTS {
            fence.mark(UnknownSideEffect::new(
                format!("extra-{index}"),
                "target",
                UnknownReason::McpTransport,
                None,
            ));
        }
        assert!(fence.overflowed());
        let overflowed = fence.evidence();
        assert!(overflowed.is_fail_closed());
        assert_eq!(overflowed.unknown_count(), MAX_UNKNOWN_EFFECTS);
        fence.mark_persist_failed();
        assert!(fence.evidence().is_fail_closed());
        assert!(!overflowed.same_revision(&fence.evidence()));
        let persisted = fence.evidence();
        fence.mark_persist_failed();
        assert!(persisted.same_revision(&fence.evidence()));
    }

    #[test]
    fn recovered_markers_keep_real_ledger_identity_without_inventing_ids() {
        let recovered = UnknownSideEffect::recovered(
            "mcp__srv__write",
            UnknownReason::InterruptedDispatch,
            "run-scope",
            7,
            "a".repeat(64),
        );
        assert!(recovered.evidence_id.is_none());
        assert!(recovered.scope.is_none());
        assert_eq!(recovered.ledger_key(), Some(("run-scope", 7)));
        assert_eq!(recovered.ledger_snapshot(), Some("a".repeat(64).as_str()));
        let value = serde_json::to_value(&recovered).unwrap();
        assert_eq!(value["ledger_scope"], "run-scope");
        assert_eq!(value["ledger_call_index"], 7);
        assert_eq!(value["reason"], "interrupted_dispatch");
    }

    #[test]
    fn re_seeding_the_same_ledger_key_is_idempotent_even_without_evidence_id() {
        let fence = UnknownEffectFence::new();
        let recovered = UnknownSideEffect::recovered(
            "write",
            UnknownReason::InterruptedDispatch,
            "run-a",
            1,
            "b".repeat(64),
        );
        assert!(fence.mark(recovered.clone()));
        assert!(
            !fence.mark(recovered),
            "the same real ledger key must not be recorded twice"
        );
        assert_eq!(fence.len(), 1);
        assert!(!fence.overflowed());
        let other = UnknownSideEffect::recovered(
            "write",
            UnknownReason::InterruptedDispatch,
            "run-a",
            2,
            "c".repeat(64),
        );
        assert!(fence.mark(other), "a different call index is a new fact");
        assert_eq!(fence.len(), 2);
    }

    #[test]
    fn a_new_snapshot_for_the_same_ledger_key_updates_the_evidence_and_revision() {
        let fence = UnknownEffectFence::new();
        let first = UnknownSideEffect::recovered(
            "write",
            UnknownReason::InterruptedDispatch,
            "run-a",
            1,
            "b".repeat(64),
        );
        assert!(fence.mark(first.clone()));
        let shown = fence.evidence();

        // The same durable key gained a fact after the host was shown the old
        // digest. Key-only dedup must not swallow it: the stored snapshot and
        // the evidence revision both move so an old confirmation cannot pass.
        let second = UnknownSideEffect::recovered(
            "write",
            UnknownReason::InterruptedDispatch,
            "run-a",
            1,
            "c".repeat(64),
        );
        assert!(fence.mark(second.clone()));
        let after = fence.evidence();
        assert!(!shown.same_revision(&after));
        assert_eq!(after.unknown_count(), 1, "the key is still one record");
        assert_eq!(
            after.unknowns()[0].ledger_snapshot(),
            Some("c".repeat(64).as_str())
        );

        // Re-seeding the identical new snapshot is the same fact again.
        assert!(!fence.mark(second));
        assert!(after.same_revision(&fence.evidence()));
    }

    #[test]
    fn legacy_serialized_markers_default_the_ledger_fields_to_none() {
        let legacy: UnknownSideEffect = serde_json::from_value(serde_json::json!({
            "operation": "write", "target": "t", "reason": "interrupted_dispatch"
        }))
        .unwrap();
        assert!(legacy.ledger_scope.is_none());
        assert!(legacy.ledger_call_index.is_none());
        assert!(legacy.ledger_snapshot_key.is_none());
        assert!(legacy.ledger_key().is_none());
        assert!(legacy.ledger_snapshot().is_none());
    }

    /// A collector that refuses a native marker (the per-call bound) must never
    /// swallow the loss: the run fence fails closed from then on and the refused
    /// marker is recorded as typed evidence instead of disappearing.
    #[tokio::test]
    async fn a_refused_native_marker_fails_the_run_closed() {
        let collector = UnknownEffectCollector::new();
        let run_id = uuid::Uuid::new_v4();
        let identity = crate::tool_identity::ToolIdentity::new(run_id, "call-bound", 1, 11);
        let fence = fence_for_scope(&run_id.to_string());
        crate::tool_identity::scope(
            Some(identity),
            scope_unknown_effects(collector.clone(), async {
                for index in 0..MAX_UNKNOWN_EFFECTS_PER_CALL {
                    let effect = UnknownSideEffect::new(
                        "shell",
                        format!("target-{index}"),
                        UnknownReason::NativeTimeout,
                        Some("call-bound".into()),
                    );
                    assert!(publish_supervision_unknown(effect).is_some());
                }
                let refused = UnknownSideEffect::new(
                    "shell",
                    "target-overflow",
                    UnknownReason::NativeCapture,
                    Some("call-bound".into()),
                );
                assert!(publish_supervision_unknown(refused).is_some());
            }),
        )
        .await;

        assert_eq!(collector.len(), MAX_UNKNOWN_EFFECTS_PER_CALL);
        let evidence = fence.evidence();
        assert!(
            evidence.is_fail_closed(),
            "a refused marker must fail the run closed: {evidence:?}"
        );
        assert!(evidence.persist_failed());
        assert!(
            evidence
                .unknowns()
                .iter()
                .any(|record| record.reason == UnknownReason::MarkerEncoding),
            "the refused marker must be recorded, not dropped: {evidence:?}"
        );
    }
}
