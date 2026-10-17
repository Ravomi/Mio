//! A denied tool must remain traceable without being reported as dispatched.
use mio_coding_agent::{
    AgentEvent, Config, Host, HostFuture, Provider, RunOptions, Services,
    run_observation::{RunObservation, RunObservationKind, RunObservationStatus},
    run_with_options,
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

#[derive(Default)]
struct RefusingHost {
    observations: Mutex<Vec<RunObservation>>,
    executions: AtomicUsize,
}

impl Host for RefusingHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok("unexpected execution".to_owned()) })
    }
    fn approve_tool(&self, _: &str, _: &str) -> Result<(), String> {
        Err("operator refused this call".to_owned())
    }
    fn on_event(&self, _: AgentEvent) {}
    fn on_run_observation(&self, observation: &RunObservation) {
        self.observations.lock().unwrap().push(observation.clone());
    }
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
    write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
}

#[tokio::test]
async fn refusal_has_correlated_outcome_but_no_committed_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let replies = [
            (
                json!({"role":"assistant","tool_calls":[{
                    "index":0,"id":"refused-call","type":"function",
                    "function":{"name":"bash","arguments":"{\"command\":\"printf forbidden\"}"}
                }]}),
                "tool_calls",
            ),
            (
                json!({"role":"assistant","content":"The command was refused."}),
                "stop",
            ),
        ];
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
    let host = Arc::new(RefusingHost::default());
    let result = run_with_options(
        Config {
            provider: Provider::Custom,
            model: "test".to_owned(),
            base_url: format!("http://{address}/v1"),
            api_key: "local-test".to_owned(),
        },
        Vec::new(),
        "Run the command if approved",
        root.path(),
        host.clone(),
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
    server.join().unwrap();
    result.expect("the model should handle a refused tool result");
    assert_eq!(host.executions.load(Ordering::SeqCst), 0);
    let observations = host.observations.lock().unwrap();
    let refused = observations
        .iter()
        .filter(|event| event.tool_call_id.as_deref() == Some("refused-call"))
        .collect::<Vec<_>>();
    assert!(
        refused
            .iter()
            .any(|event| event.kind == RunObservationKind::ToolOutcome
                && event.status == Some(RunObservationStatus::Error)),
        "refusal must remain traceable"
    );
    assert!(
        !refused
            .iter()
            .any(|event| event.kind == RunObservationKind::ToolDispatched),
        "denied admission is not a committed dispatch: {refused:?}"
    );
}
