#[path = "../src/test_http.rs"]
mod http;
use mio_coding_agent::{
    AgentEvent, Arguments, Config, Host, HostFuture, Message, RunOptions, Workspace,
    subagent::{Delegate, Executor, Status, SubagentManager, Task},
};
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct TestHost(Mutex<Vec<AgentEvent>>);
impl Host for TestHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { panic!("Read-only task reached shell host") })
    }
    fn on_event(&self, event: AgentEvent) {
        self.0.lock().unwrap().push(event);
    }
}
struct Fake(Arc<AtomicUsize>);
struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Executor for Fake {
    fn execute(&self, mut task: Task) -> HostFuture<Result<Vec<Message>, String>> {
        let active = self.0.clone();
        Box::pin(async move {
            active.fetch_add(1, Ordering::SeqCst);
            let _active = Active(active);
            match task.prompt.as_str() {
                "hold" => std::future::pending::<()>().await,
                "fail" => return Err("intentional failure".into()),
                "panic" => panic!("intentional panic"),
                "large" => task.host.on_event(AgentEvent::Text("汉".repeat(40000))),
                _ => task
                    .host
                    .on_event(AgentEvent::Text(format!("prior={}", task.history.len()))),
            }
            task.history.push(Message::user(task.prompt));
            Ok(task.history)
        })
    }
}
fn request(name: &str, prompt: &str) -> Delegate {
    Delegate {
        session: name.into(),
        goal: "Review".into(),
        prompt: prompt.into(),
        path: None,
        read_only: true,
    }
}
fn manager(
    root: &Path,
    limit: usize,
    timeout: Duration,
    active: Arc<AtomicUsize>,
) -> SubagentManager {
    SubagentManager::with_executor(
        root,
        Config {
            model: "test".into(),
            api_key: "secret-not-in-snapshot".into(),
            ..Config::default()
        },
        RunOptions::default(),
        limit,
        timeout,
        Arc::new(Fake(active)),
    )
    .unwrap()
}
async fn wait_active(active: &AtomicUsize, count: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while active.load(Ordering::SeqCst) != count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn resumes_forks_and_preserves_last_success_after_failure() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(
        dir.path(),
        4,
        Duration::from_secs(10),
        Arc::new(AtomicUsize::new(0)),
    );
    let host = Arc::new(TestHost::default());
    let first = manager
        .delegate(request("worker", "first"), host.clone())
        .unwrap();
    let first = manager
        .status(first.run_id, Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(first.status, Status::Completed);
    assert_eq!(first.output, "prior=0");
    let failed = manager
        .delegate(request("worker", "fail"), host.clone())
        .unwrap();
    assert_eq!(
        manager
            .status(failed.run_id, Duration::from_secs(1))
            .await
            .unwrap()
            .status,
        Status::Failed
    );
    manager.fork("worker", "branch").unwrap();
    for name in ["worker", "branch"] {
        let run = manager
            .delegate(request(name, "next"), host.clone())
            .unwrap();
        let done = manager
            .status(run.run_id, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(done.output, "prior=1");
        assert_ne!(done.run_id, first.run_id);
    }
    assert_eq!(manager.snapshot(first.run_id).unwrap().output, "prior=0");
    assert!(
        host.0.lock().unwrap().is_empty(),
        "Child text must not leak into parent stream"
    );
    assert!(
        !serde_json::to_string(&manager.list())
            .unwrap()
            .contains("secret-not-in-snapshot")
    );
}

#[tokio::test]
async fn cancellation_releases_once_and_old_run_cannot_cancel_new_run() {
    let dir = tempfile::tempdir().unwrap();
    let active = Arc::new(AtomicUsize::new(0));
    let manager = manager(dir.path(), 2, Duration::from_secs(10), active.clone());
    let host = Arc::new(TestHost::default());
    let old = manager
        .delegate(request("worker", "hold"), host.clone())
        .unwrap();
    wait_active(&active, 1).await;
    assert!(
        manager
            .delegate(request("worker", "hold"), host.clone())
            .is_err()
    );
    assert!(manager.fork("worker", "fork").is_err());
    assert_eq!(
        manager.kill(old.run_id).await.unwrap().status,
        Status::Cancelled
    );
    assert_eq!(active.load(Ordering::SeqCst), 0);
    let new = manager
        .delegate(request("worker", "hold"), host.clone())
        .unwrap();
    manager.kill(old.run_id).await.unwrap();
    assert_eq!(
        manager.snapshot(new.run_id).unwrap().status,
        Status::Running
    );
    let second = manager
        .delegate(request("second", "hold"), host.clone())
        .unwrap();
    assert!(manager.delegate(request("third", "hold"), host).is_err());
    manager.kill(new.run_id).await.unwrap();
    manager.kill(second.run_id).await.unwrap();
}

#[tokio::test]
async fn timeout_panic_and_manager_drop_release_futures() {
    let dir = tempfile::tempdir().unwrap();
    let active = Arc::new(AtomicUsize::new(0));
    let host = Arc::new(TestHost::default());
    let manager = manager(dir.path(), 1, Duration::from_millis(20), active.clone());
    let run = manager
        .delegate(request("timeout", "hold"), host.clone())
        .unwrap();
    assert_eq!(
        manager
            .status(run.run_id, Duration::from_secs(1))
            .await
            .unwrap()
            .status,
        Status::TimedOut
    );
    assert_eq!(active.load(Ordering::SeqCst), 0);
    let run = manager
        .delegate(request("panic", "panic"), host.clone())
        .unwrap();
    assert_eq!(
        manager
            .status(run.run_id, Duration::from_secs(1))
            .await
            .unwrap()
            .status,
        Status::Failed
    );
    let run = manager.delegate(request("drop", "hold"), host).unwrap();
    wait_active(&active, 1).await;
    assert_eq!(run.status, Status::Running);
    drop(manager);
    wait_active(&active, 0).await;
}

#[tokio::test]
async fn paths_modes_and_overlapping_workspace_leases_are_enforced() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("child")).unwrap();
    let manager = manager(
        dir.path(),
        4,
        Duration::from_secs(10),
        Arc::new(AtomicUsize::new(0)),
    );
    let host = Arc::new(TestHost::default());
    let mut invalid = request("escape", "hold");
    invalid.path = Some("..".into());
    assert!(manager.delegate(invalid, host.clone()).is_err());
    let read = manager
        .delegate(request("reader", "hold"), host.clone())
        .unwrap();
    let mut write = request("writer", "hold");
    write.read_only = false;
    write.path = Some("child".into());
    assert!(manager.delegate(write.clone(), host.clone()).is_err());
    manager.kill(read.run_id).await.unwrap();
    let run = manager.delegate(write, host.clone()).unwrap();
    assert!(
        manager
            .delegate(request("reader2", "hold"), host.clone())
            .is_err()
    );
    manager.kill(run.run_id).await.unwrap();
    let mut changed = request("writer", "next");
    changed.path = Some("child".into());
    assert!(
        manager.delegate(changed, host.clone()).is_err(),
        "Cannot silently change access mode"
    );
    let ws = Workspace::new_read_only(dir.path(), host).unwrap();
    assert!(ws.execute(Arguments::write("bad", "no")).await.is_err());
    assert!(ws.execute(Arguments::shell("touch bad")).await.is_err());
    let edit = serde_json::from_value(
        serde_json::json!({"operation":"edit","path":"bad","old_text":"a","new_text":"b"}),
    )
    .unwrap();
    assert!(ws.execute(edit).await.is_err());
    assert!(!dir.path().join("bad").exists());
}

#[tokio::test]
async fn output_and_polling_are_bounded_and_completed_status_is_immediate() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(
        dir.path(),
        1,
        Duration::from_secs(10),
        Arc::new(AtomicUsize::new(0)),
    );
    let run = manager
        .delegate(request("large", "large"), Arc::new(TestHost::default()))
        .unwrap();
    let done = manager
        .status(run.run_id, Duration::from_secs(1))
        .await
        .unwrap();
    assert!(done.output.len() <= 32768);
    assert!(done.output_truncated);
    let repeated = tokio::time::timeout(
        Duration::from_millis(100),
        manager.status(run.run_id, Duration::MAX),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(repeated.status, Status::Completed);
}

struct ApprovalHost {
    allow: bool,
    calls: AtomicUsize,
}
impl Host for ApprovalHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok("executed".into()) })
    }
    fn on_event(&self, _: AgentEvent) {}
    fn approve_tool(&self, _: &str, _: &str) -> Result<(), String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.allow {
            Ok(())
        } else {
            Err("denied by parent".into())
        }
    }
}
struct WriteExecutor;
impl Executor for WriteExecutor {
    fn execute(&self, task: Task) -> HostFuture<Result<Vec<Message>, String>> {
        Box::pin(async move {
            let workspace = Workspace::new(&task.cwd, task.host)?;
            let args = serde_json::from_value(
                serde_json::json!({"operation":"write","path":"child.txt","value":"child"}),
            )
            .unwrap();
            workspace.execute(args).await?;
            Ok(vec![Message::assistant("written")])
        })
    }
}
#[tokio::test]
async fn writable_child_obeys_parent_approval() {
    for allow in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let host = Arc::new(ApprovalHost {
            allow,
            calls: AtomicUsize::new(0),
        });
        let manager = SubagentManager::with_executor(
            dir.path(),
            Config {
                model: "test".into(),
                ..Config::default()
            },
            RunOptions::default(),
            1,
            Duration::from_secs(5),
            Arc::new(WriteExecutor),
        )
        .unwrap();
        let mut task = request("writer", "write");
        task.read_only = false;
        let run = manager.delegate(task, host.clone()).unwrap();
        let result = manager
            .status(run.run_id, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(
            result.status,
            if allow {
                Status::Completed
            } else {
                Status::Failed
            }
        );
        assert_eq!(host.calls.load(Ordering::SeqCst), 1);
        assert_eq!(dir.path().join("child.txt").exists(), allow);
        if !allow {
            assert!(result.error.unwrap().contains("denied by parent"));
        }
    }
}

#[tokio::test]
async fn model_reconfiguration_preserves_sessions_and_rejects_active_children() {
    use std::{io::Write, net::TcpListener};
    let dir = tempfile::tempdir().unwrap();
    let mut manager = manager(
        dir.path(),
        1,
        Duration::from_secs(10),
        Arc::new(AtomicUsize::new(0)),
    );
    let host = Arc::new(TestHost::default());
    let held = manager
        .delegate(request("held", "hold"), host.clone())
        .unwrap();
    assert!(
        manager
            .reconfigure(
                Config {
                    model: "next".into(),
                    ..Config::default()
                },
                RunOptions::default(),
                None
            )
            .is_err()
    );
    manager.kill(held.run_id).await.unwrap();
    let first = manager
        .delegate(request("worker", "first"), host.clone())
        .unwrap();
    assert_eq!(
        manager
            .status(first.run_id, Duration::from_secs(2))
            .await
            .unwrap()
            .status,
        Status::Completed
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            if let Ok((stream, _)) = listener.accept() {
                break stream;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (_, body) = http::read_json(&mut stream);
        assert_eq!(body["model"], "next");
        assert!(body["messages"].to_string().contains("first"));
        let chunk = serde_json::json!({"id":"t","object":"chat.completion.chunk","created":0,"model":"next","choices":[{"index":0,"delta":{"role":"assistant","content":"resumed"},"finish_reason":"stop"}]}).to_string();
        let response = format!("data: {chunk}\n\ndata: [DONE]\n\n");
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
    });
    manager
        .reconfigure(
            Config {
                model: "next".into(),
                base_url: format!("http://{address}/v1"),
                api_key: "local-test".into(),
                ..Config::default()
            },
            RunOptions::default(),
            None,
        )
        .unwrap();
    let resumed = manager.delegate(request("worker", "second"), host).unwrap();
    let finished = manager
        .status(resumed.run_id, Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(finished.status, Status::Completed, "{:?}", finished.error);
    assert_eq!(finished.output, "resumed");
    server.join().unwrap();
}
