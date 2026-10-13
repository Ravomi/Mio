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

## 工作区工具

模型通过 `workspace` 工具的 `operation` 参数选择操作。

| 操作 | 用途与边界 |
|---|---|
| `read` | UTF-8 文本读取，`offset` 从第一行起、`limit` 限定行数；输出上限 64 KiB，输入上限 8 MiB。 |
| `write` | 原子写入，单次上限 1 MiB；父目录须已存在。 |
| `edit` | `old_text` / `new_text` 精确替换，拒绝零匹配、多处或重叠匹配；保留原文件权限，不自动转换换行符。 |
| `grep` | 字面文本搜索，支持目录或单文件；不是正则搜索。 |
| `find` | 按相对路径或文件名的子串查找；不是 glob。 |
| `ls` | 有界、排序的目录列表。 |
| `shell` | 交给宿主执行非交互命令；CLI 提供原生执行实现。 |

文件工具拒绝绝对路径、`..` 和符号链接路径。递归搜索跳过 `.git`、`target`、`node_modules` 等目录，不遍历符号链接，最多访问 5000 项、深度 64；`grep` 每文件上限 2 MiB、最多 500 条匹配，截断时返回说明。它不实现 `.gitignore` 规则。

## 项目指令、Skills 与模板

CLI 自动按祖先到当前目录的顺序加载 `AGENTS.md`，再附加当前项目 `.mio/SYSTEM.md`。`--no-context` 可关闭加载。库调用方使用 `ProjectContext::load` 和 `run_with_options` 显式接入。

项目 Skills 存放于 `.agents/skills/<name>/SKILL.md` 或 `.mio/skills/<name>/SKILL.md`。默认向模型提供目录摘要，正文按需读取；`--skill <name>` 可显式附加正文。当前支持单行 `name`、`description` 元数据，不提供完整 YAML 解析、全局安装或可执行扩展兼容。

```markdown
---
name: review
description: 检查代码修改与测试覆盖
---
先阅读改动涉及的代码，再分析行为变化与测试缺口。
```

提示词模板存放于 `.mio/prompts/<name>.md`，其中 `$ARGUMENTS` 替换为本次输入：

```bash
mio --template review -p '检查最近的错误处理修改'
```

单个上下文文件上限 32 KiB，总附加指令上限 128 KiB。模板只进行文本替换，不执行脚本。

## 会话与事件

会话在成功完成一轮后原子保存至 `.mio/sessions/*.json`，默认已被本仓库忽略。`--session <path>` 指定保存或恢复的文件，`--resume` 选择当前工作区最近保存的会话，`--fork` 创建新分支，`--list-sessions` 离线列出会话。

会话包含完整对话和工具输出，但不保存 `Config` 中的模型连接凭据。对话本身仍可能包含文件内容或用户输入的敏感信息，请按项目数据管理。文件采用版本化 JSON 快照，大小上限 16 MiB；Unix 保存权限为 `0600`。失败或取消时保留上次成功落盘的会话，不回滚已经发生的文件修改。

JSON Lines 事件形状示例：

```json
{"type":"text","data":"正在查看文件"}
{"type":"tool_started","data":{"operation":"read","target":"Cargo.toml"}}
{"type":"tool_finished","data":{"operation":"read","target":"Cargo.toml","result":{"Ok":"..."}}}
```

成功结束输出 `done`（包含会话路径与消息数量），错误输出 `error` 并返回非零状态码。逐行交互支持 `/help`、`/new`、`/fork`、`/sessions`、`/exit`，Ctrl-C 取消并退出。JSON 模式用于单次任务，不是双向 RPC 协议。

## Rust 集成与模型接入

`run` 保留简洁入口；`run_with_options` 可附加项目指令与配置工具轮次。`Host` 负责 `shell` 和 `on_event`。原生宿主可复用 `mio_agent::shell::execute`，也可自行提供执行策略、PTY 或隔离环境。回调仅为通知，不能拦截文件工具。

完整示例见 [`examples/host.rs`](examples/host.rs)。示例 Shell 明确返回“未实现”，不会伪造执行结果。`Config`、`Services` 支持库层的 OpenAI 兼容接口、Codex 与 DeepSeek，以及实验性的 Antigravity / DeepSeek 网页适配；CLI 暂不提供 OAuth 登录向导。

CLI 支持导入已有本机 Antigravity OAuth 登录及默认模型：

```bash
mio --import-antigravity ~/.pi/agent
mio --provider antigravity --subagents --cwd /path/to/project -p '委派只读子任务分析项目，收集结果后汇总'
```

导入目录须包含兼容的 `auth.json`、`settings.json`，以及已安装提供方的 OAuth 应用元数据。导入过程只解析数据，不启动外部 Agent 或执行插件 JavaScript。也可用 `MIO_GOOGLE_CLIENT_ID`、`MIO_GOOGLE_CLIENT_SECRET` 显式提供与原授权对应的 OAuth 应用信息。

授权、默认模型和思考档位保存到 `$MIO_CONFIG_DIR/antigravity-profile.json`；目录默认是 `$XDG_CONFIG_HOME/mio` 或 `~/.config/mio`。文件在 Unix 下为 `0600`，不修改原授权文件。主 Agent 和子代理共享账户服务及串行令牌刷新；刷新后的凭据只写入 Mio 配置。请求前读取实时模型目录，匹配模型及思考档位；不可用时明确报错，不换供应商。

导入后 CLI 默认使用该账户；`--provider` / `MIO_PROVIDER` 可显式选择供应商，`--model` / `MIO_MODEL` 可覆盖模型。显式提供兼容 API 的 `--base-url` / `MIO_BASE_URL` 且未指定供应商时选择 `custom`。Antigravity 使用固定服务端点，不接受自定义 `--base-url`。此功能复用已有登录，不提供新的交互式登录向导。

Antigravity 需要宿主显式注入 `OAuthConfig { client_id, client_secret }`。例如宿主自行读取 `MIO_GOOGLE_CLIENT_ID`、`MIO_GOOGLE_CLIENT_SECRET` 后传入；库不会自动读取它们。仓库不内置 OAuth 客户端凭据，也不保证任意 OAuth 应用均可使用对应服务。

## 原生子代理

```bash
mio --subagents --cwd /path/to/project -p '委派两个只读子任务分别分析模块结构和测试缺口，等待结果后汇总'
```

`--subagents` 为主模型增加 `subagent` 工具，支持 `delegate`、`status`、`snapshot`、`list`、`kill`、`fork`。每次委派返回独立 `run_id`，用它查询或取消该次运行；同名会话继续最后一次成功的历史，失败不会清空已有成功历史。子任务不会收到再次委派的工具。子代理文本保留在任务结果中，不混入主回答；主 Agent 的委派和查询操作仍输出工具事件。

默认最多同时运行 4 个任务，每个任务超时 600 秒。`status` 支持最多 30 秒长轮询；状态区分 `running`、`completed`、`failed`、`cancelled`、`timed_out`。快照提供当前文本和最近工具名，文本上限 32 KiB，截断时 `output_truncated` 为真。`completed` 表示执行结束，任务结果仍需主 Agent 检查。

委派参数示例：

```json
{"operation":"delegate","task":{"session":"review-tests","goal":"检查测试缺口","prompt":"只阅读测试及其对应实现，列出未覆盖的行为","read_only":true}}
```

子任务默认只读，执行层禁止 `write`、`edit`、`shell`；明确设置 `read_only: false` 可修改文件。CLI 的 `--no-shell` 同时约束子代理。工作目录须在父工作区内；同名会话不可并发，续接时目标、目录和访问模式不能改变。重叠目录中的任务只允许只读并行，写任务独占；主 Agent 的工作区工具也遵守这些占用规则。Shell 仍使用宿主权限，不受文件路径限制，这些规则不是操作系统沙箱。

Rust 宿主通过 `subagent::SubagentManager` 管理会话，将其放入 `RunTools.subagents` 后调用 `run_with_tools`；原有 `run`、`run_with_options` 不变。`RunTools.read_only` 也可独立用于只读主 Agent。子任务共用模型配置、附加指令和轮次上限，不继承主会话历史；模型不能通过委派参数修改服务地址或密钥。`new` 支持 `Custom`、`Deepseek` API；Antigravity 使用 `new_with_antigravity` 注入与主 Agent 共用的账户实例。其他账户服务需宿主自行提供执行器。宿主须保留同一管理器才能续接子会话。

子代理通过进程内异步任务运行，没有进程隔离。会话和运行记录仅在内存保存，每个管理器最多 128 个会话及 128 次运行，达到上限后需创建新管理器；每个子会话成功历史上限 2 MiB。管理器释放会取消运行中的任务。CLI 若主模型在子任务仍运行时结束，将取消未完成任务并报告错误。取消不回滚文件改动，已开始的阻塞文件操作仍可能完成。主会话恢复不会恢复子代理管理器。当前未提供持久化调度、自动多阶段重派或对外 MCP 服务。

## 当前边界

完整 TUI、运行中指令插入和任务队列、双向 RPC、自动上下文压缩、可执行扩展与扩展包生态仍未实现。当前的会话分支是完整快照复制，不是交互式会话树编辑。取消阻塞文件操作不会撤销已开始的写入。Ravomi 桌面宿主尚未完成接入。

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
