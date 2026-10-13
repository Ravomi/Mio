//! mio-agent: Headless coding agent engine extracted from Ravomi.
//!
//! Mio 采用 Rig 驱动多轮 LLM 对话与工具调用循环。通过 Host trait 抽象终端与外部环境交互，
//! 由库内部负责文件读写安全校验、120 秒超时控制以及结构化事件流派发。

pub mod accounts;
pub mod antigravity;
pub mod context;
pub mod deepseek;
pub mod deepseek_pow;
pub mod session;
pub mod shell;
#[cfg(test)]
pub mod test_http;
pub mod workspace;

pub use workspace::{Arguments, Workspace, checked_path};

use futures_util::StreamExt;
use rig_agent::{AgentBuilder, agent::MultiTurnStreamItem};
pub use rig_core::message::Message;
use rig_core::{
    DynModel,
    operation::Completion,
    providers::openai::{OpenAIConfig, wire::Route},
    streaming::{Item, StreamEvent},
};
use serde::{Deserialize, Serialize};
use std::{future::Future, path::Path, pin::Pin, sync::Arc};

/// 异步 Shell 任务的 Boxed Future。
pub type HostFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// 宿主环境接口：负责命令执行及事件通知，生命周期由 Arc 持有。
///
/// 注意事项：
/// - 事件处理回调 (`on_event`) 必须是 drop-safe 并且是非阻塞（nonblocking）的。
/// - 工作目录限制不是容器安全沙箱，命令执行权限由宿主自身环境决定。
/// - Future 取消不会自动回滚任何已写入的文件。
pub trait Host: Send + Sync {
    /// 执行命令并返回输出。返回的 future 由调用方取消（通过 drop）。宿主负责子进程实际回收。
    fn shell(&self, command: &str, cwd: &Path) -> HostFuture<Result<String, String>>;

    /// 接收 Agent 派发的结构化事件。
    fn on_event(&self, event: AgentEvent);
}

/// Agent 执行过程中发出的结构化事件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AgentEvent {
    /// 模型流式文本输出。
    Text(String),
    /// 工具开始执行。
    ToolStarted { operation: String, target: String },
    /// 工具执行完成。
    ToolFinished {
        operation: String,
        target: String,
        result: Result<String, String>,
    },
}

/// 支持的模型供应商类型。
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    #[default]
    Custom,
    Codex,
    Deepseek,
    Antigravity,
    DeepseekWeb,
}

/// Agent 运行配置。
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default)]
    pub provider: Provider,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: Provider::Custom,
            base_url: "https://api.openai.com/v1".into(),
            model: String::new(),
            api_key: String::new(),
        }
    }
}

/// 第三方/高级账户服务集合。
#[derive(Default)]
pub struct Services<'a> {
    pub accounts: Option<&'a accounts::Accounts>,
    pub antigravity: Option<&'a antigravity::Antigravity>,
    pub deepseek: Option<&'a deepseek::Deepseek>,
}

/// Optional instructions and turn budget supplied by the host.
/// Loading project files is explicit through `context::ProjectContext`.
#[derive(Clone, Debug)]
pub struct RunOptions {
    pub instructions: String,
    pub max_turns: usize,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            instructions: String::new(),
            max_turns: 32,
        }
    }
}

/// 无状态运行一次 Agent 会话循环。
///
/// * `config`: 模型提供商与连接配置
/// * `history`: 先前的对话消息列表（Vec<Message>）
/// * `prompt`: 用户当前输入的任务描述
/// * `cwd`: 工具操作绑定的工作目录（须存在且为目录）
/// * `host`: 宿主交互接口实现
/// * `services`: 账户及协议服务（可选）
///
/// 成功时返回包含新交互的完整对话历史记录。当任务失败时返回错误信息。
/// 若调用方取消（drop 掉返回的 future），内部正在执行的宿主 shell future 也随之 drop。
pub async fn run(
    config: Config,
    history: Vec<Message>,
    prompt: &str,
    cwd: &Path,
    host: Arc<dyn Host>,
    services: Services<'_>,
) -> Result<Vec<Message>, String> {
    run_with_options(
        config,
        history,
        prompt,
        cwd,
        host,
        services,
        RunOptions::default(),
    )
    .await
}

pub async fn run_with_options(
    config: Config,
    history: Vec<Message>,
    prompt: &str,
    cwd: &Path,
    host: Arc<dyn Host>,
    services: Services<'_>,
    options: RunOptions,
) -> Result<Vec<Message>, String> {
    if options.instructions.len() > 128 * 1024 || !(1..=256).contains(&options.max_turns) {
        return Err("附加指令须不超过 128 KiB，工具轮次须在 1–256 之间".into());
    }
    if prompt.trim().is_empty() || prompt.len() > 64 * 1024 {
        return Err("输入需为 1–65536 字节".into());
    }
    if config.model.trim().is_empty() {
        return Err("请先在模型设置中填写模型名和服务地址".into());
    }
    let canonical_cwd = cwd
        .canonicalize()
        .map_err(|e| format!("工作目录不可用：{e}"))?;
    if !canonical_cwd.is_dir() {
        return Err("工作目录不是文件夹".into());
    }

    let model: DynModel<Completion> = match config.provider {
        Provider::Antigravity => services
            .antigravity
            .ok_or("Google 账号服务不可用")?
            .client(&config.model)
            .await?
            .completion(config.model)
            .into(),
        Provider::DeepseekWeb => services
            .deepseek
            .ok_or("DeepSeek 账号服务不可用")?
            .client()
            .await?
            .completion(config.model)
            .into(),
        Provider::Codex => services
            .accounts
            .ok_or("账号服务不可用")?
            .codex()
            .await?
            .completion(config.model)
            .into(),
        Provider::Custom | Provider::Deepseek => {
            let client = OpenAIConfig::new(config.api_key)
                .with_route(Route::Chat)
                .with_base_url(config.base_url)
                .client();
            client.completion(config.model).into()
        }
    };

    let workspace_tool = Workspace::new(canonical_cwd, host.clone())?;

    let preamble = format!(
        "你叫 Mio，是编程助手。使用中文简洁回答。工具在用户选择的工作目录执行。先查看文件再修改，保留用户改动，不声称未执行的测试通过。命令工具不是交互式 shell，避免需要输入的命令。\n{}",
        options.instructions
    );
    let agent = AgentBuilder::new(model)
        .preamble(preamble)
        .tool(workspace_tool)
        .build();

    // Rig's FinalResponse contains only this run's new messages, not input history.
    let mut complete_history = history.clone();
    let mut stream = agent
        .prompt(prompt)
        .history(history)
        .max_turns(options.max_turns)
        .stream();

    let mut final_history = None;

    while let Some(item) = stream.next().await {
        match item.map_err(|_| "模型请求失败，请检查地址、模型、密钥和服务端状态。".to_string())?
        {
            MultiTurnStreamItem::StreamAssistantItem(Item::Event(StreamEvent::Text {
                text,
                ..
            })) => {
                host.on_event(AgentEvent::Text(text));
            }
            MultiTurnStreamItem::FinalResponse(response) => {
                complete_history.extend_from_slice(response.messages());
                final_history = Some(std::mem::take(&mut complete_history));
            }
            _ => {}
        }
    }

    final_history.ok_or_else(|| "模型连接结束，未返回完整结果".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::{
        io::Write,
        sync::{
            Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    struct TestHost {
        events: Mutex<Vec<AgentEvent>>,
        shell_dropped: Option<Arc<AtomicBool>>,
        shell_started: Option<Arc<tokio::sync::Notify>>,
    }

    impl TestHost {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
                shell_dropped: None,
                shell_started: None,
            }
        }

        fn with_drop_flag(dropped: Arc<AtomicBool>) -> Self {
            Self {
                events: Mutex::new(Vec::new()),
                shell_dropped: Some(dropped),
                shell_started: None,
            }
        }

        fn with_drop_and_notify(
            dropped: Arc<AtomicBool>,
            started: Arc<tokio::sync::Notify>,
        ) -> Self {
            Self {
                events: Mutex::new(Vec::new()),
                shell_dropped: Some(dropped),
                shell_started: Some(started),
            }
        }
    }

    struct ShellDropGuard {
        dropped: Option<Arc<AtomicBool>>,
    }

    impl Drop for ShellDropGuard {
        fn drop(&mut self) {
            if let Some(flag) = &self.dropped {
                flag.store(true, Ordering::SeqCst);
            }
        }
    }

    impl Host for TestHost {
        fn shell(&self, command: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
            let cmd = command.to_string();
            let drop_flag = self.shell_dropped.clone();
            let started = self.shell_started.clone();
            Box::pin(async move {
                let _guard = ShellDropGuard { dropped: drop_flag };
                if let Some(n) = started {
                    n.notify_waiters();
                }
                if cmd == "hang" {
                    std::future::pending::<()>().await;
                }
                Ok(format!("executed: {cmd}"))
            })
        }

        fn on_event(&self, event: AgentEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    #[tokio::test]
    async fn rig_streams_tools_and_resumes_history_in_mio() {
        let folder = tempfile::tempdir().unwrap();
        let canonical_dir = folder.path().canonicalize().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            for turn in 0..3 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let (header, request) = crate::test_http::read_json(&mut socket);
                assert!(header.starts_with("POST /v1/chat/completions "));
                tx.send(request).unwrap();
                let delta = if turn == 0 {
                    json!({"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"workspace","arguments":json!({"operation":"write","path":"result.txt","value":"你好 Rig"}).to_string()}}]})
                } else {
                    json!({"role":"assistant","content":"完成：你好"})
                };
                let chunk = |delta: Value, finish: Value| {
                    json!({"id":"test","object":"chat.completion.chunk","created":0,"model":"local-test","choices":[{"index":0,"delta":delta,"finish_reason":finish}]}).to_string()
                };
                let body = format!(
                    "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    chunk(delta, Value::Null),
                    chunk(
                        json!({}),
                        json!(if turn == 0 { "tool_calls" } else { "stop" })
                    )
                );
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });

        let host = Arc::new(TestHost::new());
        let config = Config {
            provider: Provider::Custom,
            base_url: format!("http://{address}/v1"),
            model: "local-test".into(),
            api_key: "test-only".into(),
        };

        // Turn 1 & 2: runs tool call and produces final message history
        let history = run_with_options(
            config.clone(),
            Vec::new(),
            "写入文件",
            &canonical_dir,
            host.clone(),
            Services::default(),
            RunOptions {
                instructions: "Project rule: preserve CRLF".into(),
                ..RunOptions::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(canonical_dir.join("result.txt")).unwrap(),
            "你好 Rig"
        );
        let events = host.events.lock().unwrap().clone();
        assert!(events.iter().any(
            |e| matches!(e, AgentEvent::ToolStarted { operation, .. } if operation == "write")
        ));
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolFinished {
                operation,
                target,
                result: Ok(_)
            } if operation == "write" && target == "result.txt"
        )));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Text(txt) if txt.contains("完成：你好")))
        );

        // Persist and reload the complete tool-call/result history before resuming.
        let mut saved = session::Session::new(&canonical_dir).unwrap();
        saved.history = history;
        let session_path = canonical_dir.join("session.json");
        saved.save(&session_path).unwrap();
        let history = session::Session::load(&session_path).unwrap().history;
        assert_eq!(history, saved.history);
        assert!(
            !std::fs::read_to_string(session_path)
                .unwrap()
                .contains("test-only")
        );

        // Turn 3: resumes from returned history
        let resumed_history = run(
            config,
            history,
            "继续",
            &canonical_dir,
            host.clone(),
            Services::default(),
        )
        .await
        .unwrap();
        assert!(resumed_history.starts_with(&saved.history));
        assert!(resumed_history.len() > saved.history.len());

        server.join().unwrap();
        let requests: Vec<_> = rx.try_iter().collect();
        assert_eq!(requests.len(), 3);
        assert!(
            requests[0]["messages"]
                .to_string()
                .contains("Project rule: preserve CRLF")
        );
        let second = requests[1]["messages"].as_array().unwrap();
        assert!(second.iter().any(|m| m["role"] == "tool"));
        let third = requests[2]["messages"].as_array().unwrap();
        assert!(third.iter().any(|m| m["role"] == "tool"));
        assert!(third.iter().any(|m| m.to_string().contains("写入文件")));
        assert!(!resumed_history.is_empty());
    }

    #[tokio::test]
    async fn file_tools_reject_escape_and_roundtrip_unicode() {
        let folder = tempfile::tempdir().unwrap();
        let canonical_dir = folder.path().canonicalize().unwrap();
        let host = Arc::new(TestHost::new());
        let tools = Workspace::new(canonical_dir.clone(), host).unwrap();

        assert!(
            tools
                .execute(Arguments::write("../escape.txt", "no"))
                .await
                .is_err()
        );
        tools
            .execute(Arguments::write("test.txt", "中文内容"))
            .await
            .unwrap();
        assert_eq!(
            tools.execute(Arguments::read("test.txt")).await.unwrap(),
            "中文内容"
        );

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/", canonical_dir.join("outside")).unwrap();
            assert!(checked_path(&canonical_dir, "outside/etc/passwd", false).is_err());
        }
    }

    #[tokio::test]
    async fn cancellation_drops_host_future() {
        let folder = tempfile::tempdir().unwrap();
        let canonical_dir = folder.path().canonicalize().unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let host = Arc::new(TestHost::with_drop_flag(dropped.clone()));
        let tools = Workspace::new(canonical_dir, host).unwrap();

        let mut exec_fut = Box::pin(tools.execute(Arguments::shell("hang")));

        tokio::select! {
            _ = &mut exec_fut => panic!("should not finish"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }

        assert!(!dropped.load(Ordering::SeqCst));
        drop(exec_fut);
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn run_cancellation_drops_in_flight_shell() {
        let folder = tempfile::tempdir().unwrap();
        let canonical_dir = folder.path().canonicalize().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let (header, _request) = crate::test_http::read_json(&mut socket);
            assert!(header.starts_with("POST /v1/chat/completions "));
            let delta = json!({
                "role": "assistant",
                "tool_calls": [{
                    "index": 0,
                    "id": "call_shell",
                    "type": "function",
                    "function": {
                        "name": "workspace",
                        "arguments": json!({
                            "operation": "shell",
                            "path": "",
                            "value": "hang"
                        }).to_string()
                    }
                }]
            });
            let chunk = |delta: Value, finish: Value| {
                json!({
                    "id": "test",
                    "object": "chat.completion.chunk",
                    "created": 0,
                    "model": "local-test",
                    "choices": [{
                        "index": 0,
                        "delta": delta,
                        "finish_reason": finish
                    }]
                })
                .to_string()
            };
            let body = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                chunk(delta, Value::Null),
                chunk(json!({}), json!("tool_calls"))
            );
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });

        let dropped = Arc::new(AtomicBool::new(false));
        let started = Arc::new(tokio::sync::Notify::new());
        let host = Arc::new(TestHost::with_drop_and_notify(
            dropped.clone(),
            started.clone(),
        ));
        let config = Config {
            provider: Provider::Custom,
            base_url: format!("http://{address}/v1"),
            model: "local-test".into(),
            api_key: "test-only".into(),
        };

        let mut run_fut = Box::pin(run(
            config,
            Vec::new(),
            "执行长命令",
            &canonical_dir,
            host.clone(),
            Services::default(),
        ));

        // 等待 shell 协程确定启动，无需 sleep 忙等
        tokio::select! {
            _ = &mut run_fut => panic!("run should not finish while shell hangs"),
            _ = started.notified() => {}
        }

        assert!(!dropped.load(Ordering::SeqCst));
        drop(run_fut);
        assert!(dropped.load(Ordering::SeqCst));

        server.join().unwrap();
    }
}
