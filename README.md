<p align="center">
  <img src="assets/mio-logo.png" alt="Mio logo" width="240" />
</p>

# Mio

**Rust 编程 Agent：在终端完成编码任务，也能直接嵌入你的应用。**

Mio 将模型调用、代码工具和对话历史组织成完整工作流程：理解仓库、检索文件、精确修改、执行命令，再把任务保存为可继续的会话。

[官方网站](https://ravomi.github.io/Mio/) · [Rust 集成示例](examples/host.rs) · [许可证](LICENSE)

## 为什么使用 Mio

- **终端与应用共用一个核心。** `mio` 命令行与 `mio-agent` Rust 库使用相同的工具循环；桌面宿主可以进程内调用，无需额外维护另一套 Agent 后台。
- **修改有依据。** 内置分页读取、文本检索、文件查找和精确替换。替换内容必须唯一匹配，歧义时不修改文件；输出和扫描规模有明确上限。
- **项目规则随仓库走。** 加载 `AGENTS.md`、项目 Skills 和提示词模板，同一项目的编码约定可重复使用。
- **任务可以接着做。** 保存完整消息与工具结果，支持恢复和创建会话分支，继续探索时保留原会话。
- **过程可接入自己的界面。** 文本增量、工具开始和工具结果以结构化事件提供；CLI 支持 JSON Lines，Rust 宿主使用 `Host` 回调。
- **子任务可以独立推进。** 按需启用原生子代理，独立会话支持异步委派、状态查询、取消、续接和分叉；探索任务默认只读。
- **模型与界面解耦。** CLI 接入 OpenAI 兼容服务，包括兼容的本地模型；Rust 库还提供账户与其他服务适配。核心不绑定特定 UI 框架。

项目处于早期阶段，通过本地模拟服务和 Linux 原生命令测试验收；Antigravity 已在当前 Linux 环境完成真实账户调用验证。没有发布预编译安装包，也没有宣称不同平台或其他真实账号服务已全面验证。

## 开始使用

需要 Rust 1.95 或更新版本，以及一个支持流式工具调用的 OpenAI 兼容模型服务。

```bash
git clone https://github.com/Ravomi/Mio.git
cd Mio
cargo install --path . --locked
mio --help
```

配置模型与服务地址，通过环境变量提供密钥。以下地址和模型名为占位值，替换为你的服务配置；本地免密钥服务可不设置密钥。

```bash
export MIO_BASE_URL='https://your-provider.example/v1'
export MIO_MODEL='your-model-name'
# 在当前终端设置 MIO_API_KEY；也支持 OPENAI_API_KEY

mio --cwd /path/to/project -p '检查项目，解释主要模块的职责'
mio --cwd /path/to/project                 # 逐行交互
mio --cwd /path/to/project --resume -p '继续上次任务'
mio --cwd /path/to/project --resume --fork -p '尝试另一种方案'
mio --cwd /path/to/project --json --no-shell -p '阅读 README 并总结'
```

非终端标准输入作为一次任务读取：`printf '解释 Cargo.toml' | mio --cwd /path/to/project`。
命令会访问配置的模型服务；是否收费取决于服务提供方。

CLI 默认允许文件修改和 Shell 执行。`--no-shell` 仅禁用 Shell，文件工具仍可修改文件。Shell 使用当前用户权限、空标准输入，超时为 120 秒；不是交互式终端或操作系统沙箱。Unix 下取消时终止独立子进程组；其他平台仅处理直接子进程，尚未完成全面验收。

## 工作区工具与 MCP 扩展

模型既可通过聚合的 `workspace` 工具调用操作，也可直接调用与 `pi-coding-agent` 对齐的 7 个独立一级工具（`read`、`write`、`edit`、`bash`、`grep`、`find`、`ls`）。

| 工具 / 操作 | 用途与边界 |
|---|---|
| `read` | UTF-8 文本读取，`offset` 从第一行起、`limit` 限定行数；输出上限 64 KiB，输入上限 8 MiB。 |
| `write` | 原子写入，单次上限 1 MiB；父目录须已存在。 |
| `edit` | `old_text` / `new_text` 精确替换，拒绝零匹配、多处或重叠匹配；保留原文件权限，返回标准 Unified Diff（`--- a/path` / `+++ b/path`）。 |
| `grep` | 支持字面子串、正则表达式（`regex: true`）与大小写不敏感（`case_insensitive: true`）检索。 |
| `find` | 支持相对路径子串及 Glob 通配符（`*`、`**`、`?`）查找。 |
| `ls` | 有界、排序的目录列表。 |
| `bash` / `shell` | 交给宿主执行非交互命令；CLI 提供原生执行实现。 |

CLI 支持细粒度工具白名单与黑名单控制：
- `--tools` / `-t <list>`：仅启用指定工具（如 `-t read,grep,find,ls` 进入纯只读探索模式）
- `--exclude-tools` / `-xt <list>`：排除指定工具
- `--no-tools` / `-nt`：禁用全部内置工作区工具
- `--approve`：执行写操作或 Shell 前要求终端交互式确认（`[y/N]`）

若工作区存在 `.mio/mcp.json`（或 `~/.config/mio/mcp.json`），Mio 会通过 JSON-RPC 2.0 over stdio 自动握手外部 MCP Server 并动态注册 `mcp__<server>__<tool>` 工具（`--no-mcp` 可禁用）。

文件工具拒绝绝对路径、`..` 和符号链接路径。递归搜索跳过 `.git`、`target`、`node_modules` 等目录，不遍历符号链接，最多访问 5000 项、深度 64；`grep` 每文件上限 2 MiB、最多 500 条匹配，截断时返回说明。

## 项目指令、Skills、模板与 `@file` 展开

CLI 自动按祖先到当前目录的顺序加载 `AGENTS.md` 与 `CLAUDE.md`，再附加当前项目 `.mio/SYSTEM.md`。可通过 `--no-context`、`--no-context-files`、`--no-skills`、`--no-prompt-templates`、`--system-prompt`、`--append-system-prompt` 精细控制。

- **内联文件引用（`@file`）**：在提示词或命令行参数中使用 `@path/to/file` 或 `@path/to/file:10-50`，Mio 会在发送前自动内联展开对应文件或行区间内容。
- **项目 Skills**：自动发现 `.agents/skills/<name>/SKILL.md`、`.mio/skills/<name>/SKILL.md` 与 `.claude/skills/<name>/SKILL.md`。默认提供目录摘要，正文按需读取；`--skill <name>` 可显式附加正文。
- **提示词模板**：存放于 `.mio/prompts/<name>.md`，其中 `$ARGUMENTS` 替换为本次输入：

```bash
mio --template review -p '检查 @src/workspace.rs:1-120 的错误处理修改'
```

## 会话管理、上下文压缩、HTML 导出与双向 RPC

会话在成功完成一轮后原子保存至 `.mio/sessions/*.json`。
- `--name` / `-n <name>`：为会话设置易读名称
- `-c` / `--continue` 或 `-r` / `--resume [id|name|path]`：恢复最近会话，或按 UUID 前缀、会话名、路径恢复指定会话
- `--fork [id|name|path]`：从现有会话分叉新分支
- `--no-session`：临时无痕运行，不落盘会话文件
- `--export <out.html>` 或交互式 `/export [out.html]`：将会话导出为单文件自包含 HTML 报告
- `--auto-compact` 或交互式 `/compact [instructions]`：将早期历史消息归纳压缩为结构化摘要并保留最近上下文

支持三种运行模式（`--mode text|json|rpc`）：
- `text`：默认终端交互模式，内置 `/help`、`/new`、`/fork`、`/sessions`、`/name`、`/compact`、`/export`、`/model`、`/thinking`、`/subagents`、`/status`、`/clear`、`/exit` 斜杠命令。
- `json`：单次任务 JSON Lines 事件流输出。
- `rpc`：双向 JSONL RPC 协议（支持 `prompt`、`steer`、`follow_up`、`abort`、`compact`、`get_state`、`set_model`、`set_session_name`、`export_html`、`new_session`、`fork_session`），并在工具执行间隙支持实时 `Host::poll_steering` 引导注入。

## 多供应商目录与思考深度调节

- 支持通过 `--thinking <off|minimal|low|medium|high|xhigh>` 或 `--model [provider/]model[:thinking]`（如 `deepseek/deepseek-reasoner:high`）统一控制推理深度。
- 支持 `--list-models [search]` 列出内置多供应商目录（`antigravity`、`openai`、`anthropic`、`google`、`deepseek`、`openrouter`、`groq`、`xai`、`mistral`、`ollama`）及对应环境变量配置状态。

## 插件标准（`mio-plugin.json`）与 GitHub 插件社区

Mio 提供开放的插件标准 v1（`mio-plugin.json`）与官方 GitHub 插件社区仓库 **[Ravomi/mio-plugins](https://github.com/Ravomi/mio-plugins)**。每个插件可声明并打包：
- **Skills**（`skills/<name>/SKILL.md`）
- **提示词模板**（`prompts/<name>.md`）
- **上下文规则文件**（`context_files`）
- **MCP 服务配置**（`mcp`）
- **声明式命令工具**（`tools`，自动以 `plugin__<plugin>__<tool>` 暴露给模型并受 `--tools` / `--approve` 管控）

常用插件管理命令（同时支持交互式 `/plugins` 与 `/plugin` 斜杠命令）：

```bash
# 搜索官方社区注册表 (https://github.com/Ravomi/mio-plugins)
mio --plugin-search

# 从官方社区一键安装插件（如 git-workflow / rust-expert / security-audit / docs-writer）
mio --plugin-install git-workflow

# 从任意 GitHub 仓库（或子目录）或本地目录安装
mio --plugin-install github:Ravomi/mio-plugins#plugins/rust-expert
mio --plugin-install ./my-plugin

# 列出已安装插件 / 卸载插件
mio --plugin-list
mio --plugin-remove git-workflow

# 脚手架创建新插件并校验标准合规性
mio --plugin-init ./my-plugin
mio --plugin-validate ./my-plugin
```

## 开发与验证

```bash
cargo fmt --check
cargo check --all-targets --locked
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
```

测试包含本地 HTTP 模拟、完整 CLI 编辑与续接流程、文件边界、会话持久化及 Unix 子进程取消，不调用真实模型服务。官网源文件位于 [`docs/`](docs/)，通过 GitHub Pages 发布 `main` 分支的 `/docs` 目录，无需前端构建。

## 项目背景与致谢

Mio 从 [Ravomi](https://github.com/Ravomi/ravomi-app) 的 Agent 代码中抽离，独立发展为通用编程 Agent。来源记录见 [CREDITS.md](CREDITS.md)。

感谢 **[Rig](https://github.com/0xPlaygrounds/rig)** 提供 Rust LLM 框架基础。Mio 保留 Rig 依赖及第三方许可，以 **MIT** 发布；相关声明见 [LICENSE](LICENSE)、[licenses/](licenses/) 和 [CREDITS.md](CREDITS.md)。
