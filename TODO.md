# Mio TODO

本文件只列未完成工作，按依赖顺序推进。已完成的实现、旧失败和固定版本验收记录保留在 [PRD](PRD.md)、[v1 设计规格](docs/design-v1.md)、[v1 验收记录](docs/reviews/2026-10-17-v1-inline8k-acceptance.md)及 `docs/reviews/`，不在此重复记流水账。

## 优先：`mio` 交互 TUI 与多 Agent 房间

命令行工具统一为 `mio`（不另设包装命令）。设计见 [TUI 交互重构](docs/tui-interaction-redesign.md)。`-p`、管道、`--mode json|rpc` 与非 TTY 输出保持不变；不使用 alt-screen、不清屏、不擦除 scrollback；仅 Catppuccin Mocha 深色终端。参考 codex（Apache-2.0）只按思路重写，若移植片段须登记 `licenses/` 与 `CREDITS.md` 并保留 NOTICE。

- [ ] **调色板模块接线**：`src/bin/mio_cli/tui/palette.rs` 已写（OKLCH 插值 32 色、双色渐变、`@A` 提及解析、shimmer、spinner、xterm-256 回退及单元测试），尚未在 `src/bin/mio.rs` 用 `#[path]` 接入、未编译验证；接入后跑四项门禁并人工看一遍 32 色在真实终端的区分度。
- [ ] **P0 内联视口**：引入 `ratatui`、`crossterm`、`unicode-width`、`textwrap`、`pulldown-cmark`（本地缓存已有）；新建 `tui/terminal.rs`、`history.rs`、`theme.rs`，实现“定稿内容经滚动区写入 scrollback + 底部固定输入框/状态/footer + 按目标 Agent 着色的输入框（`▌` 色条 + 0.12–0.14 混合底色，OSC 11 查询终端底色，回退 Mocha Base）”。
- [ ] **P1 编辑器**：替换 `InteractiveEditor`，支持多行、括号粘贴、完整 Unicode 宽度、持久历史、`@` 提及高亮与 `Alt+←/→` 切换焦点 Agent。
- [ ] **P2 消息单元与流式 Markdown**：两阶段流式（视口内实时、块结束后提交到 scrollback），工具调用/审批/推理分别成单元；`CliHost` 不再直接 `print!`。
- [ ] **P3 覆盖层**：审批、会话列表、模型选择等改为视口内覆盖层，替代 `[y/N]` 提示。
- [ ] **P4 无头房间引擎**：新增 `src/room.rs`，定义稳定的 `RoomMember {agent_id, label, role, color_slot, workspace_ref, change_ref}`（`label` 为 `@A`…`@AF`，寻址不依赖可变角色名；编码 Agent 的 `workspace_ref` / `change_ref` 绑定内部 jj workspace/change）与 `RoomMessage {id, room_id, seq, from, to, kind, topic, revision, body}`；提供 `room_post` 工具，`claim` 映射协调器写租约，模型调用仍经 `acquire_model`，每主题默认 6 轮上限后交用户裁决，持久化 `.mio/rooms/<room-id>.jsonl`，RPC 增加 `room_message`/`room_state` 事件与 `room_post` 命令；用 mock provider 测试两 Agent 协商。
- [ ] **P5 房间 TUI**：多 Agent 并发显示、每个 Agent 独立动态状态行，以截图确定的 `@A`…`@AF` 固定字母和 32 色区分身份，并在旁边显示独立角色文字。
- [ ] **TUI 测试**：PTY 回放测试与 ratatui `TestBackend` 快照测试，覆盖窄终端、CJK、resize 与非 TTY 回退。
- [ ] **自动压缩后续**：CLI 已默认开启会话自动压缩（阈值为输入预算 3/4 与 512 KiB 取小，`--no-auto-compact` 关闭），并有回归测试。待做：评估用模型生成摘要替代本地抽取摘要（参考 pi 的结构化摘要与迭代摘要）、上下文溢出错误时压缩后重试一次、TUI 中显示压缩事件与剩余预算；RPC 宿主是否默认开启需单独决定。

## 路线 1：完成真实环境联调与基线测量

这些结果是后续检索、编辑和审查对照的基线；不把本地 mock 或隔离窗口结果当作真实端点与日常桌面验收。

- [ ] **Mio Chat 图片输入**：用真实视觉端点验证图片请求、能力提示、超限/拒绝和会话恢复；在日常 Wayland 窗口复验粘贴、发送、附件清除与失败时保留。确认 DeepSeek 实际图片能力；Ollama 上下文窗口未知时按真实部署容量配置并验证预算行为。
- [ ] **Mio Chat 推理展示**：在日常 Wayland 窗口用真实支持推理的端点验证增量推理与正文分离、取消和背压后的显示；真实推理不可用时明确展示能力缺失，不把普通进展标成思考。
- [ ] **技能发现 GUI 回归**：在 Mio Chat 端到端验证大 `SKILL.md` 不阻断启动、按需选择仍受正文上限约束，以及失败提示能到达用户。
- [ ] **Provider 与调度压力**：对非 DeepSeek Provider 分别验证真实认证、thinking、流式错误、429/`Retry-After` 和持续并发；记录请求、实际 usage、重试与共享冷却，缺失 usage 保持 unknown。
- [ ] **固定对照基线**：冻结源码、二进制、模型、采样、任务、调用和 token 预算；对现有 `grep`/`read` 记录任务正确率、重复读取、调用数、输入/输出 token、延迟与失败类别。后续方案沿用相同条件，不以不同快照的结果拼接收益。

## 路线 2：把 codescope 代码图原生接入 Mio

目标是 Mio 自维护的 Rust 检索能力，CLI、RPC 与 SDK 使用同一实现；外部 codescope CLI/MCP 可用于对照，但不是最终运行时依赖。[codescope](https://github.com/adventurewave-labs/codescope) 是候选源码与设计参考。

- [ ] **确定复用边界**：审查 codescope 的源码、依赖、许可和更新方式；确定可复用组件、Mio 内部模块归属及版本化索引格式。保留所复用源码的版权与许可记录，不将索引逻辑下沉到与工作区无关的 `mio-core` / `mio-agent`。
- [ ] **建立增量图缓存**：索引目录、文件、符号以及导入、调用、引用关系；用稳定身份和内容哈希更新变更文件。处理新增、删除、重命名、分支与工作树切换；定义缓存位置、大小上限、损坏重建、并发访问和清理规则。
- [ ] **接入只读查询**：提供有界 repo map、定义/引用、调用关系和改动影响查询；结果带源码位置、关系来源、索引版本及新鲜度。经现有工具准入和工作区路径边界供 CLI/RPC/SDK 使用；索引不可用、过期或语言不支持时明确降级到 `grep`/`read`，不放宽权限、输出或上下文预算。
- [ ] **验证正确性与收益**：覆盖跨文件同名符号、未解析引用、陈旧结果、文件竞态、缓存损坏及多工作树；在路线 1 的固定任务与预算下比较正确率、重复读取、调用数、token、延迟、建索引耗时和磁盘占用。完成原生接入后按对照证据决定默认启用范围；保留关闭与回退路径。
- [ ] **LSP 精度试验**：针对代码图仍不能可靠解决的定义、引用、诊断和跨文件重命名，比较候选 LSP 与代码图、现有 `grep`/`read`。只有可复查的正确性收益且服务故障可降级时接入可选 LSP；所有写入继续走 Mio 授权与租约。

## 路线 3：提高编辑、审查和协作任务的成功率

- [ ] **编辑交互对照**：用同模型同任务比较现有编辑流程与已提供的哈希锚点、`edit.preview` 组合，记录编辑成功率、冲突拒绝、重试、token 和耗时；验证预览到正式写入之间的外部修改不会被覆盖。不要将乐观哈希预检宣称为原子 CAS。
- [ ] **按模型调整提示与编辑格式**：固定模型版本、采样、任务和预算，比较候选格式的通过率、编辑重试、输出 token 与耗时；只有独立复验显示收益且可回退时才启用相应配置。
- [ ] **结构化代码审查**：针对现有真实试验中的非新缺陷误报和空白行定位，改进证据核对与位置准确性；实现可选内建审查入口，并用固定 diff、独立判分做跨模型及真实项目对照。缺陷、严重度和结论须能回溯到源码与检查证据，模型自述不算验收。
- [ ] **版本管理后端边界**：定义不泄漏具体 VCS 命令的 `WorkspaceBackend`，覆盖创建/恢复/快照/diff/整合/丢弃与 jj bookmark/远程交付；多 Agent 编码模式统一使用 jj，任务绑定稳定 change ID、Agent 绑定独立 jj workspace。审查 `jj-lib`、`gix` 的 API 稳定性、依赖、许可与升级成本；第一版可在严格版本探测和有界输出下调用受管 jj CLI，安装与版本能力必须显式诊断。
- [ ] **jj 隔离与协同整合**：在 Mio 自管的独立仓库/对象存储中创建和回收 jj workspace，支持依赖 change、并行 change、冲突状态、operation log 恢复和专门整合 Agent；取消后保留真实 worker 状态与已发生副作用证据。路径 claim、权限与资源预算继续由 Mio 强制，jj workspace 不冒充操作系统沙箱。
- [ ] **jj 远程交付与迁移**：通过 jj bookmark 与 `jj git fetch/push` 兼容现有 Git 远程，用户和 Agent 不直接调用 Git；提供从既有 Git 工作副本一次性迁移到 colocated jj 的诊断与保护，验证基线导入、远程同步、冲突、用户 working-copy change 保护和失败恢复。禁止重新引入 GitButler 状态或依赖。
- [ ] **资源读取边界**：验收非 Unix 平台的固定路径打开与外部符号链接信任策略；评估 URL/PR 等外部资源的统一读取，明确来源、授权、大小、不可信内容预算和失败反馈。

## 路线 4：可靠性、跨平台与分发

- [ ] **恢复与冲突压力**：增加非协作写入、硬件断电和跨平台故障注入；验证检查点、未知结果与覆盖不完整状态的恢复。补充真实 `wait(2)` 结果丢失路径可行性评估；未知副作用不得自动重放，Shell 副作用不承诺回滚。
- [ ] **平台隔离**：在 macOS 和 Windows 上确定可用的沙箱 backend、能力查询和拒绝行为，实测路径、网络及资源边界；不将路径租约或单进程资源上限表述为完整沙箱或进程树配额。
- [ ] **平台与宿主验收**：取得 macOS/Windows 原生构建、测试、SDK feature 和故障恢复结果；完成真实 Ravomi GUI 宿主端到端验收，区分协议解析器回环与完整宿主运行。
- [ ] **发布准备**：跟踪公开 API 兼容性，核对许可证清单、安装/升级路径与 registry 包内容；实际发布仍按项目版本管理和授权流程执行。
- [ ] **公共评测**：接入可复现的公共 coding benchmark 和预算对齐的跨 Agent 对照；分别报告正确性、成本、延迟、缓存与失败，不用自有小样本宣称公共排名。

## 路线 5：有需求证据再启动的扩展

- [ ] **条件规则**：用离线故障注入评估触发准确率、重试上限、额外调用与压缩后规则保留；不得放宽宿主/项目授权或自动重放副作用。
- [ ] **产品与协议候选**：模型角色/顾问、持久项目记忆、DAP、ACP/HTTP、会话树、Plan 模式和长期后台作业分别收集真实场景与维护成本；逐项设计，不预先扩入当前交付范围。

每项实现后先运行受影响的定向检查；提交前在稳定源码上通过 `cargo fmt --check`、`cargo check --all-targets --locked`、`cargo test --locked` 与 `cargo clippy --all-targets --locked -- -D warnings`。实际模型、平台和宿主结果分别记录，不能用本地模拟结果替代。
