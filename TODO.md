# Mio TODO

以下均为待办或需要补齐验收的能力，不代表当前已具备完整保障。

## 审查与需求入口

本轮审查日期：2026-10-15。代码基线：`60cfb8c82f419e6c3e8320db84c880e3bb19fd46`，仅指首次审查时点。首次审查只整理调查与待办，不修复实现；其后工作区（未提交）落入批次 2/3 实现，见下。共享调度已接入真实 dispatch 边界；跨进程 Workspace 租约已接入工作区执行路径，四项质量门禁均已通过。

- [项目审查、复现结果与验证范围](docs/reviews/2026-10-15-audit.md)
- [竞品调查、Mio 优势判断、需求与验证调查表](docs/agent-requirements-and-comparison.md)

优先级：`P1` 为影响执行控制、主要连接能力或资源可用性的近期工作；`P2` 为随后补齐的正确性、可靠性和维护工作；`P3` 为需要产品选择和评测支持的增强。`AUD-*` 对应审查证据，`REQ-*` 对应需求验收。未勾选不代表均为 bug，状态以审查表为准。

## 近期修复与验证

修复批次 1（`AUD-01/02/05/06/07/08/09/10/11`）已实现并由回归测试覆盖。其后工作区落入批次 2：`AUD-03/04/12/13/19` 已有实现与离线测试（见下）。批次 3 又落入共享调度实现：`src/scheduling.rs`（真实 dispatch hook）、`src/coordinator.rs`（进程内准入与预算）、`src/shared_limits.rs`（跨进程 `flock` 配额与 workspace 租约模块），以及结构化诊断/`completion_call`。证据以测试名与实跑命令为准，不以文档代替验证；没有实际运行或真实故障注入的项仍标为待验收。最终验收由 Pi 串行执行，主代理核对真实退出码与日志；结果见 `/tmp/mio-final-qa-rerun/`；四项质量门禁均已通过。

- [x] **AUD-01 / P1**：插件命令采用统一进程监督；abort、EOF、超时后清理子进程及后代，取消确认后不得继续产生新的插件副作用。
  - 实现：`src/shell.rs` 新增 `run_supervised`（Unix 独立进程组 + `kill_on_drop` + 每流 32 KiB 限额），`src/plugin.rs` 的 `execute_plugin_command_with_timeout` 复用该机制并保留 60s 超时；future 被丢弃/超时时 `Process::drop` 同步对进程组发 `SIGKILL` 并调用 `start_kill`。**发出 SIGKILL 不等于同步 `wait` 完成回收**：子进程收尸由后台任务异步执行，`agent_settled` 之前只有信号发出与后续副作用阻断的保证，不是进程已完全回收的保证。
  - 验收：`plugin::tests::test_plugin_cancellation_kills_process_group`（abort 后 `after-abort.txt` 不出现）、`test_plugin_timeout_kills_process_group`（超时后轮询确认子进程已消失或处于 zombie）、`test_plugin_output_is_bounded`、`shell::tests::cancellation_kills_background_process_group`。这些测试观察到的是副作用文件不出现或子进程不可见/zombie，属于时序与存活观察，**不能证明 `wait` 已同步回收**。
- [x] **AUD-02 / P1**：修复插件工具过滤：精确 `plugin__…` 名称可解析，完整名称和别名共同遵守 deny 优先；覆盖通配符排除绕过。
  - 实现：`src/pattern.rs` 的 `validate_tool_pattern` 接受精确 `plugin__<plugin>__<tool>`；新增 `ToolFilter::allows_aliased`，任一等价名命中排除即拒绝（deny 优先），`src/plugin.rs` 以 `[qualified, short]` 调用。
  - 验收：`pattern::tests::test_tool_filter_accepts_exact_plugin_names`、`test_tool_filter_deny_wins_over_aliases`、`plugin::tests::test_plugin_tool_filter_exact_and_deny_priority`；CLI 实测 `mio --exclude-tools plugin__probe__late …` 不再报 “Unsupported tool”，`plugin__` 畸形名被明确拒绝。
- [x] **AUD-03 / P1**：明确项目 MCP 配置的信任与启动授权；仅允许 `read` 或开启审批时，未经信任的项目配置不得借发现工具启动命令。不得把工具调用过滤宣称为进程沙箱。
  - 实现：`src/mcp.rs` 按来源区分全局/项目配置；项目作用域服务器在任何发现/过滤逻辑之前调用 `host.approve_tool_async(PROJECT_MCP_APPROVAL_OP, fingerprint)`，授权绑定命令/参数/环境变量指纹，拒绝则不连接（fail-closed），因此 `--tools read` 不能绕过启动授权。
  - 验收：`mcp::tests::test_project_mcp_requires_approval_before_spawn`、`test_trusted_project_mcp_server_starts_and_lists_tools`、`test_project_mcp_filter_cannot_bypass_approval`。
  - 残余：尚无持久化 trust store，授权不跨运行复用；非交互 CLI/RPC 下项目 MCP 的 fail-closed 行为未端到端验证；工具过滤仍不是进程沙箱。
- [x] **AUD-04 / P1**：修复供应商预设与请求协议的错配；Anthropic 直连使用原生适配，兼容端点明确标识；验证真实请求路径、认证头和 thinking 参数。
  - 实现：`src/lib.rs` 新增原生 `Provider::{Anthropic, Google, Ollama}` 分支与 `native_base_url` 归一化，Anthropic 走 Messages、Google 走 GenerateContent、Ollama 走 `/api/chat`；`Provider::Custom`/`Codex` 保留 OpenAI Chat 语义，兼容网关必须显式选 `custom`；`src/rpc.rs` 预设映射到对应原生协议。
  - 验收：`tests/provider_routes.rs` 的 `presets_route_to_matching_protocols`、`anthropic_preset_speaks_native_messages`、`google_preset_speaks_native_generate_content`、`ollama_preset_speaks_native_chat`、`custom_provider_keeps_openai_chat_semantics`、`native_thinking_validation_uses_the_matching_catalog`、`model_spec_shorthand_selects_native_providers`、`context_budget_reads_native_catalog_windows`、`provider_serde_round_trips_for_model_switching`、`subagents_accept_native_providers_and_switch_models`（均为本地模拟端点契约测试）。
  - 残余：**未联网验证真实 Provider**。认证头、thinking 参数、用量字段与错误分类仅在离线模拟端点上核对，真实服务路径待授权后另行标记。
- [x] **AUD-05 / P1**：为插件输出、MCP 协议帧、RPC 输入行、扩展配置与返回结果设置大小/时间限额；协议帧超限明确失败，展示内容可截断（**完整输出尚未外置到文件**）。
  - 实现：插件 stdout/stderr 各 32 KiB 截断；`src/rpc.rs` 新增通用 `read_bounded_line`，MCP 收发帧、`mcp.json`、`mio-plugin.json` 各 1 MiB；RPC 输入行 1 MiB，超限排空整行后显式报错并继续；MCP 超限帧明确失败。
  - 验收：`mcp::tests::test_read_bounded_line_rejects_and_drains_oversized_frames`、`test_mcp_outgoing_oversized_frame_is_rejected`、`plugin::tests::test_plugin_output_is_bounded`。残余：插件超限输出仅内联截断并附 `[TRUNCATED]` 标记，**截断不是外置**：完整内容既未写入文件也未提供引用，调用方只能看到被截断部分。
- [x] **AUD-06 / P1**：修复正则搜索平方增长；采用线性搜索实现或成熟引擎，增加长行无匹配与取消资源回收验收。
  - 实现：`src/pattern.rs` 的 `NfaRegex::is_match` 改为单次左到右扫描：状态集合同时包含所有起点可达状态，每字符每状态最多访问一次（O(n·states)），锚点/词界语义不变。
  - 验收：`pattern::tests::test_regex_matches_without_restarting_per_offset`，20 000 字符长行 `.*z` 无匹配在 10s 看门狗内完成，并覆盖 `^`/`$`/`\b`/取反字符类语义。
- [x] **AUD-12 / P1**：处理 `spawn_blocking` 文件操作取消后的实际完成状态；完成前保留写入租约，避免回报取消即释放重叠工作区。
  - 实现：`src/workspace.rs` 的 `spawn_leased` 把 `WorkspaceLease`（含 `ParentLease` 与写访问 guard）移动到 blocking worker 内部；外层 future 被取消只释放外层句柄，worker 结束前租约一直持有，新的重叠委托排队，直到 worker 释放租约。
  - 验收：`workspace::tests::cancellation_keeps_workspace_reserved_until_blocking_worker_finishes`（abort 后新 delegate 仍被拒，直到 worker 释放）。
  - 残余：该测试用 mpsc 人为阻塞 worker，属于可控构造，非真实磁盘故障注入；路径校验到实际操作之间的 TOCTOU 仍存在（见 AUD-19）。
- [x] **AUD-07 / P2**：插件路径逐组件校验并检查 canonical root，拒绝中间目录符号链接越界；补充发现和上下文加载验收。
  - 实现：`src/plugin.rs::validate_relative_subpath` 逐组件 `symlink_metadata` 拒绝符号链接，并对已存在路径做 canonical root 前缀校验；发现、安装复制、MCP 配置与 `src/context.rs` 上下文加载共用该边界。
  - 验收：`plugin::tests::test_plugin_context_linked_directory_is_rejected`。
- [x] **AUD-08 / P2**：MCP `tools/list` 遍历 `nextCursor`，限制页数/工具总量并检测重复游标。
  - 实现：`McpClient::list_tools` 循环翻页，上限 20 页 / 1000 工具，重复或空游标终止，超限报错。
  - 验收：`mcp::tests::test_mcp_tools_list_follows_pagination`。
- [x] **AUD-09 / P2**：MCP 和插件工具注册检测规范化名称冲突及超长名称，禁止静默覆盖/跳过，提供可定位诊断。
  - 实现：MCP 记录「规范化名 → 原名」映射，冲突时报出两个原始名；插件注册与 MCP 跨服务器注册均检测 qualified 名冲突并返回带来源的错误；`ToolName` 构造失败不再静默 `continue`，`load_dynamic_tools` 再做一次全局唯一性校验。
  - 验收：`mcp::tests::test_mcp_normalized_tool_name_collision_is_reported`、`plugin::tests::test_plugin_normalized_tool_name_collision_is_reported`。
- [x] **AUD-10 / P2**：同名会话明确报歧义；UUID 匹配优先于名称，测试大小写重名与短 UUID。
  - 实现：`Session::resolve` 分级匹配（精确 UUID > 唯一精确名 > 唯一 UUID 前缀 > 唯一名称子串），同级多命中显式报错。
  - 验收：`session::tests::test_resolve_prefers_uuid_and_reports_ambiguous_names`；CLI 实测重名 `--session duplicate` 返回 exit 1 且提示歧义，短 UUID 与唯一名称正常导出。
- [x] **AUD-11 / P2**：会话列表采用元数据索引或保证最新项的分页；超过 1000 项仍能准确 resume，明确目录截断与索引修复。
  - 实现：`Session::scan` 只读目录元数据并按 mtime 降序排序，`Session::list_with_limit` 在排序后截断并给出明确的截断警告；扫描条目上限 `MAX_SESSION_SCAN = 20_000`，超限时 `resolve` 拒绝猜测。
  - 验收：`session::tests::test_list_orders_by_mtime_before_applying_the_limit`、`src/bin/mio.rs` 的 `latest_session_uses_save_order_even_when_session_seconds_match`；CLI 实测 1050 个会话时 `--resume` 导出最新会话并打印 “1000 most recently modified … out of 1050” 警告。
  - 残余：`list_with_limit` 忽略 `truncated` 标志；目录超过 `MAX_SESSION_SCAN` 时扫描结果只是**部分列表**，`--resume` 的“最新”仍取自该部分集合，不能作为全局最新的证明。超过 20 000 项需索引或分页才可宣称准确。
- [x] **AUD-13 / P2，可靠性需求**：增加工具执行证据与检查点，保留失败/取消回合中已发生的副作用信息；恢复不得盲目重放，区分会话恢复与文件回滚。
  - 实现：新增 `src/journal.rs`（独立版本化 journal，目录 0700 / 文件 0600，原子替换，大小上限，超出丢弃最旧并计数）；`record_intent` 在副作用前落盘，失败则阻止操作；`RunState`/`OpStatus` 区分 completed/failed/cancelled/interrupted 与 executed/unknown；`mark_interrupted` 在重启时标记，**不自动重放**；`--no-session` 时完全禁用；`src/bin/mio.rs` 在回合结束写入证据。
  - 验收：`journal::tests` 的 `records_intent_result_and_verifies_permissions`、`failed_turn_keeps_executed_evidence_and_marks_unknown`、`cancelled_turn_marks_pending_unknown`、`restart_marks_active_run_interrupted_and_persists`、`write_failure_blocks_intent`、`disabled_journal_writes_nothing`、`size_limit_drops_oldest_runs`、`long_targets_and_details_are_truncated`、`corrupt_journal_is_preserved_and_blocks_new_side_effects`、`oversized_or_mismatched_journals_are_not_overwritten`、`symlinked_journal_directory_never_changes_external_evidence`。
  - 残余：尚未做端到端 CLI 崩溃注入；恢复报告是否暴露给用户、journal 与 session 恢复如何统一仍未验收；文件检查点未实现（journal 不提供回滚）。
- [ ] **AUD-14 / P2，能力边界**：压缩保留用户约束、未完成目标、决策和工具证据；保留原始记录，报告压缩事件及损失；评估 token 估算与稳定前缀。
- [ ] **AUD-15 / P2**：贯通底层已有 usage、错误类型、tool-call ID 和观察事件，产品层提供脱敏的结构化错误与关联 ID。
- [ ] **AUD-16 / P2**：明确 MCP 必选/可选服务，支持启动失败诊断、连接复用及失效重连；补充协商、取消和服务请求处理的兼容性测试。
- [ ] **AUD-17 / P2，部分已验证**：基线中传输/宏 helper 未纳入 workspace 测试；另一批改动已将其迁入 `crates/mio-*` 并加入 members，收尾四项质量检查通过且测试包含 helper。继续补 feature/平台矩阵和发布终端脚本。
- [ ] **AUD-18 / P2**：统一 CLI 预设、底层 catalog 和网站的能力来源；区分“有凭据”“已验证连接”“支持协议”和模型能力来源。
- [x] **AUD-19 / P2**：文件 edit 加内容版本/哈希前置条件与冲突检测；评估路径检查到操作之间的竞态，保留用户和其他任务修改。
  - 实现：`src/workspace.rs` 新增 `ensure_edit_source_unchanged`，在 persist 前按文件身份（dev/inode）与原始内容比对；同长度替换、被替换的身份、被替换为符号链接均拒绝，保留外部修改。
  - 验收：`workspace::tests::edit_conflict_detection_preserves_external_changes`、`edit_conflict_detection_rejects_replaced_identity_and_symlink`。
  - 残余：检查与 persist 之间仍有 TOCTOU 窗口，未使用基于目录句柄的操作；未做并发任务压力验收（见 REQ-SCHEDULE）。
- [ ] **AUD-20 / P2，工程维护**：按参数、终端、RPC、审批、Runner 拆分 CLI；记录 Rig 更新差异、可选特性验证和 registry 打包策略，保持许可证来源记录。

## 产品需求与评测

- [ ] **REQ-POLICY / P1**：统一工具门禁、项目信任和权限继承，覆盖内置、插件、MCP、extra_tools 与子代理。
- [ ] **REQ-PROCESS / P1**：提供共享的取消、超时、资源限额与进程清理接口。
- [ ] **REQ-PROVIDER / P1**：建立 Provider 原生/兼容协议及 thinking、用量、错误处理契约测试。
- [ ] **REQ-RECOVERY / P2**：实现持久化执行证据、可解释恢复和可选文件检查点。
- [ ] **REQ-EVENTS / P2**：版本化宿主事件与 RPC 能力协商，提供可重放状态链和压力测试。
- [ ] **REQ-CONTEXT / P2**：评估保真压缩、稳定前缀和大仓库符号检索；先测量再选 repo map、LSP 或索引。
- [ ] **REQ-EVAL / P2**：建立同模型、同任务、同预算的 agent 对比，记录成功率、正确性、成本、延迟、缓存、取消与恢复结果；不得以自测或不同模型分数宣称领先。
- [ ] **REQ-SDK / P2**：稳定 Rust 嵌入契约与宿主示例；先复用现有底层 hook/bus/observe，避免再造运行循环。
- [ ] **REQ-ISOLATE / P2**：执行环境可替换，真实 OS 沙箱按平台验收；worktree 只用于修改隔离与合并，不充当安全边界。
- [ ] **REQ-EXTENSIONS / P2**：插件/MCP 工具身份、权限、生命周期与协议 conformance；扩展失败可定位。
- [ ] **REQ-DISTRIBUTION / P2**：验证平台、安装、完整性、下载/解压限额和可重复发布；明确本地路径依赖的发布边界。
- [ ] **REQ-PRODUCT / P3**：确认首要用户是 Ravomi/终端宿主、独立 CLI 还是第三方 SDK；ACP、桌面 UI、计划模式、后台作业等按需求证据进入迭代。

## 近期验收：限流、并发、配额、预算与去重

以下对应用户明确的近期痛点。批次 3 已将共享调度接入真实 dispatch 边界，状态改为「已实现、已通过本轮离线验收」；**未联网真实 Provider、未跨平台、跨进程 token 预算未持久化**。

- [x] **REQ-SCHEDULE / P1，已实现并通过离线验收**：进程内 `Coordinator` 在模型/重任务 dispatch 前准入；跨进程 `SharedLimits`（`flock`）共享全局与每 provider 模型槽位（默认各 2）。429 按 `Retry-After`（秒或 IMF-fixdate GMT）发布 endpoint+凭据摘要键的冷却，父子/协作进程共享，**不自动重放工具或请求**。
  - 证据：`src/scheduling.rs`、`src/coordinator.rs`、`src/shared_limits.rs`；`tests/scheduling.rs`（9 项，含 `shared_scheduler_model_slot_queues_and_cancellation_releases`、`subagent_and_parent_share_one_scheduler_model_slot`、`rate_limit_cools_down_one_provider_without_replay`、`shared_root_enforces_cross_process_model_quota`、`shared_root_isolates_provider_cooldowns`）；`tests/coordinator.rs`（25 项）；`tests/shared_limits.rs`（33 项）。
  - 残余：跨进程预算未持久化；未联网真实 Provider；跨平台未验证；四项门禁均已通过。
- [x] **REQ-CONFLICT / P1，已实现并通过离线验收**：写路径按目录作用域在进程内排队，祖先/子路径冲突，同路径读写与写写冲突；子代理严格 FIFO，整工作区 writer 独占、只读并行，且不让旧 writer 被后续只读任务饿死。
  - 证据：`tests/coordinator.rs` 的 `path_conflicts_and_read_concurrency`、`queued_disjoint_task_progresses_past_conflicting_head`、`writer_is_not_starved_by_later_readers`；`src/subagent.rs` 的 `take_startable` FIFO 逻辑。
  - 残余：跨进程 Workspace 路径租约已接入 `src/workspace.rs`，有取消与崩溃回收测试；仅约束协作进程；无自动冲突合并，TOCTOU 仍存在（AUD-19）。
- [x] **REQ-QUOTA / P1，已实现并通过离线验收**：重任务在本地与跨进程均独立限流（默认 2）。`bash`/`shell`、`workspace` 的 shell/bash 操作，以及 `plugin__…`/`mcp__…` 调用保守计入 heavy。
  - 证据：`src/scheduling.rs` 的 `heavy_tool` 与 `tests/scheduling.rs::heavy_tool_quota_queues_and_cancellation_releases`、`scheduling::tests::heavy_classification_includes_aliases_and_extensions`；`tests/shared_limits.rs` 的 `heavy_slots_are_serial_in_process`、`heavy_slot_is_shared_across_processes_and_released_on_exit`。
  - 残余：未被 dispatch hook 接入的第三方工具、其他直接启动的进程不保证受限；无 OS 内存/CPU 隔离。
- [x] **REQ-BUDGET / P1，已实现并通过离线验收**：`--max-model-calls` 是硬调用次数；`--max-total-tokens` 调用前按保守预留占用，结算用 Provider 上报的真实 usage，未上报按预留计。实际超预留记录 overage 并阻止后续请求。
  - 证据：`tests/coordinator.rs` 的 `token_budget_reserves_before_call_and_refunds_on_settle`、`unknown_usage_is_conservatively_charged`、`overage_blocks_follow_up_requests`、`model_call_budget_is_hard`；`tests/scheduling.rs` 的 `usage_settles_exactly_and_unknown_usage_is_charged`、`token_budget_blocks_after_real_usage_settlement`。
  - 残余：这是 **token 计数而非精确费用硬限额，无美元换算**；预算只在本进程内生效，跨进程未持久化。
- [x] **REQ-DEDUP / P2，已实现并通过离线验收**：`Coordinator` 支持显式 `dedup_key`（相同 key 的活动/排队任务被拒绝并返回已有 `TaskId`）；子代理对相同 session + 完全相同的活动请求复用同一 `run_id`，终态不自动重放。
  - 证据：`tests/coordinator.rs` 的 `duplicate_dedup_key_is_rejected_and_reusable`、`duplicate_dedup_key_while_queued_is_rejected`；`src/subagent.rs` 的 `RequestKey` 比对路径。
  - 残余：显式 `dedup_key` 仅在 Coordinator API 层，**CLI 未暴露对应参数**。
- [x] **REQ-ACCEPT / P2，已实现并通过离线验收**：执行证据 journal、子代理 `ToolEvidence`（`ok` 仅表示工具自报）、结构化 `completion_call`/`diagnostic` 事件与离线评测 harness。
  - 证据：`src/journal.rs`；`src/lib.rs` 的 `CompletionCallObservation`/`RunDiagnostic`；`tests/diagnostics.rs`（8 项，含 `completion_observation_carries_reported_identity_and_usage`、`diagnostic_serialization_has_no_forbidden_fields`）；`scripts/agent_eval.py` + `tests/agent_eval_harness.py`。
  - 残余：harness 自述为探针非 benchmark；最终四门禁已通过，真实任务对比待执行；未验证项标 unknown。

## 分层边界

- [ ] 明确 `ai / agent / coding-agent` 的职责，依赖方向为 `coding-agent → agent → ai`：上层调用下层，底层不得依赖终端、代码工作区或产品会话策略。
- [ ] `ai`：统一模型与 Provider、认证、模型目录、请求/流式响应、取消与网络错误；不编排代码工具。
- [ ] `agent`：通用运行循环、工具调度、生命周期与事件、取消、执行预算；不硬编码代码工具或终端 UI。
- [ ] `coding-agent`：代码工具、项目上下文、技能/扩展、会话与压缩策略；CLI 和 Alacritty 作为展示与交互适配层。
- [ ] 用依赖检查与独立测试验证边界；当前 `mio-core` 的命名及能力归属与上述 `ai` 层统一，避免只改目录不拆职责。

## 安全可控：Gate / Enforce

- [ ] 将工具调用决策集中在 Agent 执行入口：允许、拒绝、等待审批；CLI、终端、RPC、插件、MCP、子代理不得绕过。
- [ ] 审批绑定具体参数和执行上下文；参数变化重新判断，取消或超时不得默认放行。
- [ ] 由执行层强制落实文件范围、命令/网络权限、执行时间、输出大小和调用预算；提示词不作为权限边界。
- [ ] coding-agent 提供面向项目的策略，宿主提供审批交互，agent 保证所有调用经过门禁。
- [ ] 验收：拒绝操作无副作用；嵌套工具与子代理无权限提升；覆盖参数替换、符号链接、取消与审批重放。

## 高可靠：Persist

- [ ] agent 定义稳定的运行状态与检查点协议，coding-agent 提供持久化存储和恢复策略。
- [ ] 原子保存会话、工具调用 ID、执行结果和待审批状态；版本化数据格式并验证损坏检测与迁移。
- [ ] 明确崩溃后已完成、未开始、结果未知操作的恢复规则；有副作用的操作不能盲目重试。
- [ ] ai 层只重试明确可重试的请求；流式中断、重试、用户取消向上层暴露不同结果。
- [ ] 验收：在请求、工具执行、结果落盘各阶段注入中断；重启后可解释地恢复，不重复写入或执行命令。

## 环境隔离：Isolate / Subagents

- [ ] 定义可替换的执行环境接口；按平台接入真实沙箱或容器，并明确不支持时的行为。
- [ ] 区分路径校验、独立工作目录、Git worktree 与操作系统安全沙箱；不得将前几者宣称为完整隔离。
- [ ] 子代理独立上下文、工作区、权限与资源预算；继承权限只能收窄，凭据按最小范围提供。
- [ ] 父任务取消时回收子任务与进程组；为并发写入定义冲突检测和结果合并规则。
- [ ] 验收：越界读写、网络限制、资源超限、残留子进程、跨任务信息泄漏和并发修改冲突。

## 可观测性：Observe

- [ ] 统一结构化事件和关联 ID：session、run、turn、tool call、subagent；贯通模型请求与工具执行。
- [ ] 区分等待模型、生成回复、运行工具、等待审批、重试、完成、取消、失败，提供明确的状态转换。
- [ ] 记录脱敏日志、延迟/用量/重试/错误指标和父子调用链路；不得记录密钥或完整认证令牌。
- [ ] Alacritty/CLI 仅消费事件，渲染标题状态、等待动画和工具反馈；空闲时停止动画，保持取消响应。
- [ ] 建立离线测试与故障注入反馈闭环，覆盖超时、断流、工具失败、恢复和多 Provider 切换。
- [ ] 验收：一次失败可从用户可见错误追踪到对应请求和工具；状态不乱序，脱敏测试通过，空闲无持续刷新。
