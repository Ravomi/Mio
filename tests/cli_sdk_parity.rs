//! CLI 与 Rust SDK 在同一真实 loopback 模型任务上的工具语义对照。
//!
//! 本文件只做一件事：让 `mio` CLI（`--json ...`）与 `mio-coding-agent`
//! Rust SDK（`run_with_tools` + `RunTools`）跑**同一个**固定假模型任务。
//! 模型服务是本地 loopback mock，两端各用一个隔离的临时工作区，seed 文件
//! 字节完全相同。
//!
//! mock 依次强制：真实 `read seed.txt` -> 用真实 read 结果派生的内容
//! `write result.txt` -> 最终文本结束。write 内容不是硬编码：handler 解析
//! 上一轮真实工具结果 `{"output": "..."}` 后再派生，绕过 read 就无法产生
//! 正确内容。
//!
//! 证据只来自真实通道：
//! - CLI：`--json` JSONL 事件（`tool_finished` / `run_observation` /
//!   `read_observation` / `text` / `done`）。
//! - SDK：`Host::on_event` 与 `Host::on_run_observation` 的真实事件与运行观察。
//! - mock：它实际收到的模型请求里的 provider `tool_call_id`、授权头与 read 结果。
//!
//! 断言的是语义而非字面请求 JSON：两端读写操作序列、模型派发次数、相同最终
//! 文件字节、seed 未改、最终回复、provider 自有 toolCallId、两端不同的真实
//! run UUID，以及无重复写副作用。
//!
//! 诚实边界：
//! - 只覆盖 `Provider::Custom` 的 OpenAI 兼容 `/v1` loopback 路径；不访问任何
//!   真实 Provider，不使用真实密钥。
//! - 不覆盖也不宣称真实 Ravomi 宿主进程内集成或 GUI/账户路径已验证。
//! - SDK 侧 `scheduler: None`（不引入共享限额），因此不宣称跨进程配额一致。
//! - `--no-shell` 与 `read,write` 过滤之外的工具语义不在本对照范围内。

#[path = "../src/test_http.rs"]
mod test_http;

use mio_coding_agent::{
    AgentEvent, Config, Host, HostFuture, Provider, RunObservation, RunObservationKind,
    RunObservationStatus, RunOptions, RunTools, Services, ToolFilter, policy, run_with_tools,
};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const MODEL: &str = "parity-loopback-model";
const API_KEY: &str = "local-fake-parity-key";
const PROMPT: &str = "Read seed.txt, then write the derived result to result.txt.";
const SEED_FILE: &str = "seed.txt";
const RESULT_FILE: &str = "result.txt";
const SEED_CONTENT: &str = "PARITY-SEED-MARKER\n";
const READ_CALL_ID: &str = "call-provider-read-seed";
const WRITE_CALL_ID: &str = "call-provider-write-result";
const FINAL_REPLY: &str = "parity task complete";

const CLI_TIMEOUT: Duration = Duration::from_secs(60);
const SDK_TIMEOUT: Duration = Duration::from_secs(60);

/// The allowlist for the second parity task. It deliberately names
/// `bash`/`shell`/`workspace`, so the only thing that can refuse command
/// execution is the shared `without_shell` rule, never the allowlist itself.
const NO_SHELL_ALLOWLIST: &str = "workspace,bash,shell,read,write";
const NO_SHELL_PROMPT: &str =
    "Try every shell route, then read seed.txt and write the derived result to result.txt.";
/// A marker that would exist if any shell command ever executed in the workspace.
const SHELL_SENTINEL_FILE: &str = "shared-no-shell-sentinel.txt";
const SHELL_SENTINEL_MARKER: &str = "shared-no-shell-sentinel";
const SHELL_COMMAND: &str = "printf shared-no-shell-sentinel > shared-no-shell-sentinel.txt";
const WORKSPACE_SHELL_CALL_ID: &str = "call-provider-workspace-shell";
const WORKSPACE_BASH_CALL_ID: &str = "call-provider-workspace-bash";
const SHELL_DENIAL_CALL_IDS: [&str; 2] = [WORKSPACE_SHELL_CALL_ID, WORKSPACE_BASH_CALL_ID];

/// 从真实 read 结果派生写内容。mock 与断言用同一函数，保证双方都比较真实
/// 往返后的字节，而不是各自硬编码一个常量。
fn derive_result(read_output: &str) -> String {
    let trimmed = read_output.trim();
    let reversed: String = trimmed.chars().rev().collect();
    format!("derived::{}::{}", trimmed.len(), reversed)
}

/// 真实模型请求的 mock 观察记录。
#[derive(Default)]
struct MockTrace {
    requests: usize,
    authorizations: Vec<Option<String>>,
    /// 每个请求携带的 tool 结果消息里的 `tool_call_id` 序列。
    tool_result_ids: Vec<Vec<String>>,
    /// handler 从真实 read 工具结果解析出的内容。
    read_output: Option<String>,
    /// handler 用 `read_output` 派生并派发给模型的 write 内容。
    write_content: Option<String>,
}

struct MockServer {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn spawn_task_server() -> (MockServer, Arc<Mutex<MockTrace>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback mock");
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let trace = Arc::new(Mutex::new(MockTrace::default()));
    let thread_stop = stop.clone();
    let thread_trace = trace.clone();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut index = 0usize;
        while !thread_stop.load(Ordering::SeqCst) && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut socket, _)) => {
                    socket
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    socket
                        .set_write_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    let (header, request) = test_http::read_json(&mut socket);
                    let authorization = header.lines().find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.trim()
                                .eq_ignore_ascii_case("authorization")
                                .then(|| value.trim().to_owned())
                        })
                    });
                    {
                        let mut trace = thread_trace.lock().unwrap();
                        trace.requests = index + 1;
                        trace.authorizations.push(authorization);
                    }
                    respond_task(&mut socket, &request, &thread_trace);
                    index += 1;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(_) => return,
            }
        }
    });
    (MockServer { address, stop }, trace)
}

fn respond_task(socket: &mut TcpStream, request: &Value, trace: &Mutex<MockTrace>) {
    let messages = request["messages"].as_array().cloned().unwrap_or_default();
    let tool_messages: Vec<Value> = messages
        .into_iter()
        .filter(|message| message["role"] == "tool")
        .collect();
    let ids: Vec<String> = tool_messages
        .iter()
        .filter_map(|message| message["tool_call_id"].as_str().map(str::to_owned))
        .collect();
    trace.lock().unwrap().tool_result_ids.push(ids);
    match tool_messages.len() {
        0 => {
            let delta = json!({
                "role": "assistant",
                "tool_calls": [tool_call(0, "read", READ_CALL_ID, json!({"path": SEED_FILE}))],
            });
            write_sse(socket, delta, json!("tool_calls"));
        }
        1 => {
            // 写内容只能来自上一轮真实 read 的工具结果，不是硬编码常量。
            let read_output = parse_tool_output(&tool_messages[0]["content"]).unwrap_or_default();
            let derived = derive_result(&read_output);
            {
                let mut trace = trace.lock().unwrap();
                trace.read_output = Some(read_output);
                trace.write_content = Some(derived.clone());
            }
            let delta = json!({
                "role": "assistant",
                "tool_calls": [tool_call(
                    0,
                    "write",
                    WRITE_CALL_ID,
                    json!({"path": RESULT_FILE, "content": derived}),
                )],
            });
            write_sse(socket, delta, json!("tool_calls"));
        }
        _ => write_text(socket, FINAL_REPLY),
    }
}

fn parse_tool_output(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => {
            let parsed: Value = serde_json::from_str(text).ok()?;
            parsed
                .get("output")
                .and_then(Value::as_str)
                .map(str::to_owned)
        }
        Value::Object(map) => map.get("output").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

fn sse_chunk(delta: Value, finish: Value) -> String {
    json!({
        "id": "parity-mock",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": MODEL,
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

// ---------------------------------------------------------------------------
// Second parity task: a shell-permitting allowlist plus the shared no-shell rule
// ---------------------------------------------------------------------------

/// Real loopback observations for the no-shell parity task.
#[derive(Default)]
struct NoShellTrace {
    requests: usize,
    authorizations: Vec<Option<String>>,
    /// Per model request: the `(tool_call_id, content)` pairs really sent back.
    tool_results: Vec<Vec<(String, String)>>,
    read_output: Option<String>,
    write_content: Option<String>,
}

fn spawn_no_shell_server() -> (MockServer, Arc<Mutex<NoShellTrace>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback mock");
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let trace = Arc::new(Mutex::new(NoShellTrace::default()));
    let thread_stop = stop.clone();
    let thread_trace = trace.clone();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut index = 0usize;
        while !thread_stop.load(Ordering::SeqCst) && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut socket, _)) => {
                    socket
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    socket
                        .set_write_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    let (header, request) = test_http::read_json(&mut socket);
                    let authorization = header.lines().find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.trim()
                                .eq_ignore_ascii_case("authorization")
                                .then(|| value.trim().to_owned())
                        })
                    });
                    {
                        let mut trace = thread_trace.lock().unwrap();
                        trace.requests = index + 1;
                        trace.authorizations.push(authorization);
                    }
                    respond_no_shell(&mut socket, &request, &thread_trace);
                    index += 1;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(_) => return,
            }
        }
    });
    (MockServer { address, stop }, trace)
}

/// The model first tries three shell routes, then reads and writes through the
/// tools the allowlist still grants.
fn respond_no_shell(socket: &mut TcpStream, request: &Value, trace: &Mutex<NoShellTrace>) {
    let messages = request["messages"].as_array().cloned().unwrap_or_default();
    let tool_messages: Vec<Value> = messages
        .into_iter()
        .filter(|message| message["role"] == "tool")
        .collect();
    let pairs: Vec<(String, String)> = tool_messages
        .iter()
        .map(|message| {
            (
                message["tool_call_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                tool_result_text(message),
            )
        })
        .collect();
    trace.lock().unwrap().tool_results.push(pairs);
    match tool_messages.len() {
        0 => {
            // The aggregate `workspace` tool is advertised even under
            // `without_shell`, and its schema names the `shell`/`bash`
            // operations. The literal `bash`/`shell` tool names are no longer
            // advertised, so the model can only reach the gate here.
            let delta = json!({
                "role": "assistant",
                "tool_calls": [
                    tool_call(
                        0,
                        "workspace",
                        WORKSPACE_SHELL_CALL_ID,
                        json!({"operation": "shell", "command": SHELL_COMMAND}),
                    ),
                    tool_call(
                        1,
                        "workspace",
                        WORKSPACE_BASH_CALL_ID,
                        json!({"operation": "bash", "command": SHELL_COMMAND}),
                    ),
                ],
            });
            write_sse(socket, delta, json!("tool_calls"));
        }
        2 => {
            let delta = json!({
                "role": "assistant",
                "tool_calls": [tool_call(0, "read", READ_CALL_ID, json!({"path": SEED_FILE}))],
            });
            write_sse(socket, delta, json!("tool_calls"));
        }
        3 => {
            let read_output = tool_messages
                .iter()
                .find(|message| message["tool_call_id"].as_str() == Some(READ_CALL_ID))
                .and_then(|message| parse_tool_output(&message["content"]))
                .unwrap_or_default();
            let derived = derive_result(&read_output);
            {
                let mut trace = trace.lock().unwrap();
                trace.read_output = Some(read_output);
                trace.write_content = Some(derived.clone());
            }
            let delta = json!({
                "role": "assistant",
                "tool_calls": [tool_call(
                    0,
                    "write",
                    WRITE_CALL_ID,
                    json!({"path": RESULT_FILE, "content": derived}),
                )],
            });
            write_sse(socket, delta, json!("tool_calls"));
        }
        _ => write_text(socket, FINAL_REPLY),
    }
}

/// Read a tool-result message body as text without inventing content.
fn tool_result_text(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        other => other.to_string(),
    }
}

/// 以真实 `mio` 二进制跑一次任务，按有界超时收集 `--json` 事件。
fn run_cli(address: SocketAddr, workspace: &Path, home: &Path) -> Vec<Value> {
    run_cli_with(address, workspace, home, "read,write", true, PROMPT)
}

/// 与 [`run_cli`] 相同，但显式给定工具集、`--no-shell` 与 prompt。
fn run_cli_with(
    address: SocketAddr,
    workspace: &Path,
    home: &Path,
    tools: &str,
    no_shell: bool,
    prompt: &str,
) -> Vec<Value> {
    let base_url = format!("http://{address}/v1");
    let mut command = Command::new(env!("CARGO_BIN_EXE_mio"));
    // 清掉继承的 MIO_*、*API_KEY* 与代理设置，避免真实配置/凭据/代理进入子进程。
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.starts_with("MIO_")
            || name.contains("API_KEY")
            || name.to_lowercase().contains("proxy")
        {
            command.env_remove(key);
        }
    }
    let mut args: Vec<String> = vec![
        "--cwd".into(),
        workspace.to_str().expect("workspace path is UTF-8").into(),
        "--provider".into(),
        "custom".into(),
        "--model".into(),
        MODEL.into(),
        "--base-url".into(),
        base_url,
        "--json".into(),
        "--no-context".into(),
        "--no-mcp".into(),
        "--no-session".into(),
        "--no-shared-limits".into(),
        "--context-window".into(),
        "32768".into(),
    ];
    if no_shell {
        args.push("--no-shell".into());
    }
    args.extend(["--tools".into(), tools.into(), "-p".into(), prompt.into()]);
    let mut child = command
        .current_dir(workspace)
        .env("HOME", home)
        .env("MIO_CONFIG_DIR", home)
        .env("MIO_PROVIDER", "custom")
        .env("MIO_API_KEY", API_KEY)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mio CLI");

    let stdout = child.stdout.take().expect("CLI stdout pipe");
    let stderr = child.stderr.take().expect("CLI stderr pipe");
    let stdout_reader = std::thread::spawn(move || drain_pipe(stdout));
    let stderr_reader = std::thread::spawn(move || drain_pipe(stderr));

    // 有界等待进程退出；超时先强杀回收，再读取已排空的管道，绝不无限等待。
    let deadline = Instant::now() + CLI_TIMEOUT;
    loop {
        if child.try_wait().expect("wait for mio CLI").is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let stderr = stderr_reader.join().unwrap_or_default();
            panic!(
                "CLI run exceeded {CLI_TIMEOUT:?}: stderr={}",
                String::from_utf8_lossy(&stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let stdout = stdout_reader.join().expect("CLI stdout reader");
    let stderr = stderr_reader.join().expect("CLI stderr reader");
    let stderr_text = String::from_utf8_lossy(&stderr).into_owned();
    let events: Vec<Value> = String::from_utf8_lossy(&stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<Value>(line)
                .unwrap_or_else(|error| panic!("non-JSON CLI line ({error}): {line}"))
        })
        .collect();
    let event_types: Vec<&Value> = events.iter().map(|event| &event["type"]).collect();
    let failures: Vec<&Value> = events
        .iter()
        .filter(|event| event["type"] == "error" || event["type"] == "diagnostic")
        .collect();
    assert!(
        events.iter().any(|event| event["type"] == "done"),
        "CLI never reached the done event: stderr={stderr_text} types={event_types:?} failures={failures:?}"
    );
    events
}

fn drain_pipe(mut stream: impl Read) -> Vec<u8> {
    let mut buffer = Vec::new();
    let _ = stream.read_to_end(&mut buffer);
    buffer
}

/// 只记录真实事件与运行观察的 SDK 宿主。shell 不可用：一旦被调用就 panic，
/// 用来证明 `read,write` 过滤之外没有被执行的命令。
#[derive(Default)]
struct RecordingHost {
    events: Mutex<Vec<AgentEvent>>,
    observations: Mutex<Vec<RunObservation>>,
    approvals: Mutex<Vec<String>>,
}

impl Host for RecordingHost {
    fn shell(&self, _command: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        panic!("parity SDK host has no shell; the shared ToolFilter must exclude shell/bash")
    }

    fn on_event(&self, event: AgentEvent) {
        self.events.lock().unwrap().push(event);
    }

    fn on_run_observation(&self, observation: &RunObservation) {
        self.observations.lock().unwrap().push(observation.clone());
    }

    fn approve_tool_binding(
        &self,
        operation: &str,
        target: &str,
        binding: &policy::ApprovalBinding,
    ) -> HostFuture<Result<(), String>> {
        self.approvals
            .lock()
            .unwrap()
            .push(format!("{operation}:{target}:{}", binding.params_sha256));
        Box::pin(async { Ok(()) })
    }
}

/// 用与 CLI 相同的 ToolFilter / prompt / 模型 / 假 key / context window 跑一次 SDK，
/// 返回真实事件与真实运行观察。
fn run_sdk(address: SocketAddr, workspace: &Path) -> (Vec<AgentEvent>, Vec<RunObservation>) {
    let filter = ToolFilter::from_cli(Some("read,write"), None, false).expect("tool filter");
    let (events, observations, _) = run_sdk_with(address, workspace, filter, PROMPT);
    (events, observations)
}

/// 与 [`run_sdk`] 相同，但显式给定 `ToolFilter` 与 prompt，并额外返回真实审批记录。
fn run_sdk_with(
    address: SocketAddr,
    workspace: &Path,
    tool_filter: ToolFilter,
    prompt: &str,
) -> (Vec<AgentEvent>, Vec<RunObservation>, Vec<String>) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let host = Arc::new(RecordingHost::default());
    let config = Config {
        provider: Provider::Custom,
        base_url: format!("http://{address}/v1"),
        model: MODEL.to_owned(),
        api_key: API_KEY.to_owned(),
    };
    let tools = RunTools {
        options: RunOptions {
            // 与 CLI 的 --context-window 32768 一致；默认无检查点。
            context_window: Some(32_768),
            ..RunOptions::default()
        },
        tool_filter,
        ..RunTools::default()
    };
    runtime.block_on(async {
        let run = run_with_tools(
            config,
            Vec::new(),
            prompt,
            workspace,
            host.clone(),
            Services::default(),
            tools,
        );
        let history = tokio::time::timeout(SDK_TIMEOUT, run)
            .await
            .expect("SDK run exceeded its bounded timeout")
            .expect("SDK run must succeed");
        assert!(!history.is_empty(), "SDK run must return its new history");
    });
    let events = host.events.lock().unwrap().clone();
    let observations = host.observations.lock().unwrap().clone();
    let approvals = host.approvals.lock().unwrap().clone();
    (events, observations, approvals)
}

#[test]
fn cli_and_sdk_share_tool_semantics_on_one_loopback_task() {
    let expected_write = derive_result(SEED_CONTENT);

    // --- CLI 端：隔离工作区 + 临时 HOME/配置 ---------------------------------
    let (cli_server, cli_trace) = spawn_task_server();
    let cli_workspace = tempfile::tempdir().unwrap();
    let cli_home = tempfile::tempdir().unwrap();
    std::fs::write(cli_workspace.path().join(SEED_FILE), SEED_CONTENT).unwrap();
    let cli_events = run_cli(cli_server.address, cli_workspace.path(), cli_home.path());

    // --- SDK 端：另一隔离工作区，相同 seed 文件字节 --------------------------
    let (sdk_server, sdk_trace) = spawn_task_server();
    let sdk_workspace = tempfile::tempdir().unwrap();
    std::fs::write(sdk_workspace.path().join(SEED_FILE), SEED_CONTENT).unwrap();
    let (sdk_events, sdk_observations) = run_sdk(sdk_server.address, sdk_workspace.path());

    // --- CLI 证据：真实操作序列、派发次数、provider toolCallId、回复 ---------
    let cli_ops: Vec<&str> = cli_events
        .iter()
        .filter(|event| event["type"] == "tool_finished")
        .filter_map(|event| event["data"]["operation"].as_str())
        .collect();
    assert_eq!(cli_ops, vec!["read", "write"], "CLI tool sequence");
    assert!(
        !cli_events.iter().any(|event| {
            event["type"] == "tool_finished"
                && matches!(event["data"]["operation"].as_str(), Some("shell" | "bash"))
        }),
        "the CLI must never run shell/bash"
    );

    let cli_models: Vec<&Value> = cli_events
        .iter()
        .filter(|event| {
            event["type"] == "run_observation" && event["data"]["kind"] == "model_dispatched"
        })
        .collect();
    assert_eq!(cli_models.len(), 3, "CLI model dispatch count");
    let cli_run_id = cli_models[0]["data"]["runId"]
        .as_str()
        .expect("CLI run_observation must carry the real run UUID")
        .to_owned();
    assert!(
        cli_models
            .iter()
            .all(|event| event["data"]["runId"] == json!(cli_run_id)),
        "all CLI dispatches must share one real run UUID"
    );

    let cli_reads: Vec<&Value> = cli_events
        .iter()
        .filter(|event| event["type"] == "read_observation")
        .collect();
    assert_eq!(cli_reads.len(), 1, "CLI must observe exactly one real read");
    assert_eq!(cli_reads[0]["data"]["path"], json!(SEED_FILE));
    assert_eq!(
        cli_reads[0]["data"]["toolCallId"],
        json!(READ_CALL_ID),
        "the observed id must be the provider's own tool_call_id"
    );
    assert_eq!(cli_reads[0]["data"]["runId"], json!(cli_run_id));

    let cli_reply: String = cli_events
        .iter()
        .filter(|event| event["type"] == "text")
        .filter_map(|event| event["data"].as_str())
        .collect();
    assert_eq!(cli_reply, FINAL_REPLY, "CLI final reply");

    // --- SDK 证据：真实事件序列、派发次数、provider toolCallId、终态 ----------
    let sdk_ops: Vec<&str> = sdk_events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolFinished { operation, .. } => Some(operation.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(sdk_ops, vec!["read", "write"], "SDK tool sequence");
    assert!(
        !sdk_events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolFinished { operation, .. } if operation == "shell" || operation == "bash"
        )),
        "the SDK must never run shell/bash"
    );

    let sdk_models: Vec<&RunObservation> = sdk_observations
        .iter()
        .filter(|observation| observation.kind == RunObservationKind::ModelDispatched)
        .collect();
    assert_eq!(sdk_models.len(), 3, "SDK model dispatch count");
    let sdk_run_id = sdk_models[0].run_id;
    assert!(
        sdk_models
            .iter()
            .all(|observation| observation.run_id == sdk_run_id),
        "all SDK dispatches must share one real run UUID"
    );

    let sdk_dispatches: Vec<&RunObservation> = sdk_observations
        .iter()
        .filter(|observation| observation.kind == RunObservationKind::ToolDispatched)
        .collect();
    assert_eq!(sdk_dispatches.len(), 2, "SDK tool dispatch count");
    let mut sdk_call_ids: Vec<&str> = sdk_dispatches
        .iter()
        .filter_map(|observation| observation.tool_call_id.as_deref())
        .collect();
    sdk_call_ids.sort_unstable();
    assert_eq!(
        sdk_call_ids,
        vec![READ_CALL_ID, WRITE_CALL_ID],
        "SDK dispatches must carry the provider's own tool_call_ids"
    );
    assert!(
        sdk_dispatches
            .iter()
            .all(|observation| observation.run_id == sdk_run_id)
    );
    assert_eq!(
        sdk_dispatches
            .iter()
            .filter(|observation| observation.tool_call_id.as_deref() == Some(READ_CALL_ID))
            .count(),
        1,
        "read must be dispatched exactly once"
    );
    assert_eq!(
        sdk_dispatches
            .iter()
            .filter(|observation| observation.tool_call_id.as_deref() == Some(WRITE_CALL_ID))
            .count(),
        1,
        "write must be dispatched exactly once"
    );

    let sdk_outcomes: Vec<&RunObservation> = sdk_observations
        .iter()
        .filter(|observation| observation.kind == RunObservationKind::ToolOutcome)
        .collect();
    assert_eq!(sdk_outcomes.len(), 2, "SDK must resolve both tool calls");
    assert!(
        sdk_outcomes
            .iter()
            .all(|observation| observation.status == Some(RunObservationStatus::Success))
    );

    let sdk_reply: String = sdk_events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(sdk_reply, FINAL_REPLY, "SDK final reply");

    let sdk_finished: Vec<&RunObservation> = sdk_observations
        .iter()
        .filter(|observation| observation.kind == RunObservationKind::RunFinished)
        .collect();
    assert_eq!(sdk_finished.len(), 1, "SDK run must settle exactly once");
    assert_eq!(sdk_finished[0].status, Some(RunObservationStatus::Success));

    // --- 两端真实 run UUID 必须是不同的 v4 UUID ------------------------------
    let cli_uuid = parse_uuid(&cli_run_id);
    assert_eq!(
        cli_uuid.get_version_num(),
        4,
        "CLI run id must be a real v4 UUID"
    );
    assert_eq!(
        sdk_run_id.get_version_num(),
        4,
        "SDK run id must be a real v4 UUID"
    );
    assert_ne!(
        cli_uuid, sdk_run_id,
        "CLI and SDK runs are separate real runs and must not share a UUID"
    );

    // --- 相同最终文件字节、seed 未改、无重复写副作用 -------------------------
    assert_eq!(
        std::fs::read(cli_workspace.path().join(SEED_FILE)).unwrap(),
        SEED_CONTENT.as_bytes(),
        "CLI seed file must be untouched"
    );
    assert_eq!(
        std::fs::read(sdk_workspace.path().join(SEED_FILE)).unwrap(),
        SEED_CONTENT.as_bytes(),
        "SDK seed file must be untouched"
    );
    let cli_result = std::fs::read_to_string(cli_workspace.path().join(RESULT_FILE)).unwrap();
    let sdk_result = std::fs::read_to_string(sdk_workspace.path().join(RESULT_FILE)).unwrap();
    assert_eq!(cli_result, expected_write, "CLI result file bytes");
    assert_eq!(sdk_result, expected_write, "SDK result file bytes");
    assert_eq!(
        cli_result.as_bytes(),
        sdk_result.as_bytes(),
        "both ends must produce identical final file bytes"
    );

    // --- mock 侧证据：provider 自有 toolCallId、真实 read 结果、派生写内容 ---
    let cli_mock = cli_trace.lock().unwrap();
    assert_eq!(
        cli_mock.requests, 3,
        "CLI must issue exactly three model requests"
    );
    assert_eq!(
        cli_mock.tool_result_ids.len(),
        3,
        "every CLI request is recorded"
    );
    assert!(
        cli_mock.tool_result_ids[0].is_empty(),
        "the first request has no tool result yet"
    );
    assert_eq!(
        cli_mock.tool_result_ids[1],
        vec![READ_CALL_ID.to_owned()],
        "the read result must round-trip the provider's own tool_call_id"
    );
    assert_eq!(
        cli_mock.tool_result_ids[2],
        vec![READ_CALL_ID.to_owned(), WRITE_CALL_ID.to_owned()],
        "the write result must round-trip the provider's own tool_call_id"
    );
    assert_eq!(
        cli_mock.tool_result_ids[2]
            .iter()
            .filter(|id| id.as_str() == WRITE_CALL_ID)
            .count(),
        1,
        "write side effect must happen exactly once"
    );
    assert_eq!(
        cli_mock.read_output.as_deref().map(str::trim),
        Some(SEED_CONTENT.trim()),
        "the write content must be derived from the real read result"
    );
    assert_eq!(
        cli_mock.write_content.as_deref(),
        Some(expected_write.as_str()),
        "mock dispatched the derived write content"
    );
    let expected_auth = format!("Bearer {API_KEY}");
    assert!(
        cli_mock
            .authorizations
            .iter()
            .all(|auth| auth.as_deref() == Some(expected_auth.as_str())),
        "CLI must send the same fixed fake key on every loopback request"
    );
    drop(cli_mock);

    let sdk_mock = sdk_trace.lock().unwrap();
    assert_eq!(
        sdk_mock.requests, 3,
        "SDK must issue exactly three model requests"
    );
    assert_eq!(
        sdk_mock.tool_result_ids.len(),
        3,
        "every SDK request is recorded"
    );
    assert!(sdk_mock.tool_result_ids[0].is_empty());
    assert_eq!(sdk_mock.tool_result_ids[1], vec![READ_CALL_ID.to_owned()]);
    assert_eq!(
        sdk_mock.tool_result_ids[2],
        vec![READ_CALL_ID.to_owned(), WRITE_CALL_ID.to_owned()]
    );
    assert_eq!(
        sdk_mock.tool_result_ids[2]
            .iter()
            .filter(|id| id.as_str() == WRITE_CALL_ID)
            .count(),
        1,
        "write side effect must happen exactly once"
    );
    assert_eq!(
        sdk_mock.read_output.as_deref().map(str::trim),
        Some(SEED_CONTENT.trim())
    );
    assert_eq!(
        sdk_mock.write_content.as_deref(),
        Some(expected_write.as_str())
    );
    assert!(
        sdk_mock
            .authorizations
            .iter()
            .all(|auth| auth.as_deref() == Some(expected_auth.as_str())),
        "SDK must send the same fixed fake key on every loopback request"
    );
    drop(sdk_mock);

    // 语义一致性总断言：两端真实记录必须逐项相等。
    assert_eq!(
        cli_ops, sdk_ops,
        "both ends must run the same tool sequence"
    );
    assert_eq!(cli_models.len(), sdk_models.len());
    assert_eq!(cli_result, sdk_result);
}

/// 第二个 loopback 任务：工具集显式包含 `bash`/`shell`/`workspace`，CLI 用
/// `--no-shell`，SDK 用 `ToolFilter::without_shell()`。模型先尝试两条 shell
/// 别名路径（`workspace {operation:shell}` 与 `{operation:bash}`），再走
/// read/write。
/// 断言两端都只在统一 policy 层拒绝、零派发、零副作用，且 file 工具仍真实执行。
#[test]
fn cli_and_sdk_deny_shell_from_an_allowlist_that_permits_it() {
    let expected_write = derive_result(SEED_CONTENT);
    let expected_auth = format!("Bearer {API_KEY}");

    // --- CLI 端：隔离工作区 + 临时 HOME/配置，显式允许 shell 的 allowlist ---
    let (cli_server, cli_trace) = spawn_no_shell_server();
    let cli_workspace = tempfile::tempdir().unwrap();
    let cli_home = tempfile::tempdir().unwrap();
    std::fs::write(cli_workspace.path().join(SEED_FILE), SEED_CONTENT).unwrap();
    let cli_events = run_cli_with(
        cli_server.address,
        cli_workspace.path(),
        cli_home.path(),
        NO_SHELL_ALLOWLIST,
        true,
        NO_SHELL_PROMPT,
    );

    // --- SDK 端：同一 allowlist 再加 `without_shell()`，另一个隔离工作区 -------
    let sdk_filter = ToolFilter::from_cli(Some(NO_SHELL_ALLOWLIST), None, false)
        .expect("allowlist")
        .without_shell();
    let (sdk_server, sdk_trace) = spawn_no_shell_server();
    let sdk_workspace = tempfile::tempdir().unwrap();
    std::fs::write(sdk_workspace.path().join(SEED_FILE), SEED_CONTENT).unwrap();
    let (sdk_events, sdk_observations, sdk_approvals) = run_sdk_with(
        sdk_server.address,
        sdk_workspace.path(),
        sdk_filter,
        NO_SHELL_PROMPT,
    );

    // --- CLI 事件：只真实执行 read/write，从不执行 shell/bash ---
    assert!(
        cli_events.iter().any(|event| event["type"] == "done"),
        "CLI must reach the done event"
    );
    let cli_ops: Vec<&str> = cli_events
        .iter()
        .filter(|event| event["type"] == "tool_finished")
        .filter_map(|event| event["data"]["operation"].as_str())
        .collect();
    assert_eq!(
        cli_ops,
        vec!["read", "write"],
        "CLI must only run the allowed file tools"
    );
    let cli_reply: String = cli_events
        .iter()
        .filter(|event| event["type"] == "text")
        .filter_map(|event| event["data"].as_str())
        .collect();
    assert_eq!(cli_reply, FINAL_REPLY, "CLI final reply");

    // --- SDK 事件：同样的真实操作序列与最终回复 ---
    let sdk_ops: Vec<&str> = sdk_events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolFinished { operation, .. } => Some(operation.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        sdk_ops,
        vec!["read", "write"],
        "SDK must only run the allowed file tools"
    );
    let sdk_reply: String = sdk_events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(sdk_reply, FINAL_REPLY, "SDK final reply");
    let sdk_finished: Vec<&RunObservation> = sdk_observations
        .iter()
        .filter(|observation| observation.kind == RunObservationKind::RunFinished)
        .collect();
    assert_eq!(sdk_finished.len(), 1, "SDK run must settle exactly once");
    assert_eq!(sdk_finished[0].status, Some(RunObservationStatus::Success));

    // --- 统一 policy：shell/bash 聚合路径在派发前被拒，理由来自共享 filter ---
    let cli_mock = cli_trace.lock().unwrap();
    assert_eq!(cli_mock.requests, 4, "CLI must issue four model requests");
    assert_eq!(cli_mock.tool_results.len(), 4);
    assert!(cli_mock.tool_results[0].is_empty());
    assert_eq!(
        cli_mock.tool_results[1].len(),
        SHELL_DENIAL_CALL_IDS.len(),
        "the second CLI request must carry both denial results"
    );
    for call_id in SHELL_DENIAL_CALL_IDS {
        let content = cli_mock.tool_results[1]
            .iter()
            .find(|(id, _)| id.as_str() == call_id)
            .map(|(_, content)| content.as_str())
            .unwrap_or_else(|| panic!("the denial result for {call_id} must reach the model"));
        assert!(
            content.contains("Tool policy denied")
                && content.contains("tool filter does not allow"),
            "CLI {call_id} must be refused by the shared policy gate, not an allowlist or host string: {content}"
        );
        assert!(
            !content.contains("Shell disabled by --no-shell"),
            "the host no-shell string must never be the reason: {content}"
        );
    }
    assert_eq!(
        cli_mock.tool_results[2].len(),
        3,
        "both denials plus the read"
    );
    assert_eq!(
        cli_mock.tool_results[3].len(),
        4,
        "both denials plus read and write"
    );
    assert_eq!(
        cli_mock.tool_results[3]
            .iter()
            .filter(|(id, _)| id.as_str() == WRITE_CALL_ID)
            .count(),
        1,
        "write must round-trip exactly once"
    );
    let cli_read_content = cli_mock.tool_results[2]
        .iter()
        .find(|(id, _)| id.as_str() == READ_CALL_ID)
        .map(|(_, content)| content.clone())
        .expect("the real read result must be paired by its provider id");
    assert_eq!(
        parse_tool_output(&Value::String(cli_read_content))
            .as_deref()
            .map(str::trim),
        Some(SEED_CONTENT.trim()),
        "the CLI read must really return the seed bytes"
    );
    assert_eq!(
        cli_mock.read_output.as_deref().map(str::trim),
        Some(SEED_CONTENT.trim())
    );
    assert_eq!(
        cli_mock.write_content.as_deref(),
        Some(expected_write.as_str())
    );
    assert!(
        cli_mock
            .authorizations
            .iter()
            .all(|auth| auth.as_deref() == Some(expected_auth.as_str())),
        "CLI must send the same fixed fake key on every loopback request"
    );
    drop(cli_mock);

    let sdk_mock = sdk_trace.lock().unwrap();
    assert_eq!(sdk_mock.requests, 4, "SDK must issue four model requests");
    assert_eq!(sdk_mock.tool_results.len(), 4);
    assert!(sdk_mock.tool_results[0].is_empty());
    for call_id in SHELL_DENIAL_CALL_IDS {
        let content = sdk_mock.tool_results[1]
            .iter()
            .find(|(id, _)| id.as_str() == call_id)
            .map(|(_, content)| content.as_str())
            .unwrap_or_else(|| panic!("the SDK denial result for {call_id} must reach the model"));
        assert!(
            content.contains("Tool policy denied")
                && content.contains("tool filter does not allow"),
            "SDK {call_id} must be refused by the shared policy gate: {content}"
        );
    }
    assert_eq!(
        sdk_mock.read_output.as_deref().map(str::trim),
        Some(SEED_CONTENT.trim())
    );
    assert_eq!(
        sdk_mock.write_content.as_deref(),
        Some(expected_write.as_str())
    );
    assert!(
        sdk_mock
            .authorizations
            .iter()
            .all(|auth| auth.as_deref() == Some(expected_auth.as_str())),
        "SDK must send the same fixed fake key on every loopback request"
    );
    drop(sdk_mock);

    // --- 准入状态机：被拒的 shell 调用从未派发 ---
    let sdk_dispatched: Vec<&RunObservation> = sdk_observations
        .iter()
        .filter(|observation| observation.kind == RunObservationKind::ToolDispatched)
        .collect();
    let mut dispatched_ids: Vec<&str> = sdk_dispatched
        .iter()
        .filter_map(|observation| observation.tool_call_id.as_deref())
        .collect();
    dispatched_ids.sort_unstable();
    assert_eq!(
        dispatched_ids,
        vec![READ_CALL_ID, WRITE_CALL_ID],
        "only allowed file calls may be dispatched"
    );
    let shell_outcomes: Vec<&RunObservation> = sdk_observations
        .iter()
        .filter(|observation| {
            observation.kind == RunObservationKind::ToolOutcome
                && observation
                    .tool_call_id
                    .as_deref()
                    .is_some_and(|id| SHELL_DENIAL_CALL_IDS.contains(&id))
        })
        .collect();
    assert_eq!(
        shell_outcomes.len(),
        2,
        "each denied shell call resolves once"
    );
    assert!(
        shell_outcomes
            .iter()
            .all(|observation| observation.status == Some(RunObservationStatus::Error)),
        "denied shell calls must resolve with an error status"
    );

    // --- 零审批：只有真实的 write 请求审批，任何 shell 路径都没有 ---
    assert_eq!(
        sdk_approvals.len(),
        1,
        "only the allowed write may ask for approval: {sdk_approvals:?}"
    );
    assert!(
        sdk_approvals[0].starts_with("write:"),
        "the single approval must be the write: {sdk_approvals:?}"
    );
    assert!(
        sdk_approvals
            .iter()
            .all(|approval| !approval.contains("shell") && !approval.contains("bash")),
        "no shell alias may reach approval: {sdk_approvals:?}"
    );

    // --- 零文件副作用：sentinel 未创建，seed 未改，read/write 结果字节相同 ---
    for (label, workspace) in [("CLI", cli_workspace.path()), ("SDK", sdk_workspace.path())] {
        assert!(
            !workspace.join(SHELL_SENTINEL_FILE).exists(),
            "{label} shell sentinel must not exist"
        );
        for entry in std::fs::read_dir(workspace).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            assert!(
                !name.contains(SHELL_SENTINEL_MARKER),
                "{label} must not contain a shell side effect: {name}"
            );
        }
        assert_eq!(
            std::fs::read(workspace.join(SEED_FILE)).unwrap(),
            SEED_CONTENT.as_bytes(),
            "{label} seed file must be untouched"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join(RESULT_FILE)).unwrap(),
            expected_write,
            "{label} must still complete the allowed write"
        );
    }

    // 语义一致性总断言：两端真实工具序列与副作用一致。
    assert_eq!(
        cli_ops, sdk_ops,
        "both ends must run the same tool sequence"
    );
}

fn parse_uuid(text: &str) -> uuid::Uuid {
    uuid::Uuid::parse_str(text).unwrap_or_else(|error| panic!("not a real UUID `{text}`: {error}"))
}
