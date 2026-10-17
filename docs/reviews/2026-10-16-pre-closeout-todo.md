# 收口前 TODO 历史快照

> 历史状态，不代表当前实现；仅调整了从项目根目录迁移后的相对链接。当前状态见 [TODO](../../TODO.md)。

# Mio TODO

本文件只记录当前进度与下一步。需求依据见 [PRD.md](../../PRD.md)；架构、控制平面与完整验收口径见 PRD 与 [需求与竞品调查](../../docs/agent-requirements-and-comparison.md)；历史审查证据编号与复现过程见 [审查记录](../../docs/reviews/2026-10-15-audit.md)。`REQ-*` 为需求编号，`AUD-*` 为审查证据编号，保留用于追溯。

## 一、状态规则

命令行约定：`mio` 和 pi 一样以终端对话为入口，只保留输入提示、流式回复与必要的工具/审批信息。支持 `-p`、管道、JSON/RPC；不规划独立 GUI、全屏 TUI 或装饰性面板。终端验收覆盖不主动清屏、不擦除滚动历史，以及非交互输出不混入提示符。

- 已完成：实现已落地，且有离线或本地运行证据。不等于线上、跨平台或极端故障下已验证。
- 部分完成：主路径可用，仍有跨进程、联网、真实崩溃注入或压力验收残余。
- 未完成：无可信验收证据，或仅停留在设计。
- 未勾选不等于缺陷。单条勾选只代表该条的离线证据成立，不代表整个需求完成。

## 二、已完成基线与证据范围

调度、安全与恢复条目主要来自本地 mock / loopback、多进程与离线验收；真实 DeepSeek 评测单独标注。均不代表真实硬件断电或跨平台验收。

- 共享调度与配额（REQ-SCHEDULE / REQ-CONFLICT / REQ-QUOTA）
  进程内 `Coordinator` 与跨进程 `SharedLimits`（`flock`）模型槽位、429 冷却、重任务配额、目录前缀写冲突 FIFO、`Workspace` 跨进程路径租约。
  证据：`src/scheduling.rs`、`src/coordinator.rs`、`src/shared_limits.rs`、`src/workspace.rs`；`tests/scheduling.rs`、`tests/coordinator.rs`、`tests/shared_limits.rs`、`tests/workspace.rs`。
- 预算预留与结算（REQ-BUDGET，本进程内）
  调用前保守预留，按 Provider 上报 usage 结算，未上报按预留计，超额记录并阻断后续请求。
- 工具与进程边界（AUD-01/05/06/07/19）
  插件命令共用 `shell::run_supervised` 独立进程组与超时强杀；插件输出、MCP 帧与 RPC 输入行大小限额；线性 NFA 正则单次扫描；插件路径逐组件符号链接拒绝；`edit` 原文件身份与内容校验。
  证据：`src/shell.rs`、`src/plugin.rs`、`src/pattern.rs`、`src/mcp.rs`。
- 扩展与协议（AUD-02/03/04/08/09/10/11）
  插件精确名与别名 deny 优先；项目 MCP 发现前指纹授权且 fail-closed；Anthropic / Google / Ollama 原生协议路由；MCP 分页与名称冲突报错；会话分级消歧与 mtime 降序扫描。
  证据：`tests/provider_routes.rs`、`src/session.rs`。
- 执行证据与诊断（REQ-ACCEPT / AUD-13/15）
  `.mio/journal` 版本化意图与状态记录、`--recovery` 报告、`CompletionCallObservation` 与 `RunDiagnostic` 脱敏诊断。
  证据：`src/journal.rs`、`tests/diagnostics.rs`。
- CLI/RPC 显式任务去重（REQ-DEDUP）
  CLI `--dedup-key` 与 RPC `prompt` / `follow_up` 的 `dedup_key` 共用 `.mio/dedup` 工作区账本，活动、排队与全部终态均不重放并返回原 `task_id`；4 进程竞争仅产生一次 loopback 模型请求。
  证据：`src/dedup.rs`、`tests/cli.rs`、`tests/rpc.rs`。
- 文档与评测自检
  `scripts/check-site.py`、`tests/agent_eval_harness.py`、`tests/osc7501.py`、`tests/cli_cancellation.py` 可离线运行；`evals/` 为自有验收探针，不是公共 benchmark。
- 评测基线（REQ-EVAL-BASELINE，已完成）
  固定模型 `deepseek-chat`、thinking off、预算 `deepseek-chat-off-20calls-180s-isolated-v2`（每任务 20 次调用/180s）、二进制 SHA-256 `f90e4527eb627cda62720e1d024922214240ed35403cac90a000cff1c15b0680`（2026-10-15）。`mio-python-acceptance-v1` v1 3/3 通过，`mio-workflows-v1` v2 2/3 通过，共 5/6。
  证据：`target/evals/iteration-02/`（本地忽略目录，不随库分发）；持久表见 [PRD 第五节](../../PRD.md#五评测基线与可提交证据)。
- 真实事件采集（REQ-EVAL-BASELINE / REQ-EVENTS 基础）
  `completion_call` 真实 usage/cache 计数、`execution_timing` 的 model/tool dispatch-to-outcome 跨度、`diagnostic_categories` 已知错误类别可被 runner 聚合；缺上报保持 `null`，`cost_usd` 永不推算。
  证据：`evals/README.md`、`scripts/agent_eval.py`；`target/evals/iteration-02/*.json`（本地）。边界：跨度可能重叠，不含前置排队/上下文准备/无 outcome 失败/未转发子代理；工具 disposition 成功不等于命令或任务成功。
- read 输出预算分页与长文件提示（REQ-CONTEXT / REQ-CONTEXT-RETRIEVAL 基础）
  `read` / workspace `read` 单次输出按已解析 `input_limit` 的 1/4 收紧并 clamp 256 B–64 KiB；直接构造 `Workspace` 默认仍 64 KiB，可显式设置实例上限；截断时返回完整行分页与 `Next offset`，并补通用大文件检索提示。
  证据：`src/lib.rs` 的 `run_with_tools`、`src/workspace.rs` 的 `Workspace::with_read_output_limit_bytes` 与 `execute_read`；Rust 分页与预算回归见 `tests/read_context_budget.rs`。
- 长历史预算系统前缀去重（REQ-CONTEXT / REQ-BUDGET 基础）
  `BudgetHook::fit` 对历史消息成本改为一次编码 + 运行前缀累计，避免每个候选切点重复编码系统前缀。
  证据：`src/context_budget.rs`。本地 debug 合成探针 633 条消息 / 1436229 输入字节，best-of-three 1551.64 ms -> 64.04 ms（相同输出指纹）；这是本地合成 fixture 的模块级探针，**不是端到端 24 倍加速**，不代表真实 Provider 或真实任务延迟。

快照口径（不要把不同快照的数字混用）：

- 阶段二验收快照（[阶段二验收记录](../../docs/reviews/2026-10-15-phase2-acceptance.md)，独立固定源码/二进制）：Rust 1.95.0 四项门禁 rc=0，全量测试 2446 passed / 0 failed / 5 ignored；站点校验 6 页面、67 CLI 参数/别名；评测器自检 38 项；终端、取消与 10 项真实 SIGKILL 验收通过。该记录的 5 项 ignored 包括需 API key 的测试、文档测试和手动性能探针，不计入通过。
- iteration-02 评测快照（`target/evals/iteration-02/`，SHA 见上；target 为本地忽略目录）：`fmt` / `check` / `test` / `clippy` rc=0，`tests/agent_eval_harness.py` 40 项 + `tests/agent_workflow_tasks.py` 12 项 = 52 项通过（同迭代早期一次 harness 38 项已被 40 项取代）。
- 两个快照的测试计数与二进制均不同，不得互相引用或相加。后续新改动仍需重新验收。

下一步补齐统一门禁的插件/MCP 端到端证据与恢复视图；文件检查点、MCP 生命周期与大输出外置的主体已实现，仍需与统一门禁在同一轮里补端到端验收（本轮未重新运行门禁）。

## 当前执行批次：v1 设计与实现

权威契约：[v1 设计规格](../../docs/design-v1.md)。逐项未完成工作见 [v1 完成度审计](../../docs/reviews/2026-10-15-v1-completion-audit.md)。设计状态与代码验收分开记录。

最新统一候选验收：Rust 1.95.0 四项门禁 rc=0，127 个 targets / 3109 passed / 0 failed / 6 既有 ignored；真实 CLI 原生捕获丢失→独立拒绝/确认→新 ID 一次写入通过，12 项 SIGKILL、SDK 三场景与 4 项契约通过；Linux SDK 27 组编译全部通过。CLI/SDK 严格清单覆盖 82 参数/别名、19 项检查器测试通过。此结果对应最终收尾修复前的候选，不等于主树最终通过。正在合入排队句柄、失败测试子进程回收和 SDK 日志唯一命名；随后冻结主树、复验隔离/宿主及同预算模型任务，更新最终证据。


本轮收口重点（2026-10-16）：任务候选已补实际 worker 收尾等待、按 task_id 投影以及晚结束子任务的原 journal 归属；主树尚未接入。持久未知候选 round5 通过 158 项定向 Rust 与 12 项真实 SIGKILL 测试，尚待整合。审查发现运行中 unknown 的确认需要先将真实 evidence_id 绑定到持久 snapshot，原生进程监督也仍需 typed Unknown；两项正修复。完整 SDK 离线示例正在编译验收。上述候选结果不计作最终主树四项门禁通过。


- [x] 收敛 `DES-01`—`DES-22`：明确范围、默认值、状态转移、兼容与失败路径；审查修正中断不代表零副作用、旧审批回调方向以及取消应答与执行结束的区别。
- [x] 协议发现与兼容协商：`get_state.data.protocol`、`schemaVersion: 1` 与 `initialize` 的接受/拒绝/重新协商已落地，4 项真实 RPC 协议回归通过。旧客户端保持字段兼容；未知关联 ID 保持 null，全链路 ID 仍未完成。
- [ ] 上下文保真：已实现类别配额、有界头尾、定位 ID、显式遗漏提示，以及独立的结构化 Pinned 注册表。定向 context_fidelity 7 项通过；仍不能宣称任意中部信息无损保留，完整整合结果另记。
- [ ] 整合门禁：2026-10-16 主目录完成 Rust 1.95.0 fmt/check/test/Clippy，四项 rc=0；默认并行全量 116 suites / 2910 passed / 0 failed / 6 既有 ignored。含新文件系统有界扫描与子代理查询语义修复，证据 `target/evals/design-implementation-20261015/main-combined-qa/`。随后新增的 RPC 传输失败传播、持久未知账本与任务基础模块不受该次绿色覆盖，最终稳定源码仍须重新验收。
- [x] 三层生产依赖方向检查：新增 `scripts/check-architecture.py` 与 3 项离线回归并接入 CI；检查当前仓库通过，SDK API 稳定性仍待单独验收。
- [ ] 显式 Pinned 与结构化损失审计：已接入 SDK/会话/CLI/RPC、固定指令成本、子代理继承、压缩事件/journal/恢复查询；定向 RPC/context/protocol 共 43 项、CLI 单元 21 项通过。新增完整 history 前后计数与显式约束/目标 ID，旧报告缺失值保持 unknown。原 Rust 定向 29 项通过、1 项失败（`/tmp/mio-compaction-audit-qa/`），修复后 35 项全通过（`/tmp/mio-compaction-size-fixed-qa/`）：固定约束原文实际进入请求、超预算拒绝零连接；原自动夹具 2729 B -> 2722 B，严格缩减断言保留。自动路径不应用无字节收益候选；极小输入的显式摘要可保留审计但必须报告真实前后大小。恢复查询与输出清理保护仍需最终全量验证。
- [ ] 关联 ID：真实 run/turn/tool/审批/子代理身份已贯通至 Host/v1 事件；已修复准入前误报 Dispatched 和拒绝后工具配对污染。定向关联 8、拒绝边界 1、协议 4 项通过。新增真实工具身份 scope 的底层测试 5 项通过，覆盖同名并发逆序完成、父身份替换与序列化隔离；context 回归 12 项通过，日志在 `/tmp/mio-invocation-scope-qa/`。上层生命周期编译错误已修复，工具包装 5 项、嵌套缺失身份 1 项、真实 RPC 同名工具并发逆序 1 项、关联 8 项、拒绝边界 1 项均通过（`/tmp/mio-identity-runtime-integrated-qa/`）。`--tool-concurrency` 已接入真实引擎，默认 1、范围 1–32；统一门禁与长任务复验仍待完成。
- [x] 调用防重（定向验收）：每个 run 的 ToolPolicy 已增加有界、共享、原子调用 ID 认领；重复、拒绝后重放、取消后重放均拒绝，容量满不驱逐旧 ID。新增 loopback/并发/容量边界测试；tool_policy 14 项已实际通过，统一版本复验仍待完成。
- [x] 任务存活（定向验收）：已增加锁内 owner/liveness 观测及真实子进程测试；CLI/RPC 已接入；liveness 16 项、真实 CLI/RPC 查询 4 项通过。unknown/stale 均不自动重放，不凭时间推断死亡。
- [ ] 输出生命周期：生产索引登记/回滚、activity 租约、外部会话来源、离线 dry-run/confirm 已接入；真实 call_id 与模型请求、磁盘字节/hash、byte 分页重建 2 项通过。模型端到端分页 3 项通过。新增结构化 output_references 内容块，SDK 与 engine 统一调用路径、rich 内容保持和失败回滚正在验收；磁盘满与最终统一门禁仍待完整记录。
- [ ] journal 权威调用状态机：v2 canonical calls 已接入真实 Policy/dispatch/terminal，兼容读取 v1，拒绝为 Denied、中断前未派发为 NotDispatched、已派发无结果为 Unknown。同名同参数逆序 RPC、非法转移、回调内快照、参数摘要一致与敏感参数不落盘、typed 输出引用等 18 个定向 suite 通过；fmt/check/Clippy 通过（`target/evals/design-implementation-20261015/canonical-integration-qa/`）。子代理查询语义回归与本轮 2910 项全量验收已通过；后续预算/持久保护改动仍须最终复验。
- [ ] 未知副作用保护：已证实仅 MCP client 内部不重发并不足够，模型可换新 ID 重放，且传输未知被误写 Failed；已增加类型化 Unknown、父子共享保护与独立宿主确认，并修复确认期证据变化、overflow和live scope驱逐三处风险。结构化输出/真实journal并发 76 项通过；未知相关 91 项串行通过，容量夹具改为独立进程后默认并行边界连续 10 次通过、相关 60 项通过。另补独立证据 ID 与真实 scope，防止父子运行复用 toolCallId 时合并不同未知事实，已随统一状态机定向复验通过。持久化账本基础已返回但未编译；已在隔离源码接线与执行真实重启验收，尚未合回主树。
- [ ] 任务权威状态：已只读核查 CLI/dedup 在真实模型租约前误报 active、journal 与引擎 run ID 不同、取消先于阻塞 worker 回收等缺口；`task_lifecycle.rs` 真实许可/worker 基础模块 23 项测试通过；CLI/RPC/SDK/子代理生产接线在隔离副本完善，尚未合回主目录，不算完成。
- [ ] SDK/宿主语义：已完成 CLI→SDK 映射缺口只读核查，尚需可机检分类和行为对照。Ravomi 当前实际接入为 CLI/PTY；真实宿主解析器与 Mio PTY 字节的契约探针正在实现，不启动 GUI、不改相邻项目源码，也不宣称原有 SDK 对照即真实宿主通过。 已将 `--no-shell` 接入公共 `ToolFilter::without_shell()`，共享准入的真实 CLI/SDK 对照及相关策略共 21 项通过（`shared-no-shell-qa/`）；更广映射尚未完成。
- [ ] 后续按 DES-21/22 推进统一任务/工具状态转移、跨重启未知保护、SDK/宿主同核检查及最终统一门禁；未获证据前不自动勾选。
- [ ] 新候选真实评测未达标：固定二进制 `60bc67318a215e198fee90dcb6c4cdd19a578db1da872ac2daa06aaa9c61673b` 在原 20 calls/180s 预算下六任务 5/6，长文独立 3 次仅 1/3（需至少 2/3）。失败包含 context_budget 与调用上限阻断；报告在 `target/evals/design-implementation-20261015/live-candidate/`。不扩大预算、不修改任务验收，继续排查通用输出预算与检索行为。
- [ ] 通用输出预算：补齐 Shell/grep/find/ls/插件/MCP 的请求预算约束，并按整个工具批次分配，外置全文、显式引用和当前工具配对均保留。现有单条静态上限及 read 的 input_limit/4 不能证明任意多调用批次可容纳；这是代码级缺口，尚无本轮真实失败的工具轨迹证明。 实现正在接入请求 prepare 前的聚合外置与真实 canonical 引用保护，尚未返回验收。

## 三、阶段二 P1（近期收口）

- [ ] 全入口统一门禁（REQ-POLICY / REQ-PROCESS，AUD-16）
  要做什么：把内置工具、插件、MCP、`extra_tools` 与子代理收敛到单一准入网关，审批绑定规范化参数与上下文，嵌套委派权限只能单向收窄。
  验收条件：任一入口都无法绕过过滤或审批；拒绝或超时路径零未授权副作用；参数篡改与重放被拒绝。
  进展：统一 dispatch 网关、参数摘要与独立审批标识、审批超时、extra_tools 保守审批、子代理权限收窄已实现；`tests/tool_policy.rs` 与全量回归通过。`tests/tool_policy_extensions.rs` 已补插件/MCP 实际路由、拒绝零副作用及参数绑定端到端用例，本轮重新执行中；完整门禁结果返回后再更新状态。
- [x] 跨进程预算持久化（REQ-BUDGET 深化）
  要做什么：在 `SharedLimits` 共享目录落地版本化预算账本，支持多进程原子预留与真实 usage 结算。
  验收条件：多进程共享 root 与重启后 `--max-total-tokens` / `--max-model-calls` 一致生效，损坏时 fail-closed，并有并发进程实测证据。
  证据：`tests/shared_budget.rs`、`tests/shared_limits.rs`、`tests/scheduling.rs`，覆盖真实多进程竞争、重启继承、SIGKILL 后不退款、账本损坏及策略收窄。本轮全量通过。仍是保守预留与 usage 结算，不宣称精确 token 硬限额。
- [ ] 真实 Provider 联调与并发压力（REQ-PROVIDER，AUD-18）
  要做什么：统一 CLI 预设、底层 `Catalog` 与站点生成数据的能力来源，区分凭据存在 / 端点可达 / 认证有效 / 协议支持，记录能力来源与更新时间；凭据授权后做真实端点联调并单独标注结果。
  验收条件：真实端点认证头、thinking 映射、usage、429 与 `Retry-After`、错误分类实测通过；持续并发压力下无泄漏，并有取消与恢复观察。
  进展：`scripts/provider-live.py` 已在本轮验收二进制完成 DeepSeek 单次认证/流式/usage 探针（64 输入、5 输出 token，报告含二进制 SHA-256）；同一 Provider 另有 PRD 第五节所列 6 任务实测，但不覆盖完整 thinking、429 或跨 Provider 能力，其他能力保持未完成。 目录已接入共同底层事实、来源与快照 SHA-256，并将凭据存在与线上状态分开；新增 `tests/catalog_provenance.rs` 待统一验收。
- [ ] CLI/RPC 崩溃恢复端到端注入（REQ-RECOVERY，AUD-13 深化）
  要做什么：对请求前、执行中、结果返回后、保存前四个时点做真实中断注入，统一 journal 与 session 恢复视图并暴露给用户。
  验收条件：四时点中断后证据完整可查、副作用不重复执行、未知副作用保持 unknown、不覆盖用户后续修改。
  进展：`tests/crash_recovery.py` 在 Linux/glibc 下 10/10 通过，覆盖 CLI/RPC 四时点真实 SIGKILL、unknown/executed 证据、不重放和用户后续修改保护。CLI `--recovery` 与 RPC `get_state.recovery` 已实测；`recoveryView` 和 `unfinished_goals` 已实现，`tests/recovery_view.rs` 覆盖磁盘历史/执行证据关联、未保存目标与损坏历史不修复，本轮待重验。SIGKILL 不代表硬件断电。
- [ ] 可选文件检查点与 rewind（REQ-RECOVERY 深化）
  要做什么：为 `write` / `edit` 提供可选 pre-edit 快照与 rewind。
  验收条件：rewind 检测并保护用户后续手工修改，不触碰 Git 历史，也不回滚 Bash 副作用。
  进展：快照与索引落在工作区 `.mio/checkpoints/`（临时文件原子替换，目录 0700、文件 0600），默认关闭，可由 `--checkpoints`、`MIO_CHECKPOINTS=1` 或交互 `/checkpoints on` 开启；`--list-checkpoints` 与 `--rewind ID`（唯一前缀至少 4 字符）是独立 CLI 维护命令，不联系模型，与 `-p` / `--mode rpc` 互斥，`--checkpoints` / `--list-checkpoints` / `--rewind` 都与 `--no-session` 互斥。rewind 经 `Workspace::rewind_checkpoint` 共用只读检查、工具过滤、跨进程写租约与 `approve_tool_async("rewind")`；索引改动在 `index.lock` 的 `flock` 内串行，只回退状态为 `Active` 且当前内容匹配「操作后指纹」的文件，`Pending` / `Failed` / `Rewound`、用户后续修改、目标缺失与损坏索引都拒绝并保留证据。
  证据：`src/checkpoint.rs`、`src/workspace.rs`（`rewind_checkpoint`）、`src/bin/mio_cli/{args,runner,terminal}.rs`；`tests/checkpoint.rs`、`tests/checkpoint_cli.rs`。
  待验收：与真实崩溃注入（`tests/crash_recovery.py`）组合的端到端用例、跨平台行为，以及非协作写入者在内容预检与 rename 之间乐观窗口的边界说明；本轮未重新运行门禁，验收条件仍以未勾选为准。

## 四、阶段二 P2

- [ ] 并发写冲突与 TOCTOU（REQ-CONFLICT / REQ-RECOVERY，AUD-19 深化）
  要做什么：对写冲突排队与 `ensure_edit_source_unchanged` 到 persist 之间的窗口做真实故障注入与压力验收，必要时引入基于目录句柄的原子操作消除 TOCTOU。
  验收条件：高并发写不交错覆盖，冲突要么排队要么显式拒绝，路径校验到写入之间无可利用窗口。
  源码审查已确认：`checked_path` 返回字符串后，读写和 checkpoint 仍会重新解析父目录；叶子 `O_NOFOLLOW` 或原子 rename 不能关闭父目录交换窗口。固定 root/parent 目录句柄原语已通过 10 项真实 inode/符号链接/FIFO/清理回归及 Clippy；尚未接入生产。确定性父目录交换已使现有行读、byte 读返回外部合成哨兵（`/tmp/mio-pinning-path-swap-baseline/`），两条失败断言保留，整合 patch 准备中。非协作内容写入的预检到 rename 竞争仍需单独记录，不冒充原子 CAS。
  补充：取消后阻塞写 worker 的租约保持目前是受控构造，需补真实磁盘压力。
- [ ] MCP trust 与连接生命周期（REQ-EXTENSIONS，AUD-03/08/09/16）
  要做什么：持久化项目 MCP trust store；划分 required / optional 服务；支持连接复用与断线重连，并给出启动失败诊断。
  验收条件：单一可选 MCP 失败不阻断整体运行，长会话复用连接，非交互 CLI/RPC 的 fail-closed 有端到端用例。
  进展：trust store 已迁到用户配置目录 `mcp-trust/<sha256(规范化工作区)>/trust.json`（0600 / 0700、原子替换），只存指纹、服务器名、工作区与批准时间；信任根位于项目内、符号链接或版本不符都拒绝读写；权限过松或损坏的存储不授予任何信任，但可由一次显式批准原子修复。`Host::allow_persisted_mcp_trust` 默认 `false` 且 `mcp_trust_directory` 默认 `None`，库宿主默认逐次自行批准，CLI 才启用持久信任。`required`（默认 true）/ `optional`（优先）已接入启动与 `tools/list` 失败路径，可选服务器只打印诊断并继续；连接按「工作区 + 服务器 + 指纹」进程内缓存（上限 64，超过按 LRU 淘汰，被淘汰连接在最后一个句柄释放时回收子进程组），配置变更换新连接，`tools/list` 传输失败最多重连一次，`tools/call` 不重放并标记副作用 unknown。
  证据：`src/mcp_trust.rs`（单元测试）、`src/mcp.rs`；`tests/mcp_lifecycle.rs` 13 项。
  待验收：非交互 CLI/RPC fail-closed 的端到端用例、长会话真实复用压力、崩溃遗留 MCP 子进程与跨平台；本轮未重新运行门禁。
- [ ] 大输出外置（REQ-EXTENSIONS，AUD-05）
  要做什么：把超过 32 KiB 的插件与工具输出外置为临时文件引用，而不是只内联截断并附 `[TRUNCATED]`。
  验收条件：调用方拿到可分段读取的引用，模型可按需读取完整输出，截断不再丢内容。
  进展：Shell 与插件命令经 `shell::capture` 单流不超过 32 KiB 时内联，超出时返回约 2 KiB 预览，完整输出外置到工作区 `.mio/outputs/<uuid>.<stream>` 并在结果里附引用；单流超过 8 MiB 硬上限时该工具失败并终止进程，不返回截断成功，取消或失败会清理未提交文件。MCP 工具大结果经 `shell::externalize_output` 走同一外置路径。`read` 新增 `byte_offset` / `byte_limit`（4–65536 字节）JSON 分页，返回 `next_byte_offset` / `total_bytes`，不能与行分页同时使用，且行分页与 byte 分页都拒绝超过 8 MiB 的文件。
  证据：`src/shell.rs`、`src/file_page.rs`、`src/workspace.rs`、`src/mcp.rs`；`tests/output_paging.rs` 2 项、`tests/mcp_lifecycle.rs` 的外置用例、`src/file_page.rs` 单元测试。
  新进展：生产捕获已登记索引并支持事务回滚；清理有独立预览/确认入口。Shell 14 项与输出集成 72 项在固定快照通过。已用真实确认删除复现缺口：先建 Workspace、后首次产生输出时，活动任务未持租约；修复中，不能把预览成功当作删除保护。
  当前修复：Workspace 与 CLI/RPC 在首次输出之前即持有项目外、按工作区唯一的 activity 租约；不创建项目 `.mio`，且不依赖配置目录或 TMPDIR。早先 `/tmp/mio-runtime-lease-qa/` 被动态工具包装器编译错误阻断，该错误现已修复。当前 `/tmp/mio-identity-runtime-integrated-qa/` 的首次捕获缺口 2 项、运行锁 7 项、生命周期 7 项、清理 39 项、清理 CLI 27 项、捕获 4 项均通过；运行目录权限回归 1 项也通过。它们不能代替全量门禁或跨平台证据。
  待验收：重跑完整门禁；补真实长任务按引用读完整输出，以及取消与跨平台边界。真实磁盘满验收已通过（`MIO_REQUIRE_BWRAP=1`、128 KiB tmpfs、256 KiB 捕获，`/tmp/mio-output-enospc-qa/`）：返回 ENOSPC、索引和旧输出不变、无临时产物与运行锁残留。已从全量回归保留默认上下文因 32 KiB 外置预览超预算的失败证据（`/tmp/mio-output-preview-context-failure.log` 的 `output_call_identity`）；现将外置预览缩至 2 KiB，完整文件不变，已在原默认窗口复验通过（`/tmp/mio-output-preview-model-qa/`）：调用身份 2 项、捕获 5 项（含一个流已发布后取消回滚）、模型实际分页与 8 MiB 超限 2 项、Shell 14 项，共 23 项。该轮分页 fixture 显式使用 400000 窗口，不能冒称默认窗口结果。后续新增真实默认窗口/32轮回归，旧分页在 32 次模型调用、31 页时失败且剩 6418 字节未读（`/tmp/mio-default-paging-baseline/run-20261015T221159/`）。改为按实际 canonical 工具输出序列化大小收紧 UTF-8 页面后，默认/显式窗口、8 MiB 拒绝及预算/范围/身份等共 24 项通过（`/tmp/mio-byte-budget-fixed-qa/`），未增加预算。定向计数实测为 9 次模型请求、7 页完整覆盖 39964 字节（`/tmp/mio-default-paging-success-metrics/`）；仍是 loopback，不是实际 Provider 结果。
- [ ] 事件版本化与上下文保真（REQ-EVENTS / REQ-CONTEXT，AUD-14/15 深化）
  要做什么：版本化 RPC/Host 事件 schema 与能力协商，贯通 session / run / turn / tool-call / subagent ID；改进压缩摘要以保留早期用户约束、未完目标、决策与工具证据，并报告损失；稳定前缀并实测 token/cache 指标。
  验收条件：一次失败可从用户错误追踪到具体请求与工具调用；慢消费者背压测试通过；长程压缩后约束保持率与缓存命中指标可测量。
  进展：有界非阻塞输出的 11 项单测、4 项真实 RPC 协议回归通过，覆盖慢消费者取消、显式丢弃计数与结构化队列满时失败退出；当前整合版本仍待重验。Host 全链路 ID 与长任务保真指标尚未完成。 关联验收必须覆盖同名工具并发、完成顺序反转和审批拒绝：不能用派发 FIFO 猜工具结果 ID，也不能把门禁前的尝试标为已执行。

## 五、阶段三（中期演进）

- [x] 评测基线建立（REQ-EVAL / REQ-EVAL-BASELINE）
  已完成：固定模型 `deepseek-chat`、thinking off、预算 `deepseek-chat-off-20calls-180s-isolated-v2`（每任务 20 次调用/180s）、二进制 SHA-256 `f90e4527eb627cda62720e1d024922214240ed35403cac90a000cff1c15b0680`（2026-10-15）；`coding` 3/3、`workflows` v2 2/3，共 5/6。
  证据：`target/evals/iteration-02/`（本地忽略目录，不随库分发）；持久表见 [PRD 第五节](../../PRD.md#五评测基线与可提交证据)。
  剩余：长文任务 `retrieve-wire-spec` 未通过；仅一次小样本、非行业 benchmark，不代表稳定性或领先。
- [ ] 评测支线下一轮：长文检索、上下文保真与预算感知（REQ-CONTEXT / REQ-CONTEXT-RETRIEVAL / REQ-CONTEXT-FIDELITY / REQ-BUDGET-AWARENESS）
  要做什么与验收条件（当前切入点 `retrieve-wire-spec`；先冻结快照，诊断和离线用例可在付费重复运行前准备）：
  1. **先固定口径并重复运行**：确认 `evals/configs/mio-deepseek*.json` 的模型、thinking、预算版本与二进制 SHA，冻结本轮 spec 与验收脚本；对失败任务重复 3 次同预算运行，报告全部结果并保留初次失败。验收：3 次运行的配置/SHA/逐任务状态与耗时均有记录，不删改初次失败。
  2. **补齐证据采集后再测（脱敏且有界）**：已接入默认关闭的 `--diagnostics-dir`，运行器 102 项、诊断采集 48 项与对照器 33 项自检通过。压缩采集已兼容真实 snake_case 与旧 camelCase，冲突拼写标为未知；前缀替换范围和完整历史前后计数分开，旧报告不伪造缺失值。结构化成功 read 与旧版读取尝试已按整份日志互斥计数，避免开始/成功事件重复统计；真实 Workspace read 9 项、CLI 输出/缓存 19 项、清理 CLI 30 项、RPC/协议 14 项通过（`/tmp/mio-read-cache-integration-qa/`），包含真实返回范围与身份、活动缓存不淘汰及容量耗尽失败；子代理转发单元测试仍待统一门禁。准入后模型派发已接入普通 JSON/v1 与逐任务诊断摘要，14 项真实 RPC/协议回归通过（`/tmp/mio-model-dispatch-wire-qa/`）；该计数不冒充网络发送或账单。read 实际行/字节区间已接入，仍缺逐约束覆盖与压缩后计数证据，未知字段保留 null；补齐后记录重复 `read`、调用次数和压缩前后覆盖。验收：限制每轮事件数量、字段长度与总文件字节数，超限显式标记；证据仅保留工作区相对路径、行/字节范围、计数、类别与离线约束标识，不含任意宿主绝对路径、回复全文或密钥；用合成事件验证脱敏与截断。诊断开启状态须在前后对照中一致；离线摘要缺陷与真实任务根因分开记录。
  3. **写独立离线保真用例**：约束保持、工具调用配对、失败证据保真各成独立用例，不依赖真实模型。已有 `src/compact.rs` 的开头约束/损失提示测试，不能据此勾选请求级保真；长文检索任务本身也不覆盖跨轮压缩。旧实现的工具结果前 240 字符摘要与请求摘要前缀截断已复现并替换为分类配额、头尾摘录和预留损失提示；`tests/context_fidelity.rs` 与 `context_budget` 单测已补基础回归，整合门禁待返回。后续按 DES-11/13 完成宿主显式约束登记与结构化损失报告，覆盖中部/尾部约束在最终请求中的保留或明确拒绝，不靠关键词假装识别全部约束。验收：先保留可复现的失败基线，再修复并使回归通过，不通过放宽断言接受信息丢失；这些代码风险与实测失败的因果关系仍待证实。
  4. **只按已证实原因做通用改进**：候选原因（重复读取、约束摘要丢失、预算规划、catalog fallback）必须先证实再改，通用优先、不为单任务特判。验收：改进前后同预算对照，重复 `read` 次数/请求次数下降且正确性保持；不以时长判断信息保真。
  5. **四门禁 + 同模型同预算全任务回归**：跑 Rust 四项门禁与 Python 自检，再对两套任务集各跑一轮、共 6 任务。验收：门禁 rc=0；6 任务全通过；长文任务另至少 3 次候选版本独立同预算运行（不复用修复前基线结果）、全部结果报告、≥2/3 通过。不得靠加预算、改验收或任务特判达成。
  权威文件：任务集 `evals/tasks/coding/`、`evals/tasks/workflows/`；配置 `evals/configs/mio-deepseek.json`、`evals/configs/mio-deepseek-workflows.json`；运行器 `scripts/agent_eval.py`；本地证据 `target/evals/iteration-02/`（被 `.gitignore` 忽略，不随库分发）。
- [ ] 大仓库检索选型（REQ-CONTEXT）
  要做什么：在上面的评测基线上对比 repo map、按需 LSP 与符号索引的准确率、延迟与 Token 成本，先测量再定型。
  验收条件：选型有对照数据支撑，明显减少大仓库无效 `grep` / `read` 轮次。
  边界：当前证据只覆盖单个约 61 KiB 长文件；不得据此宣称大仓库能力或竞品优劣。
- [ ] 同条件跨 Agent 对照（REQ-EVAL）
  要做什么：在固定模型、thinking 与 Token/时间预算下对比 Mio 与主流 Agent，生成正确性、成本、延迟、缓存、取消清理与恢复报告。
  验收条件：报告可复现并保留失败样本与原始配置；调用预算未对齐时明确标注，不以不同模型或不同预算的分数宣称领先。
  现状：已有 pi / reasonix / Mio 在 `mio-python-acceptance-v1` 上的旧 3 任务同模型单轮比较（`target/evals/compare-pi-reasonix/REPORT.md`，本地快照），但三方调用上限不一致（Mio 有 `--max-model-calls 20`，pi / reasonix 无等价开关），不能宣称优劣；公共 benchmark（`evals/adapters/harbor/`）未接入。新增 `mio-deepseek-comparison.json` / `pi-deepseek-comparison.json` 配置已就绪（`deepseek-flash`、off、temperature 0、8192 输出上限、180s、双方无调用上限），已通过 `tests/pi_sampling_wire.py` 实际安装版 Pi 的 loopback 请求验收，确认 temperature 0 与 max_tokens 8192 确实发送；尚需冻结当前 Mio 二进制并执行、归档真实对照，不能并入当前 20 次调用基线。
- [ ] 可插拔 OS 沙箱（REQ-ISOLATE）
  要做什么：定义可替换执行环境 Backend（Linux bubblewrap / landlock、macOS seatbelt 等），与 Git worktree 修改隔离解耦。
  验收条件：Backend 状态可查询，无 Backend 时行为显式；越界读写、网络与资源超限按平台阻断并验证；worktree 只做修改隔离，不充当安全边界。
  进展：可组合 Backend 与 Linux bubblewrap 基础已实现，17 项真实沙箱用例通过，含显式挂载读取隔离、工作区外写拒绝、网络隔离及每进程资源上限。CLI 状态与参数已接入，真实 CLI 隔离与 unrestricted 对照共 4 项通过；Shell / 插件 / MCP / 子代理路由另有 11 项通过。已增加强制真实 bubblewrap 的独立 CI 作业（尚未远程执行）。仍待整合门禁及自定义 Host / extra_tools 的隔离拒绝契约；macOS / Windows 尚未实现沙箱。
- [ ] CLI 拆分、SDK 契约与分发（REQ-SDK / REQ-DISTRIBUTION，AUD-17/20）
  要做什么：按用户要求保持 pi 式简洁终端对话、`-p`、管道及 JSON/RPC，不增加 GUI、全屏界面或装饰性面板；保持已拆出的参数、终端、RPC、审批、Runner 模块边界；复用现有 hook/bus/observe，稳定三层公共 API 与依赖方向检查；补齐 helper crate feature / 平台矩阵与 crates.io 发布策略。
  验收条件：最小与完整宿主示例可编译，Linux / macOS / Windows 安装升级与哈希校验可核对，失败不污染已有安装；依赖闭包与许可证来源可审核。
  进展：五个 CLI 模块已拆出，`mio` 使用简洁终端提示符，无装饰性面板或全屏界面；PTY 三项及状态/取消回归通过。CLI 与 Rust SDK 的真实 loopback read→write→done 对照已通过（`/tmp/mio-fs-capability-sdk-parity-qa/`）：两端各 3 次派发、结果字节一致、调用 ID 往返和独立 run UUID 正确；不代表真实 Ravomi 宿主。SDK 与分发部分仍未完成。
  SDK 进展：实际依赖图方向检查、路径依赖发布版本与打包排除规则已收口；26 组本机示例/feature 编译通过，`tests/sdk_contract.py` 30 项通过。新增独立 Linux/macOS/Windows CI 矩阵，后两平台尚无运行证据；全 feature 非 dev 依赖的原始声明已打包：316 个包、215 份去重原文，绑定当前 lockfile，`bundle-licenses.py --verify` 与 7 项回归通过。开发依赖正文、法律义务审查与实际发布仍未完成。

## 六、阶段四（远期规划）

- [ ] 宿主与产品形态（REQ-PRODUCT）
  要做什么：围绕独立终端 CLI 与 Ravomi 内嵌宿主两条主线验证真实任务；ACP/HTTP、会话树、Plan 模式与后台守护作业在具备真实需求证据后各自独立迭代。
  验收条件：每个形态有需求证据与独立可回退边界，且不污染 `mio-core` 与 `mio-agent` 内核。
  已确定：维护者选择 CLI 与 Ravomi 为共同首要入口；CLI 保持 pi 式简洁终端交互。第三方 SDK 保持可集成，外围形态尚无实施需求，不提前扩展。

## 七、验证与边界

提交前必须通过的离线门禁（CI 锁定 Rust 1.95.0）：

```bash
cargo +1.95.0 fmt --check
cargo +1.95.0 check --all-targets --locked
cargo +1.95.0 test --locked
cargo +1.95.0 clippy --all-targets --locked -- -D warnings
python3 -B tests/architecture.py
python3 -B scripts/check-architecture.py
python3 scripts/check-site.py
python3 -B tests/agent_eval_harness.py
python3 -B tests/agent_eval_comparison.py
python3 -B tests/sdk_contract.py
python3 scripts/check-sdk.py --strict-publish
node tests/pi_fixed_sampling_extension.mjs
python3 -B tests/agent_workflow_tasks.py
python3 tests/osc7501.py
python3 tests/cli_cancellation.py
python3 -B tests/cli_terminal.py
python3 -B tests/crash_recovery.py --binary target/debug/mio
```

### 评测支线复现与隔离约定

- **起手命令**：见 [PRD 第五节](../../PRD.md#五评测基线与可提交证据)；先运行 `python3 -B tests/agent_eval_harness.py` 与 `python3 -B tests/agent_workflow_tasks.py`。随后冻结二进制并校验 SHA，将工作流配置的 `agent_command` 指向该快照，使用 `--task retrieve-wire-spec` 聚焦失败任务（会调用真实模型）；三次运行分别指定独立报告路径，保留全部结果。完整任务集命令见 PRD。
- **密钥**：只从环境变量（`DEEPSEEK_API_KEY`）读取，不写入配置、命令、报告或文档；报告落盘前把密钥类参数替换为 `[redacted]`。
- **配额隔离**：每个任务用临时 `{cwd}/../coordination`，预算/冷却账本不跨任务、不串入用户会话；需要独立预算周期时换新的 coordination 目录；预算版本仅是配置记录，修改版本字段不会重置持久账本。
- **失败保留**：长文任务的失败报告、逐事件诊断与首次运行结果原样保留，不覆盖、不删改；改进后与之对照。
- **重型任务与 pi 委派**：执行约定见 `AGENTS.md`；评测支线不改默认模型、Provider 或全局委派配置。

补充验收清单（按主题并入上述对应条目，不单独勾选）：

- 会话扫描：已改为扫描超限时拒绝自动选择最新会话，允许显式路径；本轮超限目录用例已通过（AUD-11）。
- 重任务配额：补测持续资源竞争与取消，确认轻任务不被挤占；未接入 dispatch 的第三方工具与直接启动进程不保证受配额约束（REQ-QUOTA）。
- 进程组回收：直接子进程消失且 `waitpid` 返回 ECHILD 的回归用例已通过；后代进程回收与跨平台仍须独立验证（AUD-01）。
- 正则：线性 NFA 只有小规模复杂度探针，未做压力测试与取消后 CPU 回收验收（AUD-06）。
- 符号链接：插件路径拒绝用合成文件复现，静态拒绝不能证明抗竞态（AUD-07）。
- 去重账本：CLI/RPC 真实 SIGKILL 后 key 不重放已通过；跨进程竞争仅在 CLI 实测，RPC 跨进程互通、跨平台与硬件断电未单独实测。
- 门禁范围：上述历史门禁只覆盖本地 mock / loopback 与离线工具，不代表全入口门禁、真实 Provider、跨平台或持续并发压力。

诚实边界：

- 默认无 OS 隔离：调度、路径租约、工具过滤与 `--approve` 都是应用层协作准入。Linux 可选 bubblewrap 已有真实子进程隔离证据；宿主扩展拒绝契约仍在整合，macOS / Windows 尚未提供 OS 沙箱。每进程 rlimit 不保证进程树总资源上限。
- 无美元硬限额：`--max-model-calls` 限制调用次数，`--max-total-tokens` 采用保守预留与真实 usage 结算，可能发生超预留，不做价格换算；跨进程预算已通过本轮进程级验收；硬件断电与跨平台仍未验证。
- 取消不回滚：Ctrl-C / `abort` / 超时只停止后续推理并终止子进程组，保留上次成功落盘的会话历史，不回滚已落盘的文件修改。
- 离线契约不代替线上验证：本地 mock 与多进程测试通过，不等于真实 Provider 线上认证、跨平台环境或极端硬件断电已验证。
- 评测样本小：当前 6 任务（`coding` 3 + `workflows` 3）仅各一轮运行，长文任务仍失败；是自有验收探针，不是行业 benchmark，不代表稳定性、跨厂商或领先。
- 证据位置：原始报告与二进制在 `target/evals/iteration-02/`，该目录被 `.gitignore` 忽略、不随仓库分发；持久摘要见 PRD 第五节与本文件；本轮文档核对不代表重新运行历史门禁或模型评测。
