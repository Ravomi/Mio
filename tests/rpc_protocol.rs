//! 协议协商与慢消费者背压的真实子进程 + loopback mock 验收。
//!
//! 覆盖 DES-14/16：`initialize` 版本协商（拒绝不支持的版本、不假协商）、
//! schemaVersion 存在于每个 JSON 行、慢消费者下低价值流式事件被显式计数丢弃、
//! `abort` 真正取消模型连接、恢复读取后可审计终态、结构化事件在队列满时立即
//! fail-closed 而非等待消费者，且不伪装成功。

use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
#[path = "../src/test_http.rs"]
mod test_http;

const EVENT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy)]
enum ReplyProtocol {
    OpenAi,
    Anthropic,
    Gemini,
    Ollama,
}

/// Loopback 模型服务。`stream = true` 时持续输出文本增量直到连接被取消。
struct Mock {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    streamed: Arc<AtomicU64>,
    requests: mpsc::Receiver<(String, Value)>,
    _request_tx: mpsc::Sender<(String, Value)>,
}

impl Mock {
    fn start(stream: bool) -> Self {
        Self::start_with_reasoning(stream, false)
    }

    fn start_with_reasoning(stream: bool, reasoning: bool) -> Self {
        Self::start_with_reply_delay(stream, reasoning, Duration::ZERO)
    }

    fn start_with_reply_delay(stream: bool, reasoning: bool, reply_delay: Duration) -> Self {
        Self::start_with_protocol(stream, reasoning, reply_delay, ReplyProtocol::OpenAi)
    }

    fn start_native(protocol: ReplyProtocol) -> Self {
        Self::start_with_protocol(false, false, Duration::ZERO, protocol)
    }

    fn start_native_delayed(protocol: ReplyProtocol, reply_delay: Duration) -> Self {
        Self::start_with_protocol(false, false, reply_delay, protocol)
    }

    fn start_with_protocol(
        stream: bool,
        reasoning: bool,
        reply_delay: Duration,
        protocol: ReplyProtocol,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::new(AtomicBool::new(false));
        let streamed = Arc::new(AtomicU64::new(0));
        let (request_tx, requests) = mpsc::channel();
        let accept_stop = stop.clone();
        let accept_cancelled = cancelled.clone();
        let accept_streamed = streamed.clone();
        let accept_tx = request_tx.clone();
        std::thread::spawn(move || {
            while !accept_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        let tx = accept_tx.clone();
                        let cancelled = accept_cancelled.clone();
                        let streamed = accept_streamed.clone();
                        let stop = accept_stop.clone();
                        std::thread::spawn(move || {
                            if stream {
                                serve_stream(socket, tx, stop, cancelled, streamed);
                            } else {
                                serve_reply(socket, tx, reasoning, reply_delay, protocol);
                            }
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            address,
            stop,
            cancelled,
            streamed,
            requests,
            _request_tx: request_tx,
        }
    }

    fn wait_request(&self, timeout: Duration) -> Value {
        self.wait_request_with_path(timeout).1
    }

    fn wait_request_with_path(&self, timeout: Duration) -> (String, Value) {
        self.requests.recv_timeout(timeout).expect("model request")
    }

    fn no_request(&self) {
        assert!(
            self.requests.try_recv().is_err(),
            "negotiation rejection must not start a model request"
        );
    }

    fn streamed_bytes(&self) -> u64 {
        self.streamed.load(Ordering::SeqCst)
    }

    fn wait_cancelled(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.cancelled.load(Ordering::SeqCst) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        self.cancelled.load(Ordering::SeqCst)
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn sse(delta: Value, finish: Value) -> String {
    json!({
        "id": "test",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "test",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    })
    .to_string()
}

fn native_reply(protocol: ReplyProtocol) -> (&'static str, String) {
    match protocol {
        ReplyProtocol::Anthropic => (
            "text/event-stream",
            [
                r#"event: message_start\ndata: {"message":{"id":"msg_1","model":"test","role":"assistant","type":"message","content":[],"stop_reason":null,"usage":{"input_tokens":10,"output_tokens":1}},"type":"message_start"}"#,
                r#"event: content_block_start\ndata: {"content_block":{"text":"","type":"text"},"index":0,"type":"content_block_start"}"#,
                r#"event: content_block_delta\ndata: {"delta":{"text":"ok","type":"text_delta"},"index":0,"type":"content_block_delta"}"#,
                r#"event: content_block_stop\ndata: {"index":0,"type":"content_block_stop"}"#,
                r#"event: message_delta\ndata: {"delta":{"stop_reason":"end_turn","stop_sequence":null},"type":"message_delta","usage":{"output_tokens":4}}"#,
                r#"event: message_stop\ndata: {"type":"message_stop"}"#,
            ]
            .iter()
            .map(|event| event.replace("\\n", "\n"))
            .collect::<Vec<_>>()
            .join("\n\n")
                + "\n\n",
        ),
        ReplyProtocol::Gemini => (
            "text/event-stream",
            format!(
                "data: {}\n\n",
                json!({
                    "candidates": [{"content": {"parts": [{"text": "ok"}], "role": "model"}, "finishReason": "STOP", "index": 0}],
                    "modelVersion": "test", "responseId": "r1",
                    "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 4, "totalTokenCount": 14}
                })
            ),
        ),
        ReplyProtocol::Ollama => (
            "application/x-ndjson",
            format!(
                "{}\n{}\n",
                json!({"model":"test","message":{"role":"assistant","content":"ok"},"done":false}),
                json!({"model":"test","message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","prompt_eval_count":10,"eval_count":4})
            ),
        ),
        ReplyProtocol::OpenAi => unreachable!(),
    }
}

fn serve_reply(
    mut socket: TcpStream,
    requests: mpsc::Sender<(String, Value)>,
    reasoning: bool,
    reply_delay: Duration,
    protocol: ReplyProtocol,
) {
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let (headers, request) = test_http::read_json(&mut socket);
    let path = headers.split_whitespace().nth(1).unwrap().to_owned();
    let _ = requests.send((path, request));
    std::thread::sleep(reply_delay);
    if !matches!(protocol, ReplyProtocol::OpenAi) {
        let (content_type, body) = native_reply(protocol);
        let _ = write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.flush();
        return;
    }
    let reasoning_chunk = reasoning.then(|| {
        format!(
            "data: {}\n\n",
            sse(
                json!({"role": "assistant", "reasoning_content": "considering"}),
                Value::Null
            )
        )
    });
    let body = format!(
        "{}data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        reasoning_chunk.as_deref().unwrap_or_default(),
        sse(json!({"role": "assistant", "content": "ok"}), Value::Null),
        sse(json!({}), json!("stop")),
    );
    let _ = write!(
        socket,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.flush();
}

/// 无限文本流：客户端取消（半关闭 / 连接重置）后通过 reader 线程观察到 EOF。
fn serve_stream(
    mut socket: TcpStream,
    requests: mpsc::Sender<(String, Value)>,
    stop: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    streamed: Arc<AtomicU64>,
) {
    socket
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let mut reader = socket.try_clone().unwrap();
    let (headers, request) = test_http::read_json(&mut socket);
    let path = headers.split_whitespace().nth(1).unwrap().to_owned();
    let _ = requests.send((path, request));
    let reader_cancelled = cancelled.clone();
    std::thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => {
                    reader_cancelled.store(true, Ordering::SeqCst);
                    return;
                }
                Ok(_) => {}
            }
        }
    });
    let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
    if socket.write_all(headers.as_bytes()).is_err() {
        cancelled.store(true, Ordering::SeqCst);
        return;
    }
    let text = "s".repeat(4096);
    loop {
        if stop.load(Ordering::SeqCst) {
            let _ = socket.write_all(b"0\r\n\r\n");
            return;
        }
        let frame_body = format!(
            "data: {}\n\n",
            sse(json!({"role": "assistant", "content": text}), Value::Null)
        );
        let frame = format!("{:x}\r\n{frame_body}\r\n", frame_body.len());
        if socket
            .write_all(frame.as_bytes())
            .and_then(|()| socket.flush())
            .is_err()
        {
            cancelled.store(true, Ordering::SeqCst);
            return;
        }
        streamed.fetch_add(frame_body.len() as u64, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(1));
    }
}

struct Rpc {
    child: Child,
    events: Option<mpsc::Receiver<Value>>,
    _dir: tempfile::TempDir,
}

impl Drop for Rpc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Rpc {
    fn start(mock: &Mock) -> Self {
        Self::start_with(mock, false)
    }

    /// Start the RPC binary with persistence enabled, so the durable task
    /// evidence (journal + dedup ledger) can be inspected after exit.
    fn start_persistent(mock: &Mock) -> Self {
        Self::start_with(mock, true)
    }

    fn start_with(mock: &Mock, persist: bool) -> Self {
        Self::start_with_model(mock, persist, "test")
    }

    fn start_with_model(mock: &Mock, persist: bool, model: &str) -> Self {
        Self::start_with_provider_model(mock, persist, "custom", model)
    }

    fn start_with_provider_model(mock: &Mock, persist: bool, provider: &str, model: &str) -> Self {
        Self::start_with_provider_model_context(mock, persist, provider, model, None)
    }

    fn start_with_provider_model_context(
        mock: &Mock,
        persist: bool,
        provider: &str,
        model: &str,
        context_window: Option<u32>,
    ) -> Self {
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
        command
            .current_dir(dir.path())
            .env("HOME", dir.path())
            .env("MIO_CONFIG_DIR", dir.path())
            .env("MIO_API_KEY", "local-test-credential")
            .args([
                "--mode",
                "rpc",
                "--no-context",
                "--no-mcp",
                "--provider",
                provider,
                "--model",
                model,
                "--base-url",
                &format!(
                    "http://{}{}",
                    mock.address,
                    if provider == "google" {
                        "/v1beta/openai"
                    } else {
                        "/v1"
                    }
                ),
            ]);
        if !persist {
            command.arg("--no-session");
        }
        if let Some(context_window) = context_window {
            command.args(["--context-window", &context_window.to_string()]);
        }
        let child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self {
            events: None,
            child,
            _dir: dir,
        }
    }

    /// 开始读取 stdout（慢消费者测试在 abort 前刻意不读取）。
    fn resume_reading(&mut self) {
        let stdout = self.child.stdout.take().expect("stdout");
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
        self.events = Some(rx);
    }

    fn send(&mut self, value: Value) {
        let stdin = self.child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{value}").unwrap();
        stdin.flush().unwrap();
    }

    fn collect_until(&mut self, predicate: impl Fn(&Value) -> bool) -> Vec<Value> {
        let events = self.events.as_ref().expect("reading not started");
        let deadline = Instant::now() + EVENT_TIMEOUT;
        let mut seen = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "RPC event deadline: {seen:?}");
            let event = events
                .recv_timeout(remaining)
                .unwrap_or_else(|_| panic!("RPC event deadline: {seen:?}"));
            let matched = predicate(&event);
            seen.push(event);
            if matched {
                return seen;
            }
        }
    }

    fn response(&mut self, id: &str) -> Value {
        self.collect_until(|event| event["id"] == id).pop().unwrap()
    }

    fn settled(&mut self, status: &str) -> Vec<Value> {
        self.collect_until(|event| event["type"] == "agent_settled" && event["status"] == status)
    }

    fn close_stdin(&mut self) {
        self.child.stdin.take();
    }

    fn wait_exit(&mut self, expect_success: bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert_eq!(status.success(), expect_success, "unexpected exit status");
                return;
            }
            assert!(Instant::now() < deadline, "RPC did not exit after EOF");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn stderr_text(&mut self) -> String {
        let mut text = String::new();
        if let Some(mut stderr) = self.child.stderr.take() {
            let _ = stderr.read_to_string(&mut text);
        }
        text
    }
}

fn assert_schema_version(events: &[Value]) {
    for event in events {
        assert_eq!(
            event["schemaVersion"], 1,
            "every JSON line must carry schemaVersion: {event}"
        );
    }
}

#[test]
fn model_image_capability_tracks_successful_switches_and_rejected_switches() {
    let mock = Mock::start(false);
    let mut client = Rpc::start_with_provider_model(&mock, false, "openai", "test");
    client.resume_reading();

    client.send(json!({"type":"get_state","id":"initial"}));
    let initial = client.response("initial");
    assert_eq!(initial["data"]["modelCapabilities"]["imageInput"], false);

    client.send(json!({"type":"set_model","id":"vision","model":"gpt-4o"}));
    let vision = client.response("vision");
    assert_eq!(vision["success"], true, "{vision}");
    assert_eq!(vision["data"]["modelCapabilities"]["imageInput"], true);
    assert_eq!(
        vision["data"]["modelCapabilities"]["source"],
        "localCatalog"
    );
    client.send(json!({"type":"get_state","id":"vision-state"}));
    assert_eq!(
        client.response("vision-state")["data"]["modelCapabilities"],
        vision["data"]["modelCapabilities"]
    );

    client.send(json!({"type":"set_model","id":"invalid","model":""}));
    assert_eq!(client.response("invalid")["success"], false);
    client.send(json!({"type":"get_state","id":"after-invalid"}));
    assert_eq!(
        client.response("after-invalid")["data"]["modelCapabilities"]["imageInput"],
        true
    );

    client.send(json!({"type":"set_model","id":"text","model":"test"}));
    let text = client.response("text");
    assert_eq!(text["success"], true, "{text}");
    assert_eq!(text["data"]["modelCapabilities"]["imageInput"], false);
    client.send(json!({"type":"get_state","id":"text-state"}));
    assert_eq!(
        client.response("text-state")["data"]["modelCapabilities"],
        text["data"]["modelCapabilities"]
    );
    mock.no_request();
}

#[test]
fn message_commands_reject_unrecognized_attachments_before_dispatch() {
    let mock = Mock::start(false);
    let mut client = Rpc::start(&mock);
    client.resume_reading();

    client.send(json!({"type":"get_state","id":"state"}));
    let state = client.response("state");
    assert_eq!(
        state["data"]["protocol"]["capabilities"]["imageInput"],
        true
    );
    assert_eq!(state["data"]["modelCapabilities"]["imageInput"], false);
    assert_eq!(state["data"]["modelCapabilities"]["source"], "localCatalog");
    client.send(json!({"type":"initialize","id":"init","schema_versions":[1],"capabilities":["imageSteering"]}));
    let negotiated = client.response("init");
    assert_eq!(negotiated["success"], true);
    assert_eq!(
        negotiated["data"]["unsupportedCapabilities"],
        json!(["imageSteering"])
    );

    for (command, field) in [
        ("prompt", "attachments"),
        ("steer", "attachments"),
        ("follow_up", "content"),
    ] {
        let id = format!("reject-{command}");
        let mut request =
            json!({"type":command,"id":id,"message":"text only","dedup_key":"image-job"});
        if command == "steer" {
            request.as_object_mut().unwrap().remove("dedup_key");
        }
        request[field] = json!([{"mime_type":"image/png","data":"aGVsbG8="}]);
        client.send(request);
        let response = client.response(&id);
        assert_eq!(response["success"], false, "{response}");
        assert!(
            response["data"]["error"]
                .as_str()
                .unwrap()
                .contains("no task was started"),
            "{response}"
        );
    }
    mock.no_request();

    // Rejection does not consume the dedup key or poison a later text turn.
    client.send(json!({"type":"prompt","id":"text","message":"hello","dedup_key":"image-job"}));
    assert_eq!(client.response("text")["success"], true);
    let request = mock.wait_request(Duration::from_secs(10));
    assert!(request["messages"].to_string().contains("hello"));
}

#[test]
fn initialize_negotiates_v1_rejects_unknown_and_blocks_new_tasks() {
    let mock = Mock::start(false);
    let mut client = Rpc::start(&mock);
    client.resume_reading();

    client.send(json!({"type": "get_state", "id": "s"}));
    let discovery = client.response("s");
    let protocol = &discovery["data"]["protocol"];
    assert_eq!(protocol["eventEnvelope"], "legacy");
    assert_eq!(protocol["schemaVersion"], 1);
    assert_eq!(protocol["envelopeVersions"], json!([1]));
    assert_eq!(protocol["capabilities"]["initialize"], true);
    assert_eq!(protocol["correlation"]["sessionId"], true);
    assert_eq!(protocol["correlation"]["runId"], true);
    assert_eq!(protocol["correlation"]["turnId"], true);
    assert_eq!(protocol["correlation"]["toolCallId"], true);
    assert_eq!(protocol["correlation"]["subagentId"], true);
    assert_eq!(protocol["correlation"]["parentRunId"], true);
    assert_eq!(discovery["schemaVersion"], 1);

    // 不支持的版本必须显式拒绝，不静默降级。
    client.send(json!({"type": "initialize", "id": "i-bad", "schema_versions": [2]}));
    let rejected = client.response("i-bad");
    assert_eq!(rejected["success"], false);
    assert_eq!(rejected["data"]["status"], "rejected");
    assert_eq!(rejected["data"]["negotiated"], false);
    assert!(rejected["data"]["schemaVersion"].is_null());

    // 拒绝后新任务不得启动，也不得产生模型请求。
    client.send(json!({"type": "get_state", "id": "s2"}));
    let after_reject = client.response("s2");
    assert_eq!(
        after_reject["data"]["protocol"]["negotiation"]["rejected"],
        true
    );
    assert_eq!(after_reject["data"]["protocol"]["eventEnvelope"], "legacy");
    client.send(json!({"type": "prompt", "id": "p-blocked", "message": "should not run"}));
    let blocked = client.response("p-blocked");
    assert_eq!(blocked["success"], false);
    assert!(
        blocked["data"]["error"]
            .as_str()
            .unwrap()
            .contains("negotiation"),
        "{blocked}"
    );
    mock.no_request();

    // 重新用受支持的版本协商成功，并切到 v1 封套。
    client.send(json!({
        "type": "initialize",
        "id": "i-ok",
        "schema_versions": [1, 2],
        "capabilities": ["steering", "eventReplay", "not-a-capability"],
    }));
    let negotiated = client.response("i-ok");
    assert_eq!(negotiated["success"], true);
    assert_eq!(negotiated["data"]["status"], "negotiated");
    assert_eq!(negotiated["data"]["negotiated"], true);
    assert_eq!(negotiated["data"]["schemaVersion"], 1);
    assert_eq!(negotiated["data"]["envelope"], "v1");
    assert!(
        negotiated["data"]["grantedCapabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|capability| capability == "steering")
    );
    assert!(
        negotiated["data"]["unsupportedCapabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|capability| capability == "eventReplay")
    );
    assert!(
        negotiated["data"]["unknownCapabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|capability| capability == "not-a-capability")
    );

    client.send(json!({"type": "get_state", "id": "s3"}));
    let v1 = client.response("s3");
    assert_eq!(v1["data"]["protocol"]["eventEnvelope"], "v1");
    assert_eq!(v1["data"]["protocol"]["negotiation"]["initialized"], true);
    // v1 封套只附真实存在的关联 ID，未知的一律 null。
    assert!(v1["sessionId"].is_string());
    assert!(v1["runId"].is_null());
    assert!(v1["turnId"].is_null());
    assert!(v1["toolCallId"].is_null());

    client.send(json!({"type": "prompt", "id": "p", "message": "hello"}));
    assert_eq!(client.response("p")["success"], true);
    let request = mock.wait_request(Duration::from_secs(10));
    assert!(request["messages"].to_string().contains("hello"));
    let seen = client.settled("completed");
    assert_schema_version(&seen);
    // 关联 ID 来自真实运行观察：run_started 的 runId 必须与 done 的 runId 一致。
    let run_started = seen
        .iter()
        .find(|event| event["type"] == "run_observation" && event["data"]["kind"] == "run_started")
        .expect("v1 run_started observation");
    let run_id = run_started["runId"].clone();
    assert!(run_id.is_string(), "{run_started}");
    assert_eq!(run_started["sessionId"], v1["sessionId"]);
    assert!(
        seen.iter()
            .any(|event| event["type"] == "done" && event["runId"] == run_id),
        "v1 done event must carry the run's real UUID: {seen:?}"
    );
    assert!(
        seen.iter()
            .any(|event| event["type"] == "done" && event["sessionId"].is_string()),
        "v1 done event must carry a real session id: {seen:?}"
    );
    assert!(
        !seen
            .iter()
            .any(|event| event["type"] == "agent_settled" && event["status"] == "error"),
        "{seen:?}"
    );
    client.close_stdin();
    client.wait_exit(true);
}

#[test]
fn legacy_client_keeps_existing_fields_and_only_adds_schema_version() {
    let mock = Mock::start(false);
    let mut client = Rpc::start(&mock);
    client.resume_reading();

    client.send(json!({"type": "prompt", "id": "p", "message": "legacy"}));
    let started = client.response("p");
    assert_eq!(started["type"], "response");
    assert_eq!(started["command"], "prompt");
    assert_eq!(started["id"], "p");
    assert_eq!(started["success"], true);
    assert_eq!(started["data"]["status"], "started");
    assert_eq!(started["schemaVersion"], 1);
    // Legacy 封套不被静默改写：不追加关联字段。
    assert!(started.get("sessionId").is_none());

    let request = mock.wait_request(Duration::from_secs(10));
    assert!(request["messages"].to_string().contains("legacy"));
    let seen = client.settled("completed");
    assert_schema_version(&seen);
    assert!(
        !seen.iter().any(|event| event.get("sessionId").is_some()),
        "legacy envelope must not silently add correlation fields: {seen:?}"
    );
    client.close_stdin();
    client.wait_exit(true);
}

#[test]
fn provider_reasoning_is_separate_from_answer_text() {
    let mock = Mock::start_with_reasoning(false, true);
    let mut client = Rpc::start(&mock);
    client.resume_reading();
    client.send(json!({"type":"initialize","id":"i","schema_versions":[1],"capabilities":["reasoningEvents"]}));
    let initialized = client.response("i");
    assert_eq!(initialized["success"], true);
    assert_eq!(
        initialized["data"]["grantedCapabilities"],
        json!(["reasoningEvents"])
    );
    client.send(json!({"type":"prompt","id":"p","message":"hello"}));
    assert_eq!(client.response("p")["success"], true);
    mock.wait_request(Duration::from_secs(10));
    let seen = client.settled("completed");
    let reasoning = seen
        .iter()
        .find(|event| event["type"] == "reasoning")
        .expect("reasoning event");
    assert_eq!(reasoning["data"]["text"], "considering");
    assert!(reasoning["data"]["part"].is_u64());
    assert!(reasoning["runId"].is_string());
    assert!(reasoning["turnId"].is_u64());
    assert!(
        seen.iter()
            .any(|event| event["type"] == "text" && event["data"] == "ok")
    );
    assert!(
        !seen
            .iter()
            .any(|event| event["type"] == "text" && event["data"] == "considering")
    );
    client.close_stdin();
    client.wait_exit(true);
}

#[test]
fn rpc_image_prompt_is_sent_saved_and_replayed() {
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+XcZUAAAAASUVORK5CYII=";
    let mock = Mock::start(false);
    let mut client = Rpc::start_with_provider_model(&mock, true, "openai", "gpt-4o");
    client.resume_reading();
    client.send(
        json!({"type":"initialize","id":"i","schema_versions":[1],"capabilities":["imageInput"]}),
    );
    let initialized = client.response("i");
    assert_eq!(initialized["success"], true);
    assert_eq!(
        initialized["data"]["grantedCapabilities"],
        json!(["imageInput"])
    );

    client.send(json!({"type":"prompt","id":"bad","message":"describe","images":[{"mime_type":"image/jpeg","data_base64":PNG}],"dedup_key":"image-job"}));
    let bad = client.response("bad");
    assert_eq!(bad["success"], false);
    mock.no_request();

    client.send(json!({"type":"prompt","id":"p","message":"describe","images":[{"mime_type":"image/png","data_base64":PNG}],"dedup_key":"image-job"}));
    let started = client.response("p");
    assert_eq!(started["success"], true, "{started}");
    let first_request = mock.wait_request(Duration::from_secs(10));
    let image_part = first_request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "user")
        .unwrap()["content"][1]
        .clone();
    assert_eq!(image_part["type"], "image_url");
    assert_eq!(
        image_part["image_url"]["url"],
        format!("data:image/png;base64,{PNG}")
    );
    client.settled("completed");

    client.send(json!({"type":"get_state","id":"saved"}));
    let saved = client.response("saved");
    assert_eq!(saved["data"]["modelCapabilities"]["imageInput"], true);
    let path = saved["data"]["sessionFile"].as_str().expect("session path");
    let session = std::fs::read_to_string(path).expect("saved session");
    assert!(
        session.contains(PNG),
        "the image must be persisted for replay"
    );
    let reloaded = mio_coding_agent::session::Session::load(Path::new(path)).unwrap();
    assert!(reloaded.history.iter().any(|message| {
        matches!(message, mio_coding_agent::Message::User { content }
            if content.iter().any(|part| matches!(part, mio_core::message::UserContent::Image(_))))
    }));

    client.send(json!({"type":"prompt","id":"next","message":"continue"}));
    assert_eq!(client.response("next")["success"], true);
    let second_request = mock.wait_request(Duration::from_secs(10));
    assert!(
        second_request.to_string().contains(PNG),
        "saved image must reach the provider again"
    );
    client.settled("completed");
    client.close_stdin();
    client.wait_exit(true);
}

#[test]
fn native_rpc_image_prompt_is_sent_saved_and_replayed() {
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+XcZUAAAAASUVORK5CYII=";
    for (provider, model, protocol, context_window) in [
        (
            "anthropic",
            "claude-sonnet-4-5",
            ReplyProtocol::Anthropic,
            None,
        ),
        ("google", "gemini-2.5-pro", ReplyProtocol::Gemini, None),
        ("ollama", "gemma3", ReplyProtocol::Ollama, Some(131_072)),
    ] {
        let mock = Mock::start_native(protocol);
        let mut client =
            Rpc::start_with_provider_model_context(&mock, true, provider, model, context_window);
        client.resume_reading();
        client.send(json!({"type":"initialize","id":"i","schema_versions":[1],"capabilities":["imageInput"]}));
        let initialized = client.response("i");
        assert_eq!(initialized["success"], true, "{provider}: {initialized}");
        assert_eq!(
            initialized["data"]["grantedCapabilities"],
            json!(["imageInput"])
        );

        client.send(json!({"type":"prompt","id":"p","message":"describe","images":[{"mime_type":"image/png","data_base64":PNG}]}));
        let started = client.response("p");
        assert_eq!(started["success"], true, "{provider}: {started}");
        let (path, first) = mock.wait_request_with_path(Duration::from_secs(10));
        match protocol {
            ReplyProtocol::Anthropic => {
                assert_eq!(path, "/v1/messages");
                let image = &first["messages"][0]["content"][1];
                assert_eq!(image["type"], "image");
                assert_eq!(image["source"]["type"], "base64");
                assert_eq!(image["source"]["media_type"], "image/png");
                assert_eq!(image["source"]["data"], PNG);
            }
            ReplyProtocol::Gemini => {
                assert!(
                    path.starts_with("/v1beta/models/gemini-2.5-pro:streamGenerateContent"),
                    "{path}"
                );
                let image = &first["contents"][0]["parts"][1]["inlineData"];
                assert_eq!(image["mimeType"], "image/png");
                assert_eq!(image["data"], PNG);
            }
            ReplyProtocol::Ollama => {
                assert_eq!(path, "/api/chat");
                let user = first["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|message| message["role"] == "user")
                    .unwrap();
                assert_eq!(user["images"][0], PNG);
            }
            ReplyProtocol::OpenAi => unreachable!(),
        }
        client.settled("completed");

        client.send(json!({"type":"get_state","id":"saved"}));
        let saved = client.response("saved");
        let path = saved["data"]["sessionFile"].as_str().expect("session path");
        assert!(
            std::fs::read_to_string(path).unwrap().contains(PNG),
            "{provider}"
        );
        let reloaded = mio_coding_agent::session::Session::load(Path::new(path)).unwrap();
        assert!(reloaded.history.iter().any(|message| {
            matches!(message, mio_coding_agent::Message::User { content }
                if content.iter().any(|part| matches!(part, mio_core::message::UserContent::Image(_))))
        }), "{provider}");

        client.send(json!({"type":"prompt","id":"next","message":"continue"}));
        assert_eq!(client.response("next")["success"], true, "{provider}");
        let (_, replay) = mock.wait_request_with_path(Duration::from_secs(10));
        assert!(
            replay.to_string().contains(PNG),
            "{provider}: saved image must reach the provider again"
        );
        client.settled("completed");
        client.close_stdin();
        client.wait_exit(true);
    }
}

#[test]
fn native_rpc_queued_image_follow_up_reaches_the_next_turn() {
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+XcZUAAAAASUVORK5CYII=";
    for (provider, model, protocol, context_window) in [
        (
            "anthropic",
            "claude-sonnet-4-5",
            ReplyProtocol::Anthropic,
            None,
        ),
        ("google", "gemini-2.5-pro", ReplyProtocol::Gemini, None),
        ("ollama", "gemma3", ReplyProtocol::Ollama, Some(131_072)),
    ] {
        let mock = Mock::start_native_delayed(protocol, Duration::from_millis(250));
        let mut client =
            Rpc::start_with_provider_model_context(&mock, false, provider, model, context_window);
        client.resume_reading();
        client.send(json!({"type":"prompt","id":"p","message":"first"}));
        assert_eq!(client.response("p")["success"], true, "{provider}");
        let (_, first) = mock.wait_request_with_path(Duration::from_secs(10));
        assert!(!first.to_string().contains(PNG), "{provider}");

        client.send(json!({"type":"follow_up","id":"f","message":"describe","images":[{"mime_type":"image/png","data_base64":PNG}]}));
        let queued = client.response("f");
        assert_eq!(queued["success"], true, "{provider}: {queued}");
        assert_eq!(queued["data"]["queued"], 1, "{provider}: {queued}");
        let (_, second) = mock.wait_request_with_path(Duration::from_secs(10));
        assert!(
            second.to_string().contains(PNG),
            "{provider}: queued image was not sent"
        );
        client.settled("completed");
        client.close_stdin();
        client.wait_exit(true);
    }
}

#[test]
fn active_image_steer_is_rejected_and_queued_image_follow_up_runs() {
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+XcZUAAAAASUVORK5CYII=";
    let mock = Mock::start_with_reply_delay(false, false, Duration::from_millis(300));
    let mut client = Rpc::start_with_provider_model(&mock, false, "openai", "gpt-4o");
    client.resume_reading();
    client.send(json!({"type":"prompt","id":"p","message":"first"}));
    assert_eq!(client.response("p")["success"], true);
    mock.wait_request(Duration::from_secs(10));

    let image = json!({"mime_type":"image/png","data_base64":PNG});
    client.send(json!({"type":"steer","id":"s","message":"look at this","images":[image.clone()]}));
    let steer = client.response("s");
    assert_eq!(steer["success"], false, "{steer}");
    assert!(
        steer["data"]["error"]
            .as_str()
            .unwrap()
            .contains("follow_up")
    );

    client.send(json!({"type":"follow_up","id":"f","message":"look at this","images":[image]}));
    let queued = client.response("f");
    assert_eq!(queued["success"], true, "{queued}");
    assert_eq!(queued["data"]["queued"], 1);
    let second_request = mock.wait_request(Duration::from_secs(10));
    assert!(second_request.to_string().contains(PNG));
    client.settled("completed");
    client.close_stdin();
    client.wait_exit(true);
}

#[test]
fn slow_consumer_drops_streaming_but_cancels_work_and_keeps_terminal_truthful() {
    let mock = Mock::start(true);
    let mut client = Rpc::start(&mock);
    // 先不读取 stdout：让 stdout 写阻塞、事件队列填满。
    client.send(json!({"type": "prompt", "id": "p", "message": "stream forever"}));
    let _ = mock.wait_request(Duration::from_secs(10));

    let deadline = Instant::now() + Duration::from_secs(20);
    while mock.streamed_bytes() < 4_000_000 {
        assert!(
            Instant::now() < deadline,
            "mock did not stream enough to fill the pipe and queue"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // 慢消费者不得阻断 abort：模型连接必须被真正取消。
    client.send(json!({"type": "abort", "id": "a"}));
    assert!(
        mock.wait_cancelled(Duration::from_secs(10)),
        "abort must cancel the model connection while stdout is blocked"
    );

    // 恢复读取后可以审计终态。
    client.resume_reading();
    let seen = client
        .collect_until(|event| event["type"] == "agent_settled" && event["status"] == "cancelled");
    assert_schema_version(&seen);
    let settled = seen.last().unwrap();
    assert_ne!(settled["status"], "completed");
    let dropped = settled["droppedEvents"]
        .as_u64()
        .expect("terminal event must report dropped streaming events");
    assert!(
        dropped > 0,
        "slow consumer must be reported explicitly: {settled}"
    );
    assert!(
        seen.iter().any(|event| {
            event["type"] == "response"
                && event["command"] == "abort"
                && event["data"]["aborted"] == true
        }),
        "abort must be acknowledged: {seen:?}"
    );

    client.close_stdin();
    client.wait_exit(true);
}

/// DES-16 回归：停止消费时，结构化（关键）事件不得阻塞或假装成功。
///
/// 队列填满后传输必须立即 fail-closed，让 RPC 进程以非零退出码结束；旧实现在
/// 每个关键事件上等待队列空间最多 5 秒。
#[test]
fn stalled_durable_queue_fails_closed_instead_of_blocking() {
    let mock = Mock::start(false);
    let mut client = Rpc::start(&mock);
    // 刻意不读 stdout：writer 线程卡在管道上，命令应答（关键事件）会填满有界队列。
    // Feed on another thread so a regression that stops reading stdin cannot
    // block this test before its process-exit deadline. Rpc::drop kills/reaps
    // the child on failure, releasing a writer blocked in the pipe.
    let mut stdin = client.child.stdin.take().expect("stdin");
    let feeder = std::thread::spawn(move || {
        for index in 0..2000 {
            let command = json!({"type": "get_state", "id": format!("s{index}")});
            if writeln!(stdin, "{command}").is_err() || stdin.flush().is_err() {
                break;
            }
        }
    });
    client.wait_exit(false);
    feeder.join().unwrap();
    let stderr = client.stderr_text();
    assert!(
        stderr.contains("queue is full"),
        "fail-closed must report the full queue: {stderr}"
    );
    mock.no_request();
}

fn only_journal(root: &Path) -> Value {
    let directory = root.join(".mio/journal");
    let mut files: Vec<_> = std::fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1, "exactly one journal file: {files:?}");
    serde_json::from_slice(&std::fs::read(files.pop().unwrap()).unwrap()).unwrap()
}

/// A running turn whose output transport fails must settle finitely with the
/// real canonical `failed` terminal, both with an explicit dedup key and as an
/// unkeyed task. This is a real loopback subprocess with a real streaming model;
/// the failure is induced only by a stalled consumer, never by the test.
#[test]
fn running_output_failure_settles_finitely_with_and_without_dedup() {
    for dedup_key in [Some("fail-closed-key"), None] {
        let mock = Mock::start(true);
        let mut client = Rpc::start_persistent(&mock);
        let mut prompt = json!({"type":"prompt","id":"p","message":"stream forever"});
        if let Some(key) = dedup_key {
            prompt["dedup_key"] = json!(key);
        }
        client.send(prompt);
        // Never read stdout: the writer blocks in the pipe, the bounded durable
        // queue fills while the turn is still running, and the transport fails
        // closed. Feeding happens on another thread so a regression cannot block
        // this test before its process-exit deadline.
        let mut stdin = client.child.stdin.take().expect("stdin");
        let feeder = std::thread::spawn(move || {
            for index in 0..3000 {
                let command = json!({"type":"get_state","id":format!("s{index}")});
                if writeln!(stdin, "{command}").is_err() || stdin.flush().is_err() {
                    break;
                }
            }
        });
        client.wait_exit(false);
        feeder.join().unwrap();
        let _ = client.stderr_text();
        let root = client._dir.path();

        match dedup_key {
            Some(key) => {
                let ledger: Value = serde_json::from_slice(
                    &std::fs::read(root.join(".mio/dedup/task-keys.json")).unwrap(),
                )
                .unwrap();
                let entry = ledger["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|entry| entry["key"] == key)
                    .expect("the dedup key must be recorded");
                assert_eq!(
                    entry["state"], "failed",
                    "the keyed task must settle failed, not stay queued: {entry}"
                );
                assert_eq!(entry["canonical_phase"], "failed", "{entry}");
            }
            None => {
                let journal = only_journal(root);
                let runs = journal["runs"].as_array().unwrap();
                assert_eq!(runs.len(), 1, "one unkeyed turn: {journal}");
                assert_eq!(runs[0]["state"], "failed", "{journal}");
                assert_eq!(runs[0]["canonical_phase"], "failed", "{journal}");
            }
        }
        drop(mock);
    }
}
