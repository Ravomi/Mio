//! Unified tool admission gate (REQ-POLICY / REQ-PROCESS).
//!
//! Every tool entry - built-in tools, the aggregate `workspace` tool, plugin
//! tools, MCP tools, host-provided `extra_tools` and native delegation - is
//! admitted through one decision function at the real dispatch boundary. The
//! gate runs after all argument-patching and admission hooks (only pure
//! observers may follow), so it sees the final
//! arguments after every earlier hook's patch, and it denies before the tool
//! handler is reached: a denial or an approval timeout therefore has no side
//! effect.
//!
//! The gate keeps no accepted approval. A call is approved at its own
//! dispatch and never cached, so a denied, cancelled or never-executed call
//! cannot donate an approval to a later, identical call. The exact parameters
//! and dispatch context are folded into a SHA-256 binding that is handed to
//! [`Host::approve_tool_binding`]; the default host implementation forwards to
//! the legacy [`Host::approve_tool_async`] so existing hosts keep working.
//!
//! On top of the per-call approval, the gate keeps a bounded, fail-closed
//! single-use ledger of the provider's own tool-call ids: one
//! `(scope, tool_call_id)` is admitted or refused at most once per run, and an
//! attempt that was denied, timed out or cancelled never releases its id for a
//! replay. The ledger holds only SHA-256 digests - never a raw id or the call's
//! arguments - and every clone of the run's gate shares it.

use crate::{
    Host,
    pattern::{BUILTIN_TOOL_NAMES, ToolFilter},
    tool_lifecycle::ToolCallTracker,
    unknown_effect::{UnknownEffectEvidence, UnknownEffectFence, UnknownSideEffect},
};
use mio_agent::agent::{
    AgentHook, DispatchAction, DispatchEvent, HookContext, OutcomeAction, OutcomeEvent,
    StepEventKind,
};
use mio_core::{
    effect::{EffectKind, Outcome},
    tool::{ToolExecutionError, ToolResult},
};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

/// Where an admitted tool call comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// A directly registered built-in tool (`read`, `write`, `bash`, ...).
    Builtin,
    /// The aggregate `workspace` tool, classified by its inner operation.
    Aggregate,
    /// A declarative plugin tool (`plugin__<plugin>__<tool>`).
    Plugin,
    /// A tool bridged from an MCP server (`mcp__<server>__<tool>`).
    Mcp,
    /// A host-provided dynamic tool with no declared side effect.
    Extra,
    /// The native delegation control tool.
    Subagent,
}

/// How conservative the gate is about a call's effects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffect {
    /// Reading or searching only.
    Read,
    /// A local file/system write, including shell execution.
    Write,
    /// An out-of-process call whose effects the engine cannot bound.
    External,
}

/// One normalized tool call, produced before the tool handler runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormalizedCall {
    /// Entry that produced the call.
    pub entry: EntryKind,
    /// Canonical tool identity used for filtering.
    pub tool: String,
    /// Approval operation label. Built-in writes keep their historical label
    /// (`write`, `edit`, `shell`); plugin and MCP tools keep their qualified
    /// name.
    pub operation: String,
    /// Human-readable target shown to an approving host.
    pub display_target: String,
    /// Conservative effect classification.
    pub side_effect: SideEffect,
    /// Whether this call requires an approval before dispatch.
    pub requires_approval: bool,
    /// Every name that identifies the same tool (deny wins across all of them).
    pub aliases: Vec<String>,
    /// SHA-256 over the entry, canonical identity, parameters and context.
    pub params_sha256: String,
}

/// The binding passed to [`Host::approve_tool_binding`].
///
/// It pins an approval to the exact tool, normalized parameters and dispatch
/// context of the call. A host that overrides the method can reject a binding
/// it never approved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalBinding {
    /// Entry that produced the call.
    pub entry: EntryKind,
    /// Canonical tool identity.
    pub tool: String,
    /// Approval operation label.
    pub operation: String,
    /// Canonical workspace root the call runs against.
    pub cwd: String,
    /// Dispatch scope (the run UUID), so the same call in another run is a
    /// different binding.
    pub scope: String,
    /// SHA-256 over the call's normalized parameters.
    pub params_sha256: String,
    /// Unique approval attempt; equal parameters never reuse an earlier call.
    /// This is deliberately separate from `tool_call_id`: an approval attempt
    /// must never impersonate the model's own tool-call id.
    pub call_id: String,
    /// The model's own tool-call id this approval belongs to, when the call
    /// came from a model tool call. `None` for calls without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// Why one call was refused before its handler ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyDenial(String);

impl PolicyDenial {
    /// Build a denial with a model-visible reason.
    pub fn new(reason: impl Into<String>) -> Self {
        Self(reason.into())
    }

    /// The model-visible reason.
    pub fn reason(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PolicyDenial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Stable context an approval is bound to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyContext {
    cwd: String,
    scope: String,
}

impl PolicyContext {
    /// Build a context from the canonical workspace root and a run scope.
    pub fn new(cwd: impl Into<String>, scope: impl Into<String>) -> Self {
        Self {
            cwd: cwd.into(),
            scope: scope.into(),
        }
    }

    /// Canonical workspace root.
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Run scope (a UUID string for engine runs).
    pub fn scope(&self) -> &str {
        &self.scope
    }
}

/// The most distinct provider tool-call ids one run's gate remembers.
///
/// The ledger never evicts, so a run that exceeds this many distinct ids fails
/// closed: every new id is refused instead of silently forgetting an id that
/// was already admitted. Each entry is one 32-byte SHA-256 digest, never a raw
/// id or the call's arguments.
pub const TOOL_CALL_CLAIMS_CAPACITY: usize = 4096;

/// The longest provider tool-call id accepted, in bytes.
const MAX_TOOL_CALL_ID_BYTES: usize = 512;

/// The single admission gate for one run.
///
/// Cloning shares the gate, its approval counters, its claim ledger, its
/// unknown-effect fence and the host handle, so the hook, the built-in tools
/// and the run all observe the same decisions.
#[derive(Clone)]
pub struct ToolPolicy(
    Arc<ToolPolicyInner>,
    std::time::Duration,
    Arc<UnknownEffectFence>,
);

struct ToolPolicyInner {
    filter: ToolFilter,
    read_only: bool,
    host: Arc<dyn Host>,
    context: PolicyContext,
    approvals: AtomicUsize,
    denials: AtomicUsize,
    claims: ToolCallClaims,
    /// The run's authoritative tool-call state machine, shared with the final
    /// dispatch hook so admitted/dispatched/terminal phases are tracked once.
    lifecycle: Arc<ToolCallTracker>,
}

/// Bounded, fail-closed ledger of the provider tool-call ids a run handled.
///
/// One `(scope, tool_call_id)` claims exactly one slot, atomically, before any
/// approval or execution. Entries are never removed or evicted: a duplicate id
/// is a protocol refusal, and a full ledger refuses new ids rather than
/// dropping an older entry that was already admitted. Only a SHA-256 digest of
/// the scope and id is stored, so a raw id, the call's arguments and any
/// credential never enter the ledger.
struct ToolCallClaims {
    seen: Mutex<HashSet<[u8; 32]>>,
}

impl ToolCallClaims {
    fn new() -> Self {
        Self {
            seen: Mutex::new(HashSet::new()),
        }
    }

    /// Atomically claim `call_id` for `scope`, or refuse the call.
    ///
    /// A refusal happens before any approval or execution and never releases
    /// an earlier claim: an id that was approved, denied, timed out or
    /// cancelled stays claimed, so only a new id starts a new attempt.
    fn claim(&self, scope: &str, call_id: &str) -> Result<(), PolicyDenial> {
        validate_tool_call_id(call_id)?;
        let key = claim_key(scope, call_id);
        let Ok(mut seen) = self.seen.lock() else {
            // A poisoned ledger cannot prove single use: fail closed.
            return Err(PolicyDenial::new(
                "the tool call id ledger is unavailable; refusing the call",
            ));
        };
        if seen.contains(&key) {
            return Err(PolicyDenial::new(format!(
                "duplicate tool call id '{call_id}': this run already admitted or refused it"
            )));
        }
        if seen.len() >= TOOL_CALL_CLAIMS_CAPACITY {
            return Err(PolicyDenial::new(format!(
                "the tool call id ledger is full ({TOOL_CALL_CLAIMS_CAPACITY} distinct ids); refusing a new id"
            )));
        }
        seen.insert(key);
        Ok(())
    }

    /// How many distinct ids the ledger holds; it never shrinks.
    fn len(&self) -> usize {
        self.seen.lock().map(|seen| seen.len()).unwrap_or(0)
    }
}

/// The ledger key for one provider tool-call id within a scope.
///
/// The scope is folded in with a NUL separator even though a ledger already
/// belongs to a single run, so a key can never alias a different
/// `(scope, id)` pair.
fn claim_key(scope: &str, call_id: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(scope.as_bytes());
    hasher.update([0]);
    hasher.update(call_id.as_bytes());
    hasher.finalize().into()
}

/// Fail closed on a provider id that cannot be a real, single-use identifier.
fn validate_tool_call_id(call_id: &str) -> Result<(), PolicyDenial> {
    if call_id.is_empty() || call_id.chars().all(char::is_whitespace) {
        return Err(PolicyDenial::new(
            "the provider sent an empty tool call id; refusing the call",
        ));
    }
    if call_id.len() > MAX_TOOL_CALL_ID_BYTES {
        return Err(PolicyDenial::new(format!(
            "the provider tool call id exceeds {MAX_TOOL_CALL_ID_BYTES} bytes; refusing the call"
        )));
    }
    if call_id.chars().any(char::is_control) {
        return Err(PolicyDenial::new(
            "the provider tool call id contains control characters; refusing the call",
        ));
    }
    Ok(())
}

impl ToolPolicy {
    /// Build a gate for one run.
    pub fn new(
        filter: ToolFilter,
        read_only: bool,
        host: Arc<dyn Host>,
        context: PolicyContext,
    ) -> Self {
        // The gate owns the run's unknown-effect fence. A tool body that really
        // observed an unknown reaches the same fence through the run scope, and
        // a delegated child host overrides it with the parent's via
        // [`ToolPolicy::with_unknown_fence`].
        let fence = crate::unknown_effect::fence_for_scope(context.scope());
        let lifecycle = ToolCallTracker::new(context.scope(), host.clone());
        Self(
            Arc::new(ToolPolicyInner {
                filter,
                read_only,
                host,
                context,
                approvals: AtomicUsize::new(0),
                denials: AtomicUsize::new(0),
                claims: ToolCallClaims::new(),
                lifecycle,
            }),
            std::time::Duration::from_secs(120),
            fence,
        )
    }

    /// Share another fence instead of the one this run's scope resolved.
    ///
    /// A delegated child uses its parent's fence so an unknown observed in
    /// either run blocks new side effects in both. The child's own scope is
    /// aliased to the shared fence, so a lazily-loaded child tool resolves the
    /// same guard. When the bounded registry cannot save that association, the
    /// scope fails closed on an explicitly unavailable guard instead of being
    /// gated on a fence its own tools cannot reach; the shared fence itself is
    /// left untouched.
    pub fn with_unknown_fence(mut self, fence: Arc<UnknownEffectFence>) -> Self {
        if crate::unknown_effect::alias_scope(self.0.context.scope(), &fence) || fence.unavailable()
        {
            self.2 = fence;
        } else {
            self.2 = UnknownEffectFence::unavailable_guard();
        }
        self
    }

    /// The run's unknown-effect fence.
    pub fn unknown_fence(&self) -> &Arc<UnknownEffectFence> {
        &self.2
    }

    /// The run's authoritative tool-call lifecycle tracker.
    ///
    /// Shared with the final dispatch hook, so the whole run tracks one set of
    /// call phases instead of keeping a shadow ledger beside the tool body.
    pub fn lifecycle(&self) -> &Arc<ToolCallTracker> {
        &self.0.lifecycle
    }

    /// Bound an approval wait. Expiration denies the dispatch before execution.
    pub fn with_approval_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.1 = timeout;
        self
    }

    /// The run's tool filter.
    pub fn filter(&self) -> &ToolFilter {
        &self.0.filter
    }

    /// Whether the run only reads.
    pub fn is_read_only(&self) -> bool {
        self.0.read_only
    }

    /// The binding context.
    pub fn context(&self) -> &PolicyContext {
        &self.0.context
    }

    /// How many approvals this run requested.
    pub fn approval_count(&self) -> usize {
        self.0.approvals.load(Ordering::SeqCst)
    }

    /// How many calls this run refused.
    pub fn denial_count(&self) -> usize {
        self.0.denials.load(Ordering::SeqCst)
    }

    /// How many distinct provider tool-call ids this run has claimed.
    ///
    /// The ledger is bounded by [`TOOL_CALL_CLAIMS_CAPACITY`], never shrinks
    /// and never evicts, so this is the authoritative saturation check.
    pub fn claim_count(&self) -> usize {
        self.0.claims.len()
    }

    /// Admit one tool call, or refuse it before any side effect.
    ///
    /// The decision order is deny-first: the tool filter must allow every
    /// alias, a read-only run refuses anything that is not a read, and only
    /// then is approval requested for side-effecting calls that need it.
    pub async fn admit(&self, tool_name: &str, args: &str) -> Result<NormalizedCall, PolicyDenial> {
        self.admit_with_call_id(tool_name, args, None).await
    }

    /// [`ToolPolicy::admit`] carrying the model's own tool-call id.
    ///
    /// The id is recorded on the approval binding separately from the
    /// approval-attempt `call_id`, so an approval can be correlated back to the
    /// exact model tool call without one id pretending to be the other. A real
    /// id is also single use: it is claimed atomically before any approval or
    /// execution, so the same `(scope, id)` is admitted at most once and a
    /// refused, denied or cancelled attempt is not replayable under that id.
    /// `None` keeps the direct SDK path unchanged instead of inventing an id.
    pub async fn admit_with_call_id(
        &self,
        tool_name: &str,
        args: &str,
        tool_call_id: Option<&str>,
    ) -> Result<NormalizedCall, PolicyDenial> {
        self.admit_dispatch(tool_name, args, tool_call_id, None, None)
            .await
    }

    /// [`ToolPolicy::admit_with_call_id`] at a real dispatch boundary.
    ///
    /// `turn` and `effect_id` are the engine's real dispatch identity, recorded
    /// on the run's canonical tool-call state so the final dispatch hook and a
    /// later recovery can correlate the call without guessing from order. A
    /// direct SDK admit passes `None` for both instead of fabricating them.
    ///
    /// The canonical record is created as `Normalized` after final parameter
    /// normalization and before approval; a refusal writes `Denied` before any
    /// dispatch and with zero side effect; a successful admission writes
    /// `Admitted`, which is policy admission and never a claim of user
    /// approval. A persistence failure refuses the call instead of dispatching
    /// without durable evidence.
    pub async fn admit_dispatch(
        &self,
        tool_name: &str,
        args: &str,
        tool_call_id: Option<&str>,
        turn: Option<u64>,
        effect_id: Option<u64>,
    ) -> Result<NormalizedCall, PolicyDenial> {
        if let Some(tool_call_id) = tool_call_id
            && let Err(denial) = self.0.claims.claim(&self.0.context.scope, tool_call_id)
        {
            self.0.denials.fetch_add(1, Ordering::SeqCst);
            return Err(denial);
        }
        let call = classify(tool_name, args, &self.0.context)?;
        let (canonical_target, target_redacted) = canonical_evidence_target(&call);
        let index = match self.0.lifecycle.begin_normalized(
            &call.tool,
            &call.operation,
            canonical_target.as_str(),
            target_redacted,
            &call.params_sha256,
            tool_call_id,
            turn,
            effect_id,
        ) {
            Ok(index) => index,
            Err(error) => {
                self.0.denials.fetch_add(1, Ordering::SeqCst);
                return Err(PolicyDenial::new(format!(
                    "the tool call's normalized state could not be recorded; refusing '{}': {error}",
                    call.tool
                )));
            }
        };
        let deny = |reason: String| -> PolicyDenial {
            let _ = self.0.lifecycle.deny(index, &reason);
            self.0.denials.fetch_add(1, Ordering::SeqCst);
            PolicyDenial::new(reason)
        };
        if !self.allowed(&call) {
            return Err(deny(format!(
                "the run's tool filter does not allow '{}'",
                call.tool
            )));
        }
        if self.0.read_only && call.side_effect != SideEffect::Read {
            return Err(deny(format!(
                "a read-only (只读) run cannot execute '{}'",
                call.tool
            )));
        }
        // An earlier unknown result in this run tree blocks a new
        // write/external side effect until the host explicitly confirms it;
        // read tooling stays available for inspection. The confirmation is
        // asked for at each dispatch and never cached, and the normal
        // parameter-bound approval below still runs afterwards.
        //
        // The evidence is captured once as an atomic snapshot with a
        // monotonic revision, and that same version is re-verified after the
        // confirmation and after the normal approval. A confirmation or
        // approval therefore only ever releases the call against the exact
        // evidence the host was shown: a new unknown recorded while either
        // `await` was pending refuses the attempt instead of reusing stale
        // evidence. A refused attempt needs a new tool call id to try again;
        // nothing here re-asks in a loop.
        let evidence = self.2.evidence();
        if call.side_effect != SideEffect::Read {
            if evidence.is_fail_closed() {
                return Err(deny(
                    fail_closed_denial(&call, &evidence).reason().to_owned(),
                ));
            }
            if evidence.has_unknown() {
                let confirmed = tokio::time::timeout(
                    self.1,
                    self.0
                        .host
                        .confirm_unknown_side_effect(&call, evidence.unknowns()),
                )
                .await;
                match confirmed {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => return Err(deny(error)),
                    Err(_) => {
                        return Err(deny("unknown side-effect confirmation timed out".into()));
                    }
                }
                if !self.2.evidence().same_revision(&evidence) {
                    return Err(deny(
                        self.changed_evidence_denial(&call, &evidence)
                            .reason()
                            .to_owned(),
                    ));
                }
            }
        }
        if call.requires_approval {
            self.0.approvals.fetch_add(1, Ordering::SeqCst);
            let binding = ApprovalBinding {
                entry: call.entry,
                tool: call.tool.clone(),
                operation: call.operation.clone(),
                cwd: self.0.context.cwd.clone(),
                scope: self.0.context.scope.clone(),
                params_sha256: call.params_sha256.clone(),
                call_id: uuid::Uuid::new_v4().to_string(),
                tool_call_id: tool_call_id.map(str::to_owned),
            };
            // Persist the approval attempt id on the canonical record before
            // asking, so an approval denial and an approval success carry the
            // same real attempt id. A denial still leaves the call `Denied`.
            if let Err(error) = self.0.lifecycle.set_call_id(index, &binding.call_id) {
                return Err(deny(format!(
                    "the tool call's approval id could not be recorded; refusing: {error}"
                )));
            }
            let approval = tokio::time::timeout(
                self.1,
                self.0
                    .host
                    .approve_tool_binding(&call.operation, &call.display_target, &binding),
            )
            .await;
            match approval {
                Ok(Ok(())) => {
                    // Final admit: the call is released only while the evidence
                    // version the gate started from is still current. An
                    // unknown recorded during the approval (including a first
                    // one when the fence started empty) refuses the dispatch.
                    if call.side_effect != SideEffect::Read {
                        let after = self.2.evidence();
                        if !after.same_revision(&evidence) {
                            return Err(deny(
                                self.changed_evidence_denial(&call, &evidence)
                                    .reason()
                                    .to_owned(),
                            ));
                        }
                    }
                }
                Ok(Err(error)) => return Err(deny(error)),
                Err(_) => return Err(deny("tool approval timed out".into())),
            }
        }
        if let Err(error) = self.0.lifecycle.admit(index, turn, effect_id) {
            return Err(deny(format!(
                "the tool call's admitted state could not be recorded; refusing: {error}"
            )));
        }
        Ok(call)
    }

    /// Deny a call whose evidence version changed while the gate was waiting,
    /// naming the permanent fail-closed state when the change was one.
    fn changed_evidence_denial(
        &self,
        call: &NormalizedCall,
        previous: &UnknownEffectEvidence,
    ) -> PolicyDenial {
        let after = self.2.evidence();
        if after.is_fail_closed() {
            return fail_closed_denial(call, &after);
        }
        debug_assert!(!after.same_revision(previous));
        evidence_changed_denial(call)
    }

    fn allowed(&self, call: &NormalizedCall) -> bool {
        match call.entry {
            EntryKind::Aggregate => self.0.filter.allows_workspace_operation(&call.operation),
            EntryKind::Subagent => self.0.filter.allows("subagent"),
            EntryKind::Plugin => {
                let aliases: Vec<&str> = call.aliases.iter().map(String::as_str).collect();
                self.0.filter.allows_aliased(&aliases)
            }
            EntryKind::Builtin | EntryKind::Mcp | EntryKind::Extra => {
                self.0.filter.allows(&call.tool)
            }
        }
    }
}

/// The dispatch-boundary hook that runs the gate.
///
/// It must be registered after every argument-patching hook, so the approved
/// snapshot is the effect the bus actually dispatches.
#[derive(Clone)]
pub struct PolicyHook {
    policy: ToolPolicy,
}

impl PolicyHook {
    /// Build a hook over a run's gate.
    pub fn new(policy: ToolPolicy) -> Self {
        Self { policy }
    }

    /// The gate this hook enforces.
    pub fn policy(&self) -> &ToolPolicy {
        &self.policy
    }
}

impl AgentHook for PolicyHook {
    fn observes(&self, kind: StepEventKind) -> bool {
        matches!(kind, StepEventKind::ToolDispatch)
    }

    async fn on_dispatch(&self, _: &HookContext, event: DispatchEvent<'_>) -> DispatchAction {
        let EffectKind::ToolCall { name, args } = event.kind else {
            return DispatchAction::Proceed;
        };
        let tool_call_id = event.call_id.map(|call| call.wire().into_owned());
        match self
            .policy
            .admit_dispatch(
                name.as_str(),
                args.as_str(),
                tool_call_id.as_deref(),
                Some(event.turn as u64),
                Some(event.id.as_u64()),
            )
            .await
        {
            Ok(_) => DispatchAction::Proceed,
            Err(denial) => DispatchAction::skip(
                serde_json::json!({
                    "error": format!("Tool policy denied '{name}': {denial}")
                })
                .to_string(),
            ),
        }
    }

    /// Read a tool's published typed unknown marker and trip the run's fence.
    ///
    /// This is the engine-driven backup for the tool's own direct mark: it
    /// works even when the tool only published the marker, and it is
    /// idempotent, so it never records a second copy of the same effect. It is
    /// deliberately not a place to journal: the tool's own host callback owns
    /// the durable record, and the run's `finish_run` already turns a leftover
    /// pending intent into `Unknown`.
    async fn on_outcome(&self, _: &HookContext, event: OutcomeEvent<'_>) -> OutcomeAction {
        if let Some(effect) = event
            .tool_context()
            .and_then(|context| context.result::<UnknownSideEffect>().ok().flatten())
        {
            self.policy.unknown_fence().mark(effect);
            if let Some(result) = event.tool_result() {
                // Some extension APIs preserve their legacy JSON error payload
                // as an Ok output. A typed unknown must still be an error at
                // the engine boundary, with automatic retry explicitly off.
                let error = result.error().cloned().unwrap_or_else(|| {
                    ToolExecutionError::other("the tool's side effect is unknown")
                });
                return OutcomeAction::Replace(Ok(Outcome::ToolResult {
                    result: ToolResult::failed(
                        error
                            .with_retryable(false)
                            .with_model_output(result.output().clone()),
                    ),
                }));
            }
        }
        OutcomeAction::Proceed
    }
}

/// Deny a side effect because the fence's own evidence is permanently
/// unusable: an incomplete snapshot (overflow), a durable record that could not
/// be written, or a scope the bounded registry could not track. No host
/// confirmation is asked for, because the host cannot be shown a complete
/// snapshot.
fn fail_closed_denial(call: &NormalizedCall, evidence: &UnknownEffectEvidence) -> PolicyDenial {
    let reason = if evidence.persist_failed() {
        "an earlier call's result is unknown and its evidence could not be persisted"
    } else if evidence.overflowed() {
        "the run's unknown-effect evidence overflowed and cannot be shown completely"
    } else {
        "the run's unknown-effect guard is unavailable because its scope could not be registered"
    };
    PolicyDenial::new(format!("{reason}; refusing '{}'", call.tool))
}

/// Deny a side effect whose evidence version changed while the gate was
/// waiting. The earlier confirmation or approval only covered the version the
/// host was shown, so the attempt is refused and a new tool call id is required
/// to try again.
fn evidence_changed_denial(call: &NormalizedCall) -> PolicyDenial {
    PolicyDenial::new(format!(
        "a new unknown side effect appeared while '{}' was awaiting admission; refusing this attempt, which needs a new tool call id to try again",
        call.tool
    ))
}

/// The parsed identity and effect classification of one call.
struct Classified {
    entry: EntryKind,
    tool: String,
    operation: String,
    display_target: String,
    side_effect: SideEffect,
    requires_approval: bool,
}

/// Normalize a raw tool call into its canonical identity, effect class and
/// parameter binding.
fn classify(
    tool_name: &str,
    args: &str,
    context: &PolicyContext,
) -> Result<NormalizedCall, PolicyDenial> {
    let params =
        serde_json::from_str::<Value>(args).unwrap_or_else(|_| Value::String(args.to_owned()));
    let classified = match tool_name {
        "workspace" => classify_aggregate(&params)?,
        "read" | "grep" | "find" | "ls" => Classified {
            entry: EntryKind::Builtin,
            tool: tool_name.to_owned(),
            operation: tool_name.to_owned(),
            display_target: string_field(&params, "path")
                .or_else(|| string_field(&params, "pattern"))
                .unwrap_or_default(),
            side_effect: SideEffect::Read,
            requires_approval: false,
        },
        "write" | "edit" => Classified {
            entry: EntryKind::Builtin,
            tool: tool_name.to_owned(),
            operation: tool_name.to_owned(),
            display_target: string_field(&params, "path").unwrap_or_default(),
            side_effect: SideEffect::Write,
            requires_approval: true,
        },
        "bash" | "shell" => Classified {
            entry: EntryKind::Builtin,
            tool: "shell".to_owned(),
            operation: "shell".to_owned(),
            display_target: string_field(&params, "command")
                .or_else(|| string_field(&params, "value"))
                .unwrap_or_default(),
            side_effect: SideEffect::Write,
            requires_approval: true,
        },
        "subagent" => Classified {
            entry: EntryKind::Subagent,
            // Delegation is a control plane: the child's own dispatch gate
            // approves any side effect the child attempts.
            tool: "subagent".to_owned(),
            operation: "subagent".to_owned(),
            display_target: string_field(&params, "operation").unwrap_or_default(),
            side_effect: if matches!(
                string_field(&params, "operation").as_deref(),
                Some("status" | "snapshot" | "list")
            ) {
                SideEffect::Read
            } else {
                SideEffect::External
            },
            requires_approval: false,
        },
        name if name.starts_with("plugin__") => Classified {
            entry: EntryKind::Plugin,
            tool: name.to_owned(),
            operation: name.to_owned(),
            display_target: args.to_owned(),
            side_effect: SideEffect::External,
            requires_approval: true,
        },
        name if name.starts_with("mcp__") => Classified {
            entry: EntryKind::Mcp,
            tool: name.to_owned(),
            operation: name.to_owned(),
            display_target: args.to_owned(),
            side_effect: SideEffect::External,
            requires_approval: true,
        },
        name => Classified {
            // An unknown dynamic tool has no declared effect: treat it as an
            // external action and require approval.
            entry: EntryKind::Extra,
            tool: name.to_owned(),
            operation: name.to_owned(),
            display_target: args.to_owned(),
            side_effect: SideEffect::External,
            requires_approval: true,
        },
    };
    let aliases = aliases_for(tool_name, &classified.tool);
    let params_sha256 = digest(
        classified.entry,
        &classified.tool,
        &classified.operation,
        &params,
        context,
    );
    Ok(NormalizedCall {
        entry: classified.entry,
        tool: classified.tool,
        operation: classified.operation,
        display_target: classified.display_target,
        side_effect: classified.side_effect,
        requires_approval: classified.requires_approval,
        aliases,
        params_sha256,
    })
}

/// The bounded, content-free target stored on the canonical record.
///
/// A built-in/aggregate/subagent target is already a short human label. A
/// plugin/MCP/extra call arrives as raw JSON arguments, which must never be
/// copied into durable evidence; it is summarized as a field count and marked
/// redacted. The full arguments are still shown to the approving host through
/// [`ApprovalBinding`], so operator visibility is unchanged, and the real
/// committed output references stay on the canonical record so managed output
/// is still protected from cleanup.
fn canonical_evidence_target(call: &NormalizedCall) -> (String, bool) {
    // A shell command is never copied into durable evidence, whatever entry
    // produced it: the canonical record keeps only a bounded digest reference
    // plus the exact `params_sha256`. The real command is still handed to the
    // approving host through `display_target`/`ApprovalBinding` and executed
    // verbatim.
    if call.operation == "shell" {
        return (redacted_shell_target(call), true);
    }
    match call.entry {
        EntryKind::Builtin | EntryKind::Aggregate | EntryKind::Subagent => {
            (call.display_target.clone(), false)
        }
        EntryKind::Plugin | EntryKind::Mcp | EntryKind::Extra => {
            (redacted_arguments_target(&call.display_target), true)
        }
    }
}

/// Summarize a shell command for the canonical record without copying it.
///
/// The exact command is not lost: the same call's real `params_sha256` is
/// stored unchanged and is the digest handed to the approval binding.
fn redacted_shell_target(call: &NormalizedCall) -> String {
    match call.params_sha256.get(..12) {
        Some(prefix) => format!("<redacted shell command: params sha256 {prefix}>"),
        None => "<redacted shell command>".to_owned(),
    }
}

/// Summarize raw JSON tool arguments without copying any argument value (or a
/// secret canary embedded in one) into durable evidence.
fn redacted_arguments_target(arguments: &str) -> String {
    let fields = serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| value.as_object().map(serde_json::Map::len));
    match fields {
        Some(fields) => format!("<redacted arguments: {fields} field(s)>"),
        None => "<redacted arguments>".to_owned(),
    }
}

/// Classify the aggregate `workspace` tool by its inner operation.
fn classify_aggregate(params: &Value) -> Result<Classified, PolicyDenial> {
    let operation = string_field(params, "operation")
        .ok_or_else(|| PolicyDenial::new("workspace call without an operation"))?;
    let (canonical, side_effect, requires_approval) = match operation.as_str() {
        "read" | "grep" | "find" | "ls" => (operation.clone(), SideEffect::Read, false),
        "write" | "edit" => (operation.clone(), SideEffect::Write, true),
        "shell" | "bash" => ("shell".to_owned(), SideEffect::Write, true),
        // Conservatively treat an unrecognized operation as an external side
        // effect; the workspace itself still rejects it after admission.
        _ => (operation.clone(), SideEffect::External, true),
    };
    let display_target = match operation.as_str() {
        "shell" | "bash" => {
            string_field(params, "value").or_else(|| string_field(params, "command"))
        }
        _ => string_field(params, "path")
            .or_else(|| string_field(params, "pattern"))
            .or_else(|| string_field(params, "value")),
    }
    .unwrap_or_default();
    Ok(Classified {
        entry: EntryKind::Aggregate,
        tool: canonical.clone(),
        operation: canonical,
        display_target,
        side_effect,
        requires_approval,
    })
}

/// Every name that identifies the same tool. Deny wins across all aliases.
fn aliases_for(tool_name: &str, canonical: &str) -> Vec<String> {
    let mut aliases = vec![tool_name.to_owned()];
    if canonical != tool_name {
        aliases.push(canonical.to_owned());
    }
    if let Some(short) = plugin_short_name(tool_name) {
        aliases.push(short);
    }
    aliases
}

/// The declared short name behind `plugin__<plugin>__<tool>`.
fn plugin_short_name(qualified: &str) -> Option<String> {
    let rest = qualified.strip_prefix("plugin__")?;
    let (_, tool) = rest.split_once("__")?;
    (!tool.is_empty()).then(|| tool.to_owned())
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

#[derive(Serialize)]
struct BindingPayload<'a> {
    entry: EntryKind,
    tool: &'a str,
    operation: &'a str,
    cwd: &'a str,
    scope: &'a str,
    params: &'a Value,
}

/// Canonical digest of the exact parameters and dispatch context.
fn digest(
    entry: EntryKind,
    tool: &str,
    operation: &str,
    params: &Value,
    context: &PolicyContext,
) -> String {
    let payload = BindingPayload {
        entry,
        tool,
        operation,
        cwd: &context.cwd,
        scope: &context.scope,
        params,
    };
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    format!("{:x}", hasher.finalize())
}

/// A tool name may not shadow a built-in tool, however it was registered.
pub fn is_reserved_tool_name(name: &str) -> bool {
    BUILTIN_TOOL_NAMES.contains(&name)
}

/// Whether the run's filter exposes a dynamically registered tool under
/// `name`.
///
/// Plugin tools are matched through both the qualified and the short declared
/// name, exactly as their load-time filter did; MCP and host tools are matched
/// by their registered name.
pub fn dynamic_tool_allowed(filter: &ToolFilter, name: &str) -> bool {
    match plugin_short_name(name) {
        Some(short) => filter.allows_aliased(&[name, short.as_str()]),
        None => filter.allows(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentEvent, HostFuture};
    use std::path::Path;
    use std::sync::Mutex;

    #[derive(Default)]
    struct GateHost {
        allow: bool,
        calls: Mutex<Vec<(String, String, ApprovalBinding)>>,
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
            self.calls.lock().unwrap().push((
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
    }

    fn gate(host: Arc<GateHost>, filter: ToolFilter, read_only: bool) -> ToolPolicy {
        ToolPolicy::new(
            filter,
            read_only,
            host,
            PolicyContext::new("/work", "run-1"),
        )
    }

    #[tokio::test]
    async fn aggregate_and_named_writes_normalize_to_the_same_operation() {
        let host = Arc::new(GateHost {
            allow: true,
            ..GateHost::default()
        });
        let policy = gate(host.clone(), ToolFilter::all(), false);

        let aggregate = policy
            .admit(
                "workspace",
                r#"{"operation":"write","path":"a.txt","value":"x"}"#,
            )
            .await
            .unwrap();
        assert_eq!(aggregate.entry, EntryKind::Aggregate);
        assert_eq!(aggregate.operation, "write");
        assert_eq!(aggregate.display_target, "a.txt");
        assert!(aggregate.requires_approval);

        let direct = policy
            .admit("write", r#"{"path":"a.txt","value":"x"}"#)
            .await
            .unwrap();
        assert_eq!(direct.entry, EntryKind::Builtin);
        assert_eq!(direct.operation, "write");
        assert_eq!(direct.display_target, "a.txt");

        let calls = host.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "write");
        assert_eq!(calls[0].1, "a.txt");
    }

    #[tokio::test]
    async fn bash_and_shell_alias_label_agree() {
        let host = Arc::new(GateHost {
            allow: true,
            ..GateHost::default()
        });
        let policy = gate(host.clone(), ToolFilter::all(), false);

        let direct = policy
            .admit("bash", r#"{"command":"ls -la"}"#)
            .await
            .unwrap();
        assert_eq!(direct.tool, "shell");
        assert_eq!(direct.operation, "shell");
        assert_eq!(direct.display_target, "ls -la");

        let aggregate = policy
            .admit("workspace", r#"{"operation":"bash","value":"ls -la"}"#)
            .await
            .unwrap();
        assert_eq!(aggregate.operation, "shell");
        assert_eq!(aggregate.display_target, "ls -la");
    }

    #[tokio::test]
    async fn deny_wins_for_excluded_aliases_and_aggregate_operations() {
        let host = Arc::new(GateHost::default());
        let filter = ToolFilter::from_cli(None, Some("bash"), false).unwrap();
        let policy = gate(host, filter, false);

        assert!(policy.admit("shell", r#"{"command":"x"}"#).await.is_err());
        assert!(
            policy
                .admit("workspace", r#"{"operation":"shell","value":"x"}"#)
                .await
                .is_err()
        );
        // Unrelated read work stays available.
        assert!(
            policy
                .admit("workspace", r#"{"operation":"read","path":"a"}"#)
                .await
                .is_ok()
        );
        assert_eq!(policy.denial_count(), 2);
    }

    #[tokio::test]
    async fn plugin_short_alias_deny_is_enforced() {
        let host = Arc::new(GateHost {
            allow: true,
            ..GateHost::default()
        });
        let filter =
            ToolFilter::from_cli(Some("plugin__probe__read"), Some("read"), false).unwrap();
        let policy = gate(host, filter, false);
        assert!(
            policy.admit("plugin__probe__read", r#"{}"#).await.is_err(),
            "the short declared name deny must win"
        );
    }

    #[tokio::test]
    async fn read_only_refuses_external_entries_without_approval() {
        let host = Arc::new(GateHost {
            allow: true,
            ..GateHost::default()
        });
        let policy = gate(host.clone(), ToolFilter::all(), true);

        assert!(policy.admit("read", r#"{"path":"a"}"#).await.is_ok());
        assert!(policy.admit("fixture_extra", r#"{}"#).await.is_err());
        assert!(policy.admit("plugin__probe__run", r#"{}"#).await.is_err());
        assert!(policy.admit("mcp__srv__tool", r#"{}"#).await.is_err());
        assert!(
            policy
                .admit("subagent", r#"{"operation":"list"}"#)
                .await
                .is_ok()
        );
        for operation in ["status", "snapshot"] {
            assert!(
                policy
                    .admit(
                        "subagent",
                        &serde_json::json!({"operation": operation}).to_string()
                    )
                    .await
                    .is_ok()
            );
        }
        for operation in ["delegate", "fork", "kill"] {
            assert!(
                policy
                    .admit(
                        "subagent",
                        &serde_json::json!({"operation": operation}).to_string()
                    )
                    .await
                    .is_err()
            );
        }
        assert_eq!(policy.approval_count(), 0);
        assert!(host.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn extra_tools_require_approval_and_never_reuse_it() {
        let host = Arc::new(GateHost::default());
        let policy = gate(host.clone(), ToolFilter::all(), false);

        let first = policy.admit("fixture_extra", r#"{"a":1}"#).await;
        assert!(first.is_err());
        let second = policy.admit("fixture_extra", r#"{"a":1}"#).await;
        assert!(second.is_err());
        assert_eq!(policy.approval_count(), 2, "each dispatch asks again");

        let calls = host.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].2.entry, EntryKind::Extra);
        assert_eq!(calls[0].2.params_sha256.len(), 64);
        assert_ne!(calls[0].2.call_id, calls[1].2.call_id);
    }

    #[tokio::test]
    async fn changed_parameters_change_the_binding_digest() {
        let host = Arc::new(GateHost {
            allow: true,
            ..GateHost::default()
        });
        let policy = gate(host.clone(), ToolFilter::all(), false);

        policy
            .admit(
                "workspace",
                r#"{"operation":"write","path":"a.txt","value":"one"}"#,
            )
            .await
            .unwrap();
        policy
            .admit(
                "workspace",
                r#"{"operation":"write","path":"a.txt","value":"two"}"#,
            )
            .await
            .unwrap();

        let calls = host.calls.lock().unwrap();
        assert_ne!(calls[0].2.params_sha256, calls[1].2.params_sha256);
    }

    #[tokio::test]
    async fn unknown_workspace_operation_is_conservatively_external() {
        let host = Arc::new(GateHost {
            allow: true,
            ..GateHost::default()
        });
        let policy = gate(host, ToolFilter::all(), false);
        let call = policy
            .admit("workspace", r#"{"operation":"frobnicate"}"#)
            .await
            .unwrap();
        assert_eq!(call.side_effect, SideEffect::External);
        assert!(call.requires_approval);
    }

    struct WaitingHost;
    impl Host for WaitingHost {
        fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
            Box::pin(async { panic!("a refused dispatch must never execute") })
        }
        fn on_event(&self, _: AgentEvent) {}
        fn approve_tool_async(&self, _: &str, _: &str) -> HostFuture<Result<(), String>> {
            Box::pin(std::future::pending())
        }
    }

    #[test]
    fn the_claim_ledger_keeps_only_digests_and_separates_scopes() {
        let claims = ToolCallClaims::new();
        assert!(claims.claim("run-1", "call-a").is_ok());
        assert_eq!(claims.len(), 1);
        {
            let seen = claims.seen.lock().unwrap();
            let stored = seen.iter().next().unwrap();
            assert_eq!(stored.len(), 32, "only a digest is stored");
            assert_eq!(stored, &claim_key("run-1", "call-a"));
        }
        // The scope is part of the key, so equal ids in adjacent scopes never
        // alias, and the NUL separator keeps the field boundary unambiguous.
        assert_ne!(claim_key("run-1", "call-a"), claim_key("run-11", "call-a"));
        assert_ne!(claim_key("run", "-1call-a"), claim_key("run-1", "call-a"));
        // A different id in the same scope is a different slot, and the same id
        // in another scope is independent.
        assert!(claims.claim("run-1", "call-b").is_ok());
        assert!(claims.claim("run-2", "call-a").is_ok());
        assert_eq!(claims.len(), 3);
        assert!(claims.claim("run-1", "call-a").is_err());
    }

    #[test]
    fn a_malformed_provider_id_claims_nothing() {
        let claims = ToolCallClaims::new();
        let too_long = "x".repeat(MAX_TOOL_CALL_ID_BYTES + 1);
        for bad in ["", " \t ", "bad\nid", too_long.as_str()] {
            assert!(claims.claim("run-1", bad).is_err(), "{bad:?}");
        }
        assert_eq!(claims.len(), 0, "a refused id must claim no slot");
    }

    #[tokio::test]
    async fn approval_timeout_denies_a_pending_host() {
        let policy = ToolPolicy::new(
            ToolFilter::all(),
            false,
            Arc::new(WaitingHost),
            PolicyContext::new("/work", "run-timeout"),
        )
        .with_approval_timeout(std::time::Duration::from_millis(1));
        let error = policy
            .admit("bash", r#"{"command":"must not run"}"#)
            .await
            .unwrap_err();
        assert!(error.reason().contains("timed out"));
        assert_eq!(policy.denial_count(), 1);
    }
}
