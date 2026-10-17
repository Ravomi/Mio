# Mio JSONL RPC

使用 `mio --mode rpc` 启动常驻子进程；模型配置沿用普通 CLI 参数。stdin 每行一个 JSON 命令，stdout 每行一个 JSON 事件，stderr 用于诊断。客户端应持续读取输出。

```json
{"type":"prompt","id":"p1","message":"阅读 README 并解释项目"}
{"type":"steer","id":"s1","message":"重点说明 Rust 库集成"}
{"type":"follow_up","id":"f1","message":"然后检查测试覆盖"}
{"type":"get_state","id":"state1"}
{"type":"abort","id":"a1"}
```

RPC 支持受限的结构化图片输入：`get_state.data.protocol.capabilities.imageInput` 为 `true`，表示服务端能接收下述 `images` 字段；`get_state.data.modelCapabilities.imageInput` 则按当前配置的 Provider/模型从本地目录给出准入提示，`source` 为 `localCatalog`。目录值为 `true` 也不保证真实端点拥有视觉能力，最终仍以请求准入和 Provider 响应为准；目录值为 `false` 时客户端可在发送前提示当前模型不支持图片。`imageSteering` 为 `false`：运行中的 `steer` 仍只追加文字到工具结果，携带图片时明确拒绝，客户端可用 `follow_up` 排队下一回合；空闲 `steer` 与 `prompt` 一样启动新回合。其他输入字段（包括 `attachments`）按原命令 ID 拒绝，不发送其中的文字，也不消耗任务去重 key。文件路径字符串不能代表模型已看见图片。

图片随 `prompt`、`follow_up` 或空闲 `steer` 发送，`message` 仍须为 1–65536 字节的非空文字：

```json
{"type":"prompt","id":"p-image","message":"描述这张图片","images":[{"mime_type":"image/png","data_base64":"<base64 PNG bytes>","detail":"auto"}]}
```

`images` 最多 4 项，仅接受内联 base64 PNG/JPEG/GIF/WebP（`mime_type` 分别为 `image/png`、`image/jpeg`、`image/gif`、`image/webp`）；`detail` 可省略或为 `auto` / `low` / `high`。单张解码后最多 512 KiB、合计最多 1 MiB，RPC 输入行仍受 1 MiB 上限约束，因此实际可发送总量可能更低。Mio 校验 base64 与文件头，拒绝远程 URL、文件 ID、未知字段及本地目录未标注支持图片的模型；图片在任务去重 claim 之前验证。上下文为每张图预留 32768 估算单位，实际 token 用量以 Provider 报告为准。成功回合将原始图片块保存到会话（会话文件总上限 16 MiB），后续请求可重放；取消或失败沿用原会话保存语义。本地 RPC 子进程回环已验证 OpenAI 兼容、Anthropic、Gemini、Ollama 的图片编码、会话保存、下一轮重放与图片 `follow_up` 排队执行；SDK 回环另验证三个原生路由的出站形状。真实视觉模型仍待验收。

## 协议发现与兼容

`get_state.data.protocol` 在空闲和运行中均可读取，不发起额外模型请求：

```json
{"name":"mio-jsonl-rpc","discoveryVersion":1,"eventEnvelope":"legacy","schemaVersion":1,"envelopeVersions":[1],"capabilities":{"steering":true,"followUp":true,"asyncApproval":true,"dedupKey":true,"recoveryView":true,"pinnedContext":true,"contextCompactionAudit":true,"eventReplay":false,"automaticSideEffectReplay":false,"imageInput":true,"imageSteering":false,"reasoningEvents":true,"initialize":true},"correlation":{"sessionId":true,"runId":true,"turnId":true,"toolCallId":true,"subagentId":true,"parentRunId":true},"limits":{"inputLineBytes":1048576,"messageBytes":65536,"steeringQueue":64,"followUpQueue":64,"eventQueue":256,"eventBytes":1048576,"eventQueueBytes":8388608}}
```

`discoveryVersion` 仅版本化此发现文档；`schemaVersion` 是服务端能发出的最高事件封套版本，`envelopeVersions` 列出支持的版本集合。`eventEnvelope: legacy` 表示尚未通过 `initialize` 协商到新封套。capabilities 描述该服务端实现支持的功能，不表示当前运行已启用审批或持久化；`correlation` 如实标注哪些关联 ID 目前能由 CLI 侧提供；支持的身份来自实际运行观测，具体事件不适用或尚未产生时仍为 `null`。缺少 `protocol` 的旧版服务端应视为能力未知；客户端忽略未知字段，不把缺失字段推断为已支持。新增可选字段保持兼容，未来变更命令或事件语义必须另行协商，不能静默改变 legacy 输出。当前不支持事件断点重放，也不自动重放副作用。

## 事件封套与 schemaVersion

每一行出站 JSON（命令应答、`text`、`reasoning`、`tool_started`、`tool_finished`、`done`、`agent_settled`、`approval_request`、`completion_call`、`diagnostic`、`execution_timing`、`task`、`recovery` 等）都带顶层 `schemaVersion` 整数。新增字段是兼容变更，既有命令字段与输出内容不变。

`reasoning` 只来自 Provider 实际流出的推理文本增量，形如 `{"type":"reasoning","data":{"part":0,"text":"..."}}`；`part` 是该次响应的内容块序号。它与正文 `text` 分开，未提供推理正文的 Provider 不产生该事件。子代理的推理增量沿 Host 回调转给父宿主，但不并入子代理的答案快照。v1 封套用真实 `runId` 关联运行，已观测到回合时填 `turnId`；子代理有真实 `subagentId` / `parentRunId`，其余不适用的身份仍为 `null`。`reasoning` 与 `text` 都是可丢弃的流式增量，背压时查看 `droppedEvents`；取消时已流出的增量不表示本轮已成功保存。完成的模型回复按现有会话规则保存，不能把流式事件当作持久化记录。

未发送 `initialize` 的客户端继续使用 legacy 封套：不追加任何关联字段。协商成功切到 `v1` 后，每个事件追加 `sessionId` / `runId` / `turnId` / `toolCallId` / `subagentId` / `parentRunId`：已知的真实 ID 填值，其余为 `null`。`runId` 是本次引擎运行的 UUID，`turnId` 来自引擎回合，`toolCallId` 来自 Provider 调用；子代理保留自身 run UUID，通过 `parentRunId` 关联父运行。审批 `binding.callId` 是独立的审批尝试 UUID，不能与 `toolCallId` 混用。工具准入前的 Prepared 不表示已执行，只有全部准入通过后才观察为 Dispatched；拒绝仍保留真实调用身份。当前 CLI 的单工具并发配对已经定向验证，多工具并发逆序完成仍待验收，不能仅因能力字段为 true 推断该场景已通过。

## 能力协商 (initialize)

客户端可在空闲或运行中发送一次性协商命令：

```json
{"type":"initialize","id":"i1","schema_versions":[1],"capabilities":["steering","followUp"]}
```

`schema_versions`（别名 `protocol_versions`）是客户端接受的封套版本集合，`capabilities` 可选。服务端取双方版本交集中的最高版本：

```json
{"type":"response","command":"initialize","id":"i1","success":true,"data":{"status":"negotiated","negotiated":true,"envelope":"v1","schemaVersion":1,"clientSchemaVersions":[1],"serverSchemaVersions":[1],"capabilities":{"steering":true,"followUp":true,"asyncApproval":true,"dedupKey":true,"recoveryView":true,"pinnedContext":true,"contextCompactionAudit":true,"eventReplay":false,"automaticSideEffectReplay":false},"grantedCapabilities":["steering","followUp"],"unsupportedCapabilities":[],"unknownCapabilities":[]}}
```

交集为空（或 `schema_versions` 缺失/为空）时显式拒绝，不静默降级、不假协商：

```json
{"type":"response","command":"initialize","id":"i-bad","success":false,"data":{"status":"rejected","negotiated":false,"envelope":"legacy","schemaVersion":null,"clientSchemaVersions":[2],"serverSchemaVersions":[1],"error":"No common RPC schema version; ..."}}
```

- 协商成功后 `get_state.data.protocol.eventEnvelope` 变为 `v1`，并新增 `negotiation` 字段（`initialized` / `rejected` / `schemaVersion`）。
- 协商被拒后，`prompt` / `steer` / `follow_up` 等新任务启动返回 `success:false` 与明确错误，不产生模型请求；重新发送受支持版本的 `initialize` 即可恢复。已在运行的任务不会被撤回，也不会被谎称成功。
- `capabilities` 只做能力声明与校验：已知但未实现（如 `eventReplay`）计入 `unsupportedCapabilities`，未知名称计入 `unknownCapabilities`，不会因此假装已授予。

## 有界输出与慢消费者背压

stdout 由一个独占 writer 线程经有界队列写出，Agent 回调不再直接写 stdout：

- 事件通道容量 256 条、单事件上限 1 MiB、排队字节上限 8 MiB。
- 所有入队都**不等待消费者**：Agent 在同一个 Future 内同步调用的回调（`Host::on_event`、`on_completion_call`、`on_diagnostic`、审批请求等）不会因为客户端不读 stdout 而阻塞。
- 低价值流式增量（`text`）在队列满时被丢弃并计数；`tool_started` / `tool_finished` / `completion_call` / `diagnostic` 等结构化事件不被静默丢弃，入队时先驱逐已排队的流式增量，仍装不下就**立即** fail-closed（取消活动工作）。命令应答、结束、错误与审批事件同样不等待队列空间（不存在“等待 5 秒”的窗口），无法入队时立即 fail-closed。
- 发生丢弃时，后续结构化事件（含 `agent_settled`）带 `droppedEvents` 字段，值为在此之前被丢弃的流式事件数（下界）。这是显式的缺失声明，不会用缺失事件冒充成功。失败时只写短 stderr 诊断，不混入 stdout。
- writer 线程无法启动时传输立即 fail-closed，不会假装事件会被写出。
- 因此客户端不读 stdout 时，`abort` 仍会被及时处理：先取消模型/工具 work，再发送 `abort` 应答与 `agent_settled`；若结构化事件已占满队列，传输立即 fail-closed，RPC 循环取消活动工作并以非零退出码结束，不会静默丢弃终态。
- 退出做有界 flush（5 秒）。stdout 阻塞导致未 flush 完时进程不永久挂起，而是以非零退出码结束并在 stderr 报告未投递事件；JSON/RPC 模式下传输已 fail-closed 时不会再向 stdout 写结束事件。

客户端应持续读取 stdout；一旦出现 `droppedEvents`，说明该轮存在缺失的流式增量，终态仍以 `agent_settled` 为准。

## 显式约束与未完目标

空闲时发送 `set_pinned_context`，以完整注册表显式替换旧值；模型工具无法调用这个宿主控制接口。`get_state.data.pinnedContext` 返回当前表。

```json
{"type":"set_pinned_context","id":"pins-1","context":{"entries":[{"id":"api-contract","kind":"constraint","text":"保留现有公共 API。"},{"id":"migration","kind":"goal","text":"完成迁移并验证。"}]}}
```

条目 ID 按原文比较，表内重复 ID 被拒绝；`kind` 只接受 `constraint` / `goal`。上限为 64 条、ID 64 UTF-8 字节、单条正文 8 KiB、正文合计 32 KiB；空 ID/正文、ID 控制字符和非法正文控制字符均被拒绝。正文允许换行、回车与制表，不进行关键词识别或改写。空表显式解除全部条目。

这些条目作为每次请求的固定宿主指令携带，不参与对话摘要。模型是否遵循指令仍需任务验收；“保留”表示请求里原文未被裁剪，不表示模型行为的安全保证。放不下时拒绝模型请求，不自动扩大窗口或丢弃条目。登记表大小合法也不保证任何部署窗口都能容纳它。

运行中拒绝修改注册表。启用会话保存时，只有落盘成功才更新内存；`--no-session` 只改内存。`fork_session` 继承登记表，`new_session` 创建空表。对应终端命令为 `/pin constraint ID TEXT`、`/pin goal ID TEXT`、`/pin replace constraint|goal ID TEXT`、`/unpin ID`、`/pins`；命令分隔空白不属于正文，需要精确保留多行与前导空白时使用 SDK/RPC。

## 结构化压缩审计

请求级或自动会话压缩会通过 `Host::on_context_compaction` 发出 `context_compaction` 事件，含 `runId`、请求准备轮次 `turn`（不表示已联系 Provider）、可选 `agent`、`kind`（`request` / `session`）和 `report`。报告字段包括 `schema_version`、原历史半开区间 `source_start/source_end`、`source_bytes`、`summary_bytes`、仍保留的系统消息数、`affected_tool_call_ids`、`tool_call_ids_truncated`、`truncated_sections`、`unrepresented_content_messages` 与 `pinned_items_preserved`。

字节数描述输入记录和摘要文本，不能当成 token、账单或“语义损失字节数”。报告不包含原对话、工具结果正文或摘要正文；工具调用 ID 最多 128 个，每个最多 256 字节，超过时显式标记审计不完整。`pinned_items_preserved: true` 表示已登记条目仍在固定指令中；`null` 表示该路径没有适用的显式登记表。

启用 journal 时，报告在模型请求前保存；保存失败阻止该请求继续。`get_state.data.recovery.context_compactions` 关联 journal 运行与 engine 运行，单次 journal 运行最多保留 4 份审计，超出数计入 `dropped_context_compactions`，并沿用 journal 的全局大小与运行数上限。禁用会话时不落盘。

显式 `compact` 的响应 `data.loss_report` 记录本次审计；`Session::compact` 同时保存最近一份会话压缩报告，可通过 `get_state.data.recoveryView.lastCompactionLoss` 或离线恢复 view 查询。无实际压缩时没有新报告。摘要始终是有损摘录，普通对话未被显式登记的中部细节仍可能丢失。

## 运行控制

- 空闲时，`prompt`、`steer`、`follow_up` 都会立即开始一轮任务。
- 运行时，`steer` 按收到顺序排队，在工具完成后附加到工具结果传给模型。若本轮没有可消费引导的工具结果，完成后先处理剩余引导，再执行 follow-up。它不会中断已发出的模型请求或正在执行的工具。
- `follow_up` 按 FIFO 顺序在当前任务及引导完成后自动启动，不需要额外发送 `prompt`。
- `get_state` 在运行中可用；`isStreaming` 表示任务活动状态，`followUpCount` 表示待执行 follow-up 数量，`steeringCount` 为待注入引导数，`pendingApprovalCount` 为待审批数。运行时的消息数来自本轮开始前的已保存历史。`scheduler` 返回本进程调度的实时状态，`childTasks` 返回子代理活动的 `{running,queued,limit}`，`journalEnabled` / `journalPath` / `recovery` 返回执行证据状态，`unresolvedEffects` 返回工作区持久未决副作用账本的只读摘要（详见“调度、配额与结构化事件”）。
- `abort` 取消当前模型/工具 future 与活动子代理，清空引导和 follow-up 队列。应答中的 `aborted` 为实际是否有活动任务；空闲取消返回 `false`。取消后可再次提交任务。
- 运行中发送普通 `prompt`、修改会话、压缩或切换模型会返回失败，应使用 `steer` / `follow_up` 或等待空闲。启用会话持久化时，压缩、命名、新建和分叉仅在落盘成功后返回成功；失败保留原会话状态并可重试。--no-session 仅更新内存。
- 关闭 stdin 会取消活动任务并退出。错误会清空待执行队列。取消或失败保留上次成功保存的会话；已发生的文件修改不回滚。

每条消息须为非空文本，最多 65536 字节。引导与 follow-up 各最多排队 64 条，超过限制明确返回失败。Shell 使用当前用户权限，不是沙箱；Unix 取消会杀死进程组，其他平台只保证处理直接子进程。

## 显式任务 key

`prompt` 和 `follow_up` 可携带 `dedup_key`（1–512 字节，不得全为空白或含控制字符）。key 在同一工作区跨会话、跨进程共享，保存到 `.mio/dedup/task-keys.json`；`--no-session` 时只在当前 RPC 进程内有效。

```json
{"type":"prompt","id":"p1","message":"检查项目","dedup_key":"review-001"}
```

首次接受返回 `data.task_id`，排队的 follow-up 同样返回该标识。重复提交返回 `data.status: "deduplicated"`、原 `task_id` 和已有 `state`，不再次执行；活动、排队、completed/failed/cancelled/interrupted 均阻止重放。失败或取消后需要重试时使用新 key。强杀后记录可能仍为 active/queued，不表示任务仍存活。无效消息或队列满拒绝的新任务不会占用 key。key 按原字符串比较，即使 message 不同也不会再次执行。

账本损坏、版本不符或达到大小/条目上限时拒绝新任务，不自动淘汰历史。返回的任务标识用于去重识别，不会回放原任务输出。空闲时命中 key 会发送 `agent_settled`，其 `status` 为 `deduplicated`；运行中命中只应答该命令，当前任务继续。

### 去重命中的保守存活观测

去重命中（`data.status: "deduplicated"`）时，命令应答额外包含 `liveness` 字段，取值为 `live` / `stale` / `unknown`，表示账本记录中 owner 进程的**保守存活性观测**：

- `live`：同一 host、同一次启动、同一 PID namespace 内，owner 进程仍在运行。
- `stale`：有正向证据表明 owner 已不存在，例如同一 host 已重启、PID 被复用（启动时刻不符）、或进程条目消失且 `/proc` 没有隐藏策略。`stale` 只说明 owner 已消失，**不表示可以重放**：`task_id` 与 `state` 保持不变，记录不会被自动改写或删除，也不会自动重放任务。
- `unknown`：证据不足，例如旧版记录缺少 owner、owner 属于另一台 host 或另一个 PID namespace、`/proc` 不可读、平台不支持，或记录已处于终态（终态不参与 owner 探测）。未知不等于死亡。

新任务（`status: "started"` 或入队）没有可观测的 owner 历史，因此不附带 `liveness`，不做任何猜测（不用 `updated` 时间或进程探测的缺省值冒充存活/死亡）。运行中重复 `prompt` / `follow_up` 命中也同样附带该字段。CLI `--dedup-key` 命中时输出的 `task` 事件（`reused: true`）在 JSON 模式下包含同一 `liveness` 字段，文本模式在提示行中标注；新准入的 `task` 事件（`reused: false`）如实省略该字段。

观测是只读的：账本不可读或损坏时命令 fail-closed 返回错误，不会把探测失败降级成 `unknown`，也不会据此放行或重放任务。该字段与既有 `task_id` / `state` / `status` / `reused` 语义相互独立，不改变它们在去重中的含义。

## 响应与结束事件

命令应答携带原始 `id`，表示命令是否接受，不表示任务完成：

```json
{"type":"response","command":"prompt","id":"p1","success":true,"data":{"status":"started"}}
```

执行期间保留 `text`、`tool_started`、`tool_finished` 等事件。Shell 与插件工具的单流输出超过 8 KiB 时，`tool_finished` 的 `result` 只含约 2 KiB 预览与 `.mio/outputs/<uuid>.<stream>` 引用，完整输出写在该文件；单流超过 8 MiB 的调用失败并终止其进程，而不是返回截断的成功结果。读取长单行引用可交给 `read` 的 `byte_offset` / `byte_limit` 分段接口。`done` 表示一轮已成功保存，但后续引导或队列仍可能继续。等待整个工作队列静止应使用 `agent_settled`：

```json
{"type":"agent_settled","status":"completed"}
```

`completed` 表示运行正常结束且实际 worker 已收回；`done`、退出码0及该状态都不证明目标或产物已通过独立验收。模型可以报告未完成或未验证后正常结束，宿主应单独记录业务验收结果。

`status` 还可能为 `cancelled` 或 `error`。`error` 状态前会发送错误事件。EOF 关闭时不保证发送结束事件，客户端需同时处理进程退出。

## 调度、配额与结构化事件

`get_state.data.recoveryView` 将磁盘已保存历史与执行证据关联：`sessionId`、`sessionFile`、`sessionSaved`、`savedMessages`、`sessionError`、`evidenceField: "recovery"` 与 `automaticReplay: false`。`savedMessages` 只统计 ID 与工作区匹配的磁盘会话；未保存、损坏或 `--no-session` 时为 null，不能用内存中的 `messageCount` 代替。无会话模式的 `sessionFile` 也为 null。

CLI `--recovery --json` 保留 `{type:"recovery",session,data}`，新增同义的 `view`，其中 `evidenceField` 为 `"data"`。恢复证据中的 `unfinished_goals` 记录非完成运行的 `run_id`、`turn`、`state`、`goal` 与 `truncated`；请求摘要超过 4096 字符会标记截断，旧 journal 未记录的请求为 null。这些是历史证据，不是自动重试队列。查询不会保存会话、修复损坏历史或重放副作用。

启动时可用 `--shared-limits-dir` / `--no-shared-limits` 和本进程 `--max-model-concurrency` / `--max-provider-concurrency` / `--max-heavy-tasks` / `--max-subagents` / `--max-model-calls` / `--max-total-tokens` 配置配额；默认同时启用进程内调度与跨进程协调（用户配置目录的 `coordination/`）。

`get_state.data` 额外包含本进程调度实时状态（不伪造跨进程 active）：

```json
{
  "type": "response",
  "command": "get_state",
  "id": "s1",
  "success": true,
  "data": {
    "scheduler": {
      "tasks": { "active": 1, "queued": 0, "limit": 4 },
      "model": { "active": 1, "queued": 0, "limit": 2, "activeByProviderHash": {}, "cooldownMs": {} },
      "heavy": { "active": 0, "queued": 0, "limit": 2 },
      "budget": { "calls": 3, "maxCalls": null, "tokensReserved": 0, "tokensUsed": 1200, "maxTokens": null, "overage": 0, "blocked": false },
      "shared": { "modelLimit": 2, "providerLimit": 2, "heavyLimit": 2, "cooldowns": [] },
      "waitingSharedModel": 0,
      "waitingSharedHeavy": 0
    },
    "childTasks": { "running": 1, "queued": 0, "limit": 2 },
    "journalEnabled": true,
    "recovery": null
  }
}
```

- `scheduler.shared.budget` 返回 `max_model_calls`、`max_total_tokens`、`calls_started`、`tokens_reserved_active`、`tokens_used`、`tokens_over_reservation`、`tokens_overage` 和 `blocked`。同 root 跨运行累计，未传预算参数会继承已有策略；显式放宽会被拒绝。未知 usage 按预留结算，崩溃遗留预留不自动退款；账本损坏或不可写时拒绝后续模型请求。不同于下方本地 `scheduler.budget`，共享快照使用 snake_case 字段。
- `scheduler.model.activeByProviderHash` / `cooldownMs` 使用 endpoint + 凭据的 SHA-256 摘要作为键，不含原文；429 的 `Retry-After` 冷却也记在这里。
- `scheduler.shared` 返回跨进程配置、冷却与 `budget` 持久预算快照；跨进程持有者无法可靠枚举，因此**不返回全局 active 计数**。共享状态不可用时返回 `{"error": "..."}`。
- `scheduler.model.activeByProviderHash` 与 `budget` 均为本进程计数。`budget.maxCalls` / `maxTokens` 为 `null` 表示未设置上限；`tokensReserved` 是在途预留，`tokensUsed` 是结算后计数，`overage` > 0 表示实际用量超出预留、后续请求会被阻止。这是 token 计数，不是费用账单。
- `childTasks.queued` 对应 `SubagentManager` 的 `Queued` 状态，`limit` 来自 `--max-subagents`；子代理在并发上限或工作区忙碌时排队而非拒绝。
- `unresolvedEffects` 返回持久未决副作用账本的只读摘要：`mode`（`disabled` / `read_only` / `read_write`）、`enabled`、`present`、`entries`、`unknownCount`、`outstandingCount`、`resolvedTrimmed`、`coverageInitialized`、`coverageIncomplete` 与 `error`。账本不可读或损坏时命令 fail-closed 返回错误，计数字段为 `null` 且 `error` 给出原因，不把探测失败降级成“无未知”。它只给摘要；逐条精确 `(scope, call_index)` 与快照摘要的列出/确认是 CLI 维护通道，不是 RPC 命令。

运行中还会发送结构化事件：

```json
{"type":"completion_call","data":{"runId":"...","callIndex":0,"usage":{"inputTokens":100,"outputTokens":20},"finishReason":"stop"}}
{"type":"diagnostic","data":{"runId":"...","category":"rate_limit","message":"...","retryable":true,"httpStatus":429}}
```

`usage` 中未上报的计数为 `null`，不会伪造成 0；`completion_call` / `diagnostic` 均不含凭据、请求体或原始响应。`diagnostic.category` 为稳定分类（如 `rate_limit`、`authentication`、`scheduling`、`cancelled`），不是原始错误文本。

## 工具审批流程 (--approve)

在启动时传入 `--approve` 参数后，RPC 模式下写文件、执行 Shell 等敏感操作会触发非阻塞的异步审批通知，而不会像非 TTY 文本或 `--json` 模式下直接拒绝。

审批请求由引擎向客户端发出：

统一工具门禁还附带 `binding`：`entry`、`tool`、`operation`、`cwd`、`scope`（引擎 run UUID）、`paramsSha256`、`callId`（本次审批尝试 UUID）与可选 `toolCallId`（Provider 调用 ID）。客户端据此核对参数摘要与执行上下文；回复只释放原 `request_id` 对应的单次调用，过期、重复或其他请求 ID 无法授权新调用。MCP 服务启动等不经过工具 dispatch 的审批，`binding` 为 null。

```json
{
  "type": "approval_request",
  "request_id": "c88fcfb5-2fd5-41d3-9bc3-066e04746654",
  "operation": "write",
  "target": "src/main.rs",
  "timeout_ms": 120000
}
```

客户端通过发送 `approve_response` 指令回复允许或拒绝：

```json
{
  "type": "approve_response",
  "request_id": "c88fcfb5-2fd5-41d3-9bc3-066e04746654",
  "approved": true
}
```

Mio 回复该指令的标准 response：

```json
{
  "type": "response",
  "command": "approve_response",
  "id": "c88fcfb5-2fd5-41d3-9bc3-066e04746654",
  "success": true,
  "data": { "approved": true }
}
```

- **超时与限制**：每个审批请求的超时时间为 120 秒（`120000` 毫秒），基于统一单调时钟 deadline 判定；最大 pending 审批并发数为 64，超过限制将直接拒绝新请求。请求 drop 或 resolve 后名额立即释放，无需等待 120 秒。
- **语义与状态**：
  - `approved: false`（拒绝）：工具调用失败并向模型返回拒绝信息，Agent 轮次继续按错误分支流转。
  - `unknown` / `stale` / 重复 resolve：对不存在、已超时或已 resolve 的 `request_id` 回复 `success: false` 并附带错误信息。
  - `abort` / EOF：任务被 abort 或输入遇到 EOF 时，所有待处理审批被即时取消、future 释放并移出 pending 队列，正在等待的工具操作不会向磁盘写入。
- **状态监控**：`get_state` 命令的返回数据中包含 `pendingApprovalCount` 字段，可实时监控当前等待审批的请求数量。
- **`operation` 取值**：除 `write` / `edit` / `bash` 等工具操作外，项目 MCP 服务器启动授权也走同一审批通道，`operation` 为 `mcp_server_start`，`target` 描述包含服务器名、命令、参数、工作区与指纹摘要，不包含环境变量值。批准后该服务器按指纹记入用户配置目录的 MCP trust store（信任库不可写时只在本次进程内有效），配置不变时后续启动或进程不再重复询问；指纹变化会重新请求。声明的 `optional` 服务器在授权被拒或启动失败时只打印诊断并跳过，必需服务器则使工具加载失败。启动时传入 `--trust-project-mcp`（或 `MIO_TRUST_PROJECT_MCP=1`）时不再为项目服务器发送审批请求。

### 未知副作用的独立确认通道

未决未知副作用的**逐条**查询与确认不属于普通工具审批，也没有独立的 RPC 命令：

- CLI `--list-unresolved-effects` 只读列出持久账本中每条记录的精确 `(scope, callIndex)`、`state`、`snapshot` 摘要与 `acknowledgeable`，然后退出，不需要 Provider 或模型。
- CLI `--ack-unresolved-effect '{"scope":"...","call_index":N,"snapshot":"<sha256>","note":"..."}'` 按精确 key+当前快照确认一条 `Unknown`；它从不删除账本，也拒绝证据不完整（`facts_dropped > 0`）的记录。
- RPC / 交互确认仍复用有界审批 broker，但 `operation` 为 `confirm_unknown_side_effect`，与参数绑定工具审批相互独立：确认它不会释放随后的普通工具审批，也不写入工具 journal intent。引擎在询问宿主前先把每个展示的未知解析为真实 `(scope, call_index)` 与当前快照摘要；等待期间出现新证据会让确认变旧，持久化时被拒绝而不是静默覆盖新证据。JSON / 非 TTY 且无 broker 的运行无法回答该确认，直接拒绝。

## 模型切换

空闲时可发送 `{"type":"set_model","id":"m1","model":"deepseek/deepseek-chat"}`，或在文本交互中使用 `/model deepseek/deepseek-chat`。只写模型名会沿用当前连接；带供应商前缀则同时切换对应端点、凭据和账户服务。切回已使用的供应商会恢复本进程保存的连接配置（包括启动时的自定义地址）。首次切到 `custom` 时可通过 `MIO_BASE_URL` 指定地址。成功的 `set_model` 应答在 `data.modelCapabilities` 返回切换后模型的本地目录图片能力；拒绝切换时配置保持不变，客户端可继续用 `get_state` 读取当前能力。

命名供应商优先使用其专用密钥环境变量，再使用 `MIO_API_KEY`；不会使用其他供应商的专用密钥兜底。`custom` 使用 `MIO_API_KEY` 或 `OPENAI_API_KEY`。缺少 Antigravity 账户或不支持的原生协议会返回失败并保留原配置；不会静默换供应商。`get_state.provider` 返回当前供应商名称。

后续子任务同步采用新模型配置，已完成子会话的历史保留。有活动子任务时拒绝切换。思考等级通过原生参数传给通用 OpenAI 兼容接口和 Codex；实际支持取决于模型。DeepSeek API 支持 off/low/high/max，其他等级明确拒绝；DeepSeek 网页适配器暂不支持开启思考。Antigravity 保留专用映射。交互调整同步到后续子任务。

## 每次请求的上下文预算

`get_state.data.contextBudget` 返回 `context_window`、`max_output_tokens` 和来源 `catalog` / `override` / `fallback`。模型切换会重新查询仓库内的容量目录，显式覆盖值保持不变；后续子任务继承相同设置。

启动参数 `--context-window N`（至少 1024）覆盖部署容量；`--max-output-tokens N` 同时指定输出预留和原生请求上限。默认输出预留是 4096、容量的四分之一、目录输出上限三者的最小值；还保留 5% 余量。未知模型容量回退 32768，并非对服务端真实容量的保证。DeepSeek 网页适配器不支持输出上限，因此拒绝显式设置；默认值仅参与输入预算。

每次模型调用前（包括工具结果续轮）都计算系统指令、实际工具定义、历史和当前消息的序列化 UTF-8 字节及格式开销，作为保守 token 估算；这不是精确 tokenizer。仅在完整工具交互边界压缩旧内容，保留系统消息和当前工具调用/结果。预算摘要最多 4096 字节、优先取近期请求、发现和工具结果；可能丢失细节。预算压缩只改变本次请求，不改写保存的完整历史。手动 `compact` 和自动会话压缩用于改写会话历史；CLI 默认开启，阈值为输入预算 3/4 与 512 KiB 中较小者，`--no-auto-compact` 关闭。

无法容纳当前输入、指令、工具定义或活动工具结果时，发出明确的 `Context budget:` 错误并以 `settled(error)` 结束，后续提示仍可继续；不会把超限请求发给模型。当前估算只支持文本上下文，媒体消息明确拒绝。目录可能滞后于部署，需按实际容量覆盖参数。
