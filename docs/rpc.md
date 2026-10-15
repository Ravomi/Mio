# Mio JSONL RPC

使用 `mio --mode rpc` 启动常驻子进程；模型配置沿用普通 CLI 参数。stdin 每行一个 JSON 命令，stdout 每行一个 JSON 事件，stderr 用于诊断。客户端应持续读取输出。

```json
{"type":"prompt","id":"p1","message":"阅读 README 并解释项目"}
{"type":"steer","id":"s1","message":"重点说明 Rust 库集成"}
{"type":"follow_up","id":"f1","message":"然后检查测试覆盖"}
{"type":"get_state","id":"state1"}
{"type":"abort","id":"a1"}
```

## 运行控制

- 空闲时，`prompt`、`steer`、`follow_up` 都会立即开始一轮任务。
- 运行时，`steer` 按收到顺序排队，在工具完成后附加到工具结果传给模型。若本轮没有可消费引导的工具结果，完成后先处理剩余引导，再执行 follow-up。它不会中断已发出的模型请求或正在执行的工具。
- `follow_up` 按 FIFO 顺序在当前任务及引导完成后自动启动，不需要额外发送 `prompt`。
- `get_state` 在运行中可用；`isStreaming` 表示任务活动状态，`followUpCount` 表示待执行 follow-up 数量，`steeringCount` 为待注入引导数，`pendingApprovalCount` 为待审批数。运行时的消息数来自本轮开始前的已保存历史。`scheduler` 返回本进程调度的实时状态，`childTasks` 返回子代理活动的 `{running,queued,limit}`，`journalEnabled` / `journalPath` / `recovery` 返回执行证据状态（详见“调度、配额与结构化事件”）。
- `abort` 取消当前模型/工具 future 与活动子代理，清空引导和 follow-up 队列。应答中的 `aborted` 为实际是否有活动任务；空闲取消返回 `false`。取消后可再次提交任务。
- 运行中发送普通 `prompt`、修改会话、压缩或切换模型会返回失败，应使用 `steer` / `follow_up` 或等待空闲。启用会话持久化时，压缩、命名、新建和分叉仅在落盘成功后返回成功；失败保留原会话状态并可重试。--no-session 仅更新内存。
- 关闭 stdin 会取消活动任务并退出。错误会清空待执行队列。取消或失败保留上次成功保存的会话；已发生的文件修改不回滚。

每条消息须为非空文本，最多 65536 字节。引导与 follow-up 各最多排队 64 条，超过限制明确返回失败。Shell 使用当前用户权限，不是沙箱；Unix 取消会杀死进程组，其他平台只保证处理直接子进程。

## 响应与结束事件

命令应答携带原始 `id`，表示命令是否接受，不表示任务完成：

```json
{"type":"response","command":"prompt","id":"p1","success":true,"data":{"status":"started"}}
```

执行期间保留 `text`、`tool_started`、`tool_finished` 等事件。`done` 表示一轮已成功保存，但后续引导或队列仍可能继续。等待整个工作队列静止应使用 `agent_settled`：

```json
{"type":"agent_settled","status":"completed"}
```

`status` 还可能为 `cancelled` 或 `error`。`error` 状态前会发送错误事件。EOF 关闭时不保证发送结束事件，客户端需同时处理进程退出。

## 调度、配额与结构化事件

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

- `scheduler.model.activeByProviderHash` / `cooldownMs` 使用 endpoint + 凭据的 SHA-256 摘要作为键，不含原文；429 的 `Retry-After` 冷却也记在这里。
- `scheduler.shared` 只返回跨进程配置与冷却；跨进程持有者无法可靠枚举，因此**不返回全局 active 计数**。共享状态不可用时返回 `{"error": "..."}`。
- `scheduler.model.activeByProviderHash` 与 `budget` 均为本进程计数。`budget.maxCalls` / `maxTokens` 为 `null` 表示未设置上限；`tokensReserved` 是在途预留，`tokensUsed` 是结算后计数，`overage` > 0 表示实际用量超出预留、后续请求会被阻止。这是 token 计数，不是费用账单。
- `childTasks.queued` 对应 `SubagentManager` 的 `Queued` 状态，`limit` 来自 `--max-subagents`；子代理在并发上限或工作区忙碌时排队而非拒绝。

运行中还会发送结构化事件：

```json
{"type":"completion_call","data":{"runId":"...","callIndex":0,"usage":{"inputTokens":100,"outputTokens":20},"finishReason":"stop"}}
{"type":"diagnostic","data":{"runId":"...","category":"rate_limit","message":"...","retryable":true,"httpStatus":429}}
```

`usage` 中未上报的计数为 `null`，不会伪造成 0；`completion_call` / `diagnostic` 均不含凭据、请求体或原始响应。`diagnostic.category` 为稳定分类（如 `rate_limit`、`authentication`、`scheduling`、`cancelled`），不是原始错误文本。

## 工具审批流程 (--approve)

在启动时传入 `--approve` 参数后，RPC 模式下写文件、执行 Shell 等敏感操作会触发非阻塞的异步审批通知，而不会像非 TTY 文本或 `--json` 模式下直接拒绝。

审批请求由引擎向客户端发出：

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

## 模型切换

空闲时可发送 `{"type":"set_model","id":"m1","model":"deepseek/deepseek-chat"}`，或在文本交互中使用 `/model deepseek/deepseek-chat`。只写模型名会沿用当前连接；带供应商前缀则同时切换对应端点、凭据和账户服务。切回已使用的供应商会恢复本进程保存的连接配置（包括启动时的自定义地址）。首次切到 `custom` 时可通过 `MIO_BASE_URL` 指定地址。

命名供应商优先使用其专用密钥环境变量，再使用 `MIO_API_KEY`；不会使用其他供应商的专用密钥兜底。`custom` 使用 `MIO_API_KEY` 或 `OPENAI_API_KEY`。缺少 Antigravity 账户或不支持的原生协议会返回失败并保留原配置；不会静默换供应商。`get_state.provider` 返回当前供应商名称。

后续子任务同步采用新模型配置，已完成子会话的历史保留。有活动子任务时拒绝切换。思考等级通过原生参数传给通用 OpenAI 兼容接口和 Codex；实际支持取决于模型。DeepSeek API 支持 off/low/high/max，其他等级明确拒绝；DeepSeek 网页适配器暂不支持开启思考。Antigravity 保留专用映射。交互调整同步到后续子任务。

## 每次请求的上下文预算

`get_state.data.contextBudget` 返回 `context_window`、`max_output_tokens` 和来源 `catalog` / `override` / `fallback`。模型切换会重新查询仓库内的容量目录，显式覆盖值保持不变；后续子任务继承相同设置。

启动参数 `--context-window N`（至少 1024）覆盖部署容量；`--max-output-tokens N` 同时指定输出预留和原生请求上限。默认输出预留是 4096、容量的四分之一、目录输出上限三者的最小值；还保留 5% 余量。未知模型容量回退 32768，并非对服务端真实容量的保证。DeepSeek 网页适配器不支持输出上限，因此拒绝显式设置；默认值仅参与输入预算。

每次模型调用前（包括工具结果续轮）都计算系统指令、实际工具定义、历史和当前消息的序列化 UTF-8 字节及格式开销，作为保守 token 估算；这不是精确 tokenizer。仅在完整工具交互边界压缩旧内容，保留系统消息和当前工具调用/结果。预算摘要最多 4096 字节、优先取近期请求、发现和工具结果；可能丢失细节。预算压缩只改变本次请求，不改写保存的完整历史。手动 `compact` 和可选的 512 KiB `--auto-compact` 仍用于会话历史压缩。

无法容纳当前输入、指令、工具定义或活动工具结果时，发出明确的 `Context budget:` 错误并以 `settled(error)` 结束，后续提示仍可继续；不会把超限请求发给模型。当前估算只支持文本上下文，媒体消息明确拒绝。目录可能滞后于部署，需按实际容量覆盖参数。
