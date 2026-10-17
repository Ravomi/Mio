//! DES-05 真实引擎端到端回归：真实 loopback 模型 -> Policy -> DispatchHook ->
//! canonical 生命周期回调。
//!
//! 这不是单元测试：它启动真实 `run_with_options`，用本地 loopback mock 替换
//! Provider（无真实网络/凭据），并断言引擎回调里每个真实 provider id 的阶段链与
//! 拒绝语义。真实持久化（journal 落盘、v1 读取、GC 保护）在 `tests/tool_lifecycle.rs`
//! 与 `tests/output_cleanup.rs` 覆盖。

use mio_coding_agent::{
    AgentEvent, Config, Host, HostFuture, Provider, RunOptions, Services,
    policy::NormalizedCall,
    run_observation::{RunObservation, RunObservationKind},
    run_with_options,
    tool_lifecycle::{ToolCallPhase, ToolCallRecord},
};
use serde_json::{Value, json};
use std::{
    io::Write,
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

#[path = "../src/test_http.rs"]
mod test_http;

/// A host that records the local engine's canonical lifecycle callbacks.
#[derive(Default)]
struct LifecycleHost {
    calls: Mutex<Vec<ToolCallRecord>>,
    observations: Mutex<Vec<RunObservation>>,
    executions: AtomicUsize,
    refuse: bool,
}

impl LifecycleHost {
    fn calls_for(&self, tool_call_id: &str) -> Vec<ToolCallRecord> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.tool_call_id.as_deref() == Some(tool_call_id))
            .cloned()
            .collect()
    }
}

impl Host for LifecycleHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok("unexpected shell execution".to_owned()) })
    }

    fn on_event(&self, _: AgentEvent) {}

    fn approve_tool(&self, _: &str, _: &str) -> Result<(), String> {
        if self.refuse {
            Err("operator refused this call".to_owned())
        } else {
            Ok(())
        }
    }

    fn approve_tool_binding(
        &self,
        operation: &str,
        target: &str,
        _binding: &mio_coding_agent::policy::ApprovalBinding,
    ) -> HostFuture<Result<(), String>> {
        let result = self.approve_tool(operation, target);
        Box::pin(async move { result })
    }

    fn on_run_observation(&self, observation: &RunObservation) {
        self.observations.lock().unwrap().push(observation.clone());
    }

    fn on_tool_call_lifecycle(&self, call: &ToolCallRecord) -> Result<(), String> {
        self.calls.lock().unwrap().push(call.clone());
        Ok(())
    }

    fn confirm_unknown_side_effect(
        &self,
        _call: &NormalizedCall,
        _unknown: &[mio_coding_agent::unknown_effect::UnknownSideEffect],
    ) -> HostFuture<Result<(), String>> {
        // Not exercised here; keep the fail-closed default behaviour explicit.
        Box::pin(async { Err("no unknown was expected in this fixture".to_owned()) })
    }
}

/// Serve exactly two sequential SSE responses.
fn serve_two(
    replies: [(Value, &'static str); 2],
) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        for (delta, reason) in replies {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match listener.accept() {
                    Ok((socket, _)) => {
                        respond(socket, delta, reason);
                        break;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "model request deadline");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("local server failed: {error}"),
                }
            }
        }
    });
    (address, handle)
}

fn respond(mut socket: TcpStream, delta: Value, reason: &str) {
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let _ = test_http::read_json(&mut socket);
    let chunk = |delta: Value, finish: Value| {
        json!({
            "id":"local", "object":"chat.completion.chunk", "created":0, "model":"test",
            "choices":[{"index":0,"delta":delta,"finish_reason":finish}]
        })
    };
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(delta, Value::Null),
        chunk(json!({}), json!(reason))
    );
    write!(
        socket,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
}

fn tool_call(id: &str, name: &str, arguments: Value) -> Value {
    json!({
        "role": "assistant",
        "tool_calls": [{
            "index": 0,
            "id": id,
            "type": "function",
            "function": {"name": name, "arguments": arguments.to_string()},
        }],
    })
}

async fn run_fixture(root: &Path, address: std::net::SocketAddr, host: Arc<LifecycleHost>) {
    let result = run_with_options(
        Config {
            provider: Provider::Custom,
            model: "test".to_owned(),
            base_url: format!("http://{address}/v1"),
            api_key: "local-test".to_owned(),
        },
        Vec::new(),
        "run the fixture tool",
        root,
        host,
        Services {
            accounts: None,
            antigravity: None,
            deepseek: None,
        },
        RunOptions {
            max_turns: 4,
            ..RunOptions::default()
        },
    )
    .await;
    result.expect("the model loop must handle the tool result");
}

#[tokio::test]
async fn a_real_read_call_records_the_complete_stage_chain() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    std::fs::write(root.join("a.txt"), "hello from disk").unwrap();
    let (address, server) = serve_two([
        (
            tool_call("read-1", "read", json!({"path": "a.txt"})),
            "tool_calls",
        ),
        (json!({"role": "assistant", "content": "done"}), "stop"),
    ]);
    let host = Arc::new(LifecycleHost::default());
    run_fixture(&root, address, host.clone()).await;
    server.join().unwrap();

    let run_id = host
        .observations
        .lock()
        .unwrap()
        .iter()
        .find(|observation| observation.kind == RunObservationKind::RunStarted)
        .expect("run_started observation")
        .run_id
        .to_string();

    let records = host.calls_for("read-1");
    let phases: Vec<ToolCallPhase> = records.iter().map(|record| record.phase).collect();
    assert!(
        phases.contains(&ToolCallPhase::Normalized),
        "the chain must start normalized: {phases:?}"
    );
    assert!(phases.contains(&ToolCallPhase::Admitted), "{phases:?}");
    assert!(phases.contains(&ToolCallPhase::Dispatched), "{phases:?}");
    assert!(phases.contains(&ToolCallPhase::Executed), "{phases:?}");
    for record in &records {
        assert_eq!(record.scope, run_id, "{record:?}");
        assert_eq!(record.tool, "read", "{record:?}");
        assert_eq!(record.tool_call_id.as_deref(), Some("read-1"), "{record:?}");
    }
    assert_eq!(host.executions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_refused_approval_is_denied_with_zero_side_effect() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let (address, server) = serve_two([
        (
            tool_call(
                "refused-call",
                "bash",
                json!({"command": "touch forbidden"}),
            ),
            "tool_calls",
        ),
        (
            json!({"role": "assistant", "content": "the command was refused"}),
            "stop",
        ),
    ]);
    let host = Arc::new(LifecycleHost {
        refuse: true,
        ..LifecycleHost::default()
    });
    run_fixture(&root, address, host.clone()).await;
    server.join().unwrap();

    let records = host.calls_for("refused-call");
    let phases: Vec<ToolCallPhase> = records.iter().map(|record| record.phase).collect();
    assert!(phases.contains(&ToolCallPhase::Normalized), "{phases:?}");
    assert!(phases.contains(&ToolCallPhase::Denied), "{phases:?}");
    assert!(
        !phases.contains(&ToolCallPhase::Dispatched),
        "a refused approval must never reach dispatch: {phases:?}"
    );
    assert_eq!(
        host.executions.load(Ordering::SeqCst),
        0,
        "a refused call must not run its handler"
    );
    assert!(
        !root.join("forbidden").exists(),
        "a refused call must have no side effect"
    );
}
