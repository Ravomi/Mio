use mio_coding_agent::{AgentEvent, Config, Host, HostFuture, Provider, Services, run};
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
