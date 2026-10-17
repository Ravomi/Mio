//! Real run correlation IDs across the whole chain (DES-15).
//!
//! [`RunObservation`] is the bounded, content-free observation channel that
//! carries the real identities the engine already knows: the run's UUID (the
//! same value used for approval scope, completion calls, diagnostics and
//! timing), the engine turn, the model's own tool-call id and the dispatcher's
//! effect id. A subagent's run observation keeps its own real engine run UUID
//! and adds the delegation identity separately ([`RunObservation::subagent_id`]
//! and [`RunObservation::parent_run_id`]), so a child's approval scope and its
//! run identity cannot drift apart.
//!
//! What never crosses this channel: tool arguments, target paths, model
//! replies, provider request or response bodies, headers, credentials or any
//! conversation text. Every field is either a UUID, a bounded identifier or a
//! turn counter.
//!
//! The observations are derived from the existing [`AgentHook`] dispatch and
//! outcome boundary plus the engine's own run lifecycle. Preparation and real
//! dispatch are deliberately separate observations, because a call can be
//! prepared (an effect id minted, arguments admitted or an approval requested)
//! and still be denied before it ever reaches a handler:
//!
//! - `RunStarted` is emitted once before the first model call.
//! - `ModelPrepared` / `ToolPrepared` are emitted at the dispatch boundary
//!   *before* every admission hook. They reuse the engine's real
//!   [`EffectId`](mio_core::effect::EffectId) and the model's own `CallId`, so
//!   an approval request raised later can be correlated to the exact prepared
//!   call. A prepared observation is never a dispatch.
//! - `ModelDispatched` / `ToolDispatched` are emitted *after* the admission
//!   hooks, so they only describe effects that were really handed to the bus.
//!   A denied tool call therefore produces no `ToolDispatched`.
//! - `ToolOutcome` reuses the same real effect id and model call id for both an
//!   executed result and a denial (the engine resolves a denied call as a
//!   skipped result), so a refusal stays traceable without being reported as a
//!   committed dispatch.
//! - `RunFinished` (with `success` / `error`) is emitted by the run's settle
//!   hook, never before the engine reached its terminal outcome. A dropped run
//!   future emits `cancelled` instead, so cancellation is attributed to the
//!   actual run UUID rather than guessed by a caller.
//!
//! A host that never heard of this channel keeps compiling and behaving
//! unchanged: [`Host::on_run_observation`](crate::Host::on_run_observation)
//! defaults to a no-op.

use crate::Host;
use crate::shell::output_store::OutputReference;
use crate::tool_lifecycle::{ToolCallPhase, ToolCallTracker};
use crate::unknown_effect::UnknownSideEffect;
use mio_agent::agent::{
    AgentHook, DispatchAction, DispatchEvent, HookContext, OutcomeAction, OutcomeEvent, RunSettled,
    RunStart, RunStartAction, SettledOutcome, StepEventKind,
};
use mio_core::effect::{EffectKind, Outcome};
use mio_core::error::{ErrorKind, ErrorReport};
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use uuid::Uuid;

/// The lifecycle boundary a [`RunObservation`] reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunObservationKind {
    /// The run began, before its first model call.
    RunStarted,
    /// A model completion request was prepared (its effect id was minted) but
    /// has not passed admission yet. This is not a dispatch.
    ModelPrepared,
    /// A model completion request is about to be really dispatched.
    ModelDispatched,
    /// A tool call was prepared (its effect id was minted) but has not passed
    /// admission or approval yet; `tool_call_id` is the model's id. This is
    /// not a dispatch.
    ToolPrepared,
    /// A tool call is about to be really dispatched; `tool_call_id` is the model's id.
    ToolDispatched,
    /// A tool call resolved; `tool_call_id` is the same model id. A denied call
    /// reaches this boundary with an error status and no `ToolDispatched`.
    ToolOutcome,
    /// The run reached a terminal state. `status` is always set.
    RunFinished,
}

/// How a run or a tool call resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunObservationStatus {
    /// The run completed, or the tool reported success.
    Success,
    /// The run failed, or the tool reported an error, refusal or policy skip.
    Error,
    /// The run future was dropped before it settled.
    Cancelled,
}

/// One bounded, content-free correlation observation.
///
/// Fields the engine cannot truthfully supply stay `None`; they are never
/// fabricated. `session_id` is the embedding host's own session identity (the
/// CLI's persisted session), while `subagent_id` is the delegated run's
/// identity and `parent_run_id` links that child back to the run that started
/// it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunObservation {
    /// UUID of the run the observation belongs to. For a delegated child this
    /// is the child's own engine run UUID, never the delegation id.
    pub run_id: Uuid,
    /// Which lifecycle boundary this observation reports.
    pub kind: RunObservationKind,
    /// Host session identity, when the host has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Delegated run identity; `None` for the main run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_id: Option<String>,
    /// The run that started this delegated run; `None` for the main run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<Uuid>,
    /// One-based engine model-call index the effect belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    /// The model's own tool-call id, for tool dispatches and outcomes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// The dispatcher's minted effect id for the dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_id: Option<u64>,
    /// Terminal status, for `RunFinished` and `ToolOutcome`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<RunObservationStatus>,
}

/// Per-run lifecycle emitter shared by the hook and the run's drop guard.
///
/// Every method is idempotent, so a run that settles normally never emits a
/// second terminal observation when its future is later dropped.
pub(crate) struct RunLifecycle {
    run_id: Uuid,
    host: Arc<dyn Host>,
    session_id: Option<String>,
    started: AtomicBool,
    finished: AtomicBool,
}

impl RunLifecycle {
    pub(crate) fn new(run_id: Uuid, host: Arc<dyn Host>) -> Arc<Self> {
        Self::from_session(run_id, host.session_id(), host)
    }

    fn from_session(run_id: Uuid, session_id: Option<String>, host: Arc<dyn Host>) -> Arc<Self> {
        Arc::new(Self {
            run_id,
            host,
            session_id,
            started: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        })
    }

    /// Whether the run has begun (its `RunStarted` observation was emitted).
    pub(crate) fn started(&self) -> bool {
        self.started.load(Ordering::SeqCst)
    }

    /// Emit `RunStarted` once, before the run's first model call.
    pub(crate) fn run_started(&self) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        self.emit(RunObservationKind::RunStarted, None, None, None, None);
    }

    /// Emit the terminal observation once. Later calls are ignored.
    pub(crate) fn run_finished(&self, status: RunObservationStatus) {
        self.started.store(true, Ordering::SeqCst);
        if self.finished.swap(true, Ordering::SeqCst) {
            return;
        }
        self.emit(
            RunObservationKind::RunFinished,
            None,
            None,
            None,
            Some(status),
        );
    }

    fn emit(
        &self,
        kind: RunObservationKind,
        turn_id: Option<u64>,
        tool_call_id: Option<String>,
        effect_id: Option<u64>,
        status: Option<RunObservationStatus>,
    ) {
        self.host.on_run_observation(&RunObservation {
            run_id: self.run_id,
            kind,
            session_id: self.session_id.clone(),
            subagent_id: None,
            parent_run_id: None,
            turn_id,
            tool_call_id,
            effect_id,
            status,
        });
    }

    /// The pre-admission correlation for an effect the engine just prepared.
    ///
    /// The effect id is the real one minted for this dispatch attempt, so the
    /// later `ModelDispatched` / `ToolDispatched` (when admission allows it)
    /// and `ToolOutcome` reuse the same id. Nothing here claims the call ran.
    fn prepared(&self, event: DispatchEvent<'_>) {
        let turn_id = Some(event.turn as u64);
        let effect_id = Some(event.id.as_u64());
        match event.kind {
            EffectKind::Completion { .. } => self.emit(
                RunObservationKind::ModelPrepared,
                turn_id,
                None,
                effect_id,
                None,
            ),
            EffectKind::ToolCall { .. } => self.emit(
                RunObservationKind::ToolPrepared,
                turn_id,
                event.call_id.map(|call| call.wire().into_owned()),
                effect_id,
                None,
            ),
            // Other families have no correlation ids in this version.
            _ => {}
        }
    }

    /// The real dispatch observation for an effect that passed admission.
    fn dispatched(&self, event: DispatchEvent<'_>) {
        let turn_id = Some(event.turn as u64);
        let effect_id = Some(event.id.as_u64());
        match event.kind {
            EffectKind::Completion { .. } => self.emit(
                RunObservationKind::ModelDispatched,
                turn_id,
                None,
                effect_id,
                None,
            ),
            EffectKind::ToolCall { .. } => self.emit(
                RunObservationKind::ToolDispatched,
                turn_id,
                event.call_id.map(|call| call.wire().into_owned()),
                effect_id,
                None,
            ),
            // Other families have no correlation ids in this version.
            _ => {}
        }
    }

    fn outcome(&self, event: OutcomeEvent<'_>) {
        if let EffectKind::ToolCall { .. } = event.kind {
            self.emit(
                RunObservationKind::ToolOutcome,
                Some(event.turn as u64),
                event.call_id.map(|call| call.wire().into_owned()),
                Some(event.id.as_u64()),
                Some(classify_tool_outcome(event.outcome)),
            );
        }
    }
}

/// A tool result whose disposition is not `success` (an error, a refusal or a
/// policy skip) counts as an error. This mirrors the timing observer and is not
/// a test verdict.
fn classify_tool_outcome(outcome: &Result<Outcome, ErrorReport>) -> RunObservationStatus {
    match outcome {
        Ok(Outcome::ToolResult { result }) if !result.is_success() => RunObservationStatus::Error,
        Ok(_) => RunObservationStatus::Success,
        Err(_) => RunObservationStatus::Error,
    }
}

/// Emits the run's lifecycle and the pre-admission "prepared" correlation.
///
/// It is registered before every admission hook, so the real turn and model
/// tool-call id are on the channel while an approval is still pending and an
/// approval request can be traced back to the call that raised it. What it
/// reports is explicitly a *prepared* effect, not a dispatched one: scheduling
/// or policy may still deny the call, and a denial never emits a
/// `ToolDispatched`.
///
/// It is observation-only: every action it returns is the neutral one.
pub(crate) struct RunObservationHook {
    lifecycle: Arc<RunLifecycle>,
}

impl RunObservationHook {
    pub(crate) fn new(lifecycle: Arc<RunLifecycle>) -> Self {
        Self { lifecycle }
    }
}

impl AgentHook for RunObservationHook {
    fn observes(&self, kind: StepEventKind) -> bool {
        matches!(
            kind,
            StepEventKind::RunStart
                | StepEventKind::RunSettled
                | StepEventKind::CompletionDispatch
                | StepEventKind::ToolDispatch
        )
    }

    async fn on_run_start(&self, _: &HookContext, _: RunStart<'_>) -> RunStartAction {
        self.lifecycle.run_started();
        RunStartAction::Continue
    }

    async fn on_run_settled(&self, _: &HookContext, event: RunSettled<'_>) {
        let status = match event.outcome {
            // The engine only settles with a response after its terminal step
            // committed; it is the run's real outcome, not a prediction.
            SettledOutcome::Response(_) => RunObservationStatus::Success,
            SettledOutcome::Error(_) => RunObservationStatus::Error,
        };
        self.lifecycle.run_finished(status);
    }

    async fn on_dispatch(&self, _: &HookContext, event: DispatchEvent<'_>) -> DispatchAction {
        self.lifecycle.prepared(event);
        DispatchAction::Proceed
    }
}

/// Emits the real dispatch and outcome boundary for calls that passed admission
/// and drives the run's canonical tool-call state machine across that boundary.
///
/// It is registered *after* every admission hook. `HookStack` stops at the
/// first denial, so a denied completion or tool call never reaches this hook:
/// `ModelDispatched` / `ToolDispatched` only describe effects really handed to
/// the bus. A denied tool still produces its correlated `ToolOutcome` because
/// the engine resolves a denial as a skipped result and runs the outcome
/// boundary for it (that phase does not stop on a dispatch denial) -- so a
/// refusal stays traceable without being reported as a committed dispatch.
///
/// For a tool call this hook is the final gate before the bus: it persists
/// `Dispatched` first and refuses the dispatch if that write fails or the
/// `(scope, provider id)` was already dispatched. At the outcome it records the
/// real terminal phase (`Executed`/`Failed`, or `Unknown` for a published typed
/// unknown), while a cancelled dispatch stays `Dispatched` for the run end to
/// resolve to `Unknown`.
pub(crate) struct RunDispatchObservationHook {
    lifecycle: Arc<RunLifecycle>,
    tracker: Arc<ToolCallTracker>,
}

impl RunDispatchObservationHook {
    pub(crate) fn new(lifecycle: Arc<RunLifecycle>, tracker: Arc<ToolCallTracker>) -> Self {
        Self { lifecycle, tracker }
    }
}

impl AgentHook for RunDispatchObservationHook {
    fn observes(&self, kind: StepEventKind) -> bool {
        matches!(
            kind,
            StepEventKind::CompletionDispatch | StepEventKind::ToolDispatch
        )
    }

    async fn on_dispatch(&self, _: &HookContext, event: DispatchEvent<'_>) -> DispatchAction {
        if let EffectKind::ToolCall { name, .. } = event.kind {
            let tool_call_id = event.call_id.map(|call| call.wire().into_owned());
            // Dispatched must be persisted before the bus is handed the effect.
            // A duplicate `(scope, provider id)` or a persistence failure is
            // refused here, so a call is never really dispatched twice or
            // dispatched without durable evidence.
            if let Err(error) = self
                .tracker
                .mark_dispatched(event.id.as_u64(), tool_call_id.as_deref())
            {
                return DispatchAction::skip(format!(
                    "refusing to dispatch '{name}': the tool call's dispatched state was not durably recorded ({error})"
                ));
            }
        }
        self.lifecycle.dispatched(event);
        DispatchAction::Proceed
    }

    async fn on_outcome(&self, _: &HookContext, event: OutcomeEvent<'_>) -> OutcomeAction {
        if let EffectKind::ToolCall { .. } = event.kind {
            let tool_call_id = event.call_id.map(|call| call.wire().into_owned());
            // A large result's committed references are minted inside the tool
            // body (after its own tool-finished event for MCP), so read them
            // from the real dispatch context the tool wrapper published instead
            // of parsing model-visible text. An unwrapped call has no such slot
            // and contributes no references.
            let (references, references_truncated) =
                committed_output_references(event.tool_context());
            // A typed unknown published by the tool body is the honest phase:
            // the side effect may have happened and the result is not knowable.
            let typed_unknown = event
                .tool_context()
                .and_then(|context| context.result::<UnknownSideEffect>().ok().flatten());
            if let Some(unknown) = typed_unknown {
                let detail = format!("result unknown ({})", unknown.reason.as_str());
                let _ = self.tracker.observe_outcome_with_references(
                    event.id.as_u64(),
                    tool_call_id.as_deref(),
                    ToolCallPhase::Unknown,
                    Some(detail.as_str()),
                    &references,
                    references_truncated,
                );
            } else {
                // A cancelled dispatch has no confirmed result: it stays
                // `Dispatched` and the run end resolves it to `Unknown`.
                if let Some(phase) = classify_tool_phase(event.outcome) {
                    let _ = self.tracker.observe_outcome_with_references(
                        event.id.as_u64(),
                        tool_call_id.as_deref(),
                        phase,
                        None,
                        &references,
                        references_truncated,
                    );
                }
            }
        }
        self.lifecycle.outcome(event);
        OutcomeAction::Proceed
    }
}

/// The real committed output references the tool wrapper published on the
/// dispatch context, plus whether the typed slot itself was over the per-call
/// bound. An absent slot (an unwrapped, engine-less or older tool) contributes
/// no references; model-visible text is never parsed to recover them.
fn committed_output_references(
    context: Option<&mio_core::tool::ToolContext>,
) -> (Vec<OutputReference>, bool) {
    context
        .and_then(|context| {
            context
                .result::<crate::tool_identity::CommittedOutputReferences>()
                .ok()
                .flatten()
        })
        .map(|references| {
            let truncated =
                references.0.len() > crate::tool_identity::MAX_OUTPUT_REFERENCES_PER_CALL;
            (references.0, truncated)
        })
        .unwrap_or_default()
}

/// The terminal phase for a real tool outcome, or `None` when the dispatch was
/// cancelled and its side effect therefore cannot be classified.
fn classify_tool_phase(outcome: &Result<Outcome, ErrorReport>) -> Option<ToolCallPhase> {
    match outcome {
        Ok(Outcome::ToolResult { result }) if result.is_success() => Some(ToolCallPhase::Executed),
        Ok(_) => Some(ToolCallPhase::Failed),
        Err(report) if matches!(report.kind, ErrorKind::Cancelled) => None,
        Err(_) => Some(ToolCallPhase::Failed),
    }
}

/// Attributes a dropped run future to its real run UUID as `cancelled`.
///
/// The guard lives in the run's own scope, so it drops both when the run
/// settles normally (a no-op, because the run already finished) and when a
/// caller drops the future mid-flight.
pub(crate) struct RunLifecycleGuard(Arc<RunLifecycle>);

impl RunLifecycleGuard {
    pub(crate) fn new(lifecycle: Arc<RunLifecycle>) -> Self {
        Self(lifecycle)
    }
}

impl Drop for RunLifecycleGuard {
    fn drop(&mut self) {
        if self.0.started() {
            self.0.run_finished(RunObservationStatus::Cancelled);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentEvent, HostFuture};
    use std::{path::Path, sync::Mutex};

    #[derive(Default)]
    struct RecordingHost {
        observations: Mutex<Vec<RunObservation>>,
        session: Option<String>,
    }

    impl Host for RecordingHost {
        fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
            Box::pin(async { Ok(String::new()) })
        }
        fn on_event(&self, _: AgentEvent) {}
        fn session_id(&self) -> Option<String> {
            self.session.clone()
        }
        fn on_run_observation(&self, observation: &RunObservation) {
            self.observations.lock().unwrap().push(observation.clone());
        }
    }

    #[test]
    fn lifecycle_emits_once_and_status_classification_is_strict() {
        let host = Arc::new(RecordingHost {
            session: Some("session-1".into()),
            ..Default::default()
        });
        let lifecycle = RunLifecycle::new(Uuid::new_v4(), host.clone());
        lifecycle.run_started();
        lifecycle.run_started();
        lifecycle.run_finished(RunObservationStatus::Success);
        lifecycle.run_finished(RunObservationStatus::Cancelled);
        drop(RunLifecycleGuard::new(lifecycle.clone()));

        let observations = host.observations.lock().unwrap().clone();
        assert_eq!(observations.len(), 2, "{observations:?}");
        assert_eq!(observations[0].kind, RunObservationKind::RunStarted);
        assert_eq!(observations[1].kind, RunObservationKind::RunFinished);
        assert_eq!(observations[1].status, Some(RunObservationStatus::Success));
        assert_eq!(observations[0].session_id.as_deref(), Some("session-1"));
        assert!(observations[0].subagent_id.is_none());
        assert!(observations[0].parent_run_id.is_none());
    }

    #[test]
    fn a_dropped_unfinished_run_is_attributed_as_cancelled() {
        let host = Arc::new(RecordingHost::default());
        let lifecycle = RunLifecycle::new(Uuid::new_v4(), host.clone());
        lifecycle.run_started();
        drop(RunLifecycleGuard::new(lifecycle));
        let observations = host.observations.lock().unwrap().clone();
        assert_eq!(observations.len(), 2);
        assert_eq!(observations[1].kind, RunObservationKind::RunFinished);
        assert_eq!(
            observations[1].status,
            Some(RunObservationStatus::Cancelled)
        );
    }

    #[test]
    fn an_effect_that_never_started_leaves_no_phantom_lifecycle() {
        let host = Arc::new(RecordingHost::default());
        let lifecycle = RunLifecycle::new(Uuid::new_v4(), host.clone());
        drop(RunLifecycleGuard::new(lifecycle));
        assert!(host.observations.lock().unwrap().is_empty());
    }

    fn reference() -> OutputReference {
        OutputReference {
            path: ".mio/outputs/abcd.stdout".into(),
            total_bytes: 11,
            stream: "stdout".into(),
            call_id: Some("call-1".into()),
            sha256: "a".repeat(64),
        }
    }

    /// The canonical terminal outcome must read committed references from the
    /// real typed dispatch context, never from model-visible text.
    #[test]
    fn committed_output_references_come_from_the_published_typed_context() {
        assert_eq!(committed_output_references(None), (Vec::new(), false));
        let empty = mio_core::tool::ToolContext::new();
        assert_eq!(
            committed_output_references(Some(&empty)),
            (Vec::new(), false)
        );

        let mut context = mio_core::tool::ToolContext::new();
        context
            .insert_result(crate::tool_identity::CommittedOutputReferences(vec![
                reference(),
            ]))
            .unwrap();
        let (references, truncated) = committed_output_references(Some(&context));
        assert_eq!(references, vec![reference()]);
        assert!(!truncated);
    }
}
