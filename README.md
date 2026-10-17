<p align="center">
  <img src="assets/mio-logo.png" alt="Mio logo" width="240" />
</p>

# Mio

Mio 是一个以**共享调度与可控多代理协作**为核心的 coding agent 引擎：无头 Rust 实现，可在终端独立运行，也可通过 Rust 库直接嵌入宿主应用，由引擎统一管理模型请求、任务并发与工作区写入的调度和限额。

[官方网站](https://ravomi.github.io/Mio/) · [技术文档](https://ravomi.github.io/Mio/documentation.html) · [产品与架构 PRD](PRD.md) · [安装指南](docs/installation.md) · [程序状态规范](docs/program-status.md) · [源码来源](docs/rig-source.md) · [示例](examples/host.rs) · [完整离线宿主示例](examples/host_loopback.rs) · [许可证](LICENSE)

本文涵盖 [定位与架构](#定位与架构)、[CLI 与工具过滤](#常用-cli-使用示例与工具过滤)、[RPC 运行控制](#rpc-运行控制与协议交互)、[异步审批](#异步审批机制---approve)、[模型与子代理](#模型切换thinking-思考等级与子代理)、[上下文与会话](#上下文预算管理与会话持久化)。

## 定位与架构

Mio 针对的实际痛点是：模型限流/429 触发请求风暴、并行改动互相覆盖、编译/浏览器等重任务争抢资源、重复探索与预算/验收成本失控。近期验收口径围绕这些痛点收敛：

| 指标 | 验收含义 |
| --- | --- |
| 429 不成风暴 | 同一 Provider 命中限流后请求共享冷却，等待或快速失败，而不是各自重试放大 |
| 同 Provider 共享冷却 | endpoint + 凭据摘要键的冷却对父子任务与协作进程一致可见，而非每个任务各自维护 |
| 冲突任务排队、无关任务并行 | 同一工作区的写入在进程内串行排队或显式拒绝，写不同目标的轻任务仍并行；跨进程路径租约已接入工作区操作，取消后仍由实际执行 worker 持有至结束 |
| 重任务配额 | 编译、浏览器等重任务有独立并发/时间配额，不被轻任务挤占，也不无限并发 |
| 预算预留与真实 usage | 请求前预留预算、请求后记录 Provider 返回的真实用量；缺 usage 标记 unknown，不用字节估算冒充账单 |
| 显式任务 key 去重 | 重复提交同一任务 key 时复用或拒绝，而不是重复执行 |
| 结构化验收证据 | 成功、失败、取消都留下可查证据，验收以测试与真实运行结果为准 |

### 三层纵向架构

依赖方向自下而上为 `mio-coding-agent` → `mio-agent` → `mio-core`；底层不依赖终端、代码工作区或产品会话策略。

| 层 | Crate | 职责 |
| --- | --- | --- |
| 模型与流式协议层 | `mio-core` | Provider 适配与凭据解析、模型目录、请求与流式响应、工具调用协议、思考等级映射、取消与网络错误 |
| 无 I/O 状态机与 Hook/Bus 运行时层 | `mio-agent` | 纯状态机 Agent 循环、工具调度、生命周期与事件总线、Hook/宿主回调；不直接触盘、联网或依赖终端 UI |
| 编程工具、调度与宿主适配层 | `mio-coding-agent` | 工作区代码工具、共享调度与配额、会话与 journal 持久化、子代理、MCP/插件扩展、CLI 与 RPC 宿主适配 |

### 四大控制平面

| 控制平面 | 覆盖范围 | 主要实现 |
| --- | --- | --- |
| 共享调度与配额 | 模型请求、重任务与工作区写入在真实 dispatch 边界准入；进程内 `Coordinator` 与跨进程 `SharedLimits`（`flock`）双层配额、429 共享冷却、显式 dedup 与本地预算 | `scheduling.rs`、`coordinator.rs`、`shared_limits.rs` |
| 工作区与子代理执行 | 原子写入、唯一匹配编辑与编辑前内容/身份校验、跨进程路径租约、整工作区 writer 独占与只读并行、`Queued`/`Running` FIFO 子代理与 `ToolEvidence` | `workspace.rs`、`subagent.rs` |
| 上下文与持久化恢复 | 逐轮预算判定与工具成对边界压缩、`.mio/sessions/` 原子持久化与恢复/分叉/导出、`.mio/journal` 执行证据与 `--recovery` 报告 | `context_budget.rs`、`session.rs`、`journal.rs` |
| 宿主与扩展交互 | `Host` trait 进程内嵌入、`text`/`json`/`rpc` 三态输出、异步审批、MCP 与 `mio-plugin.json` 插件、OSC 7501 终端状态报告（仅 CLI 文本模式且 stderr 为 TTY） | `Host`、RPC 宿主、`mcp.rs`、[OSC 7501 状态规范](docs/program-status.md) |

同核两用：`mio` CLI 与 `mio-coding-agent` Rust 库共享同一 Agent 工具循环，可在终端独立运行，也可由宿主应用进程内嵌入。

近期共同优先服务独立终端 CLI 与 Ravomi 内嵌宿主。分层契约、示例与平台 feature 检查见 [SDK 契约](docs/sdk.md)；第三方集成沿用同一公共 API。

模型目录的 `configured` 只保留凭据或本地预设提示，不代表可达或认证有效。JSON 目录另列 `credential_present`、`reachable`、`authenticated`、`protocol_supported`；未探测的状态为 null。CLI 与站点优先读取同一底层 catalog，保留能力来源与快照 SHA-256，找不到的模型仅标明预设声明。

### 能力状态与四阶段路线

路线图顺序依据已复现缺陷与宿主引擎定位，与 [需求与验收口径](docs/agent-requirements-and-comparison.md) 对齐：

- **阶段一 核心防线与共享调度（已落地）**
  - 已落地：CLI/RPC 显式任务 key 工作区持久化去重；工具循环与会话持久化；进程内调度与跨进程协作配额（`flock` 模型/重任务配额与 429 共享冷却）；子代理 `Queued`/`Running` FIFO 与 `ToolEvidence`；Anthropic/Google/Ollama 原生协议路由；项目 MCP 启动前指纹授权；编辑前内容/身份校验与跨进程 `Workspace` 路径租约；执行证据 journal；插件输出/MCP 帧/RPC 行大小限额；结构化 `completion_call`/`diagnostic` 事件；四项 Cargo 质量门禁（fmt / check / test / clippy）与站点校验通过。
  - 残余边界：验收限于本地模拟端点、单元与多进程测试，不等于真实 Provider、真实崩溃或跨平台验收；跨进程预算保守保留崩溃时的在途预留，硬件断电与跨平台仍待验收。
- **阶段二 恢复、连接与事件**：补齐崩溃恢复闭环与端到端恢复验收，保证各阶段中断后证据完整且不重复副作用；MCP trust store 已迁到用户配置目录，`required` / `optional` 与有界连接复用（`tools/list` 重连一次、`tools/call` 不重放）已落地；可选文件检查点与 rewind、超出内联上限的输出外置与 `read` byte 分页已落地；全入口工具门禁、版本协商与 ID 关联、宿主显式 Pinned 约束、未知副作用账本及发布意图恢复已整合并通过主树回归。真实 SIGKILL 12 项与检查点崩溃 5 项通过，证据绑定的版本见验收记录。
- **阶段三 评测、检索与隔离**：同模型同预算对照评测（正确性、成本、延迟、缓存、取消与恢复）；大仓库检索选型（repo map / LSP / 符号索引）；Linux bubblewrap backend、状态查询与扩展拒绝已接入并有严格本机验收；其他平台隔离仍待验证；拆分 `src/bin/mio.rs`，收口 helper crate 平台/feature 矩阵与 registry 发布。
- **阶段四 宿主与产品**：优先验证独立 CLI 与 Ravomi 宿主的真实任务；ACP/HTTP、会话树、Plan 模式与后台作业在有需求证据后独立演进并可回退。

**诚实边界**：调度、路径租约与限额都是协作式准入，不是操作系统内存 / CPU / 文件描述符沙箱；`--max-total-tokens` 是 token 计数而非美元硬限额，无美元换算；取消只停止后续计算并保留上次成功落盘的历史，不回滚已写入磁盘的修改。默认执行环境为 `unrestricted`；Linux 可显式选择 `--sandbox bubblewrap`，已有本机子进程读写、网络与每进程资源上限实测，宿主扩展拒绝契约已接入，冻结版严格 Linux backend 与输出磁盘写满测试共 42 项通过，具体版本与范围见 [本轮验收记录](docs/reviews/2026-10-17-v1-inline8k-acceptance.md)。资源上限不等于进程树总配额，也没有精确费用硬限额；DeepSeek 固定模型批次已通过本机小样本门槛；其他 Provider、429 压力、跨平台与硬件断电仍待验收；现有 SIGKILL、检查点与未知结果恢复证据不能替代这些场景。

逐项证据与残余风险见 [TODO.md](TODO.md)，需求与验收口径见 [docs/agent-requirements-and-comparison.md](docs/agent-requirements-and-comparison.md)。

命令行约定：`mio` 保持 pi 式终端使用方式，支持直接对话、`-p`、管道及 JSON/RPC；交互模式为 inline TUI（底部输入框、动态状态、多代理字母与颜色标识），不进入 alt-screen、不擦除滚动历史，不增加独立 GUI。

## 源码安装与快速开始

源码编译需要 Rust 1.95 或更新版本。其他分发方式及发布状态见 [安装指南](docs/installation.md)。从 `main` 分支克隆并编译：

```bash
git clone https://github.com/Ravomi/Mio.git
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

终端 CLI 也会读取用户配置目录（默认 `~/.config/mio/settings.json`）中的
`defaultProvider`、`defaultModel`、`baseUrl` 和 `apiKey`。显式命令行参数优先，
其次是该文件，最后才是对应的 `MIO_*` 环境变量；这样已有终端中的旧环境值不会
覆盖已更新的用户默认模型。配置含密钥时请限制为当前用户可读（例如权限 `0600`）。
模型目录列出的名称并不保证当前密钥可调用；应以真实请求或供应商权限信息为准。

## 常用 CLI 使用示例与工具过滤

以下示例覆盖会话恢复、审批、工具集裁剪及子代理调度：

```bash
# 常用提示词与会话续接
mio --cwd /path/to/project -p '阅读 Cargo.toml 并解释项目依赖'
mio --cwd /path/to/project --resume -p '继续上次任务并补充单元测试'
mio --cwd /path/to/project --resume --fork -p '从最新历史分叉出新会话探索方案 B'

# 工具过滤：白名单、增删修改与完全禁用
mio -t 'read,write,edit' -p '只允许文件读写与精确修改'
mio --subagents -t '+subagent,-bash' -p '在默认工具集基础上禁用 bash 并开启 subagent'
mio -xt 'bash,write' -p '排除命令执行与直接写入'
mio -nt -p '纯文本问答，禁用所有内置与外部工具'

# Linux 子进程隔离（需安装 bubblewrap；不可用时拒绝执行）
mio --sandbox bubblewrap -p '运行项目检查'
mio --sandbox bubblewrap --sandbox-read-only /opt/my-runtime -p '使用已挂载的运行环境检查项目'
mio --sandbox-status                      # 离线查询，available 仅检查 helper 与平台

# 启用受限/探索子代理并发
mio --subagents -p '分析整个仓库的代码质量，使用只读子代理并发检索'

# 敏感操作交互审批与无 Shell 模式
mio --approve -p '修改 src/main.rs 并编译'
mio --no-shell -p '仅允许操作文件，禁止执行任何终端 Shell 命令'

# 上下文预算覆盖与自动压缩
mio --context-window 65536 --max-output-tokens 8192 -p '处理超大代码重构'   # 自动压缩默认开启，--no-auto-compact 关闭

# JSON Lines 流式输出与 RPC 守护模式
mio --json -p '统计代码行数并输出结构化事件'
mio --mode rpc --cwd /path/to/project --approve

# 项目 MCP 启动授权与中断运行的执行证据报告
mio --trust-project-mcp -p '使用项目 .mio/mcp.json 中的 MCP Server'
MIO_TRUST_PROJECT_MCP=1 mio -p '用环境变量显式信任项目 MCP 配置'
mio --recovery                       # 打印工作区全部 .mio/journal 中断运行证据
mio --resume --recovery              # 只看当前会话的持久化执行证据
```

### 工具过滤语义说明

- **白名单模式 (`-t` / `--tools`)**：支持全量指定（如 `read,write,bash`）或差量调整语法（以 `+` 或 `-` 开头的项，如 `+subagent,-bash`）。
- **黑名单模式 (`-xt` / `--exclude-tools`)**：明确排除匹配的工具名或通配符，同时作用于直接调用的独立工具及工作区聚合入口。
- **完全禁用 (`-nt` / `--no-tools`)**：禁用所有工具调用能力；与 `--tools` 互斥，同时传入将报错。
- **内置工具列表**：`workspace`、`read`、`write`、`edit`、`bash`（别名 `shell`）、`grep`、`find`、`ls`、`subagent`。

文件工具可选用整文件哈希锚点：`read` 传 `include_sha256: true` 时，行分页结果前返回原文件字节的 SHA-256，字节分页结果增加 `sha256` JSON 字段；随后 `edit` 传 `expected_sha256`，若文件内容已变化则在写入和检查点前拒绝。哈希覆盖完整文件（包括未展示的分页及换行字节），不是所选 `old_text` 的哈希；不传这两个字段时保持原有读写行为。该校验仍是乐观冲突检测，不能替代非协作写入者参与的原子比较写入。

`edit` 可传 `preview: true`，按正式编辑的唯一匹配、文件校验和可选哈希条件计算结果，返回 JSON 字符串（位置、前后整文件 SHA-256、字节数、有界 diff 与截断标志），不写入文件或创建检查点。预览期间收到用户 steer 消息时，结果以 `user_steering_message` 字段保留消息及 JSON 格式。预览仍需通过 `edit` 的工具过滤、只读限制、授权与写租约；预览后正式编辑可同时携带 `before_sha256` 作为 `expected_sha256`、`after_sha256` 作为 `expected_after_sha256`，在写入和检查点前核对原文件与完整结果，任何一项不符均拒绝。只提供 `expected_after_sha256` 或在预览请求中提供它会拒绝。预览不是写入承诺，也不是原子 CAS。

过滤器只决定允许暴露哪些工具；`+subagent` 不会代替 `--subagents` 启用子代理。`--no-shell` 仅禁用 Shell，不禁用文件写入，也不代表 MCP 或插件工具没有外部副作用。默认 CLI 允许文件修改和 Shell 执行，Shell 使用当前用户权限，不是沙箱。`--max-turns N` 控制工具循环预算，范围为 1–256，默认 32。

`grep` 工具支持 `context_lines`（0–20，默认 0），可在一次搜索中返回命中行及邻近说明。例如工具参数 `{"pattern":"parse","path":"src","context_lines":2}`。命中行使用 `path:行号:`，邻近行使用 `path-行号-`；相交窗口去重，所有内容仍受原 64 KiB 输出上限约束，截断会明确标记。聚合 `workspace` 的 `grep` 操作使用相同参数。

### 项目 MCP 授权与执行证据恢复

- **`--trust-project-mcp`**：项目 `.mio/mcp.json` 中的 MCP Server 默认 fail-closed，交互终端需逐次确认，非交互 / JSON 模式直接拒绝；该参数或环境变量 `MIO_TRUST_PROJECT_MCP=1`（亦接受 `true` / `yes`）显式信任后才启动。授权绑定具体服务器指纹（命令、参数、环境变量），配置变化会重新请求，不会只凭 cwd 布尔值长期信任。
- **持久信任目录**：CLI 把已批准的项目 MCP 服务器记在用户配置目录下 `mcp-trust/<sha256(规范化工作区)>/trust.json`（文件 0600、目录 0700，临时文件原子替换），只保存指纹摘要、服务器名、工作区与批准时间，绝不写入命令或环境变量原文。信任根绝不允许位于项目内；符号链接、项目内路径、版本不符、权限过松或损坏的信任库都不授予任何信任并重新请求授权，其中来源不可信的存储同时拒绝写入，损坏或权限过松的存储可由一次显式批准原子修复。同工作区两个进程并发批准是最后写入者生效，丢失的条目只会导致再次授权，不会误信未批准的服务器。
- **嵌入宿主默认不持久授权**：`Host::allow_persisted_mcp_trust` 默认返回 `false`，`mcp_trust_directory` 默认 `None`，库宿主每次启动项目服务器都必须自己批准，不能复用其他宿主写入的信任；CLI 显式选择持久信任。
- **required / optional**：`.mio/mcp.json` 中每个服务器可用 `required`（默认 `true`）或 `optional: true`（两者同时出现时 `optional` 优先）声明。可选服务器启动失败、授权被拒或 `tools/list` 失败只打印诊断并跳过，不阻断其他服务器；必需服务器失败仍会报错终止工具加载。
- **连接复用与重放边界**：进程内按「工作区 + 服务器名 + 指纹」缓存有界连接（上限 64，超出按 LRU 淘汰；被淘汰的连接在其最后一个句柄释放时回收子进程组）。配置变更立即换新连接。`tools/list` 是只读调用，传输失败时最多重连一次；`tools/call` 的传输失败绝不自动重放，而是丢弃连接并把结果标为副作用未知。
- **`--recovery`**：关联磁盘已保存会话与 `.mio/journal` 执行证据，展示已保存消息数、中断请求、已执行、结果未知与失败的操作（含 turn 与 run id）；未保存或损坏的会话明确标记。请求摘要最多 4096 字符并报告截断，旧证据没有请求文本时不补造。不带选择器时遍历工作区全部 journal；配合 `--session` / `--resume` 只看指定会话。与 `--no-session` 互斥。查询只读，不重放操作或回滚文件。
- **未决未知副作用维护**：`--list-unresolved-effects` 只读列出持久账本中每条记录的精确 `(scope, callIndex)`、状态与快照摘要；`--ack-unresolved-effect '{"scope":"...","call_index":N,"snapshot":"<sha256>"}'` 才按精确 key+当前快照确认一条 `Unknown`。两者都是无模型、无 Provider 的独立维护命令，不是模型可调用的工具，也从不删除账本；快照过期或证据不完整时拒绝确认。RPC `get_state.data.unresolvedEffects` 只提供只读摘要，逐条确认仍走该维护通道。

### 可选文件检查点

`mio --checkpoints -p '修改文件'` 会在 `write` / `edit` 前保存文件快照；默认关闭，也可用环境变量 `MIO_CHECKPOINTS=1` 或终端 `/checkpoints on` 开启。快照与索引存放在工作区 `.mio/checkpoints/`。

`mio --list-checkpoints` 列出记录（`id` / 状态 / 操作 / 目标），`mio --rewind ID` 回退一项，均不调用模型；`--list-checkpoints` 与 `--rewind` 互斥，且都不能与 `-p` 或 `--mode rpc` 同时使用（`--checkpoints` 本身可用于普通会话）。

- **选择与状态**：`--rewind` 接受完整 ID 或至少 4 个字符的唯一前缀。只有状态为 `Active` 的检查点能回退；`Pending`（写入结果未确认）、`Failed` 与 `Rewound` 都被拒绝，同一检查点不会重复回退。
- **用户后续修改保护**：仅当目标文件当前内容与检查点记录的「操作后指纹」一致时才恢复，否则拒绝覆盖（文件被手工修改或删除分别报错）；由检查点创建的文件也只在内容仍匹配时删除。
- **一致性语义**：rewind 与 `write` / `edit` 共用同一套工作区写租约、只读检查与工具过滤，并在 `--approve` 下同样要求宿主审批；跨进程索引改动在 `.mio/checkpoints/index.lock` 的 `flock` 内串行，避免两个进程同时回退同一项。它只按内容指纹做乐观拒绝，不是 CAS，非协作的外部写入者仍可能在预检与写入之间改动文件。
- **边界**：只恢复普通文件内容，不修改 Git 历史，也不回滚 Shell 副作用，且只覆盖 `write` / `edit`。与 `--no-session` 互斥，无持久化时检查点保持禁用且零磁盘写入。索引损坏、版本不符或落点异常时拒绝新快照与 rewind，并保留原始证据。

### 长输出外置与 byte 分页

- **8 KiB 内联上限**：Shell 与插件命令的 stdout / stderr 不超过 8 KiB 时内联；超过时仅内联约 2 KiB 预览，完整输出外置到工作区 `.mio/outputs/<uuid>.<stream>`，结果里附带 `[TRUNCATED: inline output exceeds 8 KiB; complete output: ...; N bytes]` 引用。MCP 工具的大结果走同一外置路径，小于 8 KiB 的结果仍按结构化 JSON 原样返回。完整输出可通过 `read` 分页取回；外置会增加磁盘写入，仍受原工作区与输出存储边界约束。
- **8 MiB 硬上限**：单流超过 8 MiB 时 Shell / 插件工具失败并终止其进程（MCP 结果超限则该次调用返回错误），不返回「截断成功」，也不留下半份文件；取消或失败会清理未提交的外置文件。
- **`read` byte 分页**：长单行输出无法按行分页，可改用 `read` 的 `byte_offset` / `byte_limit`（4–65536 字节，0 基 UTF-8 偏移）逐段读取。该模式返回 JSON（`text`、`byte_offset`、`next_byte_offset`、`total_bytes`），按 `next_byte_offset` 继续；不能与行分页 `offset` / `limit` 同时使用，也不会在字符中间切开 UTF-8。
- **读取上限**：`read` 的 byte 分页与行分页都拒绝超过 8 MiB 的文件，并提示改用分段 Shell 处理；byte 分页只接受工作区内不被符号链接重定向的普通文件。
- **显式清理**：默认保留已发布输出。`mio --output-cleanup-plan /tmp/mio-output-plan.json` 生成离线计划，审阅后用 `mio --output-cleanup-confirm /tmp/mio-output-plan.json` 确认；无需模型凭据。仅处理超过 7 天且能证明无引用的已登记文件，活动运行、陈旧计划、损坏或不完整的引用证据会阻止清理；工作区之外的会话来源不扫描其私密内容，而是拒绝清理。

### CLI 取消与交互恢复

运行任务时按 Ctrl-C，会取消当前请求并清理活动子任务及 Shell 进程；已成功保存的会话保留，已发生的文件修改不回滚。终端交互模式在取消或普通回合错误后继续接受输入；在输入提示符处按 Ctrl-C 则退出。单次 `-p` 任务取消或失败以非零状态退出。终端编辑器支持 Tab 补全和上下键历史，`/status` 查看状态，`/exit` 退出。

## 共享调度、配额与多代理边界

调度在真实 dispatch 边界准入：模型请求、重任务与工作区写入在发出前申请许可。CLI 默认同时启用进程内调度与跨进程配额。

```bash
# 默认：进程内调度 + 用户配置目录下的跨进程协调
mio --cwd /path/to/project --subagents -p '并行探索并汇总'

# 跨进程共享配额根（同一 root 的策略固定，不能被本地参数悄悄放大）
mio --shared-limits-dir /tmp/mio-coordination -p '多进程共享模型/重任务配额'

# 显式只用本进程配额，不读写跨进程 root
mio --no-shared-limits -p '仅本进程限流'

# 并发参数只收窄本进程；预算参数还会持久化到共享 root
mio --max-model-concurrency 1 --max-provider-concurrency 1 \
    --max-heavy-tasks 1 --max-subagents 2 \
    --max-model-calls 50 --max-total-tokens 200000 -p '受限运行'

# RPC：实时查看本地调度状态
mio --mode rpc --cwd /path/to/project --subagents
# 然后发送：{"type":"get_state","id":"s1"}
```

- **两层配额**：进程内 `Coordinator` 管理本进程的模型 / provider / 重任务 / 任务队列、显式 dedup 与本地预算；跨进程 `SharedLimits`（`--shared-limits-dir`，默认用户配置目录的 `coordination/`）通过 `flock` 共享模型（全局默认 2、每 provider 默认 2）与重任务（默认 2）配额。同一 root 首次写入版本化 `policy.json`，之后任何进程必须逐项一致，否则 fail-closed。
- **参数作用域**：模型/Provider/heavy/子代理并发参数收窄本进程，不放宽 `policy.json` 的共享槽位。启用共享协调时，`--max-model-calls` / `--max-total-tokens` 同时建立或收窄同一 root 的持久预算 `budget.json`；后续进程未传预算则继承，显式放宽会报错。该预算累计跨运行生效，需独立预算周期时使用新的 `--shared-limits-dir`。`--no-shared-limits` 只使用本地预算。
- **429 冷却**：Provider 返回 429 时按 `Retry-After`（delta-seconds 或 IMF-fixdate GMT 日期）发布冷却，键为 endpoint + 凭据摘要；同 provider 的父子任务及协作进程共享，**不会自动重放工具或请求**。
- **重任务判定**：`bash`/`shell`、`workspace` 的 shell/bash 操作，以及所有 `plugin__…` / `mcp__…` 调用保守计入 heavy 配额。通过 Shell 启动的进程随该调用受限；未被 dispatch hook 接入的工具或其他直接启动的进程不保证受限。
- **预算语义**：`--max-model-calls` 是硬调用次数上限；`--max-total-tokens` 在调用前按保守预留占用，结算时用 Provider 上报的真实 usage，未上报按预留计。实际超出预留会记录 overage 并阻止后续请求；这是 **token 计数而非精确费用硬限额，无美元换算**。
- **子代理排队与证据**：`SubagentManager` 状态为 `Queued` / `Running` / 终态；达到并发上限或工作区忙碌时排队而非拒绝，严格 FIFO，整工作区 writer 独占、只读并行。相同 session、父运行 ID（包括均未知）且完全相同的活动请求复用同一 `run_id`；已知的不同父运行不能复用，终态不会被该路径自动重放。`ToolEvidence` 只来自真实 `ToolFinished`，其中 `ok` 仅表示工具自报结果，不是测试判决或回滚保证；执行前规范化拒绝另记在有界的 `denied_calls`。
- **跨进程可观测边界**：跨进程各持有哪些槽位无法可靠枚举，`get_state.scheduler.shared` 返回配置、冷却和持久预算计数，不伪造全局 active 计数。崩溃遗留预留不自动退款，可能保守耗尽预算；账本损坏、丢失或不可写时拒绝后续模型请求。
- **显式任务 key**：单次 CLI 使用 `--dedup-key KEY -p '任务'`，RPC 的 `prompt` / `follow_up` 使用 `dedup_key` 字段。同一工作区共享 `.mio/dedup/task-keys.json`；排队、活动和所有终态均保留，重复提交返回原 `task_id`，不再次执行。失败或取消后重试须使用新 key；强杀后记录可能保持 active/queued，仍阻止重放。key 为 1–512 字节、不得全为空白或含控制字符。CLI `--dedup-key` 与 `--no-session` 互斥；RPC `--no-session` 只在当前进程内去重。账本损坏或达到大小/条目上限时拒绝新任务，不自动淘汰历史 key。
- **无 OS 隔离**：调度、路径租约与限额都是协作式准入，不是操作系统内存 / CPU / 文件描述符沙箱；未接入模块的第三方工具与进程不受约束。

## RPC 运行控制与协议交互

使用 `mio --mode rpc` 可启动常驻的 stdio JSONL RPC 服务。每行输入一个 JSON 命令，stdout 每行输出一个 JSON 事件。

stderr 用于诊断，客户端须持续读取 stdout；保持 stdin 打开以接收任务结果，不能把发送完一批 JSON 后立刻关闭管道当作等待完成。完整协议见 [docs/rpc.md](docs/rpc.md)。命令应答表示是否接受命令，不表示任务已经完成。

### 运行控制核心机制

- **队列与启动**：空闲时，`prompt`、`steer` 与 `follow_up` 均可直接开启新回合。运行中时，`prompt` 会被明确拒绝；`steer` 进入专用插队队列（最多排队 64 条），在当前工具安全点注入给模型（若当前轮没有工具调用，则本轮结束后先处理未消费的 steer）；`follow_up`（最多排队 64 条）按 FIFO 队列在当前任务及 steer 彻底完成后顺延启动。
- **引导与状态**：`steer` 在工具完成后附加到工具结果，不中断已发出的模型请求或正在运行的工具。消息须为非空文本且不超过 65536 字节。`get_state` 在运行中可用：`isStreaming` 表示有活动任务，`followUpCount` 为排队数量，`pendingApprovalCount` 为待审批数量；运行中的历史消息数取本轮开始前的已保存快照。
- **状态变更限制**：当 Agent 正在流式处理或工具正在执行时，任何修改会话历史（`compact`、`new_session`、`fork_session`、`set_session_name`）或切换模型（`set_model`）的操作均会被直接拒绝；如需切换，须等待进入空闲或显式发送 `abort`。
- **`abort` 语义**：取消当前模型推理 Future、运行中的工具、活动子进程与子代理，并彻底清空 pending 的 steer 与 follow_up 队列。取消仅停止后续计算，保留上次成功落盘的历史，**绝不回滚已写入文件的磁盘修改**。若当前原本空闲，应答的 `aborted` 字段为 `false`。
- **终止清理与孤儿子进程**：RPC 关闭 stdin (EOF) 会取消活动任务并退出；任务错误清空排队。在 Unix 平台，Mio 启动的 Shell 工具使用独立进程组，取消时终止该进程组；其他平台只保证处理直接子进程。EOF 不保证发出结束事件，客户端还需处理进程退出。

### 可复制的 JSONL RPC 交互示例

#### 启动回合、引导 (Steer) 与追加 (Follow-up)

```json
{"type":"prompt","id":"p1","message":"分析 src/main.rs 并规划重构"}
{"type":"steer","id":"s1","message":"在下一次调用工具时优先检查依赖引入"}
{"type":"follow_up","id":"f1","message":"重构规划完成后接着生成迁移计划"}
{"type":"get_state","id":"st1"}
```

标准应答与关键生命周期事件流：

```json
{"type":"response","command":"prompt","id":"p1","success":true,"data":{"status":"started"}}
{"type":"text","data":"正在分析项目结构..."}
{"type":"tool_started","data":{"operation":"read","target":"src/main.rs"}}
{"type":"tool_finished","data":{"operation":"read","target":"src/main.rs","result":{"Ok":"fn main() {}"}}}
{"type":"done","session":"/path/to/project/.mio/sessions/example.json","messages":2}
{"type":"agent_settled","status":"completed"}
```

> **注意**：`done` 表示单次回合已成功提交会话状态（`--no-session` 时仅更新内存，`session` 为 `null`），但如果存在排队的 `steer` 或 `follow_up`，系统仍会继续流转；客户端应以收到 `agent_settled`（状态为 `completed`、`cancelled` 或 `error`）作为整个工作链彻底静止的标志。

#### 中途取消任务 (`abort`)

```json
{"type":"abort","id":"a1"}
```

应答示例：

```json
{"type":"response","command":"abort","id":"a1","success":true,"data":{"aborted":true}}
{"type":"agent_settled","status":"cancelled"}
```

## 异步审批机制 (--approve)

启动时传入 `--approve` 参数后，写文件 (`write`、`edit`) 及命令执行 (`bash`) 等敏感操作需要显式授权：

- **TTY / 非 TTY / RPC 差异**：普通 CLI/JSON 模式在没有终端 TTY 交互能力时会**直接拒绝**敏感操作；而在 RPC 模式下，系统开启非阻塞的异步审批管道，由引擎向客户端发送 `approval_request`。
- **并发与超时**：待处理审批请求上限为 64 个（超过则立即拒绝新请求）。每个审批请求拥有 120 秒超时（`120000` ms），以单调时钟计算。超时未处理自动转为拒绝，对应的 pending 名额立即释放。
- **拒绝、取消与清理**：
  - 若客户端回复 `approved: false` 或超时，工具调用返回失败报错，模型知晓后可规划回退策略；
  - 任务发生 `abort` 或连接 EOF 时，所有未决审批立即被取消并丢弃，尚在等待批准的工具不会执行；已经执行的文件修改不回滚；
  - 客户端针对已过期、不存在或已判定的 `request_id` 提交响应时，引擎返回 `success: false` 并附带说明。
- **审批范围**：审批不仅限制当前会话的直接工具，也约束被授权写入的子代理。

#### RPC 审批交互示例

引擎发出审批挂起请求：
```json
{"type":"approval_request","request_id":"c88fcfb5-2fd5-41d3-9bc3-066e04746654","operation":"write","target":"src/main.rs","timeout_ms":120000}
```

客户端确认批准（或传入 `"approved": false` 拒绝）：
```json
{"type":"approve_response","request_id":"c88fcfb5-2fd5-41d3-9bc3-066e04746654","approved":true}
```

引擎回执：
```json
{"type":"response","command":"approve_response","id":"c88fcfb5-2fd5-41d3-9bc3-066e04746654","success":true,"data":{"approved":true}}
```

## 模型切换、Thinking 思考等级与子代理

### 模型切换与供应商映射

- **空闲切换**：空闲时可通过 RPC 发送 `{"type":"set_model","id":"m1","model":"deepseek/deepseek-chat"}` 或在交互中使用 `/model <name>`。若只提供模型名，则沿用当前供应商连接；若携带供应商前缀（如 `deepseek/` 或 `antigravity/`），则完整同步对应的端点、凭据及服务配置；若切回之前已配置过的供应商，会自动恢复其先前设置的基地址与参数。
- **凭据解析**：优先读取供应商专用环境变量（如 `DEEPSEEK_API_KEY`），未指定时回退到 `MIO_API_KEY`；绝不跨供应商混用专用密钥。`custom` 供应商可读取 `MIO_API_KEY` 或 `OPENAI_API_KEY`。不支持的原生协议或缺少所需账户时，切换失败并保持原配置不变。
- **思考等级 (Thinking Level)**：
  - 等级为 `off`、`minimal`、`low`、`medium`、`high`、`xhigh`、`max`；CLI 还接受 `none`、`min`、`med` 别名。RPC 使用规范等级名。
  - 通过 CLI `--thinking <level>`、模型简写 `[provider/]model[:thinking]`、交互命令 `/thinking <level>` 设置；CLI 显式参数优先于简写。RPC 示例：`{"type":"set_model","id":"m2","model":"deepseek/deepseek-chat","thinking":"high"}`。
  - 通用 OpenAI 兼容接口与 Codex 原生传递思考参数；具体等级支持取决于模型。已知模型能力会在请求前本地校验，未知模型仍可能被服务端拒绝。无效交互调整报告错误且保留原配置。
  - DeepSeek API 严格仅接受 `off`、`low`、`high`、`max`（传入 `medium` 将被明确拒绝）；DeepSeek 网页适配器暂不支持开启思考。
  - Antigravity 映射至其底层的专有 thinking 级别配置。

### 原生子代理 (Subagents)

- **开启与并发**：使用 `--subagents` 显式启用；并发由 `--max-subagents` 控制（默认 4，范围 1–16），超出上限或工作区忙碌时进入 `Queued` 队列而不是拒绝。单个运行默认超时 600 秒，上限 3600 秒。子代理在内存中维护独立历史（不污染主干会话）。
- **排队与写冲突**：严格 FIFO。整工作区一次只允许一个写任务，只读任务可并行，避免旧 writer 被后续只读任务饿死；同一 session 已有不同的活动任务时明确报错。
- **权限与边界**：委派任务时默认 `read_only: true`（禁用写入、编辑与 Shell 执行）；仅在父任务显式指定且允许时才放开写权限。子代理**永远不会再次获得派生子代理的权限**（无法层叠派生）。
- **继承与同步**：主代理中发生的模型切换、思考等级调整及交互配置变更，均自动同步应用至后续子任务（包括续接已有子会话）。
- **安全与控制**：子代理支持主动状态查询、等待与中途取消。相同 session、父运行 ID（包括均未知）且完全相同的活动请求复用同一 `run_id`；已知的不同父运行不能复用，终态不会被该路径自动重放。主代理取消或失败时清理活动子任务；若父回合结束时还有运行中的子任务，会取消它们并报告错误，不能将未收集结果视为完成。子代理提供 `delegate`、`status`、`snapshot`、`result`、`list`、`kill`、`fork` 操作，状态等待最长 30 秒；子会话随进程退出丢失。`result` 仅在终态返回 `mio.subagent.result/v1`，包含已知的父运行/来源调用 ID、状态、答案、错误、有界工具证据和规范化拒绝记录，并标为 `verification: unverified`；运行中先查 `status`。`snapshot.evidence` 的 `ToolEvidence` 只来自真实 `ToolFinished`；`denied_calls` 只记录权威生命周期中的执行前拒绝，不包含原始参数或拒绝前无法规范化的请求。两者均不是测试判决或回滚保证；工具过滤和审批不构成操作系统沙箱。

## 上下文预算管理与会话持久化

### 预算参数与默认值

- **容量目录与回退**：内置知名模型窗口容量目录。对于未知模型，默认回退容量为 `32768` tokens。可通过 `--context-window <N>`（至少 1024）覆盖部署容量。
- **输出预留 (`--max-output-tokens`)**：默认值为 `4096`、容量的四分之一（向下取整）和目录输出上限三者的最小值，并额外保留 5% 的工程安全余量。DeepSeek 网页适配器因无原生输出上限支持，禁止显式设置 `--max-output-tokens`。

输出上限必须为正数，为输入和 5% 余量留出空间，且不能超过已知目录的输出上限；它同时控制原生请求的输出上限和本地预留。DeepSeek 网页端的默认预留仅参与输入预算。`get_state.data.contextBudget` 返回 `context_window`、`max_output_tokens`、`source`（`catalog` / `override` / `fallback`）。切换模型重新查询目录，显式覆盖保持不变，后续子任务继承相同设置。

### 显式保留约束与未完目标

宿主可用 `RunOptions.pinned_context` 登记固定 ID、类别和原文；CLI 提供 `/pin constraint ID TEXT`、`/pin goal ID TEXT`、`/pin replace constraint|goal ID TEXT`、`/unpin ID` 与 `/pins`，RPC 提供 `set_pinned_context`。条目作为固定宿主指令进入每次请求，计入预算，放不下则拒绝请求；不靠关键词推断约束，也不允许模型修改注册表。会话保存后可恢复，分叉继承，新会话清空。条目最多 64 个，单条正文 8 KiB，正文合计 32 KiB。

压缩产生结构化损失报告，通过宿主回调、JSON 事件和恢复查询观察原历史范围、关联工具调用与受影响类别；报告不复制对话正文。摘要仍是有损摘录，固定原文不代表模型一定遵循，实际任务仍需独立验收。接口和保存语义见 [RPC 文档](docs/rpc.md#显式约束与未完目标)。

### 工具续轮检查与压缩边界

- **逐轮预算判定**：在主输入以及**每一次工具结果返回准备发起下一轮推理前**，均重新严格计算：`系统指令 + 工具定义模式 + 历史上下文 + 活动工具结果 + 输出预留` 的保守 UTF-8 字节占用（本地启发式估算，非分词器）。
- **安全交互边界**：预算超限时，压缩仅在**完整的工具调用/响应成对边界**触发，绝不切断单独半截工具调用；系统提示词与当前轮的工具上下文会被完好保护。
- **预算摘要**：生成的预算摘要上限为 4096 字节，优先抽取近期关键提问与关键工具输出；该预算压缩仅动态作用于当前单次请求载荷，**不会破坏持久化的原始会话完整性**。
- **历史压缩区别**：`/compact`、RPC `compact` 和自动会话压缩会更新会话历史。CLI 默认开启自动压缩，阈值取本次输入预算的 3/4 与 512 KiB 中较小者，`--no-auto-compact` 可关闭（`--auto-compact` 保留为兼容写法）。压缩采用本地抽取摘要，可能丢失细节。每请求预算检查始终生效，与会话压缩开关无关。当前预算估算仅支持文本上下文，媒体消息会明确拒绝；模型目录容量可能与实际部署不同，应按部署设置覆盖值。
- **超限阻断**：若即便压缩后仍无法容纳当前必须发送的内容，引擎阻止当前请求发往服务端，立即发出清晰的 `Context budget:` 错误，并在 RPC 中以 `agent_settled` 的 `error` 状态结束；后续输入仍可继续。当前超限请求不会发出，但先前已完成的请求仍可能计费。

### 会话持久化与失败语义

- **原子更新**：会话日志持久化写入 `.mio/sessions/`，使用临时文件配合原子替换，降低写入中断导致半份文件的风险；这不是任意硬件故障下的数据恢复保证。成功落盘后才更新内存中的当前会话。
- **失败安全**：RPC 模式下的历史压缩 (`compact`)、新建 (`new_session`)、分叉 (`fork_session`) 与重命名 (`set_session_name`) 均以**文件落盘成功作为返回 success 的前置条件**。若磁盘已满或写入失败，内存中完整保留原有会话状态并返回详细错误，调用方可重试会话操作；模型与工具执行中已发生的外部副作用不回滚。传入 `--no-session` 时完全禁用磁盘落盘，仅在内存中留存。

```bash
mio --list-sessions
mio --session '会话路径或唯一UUID前缀或名称' -p '继续任务'
mio --resume --fork --name '备选方案' -p '探索另一种实现'
mio --resume --export session.html
mio --no-session -p '本次不保存会话'
```

## 扩展与验证边界

项目规则支持祖先与当前目录的 `AGENTS.md`、`CLAUDE.md`，可用 `--no-context-files` 关闭；`--skill NAME` 加载项目 `.mio/skills/` 中的技能，`--template NAME` 使用 `.mio/prompts/NAME.md` 并替换 `$ARGUMENTS`。`--no-context` 关闭项目上下文，细分开关还有 `--no-skills` 和 `--no-prompt-templates`。

MCP 配置位于项目 `.mio/mcp.json` 与用户配置目录 `mcp.json`，通过 stdio JSON-RPC 2.0 连接；每个服务器可用 `required` / `optional` 声明是否阻断启动，项目配置的启动授权与持久信任见[项目 MCP 授权](#项目-mcp-授权与执行证据恢复)。`--no-mcp` 关闭发现，`--exclude-tools 'mcp__*'` 可过滤工具。插件使用 Mio 原生 `mio-plugin.json`，可通过 `--plugin-init`、`--plugin-validate`、`--plugin-install`、`--plugin-list` 和 `--plugin-remove` 管理。它不是 Pi 的 TypeScript 扩展运行时，尚未对齐 Pi 的完整 TUI、交互会话树与扩展生态。

内置供应商目录不代表各家的专有协议均已适配。当前验收集中于 Linux 和本地模拟服务，未声明其他平台、预编译分发包或所有真实账号服务均已通过验收。

## 开发与测试

在提交前，严格通过以下四项验收检查：

CI 固定使用 Rust 1.95.0；本地验收请使用同一版本，避免不同 Clippy 版本造成结果差异：

```bash
cargo +1.95.0 fmt --check
cargo +1.95.0 check --all-targets --locked
cargo +1.95.0 test --locked
cargo +1.95.0 clippy --all-targets --locked -- -D warnings
```

测试集涵盖本地 HTTP 模拟、CLI 读写与恢复、边界安全和会话持久化，无需调用外部真实服务。

### SDK 离线宿主示例

完整可离线运行的宿主示例是 [examples/host_loopback.rs](examples/host_loopback.rs)（分层契约与 feature 矩阵见 [SDK 契约](docs/sdk.md)）。它不读取任何环境凭据，只用 `127.0.0.1` 回环 mock，展示真实 `TaskHandle`、参数绑定审批、工具生命周期 journal 与三种场景：

```bash
cargo run --example host_loopback -- --scenario success
cargo run --example host_loopback -- --scenario model-error
cargo run --example host_loopback -- --scenario cancel
```

`tests/host_example_contract.rs` 的 4 项契约测试及上述三个场景已在主树通过；Linux SDK feature 矩阵 27/27 通过。macOS / Windows 尚无实际构建结果。当前本机验收与历史模型失败分别见[本轮记录](docs/reviews/2026-10-17-v1-inline8k-acceptance.md)和[前轮记录](docs/reviews/2026-10-17-v1-live-acceptance.md)。

Rust 宿主可通过默认关闭的 `Host::output_schema()` 指定最终 JSON Schema，CLI 一次性运行与 `--mode rpc` 也可通过 `--output-schema PATH` 读取有界 schema 文件；32 KiB 上限与本地真实回环证据见[接入记录](docs/reviews/2026-10-17-p2-output-schema-sdk.md)。它仍是输出格式约束而非内建审查模式，结果内容仍须独立验收。

### 评测入口

`evals/` 存放自有验收探针（不是公共 benchmark）；任务集版本、固定模型/预算口径与指标限制见 [evals/README.md](evals/README.md)：

```bash
python3 -B tests/agent_eval_harness.py                                  # 离线自测评测器，不调用真实模型
python3 scripts/agent_eval.py --help                                    # 查看参数
python3 scripts/agent_eval.py --config evals/configs/mio-local.json     # 报告写入 target/evals/mio.json
```

CLI 显式参数覆盖 `--config` 的同名键；报告统一为版本化 JSON，未获取到的 token/成本指标明确标为 `null`/`unknown`，不代表已支持计费或用量统计。

## 项目架构与许可证

项目采用三层分层架构：`mio-core`（模型与流式协议）、`mio-agent`（Agent 循环与工具调用）与 `mio-coding-agent`（原 `mio-agent` 重命名，提供编程工具、会话与 CLI）。

项目采用 [MIT 许可证](LICENSE) 发布。依赖许可证与源码来源记录见 [CREDITS.md](CREDITS.md) 和 [licenses/](licenses/)。
