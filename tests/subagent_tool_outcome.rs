//! Regression: a subagent control call is judged by its own `execute` result,
//! not by the top-level `error` field of a successful snapshot.
//!
//! `snapshot` / `status` / `kill` return a successful control result whose JSON
//! carries the *child's* business outcome; a failed child therefore publishes
//! `"status":"failed"` together with a top-level `"error"` string. Treating that
//! string as a failure of the control call is a false positive: the query
//! succeeded and the caller must be able to read the child's real result.
//! Genuine control-call failures (an unknown `run_id`, a missing argument, an
//! unknown operation) must still surface as tool errors.
//!
//! The test drives the real registration path: `run_with_tools` builds the
//! tool set exactly as production does, and the engine's own `ToolOutcome`
//! observations are the verdict.

#[path = "../src/test_http.rs"]
mod test_http;

use mio_coding_agent::{
    AgentEvent, Config, Host, HostFuture, Provider, RunOptions, RunTools, Services, ToolFilter,
    run_observation::{RunObservation, RunObservationKind, RunObservationStatus},
    run_with_tools,
    subagent::{
        Delegate, Executor, Status, SubagentManager, SubagentTool, SubagentToolAdapter, Task,
        ToolArgs,
    },
    tool_identity::ScopedTool,
};
use mio_core::tool::{PortableTool, Tool, ToolContext};
use serde_json::{Value, json};
use std::{
    io::Write,
    net::{SocketAddr, TcpListener},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use uuid::Uuid;

const API_KEY: &str = "local-test";

// ---------------------------------------------------------------------------
// Local mock provider
// ---------------------------------------------------------------------------

struct Response {
    body: Vec<u8>,
}

impl Response {
    fn sse(body: String) -> Self {
        Self {
            body: body.into_bytes(),
        }
    }
}

struct MockServer {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl MockServer {
    fn start(responses: Vec<Response>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut index = 0;
            while index < responses.len() && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        socket
                            .set_read_timeout(Some(Duration::from_secs(10)))
                            .unwrap();
                        socket
                            .set_write_timeout(Some(Duration::from_secs(10)))
                            .unwrap();
                        let (_, body) = test_http::read_json(&mut socket);
                        captured.lock().unwrap().push(body);
                        let response = &responses[index];
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            response.body.len()
                        );
                        socket.write_all(head.as_bytes()).unwrap();
                        socket.write_all(&response.body).unwrap();
                        socket.flush().unwrap();
                        index += 1;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("mock server accept failed: {error}"),
                }
            }
        });
        Self { address, requests }
    }

    fn bodies(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

/// One SSE completion carrying several tool calls, then a stop frame.
fn openai_tool_stream(id: &str, calls: &[(&str, Value)]) -> String {
    let tool_calls = calls
        .iter()
        .enumerate()
        .map(|(index, (call_id, arguments))| {
            json!({
                "index": index,
                "id": call_id,
                "type": "function",
                "function": {"name": "subagent", "arguments": arguments.to_string()}
            })
        })
        .collect::<Vec<_>>();
    let frames = [
        json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "test",
            "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": tool_calls}, "finish_reason": null}]
        })
        .to_string(),
        json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "test",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
        })
        .to_string(),
    ];
    frames
        .iter()
        .map(|frame| format!("data: {frame}"))
        .collect::<Vec<_>>()
        .join("\n\n")
        + "\n\ndata: [DONE]\n\n"
}

fn openai_stream(id: &str, text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "test",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": "stop"}]
        })
    )
}

// ---------------------------------------------------------------------------
// Hosts
// ---------------------------------------------------------------------------

/// Records the engine's own correlation observations.
#[derive(Default)]
struct ObservingHost {
    observations: Mutex<Vec<RunObservation>>,
}

impl Host for ObservingHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Err("shell is not expected in this test".into()) })
    }
    fn on_event(&self, _: AgentEvent) {}
    fn on_run_observation(&self, observation: &RunObservation) {
        self.observations.lock().unwrap().push(observation.clone());
    }
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

/// A real child failure: the delegated run ends `Failed` with a real error.
struct Failing;

impl Executor for Failing {
    fn execute(&self, _: Task) -> HostFuture<Result<Vec<mio_coding_agent::Message>, String>> {
        Box::pin(async { Err("intentional child failure".into()) })
    }
}

#[tokio::test]
async fn failed_child_status_is_a_successful_tool_outcome_and_unknown_id_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(ObservingHost::default());

    // A real child that really fails, admitted before the model turn starts.
    let manager = Arc::new(
        SubagentManager::with_executor(
            dir.path(),
            Config {
                model: "test".into(),
                ..Config::default()
            },
            RunOptions::default(),
            1,
            Duration::from_secs(5),
            Arc::new(Failing),
        )
        .unwrap(),
    );
    let child = manager
        .delegate(
            Delegate {
                session: "failed-child".into(),
                goal: "Review".into(),
                prompt: "fail".into(),
                path: None,
                read_only: true,
            },
            host.clone(),
        )
        .unwrap();
    let done = manager
        .status(child.run_id, Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(done.status, Status::Failed);
    assert!(
        done.error
            .as_deref()
            .is_some_and(|error| error.contains("intentional child failure")),
        "the child must have a real error: {done:?}"
    );

    let missing_id = Uuid::new_v4();
    let server = MockServer::start(vec![
        Response::sse(openai_tool_stream(
            "turn-1",
            &[
                (
                    "call_status_failed",
                    json!({"operation": "status", "run_id": child.run_id.to_string(), "wait_ms": 0}),
                ),
                (
                    "call_status_missing",
                    json!({"operation": "status", "run_id": missing_id.to_string(), "wait_ms": 0}),
                ),
            ],
        )),
        Response::sse(openai_stream("turn-2", "collected")),
    ]);

    let result = run_with_tools(
        Config {
            provider: Provider::Custom,
            base_url: format!("http://{}/v1", server.address),
            model: "test".into(),
            api_key: API_KEY.into(),
        },
        vec![],
        "go",
        dir.path(),
        host.clone(),
        Services::default(),
        RunTools {
            read_only: false,
            subagents: Some(manager.clone()),
            tool_filter: ToolFilter::all(),
            ..RunTools::default()
        },
    )
    .await;
    assert!(result.is_ok(), "{result:?}");

    // The engine's own verdict: the successful control call is a success even
    // though the snapshot it returned carries the child's real error, and the
    // nonexistent run id is a genuine control-call error.
    let observations = host.observations.lock().unwrap().clone();
    let outcome = |call_id: &str| {
        observations
            .iter()
            .find(|o| {
                o.kind == RunObservationKind::ToolOutcome
                    && o.tool_call_id.as_deref() == Some(call_id)
            })
            .cloned()
            .unwrap_or_else(|| panic!("missing ToolOutcome for {call_id}: {observations:?}"))
    };
    assert_eq!(
        outcome("call_status_failed").status,
        Some(RunObservationStatus::Success),
        "a failed child's snapshot is a successful status query"
    );
    assert_eq!(
        outcome("call_status_missing").status,
        Some(RunObservationStatus::Error),
        "an unknown run id must still fail the control call"
    );

    // The model still receives the child's real failure detail on the
    // successful query, and the real error on the failing one.
    let bodies = server.bodies();
    let last = bodies
        .last()
        .expect("a follow-up request with the tool results");
    let tool_content = |call_id: &str| {
        last["messages"]
            .as_array()
            .expect("messages array")
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == call_id)
            .and_then(|message| message["content"].as_str())
            .unwrap_or_else(|| panic!("missing tool result for {call_id}: {last}"))
            .to_owned()
    };
    let failed_content = tool_content("call_status_failed");
    assert!(
        failed_content.contains("failed") && failed_content.contains("intentional child failure"),
        "the snapshot must keep the child's real error: {failed_content}"
    );
    let missing_content = tool_content("call_status_missing");
    assert!(
        missing_content.contains("Unknown child run"),
        "the real control error must reach the model: {missing_content}"
    );
}

/// The same classification at the adapter boundary the engine registers, with
/// the real `ScopedTool` wrapper: a failed child's snapshot is `Ok`, a genuine
/// unknown run id is `Err`, and the public `PortableTool` value channel keeps
/// its original encoding.
#[tokio::test]
async fn typed_adapter_distinguishes_business_result_from_control_failure() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(ObservingHost::default());
    let manager = Arc::new(
        SubagentManager::with_executor(
            dir.path(),
            Config {
                model: "test".into(),
                ..Config::default()
            },
            RunOptions::default(),
            1,
            Duration::from_secs(5),
            Arc::new(Failing),
        )
        .unwrap(),
    );
    let child = manager
        .delegate(
            Delegate {
                session: "failed-child".into(),
                goal: "Review".into(),
                prompt: "fail".into(),
                path: None,
                read_only: true,
            },
            host.clone(),
        )
        .unwrap();
    let done = manager
        .status(child.run_id, Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(done.status, Status::Failed);

    let run_id = Uuid::new_v4();
    let tool = ScopedTool::new(
        SubagentToolAdapter::new(manager.clone(), host.clone()).with_parent_run_id(run_id),
        run_id,
    );
    let status_args = |run_id: Uuid| ToolArgs {
        operation: "status".into(),
        task: None,
        run_id: Some(run_id),
        session: None,
        target: None,
        wait_ms: Some(0),
    };

    // The real `ScopedTool` path the engine registers: a successful control
    // call whose payload carries the child's real error.
    let output = <ScopedTool<SubagentToolAdapter> as Tool>::call(
        &tool,
        &mut ToolContext::new(),
        status_args(child.run_id),
    )
    .await
    .expect("a failed child must not fail the status query");
    let snapshot = output.as_json().expect("snapshot JSON");
    assert_eq!(snapshot["status"], json!("failed"));
    assert!(
        snapshot["error"]
            .as_str()
            .is_some_and(|error| error.contains("intentional child failure")),
        "the child's real error must survive as business data: {snapshot}"
    );

    // A genuine control-call failure is still a real tool error.
    let error = <ScopedTool<SubagentToolAdapter> as Tool>::call(
        &tool,
        &mut ToolContext::new(),
        status_args(Uuid::new_v4()),
    )
    .await
    .expect_err("an unknown run id must fail the control call");
    assert_eq!(error.message(), "Unknown child run");
    assert!(
        error
            .model_output()
            .as_json()
            .is_some_and(|value| value["error"] == json!("Unknown child run")),
        "the model-visible encoding stays the original {{\"error\": ...}} JSON: {error:?}"
    );
    assert_eq!(<ScopedTool<SubagentToolAdapter> as Tool>::NAME, "subagent");

    // Public `PortableTool` compatibility is untouched: `Error = Infallible`
    // keeps the historical `Ok({"error": ...})` encoding on that channel.
    let legacy = PortableTool::call(
        &SubagentTool::new(manager, host).with_parent_run_id(run_id),
        status_args(Uuid::new_v4()),
    )
    .await
    .expect("the portable channel is infallible");
    assert_eq!(legacy["error"], json!("Unknown child run"));
}
