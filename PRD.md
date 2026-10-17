# Mio 产品需求与架构设计文档 (PRD)

> **定位**：以**共享调度与可控多代理协作**为核心的无头 coding agent 引擎（Rust 实现，同核驱动宿主库进程内嵌入与 `mio` CLI / JSONL RPC 守护进程）。  
> **配套文档**：[v1 设计规格](docs/design-v1.md) · [README.md](README.md) · [TODO.md](TODO.md) · [需求与竞品调查](docs/agent-requirements-and-comparison.md) · [审查记录](docs/reviews/2026-10-15-audit.md) · [RPC 协议](docs/rpc.md) · [评测探针](evals/README.md)

---

当前执行状态（2026-10-17）：`DES-01`—`DES-22` 设计契约已确定，核心实现已整合。本机 Linux 冻结版 `f7873aaf…` 已通过四项 Rust 门禁（全量测试 3242 passed / 0 failed / 6 既有 ignored）、SDK 27/27、严格 Linux 42 项、SIGKILL 12 项、检查点崩溃 5 项、Ravomi 解析器 7/7，以及固定模型门槛。二进制及 1139 项受检源码、配置、任务和测试文件在验收前后摘要一致。历史 R15 和 `3c196d26…` 失败批次单独保留。

**v1 本机 Linux 固定门槛已通过，尚未提交或发布**：冻结版 `f7873aaf…` 沿用 `deepseek-chat`、thinking off、每任务 20 calls / 180s，六任务 **6/6**、独立长文 **3/3**，每次 Agent 与独立验收均退出 0。唯一产品源码改动是把长工具输出外置阈值由 32 KiB 降至 8 KiB；预算、任务和验收未变。小样本不证明稳定提升，跨平台与完整 GUI 宿主仍待验。执行状态见 [TODO](TODO.md)，证据见 [本轮验收记录](docs/reviews/2026-10-17-v1-inline8k-acceptance.md)；[前轮失败](docs/reviews/2026-10-17-v1-live-acceptance.md)及[历史主树](docs/reviews/2026-10-16-v1-acceptance.md)保留。

## 一、产品定位与核心目标

### 1. 解决的核心痛点

多代理与高频自动化编程场景下，传统单会话 Agent 容易失控。Mio 在引擎真实 dispatch 边界统一收敛以下七项核心指标：

| 核心痛点 | 目标机制 | 验收指标（关联需求） |
| :--- | :--- | :--- |
| **429 限流引发请求风暴** | 进程内 + 跨进程模型槽位准入，命中 429 自动解析 `Retry-After` 发布冷却 | 并发任务请求计数不放大，恢复后正常继续（`REQ-SCHEDULE`） |
| **多代理各自重试互不知情** | 按 `endpoint + 凭据摘要` 共享冷却窗口 | 冷却对父代理、子代理、RPC 与协作进程一致可见，不自动重放副作用（`REQ-SCHEDULE`） |
| **并行写覆盖与死锁** | 目录前缀写冲突排队 + 严格 FIFO 读写调度 + 编辑前原文件身份校验 | 同目录写不交错覆盖、旧 writer 不被后续只读任务饿死、无关目录并行（`REQ-CONFLICT`） |
| **编译/浏览器重任务争抢资源** | 独立 `heavy` 并发配额（本地 + 跨进程 `flock`） | 重任务受上限约束，轻任务（只读检索/规划）不被挤占，取消后回收进程组（`REQ-QUOTA`） |
| **Token 预算失控与假账单** | 调用前保守预留、调用后按 Provider 真实 usage 结算 | 缺 usage 显式标 `None`/`unknown`，绝不用字节估算冒充账单；超额阻断后续调用（`REQ-BUDGET`） |
| **重复探索与重复计费** | 显式 `dedup_key` 准入去重 + 子代理活动请求 `RequestKey` 复用 | 相同 key 并发提交只执行一次，返回已有任务标识（`REQ-DEDUP`） |
| **中断后状态不明与盲目重放** | 副作用前落盘 `.mio/journal` 意图 + 子代理真实 `ToolEvidence` | 成功/失败/取消/崩溃均保留可查证据，重启标记 `interrupted`，绝不自动重放（`REQ-ACCEPT` / `REQ-RECOVERY`） |

### 命令行产品约定

`mio` 采用与 pi 相近的终端使用方式：直接对话、`-p` 单次执行、管道输入及 JSON/RPC。交互模式使用 inline TUI：底部固定输入框、动态状态行与 footer，定稿内容写入终端滚动历史；不进入 alt-screen，不主动清屏或擦除滚动历史，不增加独立 GUI。多代理以字母代号（`@A`、`@B`…）和颜色区分，最多支持 32 个代理协作，设计见 [docs/tui-interaction-redesign.md](docs/tui-interaction-redesign.md)。引擎仍支持宿主进程内嵌入；外围产品需求单独验证。

首要入口已由维护者明确为**独立终端 CLI 与 Ravomi 内嵌宿主**，两者共享同一引擎与执行契约。第三方 SDK 保持可集成，不单独主导近期产品路线。

### 2. 诚实边界（Non-Goals / 当前未宣称项）

- **默认不启用 OS 沙箱**：调度器、路径租约、工具过滤与 `--approve` 均为应用层协作准入。阶段三已接入可选 Linux bubblewrap，并有本机子进程隔离证据；扩展拒绝契约与整合验收仍在推进，macOS / Windows 未提供 OS 沙箱。每进程资源上限不等于进程树总配额。
- **非美元精确账单硬限额**：`--max-model-calls` 是调用次数硬上限；`--max-total-tokens` 采用调用前保守预留 + 调用后按 Provider 真实 usage 结算，实际用量可能超过预留（超额记录 overage 并阻断后续请求）。它是 token 计数与保守预算，不是精确费用或绝对 token 硬限额，也不做价格换算。跨进程预算已接入持久账本，崩溃遗留预留不自动退款；硬件断电与跨平台验收仍待执行。
- **不自动回滚已写磁盘文件**：取消（Ctrl-C / `abort` / 超时）立即停止后续推理并向子进程组发送 `SIGKILL`，保留上次成功落盘的会话历史，但不回滚已落盘的文件修改。
- **离线契约不代替线上验证**：本地 mock / 多进程测试通过不代表真实商业 Provider 线上认证、跨平台环境或极端硬件断电已验证。

### 3. 产品目标与当前能力状态（评测支线，2026-10-15 iteration-02）

- **产品目标**：用「同模型、同任务集、同预算」的自有验收任务集，把「引擎改进是否真实」变成可复查证据；当前以长文检索任务 `retrieve-wire-spec` 作为切入点，先测量再改进。
- **当前能力状态**：最新冻结版 `f7873aaf…` 在固定预算（`deepseek-chat-off-20calls-180s-isolated-v2`）下六任务 6/6、独立长文 3/3；这是本机小样本验收，完整结果见[本轮记录](docs/reviews/2026-10-17-v1-inline8k-acceptance.md)。第五节保留 iteration-02 的 5/6 历史基线。真实 usage/cache 计数、模型/工具 dispatch-to-outcome 耗时与诊断类别已可采集。
- **评测支线非目标**：不按竞品功能清单照搬产品形态；不把本地微优化或单次小样本当作稳定性/领先证据；不通过加预算、放宽验收或为特定任务特判来「提升」分数；不声称公共 benchmark（Harbor 适配仍未接入）排名。
- **成功判定与边界**：任务「通过」以 Agent 退出码 0 **且**独立验收脚本退出码 0 为准，两者缺一不可；一次运行、少量样本不构成稳定性结论。

---

## 二、整体架构设计

### 1. 纵向三层架构

工作区遵循严格单向依赖：`mio-coding-agent → mio-agent → mio-core`（外加传输与过程宏支撑包 `mio-http`、`mio-reqwest`、`mio-tungstenite`、`mio-derive`）。底层不依赖终端 UI、代码工作区或产品会话策略。

```
+-----------------------------------------------------------------------------------+
|                              宿主与入口适配 (Hosts)                                |
|   CLI (text + OSC 7501 / --json)  |  Stdio JSONL RPC (--mode rpc)  |  Rust 宿主库  |
+-----------------------------------------------------------------------------------+
                                          |  Host trait / RunOptions / RunTools
                                          v
+-----------------------------------------------------------------------------------+
|                      mio-coding-agent (编程引擎与四大控制平面)                     |
|  +-----------------------+  +-----------------------+  +-----------------------+  |
|  | 1. 调度与配额平面     |  | 2. 工具与子代理平面   |  | 3. 上下文与恢复平面   |  |
|  | - SchedulingHook      |  | - Workspace (读写租约)|  | - ContextBudget       |  |
|  | - Coordinator (进程内)|  | - Shell (进程组监督)  |  | - Session (原子落盘)  |  |
|  | - SharedLimits (flock)|  | - SubagentManager     |  | - Journal (执行证据)  |  |
|  +-----------------------+  +-----------------------+  +-----------------------+  |
|  +-----------------------------------------------------------------------------+  |
|  | 4. 外部扩展与协议桥接平面：MCP (指纹授权/分页) · Plugin (进程监督/deny优先) |  |
|  +-----------------------------------------------------------------------------+  |
+-----------------------------------------------------------------------------------+
                                          |  AgentBuilder / AgentHook / DynamicTool
                                          v
+-----------------------------------------------------------------------------------+
|                        mio-agent (无 I/O 状态机与通用运行时)                       |
|    AgentRun (可序列化纯状态机) · prepare_request · Effect Bus (DispatchScope)     |
+-----------------------------------------------------------------------------------+
                                          |  DynModel<Completion> / PortableTool
                                          v
+-----------------------------------------------------------------------------------+
|                         mio-core (模型契约、流式协议与观测)                        |
|  Catalog · OpenAI/Codex/DeepSeek · Anthropic · Gemini · Ollama · Observe/Secret   |
+-----------------------------------------------------------------------------------+
```

### 2. 横向四大控制平面（`mio-coding-agent`）

1. **调度与配额平面（`src/scheduling.rs`、`src/coordinator.rs`、`src/shared_limits.rs`）**
   - **拦截边界**：通过 `SchedulingHook` 挂载到 `mio-agent` 的 `CompletionDispatch` 与 `ToolDispatch` 事件，在真实请求发出或重工具执行前申请租约。
   - **双层配额**：进程内 `Coordinator` 管理本地队列、目录写冲突、显式 `dedup_key` 与 Token/调用次数预算；跨进程 `SharedLimits` 通过 Unix `flock` 与版本化不可变 `policy.json` 协调全局模型并发（默认 2）、单 Provider 并发（默认 2）、重任务并发（默认 2）与工作区路径租约。
   - **无死锁等待**：任务队列、模型槽位与重任务槽位彼此独立；排队等待期间不持有其他槽位，进程崩溃时内核自动释放 `flock`。

2. **工具与子代理执行平面（`src/workspace.rs`、`src/shell.rs`、`src/subagent.rs`、`src/delegation.rs`）**
   - **工作区安全边界**：规范化路径校验、逐组件拒绝符号链接越界；`edit` 在原子替换前校验原文件身份（`dev/inode`）与原始内容（`ensure_edit_source_unchanged`）；`spawn_leased` 将租约移入阻塞写线程，确保外层 Future 取消后直至底层 I/O 结束才释放工作区。
   - **统一进程监督**：Shell 与插件命令共用 `shell::run_supervised`（Unix 独立进程组、`kill_on_drop`、单流超过 8 KiB 后外置完整输出、仅保留约 2 KiB 预览，8 MiB 文件上限与超时强杀）。
   - **原生子代理（`SubagentManager`）**：内存独立会话，状态机 `Queued → Running → Completed/Failed/Cancelled/TimedOut`；严格 FIFO 排队（整工作区写独占、只读并行、防写饥饿），默认 `read_only: true`，禁止子代理再次派生子代理；采集来自真实 `ToolFinished` 的有界 `ToolEvidence`（含 FNV-1a 结果哈希）。

3. **上下文与持久化恢复平面（`src/context.rs`、`src/context_budget.rs`、`src/compact.rs`、`src/session.rs`、`src/journal.rs`）**
   - **双层上下文控制**：`BudgetHook` 在每轮推理（含工具续轮）前按 `指令 + 工具定义 + 历史 + 活动工具结果 + 输出预留` 计算保守字节预算，超限时仅在完整工具调用/响应边界生成单次请求摘要，不破坏持久化历史；`compact` 提供会话级历史压缩。
   - **原子会话与执行证据**：`.mio/sessions/` 采用临时文件原子替换，支持精确 UUID > 唯一名称 > UUID 前缀 > 名称子串分级消歧与 `mtime` 降序扫描；`.mio/journal/` 在副作用工具审批/执行前持久化 `intent`，记录 `executed` / `unknown` / `interrupted`，`--recovery` 可随时审计中断现场。

4. **宿主与扩展交互平面（`src/lib.rs`、`src/rpc.rs`、`src/mcp.rs`、`src/plugin.rs`、`src/program_status.rs`）**
   - **`Host` 契约**：统一抽象 `shell`、`approve_tool_async`、`on_event`、`poll_steering`、`on_completion_call`（真实上报 usage）、`on_execution_timing`（执行观察跨度）与 `on_diagnostic`（脱敏分类诊断，绝不泄露响应体、URL 或密钥）。
   - **RPC 控制流**：支持空闲启动、运行中 `steer` 安全点注入、`follow_up` 队列顺延、`abort` 即时取消与 120s 异步审批管道，以 `agent_settled` 作为工作链静止信号。
   - **受控扩展**：项目 `.mio/mcp.json` 在工具发现与过滤前强制执行服务器指纹授权（fail-closed），支持 `tools/list` 分页与名称冲突检测；插件与 MCP 全部纳入 `ToolFilter`（别名 deny 优先）与 1 MiB 协议帧硬限额。

---

## v1 设计与实现边界

实施契约见 [v1 设计规格](docs/design-v1.md)，以 `DES-01` 至 `DES-22` 记录范围、状态机、授权、预算、上下文、事件、输出与 SDK 决策。设计决策已收敛，实施状态仍逐项验收；历史评测和四阶段路线保留，不因设计成文而勾选实现完成。

本轮已接入协议发现/协商、v1 封套、上下文摘要保真、显式 Pinned、损失审计与恢复查询；真实运行关联 ID、保守 owner 存活观测、生产输出索引和显式清理已进入整合验收。实现存在不等于完成：审批拒绝不能误报已派发，未知副作用不得自动重放，清理必须在独占租约下核验完整引用。统一状态转移、结构化输出的真实调用关联、故障注入、SDK/宿主一致性与同预算模型评测仍按 DES-22 逐项验证。现有 backend 模块不代表所有执行入口已沙箱化。

历史固定候选 `60bc67318a215e198fee90dcb6c4cdd19a578db1da872ac2daa06aaa9c61673b` 已通过严格 Linux backend 39 项及真实 SIGKILL 恢复 15 项定向验收。但原预算 DeepSeek 六任务仍为 **5/6**，长文独立三次为 **1/3**，未达到 6/6 与至少 2/3 的既定门槛；所有失败保留在 `target/evals/design-implementation-20261015/live-candidate/`。该候选与第五节 iteration-02、2026-10-16 主树冻结版本分别记录，不相互覆盖。当时主树版本 `d1a58c47…` 的同预算六任务为 5/6，长文三次为 0/3；本机新验收见文首，不据小样本宣称性能稳定提升。

## 三、分阶段演进路线与待办清单

状态口径：**已完成**（实现落地且通过本地/离线自动化测试）、**部分完成**（主路径已落地，存在跨进程/线上/极端故障残余）、**未完成**（待设计或待端到端验证）。

### 阶段一：核心防线、共享调度与原生路由（已完成离线验收，残余转入后续阶段）

> **阶段目标**：解决 429 风暴、并行写覆盖、重任务争抢、协议错配、进程残留与无界内存分配，建立可验证的离线防线。

- [x] **共享调度与配额闭环（`REQ-SCHEDULE` / `REQ-CONFLICT` / `REQ-QUOTA` / `REQ-BUDGET` / `REQ-DEDUP`）**
  - 已完成 `Coordinator` + `SharedLimits` + `SchedulingHook` 集成，覆盖模型/Provider 槽位、429 `Retry-After` 冷却、重任务限流、Token 预留与真实结算、子代理 FIFO 排队及 `Workspace` 跨进程路径租约注入（阶段一历史快照：`tests/scheduling.rs` 9 项、`tests/coordinator.rs` 25 项、`tests/shared_limits.rs` 33 项）。
- [x] **执行控制与资源边界加固（`AUD-01/02/03/05/06/07/08/09/12/19`）**
  - 已完成插件进程组监督（`AUD-01`）、精确插件名与别名 deny 优先过滤（`AUD-02`）、项目 MCP 启动前指纹授权（`AUD-03`）、1 MiB 协议帧上限与有界输出（`AUD-05`；当前 8 KiB 外置阈值、8 MiB 完整文件上限）、线性 NFA 正则（`AUD-06`）、插件符号链接逐组件校验（`AUD-07`）、MCP 分页与冲突报错（`AUD-08/09`）、取消后阻塞写租约保持（`AUD-12`）、`edit` 原文与 inode 校验（`AUD-19`）。
- [x] **原生协议路由、会话消歧与执行证据（`AUD-04/10/11/13/15` / `REQ-ACCEPT`）**
  - 已完成 Anthropic/Google/Ollama 原生路由（`tests/provider_routes.rs`）、会话分级消歧与 `mtime` 扫描（`AUD-10/11`）、`.mio/journal` 执行证据（`AUD-13`）、脱敏 `CompletionCallObservation` / `RunDiagnostic`（`AUD-15`、`tests/diagnostics.rs`）与自有验收探针（`evals/`）。
- [x] **阶段一基线质量门禁**
  - 阶段一历史验收快照：Rust 1.95.0 四项门禁（`fmt` / `check` / `test` 2359 passed / `clippy`）、`scripts/check-site.py`（6 页面、66 CLI 参数）与 `tests/agent_eval_harness.py`（17 项）全部通过。

---

### 阶段二：恢复闭环、连接韧性、事件版本化与上下文保真（近期重点，P1/P2）

> **阶段目标**：将阶段一的防线从“单模块/离线可工作”推进到“全入口无死角、崩溃可恢复、线上真实验证、长程上下文不丢约束”。

| 优先级 | 待办项（关联编号） | 核心设计任务 | 验收标准 |
| :---: | :--- | :--- | :--- |
| **P1** | **全入口统一工具门禁**<br>(`REQ-POLICY` / `REQ-PROCESS`) | 将内置工具、插件、MCP、`extra_tools` 与子代理收敛至单一准入网关；审批绑定规范化参数与上下文；嵌套委派权限单向收窄 | 任一入口无法绕过过滤或审批；拒绝或超时路径零副作用；参数篡改或重放被拒绝 |
| **P1** | **跨进程预算持久化**<br>(`REQ-BUDGET` 深化) | 在 `SharedLimits` 共享目录中持久化版本化预算账本，支持多进程原子预留与真实 usage 结算 | 多进程共享 root 与重启后 `--max-total-tokens` / `--max-model-calls` 一致生效，损坏时 fail-closed |
| **P1** | **真实 Provider 联调与并发压力**<br>(`REQ-PROVIDER` / `AUD-18`) | 统一 CLI 预设、底层 `Catalog` 与文档站能力来源，区分“有凭据 / 端点可达 / 协议支持”；在授权环境下做真实 Provider 联调 | 真实端点认证头、thinking 映射、usage、429 冷却与错误分类实测通过；持续并发压力下无泄漏 |
| **P1** | **CLI/RPC 崩溃恢复与文件检查点**<br>(`REQ-RECOVERY` / `AUD-13` 深化) | 对请求前、执行中、结果返回后、保存前四时点做真实中断注入；统一 journal 与 session 恢复视图；为 `write`/`edit` 提供可选 pre-edit 快照与 rewind | 四时点中断后证据完整、不重复执行副作用；rewind 检测并保护用户后续手工修改，不触碰 Git 历史 |
| **P2** | **CLI/RPC 显式任务去重与竞争压力**<br>(`REQ-DEDUP` / `REQ-CONFLICT` 深化) | **任务 key 已完成本地离线验收**：CLI `--dedup-key` / RPC `dedup_key` 共用工作区持久化账本，活动/排队/终态不重放并返回原 `task_id`；4 进程竞争仅一次 loopback 模型请求（证据见 TODO）。**竞争压力待验收**：构造并发写冲突与 `ensure_edit_source_unchanged` 到 persist 之间的竞态故障注入 | CLI/RPC 重复提交同一 key 返回已有任务标识；高并发写冲突无交错覆盖，必要时引入基于目录句柄的原子操作消除 TOCTOU |
| **P2** | **MCP 生命周期与大输出外置**<br>(`REQ-EXTENSIONS` / `AUD-05/16`) | 持久化项目 MCP trust store；划分 required/optional 服务，支持连接复用与断线重连；当前超 8 KiB 的工具完整输出外置为文件引用 | 单一可选 MCP 失败不阻断整体运行；长会话复用连接；模型可按引用按需分段读取超长输出 |
| **P2** | **事件协议版本化与上下文保真**<br>(`REQ-EVENTS` / `REQ-CONTEXT` / `AUD-14/15`) | 版本化 RPC/Host 事件 schema 与能力协商，贯通 `session/run/turn/tool-call/subagent` ID；改进压缩摘要以保留早期用户约束、未完目标与失败证据；稳定前缀缓存 | 一次失败可从用户错误追踪至具体请求与工具调用；慢消费者背压测试通过；长程压缩后约束保持率与缓存命中率可测量 |

---

### 阶段三：同条件评测、大仓库检索、真实沙箱与 SDK 拆分（中期演进，P2）

> **阶段目标**：用客观评测数据指导大仓库检索选型，补齐操作系统级安全沙箱，并完成 CLI 模块化拆分与 Rust SDK 稳定发布闭环。

| 优先级 | 待办项（关联编号） | 核心设计任务 | 验收标准 |
| :---: | :--- | :--- | :--- |
| **P2** | **同模型同预算对照评测**<br>(`REQ-EVAL` / `REQ-EVAL-BASELINE`) | 基线已建立（固定 SHA、模型、预算与 6 任务，见第五节）；剩余工作是在同预算下做长文任务的重复运行、独立离线保真用例与失败样本沉淀，再考虑跨 Agent 对照 | 产出成功率、正确性、成本、延迟、缓存、取消清理与恢复报告，保留失败样本与原始配置；不同任务集/预算/模型的结果不混用 |
| **P2** | **大仓库代码检索与长文检索**<br>(`REQ-CONTEXT` / `REQ-CONTEXT-RETRIEVAL` / `REQ-CONTEXT-FIDELITY` / `REQ-BUDGET-AWARENESS`) | 以 `retrieve-wire-spec` 为当前切入点：先复现并量化重复 `read`、约束覆盖与预算规划，再决定检索/压缩选型；repo map 与符号索引对比仍待评测基线上线 | 与基线对照重复 `read` 次数/请求次数下降且正确性保持；离线约束/工具调用配对/失败证据保真用例通过；长文任务同预算 ≥2/3 通过 |
| **P2** | **可插拔 OS 沙箱与修改隔离**<br>(`REQ-ISOLATE`) | 定义可替换执行环境 Backend（Linux bubblewrap/landlock、macOS seatbelt 等），将安全沙箱与 Git worktree 代码修改隔离解耦 | Backend 状态可查询，无 Backend 时行为显式；越界读写、网络与资源超限按平台阻断并验证 |
| **P2** | **CLI 拆分、SDK 契约与多平台分发**<br>(`REQ-SDK` / `REQ-DISTRIBUTION` / `AUD-17/20`) | 参数、终端、RPC、审批、Runner 五个 CLI 模块已拆出；后续稳定三层公共 API 与依赖方向检查；完善 helper crate feature/平台矩阵与 crates.io 发布策略 | 最小与完整宿主示例编译通过；Linux/macOS/Windows 安装升级、哈希校验与依赖许可证闭包可核对 |

#### 评测支线需求子编号（挂接既有 `REQ-*`，不新增顶层编号）

| 子编号（父需求） | 目标 | 当前状态 | 可验收结果 |
| :--- | :--- | :--- | :--- |
| `REQ-EVAL-BASELINE`（`REQ-EVAL` / `REQ-ACCEPT`） | 固定模型、预算、任务集、二进制 SHA 与失败样本的可复现基线 | 已建立（2026-10-15 iteration-02，见第五节）；长文任务仍失败 | 同预算同任务集可复现；失败报告与原始诊断保留；持久结论写入文档，不能只存被忽略的 `target/` |
| `REQ-CONTEXT-RETRIEVAL`（`REQ-CONTEXT`） | 长文件定位与约束应用，减少重复 `read` 与无效轮次 | 本机固定版已通过长文 3/3；检索方案对比仍未完成 | 与基线对照重复 `read` 次数/请求次数下降且正确性保持；长文任务至少 3 次独立同预算运行、≥2/3 通过，并保留首次失败 |
| `REQ-CONTEXT-FIDELITY`（`REQ-CONTEXT` / `REQ-EVENTS`） | 分页/压缩后保留早期约束、未完目标与失败证据 | 部分基础已有：会话摘要已有开头约束/损失提示测试；请求级摘要与工具结果中后段保真仍缺独立验证 | 离线约束、工具调用配对与失败证据保真用例通过；压缩前后约束覆盖/丢失可测量、可报告 |
| `REQ-BUDGET-AWARENESS`（`REQ-BUDGET`） | 在 20 次调用/180s 内规划检索与调用，而非无界重读 | 未完成：长文任务耗尽全部 20 次调用 | 改进不得靠加预算或改验收；同预算下完成或显著减少无效调用，并有对照证据 |

候选原因（**均待证实，不得写成已确定**）：重复读取同一长文件、约束在分页/摘要后丢失、预算与调用规划不足、模型目录 fallback 影响行为。最终快照只确认 20 次调用耗尽且独立验收失败；较早辅助快照另有同一长文件 16 次 `read` 的记录，两者不能混作同一轮证据，也不足以归因。

---

### 阶段四：宿主生态与产品演进（远期规划，P3）

> **阶段目标**：聚焦验证过的核心用户场景，保持引擎内核精简，按需拓展外围产品形态。

- [ ] **首要宿主聚焦与外围形态演进（`REQ-PRODUCT`）**
  - 维护者已选择独立终端 CLI 与 Ravomi 内嵌宿主为共同首要入口；后续以这两类真实任务验证体验与集成契约，第三方 SDK 反馈作为补充。
  - ACP（Agent Communication Protocol）/ HTTP Server、交互式会话树、显式 Plan 模式、长期后台守护作业均须具备真实需求证据后，作为独立可回退模块迭代，不污染 `mio-core` 与 `mio-agent` 内核。

---

## oh-my-pi 参考：工具与协作体验（2026-10-17）

参考 [oh-my-pi README](https://github.com/can1357/oh-my-pi/blob/main/README.md)、[LSP 配置](https://github.com/can1357/oh-my-pi/blob/main/docs/lsp-config.md)、[Agent Hub](https://github.com/can1357/oh-my-pi/blob/main/docs/agent-hub.md) 与 [记忆机制](https://github.com/can1357/oh-my-pi/blob/main/docs/memory.md)。下列是其公开文档描述的产品优点和 Mio 的候选需求；没有对 oh-my-pi 做独立性能复测。其 README 中的编辑成功率、节省 token 等数字属于上游自述，不能作为 Mio 的收益承诺。

| 可借鉴的优点 | 对 Mio 的价值与现状 | 候选方向及验收口径 |
| --- | --- | --- |
| `read` 返回有定位信息的片段，代码检索可结合 LSP 符号、引用和诊断；重命名可让语言服务器更新引用 | 减少大文件整页读取及重复搜索，并降低跨文件重命名遗漏；Mio 已有 `grep` 邻近行、`read` 分页和输出引用，本机长文固定批次已通过，稳定性尚未证实 | **P2**：先用固定任务对照片段定位、符号/引用/诊断检索与现有工具；另用跨文件重命名验收引用、导出和别名更新。记录正确性、重复读取、调用数和耗时，再决定是否接入可选 LSP；服务不可用时明确降级，写入仍经过 Mio 授权与工作区租约 |
| 按内容锚点拒绝陈旧编辑，并支持结构化改写预览 | Mio 已有唯一匹配、原文与文件身份校验及可选检查点；`read.include_sha256` / `edit.expected_sha256` 提供整文件哈希条件，行分页及长单行字节分页均可取哈希；`edit.preview` 返回有界结构化 diff 与前后哈希，不写入或创建检查点 | **P2**：正式编辑可用 `expected_sha256` 与 `expected_after_sha256` 绑定预览的原文件和完整结果，失配在写入前拒绝；继续用同模型同任务对照编辑成功率、重试次数与 token。预览仍按 edit 授权和写租约准入，不把乐观预检称为原子 CAS |
| 子代理隔离工作区、返回结构化结果，并可查看进度与干预 | 对并行探索、结果整合和故障定位有帮助；Mio 已有 FIFO 子代理、共享配额、工作区写冲突控制与结构化事件。已提供只在终态可取的 `mio.subagent.result/v1`，保留父运行/来源调用身份，并明确结果未经独立验收；相同活动请求仅在父运行 ID 一致（包括均未知）时复用。失败/取消后的已完成工具证据与 CLI 权限继承有回归；规范化后的执行前拒绝另记 `denied_calls`，不伪造 `ToolFinished` | **P2**：继续验证跨平台权限继承，再评估隔离工作树；隔离合并必须保留冲突证据、授权边界与取消后的真实 worker 状态；CLI/RPC 共享同一状态，TUI 只做呈现 |
| 统一资源读取入口可读取本地文件、网页、PR 与技能，按需发现工具 | 模型面对更少不同参数形状，可保留来源与引用；Mio 已有文件工具、MCP/插件和输出引用。技能索引已限量读取前部，完整内容仅在选择时读取；Unix 上技能、工作区上下文、项目 MCP 配置及插件 manifest/MCP 配置的最终读取使用固定目录句柄，插件技能目录校验使用句柄枚举和条目上限。祖先规则与全局 MCP 配置在解析符号链接目标后固定打开并拒绝非普通文件 | **P2**：继续评估 URL/PR 等资源适配的来源、权限、大小预算与不可信内容边界；外部读取失败不得伪装成本地文件成功。非 Unix 与外部符号链接目标的信任策略仍需独立验收 |
| 针对模型调整提示词与编辑格式，而非给所有模型同一套交互 | oh-my-pi 公布了编辑格式的模型对照结果；这提示 Mio 应测量模型与工具协议的组合效果，不能直接移植其收益数字 | **P2**：固定模型版本、任务、采样参数与预算，对比现有提示/编辑格式和候选格式的任务通过率、编辑重试、输出 token 与耗时；仅在独立验收无回退且收益可复查时启用可回退的按模型配置 |
| 规则可按条件触发并在压缩后继续约束后续回合 | 可减少每轮固定上下文，但触发、重试和规则优先级会改变执行语义；Mio 已有项目指令加载与摘要保真机制 | **P3**：先用离线故障注入验证触发准确率、最多重试次数、额外模型调用和压缩后规则保留；规则不得放宽宿主/项目授权，也不得自动重放已有副作用 |
| 代码审查给出逐项严重程度、置信度、位置和明确结论 | 可让 CLI 与 Ravomi 宿主优先处理阻断问题；Mio Rust 宿主（`Host::output_schema`）及 CLI/RPC（`--output-schema PATH`）已可给最终答复指定有界 JSON Schema，但尚无内建审查结果契约。`mio-review-v1` 五任务 DeepSeek 两次独立批次各 3/5；首次修正规则后对原产物重判 4/5，第二次仍有非新缺陷误报及空白行定位，见[五任务记录](docs/reviews/2026-10-17-p2-review-five.md) | **P2**：先在固定 diff 上改善误报和位置准确性，再做跨模型与真实项目 diff 对照，评估可选结构化审查入口；结论须绑定实际差异与检查证据，不以子代理自述作为验收 |
| 独立模型角色、项目记忆、调试器与 ACP 等可选能力 | 可改善长任务与编辑器集成，但带来额外模型费用、持久数据和宿主复杂度 | **P3 研究项**：逐项以真实任务和成本证据决定；不将第二模型、自动记忆、DAP/浏览器或 ACP 列为 v1 必备能力，保持无头引擎和 CLI/RPC/Rust Host 同核 |

本机 Linux v1 固定预算门槛已通过；P2 能力仍须各自用独立任务与候选版本验证。参考项目的功能覆盖面不构成 Mio 的功能清单或通过标准。

---

## 四、质量门禁与验收命令

所有阶段的代码与文档变更在合入前必须通过以下自动化门禁（CI 锁定 Rust `1.95.0`）：

```bash
# 1. Rust  workspace 四项核心门禁
cargo +1.95.0 fmt --check
cargo +1.95.0 check --all-targets --locked
cargo +1.95.0 test --locked
cargo +1.95.0 clippy --all-targets --locked -- -D warnings

# 2. 文档站点、模型目录与 CLI 参数表同步校验
python3 -B tests/architecture.py
python3 -B scripts/check-architecture.py
python3 scripts/check-site.py

# 3. 评测器离线自检与终端协议回归（Unix）
python3 -B tests/agent_eval_harness.py
python3 -B tests/agent_workflow_tasks.py
python3 tests/osc7501.py
python3 tests/cli_cancellation.py
python3 -B tests/cli_terminal.py
python3 -B tests/crash_recovery.py --binary target/debug/mio
```

### 评测支线发布门槛（Release Gates）

- **离线保真**：约束保持、工具调用配对、失败证据保真三类独立离线用例通过（对应 `REQ-CONTEXT-FIDELITY`）。
- **全任务回归**：候选版本在固定模型与固定预算下，两套任务集各跑一轮、共 6 任务全部通过（Agent 退出 0 且验收 0）。
- **长文重复运行**：`retrieve-wire-spec` 在候选版本上另做至少 3 次独立同预算运行（不复用修复前基线结果），报告全部结果（含失败），通过率 ≥2/3，并保留初次失败记录。
- **不得伪造**：不通过增加预算、放宽验收、任务特判或删改失败样本达到门槛；降低重复 `read` 次数需与基线对照且保持正确性，不得凭时长判断信息保真。

---

## 五、评测基线与可提交证据

> （iteration-02，2026-10-15，本地快照）本节把可提交结论直接写入文档：原始证据位于本机 `target/evals/iteration-02/`，而 `target/` 被 `.gitignore` 忽略、**不随仓库分发**，因此不能只留链接。[需求与竞品调查](docs/agent-requirements-and-comparison.md) 中「无同模型同预算对照」「未运行竞品 benchmark」等表述成文于本轮评测之前，评测基线与长文任务进展以本 PRD 与 [TODO](TODO.md) 为准。

### 1. 固定口径

| 项 | 值 |
| :--- | :--- |
| 日期 | 2026-10-15（报告时间以各自 `generated_at` 为准） |
| 二进制 | `target/evals/iteration-02/bin/mio-final`，SHA-256 `f90e4527eb627cda62720e1d024922214240ed35403cac90a000cff1c15b0680` |
| 模型 / thinking | `deepseek-chat` / `off` |
| 预算版本 | `deepseek-chat-off-20calls-180s-isolated-v2`（每任务 `--max-model-calls 20`、任务 wall timeout 180s） |
| 隔离 | 每任务使用临时 `{cwd}/../coordination`，各任务独立预算/冷却账本，不串入下一任务或用户会话；临时目录不构成 OS 安全隔离 |
| 任务集 | `mio-python-acceptance-v1`（v1，3 任务）；`mio-workflows-v1`（v2，3 任务，修正了恢复任务指令歧义） |
| 配置 | `evals/configs/mio-deepseek.json`、`evals/configs/mio-deepseek-workflows.json` |

### 2. 六任务结果（两套任务集各一轮，共 5/6 通过）

| 任务集 / 任务 | 状态 | Agent + 验收耗时 | 模型调用 | 验收退出码 | 备注 |
| :--- | :--- | ---: | ---: | ---: | :--- |
| coding · `csv-reader` | passed | 10.878s | 4 | 0 | |
| coding · `median-boundaries` | passed | 13.334s | 4 | 0 | |
| coding · `stable-toposort` | passed | 18.892s | 5 | 0 | |
| workflows · `migrate-line-item-api` | passed | 11.308s | 6 | 0 | 多文件 API 迁移 |
| workflows · `resume-cache-build` | passed | 25.094s | 9 | 0 | 任务内脚本失败后恢复；v2 指令修正，不计入引擎改进 |
| workflows · `retrieve-wire-spec` | failed | 67.262s | 20 | 1 | `agent_exit_nonzero`；诊断类别 `scheduling`；未出现首读 `ContextBudget` 错误 |

表中逐任务耗时为 Agent 与独立验收耗时之和；聚合耗时另含运行器开销，两者不要求精确相等。

聚合：coding 总耗时 43.111s、13 次模型调用、input/output/total = 24200 / 2276 / 26476（cached_input 21376）；workflows 总耗时 103.679s、35 次调用、147054 / 4359 / 151413（cached_input 78592）。usage 由 `mio-jsonl` 事件聚合、`coverage: partial`，`cost_usd` 恒为 `null`。

### 3. 成功判定与指标定义

- **成功判定**：Agent 退出码 0 **且**独立验收脚本退出码 0，才记为 `passed`；失败分类保留 `agent_exit_nonzero` / `acceptance_failed` / `agent_timeout` / `acceptance_timeout` / `environment_error`，Agent 失败时验收脚本仍独立运行并记录。
- **`model_calls`**：`completion_call` 事件按 `(runId, callIndex)` 去重后的已完成调用数，不含未发出完成事件的失败尝试；无事件时为 `null`，不报 0。
- **token / cache**：只在所有已观察调用都上报时求和，否则保持 `null`；`cost_usd` 永不推算；`coverage` 为 `unknown` / `partial` / `complete`。
- **耗时**：`timing.model` / `timing.tool` 是 dispatch 到 outcome 的观察跨度之和，可能重叠，**不等于 wall time**；不含前置排队与上下文准备（`queue_seconds` / `context_seconds` 为 `null`），也不含无 outcome 的失败与被取消请求；子代理计时尚未转发。
- **工具 disposition**：工具事件里的 success 只表示工具自报结果，**不等于命令或任务成功**。
- **诊断类别**：只保存 Mio 上报的已知类别（如 `context_budget`、`scheduling`），不保存消息或原始请求。

### 4. 剩余失败与非目标

- **剩余失败**：`retrieve-wire-spec` 在一轮运行中耗尽 20 次模型调用仍失败，且验收脚本同样失败。较早的独立辅助运行记录了对 `docs/wire-format.md` 的 16 次 `read`，其二进制与最终快照不同，不能据此认定最终运行也读取了 16 次。候选原因（重复读取、约束丢失、预算规划、catalog fallback）均**待证实**。
- **非目标**：不把单次小样本当作稳定性、跨厂商或排行榜结论；不靠增加预算、放松验收或任务特判制造通过；不把本地合成探针的模块级改善当作端到端加速。
- **对照边界**：`target/evals/compare-pi-reasonix/REPORT.md` 记录过 pi / reasonix / Mio 在 `mio-python-acceptance-v1` 上同模型单轮比较，但三方调用预算未对齐（Mio 有 `--max-model-calls 20`，pi / reasonix 无等价开关），因此**不能据此宣称优劣**。公共 benchmark（`evals/adapters/harbor/`）仍未接入。
- **新增对照配置**：`evals/configs/mio-deepseek-comparison.json` 与 `pi-deepseek-comparison.json` 已提供 `deepseek-flash` / thinking off / temperature 0 / 单次输出上限 8192 / 每任务 180s 的工作流对照入口，双方不设调用或总 token 上限。这是独立预算口径，不与本节 `deepseek-chat`、20 次调用回归混用；配置存在不代表已完成实测，结论须绑定各自报告、版本与采样适配器。
- **任务覆盖**：`retrieve-wire-spec` 检验磁盘长文件检索与约束应用，不直接验证跨轮压缩保真；`resume-cache-build` 检验任务内脚本失败后的处理，不代表 Mio 进程崩溃恢复。摘要保真与崩溃恢复必须分别验收。

### 5. 本地证据与复现命令

权威本地文件（`target/` 被忽略，不随库分发；持久结论以本文档与 [TODO](TODO.md) 为准）：

- `target/evals/iteration-02/summary.json`：本轮汇总与已修正项
- `target/evals/iteration-02/coding-isolated-final.json`、`workflows-isolated-final.json`：逐任务报告（含 `binary_sha256`）
- `target/evals/iteration-02/wire-diagnosis.json`：较早辅助快照的工具事件诊断，不属于上述最终二进制运行
- `target/evals/iteration-02/quality/results.json`：本轮门禁与 Python 自检结果
- `target/evals/iteration-02/bin/mio-final`：固定二进制

以下命令在仓库根目录执行。真实运行前，凭据应已在环境变量中配置；不在命令或报告中写入密钥。历史固定二进制若已被清理，则不能把重新构建的当前源码标为相同历史快照。

```bash
python3 -B tests/agent_eval_harness.py
python3 -B tests/agent_workflow_tasks.py

# 只准备配置与摘要文件，不调用模型；每次使用新的目录，避免覆盖历史报告。
python3 - <<'PYCONFIG'
from pathlib import Path
import datetime, hashlib, json, shlex
root = Path.cwd()
binary = root / "target/evals/iteration-02/bin/mio-final"
expected = "f90e4527eb627cda62720e1d024922214240ed35403cac90a000cff1c15b0680"
assert hashlib.sha256(binary.read_bytes()).hexdigest() == expected, "snapshot mismatch"
out = root / "target/evals" / datetime.datetime.now(datetime.timezone.utc).strftime("review-%Y%m%dT%H%M%S%fZ")
out.mkdir(parents=True, exist_ok=False)
files = [binary, root / "scripts/agent_eval.py"]
for label, source, tasks in (
    ("coding", "mio-deepseek.json", "coding"),
    ("workflows", "mio-deepseek-workflows.json", "workflows"),
):
    config_path = root / "evals/configs" / source
    config = json.loads(config_path.read_text())
    command = shlex.split(config["agent_command"])
    command[0] = str(binary)
    config["agent_command"] = shlex.join(command)
    config["tasks_dir"] = str(root / "evals/tasks" / tasks)
    config["output"] = str(out / (label + ".json"))
    (out / (label + "-config.json")).write_text(json.dumps(config, indent=2) + "\n")
    files.append(config_path)
    files.extend(p for p in (root / "evals/tasks" / tasks).rglob("*") if p.is_file() and "__pycache__" not in p.parts)
manifest = {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(set(files))}
(out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
print(out)
PYCONFIG

# 将下列路径替换为刚输出的目录；以下命令会调用真实模型。
# 长文重复运行，每次显式指定新的报告名；失败退出码 1 也应保留。
python3 scripts/agent_eval.py --config <run-dir>/workflows-config.json --task retrieve-wire-spec --output <run-dir>/wire-1.json
python3 scripts/agent_eval.py --config <run-dir>/workflows-config.json --task retrieve-wire-spec --output <run-dir>/wire-2.json
python3 scripts/agent_eval.py --config <run-dir>/workflows-config.json --task retrieve-wire-spec --output <run-dir>/wire-3.json
# 全任务集回归
python3 scripts/agent_eval.py --config <run-dir>/coding-config.json
python3 scripts/agent_eval.py --config <run-dir>/workflows-config.json
```

`<run-dir>` 是路径占位符，执行前须替换。当前运行器不会自动计算 `binary_sha256`，历史报告的该字段是额外记录；上面的 manifest 单独保存二进制、运行器、原始配置、任务与验收脚本的 SHA-256，生成配置保留实际 argv。运行期间不得修改这些输入，结束后须重新校验 manifest；有变化的运行标为口径变更，不并入同快照对照。当前源码候选版本应另建固定二进制与 manifest，不能继续沿用历史 SHA。Provider 的模型别名和服务端实现可能变化，固定本地工件不能保证重跑结果相同。

当前运行器在临时目录解析并删除 `agent.log` / `check.log`，只持久化聚合报告。逐次 `read` 路径/范围、摘要约束覆盖的有界脱敏证据采集仍待实现；不能将聚合报告描述成完整轨迹，也不应默认留存原始模型输出。

### 6. 记录口径（快照不要混用）

- **阶段二验收快照**（[阶段二验收记录](docs/reviews/2026-10-15-phase2-acceptance.md)）：独立固定源码/二进制，`cargo test` 2446 passed / 0 failed / 5 ignored，评测器自检 38 项，站点校验 67 CLI 参数。
- **iteration-02 评测快照**（本节）：Rust 四项门禁 rc=0，`agent_eval_harness.py` 40 项 + `agent_workflow_tasks.py` 12 项 = 52 项通过；该迭代一次早期运行记录的 38 项已被 40 项取代。
- 两组数字来自不同快照，引用时必须标明来源，不得相加或互相替代。
