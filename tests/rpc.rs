use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
#[path = "../src/test_http.rs"]
mod test_http;

struct RpcClient {
    child: Child,
    events: mpsc::Receiver<Value>,
    requests: mpsc::Receiver<(TcpStream, Value)>,
    stop: Arc<AtomicBool>,
    _dir: tempfile::TempDir,
}
impl Drop for RpcClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.stop.store(true, Ordering::SeqCst);
    }
}
impl RpcClient {
    fn new() -> Self {
        Self::with_subagents(false)
    }
    fn with_subagents(subagents: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let (request_tx, requests) = mpsc::channel();
        std::thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .unwrap();
                        let (_, request) = test_http::read_json(&mut stream);
                        if request_tx.send((stream, request)).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(e) => panic!("{e}"),
                }
            }
        });
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
        if subagents {
            command.arg("--subagents");
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
        let (event_tx, events) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.unwrap();
                let event = serde_json::from_str(&line)
                    .unwrap_or_else(|_| panic!("Non-JSON RPC output: {line}"));
                if event_tx.send(event).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            events,
            requests,
            stop,
            _dir: dir,
        }
    }
    fn send(&mut self, value: Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{value}").unwrap();
        stdin.flush().unwrap();
    }
    fn until(&self, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let event = self
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("RPC response deadline");
            if predicate(&event) {
                return event;
            }
        }
    }
    fn response(&self, id: &str) -> Value {
        self.until(|event| event["id"] == id)
    }
    fn settled(&self, status: &str) {
        assert_eq!(
            self.until(|e| e["type"] == "agent_settled")["status"],
            status
        );
    }
    fn request(&self) -> (TcpStream, Value) {
        self.requests
            .recv_timeout(Duration::from_secs(5))
            .expect("Model request deadline")
    }
    fn wait_exit(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            assert!(Instant::now() < deadline, "RPC did not exit on EOF");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
fn reply(mut stream: TcpStream, tool: Option<Value>) {
    let delta = match tool {
        Some(tool) => {
            json!({"role":"assistant","tool_calls":[{"index":0,"id":"tool-1","type":"function","function":tool}]})
        }
        None => json!({"role":"assistant","content":"ok"}),
    };
    let reason = if delta.get("tool_calls").is_some() {
        "tool_calls"
    } else {
        "stop"
    };
    let chunk = |delta: Value, finish: Value| {
        json!({"id":"test","object":"chat.completion.chunk","created":0,"model":"test","choices":[{"index":0,"delta":delta,"finish_reason":finish}]}).to_string()
    };
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(delta, Value::Null),
        chunk(json!({}), json!(reason))
    );
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
}
fn prompt(client: &mut RpcClient, message: &str) -> (TcpStream, Value) {
    client.send(json!({"type":"prompt","id":"start","message":message}));
    assert_eq!(client.response("start")["success"], true);
    client.request()
}

#[test]
fn abort_interrupts_model_wait_clears_queues_and_allows_reuse() {
    let mut client = RpcClient::new();
    let (held, _) = prompt(&mut client, "first");
    for (kind, id, message) in [
        ("steer", "s", "discarded-steering"),
        ("follow_up", "f", "discarded-followup"),
    ] {
        client.send(json!({"type":kind,"id":id,"message":message}));
        assert_eq!(client.response(id)["success"], true);
    }
    client.send(json!({"type":"get_state","id":"state"}));
    let state = client.response("state");
    assert_eq!(state["data"]["isStreaming"], true);
    assert_eq!(state["data"]["followUpCount"], 1);
    client.send(json!({"type":"abort","id":"abort"}));
    assert_eq!(client.response("abort")["data"]["aborted"], true);
    client.settled("cancelled");
    drop(held);
    let (stream, request) = prompt(&mut client, "fresh");
    assert!(!request.to_string().contains("discarded-"));
    assert!(!request["messages"].to_string().contains("first"));
    reply(stream, None);
    client.settled("completed");
    client.send(json!({"type":"abort","id":"idle"}));
    assert_eq!(client.response("idle")["data"]["aborted"], false);
}

#[test]
fn steering_without_tools_precedes_fifo_followups() {
    let mut client = RpcClient::new();
    let (stream, _) = prompt(&mut client, "initial");
    for (kind, id, message) in [
        ("follow_up", "f1", "follow-one"),
        ("follow_up", "f2", "follow-two"),
        ("steer", "s", "steer-first"),
    ] {
        client.send(json!({"type":kind,"id":id,"message":message}));
        assert_eq!(client.response(id)["success"], true);
    }
    reply(stream, None);
    for expected in ["steer-first", "follow-one", "follow-two"] {
        let (stream, request) = client.request();
        assert!(
            request["messages"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()
                .to_string()
                .contains(expected),
            "{request}"
        );
        reply(stream, None);
    }
    client.settled("completed");
    client.send(json!({"type":"follow_up","id":"idle","message":"idle-follow"}));
    assert_eq!(client.response("idle")["data"]["status"], "started");
    let (stream, request) = client.request();
    assert!(request.to_string().contains("idle-follow"));
    reply(stream, None);
    client.settled("completed");
}

#[test]
fn steering_reaches_next_model_call_after_tool_and_is_not_duplicated() {
    let mut client = RpcClient::new();
    std::fs::write(client._dir.path().join("input.txt"), "content").unwrap();
    let (stream, _) = prompt(&mut client, "read file");
    client.send(json!({"type":"steer","id":"s","message":"unique-steering-marker"}));
    assert_eq!(client.response("s")["success"], true);
    reply(
        stream,
        Some(json!({"name":"read","arguments":"{\"path\":\"input.txt\"}"})),
    );
    let (stream, request) = client.request();
    assert_eq!(
        request
            .to_string()
            .matches("unique-steering-marker")
            .count(),
        1
    );
    reply(stream, None);
    client.settled("completed");
    let (stream, request) = prompt(&mut client, "next task");
    assert_eq!(
        request
            .to_string()
            .matches("unique-steering-marker")
            .count(),
        1
    );
    reply(stream, None);
    client.settled("completed");
}

#[test]
fn busy_mutations_and_invalid_messages_are_rejected() {
    let mut client = RpcClient::new();
    let (stream, _) = prompt(&mut client, "hold");
    for command in [
        json!({"type":"set_model","model":"other"}),
        json!({"type":"new_session"}),
        json!({"type":"prompt","message":"other"}),
        json!({"type":"steer","message":" "}),
    ] {
        let mut command = command;
        command["id"] = json!("denied");
        client.send(command);
        assert_eq!(client.response("denied")["success"], false);
    }
    reply(stream, None);
    client.settled("completed");
    client.send(json!({"type":"get_state","id":"state"}));
    let state = client.response("state");
    assert_eq!(state["data"]["model"], "test");
    assert_eq!(state["data"]["isStreaming"], false);
}

#[test]
fn eof_cancels_active_model_request() {
    let mut client = RpcClient::new();
    let (_held, _) = prompt(&mut client, "hold");
    drop(client.child.stdin.take());
    client.wait_exit();
}

#[test]
fn failed_turn_discards_pending_messages() {
    let mut client = RpcClient::new();
    let (mut stream, _) = prompt(&mut client, "fail");
    client.send(json!({"type":"follow_up","id":"f","message":"discard-on-error"}));
    assert_eq!(client.response("f")["success"], true);
    write!(
        stream,
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
    )
    .unwrap();
    drop(stream);
    client.settled("error");
    let (stream, request) = prompt(&mut client, "retry");
    assert!(!request.to_string().contains("discard-on-error"));
    reply(stream, None);
    client.settled("completed");
}

#[cfg(unix)]
#[test]
fn abort_reaps_active_shell_process() {
    let mut client = RpcClient::new();
    let (stream, _) = prompt(&mut client, "shell");
    reply(
        stream,
        Some(
            json!({"name":"bash","arguments":json!({"command":"echo $$ > child.pid; sleep 30"}).to_string()}),
        ),
    );
    let pid_file = client._dir.path().join("child.pid");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !pid_file.exists() {
        assert!(Instant::now() < deadline, "shell did not start");
        std::thread::sleep(Duration::from_millis(5));
    }
    let pid: i32 = std::fs::read_to_string(pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    client.send(json!({"type":"abort","id":"a"}));
    assert_eq!(client.response("a")["success"], true);
    client.settled("cancelled");
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(
            Instant::now() < deadline,
            "shell survived abort: {:?}",
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn abort_cancels_active_child_model_request() {
    use std::io::Read;
    let mut client = RpcClient::with_subagents(true);
    let (stream, _) = prompt(&mut client, "delegate a task");
    reply(
        stream,
        Some(
            json!({"name":"subagent","arguments":json!({"operation":"delegate","task":{"session":"child","goal":"review","prompt":"child-prompt","read_only":true}}).to_string()}),
        ),
    );
    // Both the child model and the parent's continuation remain in flight.
    let (mut first, _) = client.request();
    let (mut second, _) = client.request();
    client.send(json!({"type":"abort","id":"a"}));
    assert_eq!(client.response("a")["success"], true);
    client.settled("cancelled");
    let mut byte = [0];
    assert_eq!(first.read(&mut byte).unwrap(), 0);
    assert_eq!(second.read(&mut byte).unwrap(), 0);
    let (stream, _) = prompt(&mut client, "after children cancelled");
    reply(stream, None);
    client.settled("completed");
}
