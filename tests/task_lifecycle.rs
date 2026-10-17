//! 任务权威状态机（DES-04）的集成回归。
//!
//! 覆盖：转移全表、真实调度租约证据、准入前准备失败、排队取消零 admit、取消请求与
//! settled 分离、worker guard 延迟终态、并发终态不回退、回调读 snapshot 无死锁、
//! 持久化失败 fail-closed、legacy 投影映射、身份不可伪造、真实 Coordinator
//! 并发槽（`ActiveLease` 持有时第二请求不能获槽、`release` 后才可获槽），以及
//! SDK 接线：默认 run 返回权威任务记录、显式 `RunOptions::task` 绑定可读回真实状态。
//!
//! 这里直接使用 `lib.rs` 已声明的公共 `task_lifecycle` 模块：跟踪器是生产 lib 的同一份
//! 源码，不再是 `#[path]` 旁路编译。SDK 用例只使用本地 loopback HTTP 服务，不触互联网、
//! 不使用真实凭据；其余用例为纯内存 sink。

use mio_coding_agent::dedup::{TaskClaim, TaskKeyLedger, TaskKeyState, TaskLiveness};
use mio_coding_agent::journal::RunState;
use mio_coding_agent::subagent::Status as SubagentStatus;
use mio_coding_agent::{
    AgentEvent, Config, Host, HostFuture, RunOptions, RunTools, Services, ToolFilter, coordinator,
    journal, run_with_tools_tracked, task_lifecycle,
};
use serde_json::json;
use std::{
    io::Write,
    net::TcpListener,
    path::Path,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use task_lifecycle::{
    ActiveLease, CancelOutcome, EngineRunId, FinishOutcome, LeaseKind, MAX_DETAIL_CHARS,
    MAX_IDENTITY_CHARS, ProcessGroupReap, ProviderToolCallId, SchedulingPermit, Settlement, TaskId,
    TaskIdentity, TaskLifecycleError, TaskLifecycleSink, TaskLifecycleTracker, TaskPhase,
    TaskRecord,
};

#[path = "../src/test_http.rs"]
mod test_http;

// ---------------------------------------------------------------------------
// 测试替身
// ---------------------------------------------------------------------------

/// 记录每次持久化的 sink；可选地在回调里读取 tracker snapshot（证明无死锁）。
#[derive(Default)]
struct RecordingSink {
    records: Mutex<Vec<TaskRecord>>,
    fail: AtomicBool,
    snapshot_reader: Mutex<Option<Weak<TaskLifecycleTracker>>>,
    snapshot_phases: Mutex<Vec<Option<TaskPhase>>>,
}

impl TaskLifecycleSink for RecordingSink {
    fn on_task_lifecycle(&self, record: &TaskRecord) -> Result<(), String> {
        if self.fail.load(Ordering::SeqCst) {
            return Err("disk full".into());
        }
        // 回调在 tracker 的 state 锁之外执行：这里读 snapshot 必须不死锁。
        if let Some(tracker) = self
            .snapshot_reader
            .lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade)
        {
            self.snapshot_phases
                .lock()
                .unwrap()
                .push(tracker.snapshot(&record.task_id).map(|record| record.phase));
        }
        self.records.lock().unwrap().push(record.clone());
        Ok(())
    }
}

/// 外部实现的真实调度许可（证明 `SchedulingPermit` 可由未来 scheduler 实现）。
struct TestPermit {
    id: u64,
    kind: LeaseKind,
    holder: String,
    held: AtomicBool,
}

impl TestPermit {
    fn held(id: u64, kind: LeaseKind, holder: &str) -> Arc<Self> {
        Arc::new(Self {
            id,
            kind,
            holder: holder.into(),
            held: AtomicBool::new(true),
        })
    }

    fn release(&self) {
        self.held.store(false, Ordering::SeqCst);
    }
}

impl SchedulingPermit for TestPermit {
    fn lease_id(&self) -> u64 {
        self.id
    }
    fn kind(&self) -> LeaseKind {
        self.kind
    }
    fn holder(&self) -> &str {
        &self.holder
    }
    fn is_held(&self) -> bool {
        self.held.load(Ordering::SeqCst)
    }
}

/// 进程组回收证据替身。
#[derive(Default)]
struct Reap {
    reaped: AtomicBool,
}

impl Reap {
    fn mark(&self) {
        self.reaped.store(true, Ordering::SeqCst);
    }
}

impl ProcessGroupReap for Reap {
    fn is_reaped(&self) -> bool {
        self.reaped.load(Ordering::SeqCst)
    }
}

fn task(id: &str) -> TaskId {
    TaskId::new(id).expect("valid task id")
}

fn tracker_with_sink() -> (Arc<TaskLifecycleTracker>, Arc<RecordingSink>) {
    let sink = Arc::new(RecordingSink::default());
    let tracker = TaskLifecycleTracker::new("scope-1", sink.clone());
    (tracker, sink)
}

fn register(tracker: &TaskLifecycleTracker, id: &str) -> TaskId {
    let id = task(id);
    tracker
        .register(TaskIdentity::new(id.clone()), None)
        .expect("register");
    id
}

fn admit(tracker: &TaskLifecycleTracker, id: &TaskId) -> Arc<TestPermit> {
    let permit = TestPermit::held(1, LeaseKind::Exclusive, "scheduler");
    tracker.admit(id, &*permit).expect("admit");
    permit
}

fn settlement(phase: TaskPhase) -> Settlement<'static> {
    match phase {
        TaskPhase::Completed => Settlement::Completed,
        TaskPhase::Failed => Settlement::Failed,
        TaskPhase::Cancelled => Settlement::Cancelled,
        TaskPhase::Queued | TaskPhase::Running | TaskPhase::TimedOut => {
            panic!("not a terminal settlement: {phase}")
        }
    }
}

// ---------------------------------------------------------------------------
// 转移全表
// ---------------------------------------------------------------------------

#[test]
fn transition_table_is_exact() {
    use TaskPhase::*;
    let phases = [Queued, Running, Completed, Failed, Cancelled, TimedOut];
    for from in phases {
        for to in phases {
            let expected = from == to
                || matches!(
                    (from, to),
                    (Queued, Running | Failed | Cancelled)
                        | (Running, Completed | Failed | Cancelled | TimedOut)
                );
            assert_eq!(
                TaskPhase::transition_allowed(from, to),
                expected,
                "{from:?} -> {to:?}"
            );
        }
    }
    // 终态不回退、不换终态。
    for terminal in [Completed, Failed, Cancelled, TimedOut] {
        assert!(terminal.is_terminal());
        for other in [Queued, Running, Completed, Failed, Cancelled, TimedOut] {
            assert_eq!(
                TaskPhase::transition_allowed(terminal, other),
                other == terminal
            );
        }
    }
    // 排队任务没有进程组可销毁：排队超时不是合法终态。
    assert!(!TaskPhase::transition_allowed(Queued, TimedOut));
    // 准入前准备失败是合法终态。
    assert!(TaskPhase::transition_allowed(Queued, Failed));
}

// ---------------------------------------------------------------------------
// 真实调度租约证据
// ---------------------------------------------------------------------------

#[test]
fn running_requires_a_real_held_lease() {
    let (tracker, sink) = tracker_with_sink();
    let id = register(&tracker, "task-lease");

    let lease = TestPermit::held(7, LeaseKind::Exclusive, "scheduler-a");
    let record = tracker
        .admit(&id, &*lease)
        .expect("admit with a held lease");
    assert_eq!(record.phase, TaskPhase::Running);
    let evidence = record.admission.as_ref().expect("admission evidence");
    assert_eq!(evidence.lease_id, 7);
    assert_eq!(evidence.kind, LeaseKind::Exclusive);
    assert_eq!(evidence.holder, "scheduler-a");
    assert!(record.was_admitted());

    // 同一许可重复准入是幂等的；不同许可不能悄悄顶替。
    assert_eq!(
        tracker.admit(&id, &*lease).unwrap().phase,
        TaskPhase::Running
    );
    let other = TestPermit::held(8, LeaseKind::Shared, "scheduler-b");
    assert!(matches!(
        tracker.admit(&id, &*other),
        Err(TaskLifecycleError::Duplicate(_))
    ));

    // 真实记录中确实出现过 Queued -> Running。
    let phases: Vec<TaskPhase> = sink
        .records
        .lock()
        .unwrap()
        .iter()
        .map(|record| record.phase)
        .collect();
    assert_eq!(phases, vec![TaskPhase::Queued, TaskPhase::Running]);
}

#[test]
fn a_released_lease_cannot_admit() {
    let (tracker, _sink) = tracker_with_sink();
    let id = register(&tracker, "task-released");
    let lease = TestPermit::held(11, LeaseKind::Exclusive, "scheduler");
    lease.release();
    assert!(matches!(
        tracker.admit(&id, &*lease),
        Err(TaskLifecycleError::LeaseNotHeld { lease_id: 11 })
    ));
    assert_eq!(tracker.snapshot(&id).unwrap().phase, TaskPhase::Queued);
}

// ---------------------------------------------------------------------------
// 真实 Coordinator / 信号量槽位：ActiveLease 实际持有与释放资源
// ---------------------------------------------------------------------------

fn coordinator_with_agent_limit(agent: usize) -> coordinator::Coordinator {
    coordinator::Coordinator::new(coordinator::Limits {
        agent,
        ..coordinator::Limits::default()
    })
    .expect("valid coordinator limits")
}

#[tokio::test]
async fn active_lease_holds_a_real_coordinator_task_slot_until_released() {
    let coord = coordinator_with_agent_limit(1);
    let permit = coord
        .acquire_task(coordinator::TaskRequest::write("src/a.rs"))
        .await
        .expect("first task slot");
    let lease = ActiveLease::from_task_permit(permit);

    // 真实持有：写范围记为 Exclusive，底层 coordinator owner 仍在。
    assert!(lease.is_held());
    assert_eq!(lease.kind(), LeaseKind::Exclusive);
    assert_eq!(coord.snapshot().tasks.active, 1);

    // agent=1：lease 持有时，第二请求不能获槽。
    let blocked = tokio::time::timeout(
        Duration::from_millis(100),
        coord.acquire_task(coordinator::TaskRequest::read("src/b.rs")),
    )
    .await;
    assert!(
        blocked.is_err(),
        "the second request must not get a slot while the lease is held"
    );
    assert_eq!(coord.snapshot().tasks.active, 1);

    // 显式 release 真实 drop permit，槽位才释放。
    lease.release();
    assert!(!lease.is_held());
    let next = tokio::time::timeout(
        Duration::from_millis(500),
        coord.acquire_task(coordinator::TaskRequest::read("src/b.rs")),
    )
    .await
    .expect("the slot must be released after lease.release()")
    .expect("second task slot");
    assert!(!next.is_write());
    assert_eq!(coord.snapshot().tasks.active, 1);
}

#[tokio::test]
async fn a_coordinator_backed_lease_admits_then_releases_the_slot() {
    let (tracker, _sink) = tracker_with_sink();
    let id = register(&tracker, "task-coordinator-lease");

    let coord = coordinator_with_agent_limit(1);
    let permit = coord
        .acquire_task(coordinator::TaskRequest::write("src/x.rs"))
        .await
        .expect("task slot");
    let lease = ActiveLease::from_task_permit(permit);

    let record = tracker.admit(&id, lease.as_ref()).expect("admit");
    assert_eq!(record.phase, TaskPhase::Running);
    let evidence = record.admission.as_ref().expect("admission evidence");
    assert_eq!(evidence.kind, LeaseKind::Exclusive);
    assert!(
        evidence.holder.starts_with("coordinator-task:"),
        "audit holder must come from the real permit: {}",
        evidence.holder
    );
    assert!(evidence.lease_id > 0);

    // 释放后，同一个 lease 不再是准入证据。
    lease.release();
    assert!(!lease.is_held());
    assert!(matches!(
        tracker.admit(&id, lease.as_ref()),
        Err(TaskLifecycleError::LeaseNotHeld { .. })
    ));

    // 底层槽位被真实归还，后续请求可以获槽。
    let next = tokio::time::timeout(
        Duration::from_millis(500),
        coord.acquire_task(coordinator::TaskRequest::read("src/y.rs")),
    )
    .await
    .expect("the coordinator slot must be released")
    .expect("next task slot");
    assert!(!next.is_write());
}

#[tokio::test]
async fn active_lease_releases_real_model_and_heavy_slots() {
    let coord = coordinator::Coordinator::default();

    let model_lease = ActiveLease::from_model_permit(
        coord
            .acquire_model("provider-a", 10)
            .await
            .expect("model slot"),
    );
    assert!(model_lease.is_held());
    // 模型名额是真实现有的非路径槽位，不得伪装成工作区写独占。
    assert_eq!(model_lease.kind(), LeaseKind::Slot);
    assert_eq!(coord.snapshot().model.active_total, 1);
    model_lease.release();
    assert!(!model_lease.is_held());
    assert_eq!(coord.snapshot().model.active_total, 0);

    let heavy_lease =
        ActiveLease::from_heavy_permit(coord.acquire_heavy().await.expect("heavy slot"));
    assert!(heavy_lease.is_held());
    assert_eq!(coord.snapshot().heavy.active, 1);
    heavy_lease.release();
    assert!(!heavy_lease.is_held());
    assert_eq!(coord.snapshot().heavy.active, 0);
}

#[test]
fn active_lease_owns_and_frees_a_semaphore_slot() {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
    let permit = semaphore
        .clone()
        .try_acquire_owned()
        .expect("semaphore slot");
    assert_eq!(semaphore.available_permits(), 0);

    let lease = ActiveLease::from_semaphore_permit(permit);
    assert!(lease.is_held());
    assert_eq!(lease.kind(), LeaseKind::Slot);

    lease.release();
    assert!(!lease.is_held());
    assert_eq!(semaphore.available_permits(), 1);
}

#[test]
fn dropping_an_active_lease_releases_the_real_slot() {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
    let lease = ActiveLease::from_semaphore_permit(
        semaphore
            .clone()
            .try_acquire_owned()
            .expect("semaphore slot"),
    );
    assert_eq!(semaphore.available_permits(), 0);

    drop(lease);
    assert_eq!(semaphore.available_permits(), 1);
}

// ---------------------------------------------------------------------------
// 准入前准备失败
// ---------------------------------------------------------------------------

#[test]
fn queued_preparation_failure_is_a_real_failure_without_fake_running() {
    let (tracker, sink) = tracker_with_sink();
    let id = register(&tracker, "task-prep");

    let record = tracker
        .fail_before_admit(&id, "missing workspace dependency")
        .expect("prepare failure");
    assert_eq!(record.phase, TaskPhase::Failed);
    assert!(!record.was_admitted());
    assert!(record.admission.is_none());
    assert_eq!(
        record.detail.as_deref(),
        Some("missing workspace dependency")
    );

    // sink 从未观察到 Running：没有伪造的准入。
    assert!(
        !sink
            .records
            .lock()
            .unwrap()
            .iter()
            .any(|record| record.phase == TaskPhase::Running)
    );

    // 排队任务也不能直接跳到 Completed。
    let second = register(&tracker, "task-prep-2");
    assert!(matches!(
        tracker.finish(&second, Settlement::Completed, None),
        Err(TaskLifecycleError::Illegal {
            from: TaskPhase::Queued,
            to: TaskPhase::Completed
        })
    ));
}

// ---------------------------------------------------------------------------
// 排队取消零 admit
// ---------------------------------------------------------------------------

#[test]
fn queued_cancel_admits_nothing() {
    let (tracker, sink) = tracker_with_sink();
    let id = register(&tracker, "task-queued-cancel");

    assert_eq!(
        tracker.request_cancel(&id, Some("user abort")).unwrap(),
        CancelOutcome::Cancelled
    );
    let record = tracker.snapshot(&id).unwrap();
    assert_eq!(record.phase, TaskPhase::Cancelled);
    assert!(record.cancel_requested);
    assert!(!record.was_admitted());
    assert!(record.admission.is_none());

    // 取消后的排队任务不能凭空开始一个 worker。
    assert!(matches!(
        tracker.begin_worker(&id),
        Err(TaskLifecycleError::Illegal { .. })
    ));
    assert!(
        !sink
            .records
            .lock()
            .unwrap()
            .iter()
            .any(|record| record.phase == TaskPhase::Running)
    );
    // 幂等的第二次取消不会回退。
    assert_eq!(
        tracker.request_cancel(&id, None).unwrap(),
        CancelOutcome::AlreadySettled(TaskPhase::Cancelled)
    );
}

// ---------------------------------------------------------------------------
// 取消请求与 settled 分离
// ---------------------------------------------------------------------------

#[test]
fn cancel_request_is_separate_from_settlement() {
    let (tracker, _sink) = tracker_with_sink();
    let id = register(&tracker, "task-cancel-separate");
    let _permit = admit(&tracker, &id);

    let outcome = tracker.request_cancel(&id, None).unwrap();
    assert_eq!(outcome, CancelOutcome::Requested { workers: 0 });
    let record = tracker.snapshot(&id).unwrap();
    // 请求本身不提交终态。
    assert_eq!(record.phase, TaskPhase::Running);
    assert!(record.cancel_requested);

    // 取消请求之后，非取消终态被拒绝。
    assert!(matches!(
        tracker.finish(&id, Settlement::Completed, None),
        Err(TaskLifecycleError::CancelRequested {
            attempted: TaskPhase::Completed
        })
    ));
    // 显式 settle 才提交取消。
    assert_eq!(
        tracker.finish(&id, Settlement::Cancelled, None).unwrap(),
        FinishOutcome::Committed(TaskPhase::Cancelled)
    );
    assert_eq!(tracker.snapshot(&id).unwrap().phase, TaskPhase::Cancelled);
}

// ---------------------------------------------------------------------------
// worker guard 延迟终态
// ---------------------------------------------------------------------------

#[test]
fn worker_guards_defer_terminal_until_the_last_one_drops() {
    let (tracker, sink) = tracker_with_sink();
    let id = register(&tracker, "task-workers");
    let _permit = admit(&tracker, &id);

    let first = tracker.begin_worker(&id).expect("first worker");
    let second = tracker.begin_worker(&id).expect("second worker");
    assert_eq!(tracker.snapshot(&id).unwrap().workers, 2);

    assert_eq!(
        tracker.finish(&id, Settlement::Completed, None).unwrap(),
        FinishOutcome::Deferred {
            phase: TaskPhase::Completed,
            workers: 2
        }
    );
    let record = tracker.snapshot(&id).unwrap();
    assert_eq!(record.phase, TaskPhase::Running);
    assert_eq!(record.pending_terminal, Some(TaskPhase::Completed));

    drop(first);
    let record = tracker.snapshot(&id).unwrap();
    assert_eq!(record.phase, TaskPhase::Running);
    assert_eq!(record.workers, 1);

    drop(second);
    let record = tracker.snapshot(&id).unwrap();
    assert_eq!(record.phase, TaskPhase::Completed);
    assert_eq!(record.workers, 0);
    assert_eq!(record.pending_terminal, None);

    // 终态只提交一次：最后一次持久化就是 Completed。
    assert_eq!(
        sink.records
            .lock()
            .unwrap()
            .last()
            .map(|record| record.phase),
        Some(TaskPhase::Completed)
    );
}

#[test]
fn a_foreign_lease_release_never_finishes_a_task() {
    let (tracker, _sink) = tracker_with_sink();
    let id = register(&tracker, "task-lease-drop");
    let _permit = admit(&tracker, &id);

    let first = tracker.begin_worker(&id).unwrap();
    let second = tracker.begin_worker(&id).unwrap();
    assert!(matches!(
        tracker.finish(&id, Settlement::Failed, None).unwrap(),
        FinishOutcome::Deferred { .. }
    ));

    // 释放一个无关的**真实**信号量许可（不是 worker guard）绝不结束回合。
    let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
    let permit = semaphore.clone().try_acquire_owned().expect("slot");
    let lease = ActiveLease::from_semaphore_permit(permit);
    assert!(lease.is_held());
    lease.release();
    assert!(!lease.is_held());
    assert_eq!(semaphore.available_permits(), 1);
    drop(lease);
    assert_eq!(tracker.snapshot(&id).unwrap().phase, TaskPhase::Running);

    first.release();
    assert_eq!(tracker.snapshot(&id).unwrap().phase, TaskPhase::Running);
    drop(second);
    assert_eq!(tracker.snapshot(&id).unwrap().phase, TaskPhase::Failed);
}

// ---------------------------------------------------------------------------
// 回调读 snapshot 无死锁
// ---------------------------------------------------------------------------

#[test]
fn sink_can_read_a_snapshot_without_deadlocking() {
    let sink = Arc::new(RecordingSink::default());
    let tracker = TaskLifecycleTracker::new("scope-callback", sink.clone());
    *sink.snapshot_reader.lock().unwrap() = Some(Arc::downgrade(&tracker));

    let id = register(&tracker, "task-callback");
    let _permit = admit(&tracker, &id);

    // 回调在 state 锁外执行，因此能读到已经提交的 Queued 快照而不死锁。
    let seen = sink.snapshot_phases.lock().unwrap().clone();
    assert!(
        seen.contains(&Some(TaskPhase::Queued)),
        "the sink callback must observe the committed pre-admit snapshot: {seen:?}"
    );
}

// ---------------------------------------------------------------------------
// 持久化失败 fail-closed
// ---------------------------------------------------------------------------

#[test]
fn a_persist_failure_latches_fail_closed() {
    let (tracker, sink) = tracker_with_sink();
    let id = register(&tracker, "task-failclosed");
    let _permit = admit(&tracker, &id);
    sink.fail.store(true, Ordering::SeqCst);

    let error = tracker
        .finish(&id, Settlement::Completed, None)
        .expect_err("persist must fail");
    assert!(matches!(error, TaskLifecycleError::Persist(_)));
    assert!(tracker.is_fail_closed());

    // 绝不假装已经结束：记录仍是 Running。
    assert_eq!(tracker.snapshot(&id).unwrap().phase, TaskPhase::Running);
    assert!(matches!(
        tracker.finish(&id, Settlement::Completed, None),
        Err(TaskLifecycleError::FailClosed(_))
    ));
    assert!(matches!(
        tracker.begin_worker(&id),
        Err(TaskLifecycleError::FailClosed(_))
    ));
    let new_id = TaskId::new("task-after-latch").unwrap();
    assert!(matches!(
        tracker.register(TaskIdentity::new(new_id), None),
        Err(TaskLifecycleError::FailClosed(_))
    ));
}

// ---------------------------------------------------------------------------
// 并发终态不回退
// ---------------------------------------------------------------------------

#[test]
fn concurrent_terminal_requests_never_regress() {
    for _ in 0..32 {
        let (tracker, _sink) = tracker_with_sink();
        let id = register(&tracker, "task-race");
        let _permit = admit(&tracker, &id);

        let (first, second) = std::thread::scope(|scope| {
            let first = scope.spawn(|| tracker.finish(&id, Settlement::Completed, None));
            let second = scope.spawn(|| tracker.finish(&id, Settlement::Failed, None));
            (first.join().unwrap(), second.join().unwrap())
        });
        let committed = [first.is_ok(), second.is_ok()]
            .into_iter()
            .filter(|ok| *ok)
            .count();
        assert_eq!(
            committed, 1,
            "exactly one terminal commits: {first:?} {second:?}"
        );

        let phase = tracker.snapshot(&id).unwrap().phase;
        assert!(matches!(phase, TaskPhase::Completed | TaskPhase::Failed));
        // 已提交的终态永不回退或改换。
        let other = if phase == TaskPhase::Completed {
            TaskPhase::Failed
        } else {
            TaskPhase::Completed
        };
        assert!(tracker.finish(&id, settlement(other), None).is_err());
        assert_eq!(tracker.snapshot(&id).unwrap().phase, phase);
    }
}

#[test]
fn distinct_tasks_submit_in_parallel() {
    let (tracker, _sink) = tracker_with_sink();
    std::thread::scope(|scope| {
        for index in 0..8u64 {
            let tracker = &tracker;
            scope.spawn(move || {
                let id = task(&format!("task-parallel-{index}"));
                tracker
                    .register(TaskIdentity::new(id.clone()), None)
                    .unwrap();
                let permit = TestPermit::held(index + 1, LeaseKind::Exclusive, "scheduler");
                tracker.admit(&id, &*permit).unwrap();
                let guard = tracker.begin_worker(&id).unwrap();
                assert_eq!(
                    tracker.finish(&id, Settlement::Completed, None).unwrap(),
                    FinishOutcome::Deferred {
                        phase: TaskPhase::Completed,
                        workers: 1
                    }
                );
                guard.release();
                assert_eq!(tracker.snapshot(&id).unwrap().phase, TaskPhase::Completed);
            });
        }
    });
    assert_eq!(tracker.len(), 8);
}

// ---------------------------------------------------------------------------
// legacy 投影映射
// ---------------------------------------------------------------------------

#[test]
fn legacy_state_mappings_are_exact() {
    for (status, phase) in [
        (SubagentStatus::Queued, TaskPhase::Queued),
        (SubagentStatus::Running, TaskPhase::Running),
        (SubagentStatus::Completed, TaskPhase::Completed),
        (SubagentStatus::Failed, TaskPhase::Failed),
        (SubagentStatus::Cancelled, TaskPhase::Cancelled),
        (SubagentStatus::TimedOut, TaskPhase::TimedOut),
    ] {
        assert_eq!(TaskPhase::from(status), phase);
        assert_eq!(SubagentStatus::from(phase), status);
        assert_eq!(TaskPhase::from_subagent_status(status), phase);
        assert_eq!(phase.to_subagent_status(), status);
    }

    assert_eq!(
        TaskPhase::from_task_key_state(TaskKeyState::Active),
        Some(TaskPhase::Running)
    );
    assert_eq!(
        TaskPhase::Running.to_task_key_state(),
        Some(TaskKeyState::Active)
    );
    // 旧账本没有 TimedOut；不谎报成 Failed。
    assert_eq!(TaskPhase::TimedOut.to_task_key_state(), None);
    // Interrupted 是恢复观测，不是阶段。
    assert_eq!(
        TaskPhase::from_task_key_state(TaskKeyState::Interrupted),
        None
    );

    assert_eq!(
        TaskPhase::from_run_state(RunState::Active),
        Some(TaskPhase::Running)
    );
    assert_eq!(
        TaskPhase::Completed.to_run_state(),
        Some(RunState::Completed)
    );
    assert_eq!(TaskPhase::Queued.to_run_state(), None);
    assert_eq!(TaskPhase::TimedOut.to_run_state(), None);
    assert_eq!(TaskPhase::from_run_state(RunState::Interrupted), None);

    // Interrupted / liveness 观测绝不改写权威阶段。
    let (tracker, _sink) = tracker_with_sink();
    let id = register(&tracker, "task-observe");
    let _permit = admit(&tracker, &id);
    let view = tracker
        .view(&id, TaskLiveness::Unknown, true)
        .expect("view");
    assert_eq!(view.phase(), TaskPhase::Running);
    assert!(view.interrupted);
    assert_eq!(view.liveness, TaskLiveness::Unknown);
    assert_eq!(tracker.snapshot(&id).unwrap().phase, TaskPhase::Running);
}

// ---------------------------------------------------------------------------
// 身份不可伪造
// ---------------------------------------------------------------------------

#[test]
fn identities_are_never_fabricated() {
    assert!(TaskId::new("").is_err());
    assert!(TaskId::new("   ").is_err());
    assert!(TaskId::new("bad\nid").is_err());
    assert!(TaskId::new("x".repeat(MAX_IDENTITY_CHARS + 1)).is_err());
    assert!(TaskId::new("x".repeat(MAX_IDENTITY_CHARS)).is_ok());

    let (tracker, _sink) = tracker_with_sink();
    let id = register(&tracker, "task-ident");
    let record = tracker.snapshot(&id).unwrap();
    assert!(record.engine_run_id.is_none());
    assert!(record.provider_tool_call_id.is_none());

    let run = EngineRunId::new("run-uuid-1").unwrap();
    let updated = tracker.attach_engine_run(&id, run.clone()).unwrap();
    assert_eq!(updated.engine_run_id.as_ref(), Some(&run));
    // set-once：不同的真实 ID 被拒绝，相同的 ID 幂等。
    assert!(matches!(
        tracker.attach_engine_run(&id, EngineRunId::new("run-uuid-2").unwrap()),
        Err(TaskLifecycleError::Duplicate(_))
    ));
    assert!(tracker.attach_engine_run(&id, run).is_ok());

    let call = ProviderToolCallId::new("call-1").unwrap();
    let updated = tracker
        .attach_provider_tool_call(&id, call.clone())
        .unwrap();
    assert_eq!(updated.provider_tool_call_id.as_ref(), Some(&call));
    // 两个层级保持独立字段，绝不互相顶替。
    assert_ne!(
        updated.engine_run_id.as_ref().map(|run| run.as_str()),
        updated
            .provider_tool_call_id
            .as_ref()
            .map(|call| call.as_str())
    );

    // 登记时可选携带真实 engine run；未知字段保持 None。
    let second = TaskId::new("task-ident-2").unwrap();
    let identity = TaskIdentity::new(second.clone())
        .with_engine_run(EngineRunId::new("run-uuid-3").unwrap())
        .with_provider_tool_call(ProviderToolCallId::new("call-3").unwrap());
    let record = tracker.register(identity, None).unwrap();
    assert!(record.engine_run_id.is_some());
    assert!(record.provider_tool_call_id.is_some());

    let third = TaskId::new("task-ident-3").unwrap();
    let record = tracker.register(TaskIdentity::new(third), None).unwrap();
    assert!(record.engine_run_id.is_none());
    assert!(record.provider_tool_call_id.is_none());
}

// ---------------------------------------------------------------------------
// 有界容量
// ---------------------------------------------------------------------------

#[test]
fn capacity_is_bounded_and_never_evicts() {
    let sink = Arc::new(RecordingSink::default());
    let tracker = TaskLifecycleTracker::with_capacity("scope-cap", sink, 2);
    register(&tracker, "task-1");
    register(&tracker, "task-2");
    let third = TaskId::new("task-3").unwrap();
    assert!(matches!(
        tracker.register(TaskIdentity::new(third), None),
        Err(TaskLifecycleError::Full)
    ));
    assert_eq!(tracker.len(), 2);
    assert!(tracker.snapshot(&task("task-1")).is_some());
    assert!(tracker.snapshot(&task("task-2")).is_some());
}

// ---------------------------------------------------------------------------
// 超时证据
// ---------------------------------------------------------------------------

#[test]
fn timeout_requires_a_reaped_process_group() {
    let (tracker, _sink) = tracker_with_sink();
    let id = register(&tracker, "task-timeout");
    let _permit = admit(&tracker, &id);

    let reap = Reap::default();
    assert!(matches!(
        tracker.finish(
            &id,
            Settlement::TimedOut {
                reap: &reap as &dyn ProcessGroupReap
            },
            None
        ),
        Err(TaskLifecycleError::ProcessGroupAlive(_))
    ));
    assert_eq!(tracker.snapshot(&id).unwrap().phase, TaskPhase::Running);

    reap.mark();
    assert_eq!(
        tracker
            .finish(
                &id,
                Settlement::TimedOut {
                    reap: &reap as &dyn ProcessGroupReap
                },
                None
            )
            .unwrap(),
        FinishOutcome::Committed(TaskPhase::TimedOut)
    );

    // 排队任务没有进程组可销毁：排队超时被拒绝。
    let queued = register(&tracker, "task-queued-timeout");
    reap.mark();
    assert!(matches!(
        tracker.finish(
            &queued,
            Settlement::TimedOut {
                reap: &reap as &dyn ProcessGroupReap
            },
            None
        ),
        Err(TaskLifecycleError::Illegal {
            from: TaskPhase::Queued,
            to: TaskPhase::TimedOut
        })
    ));
    assert_eq!(tracker.snapshot(&queued).unwrap().phase, TaskPhase::Queued);
}

// ---------------------------------------------------------------------------
// 有界 detail
// ---------------------------------------------------------------------------

#[test]
fn detail_is_bounded() {
    let (tracker, _sink) = tracker_with_sink();
    let id = register(&tracker, "task-detail");
    let _permit = admit(&tracker, &id);
    let long = "x".repeat(MAX_DETAIL_CHARS + 50);
    tracker
        .finish(&id, Settlement::Failed, Some(&long))
        .unwrap();
    let detail = tracker.snapshot(&id).unwrap().detail.unwrap();
    assert!(detail.ends_with("..."));
    assert!(detail.chars().count() <= MAX_DETAIL_CHARS + 3);
}

// ---------------------------------------------------------------------------
// TaskHandle / task-local / settled 等待
// ---------------------------------------------------------------------------

#[tokio::test]
async fn task_local_handle_counts_real_workers_and_defers_terminal() {
    let (tracker, _sink) = tracker_with_sink();
    let id = register(&tracker, "task-handle");
    let _permit = admit(&tracker, &id);
    let handle = task_lifecycle::TaskHandle::new(tracker.clone(), id.clone());
    assert_eq!(handle.task_id(), &id);
    assert!(handle.snapshot().is_some());

    let guard = task_lifecycle::scope_task(handle.clone(), async {
        let current = task_lifecycle::current_task_handle().expect("scoped handle");
        assert_eq!(current.task_id(), &id);
        current.begin_worker().expect("worker within scope")
    })
    .await;
    assert_eq!(tracker.snapshot(&id).unwrap().workers, 1);

    // 真实绑定只在 scope 内有效；直接调用时没有句柄可冒充。
    assert!(task_lifecycle::current_task_handle().is_none());

    assert!(matches!(
        handle.finish(Settlement::Completed, None).unwrap(),
        FinishOutcome::Deferred { workers: 1, .. }
    ));
    // 单个 lease/guard 未回收前不得提前终态。
    assert_eq!(tracker.snapshot(&id).unwrap().phase, TaskPhase::Running);
    guard.release();
    assert_eq!(tracker.snapshot(&id).unwrap().phase, TaskPhase::Completed);
    // wait_settled 返回已提交的真实终态。
    let settled = handle.wait_settled().await.expect("settled");
    assert_eq!(settled.phase, TaskPhase::Completed);
}

#[tokio::test]
async fn cancel_settles_only_after_the_last_real_worker() {
    let (tracker, _sink) = tracker_with_sink();
    let id = register(&tracker, "task-cancel-workers");
    let _permit = admit(&tracker, &id);
    let handle = task_lifecycle::TaskHandle::new(tracker.clone(), id.clone());
    let guard = handle.begin_worker().expect("worker");

    assert_eq!(
        handle.request_cancel(Some("user abort")).unwrap(),
        CancelOutcome::Requested { workers: 1 }
    );
    assert!(matches!(
        handle.finish(Settlement::Cancelled, None).unwrap(),
        FinishOutcome::Deferred { workers: 1, .. }
    ));
    // 取消请求本身不构成终态。
    assert_eq!(tracker.snapshot(&id).unwrap().phase, TaskPhase::Running);
    guard.release();
    assert_eq!(tracker.snapshot(&id).unwrap().phase, TaskPhase::Cancelled);
    let settled = handle.wait_settled().await.expect("settled");
    assert_eq!(settled.phase, TaskPhase::Cancelled);
}

#[tokio::test]
async fn active_lease_settles_real_model_usage_without_leaking_the_slot() {
    let coord = coordinator::Coordinator::default();
    let permit = coord
        .acquire_model("provider-a", 10)
        .await
        .expect("model slot");
    let lease = ActiveLease::from_model_permit(permit);
    assert_eq!(coord.snapshot().model.active_total, 1);

    // 真实 usage 结算：permit 被取出并 complete，槽位与账本一致。
    lease.complete_model_usage(Some(3));
    assert!(!lease.is_held());
    assert_eq!(coord.snapshot().model.active_total, 0);
    assert_eq!(coord.snapshot().budget.tokens_used, 3);

    // 非模型许可不会被 complete_model_usage 误释放。
    let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
    let other =
        ActiveLease::from_semaphore_permit(semaphore.clone().try_acquire_owned().expect("slot"));
    other.complete_model_usage(Some(1));
    assert!(other.is_held());
    assert_eq!(semaphore.available_permits(), 0);
}

// ---------------------------------------------------------------------------
// legacy 投影：dedup / journal
// ---------------------------------------------------------------------------

#[test]
fn dedup_record_canonical_projects_without_touching_legacy_state() {
    let ledger = TaskKeyLedger::ephemeral();
    let TaskClaim::Acquired { task_id } = ledger.claim("key-1").unwrap() else {
        panic!("fresh key must be acquired");
    };
    ledger.record_canonical(&task_id, "queued", None).unwrap();
    ledger
        .record_canonical(&task_id, "running", Some("engine-run-1"))
        .unwrap();

    let record = ledger.lookup("key-1").unwrap().unwrap();
    assert_eq!(record.canonical_phase.as_deref(), Some("running"));
    assert_eq!(record.engine_run_id.as_deref(), Some("engine-run-1"));
    // The projection never rewrites the legacy state: only `begin`/`finish` do.
    assert_eq!(record.state, TaskKeyState::Queued);
    // set-once: a different real engine run id is refused, and so is a bogus phase.
    assert!(
        ledger
            .record_canonical(&task_id, "running", Some("engine-run-2"))
            .is_err()
    );
    assert!(ledger.record_canonical(&task_id, "bogus", None).is_err());
    assert_eq!(
        ledger
            .lookup("key-1")
            .unwrap()
            .unwrap()
            .engine_run_id
            .as_deref(),
        Some("engine-run-1")
    );
}

#[test]
fn legacy_dedup_records_keep_identity_unknown() {
    let record: mio_coding_agent::dedup::TaskKeyRecord = serde_json::from_value(json!({
        "task_id": "t1",
        "state": "interrupted",
        "key": "k",
        "updated": 0
    }))
    .unwrap();
    // Old records have no canonical fields and are never given a fabricated one.
    assert!(record.canonical_phase.is_none());
    assert!(record.engine_run_id.is_none());
    // `Interrupted` stays a recovery observation, not a canonical terminal.
    assert_eq!(
        TaskPhase::from_task_key_state(TaskKeyState::Interrupted),
        None
    );
}

#[test]
fn journal_keeps_the_real_engine_run_id_separate_from_its_legacy_run_id() {
    let dir = tempfile::tempdir().unwrap();
    let journal = journal::Journal::open(dir.path(), "s1");
    journal
        .begin_run_with_task(Some("goal"), Some("task-1"))
        .unwrap();
    journal
        .attach_run_identity("engine-run-1", Some("task-1"))
        .unwrap();
    // set-once: the same id is idempotent, a different one is refused.
    assert!(
        journal
            .attach_run_identity("engine-run-1", Some("task-1"))
            .is_ok()
    );
    assert!(journal.attach_run_identity("engine-run-2", None).is_err());
    journal.set_canonical_phase("running").unwrap();
    journal.set_canonical_phase("completed").unwrap();
    journal.finish_run(RunState::Completed).unwrap();

    let path = journal::journal_path(dir.path(), "s1");
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let run = &value["runs"][0];
    assert_eq!(run["task_id"], "task-1");
    assert_eq!(run["engine_run_id"], "engine-run-1");
    // The phase is the real projection's; `finish_run` must not rewrite it.
    assert_eq!(run["canonical_phase"], "completed");
    // The journal-local legacy id is never the engine UUID.
    assert_ne!(run["run_id"].as_str().unwrap(), "engine-run-1");
}

#[test]
fn legacy_finish_cannot_fabricate_a_canonical_phase() {
    let dir = tempfile::tempdir().unwrap();
    let journal = journal::Journal::open(dir.path(), "s-legacy");
    journal
        .begin_run_with_task(Some("goal"), Some("task-legacy"))
        .unwrap();
    // No real tracker projection happened. The legacy finish must record only
    // its own legacy state and never invent a canonical phase from it.
    journal.finish_run(RunState::Completed).unwrap();

    let path = journal::journal_path(dir.path(), "s-legacy");
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let run = &value["runs"][0];
    assert_eq!(run["state"], "completed");
    assert_eq!(run["task_id"], "task-legacy");
    assert!(
        run.get("canonical_phase").is_none(),
        "a legacy finish must not fabricate a canonical phase: {run}"
    );
}

// ---------------------------------------------------------------------------
// SDK 接线：默认 run 也有权威阶段；显式绑定可返回真实状态
// ---------------------------------------------------------------------------

/// 记录任务阶段与真实身份的 SDK 宿主；`shell` 不会被这次纯模型运行触发。
#[derive(Default)]
struct SdkTrackingHost {
    phases: Mutex<Vec<TaskPhase>>,
    identities: Mutex<Vec<(String, Option<String>)>>,
}

impl Host for SdkTrackingHost {
    fn shell(&self, _command: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Err("no shell in the SDK lifecycle test".into()) })
    }

    fn on_event(&self, _event: AgentEvent) {}

    /// Host-preparation MCP startup asks the host to approve the server. This
    /// fixture always approves so the real start can be observed; it never
    /// persists trust.
    fn approve_tool_async(
        &self,
        _operation: &str,
        _target: &str,
    ) -> HostFuture<Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn on_task_lifecycle(&self, record: &TaskRecord) -> Result<(), String> {
        self.phases.lock().unwrap().push(record.phase);
        self.identities.lock().unwrap().push((
            record.task_id.as_str().to_owned(),
            record
                .engine_run_id
                .as_ref()
                .map(|run| run.as_str().to_owned()),
        ));
        Ok(())
    }
}

/// 一个只回一次模型补全的本地 loopback 服务；不触网、不使用真实凭据。
fn spawn_completion_server(turns: usize) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        for _ in 0..turns {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let (header, _request) = test_http::read_json(&mut socket);
            assert!(header.starts_with("POST /v1/chat/completions "));
            let chunk = json!({
                "id": "t",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "local",
                "choices": [{
                    "index": 0,
                    "delta": {"role": "assistant", "content": "ok"},
                    "finish_reason": "stop"
                }]
            })
            .to_string();
            let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    (format!("http://{address}/v1"), server)
}

fn sdk_config(base_url: String) -> Config {
    Config {
        provider: mio_coding_agent::Provider::Custom,
        base_url,
        model: "local".into(),
        api_key: "test-only".into(),
    }
}

#[tokio::test]
async fn sdk_default_run_records_a_real_task_without_a_scheduler() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let (base_url, server) = spawn_completion_server(1);
    let host = Arc::new(SdkTrackingHost::default());
    let (history, record) = mio_coding_agent::run_with_options_tracked(
        sdk_config(base_url),
        Vec::new(),
        "你好",
        &cwd,
        host.clone(),
        Services::default(),
        RunOptions::default(),
    )
    .await
    .unwrap();
    server.join().unwrap();
    assert!(!history.is_empty());
    // The default SDK run returns its authoritative task record, already settled.
    assert_eq!(record.phase, TaskPhase::Completed);
    assert!(
        record.was_admitted(),
        "Running required a real engine lease"
    );
    assert!(record.engine_run_id.is_some(), "engine UUID must attach");

    let phases = host.phases.lock().unwrap().clone();
    assert_eq!(
        phases.first(),
        Some(&TaskPhase::Queued),
        "phases={phases:?}"
    );
    assert!(phases.contains(&TaskPhase::Running), "phases={phases:?}");
    assert_eq!(
        phases.last(),
        Some(&TaskPhase::Completed),
        "phases={phases:?}"
    );
    // 规范任务 id、真实 engine run UUID 都必须存在，且两者独立。
    let identities = host.identities.lock().unwrap().clone();
    let task_id = identities
        .iter()
        .find_map(|(id, _)| (!id.is_empty()).then(|| id.clone()))
        .expect("a real canonical task id");
    let engine_run = identities
        .iter()
        .find_map(|(_, run)| run.clone())
        .expect("a real engine run UUID");
    assert_ne!(task_id, engine_run);
    assert!(
        uuid::Uuid::parse_str(&task_id).is_ok(),
        "task id must be a real uuid: {task_id}"
    );
    assert!(
        uuid::Uuid::parse_str(&engine_run).is_ok(),
        "engine run must be a real uuid: {engine_run}"
    );
}

#[tokio::test]
async fn sdk_explicit_task_binding_returns_real_state() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let (base_url, server) = spawn_completion_server(1);
    let (tracker, sink) = tracker_with_sink();
    let id = register(&tracker, "explicit-sdk");
    let handle = task_lifecycle::TaskHandle::new(tracker.clone(), id.clone());
    let options = RunOptions {
        task: Some(handle.clone()),
        ..RunOptions::default()
    };
    let (result, returned) = mio_coding_agent::run_with_options_tracked(
        sdk_config(base_url),
        Vec::new(),
        "绑定任务",
        &cwd,
        Arc::new(SdkTrackingHost::default()),
        Services::default(),
        options,
    )
    .await
    .unwrap();
    assert_eq!(returned.task_id, id);
    assert_eq!(returned.phase, TaskPhase::Completed);
    server.join().unwrap();
    assert!(!result.is_empty());

    let record = handle.snapshot().expect("the bound task is tracked");
    assert_eq!(record.phase, TaskPhase::Completed);
    assert!(
        record.was_admitted(),
        "Running required a real engine lease"
    );
    assert!(record.engine_run_id.is_some(), "engine UUID must attach");
    assert!(!tracker.is_fail_closed());
    let phases: Vec<_> = sink
        .records
        .lock()
        .unwrap()
        .iter()
        .map(|record| record.phase)
        .collect();
    assert_eq!(
        phases.first(),
        Some(&TaskPhase::Queued),
        "phases={phases:?}"
    );
    assert!(phases.contains(&TaskPhase::Running), "phases={phases:?}");
    assert_eq!(
        phases.last(),
        Some(&TaskPhase::Completed),
        "phases={phases:?}"
    );
}

// ---------------------------------------------------------------------------
// ROOT-REVIEW 10/11/13：真实 worker guard、host-preparation 边界与 settled 等待
// ---------------------------------------------------------------------------

/// 一个本地 loopback 服务：首个请求返回一次工具调用，第二个请求返回最终答复。
/// 不触网、不使用真实凭据；工具参数为空对象。
fn spawn_tool_then_final_server(tool_name: &str) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let tool_name = tool_name.to_owned();
    let server = std::thread::spawn(move || {
        for turn in 0..2 {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let (header, _body) = test_http::read_json(&mut socket);
            assert!(header.starts_with("POST /v1/chat/completions "));
            let body = if turn == 0 {
                let tool_calls = json!([{
                    "index": 0,
                    "id": "call-detached",
                    "type": "function",
                    "function": {"name": tool_name, "arguments": "{}"}
                }]);
                let delta = json!({
                    "id": "t",
                    "object": "chat.completion.chunk",
                    "created": 0,
                    "model": "local",
                    "choices": [{
                        "index": 0,
                        "delta": {"role": "assistant", "tool_calls": tool_calls},
                        "finish_reason": null
                    }]
                })
                .to_string();
                let finish = json!({
                    "id": "t",
                    "object": "chat.completion.chunk",
                    "created": 0,
                    "model": "local",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
                })
                .to_string();
                format!("data: {delta}\n\ndata: {finish}\n\ndata: [DONE]\n\n")
            } else {
                let chunk = json!({
                    "id": "t",
                    "object": "chat.completion.chunk",
                    "created": 0,
                    "model": "local",
                    "choices": [{
                        "index": 0,
                        "delta": {"role": "assistant", "content": "done"},
                        "finish_reason": "stop"
                    }]
                })
                .to_string();
                format!("data: {chunk}\n\ndata: [DONE]\n\n")
            };
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    (format!("http://{address}/v1"), server)
}

/// ROOT-REVIEW 13：引擎拥有的任务在返回前必须等到真正的 settled。工具在一次模型
/// 回合中启动一个有**真实 `TaskWorkerGuard` 加上真实监督 shell/reaper** 的分离
/// worker 后立即返回；运行结束提交 `Completed` 时该 worker 仍持有 guard，终态因此
/// 是 `Deferred`。tracked API 必须等真实回收后才返回，记录不能仍是 `Running`。
#[tokio::test]
async fn engine_owned_tracked_run_awaits_a_real_deferred_worker() {
    use mio_agent::tool::DynamicTool;
    use mio_core::message::ToolName;
    use mio_core::tool::{ToolExecutionError, ToolOutput};

    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let marker = cwd.join("detached-worker.marker");
    let (base_url, server) = spawn_tool_then_final_server("detached_shell");
    let tool_cwd = cwd.clone();
    let tool_marker = marker.clone();
    let tool = DynamicTool::new(
        ToolName::new("detached_shell").expect("tool name"),
        "starts a detached real supervised shell worker",
        json!({"type": "object", "properties": {}}),
        move |_arguments| {
            let cwd = tool_cwd.clone();
            let marker = tool_marker.clone();
            Box::pin(async move {
                let handle = task_lifecycle::current_task_handle().ok_or_else(|| {
                    ToolExecutionError::other("the run must be bound to a canonical task")
                })?;
                // Hold a real worker guard in the detached task so the run
                // cannot settle before the real shell process is reaped.
                let guard = handle
                    .begin_worker()
                    .map_err(|error| ToolExecutionError::other(format!("worker: {error}")))?;
                tokio::spawn(async move {
                    let command = format!("touch {} && sleep 2", marker.display());
                    let _ = task_lifecycle::scope_task(
                        handle,
                        mio_coding_agent::shell::execute(&command, &cwd),
                    )
                    .await;
                    drop(guard);
                });
                Ok::<_, ToolExecutionError>(ToolOutput::text("detached"))
            })
        },
    );
    let host = Arc::new(SdkTrackingHost::default());
    let (history, record) = run_with_tools_tracked(
        sdk_config(base_url),
        Vec::new(),
        "detach a worker",
        &cwd,
        host,
        Services::default(),
        RunTools {
            extra_tools: vec![tool],
            ..RunTools::default()
        },
    )
    .await
    .unwrap();
    server.join().unwrap();
    assert!(!history.is_empty());
    // The final record must be a committed terminal with every real worker
    // reclaimed, not a still-`Running` snapshot of a deferred finish.
    assert_eq!(record.phase, TaskPhase::Completed, "{record:?}");
    assert_eq!(record.workers, 0, "{record:?}");
    assert!(record.was_admitted());
    assert!(
        marker.exists(),
        "the real shell worker must have executed before the run returned"
    );
}

/// ROOT-REVIEW 10：绑定任务尚未真实准入（`Queued`）、已取消（终态）或准入前准备
/// 失败（`Failed`）时，shell 必须在 spawn 之前拒绝，不能吞掉 `begin_worker` 的
/// 错误后照样起进程。断言零副作用且 worker 计数保持为 0。
#[tokio::test]
async fn a_task_that_did_not_admit_never_spawns_a_shell_worker() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let marker = cwd.join("must-not-exist");
    let command = format!("touch {}", marker.display());

    // Queued：尚无真实许可，必须拒绝。
    let (tracker, _sink) = tracker_with_sink();
    let id = register(&tracker, "queued-no-spawn");
    let handle = task_lifecycle::TaskHandle::new(tracker.clone(), id.clone());
    let refused = task_lifecycle::scope_task(
        handle.clone(),
        mio_coding_agent::shell::execute(&command, &cwd),
    )
    .await;
    assert!(refused.is_err(), "{refused:?}");
    assert!(!marker.exists(), "a queued task must not spawn a shell");
    let record = handle.snapshot().unwrap();
    assert_eq!(record.phase, TaskPhase::Queued);
    assert_eq!(record.workers, 0);
    assert!(!record.was_admitted());

    // 排队取消后是终态 `Cancelled`：Shell 仍必须拒绝且不吞掉 `Illegal`。
    assert_eq!(
        handle.request_cancel(None).unwrap(),
        CancelOutcome::Cancelled
    );
    let refused = task_lifecycle::scope_task(
        handle.clone(),
        mio_coding_agent::shell::execute(&command, &cwd),
    )
    .await;
    assert!(refused.is_err(), "{refused:?}");
    assert!(!marker.exists(), "a cancelled task must not spawn a shell");

    // 准入前准备失败 `Failed`：同样拒绝。
    let (tracker_failed, _sink_failed) = tracker_with_sink();
    let failed_id = register(&tracker_failed, "failed-no-spawn");
    let failed_handle = task_lifecycle::TaskHandle::new(tracker_failed.clone(), failed_id.clone());
    tracker_failed
        .fail_before_admit(&failed_id, "workspace dependency missing")
        .unwrap();
    let refused = task_lifecycle::scope_task(
        failed_handle.clone(),
        mio_coding_agent::shell::execute(&command, &cwd),
    )
    .await;
    assert!(refused.is_err(), "{refused:?}");
    assert!(!marker.exists(), "a failed task must not spawn a shell");
    assert_eq!(failed_handle.snapshot().unwrap().workers, 0);
}

/// ROOT-REVIEW 11：CLI 在 `Queued` 阶段加载项目 MCP/插件（host preparation）。
/// 真实启动一个本地 stdio MCP helper 时，它只能是 host-preparation 资源：不得把
/// 任务伪造成 `Running`，不得计入引擎 worker；在仍 `Queued` 时取消后，helper 进程
/// 必须被回收。默认 MCP 路径（非 `--no-mcp`）也覆盖。
#[cfg(unix)]
#[tokio::test]
async fn queued_host_preparation_starts_mcp_without_faking_a_worker() {
    use mio_coding_agent::mcp::{load_policy_bound_tools, shutdown_mcp_connections};

    const PREP_SERVER: &str = r#"
import json, os, sys
with open(os.environ["MCP_PID_FILE"], "w") as handle:
    handle.write(str(os.getpid()))
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        request = json.loads(line)
    except ValueError:
        continue
    method = request.get("method")
    if method == "initialize":
        result = {}
    elif method == "tools/list":
        result = {"tools": [{"name": "probe", "description": "", "inputSchema": {}}]}
    else:
        continue
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": request.get("id"), "result": result}) + "\n")
    sys.stdout.flush()
"#;

    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let pid_file = cwd.join("mcp.pid");
    // The real CLI default path: a project `.mio/mcp.json` loaded by
    // `load_policy_bound_tools` before the first model permit, with only a
    // per-start host approval (no persisted trust).
    let config = json!({
        "mcpServers": {
            "prep": {
                "command": "python3",
                "args": ["-u", "-c", PREP_SERVER],
                "env": {"MCP_PID_FILE": pid_file.display().to_string()}
            }
        }
    });
    std::fs::create_dir_all(cwd.join(".mio")).unwrap();
    std::fs::write(
        cwd.join(".mio/mcp.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();

    let (tracker, _sink) = tracker_with_sink();
    let id = register(&tracker, "host-preparation-mcp");
    let handle = task_lifecycle::TaskHandle::new(tracker.clone(), id.clone());
    let host: Arc<dyn Host> = Arc::new(SdkTrackingHost::default());
    let tools = task_lifecycle::scope_task(
        handle.clone(),
        load_policy_bound_tools(&cwd, host, &ToolFilter::all()),
    )
    .await
    .expect("host preparation must start the real project helper");
    assert!(!tools.is_empty(), "the MCP tool must be registered");

    let record = handle.snapshot().unwrap();
    assert_eq!(
        record.phase,
        TaskPhase::Queued,
        "host preparation must not fake a Running admission"
    );
    assert_eq!(
        record.workers, 0,
        "preparation is not a bound engine worker"
    );
    assert!(!record.was_admitted());

    // The real helper is alive while the task is still Queued.
    let pid = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(text) = tokio::fs::read_to_string(&pid_file).await
                && let Ok(pid) = text.trim().parse::<i32>()
            {
                break pid;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the MCP helper must publish its pid");
    assert!(
        unsafe { libc::kill(pid, 0) } == 0,
        "the host-preparation helper must be alive"
    );

    // Cancel while still Queued: no fake Running, and the helper must be
    // reclaimed once its host-preparation lifetime ends.
    assert_eq!(
        handle.request_cancel(None).unwrap(),
        CancelOutcome::Cancelled
    );
    assert_eq!(handle.snapshot().unwrap().phase, TaskPhase::Cancelled);
    drop(tools);
    shutdown_mcp_connections();
    let mut gone = false;
    for _ in 0..200 {
        // SAFETY: signal 0 only probes the test-created pid.
        if unsafe { libc::kill(pid, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(gone, "the MCP helper must be reaped after preparation ends");
}
