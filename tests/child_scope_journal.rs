//! Canonical journal scope ownership: a delegated child run outliving its
//! parent turn must keep its real evidence in the run that owns it.
//!
//! These exercise the public canonical-journal API (`record_run_observation` +
//! `record_canonical_phase` + `project_task_canonical`) exactly as a real
//! `Host` adapter does: the CLI only forwards `RunObservation`s. No owner is
//! ever inferred from a tool name, target or completion order.

use mio_coding_agent::{
    RunObservation, RunObservationKind,
    journal::{Journal, RunState, journal_path},
    tool_lifecycle::{ToolCallPhase, ToolCallRecord},
};
use uuid::Uuid;

fn observation(kind: RunObservationKind, run_id: Uuid) -> RunObservation {
    RunObservation {
        run_id,
        kind,
        session_id: Some("scope-session".into()),
        subagent_id: None,
        parent_run_id: None,
        turn_id: None,
        tool_call_id: None,
        effect_id: None,
        status: None,
    }
}

fn call(scope: Uuid, index: u64, phase: ToolCallPhase, tool_call_id: &str) -> ToolCallRecord {
    ToolCallRecord {
        call_index: index,
        scope: scope.to_string(),
        phase,
        tool: "write".into(),
        operation: "write".into(),
        params_sha256: "0".repeat(64),
        target: "out.txt".into(),
        target_redacted: false,
        tool_call_id: Some(tool_call_id.into()),
        call_id: None,
        turn: Some(1),
        effect_id: Some(index),
        detail: None,
        output_references: Vec::new(),
        references_truncated: false,
        started: 1,
        updated: 1,
    }
}

fn stored_runs(root: &std::path::Path, session: &str) -> Vec<serde_json::Value> {
    let path = journal_path(root, session);
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["runs"].as_array().unwrap().clone()
}

fn call_phase(run: &serde_json::Value, tool_call_id: &str) -> Option<String> {
    run["calls"]
        .as_array()
        .and_then(|calls| {
            calls
                .iter()
                .find(|call| call["tool_call_id"] == tool_call_id)
        })
        .and_then(|call| call["phase"].as_str())
        .map(str::to_owned)
}

#[test]
fn parent_final_while_a_real_child_scope_is_live_never_fabricates_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path(), "live-child");
    journal
        .begin_run_with_task(Some("parent turn"), Some("task-parent"))
        .unwrap();
    let parent = Uuid::new_v4();
    journal
        .record_run_observation(&observation(RunObservationKind::RunStarted, parent))
        .unwrap();
    journal
        .project_task_canonical("task-parent", Some(&parent.to_string()), "running")
        .unwrap();
    let child = Uuid::new_v4();
    let mut child_started = observation(RunObservationKind::RunStarted, child);
    child_started.parent_run_id = Some(parent);
    child_started.subagent_id = Some("child-run".into());
    journal.record_run_observation(&child_started).unwrap();
    journal
        .record_canonical_phase(&call(child, 1, ToolCallPhase::Dispatched, "child-1"))
        .unwrap();

    // The parent turn ends while the real child scope is still live.
    journal.finish_run(RunState::Completed).unwrap();

    let report = journal.recovery_report().unwrap();
    let child_call = report
        .calls
        .iter()
        .find(|call| call.tool_call_id.as_deref() == Some("child-1"))
        .expect("the child call must be recorded");
    assert_eq!(
        child_call.phase,
        ToolCallPhase::Dispatched,
        "a live child scope must stay Dispatched, never Unknown: {report:?}"
    );

    // A second parent turn opens before the child releases.
    journal
        .begin_run_with_task(Some("second parent turn"), Some("task-parent-2"))
        .unwrap();
    let parent_two = Uuid::new_v4();
    journal
        .record_run_observation(&observation(RunObservationKind::RunStarted, parent_two))
        .unwrap();
    journal
        .project_task_canonical("task-parent-2", Some(&parent_two.to_string()), "running")
        .unwrap();
    journal
        .record_canonical_phase(&call(
            parent_two,
            1,
            ToolCallPhase::Dispatched,
            "parent-2-1",
        ))
        .unwrap();

    // The child's real result arrives only now and must land in its own run.
    journal
        .record_canonical_phase(&call(child, 1, ToolCallPhase::Executed, "child-1"))
        .unwrap();
    journal
        .record_run_observation(&observation(RunObservationKind::RunFinished, child))
        .unwrap();

    let runs = stored_runs(dir.path(), "live-child");
    let owner = runs
        .iter()
        .find(|run| call_phase(run, "child-1").is_some())
        .expect("the child call must live in its owning run");
    assert_eq!(
        call_phase(owner, "child-1").as_deref(),
        Some("executed"),
        "the late child result must complete the original call: {owner}"
    );
    assert!(
        owner["engine_scopes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|scope| scope == &child.to_string()),
        "the owning run must bind the real child scope: {owner}"
    );
    let fresh = runs
        .iter()
        .find(|run| call_phase(run, "parent-2-1").is_some())
        .expect("the second turn keeps its own call");
    assert!(
        call_phase(fresh, "child-1").is_none(),
        "the new parent turn must not receive a duplicate of the old child call: {fresh}"
    );
    assert_eq!(
        fresh["engine_scopes"].as_array().unwrap().len(),
        1,
        "the new run only owns its own top scope: {fresh}"
    );
}

#[test]
fn child_scope_starting_after_parent_final_still_binds_to_its_owner() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path(), "late-child");
    journal
        .begin_run_with_task(Some("parent turn"), Some("task-parent"))
        .unwrap();
    let parent = Uuid::new_v4();
    journal
        .record_run_observation(&observation(RunObservationKind::RunStarted, parent))
        .unwrap();
    journal
        .project_task_canonical("task-parent", Some(&parent.to_string()), "running")
        .unwrap();
    // The parent finalizes before the child's first tool even starts.
    journal.finish_run(RunState::Completed).unwrap();

    // A second parent turn is now the active run.
    journal
        .begin_run_with_task(Some("second parent turn"), Some("task-parent-2"))
        .unwrap();
    let parent_two = Uuid::new_v4();
    journal
        .record_run_observation(&observation(RunObservationKind::RunStarted, parent_two))
        .unwrap();
    journal
        .project_task_canonical("task-parent-2", Some(&parent_two.to_string()), "running")
        .unwrap();

    // The child starts only now, linking back to the already-final parent run.
    let child = Uuid::new_v4();
    let mut child_started = observation(RunObservationKind::RunStarted, child);
    child_started.parent_run_id = Some(parent);
    journal.record_run_observation(&child_started).unwrap();
    journal
        .record_canonical_phase(&call(child, 1, ToolCallPhase::Normalized, "child-late"))
        .unwrap();
    journal
        .record_canonical_phase(&call(child, 1, ToolCallPhase::Admitted, "child-late"))
        .unwrap();
    journal
        .record_canonical_phase(&call(child, 1, ToolCallPhase::Dispatched, "child-late"))
        .unwrap();
    journal
        .record_canonical_phase(&call(child, 1, ToolCallPhase::Executed, "child-late"))
        .unwrap();
    journal
        .record_run_observation(&observation(RunObservationKind::RunFinished, child))
        .unwrap();

    let runs = stored_runs(dir.path(), "late-child");
    let owner = runs
        .iter()
        .find(|run| call_phase(run, "child-late").is_some())
        .expect("the late child call must be recorded");
    assert_eq!(
        owner["engine_run_id"].as_str(),
        Some(parent.to_string().as_str()),
        "the child belongs to the original parent run: {owner}"
    );
    assert_eq!(
        call_phase(owner, "child-late").as_deref(),
        Some("executed"),
        "{owner}"
    );
    let second = runs
        .iter()
        .find(|run| run["engine_run_id"].as_str() == Some(parent_two.to_string().as_str()))
        .expect("the second parent run exists");
    assert!(
        call_phase(second, "child-late").is_none(),
        "the second active run must not receive the original child's call: {second}"
    );
}

#[test]
fn a_queued_task_projection_does_not_touch_another_tasks_run() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path(), "queued-projection");
    journal
        .begin_run_with_task(Some("first"), Some("task-a"))
        .unwrap();
    let run_a = Uuid::new_v4();
    journal
        .record_run_observation(&observation(RunObservationKind::RunStarted, run_a))
        .unwrap();
    journal
        .project_task_canonical("task-a", Some(&run_a.to_string()), "running")
        .unwrap();

    // A second task is queued while a different run is active. Its projection
    // has no owning run yet, so it must be a no-op rather than writing "queued"
    // onto task-a's run.
    journal
        .project_task_canonical("task-b", None, "queued")
        .unwrap();

    let runs = stored_runs(dir.path(), "queued-projection");
    assert_eq!(
        runs.len(),
        1,
        "no journal run may be created by a projection"
    );
    assert_eq!(runs[0]["task_id"], "task-a");
    assert_eq!(
        runs[0]["canonical_phase"], "running",
        "task-b's queued phase must not pollute task-a's run: {runs:?}"
    );
}

#[test]
fn an_unknown_child_scope_is_not_finalized_by_a_sibling_run_finish() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path(), "unknown-child");
    journal
        .begin_run_with_task(Some("parent"), Some("task-parent"))
        .unwrap();
    let parent = Uuid::new_v4();
    journal
        .record_run_observation(&observation(RunObservationKind::RunStarted, parent))
        .unwrap();
    let child = Uuid::new_v4();
    let mut child_started = observation(RunObservationKind::RunStarted, child);
    child_started.parent_run_id = Some(parent);
    journal.record_run_observation(&child_started).unwrap();
    journal
        .record_canonical_phase(&call(child, 1, ToolCallPhase::Dispatched, "child-live"))
        .unwrap();
    // The whole parent run finalizes twice: the second call must be idempotent
    // and must never rewrite the live child's real phase.
    journal.finish_run(RunState::Completed).unwrap();
    journal.finish_run(RunState::Failed).unwrap();

    let report = journal.recovery_report().unwrap();
    let child_call = report
        .calls
        .iter()
        .find(|call| call.tool_call_id.as_deref() == Some("child-live"))
        .unwrap();
    assert_eq!(child_call.phase, ToolCallPhase::Dispatched, "{report:?}");
}

#[test]
fn crash_recovery_finalizes_every_live_child_scope() {
    let dir = tempfile::tempdir().unwrap();
    {
        let journal = Journal::open(dir.path(), "crash-live-child");
        journal
            .begin_run_with_task(Some("parent"), Some("task-parent"))
            .unwrap();
        let parent = Uuid::new_v4();
        journal
            .record_run_observation(&observation(RunObservationKind::RunStarted, parent))
            .unwrap();
        let child = Uuid::new_v4();
        let mut child_started = observation(RunObservationKind::RunStarted, child);
        child_started.parent_run_id = Some(parent);
        journal.record_run_observation(&child_started).unwrap();
        journal
            .record_canonical_phase(&call(child, 1, ToolCallPhase::Dispatched, "child-live"))
            .unwrap();
        // Simulated process crash: no finish_run.
    }

    // A real process crash ends every scope, including a live child.
    let reopened = Journal::open(dir.path(), "crash-live-child");
    let recovered = reopened.mark_interrupted().unwrap();
    let child_call = recovered
        .calls
        .iter()
        .find(|call| call.tool_call_id.as_deref() == Some("child-live"))
        .unwrap();
    assert_eq!(
        child_call.phase,
        ToolCallPhase::Unknown,
        "crash recovery must finalize a dispatched live child: {recovered:?}"
    );
}

#[test]
fn a_run_owning_a_live_child_scope_is_never_trimmed_away() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path(), "capacity");
    journal
        .begin_run_with_task(Some("protected"), Some("task-protected"))
        .unwrap();
    let parent = Uuid::new_v4();
    journal
        .record_run_observation(&observation(RunObservationKind::RunStarted, parent))
        .unwrap();
    let child = Uuid::new_v4();
    let mut child_started = observation(RunObservationKind::RunStarted, child);
    child_started.parent_run_id = Some(parent);
    journal.record_run_observation(&child_started).unwrap();
    journal
        .record_canonical_phase(&call(
            child,
            1,
            ToolCallPhase::Dispatched,
            "protected-child",
        ))
        .unwrap();
    let parent_scope = parent.to_string();

    // Overflow the bounded run list well past MAX_JOURNAL_RUNS (64).
    for index in 0..70 {
        journal
            .begin_run_with_task(Some("filler"), Some(&format!("task-filler-{index}")))
            .unwrap();
    }

    let runs = stored_runs(dir.path(), "capacity");
    let protected = runs
        .iter()
        .find(|run| run["engine_run_id"].as_str() == Some(parent_scope.as_str()))
        .expect("the run owning a live child scope must never be trimmed");
    assert_eq!(
        call_phase(protected, "protected-child").as_deref(),
        Some("dispatched"),
        "a live child's dispatched evidence must survive capacity pressure: {protected}"
    );
    assert!(
        runs.len() <= 64,
        "the run list stays bounded: {}",
        runs.len()
    );
}

/// A real child scope whose parent was never observed must fail closed: it may
/// never be attached to whichever unrelated turn is active next.
#[test]
fn an_unknown_parent_scope_is_never_bound_to_the_next_turn() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path(), "unknown-parent");
    journal
        .begin_run_with_task(Some("parent one"), Some("task-parent"))
        .unwrap();
    let parent = Uuid::new_v4();
    journal
        .record_run_observation(&observation(RunObservationKind::RunStarted, parent))
        .unwrap();

    // The child links to a parent scope this journal never observed. Binding
    // must report a failure instead of silently leaving the child unowned.
    let child = Uuid::new_v4();
    let mut child_started = observation(RunObservationKind::RunStarted, child);
    child_started.parent_run_id = Some(Uuid::new_v4());
    assert!(
        journal.record_run_observation(&child_started).is_err(),
        "an unknown parent must be reported, not silently accepted"
    );

    // A second, unrelated turn opens before any real result arrives.
    journal
        .begin_run_with_task(Some("parent two"), Some("task-parent-2"))
        .unwrap();
    let second = Uuid::new_v4();
    journal
        .record_run_observation(&observation(RunObservationKind::RunStarted, second))
        .unwrap();

    // The unowned child's canonical dispatch is refused, never attached to the
    // unrelated active turn.
    assert!(
        journal
            .record_canonical_phase(&call(child, 1, ToolCallPhase::Dispatched, "orphan"))
            .is_err(),
        "an observed-but-unbound scope must not fall back to the active run"
    );
    // The second turn's own scope still routes normally.
    journal
        .record_canonical_phase(&call(second, 1, ToolCallPhase::Dispatched, "second-1"))
        .unwrap();

    // The guard survives reopening the journal file: a fresh instance with its
    // own active run must still refuse the unowned scope.
    let reopened = Journal::open(dir.path(), "unknown-parent");
    reopened
        .begin_run_with_task(Some("parent three"), Some("task-parent-3"))
        .unwrap();
    assert!(
        reopened
            .record_canonical_phase(&call(child, 1, ToolCallPhase::Dispatched, "orphan-late"))
            .is_err(),
        "a recovered journal must keep refusing the unowned scope"
    );

    let runs = stored_runs(dir.path(), "unknown-parent");
    for id in ["orphan", "orphan-late"] {
        assert!(
            runs.iter().all(|run| call_phase(run, id).is_none()),
            "the unowned child call {id} must not be present in any run: {runs:?}"
        );
    }
    let second_owner = runs
        .iter()
        .find(|run| call_phase(run, "second-1").is_some())
        .expect("the second turn keeps its own call");
    assert_eq!(
        second_owner["engine_scopes"].as_array().unwrap().len(),
        1,
        "the second run only owns its own top scope: {second_owner}"
    );
}

/// A binding that could not be persisted must leave the real scope fail-closed,
/// and the recovered journal file must still refuse to attach it to a later
/// unrelated turn.
#[test]
fn a_binding_persist_failure_stays_fail_closed_after_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path(), "persist-fault");
    journal
        .begin_run_with_task(Some("parent one"), Some("task-parent"))
        .unwrap();
    let parent = Uuid::new_v4();
    journal
        .record_run_observation(&observation(RunObservationKind::RunStarted, parent))
        .unwrap();

    // Fault injection: `.mio/outputs` is a regular file, so the shared output
    // lease the journal requires before any write is refused. This is the same
    // refusal a real busy/cleanup workspace produces, and it leaves the
    // existing journal file byte-for-byte untouched.
    let outputs = dir.path().join(".mio/outputs");
    std::fs::write(&outputs, b"not a directory").unwrap();

    let child = Uuid::new_v4();
    let mut child_started = observation(RunObservationKind::RunStarted, child);
    child_started.parent_run_id = Some(parent);
    assert!(
        journal.record_run_observation(&child_started).is_err(),
        "a binding that cannot persist must be reported"
    );

    // Remove the fault: a later unrelated turn opens and becomes active.
    std::fs::remove_file(&outputs).unwrap();
    journal
        .begin_run_with_task(Some("parent two"), Some("task-parent-2"))
        .unwrap();
    let second = Uuid::new_v4();
    journal
        .record_run_observation(&observation(RunObservationKind::RunStarted, second))
        .unwrap();

    assert!(
        journal
            .record_canonical_phase(&call(child, 1, ToolCallPhase::Dispatched, "faulted"))
            .is_err(),
        "the failed binding stays fail-closed"
    );
    journal
        .record_canonical_phase(&call(second, 1, ToolCallPhase::Dispatched, "second-1"))
        .unwrap();

    let reopened = Journal::open(dir.path(), "persist-fault");
    reopened
        .begin_run_with_task(Some("parent three"), Some("task-parent-3"))
        .unwrap();
    assert!(
        reopened
            .record_canonical_phase(&call(child, 1, ToolCallPhase::Dispatched, "faulted-late"))
            .is_err(),
        "the recovered journal must keep refusing the unpersisted binding"
    );

    let runs = stored_runs(dir.path(), "persist-fault");
    for id in ["faulted", "faulted-late"] {
        assert!(
            runs.iter().all(|run| call_phase(run, id).is_none()),
            "the mis-bindable call {id} must not be recorded in any run: {runs:?}"
        );
    }
    assert!(
        runs.iter().any(|run| call_phase(run, "second-1").is_some()),
        "the unrelated turn keeps its own call: {runs:?}"
    );
}

/// Across the per-run canonical call capacity, a live child scope's call must
/// never be dropped. Non-live calls are trimmed first; when every call belongs
/// to a live scope the admission is refused and the previous file/state stays.
#[test]
fn live_scope_calls_survive_the_canonical_call_cap() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path(), "call-cap");
    journal
        .begin_run_with_task(Some("parent"), Some("task-parent"))
        .unwrap();
    let parent = Uuid::new_v4();
    journal
        .record_run_observation(&observation(RunObservationKind::RunStarted, parent))
        .unwrap();
    // One real live child scope owns the call that must survive.
    let child = Uuid::new_v4();
    let mut child_started = observation(RunObservationKind::RunStarted, child);
    child_started.parent_run_id = Some(parent);
    journal.record_run_observation(&child_started).unwrap();
    journal
        .record_canonical_phase(&call(child, 0, ToolCallPhase::Dispatched, "live-child"))
        .unwrap();

    // Fill the run far past the call cap with calls from an ordinary (not
    // observed as live) scope; the live child's evidence must not be trimmed.
    let filler_scope = Uuid::new_v4();
    for index in 0..(mio_coding_agent::journal::MAX_RUN_CALLS + 32) {
        journal
            .record_canonical_phase(&call(
                filler_scope,
                index as u64 + 1,
                ToolCallPhase::Dispatched,
                &format!("filler-{index}"),
            ))
            .unwrap();
    }

    let report = journal.recovery_report().unwrap();
    let live = report
        .calls
        .iter()
        .find(|call| call.tool_call_id.as_deref() == Some("live-child"))
        .expect("the live child's call must survive capacity pressure");
    assert_eq!(live.phase, ToolCallPhase::Dispatched, "{report:?}");
    assert!(
        report.calls.len() <= mio_coding_agent::journal::MAX_RUN_CALLS,
        "the call list stays bounded: {}",
        report.calls.len()
    );
}

/// When every canonical call in a run belongs to a live scope, the admission
/// must be refused instead of silently deleting live evidence, and the journal
/// file must keep exactly the last committed state.
#[test]
fn an_all_live_call_cap_refuses_admission_and_preserves_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path(), "all-live-cap");
    journal
        .begin_run_with_task(Some("parent"), Some("task-parent"))
        .unwrap();
    let parent = Uuid::new_v4();
    journal
        .record_run_observation(&observation(RunObservationKind::RunStarted, parent))
        .unwrap();
    // Several live child scopes, each contributing calls, so the run reaches
    // its call cap with every call owned by a live scope.
    let live_scopes: Vec<Uuid> = (0..8).map(|_| Uuid::new_v4()).collect();
    for scope in &live_scopes {
        let mut started = observation(RunObservationKind::RunStarted, *scope);
        started.parent_run_id = Some(parent);
        journal.record_run_observation(&started).unwrap();
    }
    let cap = mio_coding_agent::journal::MAX_RUN_CALLS;
    let per_scope = cap / live_scopes.len();
    for (scope_index, scope) in live_scopes.iter().enumerate() {
        for call_index in 0..per_scope {
            journal
                .record_canonical_phase(&call(
                    *scope,
                    call_index as u64 + 1,
                    ToolCallPhase::Dispatched,
                    &format!("live-{scope_index}-{call_index}"),
                ))
                .unwrap();
        }
    }
    let filled = journal.recovery_report().unwrap().calls.len();
    assert_eq!(filled, cap, "the run is exactly at the call cap");

    let before = std::fs::read(journal_path(dir.path(), "all-live-cap")).unwrap();
    let overflow = journal.record_canonical_phase(&call(
        live_scopes[0],
        per_scope as u64 + 1,
        ToolCallPhase::Dispatched,
        "live-overflow",
    ));
    assert!(
        overflow.is_err(),
        "an all-live cap must refuse the admission instead of dropping live evidence"
    );
    let after = std::fs::read(journal_path(dir.path(), "all-live-cap")).unwrap();
    assert_eq!(
        before, after,
        "a refused admission preserves the journal file"
    );
    let report = journal.recovery_report().unwrap();
    assert_eq!(
        report.calls.len(),
        cap,
        "no live call was dropped: {report:?}"
    );
    assert!(
        report
            .calls
            .iter()
            .all(|call| call.tool_call_id.as_deref() != Some("live-overflow")),
        "the refused call never entered the journal"
    );
}
