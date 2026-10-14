<p align="center">
  <img src="assets/mio-logo.png" alt="Mio logo" width="240" />
</p>

# Mio

Mio 是从 [Ravomi](https://github.com/Ravomi/ravomi-app) 抽离出的无头 Rust 编程 Agent 引擎，可在终端独立运行，也可通过 Rust 库直接嵌入宿主应用。

[官方网站](https://ravomi.github.io/Mio/) · [技术文档](https://ravomi.github.io/Mio/documentation.html) · [安装指南](docs/installation.md) · [程序状态规范](docs/program-status.md) · [源码来源](docs/rig-source.md) · [示例](examples/host.rs) · [许可证](LICENSE)

## 源码安装与快速开始

源码编译需要 Rust 1.95 或更新版本。其他分发方式及发布状态见 [安装指南](docs/installation.md)。当前新功能请克隆 `refactor/mio-layers` 分支：

```bash
git clone -b refactor/mio-layers https://github.com/Ravomi/Mio.git
cd Mio
cargo install --path . --locked
mio --help
```

配置 OpenAI 兼容模型服务并启动：

```bash
export MIO_BASE_URL='https://your-provider.example/v1'
export MIO_MODEL='your-model-name'
export MIO_API_KEY='your-key'

mio --cwd /path/to/project -p '阅读 Cargo.toml 并解释项目依赖'
mio --cwd /path/to/project                 # 终端交互模式
mio --cwd /path/to/project --resume -p '继续上次任务'
mio --cwd /path/to/project --json -p '统计代码行数'
```

## 核心能力与架构

- **同核两用**：`mio` CLI 与 `mio-coding-agent` Rust 库共享同一 Agent 工具循环；支持 `text`、`json`（JSON Lines 事件流）与 `rpc` 运行模式。
- **工作区代码工具**：内置 `read`、`write`（原子写入）、`edit`（唯一匹配精确替换）、`bash`（120 秒超时）、`grep`、`find`、`ls`；支持 `--tools`、`--exclude-tools` 与 `--no-tools` 过滤。
- **原生子代理**：通过 `--subagents` 启用独立会话的子代理，探索任务默认只读，支持后台异步委派、状态查询、取消与续接。
- **上下文管理与项目规则**：自动发现祖先目录与当前目录的 `AGENTS.md`、`CLAUDE.md`，支持 `@file` 内容展开、项目 Skills（`.mio/skills/`）与提示词模板（`.mio/prompts/`）。
- **会话持久化与导出**：任务历史原子写入 `.mio/sessions/`，支持恢复（`--resume`）、分叉（`--fork`）与单文件 HTML 导出（`--export`）。
- **外部扩展**：支持通过 stdio JSON-RPC 2.0 连接 MCP Server（`.mio/mcp.json`），以及通过 `mio-plugin.json` 规范加载插件。
- **终端状态报告**：仅在 CLI 文本模式且 stderr 为 TTY 时输出 OSC 7501 状态序列，详见 [OSC 7501 状态规范](docs/program-status.md)。

## 边界与重要事实

- **RPC 运行控制**：支持运行中引导、FIFO 排队、状态查询与取消；工具执行和已发出的模型请求不会被引导抢占。完整行为与结束事件见 [RPC 协议](docs/rpc.md)。
- **上下文压缩为本地启发式**：`compact` 模块通过确定性规则与文本启发式提炼会话摘要并保留最近轮次，不经过模型重新归纳生成。
- **模型与供应商参数**：内置 Provider 目录不代表原生专有协议均已适配，动态切换模型时不会自动更换底层连接；`--thinking` 当前调用链仅向 Antigravity 传递该设置。
- **审批与安全边界**：CLI 默认允许写文件和 Shell，`--no-shell` 不禁用文件修改；Shell 使用当前用户权限，不是沙箱。`--approve` 的审批覆盖可写子代理，`--exclude-tools` 同时约束独立与聚合工具入口；这些控制不是操作系统隔离。
- **生态与兼容性**：Mio 插件采用原生的 `mio-plugin.json` 规范，不是 Pi 的 TypeScript 扩展运行时；尚未对齐 Pi 的完整 TUI、交互会话树与扩展生态。
- **验证范围**：当前仅在 Linux 原生环境和本地测试中验收；未声明跨平台、预编译分发包或其他真实账号服务的全面可用性。

## 开发与测试

在提交前，严格通过以下四项验收检查：

```bash
cargo fmt --check
cargo check --all-targets --locked
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
```

测试集涵盖本地 HTTP 模拟、CLI 读写与恢复、边界安全和会话持久化，无需调用外部真实服务。

## 项目架构与许可证

项目采用三层分层架构：`mio-core`（模型与流式协议）、`mio-agent`（Agent 循环与工具调用）与 `mio-coding-agent`（原 `mio-agent` 重命名，提供编程工具、会话与 CLI）。

Mio 基于 [Rig](https://github.com/0xPlaygrounds/rig) 0.44.0 框架构建底层循环，项目采用 [MIT 许可证](LICENSE) 发布，并保留第三方依赖各自的许可证。完整来源记录见 [CREDITS.md](CREDITS.md)、[licenses/](licenses/) 与 [源码来源说明](docs/rig-source.md)。
