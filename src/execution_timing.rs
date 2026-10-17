//! Verifiable model and tool dispatch timing.
//!
//! [`ExecutionTimingHook`] reuses the existing [`AgentHook`] dispatch/outcome
//! boundary to measure one monotonic, non-negative duration per measured
//! effect. It reports only the run's UUID, the dispatcher's effect id, the
//! measured family (`model` or `tool`), a `success`/`error` classification and
//! the whole-millisecond duration. Tool arguments, target paths, model
//! replies, request or response bodies, headers and credentials are never
//! captured.
//!
//! Measurement semantics:
//!
//! - The clock is [`Instant`], which is monotonic, so a duration is never
//!   negative and is independent of wall-clock adjustments.
//! - The hook is registered after the admission hooks, so the span starts
//!   after scheduling admission and excludes the preceding queue wait. It also
//!   excludes prompt and context preparation that happens before the dispatch.
//! - A tool's span covers the tool body's own execution, which may itself
//!   include that tool's internal queueing.
//! - Concurrent effects are measured independently, so their durations must
//!   not be summed and read as wall time.
//! - An effect that never reaches an outcome (a cancellation, or a denial that
//!   short-circuits the dispatch) emits nothing: no completion event is
//!   fabricated.
//! - Only `Completion` and `ToolCall` dispatches are measured. Every other
//!   family is ignored, and the hook never patches, denies or replaces a
//!   dispatch or an outcome.

use crate::Host;
use mio_agent::agent::{
    AgentHook, DispatchAction, DispatchEvent, HookContext, OutcomeAction, OutcomeEvent,
    StepEventKind,
};
use mio_core::{
    effect::{EffectId, EffectKind, Outcome},
    error::ErrorReport,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Instant,
};
use uuid::Uuid;

/// The effect family a timing observation covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionTimingKind {
    /// One model completion dispatch.
    Model,
    /// One tool-call dispatch.
    Tool,
}

/// How the measured dispatch resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionTimingStatus {
    /// The outcome was a completion, or a tool result whose disposition is
    /// `success`.
    Success,
    /// The outcome was an error report, or a tool result whose disposition is
    /// `error`, `denied` or `skipped`.
    Error,
}

/// One measured dispatch-to-outcome span.
///
/// It carries no tool arguments, no target path, no model reply, no request or
/// response body and no credential.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionTiming {
    /// UUID of the run that dispatched the effect; shared with
    /// [`CompletionCallObservation`](crate::CompletionCallObservation) and
    /// [`RunDiagnostic`](crate::RunDiagnostic) for the same run.
    pub run_id: Uuid,
    /// The dispatcher's minted effect id, the correlation key shared with the
    /// bus tap.
    pub effect_id: u64,
    /// The measured family.
    pub kind: ExecutionTimingKind,
    /// How the outcome resolved.
    pub status: ExecutionTimingStatus,
    /// Whole milliseconds between dispatch and outcome, measured on a
    /// monotonic clock and therefore never negative.
    pub duration_ms: u64,
}

/// Classify an observed outcome as `success` or `error`.
///
/// A tool result whose disposition is anything other than `success` (an error,
/// a refusal, or a policy skip) counts as an error.
fn classify(outcome: &Result<Outcome, ErrorReport>) -> ExecutionTimingStatus {
    match outcome {
        Ok(Outcome::ToolResult { result }) if !result.is_success() => ExecutionTimingStatus::Error,
        Ok(_) => ExecutionTimingStatus::Success,
        Err(_) => ExecutionTimingStatus::Error,
    }
}

/// The measured family for an effect, or `None` when the family is unsupported.
fn measured(kind: &EffectKind) -> Option<ExecutionTimingKind> {
    match kind {
        EffectKind::Completion { .. } => Some(ExecutionTimingKind::Model),
        EffectKind::ToolCall { .. } => Some(ExecutionTimingKind::Tool),
        _ => None,
    }
}

/// Per-run hook that measures model and tool dispatch spans.
///
/// It is an observation-only helper: every action it returns is the neutral
/// one, so installing it cannot change a run's dispatch or outcome. Pending
/// spans are keyed by [`EffectId`], never by tool name, so concurrent calls of
/// the same tool are measured independently.
pub(crate) struct ExecutionTimingHook {
    run_id: Uuid,
    host: Arc<dyn Host>,
    in_flight: Mutex<HashMap<EffectId, Instant>>,
}

impl ExecutionTimingHook {
    pub(crate) fn new(run_id: Uuid, host: Arc<dyn Host>) -> Self {
        Self {
            run_id,
            host,
            in_flight: Mutex::new(HashMap::new()),
        }
    }
}

impl AgentHook for ExecutionTimingHook {
    fn observes(&self, kind: StepEventKind) -> bool {
        matches!(
            kind,
            StepEventKind::CompletionDispatch | StepEventKind::ToolDispatch
        )
    }

    async fn on_dispatch(&self, _: &HookContext, event: DispatchEvent<'_>) -> DispatchAction {
        if measured(event.kind).is_some() {
            self.in_flight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(event.id, Instant::now());
        }
        DispatchAction::Proceed
    }

    async fn on_outcome(&self, _: &HookContext, event: OutcomeEvent<'_>) -> OutcomeAction {
        if let Some(kind) = measured(event.kind) {
            let started = self
                .in_flight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&event.id);
            // A dispatch this hook never started (for example one denied by an
            // earlier admission hook) has no span: report nothing rather than
            // fabricating a completion event.
            if let Some(started) = started {
                let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                self.host.on_execution_timing(&ExecutionTiming {
                    run_id: self.run_id,
                    effect_id: event.id.as_u64(),
                    kind,
                    status: classify(event.outcome),
                    duration_ms,
                });
            }
        }
        OutcomeAction::Proceed
    }
}
