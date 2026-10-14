use mio_coding_agent::{
    AgentEvent, Config, Host, HostFuture, Message, RunOptions,
    delegation::{
        DelegateParams, DelegationAdapter, DelegationConstraints, DelegationMode, KillParams,
        SessionSnapshotParams, StatusParams, resolve_cwd_to_relative,
    },
    subagent::{Executor, Status, SubagentManager, Task},
};
use serde_json::json;
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct MockHost(Mutex<Vec<AgentEvent>>);
impl Host for MockHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Err("Mock host shell not permitted".into()) })
    }
    fn on_event(&self, event: AgentEvent) {
        self.0.lock().unwrap().push(event);
    }
}

struct MockExecutor {
    delay: Duration,
    call_count: Arc<AtomicUsize>,
}

impl Executor for MockExecutor {
    fn execute(&self, mut task: Task) -> HostFuture<Result<Vec<Message>, String>> {
        let delay = self.delay;
        let count = self.call_count.clone();
        Box::pin(async move {
            count.fetch_add(1, Ordering::SeqCst);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            if task.prompt == "fail" {
                return Err("mock failure".into());
            }
            if task.prompt == "hang" {
                std::future::pending::<()>().await;
            }
            task.host
                .on_event(AgentEvent::Text(format!("echo: {}", task.prompt)));
            task.history
                .push(Message::user(format!("ack: {}", task.prompt)));
            Ok(task.history)
        })
    }
}

fn create_adapter(
    root: &Path,
    delay: Duration,
    timeout: Duration,
) -> (DelegationAdapter, Arc<MockHost>, Arc<AtomicUsize>) {
    let call_count = Arc::new(AtomicUsize::new(0));
    let manager = SubagentManager::with_executor(
        root,
        Config {
            model: "mock-model".into(),
            api_key: "mock-key".into(),
            ..Config::default()
        },
        RunOptions::default(),
        4,
        timeout,
        Arc::new(MockExecutor {
            delay,
            call_count: call_count.clone(),
        }),
    )
    .unwrap();
    let host = Arc::new(MockHost::default());
    let adapter = DelegationAdapter::new(Arc::new(manager), host.clone());
    (adapter, host, call_count)
}

#[tokio::test]
async fn test_async_and_sync_delegation_flow() {
    let dir = tempfile::tempdir().unwrap();
    let (adapter, _host, _) = create_adapter(
        dir.path(),
        Duration::from_millis(50),
        Duration::from_secs(5),
    );

    // 1. Async delegation returns immediately with running status
    let res_async = adapter
        .delegate(DelegateParams {
            session: "s1".into(),
            goal: "inspect code".into(),
            prompt: "hello async".into(),
            cwd: None,
            mode: DelegationMode::Async,
            run_timeout_ms: None,
            wait_timeout_ms: None,
            read_only: Some(true),
            constraints: None,
        })
        .await
        .unwrap();

    let meta = match res_async {
        mio_coding_agent::delegation::DelegateResponse::Async(m) => m,
        _ => panic!("Expected async response"),
    };
    assert_eq!(meta.session, "s1");
    assert_eq!(meta.status, Status::Running);

    // Query status with wait
    let polled = adapter
        .status(StatusParams {
            run_id: meta.run_id,
            wait_timeout_ms: Some(1000),
        })
        .await
        .unwrap();
    assert_eq!(polled.status, Status::Completed);

    // 2. Sync delegation waits until terminal state
    let res_sync = adapter
        .delegate(DelegateParams {
            session: "s2".into(),
            goal: "sync run".into(),
            prompt: "hello sync".into(),
            cwd: None,
            mode: DelegationMode::Sync,
            run_timeout_ms: None,
            wait_timeout_ms: Some(2000),
            read_only: Some(true),
            constraints: None,
        })
        .await
        .unwrap();

    let snap = match res_sync {
        mio_coding_agent::delegation::DelegateResponse::Sync(s) => s,
        _ => panic!("Expected sync response"),
    };
    assert_eq!(snap.session, "s2");
    assert_eq!(snap.status, Status::Completed);
    assert!(snap.output.contains("echo: hello sync"));
}

#[tokio::test]
async fn test_single_task_timeout() {
    let dir = tempfile::tempdir().unwrap();
    // Manager default timeout 10s, but task run_timeout_ms is 100ms
    let (adapter, _host, _) = create_adapter(
        dir.path(),
        Duration::from_millis(0),
        Duration::from_secs(10),
    );

    let res = adapter
        .delegate(DelegateParams {
            session: "timeout-sess".into(),
            goal: "test timeout".into(),
            prompt: "hang".into(),
            cwd: None,
            mode: DelegationMode::Sync,
            run_timeout_ms: Some(100),
            wait_timeout_ms: Some(2000),
            read_only: Some(true),
            constraints: None,
        })
        .await
        .unwrap();

    let snap = match res {
        mio_coding_agent::delegation::DelegateResponse::Sync(s) => s,
        _ => panic!("Expected sync response"),
    };
    assert_eq!(snap.status, Status::TimedOut);
    assert_eq!(snap.error.as_deref(), Some("Child timed out"));
}

#[tokio::test]
async fn test_cwd_boundary_conversion_and_enforcement() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let sub = root.join("child_dir");
    std::fs::create_dir_all(&sub).unwrap();

    // 1. Safe conversion: absolute path inside root
    let rel = resolve_cwd_to_relative(root, sub.to_str().unwrap()).unwrap();
    assert_eq!(rel, "child_dir");

    // 2. Safe conversion: root itself
    let rel_root = resolve_cwd_to_relative(root, root.to_str().unwrap()).unwrap();
    assert_eq!(rel_root, ".");

    // 3. Reject: absolute path outside root
    let outside = tempfile::tempdir().unwrap();
    assert!(resolve_cwd_to_relative(root, outside.path().to_str().unwrap()).is_err());

    // 4. Reject: relative path with '..'
    assert!(resolve_cwd_to_relative(root, "../outside").is_err());
    assert!(resolve_cwd_to_relative(root, "child/../../outside").is_err());

    // 5. Delegation with valid cwd
    let (adapter, _host, _) =
        create_adapter(root, Duration::from_millis(0), Duration::from_secs(5));
    let res = adapter
        .delegate(DelegateParams {
            session: "cwd-sess".into(),
            goal: "cwd check".into(),
            prompt: "check".into(),
            cwd: Some(sub.to_str().unwrap().into()),
            mode: DelegationMode::Sync,
            run_timeout_ms: None,
            wait_timeout_ms: None,
            read_only: Some(true),
            constraints: None,
        })
        .await
        .unwrap();
    let snap = match res {
        mio_coding_agent::delegation::DelegateResponse::Sync(s) => s,
        _ => panic!("Expected sync response"),
    };
    assert_eq!(snap.cwd, sub.canonicalize().unwrap());
}

#[tokio::test]
async fn test_resume_and_session_queries() {
    let dir = tempfile::tempdir().unwrap();
    let (adapter, _host, count) =
        create_adapter(dir.path(), Duration::from_millis(0), Duration::from_secs(5));

    // Turn 1 on s1
    let r1 = adapter
        .delegate(DelegateParams {
            session: "resume-sess".into(),
            goal: "multi turn".into(),
            prompt: "turn 1".into(),
            cwd: None,
            mode: DelegationMode::Sync,
            run_timeout_ms: None,
            wait_timeout_ms: None,
            read_only: Some(true),
            constraints: None,
        })
        .await
        .unwrap();
    let s1 = match r1 {
        mio_coding_agent::delegation::DelegateResponse::Sync(s) => s,
        _ => panic!(),
    };
    assert_eq!(s1.status, Status::Completed);

    // Turn 2 resume same session
    let r2 = adapter
        .delegate(DelegateParams {
            session: "resume-sess".into(),
            goal: "multi turn".into(),
            prompt: "turn 2".into(),
            cwd: None,
            mode: DelegationMode::Sync,
            run_timeout_ms: None,
            wait_timeout_ms: None,
            read_only: Some(true),
            constraints: None,
        })
        .await
        .unwrap();
    let s2 = match r2 {
        mio_coding_agent::delegation::DelegateResponse::Sync(s) => s,
        _ => panic!(),
    };
    assert_eq!(s2.status, Status::Completed);
    assert_ne!(s1.run_id, s2.run_id);
    assert_eq!(count.load(Ordering::SeqCst), 2);

    // session_snapshot finds latest run for session
    let latest = adapter
        .session_snapshot(SessionSnapshotParams {
            session: "resume-sess".into(),
        })
        .unwrap();
    assert_eq!(latest.run_id, s2.run_id);
    assert!(latest.output.contains("echo: turn 2"));

    // session_list returns metadata for both runs
    let list = adapter.session_list();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].run_id, s1.run_id);
    assert_eq!(list[1].run_id, s2.run_id);
}

#[tokio::test]
async fn test_kill_operation() {
    let dir = tempfile::tempdir().unwrap();
    let (adapter, _host, _) = create_adapter(
        dir.path(),
        Duration::from_millis(0),
        Duration::from_secs(10),
    );

    let res = adapter
        .delegate(DelegateParams {
            session: "kill-sess".into(),
            goal: "will be killed".into(),
            prompt: "hang".into(),
            cwd: None,
            mode: DelegationMode::Async,
            run_timeout_ms: None,
            wait_timeout_ms: None,
            read_only: Some(true),
            constraints: None,
        })
        .await
        .unwrap();
    let meta = match res {
        mio_coding_agent::delegation::DelegateResponse::Async(m) => m,
        _ => panic!(),
    };

    let killed = adapter
        .kill(KillParams {
            run_id: meta.run_id,
            wait_timeout_ms: None,
        })
        .await
        .unwrap();
    assert_eq!(killed.status, Status::Cancelled);
}

#[tokio::test]
async fn test_explicitly_reject_unknown_constraints() {
    let dir = tempfile::tempdir().unwrap();
    let (adapter, _host, _) =
        create_adapter(dir.path(), Duration::from_millis(0), Duration::from_secs(5));

    // Reject noSkills
    let err_noskills = adapter
        .delegate(DelegateParams {
            session: "rej-1".into(),
            goal: "g".into(),
            prompt: "p".into(),
            cwd: None,
            mode: DelegationMode::Async,
            run_timeout_ms: None,
            wait_timeout_ms: None,
            read_only: Some(true),
            constraints: Some(DelegationConstraints {
                tools: None,
                no_skills: Some(true),
                thinking: None,
            }),
        })
        .await
        .unwrap_err();
    assert!(err_noskills.contains("noSkills"));

    // Reject thinking
    let err_thinking = adapter
        .delegate(DelegateParams {
            session: "rej-2".into(),
            goal: "g".into(),
            prompt: "p".into(),
            cwd: None,
            mode: DelegationMode::Async,
            run_timeout_ms: None,
            wait_timeout_ms: None,
            read_only: Some(true),
            constraints: Some(DelegationConstraints {
                tools: None,
                no_skills: None,
                thinking: Some(json!({"budget_tokens": 1000})),
            }),
        })
        .await
        .unwrap_err();
    assert!(err_thinking.contains("thinking"));

    // Reject unsupported tool in whitelist
    let err_tool = adapter
        .delegate(DelegateParams {
            session: "rej-3".into(),
            goal: "g".into(),
            prompt: "p".into(),
            cwd: None,
            mode: DelegationMode::Async,
            run_timeout_ms: None,
            wait_timeout_ms: None,
            read_only: Some(true),
            constraints: Some(DelegationConstraints {
                tools: Some(vec!["unknown_tool".into()]),
                no_skills: None,
                thinking: None,
            }),
        })
        .await
        .unwrap_err();
    assert!(err_tool.contains("Unsupported tool 'unknown_tool'"));

    // Accept valid tools whitelist
    let ok_tool = adapter
        .delegate(DelegateParams {
            session: "ok-1".into(),
            goal: "g".into(),
            prompt: "p".into(),
            cwd: None,
            mode: DelegationMode::Sync,
            run_timeout_ms: None,
            wait_timeout_ms: None,
            read_only: Some(true),
            constraints: Some(DelegationConstraints {
                tools: Some(vec!["workspace".into(), "read".into()]),
                no_skills: Some(false),
                thinking: None,
            }),
        })
        .await;
    assert!(ok_tool.is_ok());
}
