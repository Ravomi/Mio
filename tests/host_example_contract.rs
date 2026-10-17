//! Contract tests for `examples/host_loopback.rs`.
//!
//! The example's source is compiled into this test crate with `#[path]` so the
//! tests call the exact same `run_scenario` entry point `main` uses. No test
//! spawns `cargo` (that would deadlock against the enclosing test run); every
//! case drives the real SDK loop against the example's own bounded loopback
//! mock.
//!
//! The assertions go past the returned summary status: they check the real
//! `result.txt` bytes, the real task/journal ids, the real approval binding, the
//! real canonical tool-call phases, the real run-scope observations, the real
//! non-retryable failure and the real cancellation. The model's success text is
//! never used as the acceptance proof.

#[allow(dead_code)]
#[path = "../examples/host_loopback.rs"]
mod host_loopback;

use host_loopback::{
    ExampleHost, FINAL_TEXT, MOCK_SECRET_BODY, MODEL_REQUEST_LIMIT, READ_CALL_ID, RESULT_FILE,
    SEED_FILE, SEED_TEXT, Scenario, WRITE_CALL_ID, derive_write_content, run_scenario, sha256_hex,
};
use mio_coding_agent::{
    DiagnosticCategory, Host, RunObservationKind, RunObservationStatus,
    journal::{Journal, RunState},
    policy::{ApprovalBinding, EntryKind, NormalizedCall, SideEffect},
    task_lifecycle::TaskPhase,
    unknown_effect::{UnknownReason, UnknownSideEffect},
};
use serde_json::json;
use std::sync::Arc;

/// Collapse consecutive duplicate task phases.
///
/// The tracker persists on every real transition, including the set-once
/// engine-run attachment and a cancellation request, which both keep the same
/// phase; only the ordered phase changes are meaningful here.
fn collapsed_task_phases(evidence: &host_loopback::Evidence) -> Vec<TaskPhase> {
    let mut phases: Vec<TaskPhase> = Vec::new();
    for record in &evidence.task_transitions {
        if phases.last() != Some(&record.phase) {
            phases.push(record.phase);
        }
    }
    phases
}

#[tokio::test]
async fn success_scenario_really_reads_then_writes_then_finishes() {
    let evidence = run_scenario(Scenario::Success)
        .await
        .expect("the success scenario must complete offline");

    assert_eq!(evidence.scenario, "success");
    assert_eq!(evidence.model_requests, 3, "exactly three model requests");
    assert_eq!(evidence.completions.len(), 3, "three completed model calls");
    assert_eq!(evidence.task_phase, TaskPhase::Completed);
    assert!(evidence.task_admitted, "the run must have really admitted");
    assert_eq!(
        collapsed_task_phases(&evidence),
        vec![TaskPhase::Queued, TaskPhase::Running, TaskPhase::Completed]
    );
    assert_eq!(
        evidence.shell_calls, 0,
        "the example must never run a shell"
    );
    assert!(evidence.error.is_none());
    assert!(evidence.unknown_recorded.is_empty());
    assert_eq!(evidence.unknown_confirmations_refused, 0);

    // Real, distinct identities: the example's canonical task id is not the
    // engine-run UUID, and the journal bound both the task id and the scope.
    let engine_run_id = evidence
        .engine_run_id
        .clone()
        .expect("the engine must attach its real run UUID");
    assert_ne!(engine_run_id, evidence.task_id);
    assert_eq!(
        evidence.scope_run_id.as_deref(),
        Some(engine_run_id.as_str())
    );
    assert_eq!(
        evidence.journal_engine_run_id.as_deref(),
        Some(engine_run_id.as_str())
    );
    assert_eq!(
        evidence.journal_task_id.as_deref(),
        Some(evidence.task_id.as_str())
    );
    assert!(
        evidence.journal_engine_scopes.contains(&engine_run_id),
        "the real engine scope must be bound to the journal run: {:?}",
        evidence.journal_engine_scopes
    );

    // The read really happened and is the only source of the write.
    assert_eq!(evidence.read_observations.len(), 1);
    let read = &evidence.read_observations[0];
    assert_eq!(read.path, SEED_FILE);
    assert_eq!(read.tool_call_id.as_deref(), Some(READ_CALL_ID));
    assert_eq!(
        read.run_id.map(|id| id.to_string()).as_deref(),
        Some(engine_run_id.as_str())
    );
    assert_eq!(evidence.read_output.as_deref(), Some(SEED_TEXT));
    let expected = derive_write_content(SEED_TEXT);
    assert_eq!(
        evidence.mock_derived_write.as_deref(),
        Some(expected.as_str())
    );
    assert!(evidence.result_exists);
    assert_eq!(evidence.result_content.as_deref(), Some(expected.as_str()));
    assert_eq!(evidence.mock_derived_matches_disk, Some(true));
    let expected_sha = sha256_hex(expected.as_bytes());
    assert_eq!(
        evidence.result_sha256.as_deref(),
        Some(expected_sha.as_str())
    );
    assert_eq!(evidence.result_len, Some(expected.len()));
    assert_eq!(evidence.final_text, FINAL_TEXT);

    // Exactly one real, parameter-bound approval: the write to result.txt.
    assert_eq!(evidence.approvals.len(), 1, "{:?}", evidence.approvals);
    let approval = &evidence.approvals[0];
    assert!(approval.approved);
    assert_eq!(approval.operation, "write");
    assert_eq!(approval.target, RESULT_FILE);
    assert_eq!(approval.binding.tool, "write");
    assert_eq!(
        approval.binding.tool_call_id.as_deref(),
        Some(WRITE_CALL_ID)
    );
    assert_eq!(approval.binding.scope, engine_run_id);
    assert!(!approval.binding.params_sha256.is_empty());
    assert!(!approval.binding.call_id.is_empty());
    assert_ne!(approval.binding.call_id, WRITE_CALL_ID);

    // Canonical lifecycle transitions: normalized -> admitted -> dispatched ->
    // executed for both tools, with no denied/unknown/failed record.
    for tool in ["read", "write"] {
        let phases: Vec<&str> = evidence
            .tool_call_transitions
            .iter()
            .filter(|call| call.tool == tool)
            .map(|call| call.phase.as_str())
            .collect();
        assert_eq!(phases.first().copied(), Some("normalized"), "{phases:?}");
        assert!(phases.contains(&"admitted"), "{phases:?}");
        assert!(phases.contains(&"dispatched"), "{phases:?}");
        assert_eq!(phases.last().copied(), Some("executed"), "{phases:?}");
        assert!(
            !phases
                .iter()
                .any(|phase| *phase == "denied" || *phase == "failed" || *phase == "unknown"),
            "{phases:?}"
        );
    }

    // The authoritative journal file merges one record per call.
    assert_eq!(
        evidence.canonical_calls.len(),
        2,
        "{:?}",
        evidence.canonical_calls
    );
    let read_call = evidence
        .canonical_calls
        .iter()
        .find(|call| call.tool == "read")
        .expect("canonical read");
    assert_eq!(read_call.tool_call_id.as_deref(), Some(READ_CALL_ID));
    assert_eq!(read_call.phase.as_str(), "executed");
    assert!(!read_call.params_sha256.is_empty());
    let write_call = evidence
        .canonical_calls
        .iter()
        .find(|call| call.tool == "write")
        .expect("canonical write");
    assert_eq!(write_call.tool_call_id.as_deref(), Some(WRITE_CALL_ID));
    assert_eq!(write_call.phase.as_str(), "executed");
    assert_eq!(write_call.turn, Some(2));
    assert!(write_call.effect_id.is_some());
    assert_eq!(
        write_call.call_id.as_deref(),
        Some(approval.binding.call_id.as_str())
    );

    let run = evidence
        .journal
        .runs
        .iter()
        .find(|run| run.task_id.as_deref() == Some(evidence.task_id.as_str()))
        .expect("the journal run that owns the real task id");
    assert_eq!(run.state, RunState::Completed);
    assert_eq!(run.canonical_phase.as_deref(), Some("completed"));
    assert_eq!(run.calls.len(), 2);
    assert_eq!(evidence.journal_call_count, 2);
    assert!(
        run.operations.is_empty(),
        "canonical calls must not also write legacy operations: {:?}",
        run.operations
    );
    assert_eq!(evidence.journal_operation_count, 0);

    // The scheduler really enforced the hard call budget.
    assert_eq!(
        evidence.scheduler["budget"]["maxCalls"],
        json!(MODEL_REQUEST_LIMIT)
    );
    assert_eq!(evidence.scheduler["budget"]["calls"], json!(3));
}

#[tokio::test]
async fn model_error_scenario_fails_after_read_without_any_write() {
    let evidence = run_scenario(Scenario::ModelError)
        .await
        .expect("the model-error scenario must complete offline");

    assert_eq!(evidence.scenario, "model-error");
    assert_eq!(
        evidence.model_requests, 2,
        "the HTTP 400 must not be retried"
    );
    assert_eq!(
        evidence.completions.len(),
        1,
        "only the first model call completed"
    );
    assert_eq!(evidence.task_phase, TaskPhase::Failed);
    assert!(evidence.task_admitted);
    assert_eq!(
        collapsed_task_phases(&evidence),
        vec![TaskPhase::Queued, TaskPhase::Running, TaskPhase::Failed]
    );

    // The read succeeded before the failure...
    assert_eq!(evidence.read_observations.len(), 1);
    assert_eq!(evidence.read_observations[0].path, SEED_FILE);
    assert_eq!(evidence.read_output.as_deref(), Some(SEED_TEXT));

    // ...but no write ever happened, and no write was ever approved.
    assert!(evidence.approvals.is_empty(), "{:?}", evidence.approvals);
    assert!(!evidence.result_exists);
    assert!(evidence.result_content.is_none());
    assert!(evidence.mock_derived_write.is_none());
    assert_eq!(evidence.canonical_calls.len(), 1);
    assert_eq!(evidence.canonical_calls[0].tool, "read");
    assert_eq!(evidence.canonical_calls[0].phase.as_str(), "executed");

    // The failure is a real, non-retryable request error whose provider body
    // never leaked into the diagnostics, the error or the durable journal.
    assert_eq!(evidence.diagnostics.len(), 1);
    let diagnostic = &evidence.diagnostics[0];
    assert_eq!(diagnostic.category, DiagnosticCategory::Request);
    assert!(!diagnostic.retryable);
    assert_eq!(diagnostic.http_status, Some(400));
    let error = evidence
        .error
        .clone()
        .expect("the failing run must report its error");
    assert!(!error.contains(MOCK_SECRET_BODY), "{error}");
    let serialized = serde_json::to_string(&evidence).expect("evidence serializes");
    assert!(
        !serialized.contains(MOCK_SECRET_BODY),
        "the provider body must not leak into evidence"
    );

    let run = evidence
        .journal
        .runs
        .iter()
        .find(|run| run.task_id.as_deref() == Some(evidence.task_id.as_str()))
        .expect("journal run");
    assert_eq!(run.state, RunState::Failed);
    assert_eq!(run.canonical_phase.as_deref(), Some("failed"));
    assert_eq!(run.calls.len(), 1);
    assert_eq!(run.calls[0].phase.as_str(), "executed");
    assert!(run.operations.is_empty());

    // The budget counted the two real calls.
    assert_eq!(
        evidence.scheduler["budget"]["maxCalls"],
        json!(MODEL_REQUEST_LIMIT)
    );
    assert_eq!(evidence.scheduler["budget"]["calls"], json!(2));
}

#[tokio::test]
async fn cancel_scenario_drops_the_in_flight_request_and_settles_cancelled() {
    let evidence = run_scenario(Scenario::Cancel)
        .await
        .expect("the cancel scenario must complete offline");

    assert_eq!(evidence.scenario, "cancel");
    assert_eq!(
        evidence.model_requests, 1,
        "the dropped run must not send a later request"
    );
    assert!(
        evidence.completions.is_empty(),
        "no model call completed before the drop"
    );
    assert_eq!(
        evidence.task_phase_at_cancel_drop,
        Some(TaskPhase::Running),
        "the request was in flight only because the task was really running"
    );
    assert_eq!(evidence.task_phase, TaskPhase::Cancelled);
    assert!(evidence.task_cancel_requested);
    assert!(evidence.task_admitted);
    assert!(evidence.cancel_outcome.is_some());
    assert!(evidence.finish_outcome.is_some());

    assert_eq!(
        collapsed_task_phases(&evidence),
        vec![TaskPhase::Queued, TaskPhase::Running, TaskPhase::Cancelled]
    );

    // No tool ran, no write was approved, and the run's own terminal
    // observation is a real cancellation on the same engine scope.
    assert!(evidence.canonical_calls.is_empty());
    assert!(evidence.tool_call_transitions.is_empty());
    assert!(evidence.approvals.is_empty());
    assert!(evidence.read_observations.is_empty());
    assert!(!evidence.result_exists);
    assert!(evidence.error.is_none());

    let engine_run_id = evidence.engine_run_id.clone().expect("engine run id");
    assert_ne!(engine_run_id, evidence.task_id);
    let finished = evidence
        .run_observations
        .iter()
        .find(|observation| observation.kind == RunObservationKind::RunFinished)
        .expect("the dropped run must emit its real terminal observation");
    assert_eq!(finished.status, Some(RunObservationStatus::Cancelled));
    assert_eq!(finished.run_id.to_string(), engine_run_id);

    // The journal keeps the real tool facts untouched: nothing was dispatched.
    let run = evidence
        .journal
        .runs
        .iter()
        .find(|run| run.task_id.as_deref() == Some(evidence.task_id.as_str()))
        .expect("journal run");
    assert_eq!(run.state, RunState::Cancelled);
    assert_eq!(run.canonical_phase.as_deref(), Some("cancelled"));
    assert!(run.calls.is_empty());
    assert!(run.operations.is_empty());
    assert_eq!(run.engine_run_id.as_deref(), Some(engine_run_id.as_str()));
    assert_eq!(
        evidence.scheduler["budget"]["calls"],
        json!(1),
        "exactly one model call was admitted"
    );
}

#[tokio::test]
async fn host_only_approves_the_bound_result_write_and_refuses_unknown_confirmation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cwd = dir.path().canonicalize().expect("canonical temp dir");
    let host = ExampleHost::new(Arc::new(Journal::disabled()), cwd.clone());

    let binding = |tool: &str, scope: &str, params: &str, call: Option<&str>| ApprovalBinding {
        entry: EntryKind::Builtin,
        tool: tool.to_owned(),
        operation: "write".to_owned(),
        cwd: cwd.display().to_string(),
        scope: scope.to_owned(),
        params_sha256: params.to_owned(),
        call_id: "approval-attempt-1".to_owned(),
        tool_call_id: call.map(str::to_owned),
    };
    let digest = sha256_hex(br#"{"path":"result.txt"}"#);

    // A parameter-bound write to result.txt is the one approved call.
    host.approve_tool_binding(
        "write",
        RESULT_FILE,
        &binding("write", "run-1", &digest, Some(WRITE_CALL_ID)),
    )
    .await
    .expect("the bound result write must be approved");

    // A different path, a different workspace, an empty digest and a missing
    // model call id are all refused.
    assert!(
        host.approve_tool_binding(
            "write",
            "other.txt",
            &binding("write", "run-1", &digest, Some("call-write-2")),
        )
        .await
        .is_err()
    );
    let mut wrong_cwd = binding("write", "run-1", &digest, Some("call-write-3"));
    wrong_cwd.cwd = "/tmp/not-this-workspace".to_owned();
    assert!(
        host.approve_tool_binding("write", RESULT_FILE, &wrong_cwd)
            .await
            .is_err()
    );
    assert!(
        host.approve_tool_binding(
            "write",
            RESULT_FILE,
            &binding("write", "run-1", "", Some("call-write-4")),
        )
        .await
        .is_err()
    );
    assert_eq!(host.approvals().len(), 4);
    assert_eq!(
        host.approvals()
            .iter()
            .filter(|record| record.approved)
            .count(),
        1
    );

    // An unknown result is never confirmed by the ordinary approval path...
    let call = NormalizedCall {
        entry: EntryKind::Builtin,
        tool: "write".to_owned(),
        operation: "write".to_owned(),
        display_target: RESULT_FILE.to_owned(),
        side_effect: SideEffect::Write,
        requires_approval: true,
        aliases: vec!["write".to_owned()],
        params_sha256: digest.clone(),
    };
    let unknown = UnknownSideEffect::new("write", RESULT_FILE, UnknownReason::McpTransport, None);
    let confirmation = host
        .confirm_unknown_side_effect(&call, std::slice::from_ref(&unknown))
        .await;
    assert!(
        confirmation.is_err(),
        "an unknown confirmation must fail closed, not reuse approval"
    );
    assert_eq!(host.unknown_confirmations_refused(), 1);

    // ...it is recorded as an unknown fact instead.
    host.on_unknown_side_effect(&unknown)
        .expect("recording the unknown fact must succeed");
    assert_eq!(host.unknowns().len(), 1);
    assert_eq!(host.unknowns()[0].reason, UnknownReason::McpTransport);
    assert_eq!(host.unknowns()[0].operation, "write");
    assert_eq!(host.shell_calls(), 0);
}
