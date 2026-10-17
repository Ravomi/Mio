//! Real loopback scope-ownership regressions.
//!
//! A real engine parent run delegates a real child run (the engine's own
//! `SubagentManager`, not a fake executor). The child's real engine run UUID is
//! bound to the parent journal run through the public
//! [`Journal::record_run_observation`] API, and its real tool-call lifecycle is
//! forwarded to [`Journal::record_canonical_phase`]. The two cases cover a child
//! that is already `Dispatched` when the parent finalizes, and a child whose
//! first tool starts only after the parent final, with a second parent turn
//! active while the child completes.

#[path = "../src/test_http.rs"]
mod test_http;

use mio_coding_agent::{
    AgentEvent, Config, Host, HostFuture, Message, Provider, RunObservation, RunOptions, RunTools,
    Services, ToolFilter,
    journal::{Journal, RunState, journal_path},
    run_with_tools,
    subagent::SubagentManager,
    tool_lifecycle::{ToolCallPhase, ToolCallRecord},
};
use serde_json::{Value, json};
use std::{
    io::Write,
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

struct JournalHost {
    journal: Arc<Journal>,
}

impl Host for JournalHost {
    fn shell(&self, command: &str, cwd: &Path) -> HostFuture<Result<String, String>> {
        let command = command.to_owned();
        let cwd = cwd.to_path_buf();
        Box::pin(async move {
            let output = tokio::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(&command)
                .current_dir(&cwd)
                .output()
                .await
                .map_err(|error| error.to_string())?;
            if output.status.success() {
                Ok(String::from_utf8_lossy(&output.stdout).into_owned())
            } else {
                Err(String::from_utf8_lossy(&output.stderr).into_owned())
            }
        })
    }

    fn on_event(&self, _event: AgentEvent) {}

    fn session_id(&self) -> Option<String> {
        Some("child-scope-runtime".into())
    }

    fn on_run_observation(&self, observation: &RunObservation) {
        // A host adapter reports a failed scope binding instead of aborting a
        // detached worker; the next canonical write re-enforces it and fails
        // closed rather than routing to the wrong run.
        if let Err(error) = self.journal.record_run_observation(observation) {
            eprintln!("child scope runtime: failed to record run scope: {error}");
        }
    }

    fn on_tool_call_lifecycle(&self, call: &ToolCallRecord) -> Result<(), String> {
        self.journal.record_canonical_phase(call)
    }
}

struct MockServer {
    address: SocketAddr,
    requests: mpsc::UnboundedReceiver<(TcpStream, Value)>,
    stop: Arc<AtomicBool>,
}

impl MockServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let (tx, requests) = mpsc::unbounded_channel();
        std::thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(10)))
                            .unwrap();
                        let (_header, request) = test_http::read_json(&mut stream);
                        if tx.send((stream, request)).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            address,
            requests,
            stop,
        }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn config(address: SocketAddr) -> Config {
    Config {
        provider: Provider::Custom,
        base_url: format!("http://{address}/v1"),
        model: "local".into(),
        api_key: "local-test".into(),
    }
}

fn reply_delta(mut stream: TcpStream, delta: Value, reason: &str) {
    let chunk = |delta: Value, finish: Value| {
        json!({
            "id":"test","object":"chat.completion.chunk","created":0,"model":"local",
            "choices":[{"index":0,"delta":delta,"finish_reason":finish}]
        })
        .to_string()
    };
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(delta, Value::Null),
        chunk(json!({}), json!(reason))
    );
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
}

fn stop_reply(stream: TcpStream) {
    reply_delta(stream, json!({"role":"assistant","content":"ok"}), "stop");
}

fn tool_reply(stream: TcpStream, id: &str, name: &str, arguments: &Value) {
    reply_delta(
        stream,
        json!({
            "role":"assistant",
            "tool_calls":[{
                "index":0,
                "id":id,
                "type":"function",
                "function":{"name":name,"arguments":arguments.to_string()}
            }]
        }),
        "tool_calls",
    );
}

fn delegate_args(child_prompt: &str) -> Value {
    json!({
        "operation":"delegate",
        "task":{
            "session":"child-session",
            "goal":"child goal",
            "prompt":child_prompt,
            "read_only":false
        }
    })
}

fn is_child_request(request: &Value) -> bool {
    request["messages"]
        .as_array()
        .map(|messages| {
            messages.iter().any(|message| {
                message["role"] == "user" && message["content"].as_str() == Some("child-prompt")
            })
        })
        .unwrap_or(false)
}

async fn recv(mock: &mut MockServer) -> (TcpStream, Value) {
    tokio::time::timeout(Duration::from_secs(15), mock.requests.recv())
        .await
        .expect("model request deadline")
        .expect("mock server open")
}

fn spawn_parent(
    config: Config,
    root: PathBuf,
    prompt: &str,
    host: Arc<JournalHost>,
    manager: Arc<SubagentManager>,
) -> tokio::task::JoinHandle<Result<Vec<Message>, String>> {
    let prompt = prompt.to_owned();
    tokio::spawn(async move {
        let tools = RunTools {
            options: RunOptions {
                max_turns: 4,
                ..RunOptions::default()
            },
            read_only: false,
            subagents: Some(manager),
            tool_filter: ToolFilter::all(),
            extra_tools: Vec::new(),
            policy_bound_tools: None,
        };
        run_with_tools(
            config,
            Vec::new(),
            &prompt,
            &root,
            host,
            Services {
                accounts: None,
                antigravity: None,
                deepseek: None,
            },
            tools,
        )
        .await
    })
}

fn stored_runs(root: &Path, session: &str) -> Vec<Value> {
    let path = journal_path(root, session);
    let value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["runs"].as_array().unwrap().clone()
}

fn call_phase(run: &Value, tool_call_id: &str) -> Option<String> {
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

async fn wait_phase(journal: &Journal, tool_call_id: &str, phase: ToolCallPhase) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let observed = journal.recovery_report().and_then(|report| {
            report
                .calls
                .iter()
                .find(|call| call.tool_call_id.as_deref() == Some(tool_call_id))
                .map(|call| call.phase)
        });
        if observed == Some(phase) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "call {tool_call_id} never reached {phase:?}: {observed:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Wait until no delegated child run is still active, so a test never drops its
/// tempdir while a child is still emitting real scope observations.
async fn wait_children_settled(manager: &SubagentManager) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if manager.list().iter().all(|run| !run.status.is_active()) {
            // One more yield lets the child's terminal observation reach the
            // journal even after the manager reports a terminal status.
            tokio::task::yield_now().await;
            return;
        }
        assert!(
            Instant::now() < deadline,
            "delegated child never settled: {:?}",
            manager.list()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Case 1: the child's first tool is already `Dispatched` when the parent sends
/// its final, and the child's real `Executed` result arrives while a second
/// parent turn is active.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_dispatched_before_parent_final_lands_in_the_original_run() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut mock = MockServer::start();
    let config = config(mock.address);
    let journal = Arc::new(Journal::open(&root, "scope-live-child"));
    let host = Arc::new(JournalHost {
        journal: journal.clone(),
    });
    let manager =
        Arc::new(SubagentManager::new(&root, config.clone(), RunOptions::default()).unwrap());

    journal
        .begin_run_with_task(Some("parent one"), Some("task-parent-1"))
        .unwrap();
    let parent_one = spawn_parent(
        config.clone(),
        root.clone(),
        "parent-one",
        host.clone(),
        manager.clone(),
    );

    let (sock, request) = recv(&mut mock).await;
    assert!(!is_child_request(&request), "expected the parent request");
    tool_reply(
        sock,
        "parent-delegate",
        "subagent",
        &delegate_args("child-prompt"),
    );

    let mut child_first: Option<TcpStream> = None;
    let mut parent_cont: Option<TcpStream> = None;
    while child_first.is_none() || parent_cont.is_none() {
        let (sock, request) = recv(&mut mock).await;
        if is_child_request(&request) {
            child_first = Some(sock);
        } else {
            parent_cont = Some(sock);
        }
    }

    // Dispatch the child's blocking tool first; it stays `Dispatched` until the
    // test releases it.
    tool_reply(
        child_first.take().unwrap(),
        "child-call-1",
        "bash",
        &json!({"command":"while [ ! -f .gate ]; do sleep 0.05; done; echo released"}),
    );
    wait_phase(&journal, "child-call-1", ToolCallPhase::Dispatched).await;

    // Now the parent really finalizes, while the child is live.
    stop_reply(parent_cont.take().unwrap());
    parent_one.await.unwrap().unwrap();
    journal.finish_run(RunState::Completed).unwrap();

    let report = journal.recovery_report().unwrap();
    let live = report
        .calls
        .iter()
        .find(|call| call.tool_call_id.as_deref() == Some("child-call-1"))
        .expect("the live child call must be recorded");
    assert_eq!(
        live.phase,
        ToolCallPhase::Dispatched,
        "the parent final must not fabricate Unknown for a live child: {report:?}"
    );

    // Second parent turn opens and stays active while the child completes.
    journal
        .begin_run_with_task(Some("parent two"), Some("task-parent-2"))
        .unwrap();
    let parent_two = spawn_parent(
        config.clone(),
        root.clone(),
        "parent-two",
        host.clone(),
        manager.clone(),
    );
    let (sock, request) = recv(&mut mock).await;
    assert!(
        !is_child_request(&request),
        "expected the second parent request"
    );
    stop_reply(sock);
    parent_two.await.unwrap().unwrap();

    // Release the child; its real result must land in the original run.
    std::fs::write(root.join(".gate"), b"go").unwrap();
    let (sock, request) = recv(&mut mock).await;
    assert!(
        is_child_request(&request),
        "expected the child's next request"
    );
    stop_reply(sock);
    wait_phase(&journal, "child-call-1", ToolCallPhase::Executed).await;
    wait_children_settled(&manager).await;

    let runs = stored_runs(&root, "scope-live-child");
    let owner = runs
        .iter()
        .find(|run| call_phase(run, "child-call-1").is_some())
        .expect("the child call must have an owning run");
    assert_eq!(
        owner["task_id"], "task-parent-1",
        "the child belongs to the first parent run: {owner}"
    );
    assert_eq!(
        call_phase(owner, "child-call-1").as_deref(),
        Some("executed")
    );
    let second = runs
        .iter()
        .find(|run| run["task_id"] == "task-parent-2")
        .expect("the second parent run exists");
    assert!(
        call_phase(second, "child-call-1").is_none(),
        "the new active run must not receive the old child's call: {second}"
    );
    assert_eq!(
        second["state"], "active",
        "the second turn was still active when the child completed: {second}"
    );
}

/// Case 2: the parent finalizes before the child's first tool even starts, and a
/// second parent turn is active when the child's result arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_first_tool_after_parent_final_lands_in_the_original_run() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("child.txt"), "child data").unwrap();
    let mut mock = MockServer::start();
    let config = config(mock.address);
    let journal = Arc::new(Journal::open(&root, "scope-late-child"));
    let host = Arc::new(JournalHost {
        journal: journal.clone(),
    });
    let manager =
        Arc::new(SubagentManager::new(&root, config.clone(), RunOptions::default()).unwrap());

    journal
        .begin_run_with_task(Some("parent one"), Some("task-parent-1"))
        .unwrap();
    let parent_one = spawn_parent(
        config.clone(),
        root.clone(),
        "parent-one",
        host.clone(),
        manager.clone(),
    );
    let (sock, request) = recv(&mut mock).await;
    assert!(!is_child_request(&request));
    tool_reply(
        sock,
        "parent-delegate",
        "subagent",
        &delegate_args("child-prompt"),
    );

    // Hold the child's first model request so its first tool cannot start yet.
    let mut child_first: Option<TcpStream> = None;
    let mut parent_cont: Option<TcpStream> = None;
    while child_first.is_none() || parent_cont.is_none() {
        let (sock, request) = recv(&mut mock).await;
        if is_child_request(&request) {
            child_first = Some(sock);
        } else {
            parent_cont = Some(sock);
        }
    }

    // Parent one finalizes before the child's first tool starts.
    stop_reply(parent_cont.take().unwrap());
    parent_one.await.unwrap().unwrap();
    journal.finish_run(RunState::Completed).unwrap();

    // Second parent turn opens and stays active.
    journal
        .begin_run_with_task(Some("parent two"), Some("task-parent-2"))
        .unwrap();
    let parent_two = spawn_parent(
        config.clone(),
        root.clone(),
        "parent-two",
        host.clone(),
        manager.clone(),
    );
    let (sock, request) = recv(&mut mock).await;
    assert!(!is_child_request(&request));
    stop_reply(sock);
    parent_two.await.unwrap().unwrap();

    // Only now does the child's first tool start.
    tool_reply(
        child_first.take().unwrap(),
        "child-call-late",
        "read",
        &json!({"path":"child.txt"}),
    );
    let (sock, request) = recv(&mut mock).await;
    assert!(
        is_child_request(&request),
        "expected the child's next request"
    );
    stop_reply(sock);
    wait_phase(&journal, "child-call-late", ToolCallPhase::Executed).await;
    wait_children_settled(&manager).await;

    let runs = stored_runs(&root, "scope-late-child");
    let owner = runs
        .iter()
        .find(|run| call_phase(run, "child-call-late").is_some())
        .expect("the late child call must be recorded");
    assert_eq!(
        owner["task_id"], "task-parent-1",
        "the late child still belongs to the original parent run: {owner}"
    );
    assert_eq!(
        call_phase(owner, "child-call-late").as_deref(),
        Some("executed")
    );
    let second = runs
        .iter()
        .find(|run| run["task_id"] == "task-parent-2")
        .expect("the second parent run exists");
    assert!(
        call_phase(second, "child-call-late").is_none(),
        "the new active run must not receive the child's call: {second}"
    );
    assert_eq!(second["state"], "active");
}
