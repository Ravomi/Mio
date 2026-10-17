# Mio v1 设计规格

> **2026-10-17 状态**：DES-01—DES-22 契约已确定，核心实现已整合。本机 Linux 冻结二进制 `f7873aaf…` 通过原预算固定六任务 6/6、独立长文 3/3，四项 Rust 门禁、SDK 27/27、严格 Linux 42 项、SIGKILL 12 项、检查点崩溃 5 项和 Ravomi 解析器 7/7；见[本轮验收](reviews/2026-10-17-v1-inline8k-acceptance.md)。这是本地 v1 门槛验收，尚未提交或发布；历史 `3c196d26…` 失败见[前轮记录](reviews/2026-10-17-v1-live-acceptance.md)，跨平台与完整 GUI 宿主仍待验。

> 本文把 [PRD](../PRD.md) 与 [TODO](../TODO.md) 的路线收敛为可直接实施的设计。它不是路线图复述，也不新增对外承诺。
> 相关文档：[需求与竞品调查](agent-requirements-and-comparison.md) · [RPC 协议](rpc.md) · [审查记录](reviews/2026-10-15-audit.md) · [阶段二验收](reviews/2026-10-15-phase2-acceptance.md)

## 0. 状态标签与阅读约定

每条设计决策用稳定编号 `DES-NN`，覆盖既有 `REQ-*`，编号一经发布不再改义。设计决策已确定；以下标签分别描述规范、实现事实与验收范围：

- **【事实】**：仓库中已有实现，且有离线或本地运行证据（给出文件/测试）。
- **【v1 契约】**：已确定的规范与验收要求，不再表示待选方案。标签本身不证明实现完成；实现与证据须另行标注。
- **【待验收】**：主路径已实现，但缺少端到端/跨进程/跨平台证据；不计入 v1 完成。

三条全局底线贯穿全文：不宣称所有上下文都可无损压缩；不把 token 计数描述成美元硬限额或 OS 安全边界；不通过加预算、放宽验收或任务特判改写历史 5/6 结果。新冻结版的 6/6 单独记录。

---

## 1. v1 范围与不做项

### DES-01 v1 范围（覆盖 REQ-POLICY / PROCESS / SCHEDULE / BUDGET / DEDUP / RECOVERY / EVENTS / CONTEXT / EXTENSIONS / SDK / ISOLATE）

v1 是"把已落地防线收敛为全入口一致契约"的版本，交付四件事：

1. 所有工具入口（内置、`workspace` 聚合、插件、MCP、`extra_tools`、子代理）走同一准入决策，审批绑定规范化参数。
2. 任务/工具两级状态机有唯一权威定义，取消与崩溃后"未知副作用"永不被自动重放。
3. 上下文按三档分级保留，压缩产生可读损失报告，长输出以引用+分页检索替代无限内联。
4. 事件带 schema 版本与全链路关联 ID，慢消费者有界背压且旧客户端不被静默破坏。

v1 不引入新的模型能力、不做产品形态扩展。

### DES-02 不做项与明确延期（避免范围蔓延）

- 延期到 v2 之后：ACP/HTTP server、交互式会话树、显式 Plan 模式、长期后台守护作业（REQ-PRODUCT）。这些功能均需真实需求证据后独立可回退迭代。
- 延期：repo map / LSP / 符号索引选型（REQ-CONTEXT）、同条件跨 Agent 对照与公共 benchmark 接入（REQ-EVAL）。v1 只做"引用+分页+分级保留"，检索选型必须等对照数据。
- v1 不做：精确费用硬限额与价格换算；始终禁止证据不明确时的自动副作用重放；把 worktree/路径租约当作 OS 安全边界。
- v1 维持既有边界不变：`--max-model-calls` 仍是调用次数硬上限；`--max-total-tokens` 仍是"调用前保守预留 + 按真实 usage 结算"的计数预算，超预留记录 overage 并阻断后续；无美元换算。【事实：PRD 第一节、`src/shared_limits.rs`、`tests/shared_budget.rs`】

---

## 2. 入口与同核

### DES-03 CLI 与 Ravomi 同核边界（REQ-SDK / REQ-PRODUCT）

- **【事实】** `mio-coding-agent` 同时服务 CLI 与 Rust 宿主：`run` / `run_with_options` / `run_with_tools` 与 `Host` trait 是共享入口，CLI 只是 `Host` 的一个实现（`src/lib.rs`，`examples/host.rs`）。
- **【v1 契约】** v1 固化"同核"判据：CLI 与 Ravomi 宿主在任何工具准入、状态机、事件 schema、预算与恢复语义上不得出现分支差异。允许差异为终端渲染、审批交互形式（TTY / 非 TTY / RPC）、持久化默认值，以及宿主显式传入的模型/工具集合/限额/backend 配置值；相同配置下准入与执行语义必须一致。
- **【v1 契约】** 差异清单必须可机检：新增 CLI 参数若改变引擎语义，必须同时落到 `RunOptions`/`RunTools` 或 `ToolPolicy`，禁止只在 `src/bin/mio_cli/` 内实现语义。
- **【事实】** 五个 CLI 模块已拆出（`args`、`terminal`、`rpc`、`approval`、`runner`），CLI 当时为 pi 式简洁终端；后续交互模式改为 inline TUI，见 `docs/tui-interaction-redesign.md`。
- **【宿主产品接入待验】** Ravomi 侧真实任务的同核一致性尚无对照证据。Mio v1 引擎验收使用公共 SDK、离线完整宿主与 CLI 语义检查；完整 Ravomi 产品接入属于后续宿主验收，不用解析器或 mock 成功替代。

---

## 3. 任务与工具状态机

### DES-04 任务状态机（REQ-SCHEDULE / REQ-DEDUP / REQ-RECOVERY）

- **【事实】** 子代理已有明确状态枚举：`Queued → Running → Completed/Failed/Cancelled/TimedOut`（`src/subagent.rs`），FIFO 排队、整工作区 writer 独占、只读并行。
- **【事实】** 去重账本对活动/排队/全部终态都不重放，返回原 `task_id`；强杀后记录可能停在 active/queued（`src/dedup.rs`、`tests/cli.rs`、`tests/rpc.rs`）。
- **【已实现，主树回归通过】** 账本可选记录 owner，`TaskKeyLedger::observe` 在账本锁内返回 `live/stale/unknown`。Linux 结合 host、boot、PID namespace 与进程启动时间判断；跨 host/namespace、旧记录或不可读时保持 unknown。观测不修改任务状态、不删除记录、不自动重放；非 Linux 不推断死亡。参见 `tests/dedup_liveness.rs`。
- **【v1 契约】** v1 把 `Queued/Running/Finished` 提升为**所有任务（含 CLI 单次任务与 RPC 回合）的权威状态**，终态为 `Completed/Failed/Cancelled/TimedOut`，恢复观测另记 `Interrupted`；"deduplicated" 是响应状态而非任务状态，不得占用状态机位置。
- **【v1 契约】** 状态转移规则：`Queued → Running` 仅在取得真实调度租约后发生；准入前准备失败允许 `Queued → Failed`，不伪造 Running。`Running → TimedOut` 必须在实际 worker / 进程回收后提交；终态不回退或互相替换。排队取消可以直接 `Cancelled`；运行中的取消请求先记录请求，待所有实际 worker 退出才提交终态，单个工作区租约释放不代表整个任务结束。
- **【v1 契约】** `Completed` 表示运行正常结束、实际 worker 已收回，并不表示业务目标或产物已经独立验证。模型可以如实报告未完成或未验证后正常结束；宿主与评测须单独记录目标验收结果，不得由退出码0或该终态推导正确完成。R14真实批次已有Agent0但产物验收1的反例。
- **【主树验收通过】** `task_lifecycle.rs` 的六态、真实许可与 worker guard 已接入 CLI/RPC/SDK/子代理；journal 与 dedup 仅投影权威记录，不反推终态。主树四项 Rust 门禁通过，默认并行 3111 passed / 0 failed / 6 既有 ignored；排队回合从入队起持有唯一句柄，执行与取消复用该身份。
- **【v1 契约】** 强杀遗留的 active/queued 记录不自动转为失败，读取时默认标注 `liveness: unknown`，仍阻止重放；仅在持有同一根目录租约并确认原 owner 已死亡后标注 `stale`，不能只凭记录时间判断。

### DES-05 工具调用状态机（REQ-PROCESS / REQ-ACCEPT / REQ-EXTENSIONS）

- **【已实现，定向验收通过】** `tool_lifecycle.rs` 定义权威调用状态；journal v2 保存 canonical calls，兼容只读 v1 及旧 operations，不给旧记录伪造阶段或身份。512 KiB/64 run/512 call 的持久化上限与丢失计数仍须保守处理。
- 状态为 `Normalized → Admitted → Dispatched → Executed | Failed | Unknown`；过滤或审批拒绝为 `Denied`，运行结束前未派发为 `NotDispatched`。`Admitted` 表示准入通过，不代表用户做过交互审批。
  - `Denied` 发生于派发前，零工具副作用；`Dispatched` 必须在持久化成功后发布。持久化失败阻止后续派发。
  - 中断遗留 `Dispatched` 转 `Unknown`，`Normalized/Admitted` 转 `NotDispatched`；已有终态不回退。中断本身不证明零副作用。
  - 宿主持久化回调在 tracker 状态锁之外执行，可查询已提交快照；同调用串行提交，不同调用独立提交。参数 SHA-256 与审批绑定一致，扩展参数正文不进入 canonical target。
  - 非法转移、回调快照、真实 RPC 逆序关联及隐私用例已通过；证据为 `target/evals/design-implementation-20261015/canonical-integration-qa/`。当前全量整合门禁另行记录。
- **【v1 契约】** 同一 `call_id` 在同一 `scope` 内只允许一次 `Dispatched`；重复到达视为协议错误并拒绝，不重放。
- **【已实现，主树回归通过】** `ToolPolicy` 对真实 Provider 调用 ID 做每 run 原子认领，克隆共享同一表；最多 4096 条 SHA-256 摘要，不驱逐旧项，满员即拒绝新 ID。ID 限 512 字节且不得为空或含控制字符。拒绝/取消/超时均不释放已认领 ID，新尝试须使用新 ID；无 Provider ID 的直接 SDK `admit` 不伪造身份，也不声称具备该去重保证。它是运行期防重，不代替持久 journal 的状态机。
- **【事实】** `tools/call` 传输失败不重放、结果标记副作用未知；`tools/list` 允许重连一次（`src/mcp.rs`、`tests/mcp_lifecycle.rs`）。

---

## 4. 统一授权网关

### DES-06 单点准入（REQ-POLICY / REQ-PROCESS）

- **【事实】** `ToolPolicy` + `PolicyHook` 已是统一准入点：`EntryKind`（builtin/aggregate/plugin/mcp/extra/subagent）、`SideEffect`（read/write/external）、别名 deny 优先、`NormalizedCall.params_sha256`（`src/policy.rs`、`tests/tool_policy.rs`、`tests/tool_policy_extensions.rs`）。
- **【事实】** `extra_tools`（宿主注入、无声明副作用）按保守策略处理；子代理委派权限只收窄，不再派生。
- **【v1 契约】** v1 收口判据：任何工具执行路径都必须先产生 `NormalizedCall`；未经过 `ToolPolicy` 的调用属于缺陷，而非"未接入的例外"。拒绝与超时路径必须证明"零未授权副作用"。
- **【主树回归通过】** `tests/tool_policy_extensions.rs` 的实际插件/MCP 路由、拒绝零副作用、参数绑定与子代理继承用例已纳入主树全量门禁；R13 原始测试日志确认执行。该证据不等于所有第三方服务线上验收。

### DES-07 审批参数绑定（REQ-POLICY）

- **【事实】** 审批绑定已带 `entry / tool / operation / cwd / scope(run UUID) / paramsSha256 / callId`，序列化为 camelCase；RPC 审批请求携带同一 `binding`，MCP 服务器启动授权等不经过工具 dispatch 的场景 `binding` 为 null（`Host::approve_tool_binding`，`src/policy.rs`，`docs/rpc.md`）。
- **【事实】** 单次审批只释放对应 `request_id` 的单次调用；过期/重复/未知 ID 返回 `success:false`；120s 单调时钟超时；pending 上限 64。
- **【v1 契约】** v1 明确三条绑定不变量：
  1. 审批对象是 `(scope, callId, paramsSha256)` 三元组，任一项变化必须重新审批。
  2. `paramsSha256` 覆盖入口、规范工具身份、参数与 cwd；摘要算法或覆盖范围变更属破坏性事件，必须升 schema 版本（DES-15）。
  3. 审批通过不构成参数不可变保证：dispatch 前需再次校验摘要，不一致则拒绝并记 `Denied`。
- **【v1 契约】** 兼容方向保持 `approve_tool_binding → approve_tool_async → approve_tool`；新宿主可覆盖绑定版本，旧宿主无需改签名。不能反向互相调用形成递归。

---

## 5. 预算、重试、取消与恢复

### DES-08 预算语义（REQ-BUDGET）

- **【工程验收通过，模型门槛未过】** 有限模型调用预算向模型提供有界的只读运行时快照，取本地与共享已知有限余量的较小值；说明快照在准入前取得、包含本次调用且受并发影响，不构成配额保证或工具授权。共享状态不可用时保持 unknown，无有限调用配额时不增加提示。提示实际编码成本在 request fit 前计入，完整 dispatch 再检查；不修改持久历史或损失报告的源索引，不提高硬上限、不自动判成功；提示文本建议保留验证和最终响应所需调用，末槽请求策略单独说明。

- **【R11 工程通过，模型完整门槛未过】** 调用快照可按真实已知最小余量给出阶段建议：3—5次收敛必要修改并预留验证/回答，2次做必要验证并准备回答，1次如实报告完成/未完成/未验证状态，0次不保证下一调用；unknown保持通用建议。提示仍≤256字节并按实际成本计费，不改变工具定义、tool_choice、准入、终态或调用上限。模型忽略提示仍可触发真实预算失败，不能自动改为成功。

- **【R13 工程通过，模型完整门槛未过】** 临时预算note在prepared completion的dispatch边界放到完整当前工具结果及prompt之后，其他消息相对顺序、原文、配对、阶段文案与额度不变。已有note原样移动，不重读quota；Source历史及持久会话不加入note，实际成本仍照常支付。Provider既有System消息折叠/映射保持，因此不保证所有wire都尾置。受控两版CLI对照可比较稳定消息前缀，但不把前缀字节数当缓存命中、费用或模型质量证据。无操作路径不得复制整段历史，实际重排只复制一次请求。

- **【R14-reviewed 工程通过，模型完整门槛未过】** 末槽文本答复策略只在已知有效minimum=1、本地已有调用、请求含先前assistant/当前工具序列和可用工具、原batch预算守护接线、caller选择为默认/Auto时，将本次真实API请求的tool_choice设为None。初次单槽、unknown/0/>=2、无工具、明确Required/Specific/None及raw tool_choice保持caller选择。快照仍不保证并发可用，实际准入可失败；模型须真实返回文本，真实调用仍计入原上限，产物正确性由原独立验收判断。Provider忽略选择继续返回ToolCalls时，原调用与硬失败证据保持；不生成虚构答复或清除失败。该策略可需额外一次只读quota快照，不把它称作跨进程独占配额预留。 无batch预算误应用已通过先红后绿回归修正；原预算真实六任务5/6、独立长文1/3，三个失败均正常结束但产物验收失败。文本答复与正常退出不能替代正确性验收。

- **【事实】** 进程内预留/结算/overage/阻断、跨进程 `budget.json` 原子账本（`flock`）、损坏 fail-closed、崩溃遗留预留不自动退款（`src/coordinator.rs`、`src/shared_limits.rs`、`tests/shared_budget.rs`）。
- **【v1 契约】** v1 保持既有诊断与限额语义：共享准入及聚合调用/token 预算拒绝使用 `scheduling`，引擎独立回合上限使用 `max_turns`，上下文装不下使用 `context_budget`。可查询预算快照区分活动预留、已用量、超预留量及总超额；RPC 本地快照沿用 camelCase，共享账本快照沿用 `tokens_reserved_active`、`tokens_used`、`tokens_over_reservation`、`tokens_overage`。字段与现有接口见 [RPC 预算状态](rpc.md)，不扩大限额。
- **【v1 契约】** usage 缺失时保持 `null`/`unknown`，按预留结算；任何字节估算都不得冒充账单。这条是 PRD 既有承诺，v1 不修改。

### DES-09 重试与"未知副作用"恢复（REQ-RECOVERY / REQ-PROCESS）

- **【v1 契约】** v1 重试规则按副作用分类，禁止一刀切：
  - `SideEffect::Read`：最多自动重试一次，且仅对明确可重试的读错误；模型重试另受共享冷却与调用预算约束。工具重试计入工具事件，不能冒充模型 `completion_call`。
  - `SideEffect::Write`：仅当证据为 `Denied` 或明确"未进入 `Dispatched`"时可重试；`Unknown`/`Interrupted` 一律不自动重试。
  - `SideEffect::External`（插件/MCP）：不自动重试；`Unknown` 后必须先由宿主或用户确认现场，再以新调用 ID 重试；不能由模型自行绕过未知状态重放。
- **【事实】** 429 冷却按 `endpoint + 凭据摘要` 共享，父/子代理/协作进程可见，且不自动重放（`src/scheduling.rs`、`src/shared_limits.rs`）。
- **【已实现，主树验收通过】** 持久未知账本的覆盖初始化必须与全部旧 journal 迁移在同一锁内提交；文件存在不等于迁移完整。迁移丢失、损坏或超界形成不可用普通确认清除的 incomplete 标记。覆盖建立后，正常 journal 留存裁剪不阻断后续回合；纯读不登记写副作用。早期隔离候选的定向测试后，已回迁主树并通过四项门禁及 12 项真实 SIGKILL 测试。
- **【主树整合验收通过】** 运行中未知事实在询问批准前唯一解析为真实持久 key/snapshot，已知 engine scope 必须一致；缺失、歧义、覆盖不完整及等待期间证据变化均拒绝，不在批准后重新抓取证据。原生 timeout/wait/capture 丢失结果使用类型化 Unknown，未启动失败与已确认非零退出保持可区分。真实 CLI/RPC 已验证原生 8 MiB 捕获丢失后的未知、独立拒绝/确认、只读继续可用和新 ID 恰好一次写入；SIGKILL 12 项通过。Wait loss 仅有类型映射测试，没有可移植的真实 wait(2) 失败注入；不将其记为实测。主树验证见本页链接的验收记录。

- **【主树验收通过】** Linux/glibc 的真实 SIGKILL 专项已由早期 10 项扩至 12 项；`recoveryView` 与 `unfinished_goals` 的 `tests/recovery_view.rs` 已纳入主树门禁，R13 日志确认执行。SIGKILL 专项绑定主树整合冻结版，不能替代硬件断电或跨平台证据。

### DES-10 取消语义（REQ-PROCESS）

- **【事实】** 取消停止后续推理、终止进程组、清空 steer/follow_up、取消未决审批；不回滚已落盘文件修改、保留上次成功保存的会话（CLI/RPC 一致）。
- **【v1 契约】** v1 固化取消的完成条件（可判定）：取消应答只确认接收；终止完成必须满足"不再派发新副作用 + 子进程监督完成 + pending 审批归零"，并以事件序列顺序验证。取消前已派发的阻塞磁盘 worker 可能继续完成，必须保留其租约到结束，不能承诺已发生的写入回滚。非 Unix 宿主须如实报告进程树回收能力。
- **【v1 契约】** 取消后写入 journal 的运行终态与工具结果分别记录 `cancelled/interrupted` 与 `unknown`，不得统一写成成功。

---

## 6. 上下文：分级保留、引用检索与损失报告

### DES-11 三级保留策略（REQ-CONTEXT / REQ-CONTEXT-FIDELITY）

- **【工程保真已验收，模型门槛未过】** 工具结果续轮触发自动 request fit 时，最近纯文本用户请求若被裁入摘要，应改为完整保留并按实际序列化成本计入预算；装不下则明确拒绝。新用户请求本身已经保留，不因此固定过时任务。工具结果与混合消息不按纯文本请求提取；先前已压缩掉的内容不能恢复。此修复不提高任何限额；最终工程版四门禁 3119 项通过，模型六任务 5/6、长文 0/3，后续元数据修订须另验。

- **【事实】** 已有两个层次：逐轮请求预算（`BudgetHook::fit`，不破坏持久化历史）与会话级 `compact`（更新历史，512 KiB 自动阈值），请求级摘要上限 4096 字节（不含外部消息封装）；会话级摘要另有自身策略（`src/context_budget.rs`、`src/compact.rs`）。
- **【v1 契约】** v1 定义三档，语义固定、不可互相替代：
  - **Pinned（钉住）**：系统指令与工具 schema、工具续轮受保护的当前纯文本用户请求、当前轮工具调用/结果、宿主显式登记的约束与未完目标。不参与摘要，只能整体保留或明确报错。
  - **Summarizable（可摘要）**：早期对话与已完成工具序列，可被抽取摘要替换。
  - **Referenceable（可引用）**：外置长输出，正文不进入上下文，只保留引用、行/字节范围与总量。
- **【v1 契约】** Pinned 判定必须可测：约束一旦被识别为 Pinned，压缩前后必须逐条可比对。v1 禁止从自然语言关键词推断强约束：显式登记是目标 API 的结构化条目（稳定 ID + 原文 + 状态），仅宿主可登记/解除；受保护当前请求以外、未登记的历史对话仍用有损摘要，必须报告丢失风险。登记项无法容纳时拒绝请求，不裁剪为成功。
- **【已实现，主树回归通过】** `context_memory::PinnedContext`、`RunOptions.pinned_context`、会话持久化、CLI `/pin` 与 RPC `set_pinned_context` 已接入。注册表中存在即为有效条目，解除由宿主显式删除，模型不能自行完成或删除目标。限制为 64 条 / ID 64 字节 / 单条 8 KiB / 总正文 32 KiB，反序列化同样验证；完整条目计入固定指令成本，放不下拒绝请求。

- **【R7 工程通过，模型完整门槛未过】** 成功工具结果可从通用 Markdown 标点选择有界中部摘录，按实际记录空间裁剪并转义；仅精确单键字符串 `output` 信封可解一层，无结构时保留旧序列化表示，未知字段不丢弃。每记录头尾与原文片段总存储 ≤2048 字符，转义中段 ≤768 字符，摘要总预算不变。该片段始终为不可信的 Summarizable 工具数据，不推断权限或 Pinned；结构化输出的损失使用诚实通用标记，失败结果保留旧尾部策略。R7 四门禁与真实出站离线探针已过，真实模型本轮六任务 6/6、独立长文 0/3，完整门槛未过。

- **【R10 工程通过，模型完整门槛未过】** 在同一摘要总预算内，已返回的工具证据优先于操作请求描述和 assistant 自述；用户请求及失败证据仍保持更高配额。当前段权重为用户5、失败4、返回3、自述2、请求1，只影响有损摘要的空间分配，不决定信任或权限。请求来源、真实调用ID、requested/returned 区别及明确省略保留；更多操作记录可能被省略，不能据摘要推断未执行。

### DES-12 引用检索（REQ-CONTEXT-RETRIEVAL / REQ-EXTENSIONS）

- **【当前实现】** 8 KiB 以上输出外置到 `.mio/outputs/<uuid>.<stream>`，`read` 支持 `byte_offset`/`byte_limit`（4–65536 字节）并返回 `next_byte_offset`/`total_bytes`（`src/shell.rs`、`src/file_page.rs`、`tests/output_paging.rs`）。历史冻结版本使用 32 KiB 阈值，历史验收记录不随当前值改写。
- **【v1 契约】** v1 把"引用"升为上下文一等公民：引用对象至少包含 `path`、`total_bytes`、`stream`、产生该输出的 `call_id`。模型按引用分页检索，引擎不自动把外置正文重新内联。
- **【v1 契约】** 内联输出预算覆盖整批工具结果，而非每条分别占满同一份额度：保留当前 assistant 调用与结果配对后，按实际编码成本分配剩余空间，超出部分外置并保留完整引用。`read`、Shell、grep/find/ls、插件与 MCP 均适用，固定 8 KiB 阈值不能替代请求预算；多调用批次不能把单条上限乘以调用数。固定指令、调用身份及引用本身都无法容纳时应在新增副作用前拒绝，不能执行后再假称结果完整。该规则不扩大模型调用数或上下文窗口，也不允许丢弃当前工具配对。
- **【R8 工程通过，模型完整门槛未过】** 历史摘要与整批工具外置应组合规划：仅在原始请求无法容纳时，以纯内存的最小 typed 引用成本寻找安全摘要切点，摘要仍从原始内容抽取。规划用引用不得进入模型、journal 或持久会话；整批候选上限须在快速接受和候选切点上精确复核。实际引用只由既有 dispatch 事务发布，最终实际请求再次检查同一预算。当前用户原文、System/Pinned、当前工具配对保持原规则；不可约或审计失败继续拒绝。红绿端到端回归已证明该组合可在原 27033 输入估计内服务旧路径拒绝的请求，不声称它解释了全部真实模型失败。
- **【主树回归通过】** `tests/output_lifecycle.rs::production_shell_output_flows_through_index_and_byte_paging` 已从真实 shell 生成输出、登记引用，再使用只读 `read` 分页重建全部 100000 字节；`tests/output_paging.rs` 另覆盖大段 UTF-8 单行重建。此证据不等于真实模型一定会正确使用引用。

- **【R12 工程通过，模型完整门槛未过】** 原生与聚合 `grep` 可选择 `context_lines`（0—20，默认0）；省略或0沿用既有输出。命中行与邻近非命中行必须明确区分，真实行号、相交窗口去重；命中优先，邻近行计入同一64KiB总输出与既有单行/文件/遍历限额，500条匹配上限只计真实匹配。无命中不输出孤立上下文，截断明确，权限/固定目录句柄边界不变。请求摘要中的context_lines只表示请求，不推算实际返回范围。

- **【R15 工程通过；后续真实模型完整批次仍未达标】** 直接摘要路径在原首个可用安全切点及原文后缀不变时，可复用源计划额外至多5次渲染/实际编码成本复核，尝试利用原7/8估算和折半留下的空间。只选择实际装得下的候选，保留旧候选；无法编码则放弃该次细化。摘要正文上限仍4096字节，wrapper/编码开销照常付费，最终dispatch继续检查整批预算；组合外置规划沿用原策略。合成用例已证明两句完整原文的保留改善，但更多字节或记录数不等于语义支配，也未证明真实失败根因；普通损失仍报告，System/Pinned/当前工具配对/最近用户原文保持原契约。

### DES-13 损失报告（REQ-CONTEXT-FIDELITY）

- **【v1 契约】** v1 **不承诺无损压缩**。每次摘要必须产出一份结构化损失报告，字段至少：被替换的消息范围、源记录与摘要的编码字节数（不把差值称为精确语义损失）、受影响的工具调用 ID、Pinned 项是否完整、被截断的摘要段落。
- **【v1 契约】** 报告可在事件与恢复视图中查询；缺少报告视为摘要未完成，不允许静默降级。
- **【事实】** 本轮已实现分类配额、头尾摘录、调用 ID 与结构化 `CompactionLossReport`；请求与自动压缩经宿主回调/事件发布，CLI journal 在请求前持久化，失败则阻断；恢复查询支持有界审计列表。显式 `Session::compact` 把最近报告随会话保存。字节数描述源记录和摘要，不伪装成精确语义损失量；普通内容仍有损。集成回归已纳入主树门禁，逐版范围见主树验收记录。

---

## 7. 事件协议

### DES-14 schema 版本与能力协商（REQ-EVENTS）

- **【事实】** legacy 事件尚无统一 schema 封套；RPC 事件为 `response/text/tool_started/tool_finished/done/agent_settled/approval_request/completion_call/diagnostic`，`agent_settled.status` 为 `completed|cancelled|error|deduplicated`（`docs/rpc.md`）。
- **【事实】** 本轮新增 `get_state.data.protocol` 发现文档（`discoveryVersion:1`、`eventEnvelope:legacy`）；它不等于全事件版本化，RPC 集成 28 项已通过，其中覆盖空闲/运行中查询不增加模型请求；完整门禁另行记录。
- **【v1 契约】** v1 为 RPC 出站 JSON 引入 `schemaVersion`（整数，单调递增）与一次启动期能力协商；Rust `Host::on_event` 保持既有类型签名，关联通过独立的默认观察回调提供。RPC 客户端通过新增 `initialize` 命令声明支持的版本集合与可选能力，引擎选择双方版本交集中的最高版本；不发送 initialize 的旧客户端继续 legacy，交集为空则拒绝本次协商且不得启动新版任务，不静默裁剪字段。
- **【v1 契约】** 兼容规则：新增可选字段不升主版本；字段改名、语义变化、枚举新增必升版本；v1 客户端必须能安全忽略未知字段。
- **【事实】** journal 已有 `JOURNAL_VERSION`，可作为版本化先例。

### DES-15 关联 ID 贯通（REQ-EVENTS / REQ-ACCEPT）

- **【事实】** `completion_call` 带 `runId`/`callIndex`，`diagnostic` 带 `runId`/`category`/`retryable`/`httpStatus`，均脱敏。
- **【已实现，主树回归通过】** `Host::on_run_observation`、审批绑定与 v1 封套已接入真实 run/turn/tool 身份；子代理另带 `subagentId/parentRunId`，保留自身 engine run UUID。审批尝试 `callId` 与 Provider `toolCallId` 分开。准入前仅记 Prepared，所有准入通过后才记 Dispatched；拒绝仍有相关联的错误 outcome，不能污染下一工具的配对。`tests/run_correlation.rs` 8 项、`tests/dispatch_observation_denial.rs` 1 项与协议 4 项已通过定向验证；已纳入主树四项门禁。
- **【v1 契约】** v1 要求 `sessionId / runId / turnId / toolCallId / subagentId` 五级 ID 在全链路适用时齐备且稳定（模型请求前的参数错误等无 run 的事件允许为 null，不能伪造 ID）：子代理事件必须携带自身 `subagentId` 与父 `runId`，工具结果必须携带产生它的 `toolCallId` 与审批 `callId`。
- **【v1 契约】** 验收判据：从任一用户可见错误出发，可仅凭事件流定位到具体请求与工具调用，无需读取原始响应体。
- **【v1 契约】** 并发配对必须由每次执行携带的实际调用上下文完成，禁止按工具名、参数相似度或完成顺序猜测。现已用每次执行的 `ToolInvocation` 与适配层 task scope 取代工具队列猜测；`tests/rpc_parallel_ids.rs` 的真实同名调用逆序完成测试已通过定向验证。`--tool-concurrency` 默认 1、范围 1–32，主树统一门禁已通过。
- **【v1 契约】** 已接入并通过主树整合验收。实现沿用现有 `ToolContext` 通道：引擎在创建每次 inbound dispatch context 时注入真实 effect/turn/Provider call 身份，适配层据此关联宿主事件与输出索引；run UUID 由上层固定上下文提供，底层不得反向依赖 `mio-coding-agent`。`PortableTool` 仍只接收业务参数，身份处理放在执行适配器，不能把 ID 塞入模型参数或全局可变的“当前工具”。直接调用 shell/工具且没有 dispatch 上下文时允许 `call_id: null`，不能伪造。

### DES-16 背压与慢消费者（REQ-EVENTS / REQ-PROCESS）

- **【v1 契约】** v1 定义显式有界背压：事件通道容量为 256 条、单事件 1 MiB、总排队负载不超过 8 MiB；流式增量可合并或丢弃并计数，不能无界缓冲。
- **【v1 契约】** 默认策略为**丢弃低价值流式增量并保留状态变更事件**，同时发出可观测的丢弃计数；`agent_settled`、审批、错误与终态事件不可丢弃。所有入队均不等待消费者；结构化事件先驱逐排队的流式增量，仍满则立即失败、停止派发、取消运行并关闭连接；断连时不能保证最终事件已送达，宿主通过进程状态和持久证据恢复，不能为保留终态无限等待。
- **【事实】** 1 MiB 协议帧与 65536 字节消息上限已存在；RPC 无显式背压验收用例。

---

## 8. 输出生命周期、清理与检查点

### DES-17 输出生命周期（REQ-EXTENSIONS / REQ-PROCESS）

- **【事实】** 外置文件在取消/失败时清理未提交产物；单流 8 MiB 硬上限使调用失败并终止进程，不返回"截断成功"。
- **【v1 契约】** v1 定义生命周期：`Pending → Committed | Abandoned`。成功捕获并发布引用才提交；失败/取消未发布的临时文件立即清理，已发布引用保持有效。未知副作用由 journal 表示，不擅自把不完整输出发布为完整证据。
- **【v1 契约】** 清理策略必须显式：默认不自动删除已发布输出；v1 提供显式离线清理的 dry-run/确认两阶段，持工作区写租约并扫描会话/journal引用，仅清理无引用且超过 7 天的已完成文件；索引损坏、活动任务或引用关系不明时拒绝清理；磁盘满时必须先失败该次调用并保留既有引用，不得删除仍被引用的文件。
- **【已实现，主树回归通过】** 生产捕获接入输出索引、事务回滚与 activity/capture 租约；显式离线清理入口为 `--output-cleanup-plan PATH`、`--output-cleanup-confirm PATH`。清理独占 activity 租约后扫描真实 Session/journal；压缩后引用证据不完整、来源未知、活动任务或陈旧计划均拒绝。`tests/output_lifecycle.rs`、`tests/output_cleanup_cli.rs` 已在主树全量门禁执行。真实调用 scope 的 `call_id` 与结构化引用已接入，缺失身份保持空值；引用/清理/真实磁盘满定向 76 项通过，canonical terminal 另以 typed ToolContext 引用补齐并通过定向回归。主树整合版全量门禁及强制真实 ENOSPC 专项已通过，版本与范围见验收记录；默认不自动清理。
- **【已实现，定向验收通过】** 引用发布意图账本收口“Host 回调 `Err` 是否已部分持久化”这一不确定性：`OutputStore` 在**文件/index 已登记、真实 Host 回调之前**写入一条 durable、版本化、有界的 `Pending` 意图（`attempt_id` 本地生成，独立于 run/tool/approval ID；记录真实 run scope、Provider `call_id` 与 typed 引用元数据，不存正文/args）。Host 成功后意图移交常规 journal/session 引用扫描，Host `Err` 保持 `PendingUncertain` 并在错误里报告真实 attempt/snapshot；未决意图在清理扫描中精确保护其引用，不会被 7 天保留期静默 GC。`reconcile_publication_intent` 是显式、snapshot 绑定的宿主对账 API（不在模型工具面内）：只有 Host 明确声明 `Published`/`Unpublished` 才注销意图，旧快照返回 `StalePlan`、缺失身份返回 `None`，从不由时间或 PID 死亡自动断言未发布。账本损坏/版本不符/超限 fail-closed；`open_readonly` 恢复读取不创建任何目录或文件。
- **【已实现，主树回归通过】** 上述账本的边界如实记录：`Published` 只是把保护移交给真实 journal/session 扫描，不冒充外部 Host 磁盘证明；没有持久 sink 的 SDK 仍保持原边界（依索引保留期回收）；对账本身不删除任何物理文件（精确回滚只发生在任何 Host 尝试之前的 `CaptureTransaction` 回滚）。`tests/output_publication.rs` 以真实 loopback、真实磁盘与真实 SIGKILL 覆盖 Host 纯失败、真实部分写 journal 后失败、先成功后失败、意图写入失败回滚、崩溃后只读恢复、旧快照拒绝与保留期保护；成功流水线不留未决意图、不 churn、不永久 pin。

### DES-18 检查点与 rewind（REQ-RECOVERY）

- **【事实】** `.mio/checkpoints/` 支持 `--checkpoints`、`--list-checkpoints`、`--rewind ID`（唯一前缀 ≥4 字符）；只在"当前内容匹配操作后指纹"时回退；索引改动在 `index.lock` 的 `flock` 内串行；不碰 Git 历史、不回滚 Shell 副作用（`src/checkpoint.rs`、`tests/checkpoint.rs`、`tests/checkpoint_cli.rs`）。
- **【v1 契约】** v1 明确检查点的定位：它是**乐观、非 CAS** 的可用性功能，不承担安全保证。非协作写入者仍存在预检与 rename 之间的窗口，必须在文档中保留该边界说明。
- **【Linux 主树验收通过】** 检查点与真实崩溃组合的 5 项专项已通过，绑定主树整合冻结版；仍检查不重放与独立文件内容。跨平台行为及非协作写入压力仍待验，乐观检查的边界保持。

---

## 9. SDK 公共边界

### DES-19 三层公共 API 契约（REQ-SDK）

- **【事实】** 现有公开面（尚未承诺 v1 稳定）：`mio-core`（模型/流式协议与观测）、`mio-agent`（`AgentRun` 可序列化状态机、`AgentRunStep`、`prepare_request`、`DispatchScope`、bus/hook）、`mio-coding-agent`（`Host`、`RunOptions`、`RunTools`、`run*`）。
- **【v1 契约】** v1 划定公共边界：三层只通过上述类型交互；`mio-coding-agent` 的调度/会话/journal 内部类型不进入 `mio-agent`；依赖方向必须由 CI 检查（`mio-core ← mio-agent ← mio-coding-agent`）。
- **【事实】** `scripts/check-architecture.py` 已检查生产/构建/平台依赖的直接与间接反向路径，支持 renamed/workspace 依赖；`tests/architecture.py` 3 项和当前仓库检查通过，CI 已接入。开发依赖不进入交付库依赖图，不据此承诺 API 稳定。
- **【v1 契约】** 稳定承诺范围限于已文档化类型；`pub` 但未文档化的模块（如 `execution_backend` 的部分内部构造）不构成兼容承诺。
- **【主树验收通过】** CLI/SDK 语义清单严格覆盖 82 个参数/别名，19 项检查器测试通过；Linux 27 组示例/feature 编译通过。完整离线 `host_loopback` 示例的 4 项契约及成功/模型失败/取消三场景已实际运行。主树已复验；macOS/Windows 不据此算通过。

---

## 10. OS 隔离 backend 策略

### DES-20 可插拔 backend（REQ-ISOLATE）

- **【事实】** `src/execution_backend.rs` 已定义 `ExecutionBackend` trait、`UnrestrictedBackend`、`BubblewrapBackend`（Linux）、`BackendStatus`（`available` / `unavailable_reason`）、`IsolationGuarantees`、`ResourceLimits`、`FilesystemPolicy`、`EnvironmentPolicy`、`ExecutionPolicy`；`tests/execution_backend.rs` 在 `/usr/bin/bwrap` 存在且内核允许非特权 namespace 时跑真实用例，否则**跳过**（`MIO_REQUIRE_BWRAP=1` 可要求必跑）。
- **【事实】** 本轮已补 `Host::execution_policy`（默认 unrestricted）与 `shell::execute_with_policy`，CLI 的 Shell 监督调用已连接，Linux 严格整合 39 项通过；不代表插件/MCP/任意宿主工具都已隔离，不能宣称全入口沙箱覆盖。
- **【v1 契约】** v1 的 backend 策略：
  1. backend 选择显式化，状态可查询并写入 `get_state`（`available`、`name`、`unavailable_reason`、`guarantees`）。
  2. 无 backend 时行为显式：默认 `unrestricted` 并如实声明无隔离；要求隔离而 backend 不可用时**拒绝执行**，不静默降级。
  3. worktree/路径租约只做修改隔离，不冒充安全边界。要求隔离时，Shell、插件、本地 MCP 子进程必须复用同一 policy；未接入的执行入口与无法约束的远程 MCP/任意 extra_tools 必须拒绝启用，不能声称受保护后绕过。
  4. backend 覆盖工具子进程，不覆盖宿主进程的 Provider 网络客户端；内置文件工具仍由工作区路径策略约束。状态必须同时报告请求的 policy、实际接入入口和剩余未覆盖项。HostRootReadOnly 仅限制写入、不隐藏宿主可读秘密；需要读隔离必须选 ExplicitMounts。
- **【Linux 主树验收通过】** 严格 backend 39 项包含本机读写、网络与每进程资源阻断；真实 ENOSPC 另有 3 项专项。证据绑定主树整合冻结版，不代表进程树总配额或全部宿主扩展。macOS seatbelt 与 Windows 仍待实现和原生验收。
- **不做**：v1 不宣称全入口、跨平台的 OS 内存/CPU/网络/文件描述符沙箱覆盖；已验证范围限于上述 Linux backend 与接入入口。

---

## 11. 实现顺序（DES-21）

依赖优先，每步自带验收，禁止跳步宣称完成：

1. **状态机与事件骨架**（DES-04/05/14/15）：先固化协议发现与兼容规则，再固化状态枚举、`schemaVersion`、五级 ID 与能力协商。理由：其余项的验收都依赖事件可追踪。
2. **统一门禁端到端**（DES-06/07）：补插件/MCP 实际路由、拒绝零副作用、参数篡改三类用例。
3. **恢复闭环**（DES-09/10/18）：四时点注入 + 统一恢复视图 + 未知副作用不重放；检查点与崩溃注入组合验收。
4. **上下文分级与损失报告**（DES-11/12/13）：先建可复现失败基线，再实现 Pinned 判定与损失报告，最后接引用检索。
5. **输出生命周期与清理**（DES-17）：在引用语义稳定后定义淘汰与磁盘满行为。
6. **背压**（DES-16）：需要慢消费者测试夹具，放在事件骨架稳定之后。
7. **SDK 边界与依赖检查**（DES-19）：以 CI 检查固化方向，不改变既有类型语义。
8. **backend 接入**（DES-20）：先做状态可查询与显式拒绝，再接 Shell 路径与平台实测。

---

## 12. 逐项可判定验收矩阵（DES-22）

| 决策 | 覆盖需求 | 可执行判定 | 通过条件 | 证据落点 | 当前实现/验收状态 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| DES-04/05 | REQ-SCHEDULE/RECOVERY | 状态转移单测 + 非法转移拒绝用例 | 无终态回退、无重复 `Dispatched`、强杀遗留不重放、未确证存活状态记 unknown | `tests/subagent.rs`、`tests/recovery_view.rs` | 已纳入主树四门禁回归；范围见主树验收记录 |
| DES-06 | REQ-POLICY | 四入口（builtin/plugin/mcp/extra）拒绝用例 + 参数篡改用例 | 拒绝路径零副作用；篡改被拒 | `tests/tool_policy.rs`、`tests/tool_policy_extensions.rs` | 已纳入主树四门禁回归；范围见主树验收记录 |
| DES-07 | REQ-POLICY | 重放同 `request_id`、伪造 `paramsSha256`、跨 scope 复用 | 全部返回失败，无工具执行 | `tests/tool_policy.rs`、`docs/rpc.md` | 已纳入主树四门禁回归；范围见主树验收记录 |
| DES-08 | REQ-BUDGET | 多进程并发预留、重启继承、损坏 fail-closed | 多进程一致；损坏拒绝后续请求 | `tests/shared_budget.rs` | 【事实】（离线） |
| DES-09 | REQ-RECOVERY | 四时点 SIGKILL 注入 + unknown 不重放 | 证据完整；无自动重放 | `tests/crash_recovery.py`、`tests/recovery_view.rs` | 主树 12 项 SIGKILL 通过；不重放回归通过 |
| DES-10 | REQ-PROCESS | 取消后进程组/审批/事件顺序断言 | 无后续派发、监督完成、pending 归零、阻塞worker租约保持 | `tests/cli_cancellation.py` + 新增 | 已纳入主树四门禁回归；范围见主树验收记录 |
| DES-11/13 | REQ-CONTEXT-FIDELITY | 约束置中/置尾 + 压缩后逐条比对 + 损失报告存在性 | 显式 Pinned 零丢失或拒绝请求；普通摘录丢失必须报告 | `tests/context_fidelity.rs`、`tests/rpc.rs`、`tests/recovery_view.rs` | 已纳入主树四门禁回归；范围见主树验收记录 |
| DES-12 | REQ-CONTEXT-RETRIEVAL | `read` byte 分页读完外置输出 | 全量可达、无字符截断 | `tests/output_paging.rs` + 端到端 | 真实输出→引用→只读分页重建通过；模型长文门槛未达 |
| DES-14/15 | REQ-EVENTS | 版本协商矩阵 + 错误→请求→工具调用追踪 + 准入拒绝 | 交集为空显式失败；追踪无需原始响应体；拒绝无 Dispatched | `tests/rpc_protocol.rs`、`tests/run_correlation.rs`、`tests/dispatch_observation_denial.rs` | 已纳入主树四门禁回归；范围见主树验收记录 |
| DES-16 | REQ-EVENTS | 慢消费者注入，断言丢弃计数与终态不丢 | 有界内存；无法容纳关键事件则停止派发并断连，不虚报送达 | `src/bin/mio_cli/json_output.rs` 单元与 RPC 用例 | 已纳入主树四门禁回归；范围见主树验收记录 |
| DES-17 | REQ-EXTENSIONS | 磁盘满 + 取消 + 引用可达性淘汰 | 失败显式；被引用文件不被删 | `tests/output_lifecycle.rs`、`tests/output_cleanup.rs`、`tests/output_cleanup_cli.rs` | 主树生命周期/清理/发布回归通过；强制 bwrap 的真实 ENOSPC 通过 |
| DES-18 | REQ-RECOVERY | 检查点 + 崩溃注入组合；用户后改拒绝 | 拒绝覆盖用户修改；不碰 Git | `tests/checkpoint*.rs`、`tests/checkpoint_crash_recovery.py` | 主树检查点与崩溃 5 项通过 |
| DES-19 | REQ-SDK | 最小/完整宿主示例编译 + 依赖方向检查 | 示例编译通过；方向单向 | `examples/host.rs` + CI | 主树 SDK 27/27、宿主 4 项契约通过 |
| DES-20 | REQ-ISOLATE | backend 状态查询 + 不可用时拒绝执行 + 平台越界实测 | 状态真实；无静默降级；跳过不算实测通过 | `tests/execution_backend.rs`、`tests/execution_backend_routing.rs`、`tests/sandbox_host_boundary.rs`、`tests/sandbox_cli.rs` | Linux 严格 39 项通过；其他平台待验收 |

### 评测支线门槛（不因 v1 设计而放宽）

- iteration-02 基线保持 **5/6**，该版本的 `retrieve-wire-spec` 仍为失败；新冻结版 6/6 单独记录，不改写旧结果（证据见 [PRD 第五节](../PRD.md#五评测基线与可提交证据)与[本轮验收](reviews/2026-10-17-v1-inline8k-acceptance.md)）。
- v1 不修改 `deepseek-chat-off-20calls-180s-isolated-v2` 的模型、thinking、预算或验收脚本；改进必须同预算对照并保留失败样本。
- 发布门槛沿用 PRD：三类离线保真用例通过 + 两套任务集全通过 + 长文任务 ≥2/3（≥3 次独立同预算运行、不复用修复前基线）。
- 候选原因（重复 `read`、分页/摘要后约束丢失、预算规划、catalog fallback）在证实前**不得**写成已确定根因，也不得据此特判。

---

## 13. 延期项汇总

| 项目 | 归属 | 触发条件 |
| :--- | :--- | :--- |
| ACP/HTTP server、会话树、Plan 模式、后台守护 | DES-02 | 真实需求证据 + 可回退模块边界 |
| repo map / LSP / 符号索引选型 | DES-02 | `REQ-CONTEXT-RETRIEVAL` 对照数据 |
| 跨 Agent 对照、公共 benchmark 接入 | DES-02 | 调用预算可对齐 + 采样适配器就绪 |
| macOS seatbelt / Windows 隔离 backend | DES-20 | v1 Linux backend 接入通过后 |
| 精确费用硬限额 | DES-02 | v1 不纳入；未来必须另有真实账单与预留机制设计，不能把当前 token 计数改名冒充 |

本文只定义契约与顺序；**任何 `DES-*` 均不得在未通过 DES-22 对应判定前被引用为已有能力**。
