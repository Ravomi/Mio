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
    requests: mpsc::Receiver<(TcpStream, Value, String)>,
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
        Self::configured(subagents, "custom", &[])
    }
    fn configured(subagents: bool, provider: &str, environment: &[(&str, &str)]) -> Self {
        Self::configured_with_persist(subagents, provider, environment, false)
    }
    fn configured_with_persist(
        subagents: bool,
        provider: &str,
        environment: &[(&str, &str)],
        persist: bool,
    ) -> Self {
        Self::configured_all(subagents, provider, environment, persist, false)
    }
    fn configured_all(
        subagents: bool,
        provider: &str,
        environment: &[(&str, &str)],
        persist: bool,
        approve: bool,
    ) -> Self {
        Self::configured_args(subagents, provider, environment, persist, approve, &[])
    }
    fn configured_args(
        subagents: bool,
        provider: &str,
        environment: &[(&str, &str)],
        persist: bool,
        approve: bool,
        args: &[&str],
    ) -> Self {
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
                        let (header, request) = test_http::read_json(&mut stream);
                        if request_tx.send((stream, request, header)).is_err() {
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
        command
            .current_dir(dir.path())
            .env("HOME", dir.path())
            .env("MIO_CONFIG_DIR", dir.path())
            .env("MIO_API_KEY", "local-test-credential")
            .envs(environment.iter().copied())
            .args([
                "--mode",
                "rpc",
                "--no-context",
                "--no-mcp",
                "--provider",
                provider,
                "--model",
                "test",
                "--base-url",
                &format!("http://{address}/v1"),
            ]);
        if !persist {
            command.arg("--no-session");
        }
        if approve {
            command.arg("--approve");
        }
        let mut child = command
            .args(args)
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
        let (stream, value, _) = self.request_with_headers();
        (stream, value)
    }
    fn request_with_headers(&self) -> (TcpStream, Value, String) {
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

fn system_request_contains(request: &Value, expected: &str) -> bool {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| {
            if message["role"] != "system" {
                return false;
            }
            let content = &message["content"];
            if let Some(text) = content.as_str() {
                return text.contains(expected);
            }
            content.as_array().is_some_and(|parts| {
                parts
                    .iter()
                    .filter(|part| part["type"] == "text")
                    .filter_map(|part| part["text"].as_str())
                    .collect::<String>()
                    .contains(expected)
            })
        })
}

/// 读取工作区持久化的全部 journal run（`.mio/journal/*.json`）。
fn journal_runs(client: &RpcClient) -> Vec<Value> {
    let directory = client._dir.path().join(".mio/journal");
    let mut runs = Vec::new();
    for entry in std::fs::read_dir(&directory).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("json") {
            let file: Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            runs.extend(file["runs"].as_array().cloned().unwrap_or_default());
        }
    }
    runs
}

/// 账本里的全部 key 记录（`.mio/dedup/task-keys.json`）。
fn dedup_entries(client: &RpcClient) -> Vec<Value> {
    let path = client._dir.path().join(".mio/dedup/task-keys.json");
    let file: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    file["entries"].as_array().cloned().unwrap_or_default()
}

/// 按真实 goal 定位一个 journal run；缺失即测试失败。
fn run_with_goal<'a>(runs: &'a [Value], goal: &str) -> &'a Value {
    runs.iter()
        .find(|run| run["goal"] == goal)
        .unwrap_or_else(|| panic!("journal must contain a run for {goal:?}: {runs:?}"))
}

#[test]
fn request_compaction_keeps_pins_and_exposes_content_free_audits_in_recovery() {
    let mut client = RpcClient::configured_args(
        false,
        "custom",
        &[],
        true,
        false,
        &[
            "--context-window",
            "8192",
            "--max-output-tokens",
            "256",
            "--no-tools",
        ],
    );
    let pin = format!(
        "pin-head {} MIDDLE-CONSTRAINT {} pin-tail",
        "界".repeat(30),
        "文".repeat(30)
    );
    client.send(
        json!({"type":"set_pinned_context","id":"audit-pins","context":{
            "entries":[{"id":"audit-rule","kind":"constraint","text":pin}]
        }}),
    );
    assert_eq!(client.response("audit-pins")["success"], true);
    let mut audits = Vec::new();
    for index in 0..6 {
        let (stream, request) = prompt(
            &mut client,
            &format!("ORIGINAL-CONTENT-{index} {}", "history ".repeat(280)),
        );
        assert!(system_request_contains(&request, &pin));
        reply(stream, None);
        loop {
            let event = client
                .events
                .recv_timeout(Duration::from_secs(5))
                .expect("compaction event deadline");
            if event["type"] == "context_compaction" {
                audits.push(event["data"].clone());
            }
            if event["type"] == "agent_settled" {
                assert_eq!(event["status"], "completed");
                break;
            }
        }
    }
    assert!(
        !audits.is_empty(),
        "fixture must trigger actual request compaction"
    );
    assert!(
        audits
            .iter()
            .all(|audit| audit["report"]["pinned_items_preserved"] == true)
    );
    assert!(
        audits
            .iter()
            .all(|audit| !audit["runId"].as_str().unwrap().is_empty())
    );
    assert!(
        !serde_json::to_string(&audits)
            .unwrap()
            .contains("ORIGINAL-CONTENT-")
    );
    client.send(json!({"type":"get_state","id":"audit-recovery"}));
    let state = client.response("audit-recovery");
    let stored = state["data"]["recovery"]["context_compactions"]
        .as_array()
        .unwrap();
    assert!(!stored.is_empty());
    assert!(
        !serde_json::to_string(stored)
            .unwrap()
            .contains("ORIGINAL-CONTENT-")
    );
    assert!(
        stored
            .iter()
            .all(|audit| !audit["journal_run_id"].as_str().unwrap().is_empty())
    );
    client.send(json!({"type":"compact","id":"manual-audit"}));
    let compacted = client.response("manual-audit");
    assert_eq!(compacted["success"], true);
    let loss = &compacted["data"]["loss_report"];
    assert!(loss.is_object());
    assert_eq!(loss["pinned_items_preserved"], true);
    client.send(json!({"type":"get_state","id":"manual-audit-recovery"}));
    assert_eq!(
        client.response("manual-audit-recovery")["data"]["recoveryView"]["lastCompactionLoss"],
        *loss
    );
}

#[test]
fn oversized_pinned_instructions_refuse_the_request_without_contacting_provider() {
    let mut client = RpcClient::configured_args(
        false,
        "custom",
        &[],
        false,
        false,
        &[
            "--context-window",
            "1024",
            "--max-output-tokens",
            "128",
            "--no-tools",
        ],
    );
    client.send(
        json!({"type":"set_pinned_context","id":"large-pin","context":{
            "entries":[{"id":"large","kind":"constraint","text":"x".repeat(4000)}]
        }}),
    );
    assert_eq!(client.response("large-pin")["success"], true);
    client.send(json!({"type":"prompt","id":"refused-pin","message":"continue"}));
    assert_eq!(client.response("refused-pin")["success"], true);
    client.settled("error");
    assert!(client.requests.try_recv().is_err());
    client.send(json!({"type":"get_state","id":"pin-retained"}));
    assert_eq!(
        client.response("pin-retained")["data"]["pinnedContext"]["entries"][0]["text"],
        "x".repeat(4000)
    );
}

#[test]
fn pinned_context_reaches_requests_survives_fork_and_is_cleared_by_new_session() {
    let mut client = RpcClient::new();
    let text = "Preserve the public API exactly.\n尾部约束：不得发布。";
    let pinned = json!({"entries":[{"id":"api-rule","kind":"constraint","text":text}]});
    client.send(json!({"type":"set_pinned_context","id":"pins","context":pinned}));
    assert_eq!(client.response("pins")["success"], true);
    assert!(client.requests.try_recv().is_err());
    let (stream, request) = prompt(&mut client, "inspect only");
    assert!(system_request_contains(&request, text));
    client.send(json!({"type":"set_pinned_context","id":"busy-pins","context":{"entries":[]}}));
    assert_eq!(client.response("busy-pins")["success"], false);
    reply(stream, None);
    client.settled("completed");
    client.send(json!({"type":"compact","id":"compact-pins"}));
    assert_eq!(client.response("compact-pins")["success"], true);
    client.send(json!({"type":"fork_session","id":"fork-pins"}));
    assert_eq!(client.response("fork-pins")["success"], true);
    client.send(json!({"type":"get_state","id":"state-pins"}));
    assert_eq!(
        client.response("state-pins")["data"]["pinnedContext"],
        pinned
    );
    client.send(json!({"type":"new_session","id":"clear-pins"}));
    assert_eq!(client.response("clear-pins")["success"], true);
    client.send(json!({"type":"get_state","id":"state-clear"}));
    assert_eq!(
        client.response("state-clear")["data"]["pinnedContext"]["entries"],
        json!([])
    );
    assert!(client.requests.try_recv().is_err());
}

#[test]
fn invalid_or_unsavable_pinned_context_keeps_the_previous_registry() {
    let mut client = RpcClient::configured_with_persist(false, "custom", &[], true);
    let pinned = json!({"entries":[{"id":"keep","kind":"goal","text":"Finish migration"}]});
    client.send(json!({"type":"set_pinned_context","id":"save-pins","context":pinned}));
    assert_eq!(client.response("save-pins")["success"], true);
    let duplicate = json!({"entries":[
        {"id":"duplicate","kind":"goal","text":"first"},
        {"id":"duplicate","kind":"goal","text":"second"}
    ]});
    client.send(json!({"type":"set_pinned_context","id":"invalid","context":duplicate}));
    let rejected = client.until(|event| event["type"] == "response" && event["command"] == "parse");
    assert_eq!(rejected["success"], false);
    client.send(json!({"type":"get_state","id":"saved-state"}));
    let state = client.response("saved-state");
    assert_eq!(state["data"]["pinnedContext"], pinned);
    let path = std::path::PathBuf::from(state["data"]["sessionFile"].as_str().unwrap());
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    client.send(json!({"type":"set_pinned_context","id":"unsavable","context":{"entries":[]}}));
    assert_eq!(client.response("unsavable")["success"], false);
    client.send(json!({"type":"get_state","id":"retained-state"}));
    assert_eq!(
        client.response("retained-state")["data"]["pinnedContext"],
        pinned
    );
    assert!(path.is_dir());
    assert!(client.requests.try_recv().is_err());
}

#[test]
fn protocol_discovery_is_available_idle_and_running_without_extra_requests() {
    let mut client = RpcClient::new();
    client.send(json!({"type":"get_state","id":"discover-idle"}));
    let idle = client.response("discover-idle");
    let protocol = &idle["data"]["protocol"];
    assert_eq!(protocol["name"], "mio-jsonl-rpc");
    assert_eq!(protocol["discoveryVersion"], 1);
    assert_eq!(protocol["eventEnvelope"], "legacy");
    assert_eq!(protocol["capabilities"]["eventReplay"], false);
    assert_eq!(protocol["capabilities"]["automaticSideEffectReplay"], false);
    assert_eq!(protocol["limits"]["inputLineBytes"], 1024 * 1024);
    assert!(client.requests.try_recv().is_err());

    let (held, _) = prompt(&mut client, "hold for discovery");
    client.send(json!({"type":"get_state","id":"discover-running"}));
    let running = client.response("discover-running");
    assert_eq!(running["data"]["protocol"], *protocol);
    assert_eq!(running["data"]["isStreaming"], true);
    assert!(client.requests.try_recv().is_err());
    client.send(json!({"type":"abort","id":"discover-stop"}));
    assert_eq!(client.response("discover-stop")["success"], true);
    client.settled("cancelled");
    drop(held);
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
    assert!(state["data"]["recoveryView"]["sessionFile"].is_null());
    assert!(state["data"]["recoveryView"]["savedMessages"].is_null());
    assert_eq!(state["data"]["recoveryView"]["automaticReplay"], false);
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

#[test]
fn model_switch_is_atomic_and_restores_configured_connections() {
    let mut client = RpcClient::with_subagents(true);
    client.send(json!({"type":"set_model","id":"switch","model":"deepseek/deepseek-chat:high"}));
    let switched = client.response("switch");
    assert_eq!(switched["success"], true);
    assert_eq!(switched["data"]["provider"], "deepseek");
    assert_eq!(switched["data"]["thinkingLevel"], "high");
    for invalid in ["", "openai/", "antigravity/missing-profile", "codex/test"] {
        client.send(json!({"type":"set_model","id":"bad","model":invalid}));
        assert_eq!(client.response("bad")["success"], false, "{invalid}");
        client.send(json!({"type":"get_state","id":"state"}));
        let state = client.response("state");
        assert_eq!(state["data"]["provider"], "deepseek");
        assert_eq!(state["data"]["model"], "deepseek-chat");
        assert_eq!(state["data"]["modelCapabilities"]["imageInput"], false);
    }
    client.send(json!({"type":"set_model","id":"vision","model":"openai/gpt-4o","thinking":"off"}));
    let vision = client.response("vision");
    assert_eq!(vision["success"], true, "{vision}");
    assert_eq!(vision["data"]["modelCapabilities"]["imageInput"], true);
    client.send(json!({"type":"get_state","id":"vision-state"}));
    assert_eq!(
        client.response("vision-state")["data"]["modelCapabilities"]["imageInput"],
        true
    );
    for (spec, provider) in [
        ("anthropic/claude", "anthropic"),
        ("google/test", "google"),
        ("ollama/test", "ollama"),
    ] {
        client.send(json!({"type":"set_model","id":"native","model":spec,"thinking":"off"}));
        let switched = client.response("native");
        assert_eq!(switched["success"], true, "{spec}: {switched}");
        assert_eq!(switched["data"]["provider"], provider);
    }
    client.send(json!({"type":"set_model","id":"back","model":"custom/restored"}));
    assert_eq!(client.response("back")["success"], true);
    let (stream, request) = prompt(&mut client, "uses original local endpoint");
    assert_eq!(request["model"], "restored");
    reply(stream, None);
    client.settled("completed");
}

#[test]
fn rpc_exposes_live_admission_and_enforces_cli_budget_before_network() {
    let mut client = RpcClient::configured_args(
        true,
        "custom",
        &[],
        false,
        false,
        &[
            "--no-shared-limits",
            "--max-subagents",
            "1",
            "--max-model-concurrency",
            "1",
            "--max-heavy-tasks",
            "1",
            "--max-model-calls",
            "1",
        ],
    );
    let (stream, _) = prompt(&mut client, "first admitted request");
    client.send(json!({"type":"get_state","id":"live"}));
    let live = client.response("live");
    assert_eq!(live["data"]["scheduler"]["model"]["active"], 1);
    assert_eq!(live["data"]["scheduler"]["model"]["limit"], 1);
    assert_eq!(live["data"]["scheduler"]["heavy"]["limit"], 1);
    assert_eq!(live["data"]["childTasks"]["limit"], 1);
    assert!(
        live["data"]["scheduler"]["budget"]["tokensReserved"]
            .as_u64()
            .unwrap()
            > 0
    );
    reply(stream, None);
    client.settled("completed");
    client.send(json!({"type":"prompt","id":"blocked","message":"must not reach model"}));
    assert_eq!(client.response("blocked")["success"], true);
    client.settled("error");
    assert!(
        client.requests.try_recv().is_err(),
        "exhausted budget must prevent HTTP dispatch"
    );
    client.send(json!({"type":"get_state","id":"settled"}));
    let settled = client.response("settled");
    assert_eq!(settled["data"]["scheduler"]["budget"]["calls"], 1);
    assert_eq!(settled["data"]["scheduler"]["model"]["active"], 0);
}

#[test]
fn cross_provider_switch_uses_target_endpoint_and_credentials() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let target_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok((mut stream, _)) = listener.accept() {
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let (headers, body) = test_http::read_json(&mut stream);
                tx.send((stream, headers, body)).unwrap();
                break;
            }
            assert!(Instant::now() < deadline, "new endpoint not used");
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    let mut client = RpcClient::configured(
        false,
        "deepseek",
        &[
            ("MIO_BASE_URL", &target_url),
            ("DEEPSEEK_API_KEY", "old-provider-test-key"),
            ("MIO_API_KEY", "target-provider-test-key"),
        ],
    );
    client.send(json!({"type":"prompt","id":"start","message":"original endpoint"}));
    assert_eq!(client.response("start")["success"], true);
    let (stream, _, headers) = client.request_with_headers();
    assert!(headers.contains("Bearer old-provider-test-key"));
    reply(stream, None);
    client.settled("completed");
    client.send(json!({"type":"set_model","id":"switch","model":"custom/target-model"}));
    assert_eq!(client.response("switch")["success"], true);
    client.send(json!({"type":"prompt","id":"next","message":"new endpoint"}));
    assert_eq!(client.response("next")["success"], true);
    let (stream, headers, body) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(headers.contains("Bearer target-provider-test-key"));
    assert!(!headers.contains("old-provider-test-key"));
    assert_eq!(body["model"], "target-model");
    reply(stream, None);
    client.settled("completed");
    server.join().unwrap();
}

#[test]
fn session_persistence_save_failure_retains_state_and_allows_retry() {
    let mut client = RpcClient::configured_with_persist(false, "custom", &[], true);

    // Initial prompts to populate session and verify file creation
    for index in 0..6 {
        let (s, _) = prompt(&mut client, &format!("prompt {index}"));
        reply(s, None);
        client.settled("completed");
    }

    // Get current state
    client.send(json!({"type":"get_state","id":"st1"}));
    let st1 = client.response("st1")["data"].clone();
    let session_file = std::path::PathBuf::from(st1["sessionFile"].as_str().unwrap());
    assert!(session_file.exists());
    let session_dir = session_file.parent().unwrap().to_path_buf();
    let backup_dir = session_dir.with_extension("backup");

    let orig_id = st1["sessionId"].as_str().unwrap().to_string();
    let orig_file = st1["sessionFile"].as_str().unwrap().to_string();
    let orig_msg_count = st1["messageCount"].as_u64().unwrap();
    assert_eq!(orig_msg_count, 12);

    // Backup session dir and replace with a regular file to make saves deterministically fail
    std::fs::rename(&session_dir, &backup_dir).unwrap();
    std::fs::write(&session_dir, b"blocker file").unwrap();

    for (cmd_type, req_json) in [
        ("compact", json!({"type":"compact","id":"c1"})),
        (
            "set_session_name",
            json!({"type":"set_session_name","id":"n1","name":"new-name"}),
        ),
        ("fork_session", json!({"type":"fork_session","id":"f1"})),
        (
            "new_session",
            json!({"type":"new_session","id":"ns1","name":"fresh"}),
        ),
    ] {
        let req_id = req_json["id"].as_str().unwrap().to_string();
        client.send(req_json);
        let resp = client.response(&req_id);
        assert_eq!(resp["success"], false);
        assert!(!resp["data"]["error"].as_str().unwrap().is_empty());

        let check_id = format!("st_{cmd_type}");
        client.send(json!({"type":"get_state","id":check_id}));
        let st = client.response(&check_id)["data"].clone();
        assert_eq!(st["sessionId"].as_str().unwrap(), orig_id);
        assert_eq!(st["sessionName"], st1["sessionName"]);
        assert_eq!(st["sessionFile"].as_str().unwrap(), orig_file);
        assert_eq!(st["messageCount"].as_u64().unwrap(), orig_msg_count);
    }

    // Clear blocker and restore directory
    std::fs::remove_file(&session_dir).unwrap();
    std::fs::rename(&backup_dir, &session_dir).unwrap();

    // Retry operations successfully and verify files updated
    client.send(json!({"type":"set_session_name","id":"n_retry","name":"renamed-ok"}));
    let resp = client.response("n_retry");
    assert_eq!(resp["success"], true);
    let disk_content = std::fs::read_to_string(&session_file).unwrap();
    assert!(disk_content.contains("renamed-ok"));

    client.send(json!({"type":"compact","id":"c_retry"}));
    let resp = client.response("c_retry");
    assert_eq!(resp["success"], true);
    let compacted_messages = resp["data"]["compacted_messages"].as_u64().unwrap();
    let original_messages = resp["data"]["original_messages"].as_u64().unwrap();
    assert!(compacted_messages < original_messages);
    let disk_content = std::fs::read_to_string(&session_file).unwrap();
    let parsed: Value = serde_json::from_str(&disk_content).unwrap();
    assert_eq!(
        parsed["history"].as_array().unwrap().len(),
        compacted_messages as usize
    );

    client.send(json!({"type":"fork_session","id":"f_retry"}));
    let resp = client.response("f_retry");
    assert_eq!(resp["success"], true);
    let forked_id = resp["data"]["sessionId"].as_str().unwrap();
    let forked_file = session_dir.join(format!("{forked_id}.json"));
    assert!(forked_file.exists());

    client.send(json!({"type":"new_session","id":"ns_retry","name":"new-ok"}));
    let resp = client.response("ns_retry");
    assert_eq!(resp["success"], true);
    let new_id = resp["data"]["sessionId"].as_str().unwrap();
    let new_file = session_dir.join(format!("{new_id}.json"));
    assert!(new_file.exists());
}

#[test]
fn prompt_save_failure_reports_settled_error_no_done_and_does_not_leak_into_next_turn() {
    let mut client = RpcClient::configured_with_persist(false, "custom", &[], true);

    // Initial prompt to establish session file
    let (s1, _) = prompt(&mut client, "first successful message");
    reply(s1, None);
    client.settled("completed");

    client.send(json!({"type":"get_state","id":"st_init"}));
    let st_init = client.response("st_init")["data"].clone();
    let session_file = std::path::PathBuf::from(st_init["sessionFile"].as_str().unwrap());
    let session_dir = session_file.parent().unwrap().to_path_buf();
    let backup_dir = session_dir.with_extension("backup_prompt");
    let init_msg_count = st_init["messageCount"].as_u64().unwrap();

    // Introduce save failure by replacing parent dir with file
    std::fs::rename(&session_dir, &backup_dir).unwrap();
    std::fs::write(&session_dir, b"blocker file").unwrap();

    // Send prompt, model responds ok, but save will fail
    let (s2, _) = prompt(&mut client, "this turn will fail saving");
    reply(s2, None);

    // Expect agent_settled with status error and verify no "done" event
    let mut saw_done = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let event = client
            .events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("event deadline");
        if event["type"] == "done" {
            saw_done = true;
        }
        if event["type"] == "agent_settled" {
            assert_eq!(event["status"], "error");
            break;
        }
    }
    assert!(!saw_done, "Must not emit 'done' when saving session fails");

    // Verify get_state messageCount did not grow
    client.send(json!({"type":"get_state","id":"st_after_fail"}));
    let st_after = client.response("st_after_fail")["data"].clone();
    assert_eq!(st_after["messageCount"].as_u64().unwrap(), init_msg_count);

    // Restore directory
    std::fs::remove_file(&session_dir).unwrap();
    std::fs::rename(&backup_dir, &session_dir).unwrap();

    // Next prompt should not contain the failed turn content
    let (s3, req3) = prompt(&mut client, "clean follow-up turn");
    assert!(!req3.to_string().contains("this turn will fail saving"));
    reply(s3, None);
    client.settled("completed");

    // State now reflects the successful turns (each turn adds user message + assistant reply)
    client.send(json!({"type":"get_state","id":"st_final"}));
    let st_final = client.response("st_final")["data"].clone();
    assert_eq!(
        st_final["messageCount"].as_u64().unwrap(),
        init_msg_count + 2
    );
}

#[test]
fn approve_flow_allow_deny_and_unknown_request() {
    let mut client = RpcClient::configured_all(false, "custom", &[], false, true);
    let target_file = client._dir.path().join("approved.txt");
    let denied_file = client._dir.path().join("denied.txt");

    // 1. Allow 流程
    let (s1, _) = prompt(&mut client, "write approved");
    let write_call = json!({
        "name": "workspace",
        "arguments": json!({
            "operation": "write",
            "path": "approved.txt",
            "value": "hello approval"
        }).to_string()
    });
    reply(s1, Some(write_call));

    let req_event = client.until(|e| e["type"] == "approval_request");
    let req_id = req_event["request_id"].as_str().unwrap().to_string();
    assert_eq!(req_event["operation"], "write");
    assert_eq!(req_event["target"], "approved.txt");
    assert_eq!(req_event["binding"]["operation"], "write");
    assert_eq!(req_event["binding"]["tool"], "write");
    assert_eq!(req_event["binding"]["entry"], "aggregate");
    assert_eq!(
        req_event["binding"]["cwd"],
        client._dir.path().canonicalize().unwrap().to_str().unwrap()
    );
    assert_eq!(
        req_event["binding"]["paramsSha256"].as_str().unwrap().len(),
        64
    );
    assert!(uuid::Uuid::parse_str(req_event["binding"]["scope"].as_str().unwrap()).is_ok());
    assert!(uuid::Uuid::parse_str(req_event["binding"]["callId"].as_str().unwrap()).is_ok());
    assert!(!target_file.exists());

    client.send(json!({"type":"get_state","id":"st1"}));
    let st1 = client.response("st1");
    assert_eq!(st1["data"]["pendingApprovalCount"], 1);

    // 重复 / 未知 request_id 回复 success false
    client.send(json!({
        "type": "approve_response",
        "request_id": "non-existent-req-id",
        "approved": true
    }));
    let unk_resp = client.response("non-existent-req-id");
    assert_eq!(unk_resp["success"], false);

    // 发送真实同意
    client.send(json!({
        "type": "approve_response",
        "request_id": req_id,
        "approved": true
    }));
    let ok_resp = client.response(&req_id);
    assert_eq!(ok_resp["success"], true);

    // 重复同一 request_id 回复 success false
    client.send(json!({
        "type": "approve_response",
        "request_id": req_id,
        "approved": true
    }));
    let dup_resp = client.response(&req_id);
    assert_eq!(dup_resp["success"], false);

    // 下一 model request 到达时文件已经写入
    let (s2, req2) = client.request();
    assert!(target_file.exists());
    assert_eq!(
        std::fs::read_to_string(&target_file).unwrap(),
        "hello approval"
    );
    assert!(req2.to_string().contains("tool_call_id"));
    reply(s2, None);
    client.settled("completed");

    // 2. Deny 流程
    let (s3, _) = prompt(&mut client, "write denied");
    let write_call_deny = json!({
        "name": "workspace",
        "arguments": json!({
            "operation": "write",
            "path": "denied.txt",
            "value": "should not exist"
        }).to_string()
    });
    reply(s3, Some(write_call_deny));

    let req_event_deny = client.until(|e| e["type"] == "approval_request");
    let req_id_deny = req_event_deny["request_id"].as_str().unwrap().to_string();
    assert_ne!(
        req_event_deny["binding"]["callId"],
        req_event["binding"]["callId"]
    );
    assert_ne!(
        req_event_deny["binding"]["paramsSha256"],
        req_event["binding"]["paramsSha256"]
    );
    assert!(!denied_file.exists());

    client.send(json!({
        "type": "approve_response",
        "request_id": req_id_deny,
        "approved": false
    }));
    let deny_ack = client.response(&req_id_deny);
    assert_eq!(deny_ack["success"], true);

    // 下一 model request 到达时文件仍不存在且包含拒绝结果
    let (s4, req4) = client.request();
    assert!(!denied_file.exists());
    assert!(req4.to_string().contains("rejected"));
    reply(s4, None);
    client.settled("completed");
}

#[test]
fn approve_abort_and_eof_handling() {
    let mut client = RpcClient::configured_all(false, "custom", &[], false, true);
    let target_file = client._dir.path().join("abort.txt");

    // 1. abort 等待审批时应立即 settled cancelled 并清 pending，旧 reply 失败，下一 prompt 正常运行
    let (s1, _) = prompt(&mut client, "write abort");
    reply(
        s1,
        Some(json!({
            "name": "workspace",
            "arguments": json!({
                "operation": "write",
                "path": "abort.txt",
                "value": "abort test"
            }).to_string()
        })),
    );

    let req_event = client.until(|e| e["type"] == "approval_request");
    let req_id = req_event["request_id"].as_str().unwrap().to_string();

    client.send(json!({"type":"abort","id":"ab1"}));
    assert_eq!(client.response("ab1")["data"]["aborted"], true);
    client.settled("cancelled");
    assert!(!target_file.exists());

    // 旧 reply 失败
    client.send(json!({
        "type": "approve_response",
        "request_id": req_id,
        "approved": true
    }));
    let old_reply_resp = client.response(&req_id);
    assert_eq!(old_reply_resp["success"], false);

    // 下一 prompt 正常运行
    let (s2, _) = prompt(&mut client, "clean follow-up");
    reply(s2, None);
    client.settled("completed");

    // 2. EOF 等待审批关闭 stdin 进程正常退出且无文件
    let target_file_eof = client._dir.path().join("eof.txt");
    let (s3, _) = prompt(&mut client, "write eof");
    reply(
        s3,
        Some(json!({
            "name": "workspace",
            "arguments": json!({
                "operation": "write",
                "path": "eof.txt",
                "value": "eof test"
            }).to_string()
        })),
    );
    let _ = client.until(|e| e["type"] == "approval_request");

    // 关闭 stdin
    drop(client.child.stdin.take());
    client.wait_exit();
    assert!(!target_file_eof.exists());
}

#[test]
fn subagent_approval_flow_and_parent_abort() {
    let mut client = RpcClient::configured_all(true, "custom", &[], false, true);
    let target_file = client._dir.path().join("child_output.txt");

    // Parent 首次模型 reply subagent.delegate task read_only:false
    let (parent_init_stream, _) = prompt(&mut client, "start subagent task");
    let delegate_call = json!({
        "name": "subagent",
        "arguments": json!({
            "operation": "delegate",
            "task": {
                "session": "child-session",
                "goal": "write child file",
                "prompt": "child-prompt",
                "read_only": false
            }
        }).to_string()
    });
    reply(parent_init_stream, Some(delegate_call));

    // 获取 child 和 parent 续接两个 HTTP 请求
    let (stream_a, req_a) = client.request();
    let (stream_b, req_b) = client.request();

    let is_child_req = |req: &Value| -> bool {
        if let Some(messages) = req.get("messages").and_then(|m| m.as_array()) {
            for m in messages {
                if m.get("role").and_then(|r| r.as_str()) == Some("user") {
                    if let Some(content) = m.get("content").and_then(|c| c.as_str()) {
                        if content == "child-prompt" {
                            return true;
                        }
                    } else if let Some(parts) = m.get("content").and_then(|c| c.as_array()) {
                        let mut texts = String::new();
                        for part in parts {
                            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                                texts.push_str(text);
                            } else if let Some(text) = part.as_str() {
                                texts.push_str(text);
                            }
                        }
                        if texts == "child-prompt" {
                            return true;
                        }
                    }
                }
            }
        }
        false
    };

    let a_is_child = is_child_req(&req_a);
    let b_is_child = is_child_req(&req_b);
    assert!(
        a_is_child != b_is_child,
        "Expected exactly one child request, got a={a_is_child}, b={b_is_child}"
    );

    let (child_stream, _parent_cont_stream) = if a_is_child {
        (stream_a, stream_b)
    } else {
        (stream_b, stream_a)
    };

    // parent 连接保持（_parent_cont_stream 保持不 reply），child 模型 reply workspace.write
    let child_write_call = json!({
        "name": "workspace",
        "arguments": json!({
            "operation": "write",
            "path": "child_output.txt",
            "value": "child created content"
        }).to_string()
    });
    reply(child_stream, Some(child_write_call));

    // 收到 approval_request 且文件不存在
    let req_event = client.until(|e| e["type"] == "approval_request");
    let req_id = req_event["request_id"].as_str().unwrap().to_string();
    assert_eq!(req_event["operation"], "write");
    assert_eq!(req_event["target"], "child_output.txt");
    assert!(!target_file.exists());

    // 发 approve_response true
    client.send(json!({
        "type": "approve_response",
        "request_id": req_id,
        "approved": true
    }));
    let ok_resp = client.response(&req_id);
    assert_eq!(ok_resp["success"], true);

    // child 下一 HTTP 请求到达确认文件内容
    let (child_cont_stream, req_child_cont) = client.request();
    assert!(target_file.exists());
    assert_eq!(
        std::fs::read_to_string(&target_file).unwrap(),
        "child created content"
    );
    assert!(req_child_cont.to_string().contains("tool_call_id"));

    // 随后 abort 父任务清理所有连接/审批，无需等子任务完整成功
    client.send(json!({"type":"abort","id":"abort_sub"}));
    assert_eq!(client.response("abort_sub")["data"]["aborted"], true);
    client.settled("cancelled");
    drop(child_cont_stream);

    // pendingApprovalCount=0 并可再 prompt
    client.send(json!({"type":"get_state","id":"st_after_abort"}));
    let state_resp = client.response("st_after_abort");
    assert_eq!(state_resp["data"]["pendingApprovalCount"], 0);

    let (s_after, _) = prompt(&mut client, "prompt after subagent abort");
    reply(s_after, None);
    client.settled("completed");
}

#[test]
fn thinking_switch_reaches_parent_and_child_requests() {
    let mut client = RpcClient::configured_args(
        true,
        "custom",
        &[],
        false,
        false,
        &["--context-window", "24576", "--max-output-tokens", "256"],
    );
    client.send(json!({"type":"set_model","id":"thinking","model":"test:high"}));
    assert_eq!(client.response("thinking")["success"], true);
    let (stream, request) = prompt(&mut client, "delegate");
    assert_eq!(request["reasoning_effort"], "high");
    assert_eq!(request["max_tokens"], 256);
    reply(
        stream,
        Some(
            json!({"name":"subagent","arguments":json!({"operation":"delegate","task":{"session":"thinking-child","goal":"review","prompt":"child-prompt","read_only":true}}).to_string()}),
        ),
    );
    let mut outstanding = Vec::new();
    for _ in 0..2 {
        let (stream, request) = client.request();
        assert_eq!(request["reasoning_effort"], "high");
        assert_eq!(request["max_tokens"], 256);
        // Keep both requests outstanding until cancellation so parent cleanup cannot race child capture.
        outstanding.push(stream);
    }
    client.send(json!({"type":"abort","id":"stop"}));
    client.response("stop");
}

#[test]
fn unsupported_thinking_switch_preserves_old_state() {
    let mut client = RpcClient::configured(false, "deepseek", &[]);
    client.send(json!({"type":"set_model","id":"valid","model":"test:high"}));
    assert_eq!(client.response("valid")["success"], true);
    client.send(json!({"type":"set_model","id":"invalid","model":"test:medium"}));
    let refused = client.response("invalid");
    assert_eq!(refused["success"], false);
    assert!(refused.to_string().contains("不支持 medium"));
    client.send(json!({"type":"get_state","id":"state"}));
    assert_eq!(client.response("state")["data"]["thinkingLevel"], "high");
}

#[test]
fn context_budget_blocks_oversized_input_before_any_model_request() {
    let mut client = RpcClient::configured_args(
        false,
        "custom",
        &[],
        false,
        false,
        &[
            "--no-tools",
            "--context-window",
            "2048",
            "--max-output-tokens",
            "128",
        ],
    );
    client.send(json!({"type":"prompt","id":"large","message":"中".repeat(1000)}));
    assert_eq!(client.response("large")["success"], true);
    let error = client.until(|event| event["type"] == "error");
    assert!(error.to_string().contains("Context budget:"), "{error}");
    client.settled("error");
    assert!(client.requests.try_recv().is_err());
    let (stream, request) = prompt(&mut client, "recover");
    assert_eq!(request["max_tokens"], 128);
    reply(stream, None);
    client.settled("completed");
}

#[test]
fn context_budget_rechecks_after_tools_and_preserves_call_result_pairs() {
    let mut client = RpcClient::configured_args(
        false,
        "custom",
        &[],
        false,
        false,
        &[
            "--tools",
            "grep",
            "--context-window",
            "8192",
            "--max-output-tokens",
            "512",
        ],
    );
    // grep retains enough data to exercise both history compaction and the
    // generic active-result externalization path; read paginates before this.
    // Two old turns fit alone; a medium grep result forces request-only compaction.
    for index in 0..2 {
        let (stream, _) = prompt(&mut client, &format!("old-{index}:{}", "x".repeat(1600)));
        reply(stream, None);
        client.settled("completed");
    }
    std::fs::write(client._dir.path().join("medium.txt"), "m".repeat(3600)).unwrap();
    let (stream, _) = prompt(&mut client, "grep medium");
    reply(
        stream,
        Some(
            json!({"name":"grep","arguments":json!({"pattern":"m","path":"medium.txt"}).to_string()}),
        ),
    );
    let (stream, request) = client.request();
    let messages = request["messages"].as_array().unwrap();
    assert!(
        request.to_string().contains("Earlier context summary"),
        "{request}"
    );
    let call_index = messages
        .iter()
        .position(|m| m["tool_calls"].is_array())
        .unwrap();
    let result_index = messages.iter().position(|m| m["role"] == "tool").unwrap();
    assert!(call_index < result_index);
    assert_eq!(
        messages[call_index]["tool_calls"][0]["id"],
        messages[result_index]["tool_call_id"]
    );
    assert!(
        messages[result_index]["content"]
            .as_str()
            .unwrap()
            .contains(&"m".repeat(3600))
    );
    assert_eq!(request["max_tokens"], 512);
    reply(stream, None);
    client.settled("completed");
    // Preserve the active pair by spilling its body, within the same budget.
    std::fs::write(client._dir.path().join("huge.txt"), "z".repeat(20000)).unwrap();
    let (stream, _) = prompt(&mut client, "grep huge");
    reply(
        stream,
        Some(
            json!({"name":"grep","arguments":json!({"pattern":"z","path":"huge.txt"}).to_string()}),
        ),
    );
    let (stream, request) = client.request();
    let messages = request["messages"].as_array().unwrap();
    let (call_index, call) = messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, message)| message["tool_calls"].is_array())
        .unwrap();
    let (result_index, result) = messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, message)| message["role"] == "tool")
        .unwrap();
    assert!(call_index < result_index);
    assert_eq!(call["tool_calls"][0]["id"], result["tool_call_id"]);
    let content: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
    let references = content["output_references"].as_array().unwrap();
    assert_eq!(references.len(), 1);
    let reference = &references[0];
    // reply() supplied the real provider id "tool-1". The provider encoder
    // may rename a repeated historical id in this request; the reference must
    // keep the original identity, while the wire call/result stay paired above.
    assert_eq!(reference["call_id"], "tool-1");
    let relative = reference["path"].as_str().unwrap();
    let name = relative.strip_prefix(".mio/outputs/").unwrap();
    assert!(!name.contains('/'));
    let bytes = std::fs::read(client._dir.path().join(".mio/outputs").join(name)).unwrap();
    use sha2::{Digest, Sha256};
    assert_eq!(reference["sha256"], format!("{:x}", Sha256::digest(&bytes)));
    assert_eq!(reference["total_bytes"], bytes.len());
    let full: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        full["output"]
            .as_str()
            .unwrap()
            .contains(&"z".repeat(20000))
    );
    assert!(
        !result["content"]
            .as_str()
            .unwrap()
            .contains(&"z".repeat(20000))
    );
    let input_limit = 8192 - 512 - (8192_usize.div_ceil(20));
    assert!(serde_json::to_vec(messages).unwrap().len() < input_limit);
    assert_eq!(request["max_tokens"], 512);
    reply(stream, None);
    client.settled("completed");
    assert!(client.requests.try_recv().is_err());
}

#[test]
fn context_budget_model_switch_recomputes_catalog_and_keeps_override() {
    for args in [
        vec![],
        vec!["--context-window", "8192", "--max-output-tokens", "512"],
    ] {
        let mut client = RpcClient::configured_args(false, "custom", &[], false, false, &args);
        client.send(json!({"type":"set_model","id":"switch","model":"openai/gpt-4.1"}));
        assert_eq!(client.response("switch")["success"], true);
        client.send(json!({"type":"get_state","id":"state"}));
        let state = client.response("state");
        let budget = &state["data"]["contextBudget"];
        assert_eq!(
            budget["context_window"],
            if args.is_empty() { 1_047_576 } else { 8192 }
        );
        assert_eq!(
            budget["source"],
            if args.is_empty() {
                "catalog"
            } else {
                "override"
            }
        );
    }
}

#[test]
fn known_nonreasoning_model_switch_is_rejected_without_changing_state() {
    let mut client = RpcClient::new();
    client.send(json!({"type":"set_model","id":"valid","model":"test:high"}));
    assert_eq!(client.response("valid")["success"], true);
    client.send(json!({"type":"set_model","id":"invalid","model":"gpt-4.1"}));
    let response = client.response("invalid");
    assert_eq!(response["success"], false);
    assert!(response.to_string().contains("does not reason"));
    client.send(json!({"type":"get_state","id":"state"}));
    let state = client.response("state");
    assert_eq!(state["data"]["model"], "test");
    assert_eq!(state["data"]["thinkingLevel"], "high");
    client.send(json!({"type":"set_model","id":"off","model":"gpt-4.1:off"}));
    assert_eq!(client.response("off")["success"], true);
}

#[test]
fn prompt_dedup_key_reuses_task_id_without_rerunning() {
    let mut client = RpcClient::new();
    client.send(json!({"type":"prompt","id":"p1","message":"first","dedup_key":"job-1"}));
    let first = client.response("p1");
    assert_eq!(first["success"], true);
    assert_eq!(first["data"]["status"], "started");
    assert_eq!(first["data"]["dedup_key"], "job-1");
    let task_id = first["data"]["task_id"].as_str().unwrap().to_owned();
    let (stream, _) = client.request();
    client.send(json!({"type":"prompt","id":"active-duplicate","message":"duplicate while active","dedup_key":"job-1"}));
    let duplicate = client.response("active-duplicate");
    assert_eq!(duplicate["data"]["status"], "deduplicated");
    assert_eq!(duplicate["data"]["task_id"], task_id);
    assert_eq!(duplicate["data"]["state"], "active");
    reply(stream, None);
    client.settled("completed");

    // 相同 key 再次提交：不触发模型请求，只回放已有任务标识。
    client.send(json!({"type":"prompt","id":"p2","message":"second","dedup_key":"job-1"}));
    let second = client.response("p2");
    assert_eq!(second["success"], true);
    assert_eq!(second["data"]["status"], "deduplicated");
    assert_eq!(second["data"]["task_id"].as_str().unwrap(), task_id);
    assert_eq!(second["data"]["state"], "completed");
    client.settled("deduplicated");
}

#[test]
fn rejected_follow_up_does_not_consume_its_dedup_key() {
    let mut client = RpcClient::new();
    let (held, _) = prompt(&mut client, "hold");
    // 空消息被拒绝：key 不得被消耗。
    client.send(json!({"type":"follow_up","id":"bad","message":"   ","dedup_key":"q1"}));
    assert_eq!(client.response("bad")["success"], false);
    client.send(json!({"type":"follow_up","id":"ok","message":"queued-work","dedup_key":"q1"}));
    let ok = client.response("ok");
    assert_eq!(ok["success"], true);
    assert_eq!(ok["data"]["queued"], 1);
    let task_id = ok["data"]["task_id"].as_str().unwrap().to_owned();

    // 同 key 再次排队：合并到已有排队任务，队列不增长。
    client.send(json!({"type":"follow_up","id":"dup","message":"duplicate","dedup_key":"q1"}));
    let dup = client.response("dup");
    assert_eq!(dup["data"]["status"], "deduplicated");
    assert_eq!(dup["data"]["state"], "queued");
    assert_eq!(dup["data"]["task_id"].as_str().unwrap(), task_id);
    client.send(json!({"type":"get_state","id":"state"}));
    assert_eq!(client.response("state")["data"]["followUpCount"], 1);

    client.send(json!({"type":"abort","id":"abort"}));
    assert_eq!(client.response("abort")["data"]["aborted"], true);
    client.settled("cancelled");
    drop(held);
}

#[test]
fn queue_full_rejection_does_not_consume_dedup_key() {
    let mut client = RpcClient::new();
    let (held, _) = prompt(&mut client, "hold");
    for index in 0..64 {
        let id = format!("f{index}");
        client.send(json!({"type":"follow_up","id":id,"message":format!("queued {index}")}));
        assert_eq!(client.response(&id)["success"], true);
    }
    client
        .send(json!({"type":"follow_up","id":"overflow","message":"blocked","dedup_key":"q-full"}));
    let overflow = client.response("overflow");
    assert_eq!(overflow["success"], false);
    assert!(
        overflow["data"]["error"]
            .as_str()
            .unwrap()
            .contains("queue must have space")
    );

    client.send(json!({"type":"abort","id":"abort"}));
    assert_eq!(client.response("abort")["data"]["aborted"], true);
    client.settled("cancelled");
    drop(held);

    // 队列已清空：被拒绝的 key 仍可正常准入，说明拒绝没有消耗它。
    client.send(json!({"type":"prompt","id":"reuse","message":"retry","dedup_key":"q-full"}));
    let reuse = client.response("reuse");
    assert_eq!(reuse["success"], true);
    assert_eq!(reuse["data"]["status"], "started");
    let (stream, _) = client.request();
    reply(stream, None);
    client.settled("completed");
}

/// queued 状态的 dedup key 在真正轮到它时必须真实执行一次，执行后 key 变为
/// completed 并继续阻止重放；受理入队那一刻就拥有权威 `Queued` 句柄，且不
/// 会改写仍处于 active 的原任务身份/阶段。
#[test]
fn queued_dedup_follow_up_really_executes_when_it_becomes_active() {
    let mut client = RpcClient::configured_with_persist(false, "custom", &[], true);
    let (held, _) = prompt(&mut client, "hold");
    // 原任务已真实准入：journal run 已绑定真实 task id 与 running 阶段。
    let parent = run_with_goal(&journal_runs(&client), "hold").clone();
    assert_eq!(parent["canonical_phase"], "running", "{parent}");
    let parent_task = parent["task_id"].as_str().unwrap().to_owned();

    client
        .send(json!({"type":"follow_up","id":"q","message":"keyed-work","dedup_key":"queued-job"}));
    let queued = client.response("q");
    assert_eq!(queued["success"], true);
    assert_eq!(queued["data"]["queued"], 1);
    assert_eq!(queued["data"]["dedup_key"], "queued-job");
    let task_id = queued["data"]["task_id"].as_str().unwrap().to_owned();
    assert_ne!(task_id, parent_task);

    // 受理入队那一刻就是权威 `Queued`：ledger 的 canonical_phase 立即为 queued，
    // 而不是等 pop 才登记。
    let entry = dedup_entries(&client)
        .into_iter()
        .find(|entry| entry["key"] == "queued-job")
        .expect("the accepted follow_up must be recorded");
    assert_eq!(entry["state"], "queued");
    assert_eq!(entry["canonical_phase"], "queued", "{entry}");
    assert_eq!(entry["task_id"].as_str().unwrap(), task_id);

    // 排队登记与投影绝不改写正在运行的原任务身份/阶段，也不提前派发 HTTP。
    let runs = journal_runs(&client);
    let parent_now = run_with_goal(&runs, "hold");
    assert_eq!(parent_now["canonical_phase"], "running", "{parent_now}");
    assert_eq!(parent_now["task_id"].as_str().unwrap(), parent_task);
    assert!(
        runs.iter().all(|run| run["task_id"] != task_id.as_str()),
        "a queued sibling must not open a journal run before it is admitted: {runs:?}"
    );
    assert!(client.requests.try_recv().is_err());

    // 仍处于 queued：同 key 再次排队只会合并回放，不增长队列、不抢占执行权。
    client
        .send(json!({"type":"follow_up","id":"q2","message":"duplicate","dedup_key":"queued-job"}));
    let duplicate = client.response("q2");
    assert_eq!(duplicate["data"]["status"], "deduplicated");
    assert_eq!(duplicate["data"]["state"], "queued");
    assert_eq!(duplicate["data"]["task_id"].as_str().unwrap(), task_id);
    client.send(json!({"type":"get_state","id":"state"}));
    assert_eq!(client.response("state")["data"]["followUpCount"], 1);

    // 初始回合结束后，排队中的 keyed 回合必须变成 active 并真正调用模型，
    // 且沿用受理时的同一个真实 task id 打开 journal run。
    reply(held, None);
    let (stream, request) = client.request();
    assert!(
        request.to_string().contains("keyed-work"),
        "queued turn must really execute: {request}"
    );
    let started = run_with_goal(&journal_runs(&client), "keyed-work").clone();
    assert_eq!(started["task_id"].as_str().unwrap(), task_id);
    assert_eq!(started["canonical_phase"], "running", "{started}");
    reply(stream, None);
    client.settled("completed");

    // 终态 key 保留：重复提交只回放同一个 task_id。
    client.send(json!({"type":"prompt","id":"again","message":"again","dedup_key":"queued-job"}));
    let again = client.response("again");
    assert_eq!(again["success"], true);
    assert_eq!(again["data"]["status"], "deduplicated");
    assert_eq!(again["data"]["state"], "completed");
    assert_eq!(again["data"]["task_id"].as_str().unwrap(), task_id);
    client.settled("deduplicated");
}

/// 取消后排队的 key 必须保留为 cancelled 终态：既不能重放，也不会从账本消失。
#[test]
fn cancelled_dedup_key_is_retained_and_not_replayed() {
    let mut client = RpcClient::configured_with_persist(false, "custom", &[], true);
    let (held, _) = prompt(&mut client, "hold");
    client
        .send(json!({"type":"follow_up","id":"q","message":"never-runs","dedup_key":"cancel-job"}));
    let queued = client.response("q");
    assert_eq!(queued["success"], true);
    let task_id = queued["data"]["task_id"].as_str().unwrap().to_owned();

    client.send(json!({"type":"abort","id":"abort"}));
    assert_eq!(client.response("abort")["data"]["aborted"], true);
    client.settled("cancelled");
    drop(held);

    // key 保留在落盘账本里，状态为 cancelled。
    let ledger: Value = serde_json::from_str(
        &std::fs::read_to_string(client._dir.path().join(".mio/dedup/task-keys.json")).unwrap(),
    )
    .unwrap();
    let entry = ledger["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["key"] == "cancel-job")
        .expect("cancelled key must stay in the ledger");
    assert_eq!(entry["state"], "cancelled");
    assert_eq!(
        entry["canonical_phase"], "cancelled",
        "the queued cancel must go through the real canonical phase: {entry}"
    );
    assert_eq!(entry["task_id"].as_str().unwrap(), task_id);

    // 相同 key 再次提交：只回放 cancelled，不再执行。
    client.send(json!({"type":"prompt","id":"replay","message":"retry","dedup_key":"cancel-job"}));
    let replay = client.response("replay");
    assert_eq!(replay["success"], true);
    assert_eq!(replay["data"]["status"], "deduplicated");
    assert_eq!(replay["data"]["state"], "cancelled");
    assert_eq!(replay["data"]["task_id"].as_str().unwrap(), task_id);
    client.settled("deduplicated");
}

/// 无 key 的排队回合在受理时就必须持有唯一真实句柄：应答回传真实 canonical
/// task id（绝不附带 dedup_key），排队期零 HTTP 派发，开始执行后 journal 绑定
/// 同一 id 不变。
#[test]
fn unkeyed_queued_follow_up_holds_one_real_handle_and_keeps_its_id() {
    let mut client = RpcClient::configured_with_persist(false, "custom", &[], true);
    let (held, _) = prompt(&mut client, "hold");
    let parent_task = run_with_goal(&journal_runs(&client), "hold")["task_id"]
        .as_str()
        .unwrap()
        .to_owned();

    client.send(json!({"type":"follow_up","id":"u","message":"unkeyed-work"}));
    let queued = client.response("u");
    assert_eq!(queued["success"], true);
    assert_eq!(queued["data"]["queued"], 1);
    let task_id = queued["data"]["task_id"]
        .as_str()
        .expect("an unkeyed queued turn must report its real canonical task id")
        .to_owned();
    assert!(!task_id.is_empty());
    assert_ne!(task_id, parent_task);
    assert!(
        queued["data"].get("dedup_key").is_none(),
        "an unkeyed turn must not pretend to be deduplicated: {queued}"
    );

    // 排队期零提前派发：没有第二个模型请求，也没有为它提前打开 journal run。
    assert!(client.requests.try_recv().is_err());
    let runs = journal_runs(&client);
    assert_eq!(run_with_goal(&runs, "hold")["canonical_phase"], "running");
    assert!(
        runs.iter().all(|run| run["task_id"] != task_id.as_str()),
        "the unkeyed queued turn must not have started yet: {runs:?}"
    );

    // 轮到它时沿用同一真实 id 打开 journal run 并执行一次。
    reply(held, None);
    let (stream, request) = client.request();
    assert!(request.to_string().contains("unkeyed-work"), "{request}");
    let started = run_with_goal(&journal_runs(&client), "unkeyed-work").clone();
    assert_eq!(
        started["task_id"].as_str().unwrap(),
        task_id,
        "the journal run must keep the id minted at acceptance"
    );
    assert_eq!(started["canonical_phase"], "running", "{started}");
    reply(stream, None);
    client.settled("completed");
    let completed = run_with_goal(&journal_runs(&client), "unkeyed-work").clone();
    assert_eq!(completed["task_id"].as_str().unwrap(), task_id);
}

/// 取消排队的无 key 回合沿用受理时持有的同一真实句柄：不重新注册、不重铸 ID，
/// 取消后不派发、不开 run，后续无 key 回合铸出不同的真实 id。
#[test]
fn cancelled_unkeyed_queued_turn_reuses_its_accepted_handle() {
    let mut client = RpcClient::configured_with_persist(false, "custom", &[], true);
    let (held, _) = prompt(&mut client, "hold");
    client.send(json!({"type":"follow_up","id":"u","message":"cancelled-unkeyed"}));
    let queued = client.response("u");
    assert_eq!(queued["success"], true);
    let task_id = queued["data"]["task_id"].as_str().unwrap().to_owned();

    client.send(json!({"type":"abort","id":"abort"}));
    assert_eq!(client.response("abort")["data"]["aborted"], true);
    client.settled("cancelled");
    drop(held);

    // 取消不派发、不开 run；无 key 回合没有 legacy key 记录可查。
    assert!(client.requests.try_recv().is_err());
    let runs = journal_runs(&client);
    assert!(
        runs.iter().all(|run| run["task_id"] != task_id.as_str()),
        "a cancelled queued turn must not open a journal run: {runs:?}"
    );

    // 取消后再排一个无 key 回合：必须铸出不同的真实 id，不复用被取消的身份。
    let (held, _) = prompt(&mut client, "hold-again");
    client.send(json!({"type":"follow_up","id":"u2","message":"fresh-unkeyed"}));
    let fresh = client.response("u2");
    assert_eq!(fresh["success"], true);
    assert_ne!(fresh["data"]["task_id"].as_str().unwrap(), task_id);
    client.send(json!({"type":"abort","id":"abort2"}));
    assert_eq!(client.response("abort2")["data"]["aborted"], true);
    client.settled("cancelled");
    drop(held);
}
