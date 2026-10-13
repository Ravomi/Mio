# Mio (`mio-agent`)

Mio 是从 [Ravomi](https://github.com/Ravomi/ravomi-app) 提取的无头（Headless）编程 Agent 运行时内核，基于 Rust 编写并以 [Rig](https://github.com/0xPlaygrounds/rig) 驱动大语言模型交互。

本项目专注于轻量、可靠的对话与工具多轮循环执行引擎，不依赖任何特定前端桌面技术（Bevy、GPUI、Alacritty 或 SQLite），可嵌入任意 Rust 宿主应用。

## 现状与范围说明

- **未对齐 Pi 全部功能**：Mio 聚焦于 Agent 多轮交互与基础 workspace 工具循环，不包含扩展机制、TUI 界面系统、会话持久化与自定义主题等。
- **实测与落地状态**：真实多账号服务（Google Antigravity / DeepSeek Web）和跨平台（尤其是非 Unix 环境）尚未验收；宿主工程 Ravomi 目前尚未接入本新独立库。

## 定位与特性

- **轻量嵌入式引擎**：通过统一的 `run(...)` 函数驱动 32 轮流式调用循环与多轮工具交互。
- **无状态设计**：调用方全权掌控对话历史持久化、会话状态存储及超时策略；单次 `run` 返回包含本次工具与回复的完整历史（`Vec<Message>`）。
- **外部宿主抽象（`Host` trait）**：
  - 负责终端命令执行与流式事件回调（`Text`、`ToolStarted`、`ToolFinished`）。
  - 事件处理回调应遵循 drop-safe 并且是非阻塞（nonblocking）的。
  - 工作目录限制并不是容器沙箱（外部命令权限完全取决于宿主）。
  - 任务取消并不会自动回滚已经写入磁盘的文件。
- **内置安全约束**：工作目录路径逃逸检查（防止目录遍历）、单文件读取 64 KiB 限制、写入 1 MiB 限制以及库内执行的 120 秒超时控制。
- **全链路协程取消**：调用方 drop 掉 `run` 的 future，会同步释放正在运行的内部工具及宿主 shell future。
- **丰富供应商生态**：保留并支持 OpenAI 兼容 API、Codex、DeepSeek（API 与 Web 协议）、Google Antigravity OAuth 协议。
  - 使用 Google Antigravity 时，宿主须提供自己获授权使用的 Google OAuth 客户端配置（`OAuthConfig`，可从 `MIO_GOOGLE_CLIENT_ID` 与 `MIO_GOOGLE_CLIENT_SECRET` 读取）。若未提供有效配置则无法启用 Antigravity；本库不承诺任意注册的普通 OAuth 客户端一定支持 Cloud Code Assist 协议。

## 调用示例

代码位于 `examples/host.rs`，宿主示例显式返回未实现 shell：

```rust
use mio_agent::{AgentEvent, Config, Host, HostFuture, Provider, Services, run};
use std::{path::Path, sync::Arc};

struct ExampleHost;

impl Host for ExampleHost {
    fn shell(&self, _command: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async move {
            // 宿主环境未实现真正的 PTY / 子进程 shell 执行
            Err("示例 Host 未实现 shell 执行功能".to_string())
        })
    }

    fn on_event(&self, event: AgentEvent) {
        match event {
            AgentEvent::Text(delta) => print!("{delta}"),
            AgentEvent::ToolStarted { operation, target } => {
                eprintln!("[tool start] {operation}: {target}");
            }
            AgentEvent::ToolFinished {
                operation,
                target,
                result,
            } => {
                eprintln!("[tool finish] {operation} ({target}): {:?}", result.is_ok());
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config {
        provider: Provider::Custom,
        base_url: "https://api.openai.com/v1".into(),
        model: "gpt-4o".into(),
        api_key: std::env::var("OPENAI_API_KEY").unwrap_or_default(),
    };

    let host = Arc::new(ExampleHost);
    let cwd = std::env::current_dir()?;
    let history = Vec::new();

    let updated_history = run(
        config,
        history,
        "请查看当前目录下的 Cargo.toml",
        &cwd,
        host,
        Services::default(),
    )
    .await?;

    println!("Total turns in history: {}", updated_history.len());
    Ok(())
}
```

可使用 `cargo check --examples` 进行编译验证。

## 取消与运行时约定

- 宿主须在多线程 Tokio 运行时环境下调用本 crate。
- 取消任务只需直接丢弃（drop）`run(...)` 返回的 Future；Mio 将协同 drop 宿主 `shell` 返回的 Future。宿主负责其启动的子进程生命周期回收。
- 宿主负责总任务超时与会话落盘；当任务失败或被中断取消时，宿主可记录失败原因并自主决定如何保留上下文。

## 历史溯源与致谢

- 历史源文件及上游开源协议见 [CREDITS.md](CREDITS.md)。
- [Rig](https://github.com/0xPlaygrounds/rig) - 优雅模块化的 Rust LLM 应用框架。
- [Ravomi](https://github.com/Ravomi/ravomi-app) - 本项目的源生桌面环境与设计来源。
