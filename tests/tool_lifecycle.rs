//! DES-05 工具调用权威状态机的离线回归。
//!
//! 覆盖纯状态定义、非法转移拒绝、终态幂等、同一 `(scope, provider id)` 只派发一次、
//! canonical journal 的完整阶段链、崩溃恢复阶段区分、v1 读取不伪造 phase、首次真实
//! 写入升级、真实 typed 输出引用保留、以及 no-session 零落盘。真实引擎的端到端链在
//! `tests/tool_lifecycle_runtime.rs`。

use mio_coding_agent::{
    AgentEvent, Host, HostFuture, ToolFilter,
    journal::{JOURNAL_VERSION, JOURNAL_VERSION_V1, Journal, RunState, journal_path},
    policy::{PolicyContext, ToolPolicy},
    shell::output_store::{OutputEntry, OutputReference, OutputStore},
    tool_lifecycle::{ToolCallPhase, ToolCallRecord, ToolCallTracker, ToolLifecycleError},
};
use std::{path::Path, sync::Arc};

struct NullHost;

impl Host for NullHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }
    fn on_event(&self, _: AgentEvent) {}
}

fn call(
    scope: &str,
    index: u64,
    phase: ToolCallPhase,
    tool_call_id: Option<&str>,
) -> ToolCallRecord {
    ToolCallRecord {
        call_index: index,
        scope: scope.into(),
        phase,
        tool: "read".into(),
        operation: "read".into(),
        params_sha256: "0".repeat(64),
        target: "a.txt".into(),
        target_redacted: false,
        tool_call_id: tool_call_id.map(str::to_owned),
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

#[test]
fn the_state_machine_rejects_illegal_transitions_and_is_terminal_idempotent() {
    use ToolCallPhase::*;
    assert!(ToolCallPhase::transition_allowed(Normalized, Admitted));
    assert!(ToolCallPhase::transition_allowed(Normalized, Denied));
    assert!(ToolCallPhase::transition_allowed(Admitted, Dispatched));
    assert!(ToolCallPhase::transition_allowed(Dispatched, Executed));
    assert!(ToolCallPhase::transition_allowed(Dispatched, Failed));
    assert!(ToolCallPhase::transition_allowed(Dispatched, Unknown));
    // Strictly illegal: no dispatch without admission, no terminal rollback.
    assert!(!ToolCallPhase::transition_allowed(Normalized, Dispatched));
    assert!(!ToolCallPhase::transition_allowed(Denied, Admitted));
    assert!(!ToolCallPhase::transition_allowed(Executed, Failed));
    assert!(!ToolCallPhase::transition_allowed(Unknown, Executed));
    for terminal in [Denied, Executed, Failed, Unknown, NotDispatched] {
        assert!(ToolCallPhase::transition_allowed(terminal, terminal));
        assert!(terminal.is_terminal());
    }
}

#[test]
fn the_tracker_refuses_a_duplicate_dispatch_for_the_same_scope_and_provider_id() {
    let tracker = ToolCallTracker::new("run-1", Arc::new(NullHost));
    let index = tracker
        .begin("read", "read", "a.txt", Some("call-1"), Some(1), Some(5))
        .unwrap();
    tracker.admit(index, Some(1), Some(5)).unwrap();
    tracker.mark_dispatched(5, Some("call-1")).unwrap();
    assert!(matches!(
        tracker.mark_dispatched(5, Some("call-1")),
        Err(ToolLifecycleError::Duplicate(_))
    ));
}

#[test]
fn a_full_journal_records_the_whole_stage_chain_and_a_denial_is_not_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path(), "chain");
    journal.begin_run().unwrap();
    // A complete read chain: Normalized -> Admitted -> Dispatched -> Executed.
    journal
        .record_canonical_phase(&call("run-1", 1, ToolCallPhase::Normalized, Some("c1")))
        .unwrap();
    journal
        .record_canonical_phase(&call("run-1", 1, ToolCallPhase::Admitted, Some("c1")))
        .unwrap();
    journal
        .record_canonical_phase(&call("run-1", 1, ToolCallPhase::Dispatched, Some("c1")))
        .unwrap();
    journal
        .record_canonical_phase(&call("run-1", 1, ToolCallPhase::Executed, Some("c1")))
        .unwrap();
    // A filter/approval denial is Denied before dispatch, not Unknown.
    journal
        .record_canonical_phase(&call("run-1", 2, ToolCallPhase::Normalized, Some("c2")))
        .unwrap();
    journal
        .record_canonical_phase(&call("run-1", 2, ToolCallPhase::Denied, Some("c2")))
        .unwrap();
    journal.finish_run(RunState::Completed).unwrap();

    let report = Journal::open(dir.path(), "chain")
        .recovery_report()
        .unwrap();
    assert_eq!(report.calls.len(), 2, "{report:?}");
    let executed = report
        .calls
        .iter()
        .find(|call| call.tool_call_id.as_deref() == Some("c1"))
        .unwrap();
    assert_eq!(executed.phase, ToolCallPhase::Executed);
    let denied = report
        .calls
        .iter()
        .find(|call| call.tool_call_id.as_deref() == Some("c2"))
        .unwrap();
    assert_eq!(denied.phase, ToolCallPhase::Denied);
    assert!(
        !report.has_unfinished(),
        "an executed read and a denial are not unknown side effects: {report:?}"
    );
}

#[test]
fn crash_recovery_separates_unknown_dispatch_from_predispatch_interruption() {
    let dir = tempfile::tempdir().unwrap();
    {
        let journal = Journal::open(dir.path(), "crash");
        journal.begin_run().unwrap();
        journal
            .record_canonical_phase(&call("run-1", 1, ToolCallPhase::Normalized, Some("c1")))
            .unwrap();
        journal
            .record_canonical_phase(&call("run-1", 1, ToolCallPhase::Admitted, Some("c1")))
            .unwrap();
        journal
            .record_canonical_phase(&call("run-1", 1, ToolCallPhase::Dispatched, Some("c1")))
            .unwrap();
        // A second call was only admitted; it never reached the bus.
        journal
            .record_canonical_phase(&call("run-1", 2, ToolCallPhase::Normalized, Some("c2")))
            .unwrap();
        journal
            .record_canonical_phase(&call("run-1", 2, ToolCallPhase::Admitted, Some("c2")))
            .unwrap();
        // Simulated crash: no finish_run.
    }
    let reopened = Journal::open(dir.path(), "crash");
    let report = reopened.mark_interrupted().unwrap();
    let dispatched = report
        .calls
        .iter()
        .find(|call| call.tool_call_id.as_deref() == Some("c1"))
        .unwrap();
    assert_eq!(
        dispatched.phase,
        ToolCallPhase::Unknown,
        "a dispatched call with no confirmed result is unknown"
    );
    let admitted = report
        .calls
        .iter()
        .find(|call| call.tool_call_id.as_deref() == Some("c2"))
        .unwrap();
    assert_eq!(
        admitted.phase,
        ToolCallPhase::NotDispatched,
        "an admitted-but-not-dispatched call must never be reported as unknown"
    );
    assert!(report.has_unfinished());
}

#[test]
fn a_v1_journal_reads_without_fabricated_phases_and_upgrades_on_first_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = journal_path(dir.path(), "legacy");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let fixture = serde_json::json!({
        "version": JOURNAL_VERSION_V1,
        "session_id": "legacy",
        "updated": 1,
        "runs": [{
            "run_id": "run-1",
            "turn": 1,
            "started": 1,
            "state": "completed",
            "operations": [{
                "op_id": 1,
                "operation": "write",
                "target": "a.txt",
                "status": "executed",
                "started": 1,
                "finished": 2,
                "detail": "ok"
            }]
        }]
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&fixture).unwrap()).unwrap();

    let journal = Journal::open(dir.path(), "legacy");
    let report = journal.recovery_report().unwrap();
    assert!(report.journal_error.is_none(), "{report:?}");
    assert_eq!(report.executed_operations, 1, "{report:?}");
    assert!(
        report.calls.is_empty(),
        "a v1 file must never gain fabricated canonical phases: {report:?}"
    );
    // A query is read-only: the on-disk file is still v1.
    let stored: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(stored["version"], JOURNAL_VERSION_V1);

    // The first real write upgrades the format and preserves legacy evidence.
    journal.begin_run().unwrap();
    journal.finish_run(RunState::Completed).unwrap();
    let stored: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(stored["version"], JOURNAL_VERSION);
    assert_eq!(
        stored["runs"][0]["operations"][0]["status"], "executed",
        "the original legacy operation evidence must not be deleted: {stored}"
    );
}

#[test]
fn committed_output_references_are_preserved_on_the_canonical_call() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path(), "refs");
    journal.begin_run().unwrap();
    journal
        .record_canonical_phase(&call("run-1", 1, ToolCallPhase::Dispatched, Some("c1")))
        .unwrap();
    let reference = OutputReference {
        path: ".mio/outputs/abcd.stdout".into(),
        total_bytes: 11,
        stream: "stdout".into(),
        call_id: Some("c1".into()),
        sha256: "a".repeat(64),
    };
    journal
        .record_canonical_detail(
            "run-1",
            Some("c1"),
            "hello world",
            std::slice::from_ref(&reference),
        )
        .unwrap();
    journal.finish_run(RunState::Completed).unwrap();

    let report = Journal::open(dir.path(), "refs").recovery_report().unwrap();
    assert_eq!(report.calls.len(), 1, "{report:?}");
    assert_eq!(report.calls[0].detail.as_deref(), Some("hello world"));
    assert_eq!(report.calls[0].output_references, vec![reference]);
}

#[test]
fn an_engine_less_unknown_cannot_rewrite_a_denied_call() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path(), "monotonic");
    journal.begin_run().unwrap();
    let mut record = call("run-1", 1, ToolCallPhase::Denied, Some("c1"));
    journal.record_canonical_phase(&record).unwrap();
    // A later `Unknown`/`Failed` write for the same call must not regress it.
    record.phase = ToolCallPhase::Unknown;
    journal.record_canonical_phase(&record).unwrap();
    journal.finish_run(RunState::Completed).unwrap();
    let report = Journal::open(dir.path(), "monotonic")
        .recovery_report()
        .unwrap();
    assert_eq!(report.calls[0].phase, ToolCallPhase::Denied, "{report:?}");
}

#[test]
fn a_disabled_journal_writes_no_canonical_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::disabled();
    journal.begin_run().unwrap();
    journal
        .record_canonical_phase(&call("run-1", 1, ToolCallPhase::Normalized, Some("c1")))
        .unwrap();
    journal.finish_run(RunState::Completed).unwrap();
    assert!(journal.recovery_report().is_none());
    assert!(!dir.path().join(".mio").exists());
}

#[test]
fn a_journal_write_failure_refuses_the_canonical_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = journal_path(dir.path(), "broken");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let journal = Journal::open(dir.path(), "broken");
    journal.begin_run().unwrap();
    // Turn the file into a directory so the next write must fail.
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let error = journal
        .record_canonical_phase(&call("run-1", 1, ToolCallPhase::Normalized, Some("c1")))
        .unwrap_err();
    assert!(!error.is_empty(), "a refused write must report a reason");
    // The refusal must not fabricate a phase on disk.
    let report = Journal::open(dir.path(), "broken")
        .recovery_report()
        .unwrap();
    assert!(report.journal_error.is_some(), "{report:?}");
}

#[derive(Default)]
struct CountingHost {
    writes: std::sync::atomic::AtomicUsize,
}

impl Host for CountingHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }
    fn on_event(&self, _: AgentEvent) {}
    fn on_tool_call_lifecycle(&self, _: &ToolCallRecord) -> Result<(), String> {
        self.writes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

/// Same-call transitions are serialized (a duplicate dispatch can win only
/// once) while distinct calls dispatch without rejecting each other, and every
/// persistence callback runs outside the tracker's state lock.
#[test]
fn concurrent_same_call_transitions_serialize_while_distinct_calls_proceed() {
    let tracker = ToolCallTracker::new("run-concurrent", Arc::new(CountingHost::default()));
    let mut distinct = Vec::new();
    for index in 0..8u64 {
        distinct.push(
            tracker
                .begin(
                    "read",
                    "read",
                    "a.txt",
                    Some(&format!("call-{index}")),
                    None,
                    Some(index + 1),
                )
                .unwrap(),
        );
    }
    for (position, index) in distinct.iter().enumerate() {
        tracker
            .admit(*index, None, Some(position as u64 + 1))
            .unwrap();
    }
    let handles: Vec<_> = (0..8u64)
        .map(|index| {
            let tracker = tracker.clone();
            std::thread::spawn(move || {
                tracker.mark_dispatched(index + 1, Some(&format!("call-{index}")))
            })
        })
        .collect();
    for handle in handles {
        assert!(handle.join().unwrap().is_ok());
    }

    let index = tracker
        .begin(
            "read",
            "read",
            "same.txt",
            Some("same-call"),
            None,
            Some(100),
        )
        .unwrap();
    tracker.admit(index, None, Some(100)).unwrap();
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let tracker = tracker.clone();
            std::thread::spawn(move || tracker.mark_dispatched(100, Some("same-call")))
        })
        .collect();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(
        results.iter().filter(|result| result.is_ok()).count(),
        1,
        "exactly one dispatch may win: {results:?}"
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(ToolLifecycleError::Duplicate(_))))
            .count(),
        7,
        "every loser is an explicit duplicate refusal: {results:?}"
    );
    assert!(!tracker.is_fail_closed());
}

/// A canonical call's real committed output reference must keep its managed
/// output protected from cleanup, exactly like the legacy operation path.
#[test]
fn a_canonical_output_reference_protects_the_managed_output_from_cleanup() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    let outputs = dir.path().join(".mio/outputs");
    std::fs::create_dir_all(&outputs).unwrap();
    std::fs::write(outputs.join("canon.stdout"), b"canonical").unwrap();
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .saturating_sub(30 * 24 * 60 * 60);
    let lease = store.begin_capture().unwrap();
    let stored = store
        .register(
            &lease,
            OutputEntry {
                path: ".mio/outputs/canon.stdout".into(),
                bytes: None,
                hash: None,
                stream: "stdout".into(),
                created,
                call_id: Some("c1".into()),
            },
        )
        .unwrap();
    let reference = OutputReference::from_entry(&stored).unwrap();

    let journal = Journal::open(dir.path(), "gc-canonical");
    journal.begin_run().unwrap();
    journal
        .record_canonical_phase(&call("run-1", 1, ToolCallPhase::Executed, Some("c1")))
        .unwrap();
    journal
        .record_canonical_detail(
            "run-1",
            Some("c1"),
            "preview",
            std::slice::from_ref(&reference),
        )
        .unwrap();
    journal.finish_run(RunState::Completed).unwrap();

    let plan = store.plan_cleanup(SystemTime::now()).unwrap();
    assert!(plan.candidates.is_empty(), "{plan:?}");
    assert!(
        plan.retained_referenced
            .iter()
            .any(|path| path == ".mio/outputs/canon.stdout"),
        "{plan:?}"
    );
    assert!(outputs.join("canon.stdout").exists());
}

#[tokio::test]
async fn a_filter_denial_is_recorded_as_denied_before_dispatch() {
    let filter = ToolFilter::from_cli(None, Some("bash"), false).unwrap();
    let policy = ToolPolicy::new(
        filter,
        false,
        Arc::new(NullHost),
        PolicyContext::new("/work", "run-1"),
    );
    let error = policy
        .admit("bash", r#"{"command":"must not run"}"#)
        .await
        .unwrap_err();
    assert!(error.reason().contains("filter"), "{error}");
    let calls = policy.lifecycle().calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].phase, ToolCallPhase::Denied, "{calls:?}");
    assert_eq!(calls[0].tool, "shell", "{calls:?}");
    assert!(
        calls[0].call_id.is_none() && calls[0].effect_id.is_none(),
        "a pure filter denial never mints dispatch identity: {calls:?}"
    );
}
