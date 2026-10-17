//! 真实 `mio --mode rpc` 的同名工具并发关联 ID 回归（DES-15）。
//!
//! 具体缺口：`CliHost` 目前把 `Host::on_event` 的 `ToolStarted` / `ToolFinished`
//! 关联到引擎派发身份时用的是「开始 peek 等待队列队首、结束 pop 队首」。这个
//! 假设在工具严格按派发顺序串行完成时成立；一旦**同名**工具的两个并发调用
//! 完成顺序反转，队首就属于另一个调用，peek/pop 会把 fast 的结果配上 slow 的
//! `toolCallId`（反之亦然）。本用例不复现 FIFO 之外的任何猜测：
//!
//! - 模型在同一个 turn 里给出两个同名 `plugin__probe__emit` 调用，供应商 id
//!   分别是 `call_slow` / `call_fast`，参数 `tag` 分别是 `slow` / `fast`；
//! - 插件命令用 `python3` 读取显式的 `MIO_TOOL_ARGS`：`fast` 立即输出
//!   `fast-result` 并结束；`slow` 等待工作区里的 `release` 文件再输出
//!   `slow-result`，并且最多 15 秒就自行退出，绝不无限挂住；
//! - 测试客户端**只在真实收到 fast 的 `ToolFinished` 之后**才写 `release`，
//!   用事件顺序而不是固定 sleep 制造完成顺序反转；
//! - 断言只使用真实字段：用 `data.target.tag` 区分 fast/slow，再逐个断言
//!   各自的 `toolCallId` 等于供应商 id，`runId` 是真实且稳定的运行 UUID，
//!   `turnId` 在同一调用的开始与结束之间一致，且这些字段不是 `null`。
//!
//! 这是回归基线：当前实现未必能让两个工具调用并发完成，因此在修复并发与
//! 关联配对之前本用例可能失败。断言不因失败而放松。
//!
//! 为了让插件加载器工作，本用例**不设置** `--no-mcp`（当前该开关也会一并
//! 禁用插件加载）；只在临时工作区里配置声明式插件，不配置任何 MCP server。
//! 临时 `MIO_CONFIG_DIR` 与清除 `MIO_*` / API key / proxy 覆盖保证不会启动
//! 用户全局 MCP，也不会连到真实 Provider（凭据固定 `local-test-credential`）。

#[path = "../src/test_http.rs"]
mod test_http;

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

/// 插件限定工具名：`plugin__<plugin>__<tool>`。
const PLUGIN: &str = "plugin__probe__emit";
/// 模型在 SSE 里给出的两个供应商工具调用 id。
const SLOW_ID: &str = "call_slow";
const FAST_ID: &str = "call_fast";
/// 客户端等待一个真实事件的上限；插件的 15 秒自限必须落在这个上限之内。
const CLIENT_EVENT_DEADLINE: Duration = Duration::from_secs(30);
/// 子进程在 Drop 里放弃等待并强杀前的有界等待。
const CHILD_GRACE: Duration = Duration::from_secs(3);

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
    let thread_stop = Arc::clone(&stop);
    std::thread::spawn(move || {
        // mock reader 也有明确 deadline：测试异常退出时不会留下永久 accept 循环。
        let deadline = Instant::now() + Duration::from_secs(90);
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

/// 在工作区安装一个原生声明式插件，命令是 `python3 -c ...`。
///
/// 两个调用共用同一个工具名与命令，只靠显式 `MIO_TOOL_ARGS` 里的 `tag` 区分：
/// `fast` 立即结束，`slow` 等 `release`，两者都不会超过 15 秒。
fn write_probe_plugin(cwd: &Path) {
    let directory = cwd.join(".mio").join("plugins").join("probe");
    std::fs::create_dir_all(&directory).unwrap();
    let script = r##"
import json
import os
import sys
import time

try:
    arguments = json.loads(os.environ.get("MIO_TOOL_ARGS") or "{}")
except ValueError:
    arguments = {}

tag = arguments.get("tag") if isinstance(arguments, dict) else None
if tag == "fast":
    sys.stdout.write("fast-result")
    sys.stdout.flush()
    sys.exit(0)
if tag == "slow":
    # 自限：fixture 永不比测试自身的 deadline 活得更久。
    deadline = time.monotonic() + 15.0
    while time.monotonic() < deadline:
        if os.path.exists("release"):
            sys.stdout.write("slow-result")
            sys.stdout.flush()
            sys.exit(0)
        time.sleep(0.02)
    sys.exit(3)
sys.exit(4)
"##;
    let manifest = json!({
        "schema_version": 1,
        "name": "probe",
        "version": "0.1.0",
        "description": "parallel tool-call correlation fixture",
        "tools": [{
            "name": "emit",
            "description": "emit a tagged result",
            "parameters": {
                "type": "object",
                "properties": {"tag": {"type": "string"}},
                "additionalProperties": true,
            },
            "command": ["python3", "-c", script],
        }],
    });
    std::fs::write(
        directory.join("mio-plugin.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
}

/// 真实 `mio --mode rpc` 子进程封装，Drop 时先 EOF、再有界等待、最后强杀。
struct RpcClient {
    child: Child,
    events: mpsc::Receiver<Value>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Drop for RpcClient {
    fn drop(&mut self) {
        // 先关闭 stdin：RPC 循环把 EOF 当作有界退出信号。
        let _ = self.child.stdin.take();
        let deadline = Instant::now() + CHILD_GRACE;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        // reader 线程在管道关闭后会自行结束；仍然有界，绝不无限等待。
        if let Some(reader) = self.reader.take() {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !reader.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if reader.is_finished() {
                let _ = reader.join();
            }
        }
    }
}

impl RpcClient {
    fn start(root: &Path, address: SocketAddr) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mio"));
        // 清除任何继承的 MIO_*、API key 与 proxy 覆盖，只保留临时工作区配置。
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
            .current_dir(root)
            .env("HOME", root)
            .env("MIO_CONFIG_DIR", root)
            .env("MIO_API_KEY", "local-test-credential")
            .args([
                "--mode",
                "rpc",
                "--no-context",
                "--no-session",
                "--tool-concurrency",
                "2",
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
        let (tx, rx) = mpsc::channel::<Value>();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(&line) {
                    Ok(event) => {
                        if tx.send(event).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        // 不静默丢弃：协议污染必须能被断言看到。
                        if tx
                            .send(json!({
                                "type": "__invalid_json__",
                                "error": error.to_string(),
                                "raw": line,
                            }))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            child,
            events: rx,
            reader: Some(reader),
        }
    }

    fn send(&mut self, value: Value) {
        let stdin = self.child.stdin.as_mut().expect("rpc stdin");
        writeln!(stdin, "{value}").unwrap();
        stdin.flush().unwrap();
    }

    /// 按需读取事件直到谓词为真；超时即失败，不会无限等待。
    fn collect_until(&self, predicate: impl Fn(&Value) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + CLIENT_EVENT_DEADLINE;
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
}

/// 只对插件工具事件解析真实 `data.target` 里的 `tag`。
fn plugin_tag(event: &Value) -> Option<String> {
    if event["data"]["operation"].as_str() != Some(PLUGIN) {
        return None;
    }
    let target = event["data"]["target"].as_str()?;
    let parsed: Value = serde_json::from_str(target).ok()?;
    parsed.get("tag").and_then(Value::as_str).map(str::to_owned)
}

fn find_tagged<'a>(events: &[&'a Value], tag: &str) -> &'a Value {
    events
        .iter()
        .copied()
        .find(|event| plugin_tag(event).as_deref() == Some(tag))
        .unwrap_or_else(|| panic!("no plugin tool event tagged `{tag}`: {events:?}"))
}

fn result_text(event: &Value) -> String {
    let result = &event["data"]["result"];
    if let Some(ok) = result.get("Ok").and_then(Value::as_str) {
        return ok.to_owned();
    }
    if let Some(err) = result.get("Err").and_then(Value::as_str) {
        return err.to_owned();
    }
    result.to_string()
}

#[test]
fn rpc_parallel_same_name_calls_keep_their_own_provider_ids() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write_probe_plugin(&root);
    assert!(
        !root.join(".mio").join("mcp.json").exists(),
        "the fixture must not configure MCP"
    );

    let turn = Mutex::new(0usize);
    let mock = mock_server(move |socket, _request| {
        let mut turn = turn.lock().unwrap();
        if *turn == 0 {
            // 同名工具两次，派发顺序 slow -> fast；完成顺序由 release 反转。
            let delta = json!({
                "role": "assistant",
                "tool_calls": [
                    tool_call(0, PLUGIN, SLOW_ID, json!({"tag": "slow"})),
                    tool_call(1, PLUGIN, FAST_ID, json!({"tag": "fast"})),
                ],
            });
            write_sse(socket, delta, json!("tool_calls"));
        } else {
            write_text(socket, "done");
        }
        *turn += 1;
    });

    let mut client = RpcClient::start(&root, mock.address);

    client.send(json!({"type": "initialize", "id": "i1", "schema_versions": [1]}));
    let negotiated = client.response("i1");
    assert_eq!(negotiated["success"], true, "{negotiated}");
    assert_eq!(negotiated["data"]["envelope"], "v1", "{negotiated}");

    client.send(json!({"type": "prompt", "id": "p1", "message": "run both probes"}));
    assert_eq!(client.response("p1")["success"], true);

    // 只在真实收到 fast 的 ToolFinished 之后才释放 slow；顺序反转来自事件，
    // 不是固定 sleep 的猜测。
    let mut seen = client.collect_until(|event| {
        event["type"] == "tool_finished" && plugin_tag(event).as_deref() == Some("fast")
    });
    std::fs::write(root.join("release"), b"go").unwrap();
    seen.extend(client.collect_until(|event| event["type"] == "agent_settled"));

    let _ = client.child.stdin.take();

    assert!(
        seen.iter().all(|event| event["type"] != "__invalid_json__"),
        "the RPC stream must stay valid JSON: {seen:?}"
    );

    let started: Vec<&Value> = seen
        .iter()
        .filter(|event| event["type"] == "tool_started" && plugin_tag(event).is_some())
        .collect();
    let finished: Vec<&Value> = seen
        .iter()
        .filter(|event| event["type"] == "tool_finished" && plugin_tag(event).is_some())
        .collect();
    assert_eq!(started.len(), 2, "{seen:?}");
    assert_eq!(finished.len(), 2, "{seen:?}");

    // 真实运行身份：来自 v1 的 run_started 观察，必须是稳定 UUID。
    let run_started = seen
        .iter()
        .find(|event| event["type"] == "run_observation" && event["data"]["kind"] == "run_started")
        .expect("v1 run_started observation");
    let run_id = run_started["runId"]
        .as_str()
        .unwrap_or_else(|| panic!("run_started must carry a real runId: {run_started}"))
        .to_owned();
    let parsed_run_id = uuid::Uuid::parse_str(&run_id).expect("runId must be a UUID");
    assert_eq!(parsed_run_id.get_version_num(), 4, "{run_id}");

    // 每个同名调用只按真实 tag 定位，再断言它自己的关联字段。
    for (tag, expected_id, expected_text) in [
        ("fast", FAST_ID, "fast-result"),
        ("slow", SLOW_ID, "slow-result"),
    ] {
        let start = find_tagged(&started, tag);
        let finish = find_tagged(&finished, tag);

        assert_eq!(start["toolCallId"], json!(expected_id), "{start}");
        assert_eq!(finish["toolCallId"], json!(expected_id), "{finish}");
        assert_eq!(start["runId"], json!(run_id), "{start}");
        assert_eq!(finish["runId"], json!(run_id), "{finish}");
        assert_eq!(start["turnId"], json!(1), "{start}");
        assert_eq!(finish["turnId"], json!(1), "{finish}");

        // 没有伪造或借用其他调用的身份。
        assert_eq!(start["subagentId"], Value::Null, "{start}");
        assert_eq!(finish["subagentId"], Value::Null, "{finish}");
        assert_eq!(start["parentRunId"], Value::Null, "{start}");
        assert_eq!(finish["parentRunId"], Value::Null, "{finish}");

        let text = result_text(finish);
        assert!(
            text.contains(expected_text),
            "`{tag}` result must come from its own run, got `{text}`: {finish}"
        );

        // 开始与结束必须是同一个供应商调用，且 turn 一致。
        assert_eq!(
            start["toolCallId"], finish["toolCallId"],
            "start/finish must share one toolCallId: {start} {finish}"
        );
        assert_eq!(
            start["turnId"], finish["turnId"],
            "start/finish must share one turnId: {start} {finish}"
        );
    }

    // 关键：fast 的真实结束必须早于 slow，否则 FIFO 假设根本未被证伪，
    // 但 slow 的 `slow-result` 也证明 release 是在 fast 之后才写入的。
    let fast_finish = seen
        .iter()
        .position(|event| {
            event["type"] == "tool_finished" && plugin_tag(event).as_deref() == Some("fast")
        })
        .expect("fast tool_finished");
    let slow_finish = seen
        .iter()
        .position(|event| {
            event["type"] == "tool_finished" && plugin_tag(event).as_deref() == Some("slow")
        })
        .expect("slow tool_finished");
    assert!(
        fast_finish < slow_finish,
        "the fast call must finish before the slow one; correlation cannot rely on dispatch FIFO: {seen:?}"
    );

    // 所有工具事件都必须诚实携带真实身份，不能用 null 冒充已关联。
    for event in started.iter().chain(finished.iter()) {
        assert!(event["toolCallId"].is_string(), "{event}");
        assert!(event["runId"].is_string(), "{event}");
        assert!(event["turnId"].is_u64(), "{event}");
    }

    // 两次模型请求也属于同一个真实运行。
    let completions: Vec<&Value> = seen
        .iter()
        .filter(|event| event["type"] == "completion_call")
        .collect();
    assert_eq!(completions.len(), 2, "{seen:?}");
    for completion in &completions {
        assert_eq!(completion["runId"], json!(run_id), "{completion}");
        assert!(completion["turnId"].is_u64(), "{completion}");
    }

    let run_finished = seen
        .iter()
        .find(|event| event["type"] == "run_observation" && event["data"]["kind"] == "run_finished")
        .expect("v1 run_finished observation");
    assert_eq!(run_finished["runId"], json!(run_id), "{run_finished}");
    assert_eq!(
        run_finished["data"]["status"],
        json!("success"),
        "{run_finished}"
    );

    let settled = seen
        .iter()
        .find(|event| event["type"] == "agent_settled")
        .expect("agent_settled");
    assert_eq!(settled["status"], json!("completed"), "{settled}");
    assert_eq!(settled["runId"], json!(run_id), "{settled}");

    let done = seen
        .iter()
        .find(|event| event["type"] == "done")
        .expect("done event");
    assert_eq!(done["runId"], json!(run_id), "{done}");
}
