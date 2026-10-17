//! DES-15 真实全链路关联 ID 的离线 loopback 验收。
//!
//! 每个用例都通过 `run_with_options` / `run_with_tools` 驱动真实 Agent 循环，
//! 模型请求由本地 loopback mock 服务应答；不使用真实 Provider 或密钥，也不构造
//! 虚假事件自证。断言只读取真实观察通道（`Host::on_run_observation`）、真实
//! completion/diagnostic 回调与真实审批绑定：
//!
//! - 同一 run + turn 上，模型派发、工具派发/结果与错误共享同一个真实 run UUID；
//! - 多个同名工具的 toolCallId 不同（来自模型的 `CallId`，不是工具名）；
//! - 子代理保留自己的 engine runId，以 subagentId 携带委派标识、parentRunId
//!   关联父运行，且审批 binding.scope 与子运行 runId 一致；
//! - 取消归属到被丢弃 future 的真实 run UUID，而不是由调用方猜测。

#[path = "../src/test_http.rs"]
mod test_http;

use mio_coding_agent::{
    AgentEvent, CompletionCallObservation, Config, DiagnosticCategory, Host, HostFuture, Provider,
    RunDiagnostic, RunObservation, RunObservationKind, RunObservationStatus, RunOptions, RunTools,
    Services, ToolFilter, policy::ApprovalBinding, run_with_options, run_with_tools,
    subagent::SubagentManager,
};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

/// 记录真实观察通道与审批绑定的宿主。
#[derive(Default)]
struct RecordingHost {
    observations: Mutex<Vec<RunObservation>>,
    completions: Mutex<Vec<CompletionCallObservation>>,
    diagnostics: Mutex<Vec<RunDiagnostic>>,
    bindings: Mutex<Vec<ApprovalBinding>>,
    events: Mutex<Vec<AgentEvent>>,
    session: Option<String>,
    /// When set, every approval is refused (the denial half of DES-15).
    refuse_approval: bool,
}

impl RecordingHost {
    fn with_session(session: &str) -> Self {
        Self {
            session: Some(session.to_owned()),
            ..Self::default()
        }
    }

    fn observations(&self) -> Vec<RunObservation> {
        self.observations.lock().unwrap().clone()
    }

    fn of_kind(&self, kind: RunObservationKind) -> Vec<RunObservation> {
        self.observations()
            .into_iter()
            .filter(|observation| observation.kind == kind)
            .collect()
    }
}

impl Host for RecordingHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Err("unexpected shell".into()) })
    }

    fn on_event(&self, event: AgentEvent) {
        self.events.lock().unwrap().push(event);
    }

    fn session_id(&self) -> Option<String> {
        self.session.clone()
    }

    fn on_run_observation(&self, observation: &RunObservation) {
        self.observations.lock().unwrap().push(observation.clone());
    }

    fn on_completion_call(&self, call: &CompletionCallObservation) {
        self.completions.lock().unwrap().push(call.clone());
    }

    fn on_diagnostic(&self, diagnostic: &RunDiagnostic) {
        self.diagnostics.lock().unwrap().push(diagnostic.clone());
    }

    fn approve_tool_binding(
        &self,
        _operation: &str,
        _target: &str,
        binding: &ApprovalBinding,
    ) -> HostFuture<Result<(), String>> {
        self.bindings.lock().unwrap().push(binding.clone());
        if self.refuse_approval {
            return Box::pin(async { Err("operator refused this call".into()) });
        }
        Box::pin(async { Ok(()) })
    }
}

/// 顺序服务的本地 mock：每个连接读一个模型请求，再由 handler 自行写响应。
struct MockServer {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn mock_server<F>(handler: F) -> MockServer
where
    F: Fn(&mut TcpStream, &Value) + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !thread_stop.load(Ordering::SeqCst) && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut socket, _)) => {
                    socket
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    socket
                        .set_write_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    let request = test_http::read_json(&mut socket).1;
                    handler(&mut socket, &request);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(_) => return,
            }
        }
    });
    MockServer { address, stop }
}

fn sse_chunk(delta: Value, finish: Value) -> String {
    json!({
        "id": "test",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "test",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    })
    .to_string()
}

fn write_sse(socket: &mut TcpStream, delta: Value, finish: Value) {
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        sse_chunk(delta, Value::Null),
        sse_chunk(json!({}), finish)
    );
    let _ = write!(
        socket,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.flush();
}

fn tool_call(index: usize, name: &str, id: &str, arguments: Value) -> Value {
    json!({
        "index": index,
        "id": id,
        "type": "function",
        "function": {"name": name, "arguments": arguments.to_string()},
    })
}

fn write_text(socket: &mut TcpStream, text: &str) {
    write_sse(
        socket,
        json!({"role": "assistant", "content": text}),
        json!("stop"),
    );
}

fn write_status(socket: &mut TcpStream, status: u16, body: &str) {
    let reason = match status {
        500 => "Internal Server Error",
        _ => "Mock",
    };
    let _ = write!(
        socket,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.flush();
}

fn config(address: SocketAddr) -> Config {
    Config {
        provider: Provider::Custom,
        base_url: format!("http://{address}/v1"),
        model: "test".into(),
        api_key: "local-test-credential".into(),
    }
}

async fn wait_for<F>(predicate: F)
where
    F: Fn() -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(Instant::now() < deadline, "observation deadline");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn parse_uuid(text: &str) -> uuid::Uuid {
    uuid::Uuid::parse_str(text).unwrap_or_else(|error| panic!("not a UUID `{text}`: {error}"))
}

/// 同名工具在同一个 turn 内派发，但必须得到不同的真实 toolCallId。
#[tokio::test]
async fn same_name_tools_keep_distinct_model_call_ids_on_one_real_run() {
    let mock = mock_server(|socket, request| {
        let turn = request["messages"]
            .as_array()
            .map(|messages| messages.iter().filter(|m| m["role"] == "assistant").count())
            .unwrap_or(0);
        if turn == 0 {
            let delta = json!({
                "role": "assistant",
                "tool_calls": [
                    tool_call(0, "read", "call-a", json!({"path": "a.txt"})),
                    tool_call(1, "read", "call-b", json!({"path": "b.txt"})),
                ],
            });
            write_sse(socket, delta, json!("tool_calls"));
        } else {
            write_text(socket, "done");
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    std::fs::write(cwd.join("a.txt"), "alpha").unwrap();
    std::fs::write(cwd.join("b.txt"), "beta").unwrap();
    let host = Arc::new(RecordingHost::with_session("session-real"));

    let history = run_with_options(
        config(mock.address),
        Vec::new(),
        "read both",
        &cwd,
        host.clone(),
        Services::default(),
        RunOptions::default(),
    )
    .await
    .unwrap();
    assert!(!history.is_empty());

    let starts = host.of_kind(RunObservationKind::RunStarted);
    assert_eq!(starts.len(), 1, "{starts:?}");
    let run_id = starts[0].run_id;
    assert_eq!(starts[0].subagent_id, None);
    assert_eq!(starts[0].parent_run_id, None);
    assert_eq!(starts[0].session_id.as_deref(), Some("session-real"));

    let models = host.of_kind(RunObservationKind::ModelDispatched);
    assert_eq!(
        models.iter().map(|o| o.turn_id).collect::<Vec<_>>(),
        vec![Some(1), Some(2)],
        "{models:?}"
    );
    assert!(models.iter().all(|o| o.run_id == run_id));

    let dispatches = host.of_kind(RunObservationKind::ToolDispatched);
    assert_eq!(dispatches.len(), 2, "{dispatches:?}");
    assert!(dispatches.iter().all(|o| o.turn_id == Some(1)));
    assert!(dispatches.iter().all(|o| o.run_id == run_id));
    let mut call_ids: Vec<String> = dispatches
        .iter()
        .filter_map(|o| o.tool_call_id.clone())
        .collect();
    call_ids.sort();
    assert_eq!(call_ids, vec!["call-a".to_owned(), "call-b".to_owned()]);
    // 同名工具的 effect id 也不同：不会因为工具名相同就串号。
    let mut effect_ids: Vec<u64> = dispatches.iter().filter_map(|o| o.effect_id).collect();
    effect_ids.sort_unstable();
    assert_eq!(effect_ids.len(), 2);
    assert_ne!(effect_ids[0], effect_ids[1]);

    let outcomes = host.of_kind(RunObservationKind::ToolOutcome);
    assert_eq!(outcomes.len(), 2, "{outcomes:?}");
    assert!(
        outcomes
            .iter()
            .all(|o| o.status == Some(RunObservationStatus::Success))
    );
    let mut outcome_ids: Vec<String> = outcomes
        .iter()
        .filter_map(|o| o.tool_call_id.clone())
        .collect();
    outcome_ids.sort();
    assert_eq!(outcome_ids, call_ids);

    let finishes = host.of_kind(RunObservationKind::RunFinished);
    assert_eq!(finishes.len(), 1);
    assert_eq!(finishes[0].run_id, run_id);
    assert_eq!(finishes[0].status, Some(RunObservationStatus::Success));

    let completions = host.completions.lock().unwrap().clone();
    assert_eq!(completions.len(), 2, "{completions:?}");
    assert!(completions.iter().all(|call| call.run_id == run_id));
    assert!(completions.iter().all(|call| call.agent.is_none()));
    assert!(host.diagnostics.lock().unwrap().is_empty());
}

/// 拒绝产生 Outcome 但绝不产生 Dispatched；拒绝不得污染下一个真实派发，
/// 且 run 终态只能在真实工具结果之后。
#[tokio::test]
async fn denial_keeps_its_outcome_without_polluting_the_next_real_dispatch() {
    let mock = mock_server(|socket, request| {
        let assistant_turns = request["messages"]
            .as_array()
            .map(|messages| {
                messages
                    .iter()
                    .filter(|message| message["role"] == "assistant")
                    .count()
            })
            .unwrap_or(0);
        match assistant_turns {
            0 => {
                let delta = json!({
                    "role": "assistant",
                    "tool_calls": [tool_call(
                        0,
                        "write",
                        "denied-write",
                        json!({"path": "denied.txt", "value": "x"}),
                    )],
                });
                write_sse(socket, delta, json!("tool_calls"));
            }
            1 => {
                let delta = json!({
                    "role": "assistant",
                    "tool_calls": [tool_call(
                        0,
                        "read",
                        "call-read",
                        json!({"path": "a.txt"}),
                    )],
                });
                write_sse(socket, delta, json!("tool_calls"));
            }
            _ => write_text(socket, "done"),
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    std::fs::write(cwd.join("a.txt"), "alpha").unwrap();
    let host = Arc::new(RecordingHost {
        refuse_approval: true,
        ..RecordingHost::with_session("session-denial")
    });

    let result = run_with_options(
        config(mock.address),
        Vec::new(),
        "try the write, then read",
        &cwd,
        host.clone(),
        Services::default(),
        RunOptions::default(),
    )
    .await;
    assert!(result.is_ok(), "{result:?}");
    assert!(
        !cwd.join("denied.txt").exists(),
        "a denied write must never run"
    );

    let observations = host.observations();
    let run_id = host.of_kind(RunObservationKind::RunStarted)[0].run_id;

    let denied: Vec<&RunObservation> = observations
        .iter()
        .filter(|observation| observation.tool_call_id.as_deref() == Some("denied-write"))
        .collect();
    let denied_prepared = denied
        .iter()
        .find(|observation| observation.kind == RunObservationKind::ToolPrepared)
        .expect("the denied call was prepared before approval");
    let denied_outcome = denied
        .iter()
        .find(|observation| observation.kind == RunObservationKind::ToolOutcome)
        .expect("a denial keeps a correlated outcome");
    assert_eq!(denied_outcome.status, Some(RunObservationStatus::Error));
    assert_eq!(denied_prepared.turn_id, Some(1));
    assert_eq!(denied_prepared.run_id, run_id);
    // The same real effect id is reused for the prepared and outcome records.
    assert_eq!(denied_prepared.effect_id, denied_outcome.effect_id);
    assert!(
        !denied
            .iter()
            .any(|observation| observation.kind == RunObservationKind::ToolDispatched),
        "a denied admission is not a committed dispatch: {denied:?}"
    );

    // The next tool really dispatches with its own identity, not the refusal's.
    let read: Vec<&RunObservation> = observations
        .iter()
        .filter(|observation| observation.tool_call_id.as_deref() == Some("call-read"))
        .collect();
    let read_dispatch = read
        .iter()
        .find(|observation| observation.kind == RunObservationKind::ToolDispatched)
        .expect("the read must really dispatch");
    assert_eq!(read_dispatch.turn_id, Some(2));
    assert_eq!(read_dispatch.run_id, run_id);
    assert_ne!(read_dispatch.effect_id, denied_prepared.effect_id);
    assert!(read.iter().any(|observation| {
        observation.kind == RunObservationKind::ToolOutcome
            && observation.status == Some(RunObservationStatus::Success)
    }));

    // The run's terminal state only comes after the real tool results, and it
    // is never an early success before them.
    let denied_outcome_index = observations
        .iter()
        .position(|observation| {
            observation.kind == RunObservationKind::ToolOutcome
                && observation.tool_call_id.as_deref() == Some("denied-write")
        })
        .unwrap();
    let run_finished_index = observations
        .iter()
        .rposition(|observation| observation.kind == RunObservationKind::RunFinished)
        .expect("the run reports a terminal state");
    assert!(
        denied_outcome_index < run_finished_index,
        "the run must not report a terminal state before its real tool result: {observations:?}"
    );
    let finishes = host.of_kind(RunObservationKind::RunFinished);
    assert_eq!(finishes.len(), 1);
    assert_eq!(finishes[0].run_id, run_id);
    assert_eq!(finishes[0].status, Some(RunObservationStatus::Success));
}

/// 供应商错误必须被定位到同一个真实 run 与 turn，而不是丢失关联。
#[tokio::test]
async fn provider_error_is_attributed_to_the_real_run_and_turn() {
    let mock = mock_server(|socket, _request| {
        write_status(
            socket,
            500,
            &json!({"error": {"message": "SECRET-BODY"}}).to_string(),
        );
    });
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let host = Arc::new(RecordingHost::with_session("session-error"));

    let result = run_with_options(
        config(mock.address),
        Vec::new(),
        "fail",
        &cwd,
        host.clone(),
        Services::default(),
        RunOptions::default(),
    )
    .await;
    assert!(result.is_err());

    let starts = host.of_kind(RunObservationKind::RunStarted);
    assert_eq!(starts.len(), 1);
    let run_id = starts[0].run_id;
    let models = host.of_kind(RunObservationKind::ModelDispatched);
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].turn_id, Some(1));
    assert_eq!(models[0].run_id, run_id);

    let finishes = host.of_kind(RunObservationKind::RunFinished);
    assert_eq!(finishes.len(), 1);
    assert_eq!(finishes[0].run_id, run_id);
    assert_eq!(finishes[0].status, Some(RunObservationStatus::Error));

    let diagnostics = host.diagnostics.lock().unwrap().clone();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].run_id, run_id);
    assert_eq!(diagnostics[0].category, DiagnosticCategory::Service);
    assert!(!diagnostics[0].message.contains("SECRET-BODY"));
}

/// 无限流：取消必须归属到被丢弃 future 的真实 run UUID。
#[tokio::test]
async fn cancellation_is_attributed_to_the_dropped_real_run() {
    let mock = mock_server(|socket, _request| {
        let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
        if socket.write_all(headers.as_bytes()).is_err() {
            return;
        }
        let text = "s".repeat(1024);
        loop {
            let frame_body = format!(
                "data: {}\n\n",
                sse_chunk(json!({"role": "assistant", "content": text}), Value::Null)
            );
            let frame = format!("{:x}\r\n{frame_body}\r\n", frame_body.len());
            if socket
                .write_all(frame.as_bytes())
                .and_then(|()| socket.flush())
                .is_err()
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let host = Arc::new(RecordingHost::with_session("session-cancel"));

    let mut run_fut = Box::pin(run_with_options(
        config(mock.address),
        Vec::new(),
        "stream forever",
        &cwd,
        host.clone(),
        Services::default(),
        RunOptions::default(),
    ));

    let started = {
        let host = host.clone();
        async move {
            wait_for(|| !host.of_kind(RunObservationKind::RunStarted).is_empty()).await;
        }
    };
    tokio::select! {
        _ = &mut run_fut => panic!("streaming run must not finish on its own"),
        _ = started => {}
    }

    let run_id = host.of_kind(RunObservationKind::RunStarted)[0].run_id;
    drop(run_fut);

    wait_for(|| !host.of_kind(RunObservationKind::RunFinished).is_empty()).await;
    let finishes = host.of_kind(RunObservationKind::RunFinished);
    assert_eq!(finishes.len(), 1, "{finishes:?}");
    assert_eq!(finishes[0].run_id, run_id);
    assert_eq!(finishes[0].status, Some(RunObservationStatus::Cancelled));
}

/// 真实父子运行：子代理保留自己的 engine runId，审批 scope 与之一致。
#[tokio::test]
async fn subagent_child_run_stays_isolated_and_links_the_parent_scope() {
    let mock = mock_server(|socket, request| {
        let is_parent = request["tools"]
            .as_array()
            .map(|tools| {
                tools
                    .iter()
                    .any(|tool| tool["function"]["name"] == "subagent")
            })
            .unwrap_or(false);
        if is_parent {
            let assistant_turns = request["messages"]
                .as_array()
                .map(|messages| messages.iter().filter(|m| m["role"] == "assistant").count())
                .unwrap_or(0);
            if assistant_turns == 0 {
                let delta = json!({
                    "role": "assistant",
                    "tool_calls": [tool_call(
                        0,
                        "subagent",
                        "parent-delegate",
                        json!({
                            "operation": "delegate",
                            "task": {
                                "session": "child",
                                "goal": "child goal",
                                "prompt": "child task",
                                "read_only": false,
                            },
                        }),
                    )],
                });
                write_sse(socket, delta, json!("tool_calls"));
                return;
            }
            let last_tool = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .rev()
                .find(|message| message["role"] == "tool")
                .map(|message| serde_json::from_str::<Value>(message["content"].as_str().unwrap()))
                .transpose()
                .unwrap();
            let completed = last_tool
                .as_ref()
                .and_then(|value| value["status"].as_str())
                .map(|status| status == "completed")
                .unwrap_or(false);
            if completed {
                write_text(socket, "parent done");
            } else {
                let run_id = last_tool
                    .as_ref()
                    .map(|value| value["run_id"].clone())
                    .unwrap_or(Value::Null);
                let delta = json!({
                    "role": "assistant",
                    "tool_calls": [tool_call(
                        0,
                        "subagent",
                        "parent-poll",
                        json!({"operation": "status", "run_id": run_id, "wait_ms": 10000}),
                    )],
                });
                write_sse(socket, delta, json!("tool_calls"));
            }
            return;
        }
        // 子代理：先写一个需要审批的文件，再结束。
        let child_turns = request["messages"]
            .as_array()
            .map(|messages| messages.iter().filter(|m| m["role"] == "assistant").count())
            .unwrap_or(0);
        if child_turns == 0 {
            let delta = json!({
                "role": "assistant",
                "tool_calls": [tool_call(
                    0,
                    "write",
                    "child-write",
                    json!({"path": "child.txt", "value": "written by child"}),
                )],
            });
            write_sse(socket, delta, json!("tool_calls"));
        } else {
            write_text(socket, "child done");
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let host = Arc::new(RecordingHost::with_session("session-subagent"));
    let manager =
        Arc::new(SubagentManager::new(&cwd, config(mock.address), RunOptions::default()).unwrap());

    let result = run_with_tools(
        config(mock.address),
        Vec::new(),
        "delegate a child task",
        &cwd,
        host.clone(),
        Services::default(),
        RunTools {
            subagents: Some(manager.clone()),
            tool_filter: ToolFilter::default(),
            ..RunTools::default()
        },
    )
    .await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        std::fs::read_to_string(cwd.join("child.txt")).unwrap(),
        "written by child"
    );

    let completions = host.completions.lock().unwrap().clone();
    let parent_run = completions
        .iter()
        .find(|call| call.agent.is_none())
        .expect("parent completion")
        .run_id;
    let child_calls: Vec<&CompletionCallObservation> = completions
        .iter()
        .filter(|call| call.agent.as_deref() == Some("child"))
        .collect();
    assert!(!child_calls.is_empty(), "{completions:?}");
    let child_run = child_calls[0].run_id;
    assert!(
        child_calls.iter().all(|call| call.run_id == child_run),
        "a child keeps one real engine run id: {child_calls:?}"
    );
    assert_ne!(child_run, parent_run);

    let starts = host.of_kind(RunObservationKind::RunStarted);
    let parent_start = starts
        .iter()
        .find(|observation| observation.subagent_id.is_none())
        .expect("parent RunStarted");
    assert_eq!(parent_start.run_id, parent_run);
    assert_eq!(parent_start.parent_run_id, None);
    let child_start = starts
        .iter()
        .find(|observation| observation.run_id == child_run)
        .expect("child RunStarted");
    assert_eq!(child_start.parent_run_id, Some(parent_run));
    let delegation_id = child_start
        .subagent_id
        .clone()
        .expect("child carries its delegation identity");
    parse_uuid(&delegation_id);
    assert_ne!(delegation_id, child_run.to_string());

    let child_dispatch = host
        .of_kind(RunObservationKind::ToolDispatched)
        .into_iter()
        .find(|observation| observation.tool_call_id.as_deref() == Some("child-write"))
        .expect("child write dispatch");
    assert_eq!(child_dispatch.run_id, child_run);

    let bindings = host.bindings.lock().unwrap().clone();
    let write_binding = bindings
        .iter()
        .find(|binding| binding.tool == "write")
        .expect("child write approval binding");
    // 审批 scope 就是子运行的真实 engine run UUID，与它自己的 runId 一致。
    assert_eq!(write_binding.scope, child_run.to_string());
    assert_eq!(write_binding.tool_call_id.as_deref(), Some("child-write"));
    // 审批尝试的 callId 独立存在，不冒充模型的 toolCallId。
    assert_ne!(write_binding.call_id, "child-write");
    assert_eq!(
        host.bindings
            .lock()
            .unwrap()
            .iter()
            .filter(|binding| binding.scope == parent_run.to_string())
            .count(),
        0,
        "the child's approval must not be scoped to the parent run"
    );
}

/// 真实 RPC + loopback 集成：协商 v1 后，模型/工具/审批/错误共享同一真实
/// run+turn，同名工具 toolCallId 不同，审批 callId 不冒充模型 toolCallId。
struct RpcClient {
    child: Child,
    events: mpsc::Receiver<Value>,
    _dir: tempfile::TempDir,
}

impl Drop for RpcClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl RpcClient {
    fn start(address: SocketAddr) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_mio"));
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if name.starts_with("MIO_")
                || name.contains("API_KEY")
                || name.to_lowercase().contains("proxy")
            {
                command.env_remove(key);
            }
        }
        let mut child = command
            .current_dir(dir.path())
            .env("HOME", dir.path())
            .env("MIO_CONFIG_DIR", dir.path())
            .env("MIO_API_KEY", "local-test-credential")
            .args([
                "--mode",
                "rpc",
                "--no-context",
                "--no-mcp",
                "--no-session",
                "--approve",
                "--provider",
                "custom",
                "--model",
                "test",
                "--base-url",
                &format!("http://{address}/v1"),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                let event: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|error| panic!("non-JSON RPC line ({error}): {line}"));
                if tx.send(event).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            events: rx,
            _dir: dir,
        }
    }

    fn send(&mut self, value: Value) {
        let stdin = self.child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{value}").unwrap();
        stdin.flush().unwrap();
    }

    fn collect_until(&self, predicate: impl Fn(&Value) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut seen = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "RPC event deadline: {seen:?}");
            let event = self
                .events
                .recv_timeout(remaining)
                .unwrap_or_else(|_| panic!("RPC event deadline: {seen:?}"));
            let matched = predicate(&event);
            seen.push(event);
            if matched {
                return seen;
            }
        }
    }

    fn response(&self, id: &str) -> Value {
        self.collect_until(|event| event["id"] == id).pop().unwrap()
    }

    fn close_stdin(&mut self) {
        self.child.stdin.take();
    }
}

#[test]
fn legacy_rpc_exposes_post_admission_model_dispatch_counts() {
    let turn = Mutex::new(0usize);
    let mock = mock_server(move |socket, _request| {
        let mut turn = turn.lock().unwrap();
        if *turn == 0 {
            write_sse(
                socket,
                json!({"role":"assistant", "tool_calls":[
                    tool_call(0, "read", "read-for-count", json!({"path":"a.txt"}))
                ]}),
                json!("tool_calls"),
            );
        } else {
            write_text(socket, "done");
        }
        *turn += 1;
    });
    let mut client = RpcClient::start(mock.address);
    std::fs::write(
        client._dir.path().join("a.txt"),
        "READ-SECRET-FIRST\nREAD-SECRET-LAST",
    )
    .unwrap();
    client.send(json!({"type":"prompt", "id":"count", "message":"read a.txt"}));
    assert_eq!(client.response("count")["success"], true);
    let seen = client
        .collect_until(|event| event["type"] == "agent_settled" && event["status"] == "completed");
    let dispatches = seen
        .iter()
        .filter(|event| {
            event["type"] == "run_observation" && event["data"]["kind"] == "model_dispatched"
        })
        .collect::<Vec<_>>();
    assert_eq!(dispatches.len(), 2, "{seen:?}");
    uuid::Uuid::parse_str(dispatches[0]["data"]["runId"].as_str().unwrap()).unwrap();
    assert!(dispatches[0]["data"]["effectId"].as_u64().is_some());
    assert!(dispatches[1]["data"]["effectId"].as_u64().is_some());
    assert_eq!(
        dispatches[0]["data"]["runId"],
        dispatches[1]["data"]["runId"]
    );
    assert_ne!(
        dispatches[0]["data"]["effectId"],
        dispatches[1]["data"]["effectId"]
    );
    assert!(!seen.iter().any(|event| {
        event["type"] == "run_observation" && event["data"]["kind"] == "model_prepared"
    }));
    let reads = seen
        .iter()
        .filter(|event| event["type"] == "read_observation")
        .collect::<Vec<_>>();
    assert_eq!(reads.len(), 1, "{seen:?}");
    let read = reads[0];
    assert_eq!(read["data"]["path"], "a.txt");
    assert_eq!(read["data"]["offset"], 1);
    assert_eq!(read["data"]["limit"], 2);
    assert_eq!(read["data"]["total"], 2);
    assert_eq!(read["data"]["truncated"], false);
    assert_eq!(read["data"]["runId"], dispatches[0]["data"]["runId"]);
    assert_eq!(read["data"]["toolCallId"], "read-for-count");
    assert!(!read.to_string().contains("READ-SECRET"));
    assert!(
        !read
            .to_string()
            .contains(client._dir.path().to_str().unwrap())
    );
    client.close_stdin();
}

#[test]
fn rpc_v1_envelope_correlates_model_tool_approval_and_error_on_one_real_run() {
    let turn = Mutex::new(0usize);
    let mock = mock_server(move |socket, _request| {
        let mut turn = turn.lock().unwrap();
        match *turn {
            0 => {
                let delta = json!({
                    "role": "assistant",
                    "tool_calls": [
                        tool_call(0, "read", "call-read-a", json!({"path": "a.txt"})),
                        tool_call(1, "read", "call-read-b", json!({"path": "b.txt"})),
                    ],
                });
                write_sse(socket, delta, json!("tool_calls"));
            }
            1 => {
                let delta = json!({
                    "role": "assistant",
                    "tool_calls": [tool_call(
                        0,
                        "write",
                        "call-write",
                        json!({"path": "out.txt", "value": "x"}),
                    )],
                });
                write_sse(socket, delta, json!("tool_calls"));
            }
            _ => write_text(socket, "done"),
        }
        *turn += 1;
    });
    let mut client = RpcClient::start(mock.address);

    client.send(json!({"type": "initialize", "id": "i1", "schema_versions": [1]}));
    let negotiated = client.response("i1");
    assert_eq!(negotiated["success"], true, "{negotiated}");
    assert_eq!(negotiated["data"]["envelope"], "v1");

    client.send(json!({"type": "prompt", "id": "p1", "message": "go"}));
    assert_eq!(client.response("p1")["success"], true);

    let mut seen = Vec::new();
    let mut approved = false;
    loop {
        let batch = client.collect_until(|event| {
            event["type"] == "approval_request"
                || (event["type"] == "agent_settled" && event["status"] == "completed")
        });
        if let Some(request) = batch
            .iter()
            .find(|event| event["type"] == "approval_request")
        {
            // 审批绑定携带真实 runId（scope）与模型 toolCallId，callId 独立。
            let binding = &request["binding"];
            assert_eq!(binding["toolCallId"], json!("call-write"), "{request}");
            assert_ne!(binding["callId"], json!("call-write"), "{request}");
            let id = request["request_id"].as_str().unwrap();
            client.send(json!({"type": "approve_response", "request_id": id, "approved": true}));
            approved = true;
        }
        seen.extend(batch);
        if seen
            .iter()
            .any(|event| event["type"] == "agent_settled" && event["status"] == "completed")
        {
            break;
        }
    }
    assert!(approved, "the write must have required approval");

    let run_id = seen
        .iter()
        .find(|event| event["type"] == "run_observation" && event["data"]["kind"] == "run_started")
        .map(|event| event["runId"].clone())
        .expect("v1 run_started observation");
    assert!(run_id.is_string(), "{run_id}");
    let run_id_text = run_id.as_str().unwrap().to_owned();

    let completions: Vec<&Value> = seen
        .iter()
        .filter(|event| event["type"] == "completion_call")
        .collect();
    assert_eq!(completions.len(), 3, "{seen:?}");
    assert!(completions.iter().all(|event| event["runId"] == run_id));
    assert_eq!(
        completions
            .iter()
            .map(|event| event["turnId"].as_u64())
            .collect::<Vec<_>>(),
        vec![Some(1), Some(2), Some(3)],
        "{completions:?}"
    );

    let read_starts: Vec<&Value> = seen
        .iter()
        .filter(|event| event["type"] == "tool_started" && event["data"]["operation"] == "read")
        .collect();
    assert_eq!(read_starts.len(), 2, "{seen:?}");
    let mut read_ids: Vec<String> = read_starts
        .iter()
        .map(|event| event["toolCallId"].as_str().unwrap().to_owned())
        .collect();
    read_ids.sort();
    assert_eq!(
        read_ids,
        vec!["call-read-a".to_owned(), "call-read-b".to_owned()]
    );
    assert!(read_starts.iter().all(|event| event["runId"] == run_id));
    assert!(read_starts.iter().all(|event| event["turnId"] == json!(1)));

    let approval = seen
        .iter()
        .find(|event| event["type"] == "approval_request")
        .expect("approval_request");
    assert_eq!(approval["runId"], run_id);
    assert_eq!(approval["toolCallId"], json!("call-write"));
    assert_eq!(approval["turnId"], json!(2));
    assert_eq!(approval["binding"]["scope"], run_id);
    assert_eq!(approval["binding"]["toolCallId"], json!("call-write"));
    assert_ne!(approval["binding"]["callId"], json!("call-write"));

    let write_finish = seen
        .iter()
        .find(|event| event["type"] == "tool_finished" && event["data"]["operation"] == "write")
        .expect("write tool_finished");
    assert_eq!(write_finish["toolCallId"], json!("call-write"));
    assert_eq!(write_finish["runId"], run_id);

    let finished = seen
        .iter()
        .find(|event| event["type"] == "run_observation" && event["data"]["kind"] == "run_finished")
        .expect("v1 run_finished observation");
    assert_eq!(finished["runId"], run_id);
    assert_eq!(finished["data"]["status"], json!("success"));
    assert!(finished["sessionId"].is_string(), "{finished}");

    let done = seen
        .iter()
        .find(|event| event["type"] == "done")
        .expect("done event");
    assert_eq!(done["runId"], run_id_text);
    assert!(seen.iter().all(|event| event["schemaVersion"] == 1));

    client.close_stdin();
}

/// v1 封套中的错误必须与同一真实 run+turn 关联，且不泄漏供应商正文。
#[test]
fn rpc_v1_envelope_correlates_provider_error_with_the_real_run() {
    let mock = mock_server(|socket, _request| {
        write_status(
            socket,
            500,
            &json!({"error": {"message": "SECRET-BODY"}}).to_string(),
        );
    });
    let mut client = RpcClient::start(mock.address);
    client.send(json!({"type": "initialize", "id": "i1", "schema_versions": [1]}));
    assert_eq!(client.response("i1")["data"]["envelope"], "v1");
    client.send(json!({"type": "prompt", "id": "p1", "message": "fail"}));
    assert_eq!(client.response("p1")["success"], true);

    let seen = client
        .collect_until(|event| event["type"] == "agent_settled" && event["status"] == "error");
    let run_id = seen
        .iter()
        .find(|event| event["type"] == "run_observation" && event["data"]["kind"] == "run_started")
        .map(|event| event["runId"].clone())
        .expect("v1 run_started observation");
    let diagnostic = seen
        .iter()
        .find(|event| event["type"] == "diagnostic")
        .expect("diagnostic event");
    assert_eq!(diagnostic["runId"], run_id);
    assert_eq!(diagnostic["turnId"], json!(1));
    assert_eq!(diagnostic["data"]["category"], json!("service"));
    assert!(
        !diagnostic.to_string().contains("SECRET-BODY"),
        "{diagnostic}"
    );
    let finished = seen
        .iter()
        .find(|event| event["type"] == "run_observation" && event["data"]["kind"] == "run_finished")
        .expect("v1 run_finished observation");
    assert_eq!(finished["runId"], run_id);
    assert_eq!(finished["data"]["status"], json!("error"));
    client.close_stdin();
}

/// v1 封套中的取消必须归属到被取消运行的真实 run UUID。
#[test]
fn rpc_v1_envelope_attributes_cancellation_to_the_real_run() {
    let mock = mock_server(|socket, _request| {
        let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
        if socket.write_all(headers.as_bytes()).is_err() {
            return;
        }
        let text = "s".repeat(1024);
        loop {
            let frame_body = format!(
                "data: {}\n\n",
                sse_chunk(json!({"role": "assistant", "content": text}), Value::Null)
            );
            let frame = format!("{:x}\r\n{frame_body}\r\n", frame_body.len());
            if socket
                .write_all(frame.as_bytes())
                .and_then(|()| socket.flush())
                .is_err()
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let mut client = RpcClient::start(mock.address);
    client.send(json!({"type": "initialize", "id": "i1", "schema_versions": [1]}));
    assert_eq!(client.response("i1")["data"]["envelope"], "v1");
    client.send(json!({"type": "prompt", "id": "p1", "message": "stream"}));
    assert_eq!(client.response("p1")["success"], true);

    let started = client.collect_until(|event| {
        event["type"] == "run_observation" && event["data"]["kind"] == "run_started"
    });
    let run_id = started
        .iter()
        .find(|event| event["type"] == "run_observation" && event["data"]["kind"] == "run_started")
        .unwrap()["runId"]
        .clone();
    assert!(run_id.is_string(), "{run_id}");

    client.send(json!({"type": "abort", "id": "a1"}));
    let seen = client
        .collect_until(|event| event["type"] == "agent_settled" && event["status"] == "cancelled");
    let finished = seen
        .iter()
        .find(|event| event["type"] == "run_observation" && event["data"]["kind"] == "run_finished")
        .expect("v1 run_finished observation");
    assert_eq!(finished["runId"], run_id);
    assert_eq!(finished["data"]["status"], json!("cancelled"));
    let settled = seen.last().unwrap();
    assert_eq!(settled["status"], json!("cancelled"));
    assert_eq!(settled["runId"], run_id);
    client.close_stdin();
}
