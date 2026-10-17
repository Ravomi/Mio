# Mio SDK 契约与平台 / feature 矩阵

> **状态（诚实边界）**
> - 本文档收口 TODO「CLI 拆分、SDK 契约与分发」(`REQ-SDK` / `REQ-DISTRIBUTION` / `AUD-17/20`) 的**基础**部分：依赖方向、发布版本闭包、许可证闭包、feature 矩阵、示例编译与独立 CI 平台矩阵计划。
> - 已在**本机 Linux x86_64**（Rust 1.95.0）主树验证：静态契约、31 项 Python 自检、27/27 组示例 / feature 编译；完整宿主 4 项契约与成功/模型失败/取消三场景通过。证据见 [主树验收记录](reviews/2026-10-16-v1-acceptance.md)。
> - **macOS 与 Windows 真机构建尚未执行**，仅在 `.github/workflows/sdk.yml` 中给出 native 矩阵计划；**crates.io / npm / GitHub Release 发布未执行、未宣称完成**。
> - 脚本运行**不需要任何外部凭据**；只有 `--run-local --online` 会从 crates.io 拉取公共 crate（无需 token），可选。
> - 本文件与 `scripts/check-sdk.py` 不改动 `TODO`、`PRD` 或 Cargo 清单；CLI→SDK 语义映射门禁（`scripts/check-cli-semantics.py` + `tests/cli_semantics.py`）已加入主 CI `ci.yml`，`release.yml` 未改动。

---

## 1. 范围

覆盖：

1. 三层 SDK 公共 API 与 helper crate 的依赖方向检查（`mio-core` 不得依赖上层 / 终端 / 工作区；`mio-agent` 不得依赖 coding 层）。
2. helper crate 的依赖方向与**发布路径版本闭包**核对。
3. 依赖许可证元数据闭包核对（缺元数据必须 fail，或显式 unknown，不冒充已验收）。
4. 可离线执行的本地 / 平台 feature 验收命令与最小 / 完整示例编译。
5. 独立 CI 的 native Linux / macOS / Windows 矩阵计划。

不覆盖：真实 Provider 联调、OS 沙箱、registry 实际发布、wasm32 target 构建、macOS / Windows 真机结果。

---

## 2. SDK 分层与依赖方向

检查器按 `scripts/check-sdk.py` 中的 `LAYERS` 策略，用 `cargo metadata --locked --offline` 解析出的**实际 resolve 边**（非 manifest 文本）判定方向。普通依赖（`kind = null`）只能指向严格更低层；`dev` / `build` 边不参与方向判定（dev 依赖可形成合法的反向环）。

| crate | 层 | 职责 | 允许依赖 |
| :--- | :---: | :--- | :--- |
| `mio-derive` | L0 | proc-macro 基础 | 无内部依赖 |
| `mio-http` | L0 | 传输契约（HTTP / WebSocket trait） | 无内部依赖 |
| `mio-reqwest` | L1 | HTTP 传输 helper | `mio-http` |
| `mio-tungstenite` | L1 | WebSocket 传输 helper | `mio-http` |
| `mio-core` | L2 | 模型 / Provider 契约与协议 | L0 + L1 |
| `mio-agent` | L3 | Agent 运行时 | `mio-core`、`mio-derive` |
| `mio-coding-agent` | L4 | coding 层：CLI、终端、RPC、审批、Runner、工作区 | `mio-core`、`mio-agent`、`mio-reqwest` |

派生的命名不变量：

- `mio-core` **不得**依赖 `mio-agent` / `mio-coding-agent`（终端、RPC、审批、Runner、工作区全部位于 L4，因此「不得依赖终端 / 工作区」等价于「不得依赖 L4」）。
- `mio-agent` **不得**依赖 `mio-coding-agent`。
- `mio-http` / `mio-derive` **不得**依赖任何其它内部 crate；transports 只能依赖 `mio-http`。

新增 workspace crate 若未登记进 `LAYERS`，检查器直接失败（`unclassified-workspace-crate`），避免策略悄悄过期。

`tests/sdk_contract.py` 用合成有向图构造负例（`mio-core -> mio-agent`、`mio-agent -> mio-coding-agent`、`mio-reqwest -> mio-core`、自依赖、未登记 crate），并断言「移除反向边后判定翻转为通过」，证明门禁确实按边阻断，而不是回显 manifest 顺序。

---

## 3. 离线验收命令

```bash
# 静态契约（默认离线）：依赖方向 + 版本闭包 + 打包卫生 + 许可证闭包 + feature 矩阵
python3 scripts/check-sdk.py

# 机器可读报告（JSON）
python3 scripts/check-sdk.py --json /tmp/mio-sdk-qa/sdk-report.json

# 发布门槛：把版本闭包 / 打包卫生 findings 视为失败
python3 scripts/check-sdk.py --strict-publish

# 有向图负例与单元自检（离线）
python3 -B tests/sdk_contract.py

# 本机示例 / feature 编译矩阵（离线，日志写入 /tmp/mio-sdk-qa）
python3 scripts/check-sdk.py --run-local

# 冷缓存时补齐需要拉取公共 crate 的组合（无需凭据）
python3 scripts/check-sdk.py --run-local --online --only-network
```

退出码语义：

- `0`：所有默认门禁通过（版本闭包 / 打包卫生 finding 不阻塞默认退出）。
- `1`：依赖方向、许可证缺元数据、feature 引用不存在，或 `--strict-publish` 下版本闭包 / 打包卫生不达标。
- `2`：无法取得 `cargo metadata`（例如离线缓存缺失、清单损坏），失败原因直接打印。

冷缓存行为：`--run-local` 对 `--offline` 下无法下载的 crate 记为 `skip-needs-network` 而非失败，报告 `status=pass-with-skips`，不会把未运行的组合冒充已验证。

---

## 模型推理增量的宿主回调

`Host::on_reasoning(run_id, part, text)` 接收 Provider 实际流出的推理文本增量；默认实现为空，已有宿主无需修改。`run_id` 是该次引擎运行的真实 UUID，`part` 是响应内的内容块序号。它与 `AgentEvent::Text` 分开，子代理通过同一回调转发真实子运行 UUID，推理正文不进入子代理的答案快照。回调须保持非阻塞；流式增量在取消或输出背压时不构成已保存会话的证据。CLI/RPC 的事件封套与能力发现见 [RPC 协议](rpc.md)。

## SDK 图片回合

`run_with_tools_message` / `run_with_tools_message_tracked` 接收 `mio_core::message::Message::User`，与文字入口走同一引擎和任务状态机。图片输入限定为本地目录已标注支持图片的模型：OpenAI 兼容路由（`Custom` / `Deepseek`）及 Anthropic、Gemini、Ollama 原生路由。每回合最多 4 张、单张解码后最多 512 KiB、合计最多 1 MiB，数据必须是内联 base64 PNG/JPEG/GIF/WebP，媒体类型须与文件头一致。不读取远程 URL 或 Provider 文件 ID，也不接受带 Provider 原生元数据的用户图片。模型目录描述的是本地能力信息，不等于线上端点已验证；Provider 拒绝图片时该次运行照常报告错误。

上下文准入对每张图片预留 32768 个估算单位，另计文字和结构开销；这是保守的本地容量预算，不是 Provider 的真实图片 token 数。实际 usage 仍以 Provider 报告为准。Ollama 目录未给部分模型的上下文窗口，宿主需按本地部署的真实容量显式设置 `context_window`，否则保守预算可能在发请求前拒绝图片。返回的 `Vec<Message>` 保留原始图片块；嵌入宿主需按自身会话策略持久化。Mio RPC 已接入受限 `images` 字段，运行中的图片 `steer` 仍明确拒绝（见 [RPC 协议](rpc.md)）。本地 SDK 回环已验证 Anthropic、Gemini、Ollama 的原生出站图片形状；RPC 子进程回环已验证 OpenAI 兼容及三个原生路由的会话保存与重放。GUI 和真实视觉模型仍待验收。

---

## 执行隔离的宿主契约

已有宿主默认使用 `Unrestricted`，旧 `Host::shell` 继续可用。选择隔离时必须实现 `Host::shell_with_policy`，按传入的策略执行并保留自身禁用命令的规则；默认实现会拒绝，避免旧回调静默绕过隔离。工作区和扩展批次固定使用各自捕获的策略快照。

隔离运行拒绝任意 `RunTools.extra_tools`。原生插件与 stdio MCP 应通过 `mcp::load_policy_bound_tools` 取得不可手工构造的容器，放入 `RunTools.policy_bound_tools`；运行前再次校验策略指纹。Provider 网络客户端在宿主进程内，不属于工具子进程沙箱。

### 外置输出的宿主生命周期

`Workspace` 在构造时取得工作区共享 activity 租约，最后一个克隆释放时归还。CLI/RPC 在整个进程使用期持有同一租约，包含尚未产生首个输出、空闲及无会话运行；锁位于项目外，不会为纯问答创建 `.mio`。

宿主若在两个 run 之间只保留内存历史，其中可能引用 `.mio/outputs`，应通过 `mio_coding_agent::shell::workspace_activity::shared_for_workspace` 取得并持续持有租约。返回 `Ok(None)` 表示清理正在独占该工作区，宿主须等待或拒绝继续使用引用；不能将它当作取得保护。底层无法推断宿主私有内存中的引用。

离线清理确认在存在共享租约时拒绝执行；预览不是删除授权。该保护依赖参与方遵守租约，非 Unix 平台尚不支持清理独占保护。输出生命周期与清理已进入主树整合门禁，Linux 真实 ENOSPC 专项也已通过；冻结版本与证据范围见 [主树验收记录](reviews/2026-10-16-v1-acceptance.md)，跨平台仍未验收。

引用发布意图账本（DES-17）：当引擎把超预算工具输出外置时，它在写入文件与索引之后、调用 `Host::on_tool_call_lifecycle` 之前，先向工作区内的 `.mio/output-index/publication-intents.json` 写入一条 durable、版本化、有界的 `Pending` 意图（本地 `attempt_id`，记录真实 run scope、Provider `call_id` 与 typed 引用元数据；不保存正文或工具参数）。`Host` 回调成功时意图被移交常规 journal/session 引用扫描；返回 `Err` 时保持 `PendingUncertain`，其引用精确受清理保护，保留期不会静默回收。宿主可显式调用 `mio_coding_agent::shell::output_store::OutputStore::reconcile_publication_intent(attempt_id, snapshot, PublicationResolution::{Published | Unpublished})`，依据真实持久化现场注销意图；该 API 不在模型工具面内，旧快照与缺失身份一律拒绝，且不会依据时间或 PID 死亡自动断言未发布。`OutputStore::publication_intents` 提供有界摘要，`OutputStore::open_readonly` 可在不创建任何目录/文件的前提下恢复读取。该账本只表达本地发布意图，不冒充外部 Host 的磁盘持久化证明；没有持久 sink 的宿主仍按索引保留期回收，账本在成功路径上不会永久膨胀。

### 禁用 shell 工具

CLI `--no-shell` 使用公共 `ToolFilter::without_shell()`，SDK 将同一过滤器放入 `RunTools.tool_filter` 即可采用相同准入规则。命名的 `bash` / `shell` 从广告工具集中移除，模型仍调用这些名字时由协议层拒绝；聚合 `workspace` 的 shell/bash 操作由统一 policy 在工具正文和普通审批之前拒绝。文件工具继续受原有 allow/deny 与父权限上限约束。

此选项不隔离任意插件/MCP 子进程；需要进程隔离时使用明确的 backend policy。CLI 的宿主拒绝仍作为额外防线保留。专项 CLI/SDK 真实 loopback 及过滤器/策略定向共 21 项通过，证据 `target/evals/design-implementation-20261015/shared-no-shell-qa/`；这只验证该准入语义，不代表全部 SDK/宿主契约已完成。

### 工具并发

`RunOptions.tool_concurrency` 与 CLI `--tool-concurrency` 使用同一引擎配置，范围 1–32，默认 1。宿主显式增加并发时，同一轮工具可以乱序完成；事件使用每次派发的真实调用 ID 关联。工作区写入租约、审批和重任务配额继续分别生效。动态扩展自行维护其内部共享状态，这个参数不提供扩展代码的线程安全或系统隔离保证。RPC `get_state` 的 `toolConcurrency` 返回有效配置。

Rust 宿主可实现 `Host::output_schema() -> Option<OutputSchema>`，为该宿主的一次运行指定最终答复的 JSON Schema；默认方法返回 `None`，已有宿主和 `RunOptions` 字面构造不需改动。引擎在运行开始时只读取一次；Schema 序列化后超过 32 KiB 会在模型请求前拒绝。底层根据 Provider 能力选择原生格式、结果工具或提示约束。委派子代理的 `ChildHost` 沿用默认 `None`，不会自动继承父运行格式。`tests/output_schema_sdk.rs` 的本地回环验证无工具请求带上原生 `response_format`，以及真实 `read` 工具结果进入下一轮后才发送原生 schema；超限请求不会触达模型。这是输出**格式**约束，不证明审查结论或其它内容正确；宿主仍应独立核对来源文件和业务判定。CLI 一次性运行与 `--mode rpc` 亦可通过 `--output-schema PATH` 读取有界 schema 文件并写入同一 `Host::output_schema` 入口（由 `tests/output_schema_cli.rs` 本地回环覆盖），但这仍是通用输出格式约束而非内建代码审查模式。

CLI/RPC schema 的本地工程验收和未覆盖范围见 [CLI/RPC 接入记录](reviews/2026-10-17-p2-output-schema-cli.md)。

### 正常结束与目标验收

`run*` 正常返回、CLI 退出码0和任务 `Completed` 表示引擎运行正常结束，不保证业务目标或产物正确。宿主须单独执行所需验收并保存结果；模型报告未完成或未验证后的正常结束也遵循这一边界。实际 worker 是否已收回仍由权威任务生命周期判断。

## 4. 示例与 feature / 平台矩阵

`scripts/check-sdk.py` 的 `FEATURE_MATRIX` 是唯一矩阵来源；每个 feature 名都会先在 metadata 中校验存在性，不存在即失败，避免文档写入手工臆造的 feature。

### 4.1 示例（最小 / 完整宿主）

| 组合 | 用途 | 本机结果 |
| :--- | :--- | :---: |
| `cargo check -p mio-agent --examples` | 最小 SDK 消费者：仅 agent 运行时，不引入 coding 层 | pass |
| `cargo check -p mio-coding-agent --example host` | 完整宿主嵌入：`Host` / `AgentEvent` / `Services` API 编译 | pass |
| `cargo check -p mio-coding-agent --example host_loopback` | 完整离线宿主示例：`run_with_tools_tracked` + 真实 `TaskHandle`/journal/绑定审批三场景 | 主树编译与 4 项契约通过 |

这些示例只做编译校验，不需要 API key；`examples/host.rs` 在运行期才读取 `OPENAI_API_KEY`。[examples/host_loopback.rs](../examples/host_loopback.rs) 完全不读环境凭据，只在 `127.0.0.1` 回环 mock 上运行三个场景，对应 `tests/host_example_contract.rs` 的 4 项契约测试（含只批准绑定写入并拒绝未知确认）。这 4 项测试已在主树通过：

```bash
cargo run --example host_loopback -- --scenario success
cargo run --example host_loopback -- --scenario model-error
cargo run --example host_loopback -- --scenario cancel
```

### 4.2 helper feature 矩阵

`[net]` 表示该组合的传递 crate 不保证在冷缓存中，离线本机运行会记为 `skip-needs-network`；用 `--online` 可补齐（公共 crate，无凭据）。

| crate | feature 选择 | 说明 | 本机 |
| :--- | :--- | :--- | :---: |
| `mio-derive` | `--examples` | proc-macro 示例 | pass |
| `mio-http` | 默认 | 传输契约 | pass |
| `mio-http` | `websocket` | WebSocket 契约 | pass |
| `mio-http` | `test-utils` | 测试传输 | pass |
| `mio-reqwest` | `--no-default-features` | 无 TLS 后端 | pass |
| `mio-reqwest` | `rustls` | 默认 rustls 传输 | pass |
| `mio-reqwest` | `--no-default-features --features native-tls` | 平台原生 TLS | pass |
| `mio-reqwest` | `socks` | SOCKS 代理 | pass |
| `mio-reqwest` | `reqwest-middleware` | rustls 上的中间件栈 `[net]` | pass |
| `mio-reqwest` | `--no-default-features --features reqwest-middleware-native-tls` | native TLS 上的中间件栈 `[net]` | pass |
| `mio-tungstenite` | `--no-default-features` | 无 TLS 后端 | pass |
| `mio-tungstenite` | `rustls` | 默认 rustls | pass |
| `mio-tungstenite` | `--no-default-features --features native-tls` | 平台原生 TLS | pass |
| `mio-core` | `--no-default-features` | 无 Provider / 传输 | pass |
| `mio-core` | 默认（`derive`,`rustls`） | 默认能力 | pass |
| `mio-core` | `--no-default-features --features reqwest` | + reqwest 传输 | pass |
| `mio-core` | `--no-default-features --features reqwest,native-tls` | + 平台原生 TLS | pass |
| `mio-core` | `tungstenite` | WebSocket 协议 + tungstenite 后端 | pass |
| `mio-core` | `audio,image,test-utils` | 能力开关与测试工具 | pass |
| `mio-core` | `pdf` `[net]` | PDF 解析 | pass |
| `mio-core` | `epub` `[net]` | EPUB 解析 | pass |
| `mio-agent` | `--no-default-features` | 无 derive | pass |
| `mio-agent` | `image,audio` | 能力开关 | pass |
| `mio-agent` | `test-utils` | agent 测试工具 | pass |

平台相关说明（来自清单，不是实测结论）：

- `mio-tungstenite` 仅在 `cfg(not(target_family = "wasm"))` 下被 `mio-core` 依赖；wasm32 不在本矩阵内。
- `rustls` 与 `native-tls` 是 `mio-core` / transports 的互斥 TLS 后端选择；native 后端在 Windows 使用 Schannel、macOS 使用 Security.framework，**这两条只有云端 CI 运行后才能作为证据**。
- `pdf` / `epub` 依赖仅在 native 目标可用。

---

## 5. 许可证与发布闭包

### 5.1 许可证元数据闭包

- 覆盖 `cargo metadata` 解析出的**全部 330 个包**（含 workspace 与 registry 依赖）。
- 缺 `license` 且缺 `license_file` 的包：默认 **fail**；`--allow-unknown-license` 时记 `status=unknown` 并显式置 `license_metadata_complete=false`，不冒充已验收。
- 本次快照：0 个包缺元数据，元数据 `license_metadata_complete=true`；不代表原始文本或义务审核完成。
- 14 个包使用非严格 SPDX 的 `/` 分隔写法（如 `MIT/Apache-2.0`、`Unlicense/MIT`）或需要人工复核的表达式；脚本将其列为 `review`，不阻塞默认门禁，但**不代表 SPDX 归一化已验收**。涉及 crate：`core_detect`、`fnv`、`futures-timer`、`generator`、`minicov`、`quick-error`、`rusty-fork`、`same-file`、`scoped-tls`、`send_wrapper`、`serde_urlencoded`、`version_check`、`wait-timeout`、`walkdir`。

### 5.2 发布路径版本闭包

`cargo package` / `cargo publish` 会剥离 path 依赖，要求同仓库依赖带版本要求。检查器对每个非 dev 的 workspace path 依赖核对：

- `req` 不能是 `*`（即 manifest 未写 `version`）；
- `req` 必须匹配目标 crate 的 workspace 版本。

本次快照：11 个普通 workspace path 依赖均带与 0.1.0 匹配的版本要求（新增缺项使用 `version = "=0.1.0"`），版本闭包通过。

快照说明：同一命令在早期快照（工作树尚未给 path 依赖补 `version`）曾报告 6 条 finding。检查器逻辑未变，闭包按工作树快照每次重新核对；报告中的 `generated_at` 与 `metadata_source` 用于绑定快照。

### 5.3 上游归档文件与打包卫生

`crates/*/Cargo.toml.orig` 与 `.cargo_vcs_info.json` 是上游归档证据，继续保留在源码仓库。各 helper crate 已用精确的 `package.exclude` 规则排除它们，避免 Cargo 把原始归档文件与自行生成的包元数据混淆。

检查器识别精确根路径排除规则；没有明确排除或被 `include` 覆盖时仍报告 finding。`cargo package --locked --offline --allow-dirty --list -p mio-http` 已通过；列表中的 `Cargo.toml.orig` 与 `.cargo_vcs_info.json` 是 Cargo 生成的合法包文件，并非重新包含被排除的上游档案。

```bash
python3 scripts/check-sdk.py --strict-publish # rc=0，版本与源码打包卫生检查通过
```

这只验证发布的静态前提，不证明所有 crate 已在 registry 可解析、打包验证完成或发布成功。许可证检查也只证明元数据完整，文本与义务审核另行记录。

---

## 6. 独立 CI native 平台矩阵计划

新增独立工作流 `.github/workflows/sdk.yml`，不覆盖也不修改主 `ci.yml`：

- `sdk-contract`（`ubuntu-latest`）：`python3 scripts/check-sdk.py`、`python3 -B tests/sdk_contract.py`，上传 JSON 报告 artifact。
- `examples-and-features`（native matrix）：

| runner | 覆盖 |
| :--- | :--- |
| `ubuntu-latest` | 最小示例、完整宿主示例、全部 helper feature 组合 |
| `macos-15` | 同上；native-tls 走 Security.framework |
| `windows-latest` | 同上；native-tls 走 Schannel |

CI 步骤使用 `--locked`（联网抓取公共 crate，**无 secrets、无外部凭据**），不传 `--offline`，因此 `[net]` 组合也会被真实执行。

**待云端执行证据（尚未运行）**：

- macOS / Windows runner 是否全部组合 `rc=0`；
- Windows 上 `mio-core --features pdf,epub` 与 `native-tls` 的结果；
- 各 runner 的 JSON 报告与日志 artifact；
- 尚未接入 wasm32 target 检查（无 runner 计划，明确不在本矩阵内）。

在这些证据产出前，本文档只声明 Linux 本机结果，不声明跨平台完成。

---

## 7. 本机执行证据

- 环境：Fedora 44 / Linux x86_64（kernel 7.2.9）、`rustc 1.95.0`、`cargo 1.95.0`、Python 3.14.8、工作树 HEAD `2a17bab`（含未提交改动）。
- 时间：`2026-10-15T11:39Z`（各报告 `generated_at` 为准）。
- 日志目录：`/tmp/mio-sdk-qa`（26 个 `cargo check` 日志 + `sdk-report.json` + `sdk-local-matrix.json`）。
- 下表为早期 26 组历史记录。2026-10-16 主树已实际执行 27 组：27 pass / 0 fail / 0 skip，独立日志与命令、退出码核验通过；SDK 自检为 31 项。新增宿主 4 项契约及三场景通过。最新证据见 [主树验收记录](reviews/2026-10-16-v1-acceptance.md)。

| 命令 | 结果 |
| :--- | :--- |
| `python3 scripts/check-sdk.py`（静态，含新增第 27 组） | rc=0；方向 77 条普通边通过；版本闭包 11/11；打包卫生通过；许可证元数据 330 包 0 缺失；feature 矩阵 27 组合全部存在（新增 `host_loopback` 已在 metadata 中解析） |
| `python3 -B tests/sdk_contract.py` | rc=0；30 tests OK |
| `python3 scripts/check-sdk.py --run-local` | rc=0；26/26 pass，0 fail，0 skip（缓存已预热） |
| `python3 scripts/check-sdk.py --run-local --online --only-network` | rc=0；4/4 `[net]` 组合通过 |
| `python3 scripts/check-sdk.py --strict-publish` | rc=0；版本与源码卫生静态检查通过 |
| `cargo metadata --locked --offline`（脚本内部） | rc=0，330 包 |

---

## 8. 未完成与后续

- 保持上游归档证据与打包排除规则；后续验证所有 crate 的实际归档、registry 解析与许可证文本清单。
- 运行 `.github/workflows/sdk.yml` 并归档 macOS / Windows 证据。
- 后续 SDK 变更继续执行 27 组 feature 矩阵与 4 项宿主契约；本轮主树结果已通过。
- 14 个非严格 SPDX 表达式的人工归属复核。
- 未执行：crates.io 实际发布、npm 包发布、GitHub Release 上传、wasm32 构建、真实 Provider 联调。
