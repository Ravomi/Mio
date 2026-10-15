//! `coordinator` 的本地异步测试。
//!
//! 该文件通过 `#[path]` 直接编译 `src/coordinator.rs`，因此在 `lib.rs` 导出该模块
//! 之前即可独立运行：`cargo +1.95.0 test --locked --test coordinator`。

#[path = "../src/coordinator.rs"]
mod coordinator;

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use coordinator::{Coordinator, CoordinatorError, Limits, TaskRequest};
use tokio::sync::{mpsc, watch};

#[tokio::test]
async fn permanent_budget_failure_does_not_wait_for_unrelated_in_flight_calls() {
    let coordinator = Coordinator::new(Limits {
        max_total_tokens: Some(100),
        ..Limits::default()
    })
    .unwrap();
    coordinator
        .acquire_model("settled", 80)
        .await
        .unwrap()
        .complete(Some(80));
    let active = coordinator.acquire_model("active", 10).await.unwrap();
    let result = tokio::time::timeout(
        Duration::from_millis(100),
        coordinator.acquire_model("new", 30),
    )
    .await
    .unwrap();
    assert!(matches!(
        result,
        Err(CoordinatorError::BudgetExhausted { .. })
    ));
    active.complete(Some(0));
    assert_eq!(coordinator.snapshot().budget.tokens_used, 80);
}

/// 获取任务许可，上报标签，等待 release 置位后再释放许可。
async fn hold_task(
    coordinator: Coordinator,
    request: TaskRequest,
    label: u8,
    events: mpsc::UnboundedSender<u8>,
    release: watch::Receiver<bool>,
) {
    let permit = coordinator.acquire_task(request).await.unwrap();
    let _ = events.send(label);
    wait_release(release).await;
    drop(permit);
}

/// 获取模型许可，上报标签，等待 release 置位后再释放许可。
async fn hold_model(
    coordinator: Coordinator,
    provider: &'static str,
    label: u8,
    events: mpsc::UnboundedSender<u8>,
    release: watch::Receiver<bool>,
) {
    let permit = coordinator.acquire_model(provider, 0).await.unwrap();
    let _ = events.send(label);
    wait_release(release).await;
    drop(permit);
}

/// 获取重任务许可，上报标签，等待 release 置位后再释放许可。
async fn hold_heavy(
    coordinator: Coordinator,
    label: u8,
    events: mpsc::UnboundedSender<u8>,
    release: watch::Receiver<bool>,
) {
    let permit = coordinator.acquire_heavy().await.unwrap();
    let _ = events.send(label);
    wait_release(release).await;
    drop(permit);
}

async fn wait_release(mut release: watch::Receiver<bool>) {
    while !*release.borrow() {
        if release.changed().await.is_err() {
            break;
        }
    }
}

#[test]
fn default_limits_are_valid() {
    let limits = Limits::default();
    assert!(limits.validate().is_ok());
    assert_eq!(limits.agent, 4);
    assert_eq!(limits.heavy, 2);
    assert_eq!(limits.model, 2);
    assert_eq!(limits.provider_model, 2);
    assert_eq!(Coordinator::with_defaults().limits().agent, 4);
}

#[test]
fn invalid_limits_are_rejected() {
    for limits in [
        Limits {
            agent: 0,
            ..Limits::default()
        },
        Limits {
            heavy: 0,
            ..Limits::default()
        },
        Limits {
            model: 0,
            ..Limits::default()
        },
        Limits {
            provider_model: 0,
            ..Limits::default()
        },
        Limits {
            queue_capacity: 0,
            ..Limits::default()
        },
    ] {
        assert!(matches!(
            Coordinator::new(limits),
            Err(CoordinatorError::InvalidLimits(_))
        ));
    }

    let mut overrides = BTreeMap::new();
    overrides.insert("p".to_string(), 0);
    assert!(matches!(
        Coordinator::new(Limits {
            provider_model_overrides: overrides,
            ..Limits::default()
        }),
        Err(CoordinatorError::InvalidLimits(_))
    ));
}

#[tokio::test]
async fn agent_cap_is_fifo() {
    let coordinator = Coordinator::new(Limits {
        agent: 1,
        ..Limits::default()
    })
    .unwrap();
    let held = coordinator
        .acquire_task(TaskRequest::write("a"))
        .await
        .unwrap();

    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<u8>();
    let (release_tx, release_rx) = watch::channel(false);

    let first = tokio::spawn(hold_task(
        coordinator.clone(),
        TaskRequest::write("b"),
        1,
        events_tx.clone(),
        release_rx.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(30)).await;
    let second = tokio::spawn(hold_task(
        coordinator.clone(),
        TaskRequest::write("c"),
        2,
        events_tx.clone(),
        release_rx.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(30)).await;

    assert_eq!(coordinator.snapshot().tasks.queued, 2);
    drop(held);
    assert_eq!(events_rx.recv().await.unwrap(), 1);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        events_rx.try_recv().is_err(),
        "second task must wait for the first to release"
    );
    release_tx.send(true).unwrap();
    assert_eq!(events_rx.recv().await.unwrap(), 2);
    first.await.unwrap();
    second.await.unwrap();
}

#[tokio::test]
async fn queued_disjoint_task_progresses_past_conflicting_head() {
    let coordinator = Coordinator::new(Limits::default()).unwrap();
    let held = coordinator
        .acquire_task(TaskRequest::write("a"))
        .await
        .unwrap();

    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<u8>();
    let (release_tx, release_rx) = watch::channel(false);

    // 队列头部与活动写范围 a 冲突，暂时无法授予。
    let blocked = tokio::spawn(hold_task(
        coordinator.clone(),
        TaskRequest::write("a/deep"),
        1,
        events_tx.clone(),
        release_rx.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(30)).await;
    // 与头部不相交的后继可以直接授予，不被头部阻塞。
    let disjoint = tokio::spawn(hold_task(
        coordinator.clone(),
        TaskRequest::write("b"),
        2,
        events_tx.clone(),
        release_rx.clone(),
    ));

    assert_eq!(events_rx.recv().await.unwrap(), 2);
    assert_eq!(coordinator.snapshot().tasks.queued, 1);
    drop(held);
    assert_eq!(events_rx.recv().await.unwrap(), 1);
    release_tx.send(true).unwrap();
    blocked.await.unwrap();
    disjoint.await.unwrap();
}

#[tokio::test]
async fn heavy_cap_serializes() {
    let coordinator = Coordinator::new(Limits {
        heavy: 1,
        ..Limits::default()
    })
    .unwrap();
    let first = coordinator.acquire_heavy().await.unwrap();

    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<u8>();
    let (release_tx, release_rx) = watch::channel(false);
    let queued = tokio::spawn(hold_heavy(coordinator.clone(), 1, events_tx, release_rx));
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(coordinator.snapshot().heavy.active, 1);
    assert_eq!(coordinator.snapshot().heavy.queued, 1);

    // 被 timeout 取消的等待不会泄漏名额。
    assert!(
        tokio::time::timeout(Duration::from_millis(30), coordinator.acquire_heavy())
            .await
            .is_err()
    );
    assert_eq!(coordinator.snapshot().heavy.queued, 1);

    drop(first);
    assert_eq!(events_rx.recv().await.unwrap(), 1);
    release_tx.send(true).unwrap();
    queued.await.unwrap();
    assert_eq!(coordinator.snapshot().heavy.queued, 0);
    assert_eq!(coordinator.snapshot().heavy.active, 0);
}

#[tokio::test]
async fn path_conflicts_and_read_concurrency() {
    let coordinator = Coordinator::new(Limits::default()).unwrap();

    // 同一路径的两个只读任务可以并发。
    let read_a = coordinator
        .acquire_task(TaskRequest::read("src/a.rs"))
        .await
        .unwrap();
    let read_b = coordinator
        .acquire_task(TaskRequest::read("src/a.rs"))
        .await
        .unwrap();
    // 写同一路径会被活动的读阻塞。
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            coordinator.acquire_task(TaskRequest::write("src/a.rs"))
        )
        .await
        .is_err()
    );
    drop(read_a);
    drop(read_b);

    // 不相交路径的写可以并发。
    let write_a = coordinator
        .acquire_task(TaskRequest::write("src/a.rs"))
        .await
        .unwrap();
    let write_b = coordinator
        .acquire_task(TaskRequest::write("src/b.rs"))
        .await
        .unwrap();
    // 目录祖先冲突。
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            coordinator.acquire_task(TaskRequest::write("src"))
        )
        .await
        .is_err()
    );
    // 子路径冲突。
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            coordinator.acquire_task(TaskRequest::write("src/a.rs/deep"))
        )
        .await
        .is_err()
    );
    // 读与活动写范围重叠时冲突。
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            coordinator.acquire_task(TaskRequest::read("src/a.rs"))
        )
        .await
        .is_err()
    );
    // 不相交的读不受影响。
    let read_other = coordinator
        .acquire_task(TaskRequest::read("other/x"))
        .await
        .unwrap();
    drop(read_other);
    drop(write_a);
    drop(write_b);
}

#[tokio::test]
async fn invalid_scopes_are_rejected() {
    let coordinator = Coordinator::new(Limits::default()).unwrap();
    for path in ["../x", "/etc/passwd", "a/../b", "", "a/./../b"] {
        let error = coordinator
            .acquire_task(TaskRequest::write(path))
            .await
            .unwrap_err();
        assert!(
            matches!(error, CoordinatorError::InvalidScope(_)),
            "path {path:?} -> {error:?}"
        );
    }
}

#[tokio::test]
async fn cancelling_queued_task_releases_queue_slot() {
    let coordinator = Coordinator::new(Limits {
        agent: 1,
        queue_capacity: 1,
        ..Limits::default()
    })
    .unwrap();
    let held = coordinator
        .acquire_task(TaskRequest::write("a"))
        .await
        .unwrap();

    // 排队等待被 timeout 取消后，队列名额必须释放，不能泄漏。
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            coordinator.acquire_task(TaskRequest::write("b"))
        )
        .await
        .is_err()
    );
    assert_eq!(coordinator.snapshot().tasks.queued, 0);

    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<u8>();
    let (release_tx, release_rx) = watch::channel(false);
    let queued = tokio::spawn(hold_task(
        coordinator.clone(),
        TaskRequest::write("c"),
        7,
        events_tx,
        release_rx,
    ));
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(coordinator.snapshot().tasks.queued, 1);
    drop(held);
    assert_eq!(events_rx.recv().await.unwrap(), 7);
    release_tx.send(true).unwrap();
    queued.await.unwrap();
}

#[tokio::test]
async fn duplicate_dedup_key_is_rejected_and_reusable() {
    let coordinator = Coordinator::new(Limits::default()).unwrap();
    let first = coordinator
        .acquire_task(TaskRequest::write("a").with_dedup_key("k"))
        .await
        .unwrap();
    let id = first.id().clone();
    assert!(!id.as_str().is_empty());
    assert_eq!(first.dedup_key(), Some("k"));
    assert!(first.is_write());
    assert_eq!(first.path(), "a");
    assert_eq!(coordinator.lookup_dedup("k"), Some(id.clone()));

    let error = coordinator
        .acquire_task(TaskRequest::read("b").with_dedup_key("k"))
        .await
        .unwrap_err();
    match error {
        CoordinatorError::DuplicateTask {
            dedup_key,
            existing,
        } => {
            assert_eq!(dedup_key, "k");
            assert_eq!(existing, id);
        }
        other => panic!("expected duplicate error, got {other:?}"),
    }

    drop(first);
    assert!(coordinator.lookup_dedup("k").is_none());
    let again = coordinator
        .acquire_task(TaskRequest::read("b").with_dedup_key("k"))
        .await
        .unwrap();
    drop(again);
}

#[tokio::test]
async fn duplicate_dedup_key_while_queued_is_rejected() {
    let coordinator = Coordinator::new(Limits {
        agent: 1,
        ..Limits::default()
    })
    .unwrap();
    let held = coordinator
        .acquire_task(TaskRequest::write("a"))
        .await
        .unwrap();

    let (events_tx, _events_rx) = mpsc::unbounded_channel::<u8>();
    let (release_tx, release_rx) = watch::channel(false);
    let queued = tokio::spawn(hold_task(
        coordinator.clone(),
        TaskRequest::write("b").with_dedup_key("q"),
        1,
        events_tx,
        release_rx,
    ));
    tokio::time::sleep(Duration::from_millis(30)).await;

    let error = coordinator
        .acquire_task(TaskRequest::read("c").with_dedup_key("q"))
        .await
        .unwrap_err();
    assert!(matches!(error, CoordinatorError::DuplicateTask { .. }));

    drop(held);
    release_tx.send(true).unwrap();
    queued.await.unwrap();
}

#[tokio::test]
async fn queue_capacity_is_enforced() {
    let coordinator = Coordinator::new(Limits {
        agent: 1,
        queue_capacity: 1,
        ..Limits::default()
    })
    .unwrap();
    let held = coordinator
        .acquire_task(TaskRequest::write("a"))
        .await
        .unwrap();

    let (events_tx, _events_rx) = mpsc::unbounded_channel::<u8>();
    let (release_tx, release_rx) = watch::channel(false);
    let queued = tokio::spawn(hold_task(
        coordinator.clone(),
        TaskRequest::write("b"),
        1,
        events_tx,
        release_rx,
    ));
    tokio::time::sleep(Duration::from_millis(30)).await;

    let error = coordinator
        .acquire_task(TaskRequest::write("c"))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        CoordinatorError::QueueFull {
            queue: "task",
            capacity: 1
        }
    ));

    drop(held);
    release_tx.send(true).unwrap();
    queued.await.unwrap();
}

#[tokio::test]
async fn provider_cooldown_is_shared_but_isolated_per_provider() {
    let coordinator = Coordinator::new(Limits {
        model: 2,
        provider_model: 2,
        ..Limits::default()
    })
    .unwrap();
    coordinator.defer_provider("cooling-provider", Duration::from_millis(200));

    let start = Instant::now();
    let healthy = coordinator
        .acquire_model("healthy-provider", 0)
        .await
        .unwrap();
    assert!(
        start.elapsed() < Duration::from_millis(100),
        "healthy provider must not wait on another provider's cooldown, waited {:?}",
        start.elapsed()
    );

    let start = Instant::now();
    let cooled = coordinator
        .acquire_model("cooling-provider", 0)
        .await
        .unwrap();
    assert!(
        start.elapsed() >= Duration::from_millis(150),
        "cooling provider must wait out its cooldown, waited {:?}",
        start.elapsed()
    );
    drop(healthy);
    drop(cooled);
    assert!(coordinator.snapshot().model.cooling.is_empty());
}

#[tokio::test]
async fn defer_provider_takes_max_deadline() {
    let coordinator = Coordinator::new(Limits::default()).unwrap();
    coordinator.defer_provider("p", Duration::from_millis(40));
    coordinator.defer_provider("p", Duration::from_millis(160));

    let start = Instant::now();
    let permit = coordinator.acquire_model("p", 0).await.unwrap();
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(120),
        "expected max cooldown deadline, waited {elapsed:?}"
    );
    drop(permit);
}

#[tokio::test]
async fn model_queue_is_fifo_per_provider() {
    let coordinator = Coordinator::new(Limits {
        model: 1,
        provider_model: 2,
        ..Limits::default()
    })
    .unwrap();
    let held = coordinator.acquire_model("p", 0).await.unwrap();

    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<u8>();
    let (release_tx, release_rx) = watch::channel(false);
    let first = tokio::spawn(hold_model(
        coordinator.clone(),
        "p",
        1,
        events_tx.clone(),
        release_rx.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(30)).await;
    let second = tokio::spawn(hold_model(
        coordinator.clone(),
        "p",
        2,
        events_tx.clone(),
        release_rx.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(30)).await;

    assert_eq!(coordinator.snapshot().model.queued, 2);
    drop(held);
    assert_eq!(events_rx.recv().await.unwrap(), 1);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        events_rx.try_recv().is_err(),
        "same-provider queue must be FIFO"
    );
    release_tx.send(true).unwrap();
    assert_eq!(events_rx.recv().await.unwrap(), 2);
    first.await.unwrap();
    second.await.unwrap();
}

#[tokio::test]
async fn provider_override_limits_single_provider() {
    let mut overrides = BTreeMap::new();
    overrides.insert("slow".to_string(), 1);
    let coordinator = Coordinator::new(Limits {
        model: 4,
        provider_model: 4,
        provider_model_overrides: overrides,
        ..Limits::default()
    })
    .unwrap();

    let slow = coordinator.acquire_model("slow", 0).await.unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            coordinator.acquire_model("slow", 0)
        )
        .await
        .is_err()
    );
    let fast = tokio::time::timeout(
        Duration::from_millis(100),
        coordinator.acquire_model("fast", 0),
    )
    .await
    .expect("other provider must be unaffected by override")
    .unwrap();
    drop(slow);
    drop(fast);
}

#[tokio::test]
async fn token_budget_reserves_before_call_and_refunds_on_settle() {
    let coordinator = Coordinator::new(Limits {
        model: 4,
        provider_model: 4,
        max_total_tokens: Some(100),
        ..Limits::default()
    })
    .unwrap();

    let first = coordinator.acquire_model("p", 60).await.unwrap();
    assert_eq!(first.provider(), "p");
    assert_eq!(first.reservation(), 60);
    // 并发预留不能超发。
    assert!(
        tokio::time::timeout(
            Duration::from_millis(60),
            coordinator.acquire_model("p", 60)
        )
        .await
        .is_err()
    );
    assert_eq!(coordinator.snapshot().budget.tokens_reserved_active, 60);

    // 已知使用量为 0 时退还预留额。
    first.complete(Some(0));
    let second = tokio::time::timeout(
        Duration::from_millis(200),
        coordinator.acquire_model("p", 60),
    )
    .await
    .unwrap()
    .unwrap();
    drop(second);

    let budget = coordinator.snapshot().budget;
    assert_eq!(budget.tokens_used, 60);
    assert_eq!(budget.tokens_reserved_active, 0);
    assert!(!budget.blocked);
}

#[tokio::test]
async fn unknown_usage_is_conservatively_charged() {
    let limits = Limits {
        model: 4,
        provider_model: 4,
        max_total_tokens: Some(100),
        ..Limits::default()
    };
    let coordinator = Coordinator::new(limits.clone()).unwrap();
    let permit = coordinator.acquire_model("p", 80).await.unwrap();
    permit.complete(None);
    assert_eq!(coordinator.snapshot().budget.tokens_used, 80);
    // 未知 usage 按预留额 80 计费且已无在途预留，剩余 20 无法容纳 30，必须快速失败。
    let blocked = tokio::time::timeout(
        Duration::from_millis(60),
        coordinator.acquire_model("p", 30),
    )
    .await
    .expect("no in-flight reservation: must fail fast rather than hang");
    assert!(
        matches!(blocked, Err(CoordinatorError::BudgetExhausted { .. })),
        "unknown usage must not be treated as zero"
    );

    // drop 而不 complete 同样保守结算。
    let coordinator = Coordinator::new(limits).unwrap();
    let permit = coordinator.acquire_model("p", 80).await.unwrap();
    drop(permit);
    assert_eq!(coordinator.snapshot().budget.tokens_used, 80);
}

#[tokio::test]
async fn overage_blocks_follow_up_requests() {
    let coordinator = Coordinator::new(Limits {
        model: 4,
        provider_model: 4,
        max_total_tokens: Some(100),
        ..Limits::default()
    })
    .unwrap();

    let permit = coordinator.acquire_model("p", 60).await.unwrap();
    permit.complete(Some(150));

    let budget = coordinator.snapshot().budget;
    assert!(budget.blocked);
    assert_eq!(budget.tokens_used, 150);
    assert_eq!(budget.tokens_overage, 50);

    let error = coordinator.acquire_model("p", 1).await.unwrap_err();
    assert!(matches!(error, CoordinatorError::BudgetExhausted { .. }));
}

#[tokio::test]
async fn model_call_budget_is_hard() {
    let coordinator = Coordinator::new(Limits {
        model: 4,
        provider_model: 4,
        max_model_calls: Some(1),
        ..Limits::default()
    })
    .unwrap();

    let permit = coordinator.acquire_model("p", 0).await.unwrap();
    permit.complete(Some(0));
    assert_eq!(coordinator.snapshot().budget.model_calls_started, 1);

    let error = coordinator.acquire_model("p", 0).await.unwrap_err();
    assert!(matches!(
        error,
        CoordinatorError::ModelCallBudgetExceeded { used: 1, limit: 1 }
    ));
}

#[tokio::test]
async fn budget_that_can_never_fit_fails_fast() {
    let coordinator = Coordinator::new(Limits {
        model: 4,
        provider_model: 4,
        max_total_tokens: Some(100),
        ..Limits::default()
    })
    .unwrap();

    // 结算 100 后已无在途预留，剩余预算为 0：任何正预留都不可能再容纳。
    let first = coordinator.acquire_model("p", 60).await.unwrap();
    first.complete(Some(100));
    assert_eq!(coordinator.snapshot().budget.tokens_reserved_active, 0);

    // 看门狗：必须在超时前立即 fail-closed，而不是永久等待 notify。
    let result = tokio::time::timeout(
        Duration::from_millis(200),
        coordinator.acquire_model("p", 10),
    )
    .await
    .expect("must fail fast when no in-flight request can ever refund");
    assert!(matches!(
        result,
        Err(CoordinatorError::BudgetExhausted { .. })
    ));
}

#[tokio::test]
async fn budget_waiter_continues_after_refund() {
    let coordinator = Coordinator::new(Limits {
        model: 4,
        provider_model: 4,
        max_total_tokens: Some(100),
        ..Limits::default()
    })
    .unwrap();
    let first = coordinator.acquire_model("p", 60).await.unwrap();

    let waiter_coordinator = coordinator.clone();
    let waiter =
        tokio::spawn(async move { waiter_coordinator.acquire_model("p", 60).await.map(drop) });
    tokio::time::sleep(Duration::from_millis(30)).await;
    // 有在途预留时，即便暂时超上限也不能判为永久不可行，必须继续等待退款。
    assert_eq!(coordinator.snapshot().model.queued, 1);

    first.complete(Some(0));
    let result = tokio::time::timeout(Duration::from_millis(300), waiter)
        .await
        .expect("waiter must continue once the in-flight reservation is refunded")
        .unwrap();
    assert!(result.is_ok());
}

#[tokio::test]
async fn writer_is_not_starved_by_later_readers() {
    let coordinator = Coordinator::new(Limits::default()).unwrap();
    let reader = coordinator
        .acquire_task(TaskRequest::read("x"))
        .await
        .unwrap();

    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<u8>();
    let (release_tx, release_rx) = watch::channel(false);
    // 写等待者先入队。
    let writer = tokio::spawn(hold_task(
        coordinator.clone(),
        TaskRequest::write("x"),
        1,
        events_tx.clone(),
        release_rx.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(30)).await;
    // 同路径的后来读者入队，不得越过更早的写者。
    let late_reader = tokio::spawn(hold_task(
        coordinator.clone(),
        TaskRequest::read("x"),
        2,
        events_tx.clone(),
        release_rx.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(coordinator.snapshot().tasks.queued, 2);

    // 与两者都不相交的任务仍可进入。
    let disjoint = coordinator
        .acquire_task(TaskRequest::write("y"))
        .await
        .unwrap();
    assert_eq!(coordinator.snapshot().tasks.queued, 2);

    drop(reader);
    assert_eq!(events_rx.recv().await.unwrap(), 1);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        events_rx.try_recv().is_err(),
        "later reader must not bypass the queued writer"
    );

    release_tx.send(true).unwrap();
    assert_eq!(events_rx.recv().await.unwrap(), 2);
    writer.await.unwrap();
    late_reader.await.unwrap();
    drop(disjoint);
}

#[tokio::test]
async fn dot_scope_is_workspace_root() {
    let coordinator = Coordinator::new(Limits::default()).unwrap();

    let root_read = coordinator
        .acquire_task(TaskRequest::read("."))
        .await
        .unwrap();
    assert_eq!(root_read.path(), ".");
    let sub_read = coordinator
        .acquire_task(TaskRequest::read("src/a.rs"))
        .await
        .unwrap();
    // 根写与任意子路径写冲突。
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            coordinator.acquire_task(TaskRequest::write("src/a.rs"))
        )
        .await
        .is_err()
    );
    drop(sub_read);
    drop(root_read);

    let root_write = coordinator
        .acquire_task(TaskRequest::write("."))
        .await
        .unwrap();
    assert_eq!(root_write.path(), ".");
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            coordinator.acquire_task(TaskRequest::write("deep/child"))
        )
        .await
        .is_err()
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            coordinator.acquire_task(TaskRequest::read("x"))
        )
        .await
        .is_err()
    );
    drop(root_write);

    // 空字符串仍然拒绝。
    assert!(matches!(
        coordinator.acquire_task(TaskRequest::write("")).await,
        Err(CoordinatorError::InvalidScope(_))
    ));
}

#[tokio::test]
async fn waiting_task_does_not_block_model_or_heavy() {
    let coordinator = Coordinator::new(Limits {
        agent: 1,
        model: 1,
        heavy: 1,
        ..Limits::default()
    })
    .unwrap();
    let held = coordinator
        .acquire_task(TaskRequest::write("a"))
        .await
        .unwrap();

    let (events_tx, _events_rx) = mpsc::unbounded_channel::<u8>();
    let (release_tx, release_rx) = watch::channel(false);
    let queued = tokio::spawn(hold_task(
        coordinator.clone(),
        TaskRequest::write("a/deep"),
        1,
        events_tx,
        release_rx,
    ));
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(coordinator.snapshot().tasks.queued, 1);

    // 任务队列的等待不能阻塞模型 / 重任务许可，避免循环等待。
    let model = tokio::time::timeout(
        Duration::from_millis(100),
        coordinator.acquire_model("p", 0),
    )
    .await
    .expect("model must not be blocked by a queued task")
    .unwrap();
    let heavy = tokio::time::timeout(Duration::from_millis(100), coordinator.acquire_heavy())
        .await
        .expect("heavy must not be blocked by a queued task")
        .unwrap();
    drop(model);
    drop(heavy);

    drop(held);
    release_tx.send(true).unwrap();
    queued.await.unwrap();
}
