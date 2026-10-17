//! DES-05 journal correlation tests (offline).
//!
//! The execution journal must pair a recorded result with the exact
//! side-effecting call it belongs to, even when several calls to the same tool
//! with the same target are in flight at once. These tests drive the real
//! public surface only:
//!
//! - an [`ApprovalBinding`] pins an intent to its real engine run scope, the
//!   provider's own tool-call id and the approval attempt id;
//! - `record_result` reads the real `tool_identity::current()` bound to the
//!   executing task.
//!
//! No provider, bus, network or extra dependency is involved.
//!
//! What is asserted: identical concurrent calls keep their own dispatch
//! identity even when their results land in reverse order; a result from
//! another scope never completes a foreign record; legacy operation/target
//! records are completed only by results without a real identity and never by
//! a pinned call; terminal records never move again; and corrupt, refused or
//! disabled (no-session) journals write nothing.

use mio_coding_agent::{
    journal::{JOURNAL_VERSION, Journal, JournalFile, OpStatus, RunState, journal_path},
    policy::{ApprovalBinding, EntryKind},
    tool_identity::{self, ToolIdentity},
};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};

/// A real approval binding for a `write` call: the engine run scope, the
/// provider's own tool-call id and a distinct approval attempt id.
fn bound_write(scope: &str, tool_call_id: &str) -> ApprovalBinding {
    ApprovalBinding {
        entry: EntryKind::Builtin,
        tool: "write".into(),
        operation: "write".into(),
        cwd: "/workspace".into(),
        scope: scope.into(),
        params_sha256: "0".repeat(64),
        call_id: uuid::Uuid::new_v4().to_string(),
        tool_call_id: Some(tool_call_id.into()),
    }
}

fn stored(root: &Path, session_id: &str) -> JournalFile {
    serde_json::from_slice(&std::fs::read(journal_path(root, session_id)).unwrap()).unwrap()
}

fn raw_file(root: &Path, session_id: &str) -> Value {
    serde_json::from_slice(&std::fs::read(journal_path(root, session_id)).unwrap()).unwrap()
}

/// Two identical calls on one run land their results in reverse order yet keep
/// their own identity: real scope, tool-call id, turn and effect id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_same_tool_and_target_keep_their_own_dispatch_identity() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open(dir.path(), "concurrent"));
    journal.begin_run().unwrap();
    let run_id = uuid::Uuid::new_v4();
    let scope = run_id.to_string();
    // Same tool, same target: only the real dispatch identity tells them apart.
    journal
        .record_bound_intent("write", "a.txt", &bound_write(&scope, "call-a"))
        .unwrap();
    journal
        .record_bound_intent("write", "a.txt", &bound_write(&scope, "call-b"))
        .unwrap();

    let meeting = Arc::new(tokio::sync::Barrier::new(2));
    let later_finished = Arc::new(tokio::sync::Notify::new());

    let first = tokio::spawn({
        let journal = journal.clone();
        let meeting = meeting.clone();
        let later_finished = later_finished.clone();
        async move {
            tool_identity::scope(
                Some(ToolIdentity::new(run_id, "call-a", 4, 41)),
                async move {
                    meeting.wait().await;
                    // The later call records first: completion order is
                    // deliberately reversed.
                    later_finished.notified().await;
                    journal
                        .record_result("write", "a.txt", &Ok("written-by-call-a".into()))
                        .unwrap();
                },
            )
            .await;
        }
    });
    let second = tokio::spawn({
        let journal = journal.clone();
        let meeting = meeting.clone();
        async move {
            tool_identity::scope(
                Some(ToolIdentity::new(run_id, "call-b", 5, 42)),
                async move {
                    meeting.wait().await;
                    journal
                        .record_result("write", "a.txt", &Ok("written-by-call-b".into()))
                        .unwrap();
                    later_finished.notify_one();
                },
            )
            .await;
        }
    });
    first.await.unwrap();
    second.await.unwrap();
    journal.finish_run(RunState::Completed).unwrap();

    let file = stored(dir.path(), "concurrent");
    let ops = &file.runs[0].operations;
    assert_eq!(ops.len(), 2, "{ops:?}");
    assert!(ops.iter().all(|op| op.status == OpStatus::Executed));
    assert_eq!(ops[0].correlation.tool_call_id.as_deref(), Some("call-a"));
    assert_eq!(ops[1].correlation.tool_call_id.as_deref(), Some("call-b"));
    assert_eq!(ops[0].detail.as_deref(), Some("written-by-call-a"));
    assert_eq!(ops[1].detail.as_deref(), Some("written-by-call-b"));
    // The real turn and effect id come from each call's own identity.
    assert_eq!(ops[0].correlation.turn, Some(4));
    assert_eq!(ops[1].correlation.turn, Some(5));
    assert_eq!(ops[0].correlation.effect_id, Some(41));
    assert_eq!(ops[1].correlation.effect_id, Some(42));
    assert!(
        ops.iter()
            .all(|op| op.correlation.scope.as_deref() == Some(scope.as_str()))
    );
    assert!(ops.iter().all(|op| op.correlation.call_id.is_some()));
}

/// The same provider tool-call id in another run scope is a different call and
/// never completes a foreign record.
#[tokio::test]
async fn a_result_from_another_scope_never_completes_a_bound_call() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open(dir.path(), "scopes"));
    journal.begin_run().unwrap();
    let run_id = uuid::Uuid::new_v4();
    let other_run = uuid::Uuid::new_v4();
    journal
        .record_bound_intent(
            "write",
            "a.txt",
            &bound_write(&run_id.to_string(), "shared-id"),
        )
        .unwrap();

    tool_identity::scope(Some(ToolIdentity::new(other_run, "shared-id", 1, 1)), {
        let journal = journal.clone();
        async move {
            journal
                .record_result("write", "a.txt", &Ok("from-another-run".into()))
                .unwrap();
        }
    })
    .await;
    let file = stored(dir.path(), "scopes");
    assert_eq!(file.runs[0].operations[0].status, OpStatus::Pending);
    assert!(file.runs[0].operations[0].detail.is_none());

    // Its own scope and call id do complete it, with the real turn/effect.
    tool_identity::scope(Some(ToolIdentity::new(run_id, "shared-id", 7, 77)), {
        let journal = journal.clone();
        async move {
            journal
                .record_result("write", "a.txt", &Ok("own-result".into()))
                .unwrap();
        }
    })
    .await;
    let file = stored(dir.path(), "scopes");
    let op = &file.runs[0].operations[0];
    assert_eq!(op.status, OpStatus::Executed);
    assert_eq!(op.detail.as_deref(), Some("own-result"));
    assert_eq!(op.correlation.turn, Some(7));
    assert_eq!(op.correlation.effect_id, Some(77));
}

/// A legacy intent is completed by a legacy result, and its on-disk shape stays
/// byte-compatible: no correlation key is invented.
#[test]
fn legacy_intents_are_completed_only_by_a_legacy_result() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path(), "legacy-live");
    journal.begin_run().unwrap();
    journal.record_intent("write", "legacy.txt").unwrap();
    // No real dispatch identity: the historical operation/target match applies.
    journal
        .record_result("write", "legacy.txt", &Ok("legacy ok".into()))
        .unwrap();
    let file = stored(dir.path(), "legacy-live");
    let op = &file.runs[0].operations[0];
    assert_eq!(op.status, OpStatus::Executed);
    assert_eq!(op.detail.as_deref(), Some("legacy ok"));
    assert!(op.correlation.is_none());
    assert!(
        raw_file(dir.path(), "legacy-live")["runs"][0]["operations"][0]
            .get("correlation")
            .is_none(),
        "a record without real correlation must not gain a fabricated one"
    );
}

/// An operation/target coincidence never completes a call that is already
/// pinned to its own dispatch, while a real identity does complete its own
/// record.
#[tokio::test]
async fn a_legacy_result_never_completes_a_bound_call_by_operation_and_target() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open(dir.path(), "no-cross"));
    journal.begin_run().unwrap();
    let run_id = uuid::Uuid::new_v4();
    journal
        .record_bound_intent(
            "write",
            "a.txt",
            &bound_write(&run_id.to_string(), "call-1"),
        )
        .unwrap();
    // A direct, engine-less result carries no real identity, so it must not
    // complete a call that has its own dispatch.
    journal
        .record_result("write", "a.txt", &Ok("engine-less".into()))
        .unwrap();
    let file = stored(dir.path(), "no-cross");
    assert_eq!(file.runs[0].operations[0].status, OpStatus::Pending);

    // The same real dispatch identity completes its own record even when the
    // reported operation/target differ from the recorded ones: the identity is
    // the ground truth of the dispatch, never the label coincidence.
    tool_identity::scope(Some(ToolIdentity::new(run_id, "call-1", 2, 2)), {
        let journal = journal.clone();
        async move {
            journal
                .record_result("edit", "other.txt", &Ok("own-result".into()))
                .unwrap();
        }
    })
    .await;
    let file = stored(dir.path(), "no-cross");
    let op = &file.runs[0].operations[0];
    assert_eq!(op.status, OpStatus::Executed);
    assert_eq!(op.detail.as_deref(), Some("own-result"));
}

/// A repeated result or finish never rewrites a terminal record or run.
#[tokio::test]
async fn a_repeated_result_or_finish_never_rewrites_a_terminal_record() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open(dir.path(), "terminal"));
    journal.begin_run().unwrap();
    let run_id = uuid::Uuid::new_v4();
    journal
        .record_bound_intent(
            "write",
            "a.txt",
            &bound_write(&run_id.to_string(), "call-1"),
        )
        .unwrap();
    tool_identity::scope(Some(ToolIdentity::new(run_id, "call-1", 1, 1)), {
        let journal = journal.clone();
        async move {
            journal
                .record_result("write", "a.txt", &Ok("first".into()))
                .unwrap();
            // The same call reports again: a terminal record is not revisited.
            journal
                .record_result("write", "a.txt", &Err("second".into()))
                .unwrap();
        }
    })
    .await;
    journal.finish_run(RunState::Completed).unwrap();
    // A late finish cannot roll the run's terminal state back.
    journal.finish_run(RunState::Failed).unwrap();

    let file = stored(dir.path(), "terminal");
    assert_eq!(file.runs[0].state, RunState::Completed);
    assert!(file.runs[0].ended.is_some());
    let op = &file.runs[0].operations[0];
    assert_eq!(op.status, OpStatus::Executed);
    assert_eq!(op.detail.as_deref(), Some("first"));
}

/// The recovery view exposes the real dispatch correlation of a bound record.
#[tokio::test]
async fn the_recovery_report_exposes_the_real_dispatch_correlation() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open(dir.path(), "reported"));
    journal.begin_run().unwrap();
    let run_id = uuid::Uuid::new_v4();
    let scope = run_id.to_string();
    let binding = bound_write(&scope, "call-reported");
    journal
        .record_bound_intent("write", "a.txt", &binding)
        .unwrap();
    tool_identity::scope(Some(ToolIdentity::new(run_id, "call-reported", 6, 66)), {
        let journal = journal.clone();
        async move {
            journal
                .record_result("write", "a.txt", &Ok("ok".into()))
                .unwrap();
        }
    })
    .await;
    journal.finish_run(RunState::Completed).unwrap();

    let report = Journal::open(dir.path(), "reported")
        .recovery_report()
        .unwrap();
    assert_eq!(report.executed_operations, 1);
    let operation = &report.executed[0];
    assert_eq!(operation.correlation.scope.as_deref(), Some(scope.as_str()));
    assert_eq!(
        operation.correlation.tool_call_id.as_deref(),
        Some("call-reported")
    );
    assert_eq!(
        operation.correlation.call_id.as_deref(),
        Some(binding.call_id.as_str())
    );
    assert_eq!(operation.correlation.turn, Some(6));
    assert_eq!(operation.correlation.effect_id, Some(66));
}

/// A journal written before the correlation fields existed still loads, keeps
/// its status semantics and reports no invented ids.
#[test]
fn a_journal_without_correlation_fields_loads_and_reports_no_invented_ids() {
    let dir = tempfile::tempdir().unwrap();
    let path = journal_path(dir.path(), "legacy-file");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let legacy = serde_json::to_vec_pretty(&json!({
        "version": JOURNAL_VERSION,
        "session_id": "legacy-file",
        "updated": 1,
        "runs": [{
            "run_id": "legacy-run",
            "turn": 1,
            "started": 1,
            "state": "active",
            "operations": [{
                "op_id": 1,
                "operation": "write",
                "target": "legacy.txt",
                "status": "pending",
                "started": 1
            }]
        }]
    }))
    .unwrap();
    std::fs::write(&path, &legacy).unwrap();

    let report = Journal::open(dir.path(), "legacy-file")
        .recovery_report()
        .unwrap();
    assert!(report.journal_error.is_none());
    assert_eq!(report.interrupted_runs, 1);
    assert_eq!(report.unknown_operations.len(), 1);
    let operation = &report.unknown_operations[0];
    assert_eq!(operation.operation, "write");
    assert_eq!(operation.status, OpStatus::Pending);
    assert!(
        operation.correlation.is_none(),
        "absent metadata is never fabricated"
    );
    // Opening and reporting are read-only.
    assert_eq!(std::fs::read(&path).unwrap(), legacy);
}

/// Corrupt evidence is preserved and a write that cannot land is refused.
#[test]
fn corrupt_or_unwritable_journals_refuse_bound_intents_and_keep_their_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let path = journal_path(dir.path(), "corrupt");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{ not json").unwrap();
    let journal = Journal::open(dir.path(), "corrupt");
    assert!(journal.recovery_report().unwrap().journal_error.is_some());
    assert!(
        journal
            .record_bound_intent("write", "a.txt", &bound_write("run-scope", "call-1"))
            .is_err()
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");

    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path(), "unwritable");
    journal.begin_run().unwrap();
    let path = journal_path(dir.path(), "unwritable");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let error = journal
        .record_bound_intent("write", "a.txt", &bound_write("run-scope", "call-1"))
        .unwrap_err();
    assert!(!error.is_empty());
    assert!(path.is_dir(), "a refused intent must write nothing");
}

/// A disabled journal (the `--no-session` mode) writes nothing at all.
#[test]
fn a_disabled_journal_records_no_bound_intent_result_or_run() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::disabled();
    assert!(
        journal
            .record_bound_intent("write", "a.txt", &bound_write("run-scope", "call-1"))
            .is_ok()
    );
    assert!(
        journal
            .record_result("write", "a.txt", &Ok("ok".into()))
            .is_ok()
    );
    assert!(journal.finish_run(RunState::Completed).is_ok());
    assert!(journal.recovery_report().is_none());
    assert!(!dir.path().join(".mio").exists());
}

#[test]
fn invalid_binding_ids_are_refused_without_publishing_a_truncated_identity() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path(), "bounded-identity");
    journal.begin_run().unwrap();
    let path = journal_path(dir.path(), "bounded-identity");
    let before = std::fs::read(&path).unwrap();
    for field in ["scope", "call_id", "tool_call_id"] {
        for value in [
            "a".repeat(513),
            "界".repeat(171),
            "bad\nid".into(),
            " ".into(),
        ] {
            let mut binding = bound_write("valid-scope", "valid-tool-call");
            match field {
                "scope" => binding.scope = value,
                "call_id" => binding.call_id = value,
                _ => binding.tool_call_id = Some(value),
            }
            assert!(
                journal
                    .record_bound_intent("write", "a.txt", &binding)
                    .is_err()
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
    }
}
