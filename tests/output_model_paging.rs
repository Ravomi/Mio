//! 真实 `mio --json` + loopback Provider 的长输出外置与 `read` 字节分页端到端回归。
//!
//! 这条链路要同时证明：
//!
//! - 临时工作区里的声明式插件真实输出一段确定性的、超过 32 KiB 的 UTF-8 长单行；
//! - 真实 `mio --json -p ...` 子进程把超限输出外置到工作区 `.mio/outputs/<uuid>.stdout`，
//!   并在 `tool_finished` 事件里给出可读引用；
//! - 本地 loopback mock 模型**从真实工具结果**里解析这个引用路径（不硬编码任何 UUID），
//!   再逐轮要求真实 `read` 工具用 `byte_offset` / `byte_limit` 分页，直到
//!   `next_byte_offset` 为 `null`；
//! - mock 每轮都从真实 read 结果重建完整输出；测试断言重建字节与磁盘上的真实产物
//!   **完全一致**，末尾 sentinel 存在，页与页之间既无丢失也无重叠，单页长度受限，
//!   读取至少两页，并且真实完成了后续模型回合。
//!
//! 边界与隔离：mock 只在 `127.0.0.1` 上服务，凭据是固定的假 key；工作区、HOME、
//! 配置目录与 TMPDIR 全部落在各自的临时目录里，并清除继承的 `MIO_*` / API key /
//! proxy 覆盖。CLI 子进程有硬超时，超时会被强杀。loopback mock 只证明协议与分页
//! 接线，**不**代表真实 Provider 的性能或线上行为。

#[path = "../src/test_http.rs"]
mod test_http;

use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// 限定工具名：`plugin__<plugin>__<tool>`。
const PLUGIN_TOOL: &str = "plugin__pager__emit_long";
/// 故意超过单流 8 MiB 硬上限的插件工具名。
const OVERSIZED_TOOL: &str = "plugin__pager__emit_oversized";
/// 确定性多字节 UTF-8 片段；用于证明分页不会把字符切成两半。
const PATTERN: &str = "mio-paging-字节🌊海-";
/// 完整输出必须以此结尾；缺失即说明尾部丢失。
const SENTINEL: &str = "SENTINEL-END-mio-paging-9f3c";
/// 正文按字节截断到该长度，再拼 sentinel；整体仍不超过约 40 KiB。
const TARGET_BYTES: usize = 39 * 1024;
/// 模型请求的单页上限；真实预算更小时工具会进一步收紧。
const PAGE_LIMIT: u64 = 16 * 1024;
/// CLI 子进程硬超时；超时后强杀，避免测试挂死。
const CLI_TIMEOUT: Duration = Duration::from_secs(120);
/// mock accept 循环自限，避免测试异常退出时留下永久线程。
const MOCK_DEADLINE: Duration = Duration::from_secs(90);

/// mock 的逐轮决策；`ToolCall` 模拟模型请求一次工具调用，`Text` 结束该轮。
enum Reply {
    ToolCall {
        name: String,
        id: String,
        arguments: Value,
    },
    Text(String),
}

/// mock 从真实工具结果里累积出来的全部证据。
#[derive(Default)]
struct MockState {
    /// 从真实插件工具结果里解析出的 `.mio/outputs/...` 引用，绝不预置。
    reference: Option<String>,
    /// 下一页期望的起始字节偏移（连续性检查）。
    next_offset: u64,
    /// 真实 read 返回的页对象，按顺序记录。
    pages: Vec<Value>,
    /// 从真实 read 结果逐页重建的完整输出。
    reconstructed: String,
    /// 模型请求总数（真实子进程发出的 HTTP 请求数）。
    turns: usize,
    /// 最后一页（`next_byte_offset == null`）是否已经被观察到。
    finished: bool,
    /// 任何与预期不一致的证据；测试要求它保持 `None`。
    failure: Option<String>,
}

/// 顺序服务请求的本地 mock；Drop 时通知 accept 线程退出。
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
    let thread_stop = Arc::clone(&stop);
    std::thread::spawn(move || {
        let deadline = Instant::now() + MOCK_DEADLINE;
        while !thread_stop.load(Ordering::SeqCst) && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut socket, _)) => {
                    socket
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    socket
                        .set_write_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    let (_, request) = test_http::read_json(&mut socket);
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

fn write_tool_call(socket: &mut TcpStream, name: &str, id: &str, arguments: &Value) {
    write_sse(
        socket,
        json!({
            "role": "assistant",
            "tool_calls": [{
                "index": 0,
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()},
            }],
        }),
        json!("tool_calls"),
    );
}

fn write_text(socket: &mut TcpStream, text: &str) {
    write_sse(
        socket,
        json!({"role": "assistant", "content": text}),
        json!("stop"),
    );
}

/// 取一条消息的纯文本内容；OpenAI 兼容 wire 既可能是字符串，也可能是文本部件数组。
fn message_text(message: &Value) -> Option<String> {
    match message.get("content")? {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => Some(
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

/// 解析插件/工具 payload `{"output": "..."}`；不是该形状就返回 `None`。
fn tool_output_text(text: &str) -> Option<String> {
    let value: Value = serde_json::from_str(text).ok()?;
    value
        .get("output")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// 从真实工具结果文本里提取外置引用，不假设任何 UUID。
fn reference_from_text(text: &str) -> Option<String> {
    const MARKER: &str = "complete output: ";
    let start = text.find(MARKER)?;
    let rest = &text[start + MARKER.len()..];
    let end = rest
        .find(|c: char| c == ';' || c == '"' || c == '\\' || c == ']' || c.is_whitespace())
        .unwrap_or(rest.len());
    let path = rest[..end].trim();
    (!path.is_empty()).then(|| path.to_string())
}

/// 在消息列表里找插件输出的真实引用；同时兼容 `{"output": "..."}` 包装。
fn extract_reference(messages: &[Value]) -> Option<String> {
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        let Some(text) = message_text(message) else {
            continue;
        };
        if let Some(path) = reference_from_text(&text) {
            return Some(path);
        }
        if let Some(output) = tool_output_text(&text)
            && let Some(path) = reference_from_text(&output)
        {
            return Some(path);
        }
    }
    None
}

/// 从工具结果文本里解析 `read` 的字节分页 JSON。
fn page_from_text(text: &str) -> Option<Value> {
    let mut current: Value = serde_json::from_str(text).ok()?;
    for _ in 0..3 {
        if current.get("byte_offset").is_some() && current.get("total_bytes").is_some() {
            return Some(current);
        }
        let inner = current.get("output").and_then(Value::as_str)?;
        current = serde_json::from_str(inner).ok()?;
    }
    None
}

/// 取最新一条可解析的 `read` 字节分页结果。
fn parse_page(messages: &[Value]) -> Option<Value> {
    for message in messages.iter().rev() {
        if message.get("role").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        let Some(text) = message_text(message) else {
            continue;
        };
        if let Some(page) = page_from_text(&text) {
            return Some(page);
        }
    }
    None
}

/// 每轮根据真实工具结果决定模型下一步：先触发插件，再逐页 `read`，最后收尾。
fn decide(state: &mut MockState, messages: &[Value]) -> Reply {
    state.turns += 1;
    if state.reference.is_none() {
        state.reference = extract_reference(messages);
    }
    if state.turns == 1 {
        return Reply::ToolCall {
            name: PLUGIN_TOOL.to_string(),
            id: "call_plugin".to_string(),
            arguments: json!({}),
        };
    }
    if state.finished {
        return Reply::Text("MOCK-EXTRA-TURN".to_string());
    }
    let Some(reference) = state.reference.clone() else {
        state.failure.get_or_insert_with(|| {
            "no `complete output:` reference in the real plugin tool result".to_string()
        });
        return Reply::Text("MOCK-MISSING-REFERENCE".to_string());
    };
    let Some(page) = parse_page(messages) else {
        // 第二轮一定是插件刚跑完、还没有任何 read 结果。
        if state.pages.is_empty() && state.turns == 2 {
            return Reply::ToolCall {
                name: "read".to_string(),
                id: "call_read_0".to_string(),
                arguments: json!({
                    "path": reference.clone(),
                    "byte_offset": 0,
                    "byte_limit": PAGE_LIMIT,
                }),
            };
        }
        state.failure.get_or_insert_with(|| {
            "expected a real read byte page in the tool results".to_string()
        });
        return Reply::Text("MOCK-NO-PAGE".to_string());
    };

    let offset = page["byte_offset"].as_u64().unwrap_or(u64::MAX);
    let text = page["text"].as_str().unwrap_or_default().to_string();
    let next = page["next_byte_offset"].as_u64();
    let total = page["total_bytes"].as_u64().unwrap_or(0);
    if state.pages.is_empty() {
        if offset != 0 {
            state
                .failure
                .get_or_insert_with(|| format!("first byte page must start at 0, got {offset}"));
        }
    } else if offset != state.next_offset {
        state.failure.get_or_insert_with(|| {
            format!(
                "byte page gap: expected offset {}, got {offset}",
                state.next_offset
            )
        });
    }
    if text.is_empty() {
        state
            .failure
            .get_or_insert_with(|| "a byte page returned no text".to_string());
    }
    state.reconstructed.push_str(&text);
    let page_index = state.pages.len();
    state.pages.push(page);
    match next {
        Some(next) => {
            if next <= offset {
                state.failure.get_or_insert_with(|| {
                    format!("byte page next offset {next} did not advance past {offset}")
                });
            }
            state.next_offset = next;
            Reply::ToolCall {
                name: "read".to_string(),
                id: format!("call_read_{}", page_index + 1),
                arguments: json!({
                    "path": reference,
                    "byte_offset": next,
                    "byte_limit": PAGE_LIMIT,
                }),
            }
        }
        None => {
            state.next_offset = total;
            state.finished = true;
            Reply::Text(format!(
                "MIO-PAGING-DONE pages={} total_bytes={total}",
                state.pages.len()
            ))
        }
    }
}

fn handle_request(socket: &mut TcpStream, request: &Value, shared: &Arc<Mutex<MockState>>) {
    let messages = request["messages"].as_array().cloned().unwrap_or_default();
    let reply = {
        let mut state = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        decide(&mut state, &messages)
    };
    match reply {
        Reply::ToolCall {
            name,
            id,
            arguments,
        } => write_tool_call(socket, &name, &id, &arguments),
        Reply::Text(text) => write_text(socket, &text),
    }
}

/// 安装声明式插件：一个确定性长单行输出，一个故意超过 8 MiB 硬上限的输出。
fn write_pager_plugin(root: &Path) {
    let directory = root.join(".mio").join("plugins").join("pager");
    std::fs::create_dir_all(&directory).unwrap();
    let long_script = r#"
import sys
pattern = "mio-paging-字节🌊海-"
sentinel = "SENTINEL-END-mio-paging-9f3c"
target = 39 * 1024
raw = (pattern * 4000).encode("utf-8")[:target]
while True:
    try:
        raw.decode("utf-8")
        break
    except UnicodeDecodeError:
        raw = raw[:-1]
sys.stdout.buffer.write(raw + sentinel.encode("utf-8"))
sys.stdout.buffer.flush()
"#;
    let oversized_script = r#"
import sys
sys.stdout.buffer.write(b"x" * (8 * 1024 * 1024 + 1))
sys.stdout.buffer.flush()
"#;
    let manifest = json!({
        "schema_version": 1,
        "name": "pager",
        "version": "0.1.0",
        "description": "long-output byte-paging fixture",
        "tools": [
            {
                "name": "emit_long",
                "description": "write a deterministic long single-line UTF-8 artifact",
                "parameters": {"type": "object", "properties": {}, "additionalProperties": false},
                "command": ["python3", "-c", long_script],
            },
            {
                "name": "emit_oversized",
                "description": "write more than the 8 MiB per-stream hard limit",
                "parameters": {"type": "object", "properties": {}, "additionalProperties": false},
                "command": ["python3", "-c", oversized_script],
            },
        ],
    });
    std::fs::write(
        directory.join("mio-plugin.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
}

/// 测试侧独立重建的期望字节：与插件脚本同样的确定性对象。
fn expected_output() -> Vec<u8> {
    let mut bytes = PATTERN.repeat(4000).into_bytes();
    bytes.truncate(TARGET_BYTES);
    while std::str::from_utf8(&bytes).is_err() {
        bytes.pop();
    }
    bytes.extend_from_slice(SENTINEL.as_bytes());
    bytes
}

/// CLI 容量与轮次参数；`None` 表示不传该参数，让 CLI 使用自身的实际默认值。
#[derive(Clone, Copy)]
struct CliBudget {
    context_window: Option<&'static str>,
    max_turns: Option<&'static str>,
}

impl CliBudget {
    /// 既有主例的显式配置：上下文窗口 400000、工具循环 16 轮。
    const fn explicit_400k_16_turns() -> Self {
        Self {
            context_window: Some("400000"),
            max_turns: Some("16"),
        }
    }

    /// 不传 `--context-window` / `--max-turns`，走 CLI 实际默认（32768 / 32）。
    const fn cli_defaults() -> Self {
        Self {
            context_window: None,
            max_turns: None,
        }
    }
}

/// 构造完全隔离的 CLI 调用；容量与轮次由 `budget` 决定，其余配置固定。
fn cli_command(
    workspace: &Path,
    home: &Path,
    config: &Path,
    address: SocketAddr,
    budget: CliBudget,
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mio"));
    // 清除继承的 MIO_*、API key 与 proxy 覆盖，只保留显式传入的临时配置。
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.starts_with("MIO_")
            || name.contains("API_KEY")
            || name.to_lowercase().contains("proxy")
        {
            command.env_remove(key);
        }
    }
    let base_url = format!("http://{address}/v1");
    let workspace_arg = workspace.to_str().unwrap().to_string();
    let tmp = home.join("tmp");
    let mut args = vec![
        "--cwd".to_string(),
        workspace_arg,
        "--json".to_string(),
        "--no-session".to_string(),
        "--no-shared-limits".to_string(),
        "--provider".to_string(),
        "custom".to_string(),
        "--model".to_string(),
        "test".to_string(),
        "--base-url".to_string(),
        base_url,
    ];
    if let Some(window) = budget.context_window {
        args.push("--context-window".to_string());
        args.push(window.to_string());
    }
    if let Some(turns) = budget.max_turns {
        args.push("--max-turns".to_string());
        args.push(turns.to_string());
    }
    args.push("-p".to_string());
    args.push("read the complete plugin output in byte pages".to_string());
    command
        .current_dir(workspace)
        .env("HOME", home)
        .env("MIO_CONFIG_DIR", config)
        .env("MIO_API_KEY", "local-test-credential")
        .env("TMPDIR", &tmp)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(args);
    command
}

/// 有界运行真实子进程；超时强杀并失败，绝不无限等待。
fn run_bounded(mut command: Command, timeout: Duration) -> Output {
    let mut child = command.spawn().expect("spawn mio CLI");
    let stdout = child.stdout.take().expect("CLI stdout pipe");
    let stderr = child.stderr.take().expect("CLI stderr pipe");
    let out_reader = std::thread::spawn(move || drain_pipe(stdout));
    let err_reader = std::thread::spawn(move || drain_pipe(stderr));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait for mio CLI") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = out_reader.join();
            let _ = err_reader.join();
            panic!("mio CLI did not exit within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Output {
        status,
        stdout: out_reader.join().expect("CLI stdout reader"),
        stderr: err_reader.join().expect("CLI stderr reader"),
    }
}

fn drain_pipe(mut stream: impl Read) -> Vec<u8> {
    let mut buffer = Vec::new();
    stream.read_to_end(&mut buffer).expect("read CLI pipe");
    buffer
}

fn parse_events(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<Value>(line)
                .unwrap_or_else(|error| panic!("non-JSON CLI line `{line}`: {error}"))
        })
        .collect()
}

fn artifact_files(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = match std::fs::read_dir(root.join(".mio/outputs")) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .collect(),
        Err(_) => Vec::new(),
    };
    files.sort();
    files
}

fn prepared_workspace() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    PathBuf,
    PathBuf,
    PathBuf,
) {
    let workspace_dir = tempfile::tempdir().unwrap();
    let home_dir = tempfile::tempdir().unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    let home = home_dir.path().canonicalize().unwrap();
    let config = home.join("mio-config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(home.join("tmp")).unwrap();
    write_pager_plugin(&workspace);
    (workspace_dir, home_dir, workspace, home, config)
}

/// 主验证 helper：用给定 CLI 预算跑完整链路，并对真实引用、真实 `read` 分页、
/// 逐字节重建、页间连续性、末尾 sentinel、单页上限与模型最后一轮做全套断言。
///
/// `budget` 只改变传给 CLI 的容量/轮次参数；插件、payload、mock 决策与断言完全一致，
/// 因此显式预算与 CLI 默认预算走的是同一条可复用的验证路径。
fn verify_cli_paging_until_complete(
    workspace: &Path,
    home: &Path,
    config: &Path,
    budget: CliBudget,
) {
    let state = Arc::new(Mutex::new(MockState::default()));
    let handler_state = Arc::clone(&state);
    let mock = mock_server(move |socket, request| handle_request(socket, request, &handler_state));

    let output = run_bounded(
        cli_command(workspace, home, config, mock.address, budget),
        CLI_TIMEOUT,
    );
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let events = parse_events(&output.stdout);
    assert!(
        events.iter().any(|event| event["type"] == "done"),
        "CLI must finish the task: {events:?}"
    );

    // 真实 `--json` 事件里必须带上插件引用。
    let finished = events
        .iter()
        .find(|event| event["type"] == "tool_finished" && event["data"]["operation"] == PLUGIN_TOOL)
        .unwrap_or_else(|| panic!("plugin tool_finished event: {events:?}"));
    let cli_result = finished["data"]["result"]["Ok"]
        .as_str()
        .unwrap_or_else(|| panic!("plugin tool must succeed: {finished}"));
    assert!(
        cli_result.contains("complete output: .mio/outputs/"),
        "CLI result must carry the externalized reference: {cli_result}"
    );
    let cli_reference =
        reference_from_text(cli_result).expect("CLI event must expose a readable reference");

    // 模型真实完成了最后一轮。
    let text: String = events
        .iter()
        .filter(|event| event["type"] == "text")
        .filter_map(|event| event["data"].as_str())
        .collect();
    assert!(
        text.contains("MIO-PAGING-DONE"),
        "the mock model must complete its follow-up turn: {text}"
    );

    // 磁盘上的真实产物必须与确定性期望一致，并且是长单行。
    let artifacts = artifact_files(workspace);
    assert_eq!(artifacts.len(), 1, "exactly one complete output artifact");
    let bytes = std::fs::read(&artifacts[0]).unwrap();
    let expected = expected_output();
    assert_eq!(bytes, expected, "stored bytes must be the exact artifact");
    assert!(bytes.ends_with(SENTINEL.as_bytes()), "trailing sentinel");
    assert!(
        !bytes.contains(&b'\n'),
        "the artifact must be a long single line"
    );
    assert!(
        bytes.len() > 32 * 1024,
        "must exceed the 8 KiB inline limit"
    );
    assert!(
        bytes.len() <= 40 * 1024,
        "output stays near the 40 KiB budget"
    );

    let snapshot = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(snapshot.failure.is_none(), "{:?}", snapshot.failure);
    let reference = snapshot
        .reference
        .clone()
        .expect("mock must read the reference from the real tool result");
    assert_eq!(
        reference, cli_reference,
        "mock and CLI must agree on the same real reference"
    );
    assert_eq!(
        reference,
        format!(
            ".mio/outputs/{}",
            artifacts[0].file_name().unwrap().to_string_lossy()
        ),
        "reference must point at the real artifact"
    );
    assert!(
        snapshot.pages.len() >= 2,
        "the real read must need at least two pages: {}",
        snapshot.pages.len()
    );
    assert_eq!(
        snapshot.turns,
        snapshot.pages.len() + 2,
        "plugin turn + one turn per page + final turn"
    );

    // 重建结果与真实产物逐字节一致，且以 sentinel 结尾。
    assert_eq!(
        snapshot.reconstructed.as_bytes(),
        bytes.as_slice(),
        "paged reconstruction must match the stored artifact exactly"
    );
    assert!(
        snapshot.reconstructed.ends_with(SENTINEL),
        "reconstruction must end with the sentinel"
    );

    // 每页长度受限、无缝衔接、无重叠；只有最后一页给出 null。
    let mut running = 0u64;
    for (index, page) in snapshot.pages.iter().enumerate() {
        let text = page["text"].as_str().unwrap();
        let offset = page["byte_offset"].as_u64().unwrap();
        let total = page["total_bytes"].as_u64().unwrap();
        assert_eq!(
            total,
            bytes.len() as u64,
            "page {index} must report the real file size"
        );
        assert!(
            text.len() as u64 <= PAGE_LIMIT,
            "page {index} exceeds the requested byte limit"
        );
        assert!(!text.is_empty(), "page {index} must not be empty");
        assert_eq!(
            offset, running,
            "page {index} must continue with no gap or overlap"
        );
        running += text.len() as u64;
        match page["next_byte_offset"].as_u64() {
            Some(next) => assert_eq!(
                next, running,
                "page {index} next_byte_offset must equal the consumed prefix"
            ),
            None => assert_eq!(
                index,
                snapshot.pages.len() - 1,
                "only the last page may end the paging"
            ),
        }
    }
    assert_eq!(
        running,
        bytes.len() as u64,
        "pages must cover the whole artifact"
    );
    println!(
        "mio_paging_metrics context_window={} max_turns={} model_turns={} read_pages={} bytes={}",
        budget.context_window.unwrap_or("default"),
        budget.max_turns.unwrap_or("default"),
        snapshot.turns,
        snapshot.pages.len(),
        running,
    );
}

#[test]
fn cli_plugin_output_pages_through_real_read_until_complete() {
    let (_workspace_dir, _home_dir, workspace, home, config) = prepared_workspace();
    verify_cli_paging_until_complete(
        &workspace,
        &home,
        &config,
        CliBudget::explicit_400k_16_turns(),
    );
}

/// CLI 实际默认预算（不传 `--context-window` / `--max-turns`，即 32768 / 32）下的同一链路。
///
/// 原先始终按六倍转义开销缩页，实际达到 32 次模型调用后仍有 6418 字节未读。
/// 回归保持默认预算，验证按真实序列化尺寸分页后完整覆盖同一产物。
#[test]
fn cli_default_budget_pages_long_output_until_complete() {
    let (_workspace_dir, _home_dir, workspace, home, config) = prepared_workspace();
    verify_cli_paging_until_complete(&workspace, &home, &config, CliBudget::cli_defaults());
}

#[test]
fn oversized_plugin_stream_fails_without_truncated_success() {
    let (_workspace_dir, _home_dir, workspace, home, config) = prepared_workspace();

    let turns = Mutex::new(0usize);
    let mock = mock_server(move |socket, _request| {
        let mut turn = turns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *turn == 0 {
            write_tool_call(socket, OVERSIZED_TOOL, "call_big", &json!({}));
        } else {
            write_text(socket, "OVERSIZED-DONE");
        }
        *turn += 1;
    });

    let output = run_bounded(
        cli_command(
            &workspace,
            &home,
            &config,
            mock.address,
            CliBudget::explicit_400k_16_turns(),
        ),
        CLI_TIMEOUT,
    );
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let events = parse_events(&output.stdout);
    let finished = events
        .iter()
        .find(|event| {
            event["type"] == "tool_finished" && event["data"]["operation"] == OVERSIZED_TOOL
        })
        .unwrap_or_else(|| panic!("oversized tool_finished event: {events:?}"));
    let error = finished["data"]["result"]["Err"]
        .as_str()
        .unwrap_or_else(|| panic!("oversized stream must fail, not truncate: {finished}"));
    assert!(
        error.contains("exceeds complete output file limit"),
        "failure must name the real hard limit: {error}"
    );
    assert!(
        finished["data"]["result"]["Ok"].is_null(),
        "the oversized stream must not report truncated success: {finished}"
    );
    assert!(
        artifact_files(&workspace).is_empty(),
        "a failed capture must leave no complete-output artifact"
    );
    let text: String = events
        .iter()
        .filter(|event| event["type"] == "text")
        .filter_map(|event| event["data"].as_str())
        .collect();
    assert!(
        text.contains("OVERSIZED-DONE"),
        "the model turn after the tool failure must still complete: {text}"
    );
}
