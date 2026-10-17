# Mio 需求与 coding agent 调查

调查日期：2026-10-15。Mio 首次审查基线：`60cfb8c82f419e6c3e8320db84c880e3bb19fd46`。本文档据工作区代码与离线测试用例记录定位与验收口径，阶段划分与 [TODO](../TODO.md)、[项目审查](reviews/2026-10-15-audit.md) 对齐。

## 结论与定位建议

**把 Mio 定位为「共享调度与可控多代理协作的 coding agent 引擎」**：由引擎在真实 dispatch 边界统一管理模型请求（限流/429 冷却）、任务并发（冲突排队、无关并行）、重任务配额、预算预留与真实 usage、显式任务 key 去重和结构化验收证据，并可由 Rust 宿主直接集成。进程内调度、跨进程协作配额、子代理 `Queued` FIFO、执行证据 journal 与跨进程 `Workspace` 路径租约均有实现与离线测试用例；四项 Cargo 质量门禁、站点校验与离线评测器自测均已通过。**不宣称** OS 内存隔离、自动冲突合并、精确费用硬限额，也不宣称真实线上 Provider、真实崩溃或跨平台已验证。

“无头”“可嵌入”“单二进制”“多模型”“原生子代理”都不能单独构成独有优势。Reasonix 新主线已有单 Go 二进制与多入口，Pi 有进程内 SDK，OpenCode 有服务端接口，Codex 本身也有 Rust 实现和 App Server。Rust 对现有 Rust 宿主的集成价值是合理优势判断，低延迟、低内存和任务效果则必须测量。[Reasonix 主线][S1]、[Pi SDK][S7]、[OpenCode Server][S9]、[Codex App Server][S12]。

路线图按四阶段推进，顺序依据已复现缺陷与宿主引擎定位：阶段一已完成核心防线、共享调度与原生路由离线验收；阶段二补恢复闭环、连接韧性、事件版本化与上下文保真；阶段三做同条件评测、大仓库检索、真实沙箱与 SDK 拆分；阶段四再按需求证据演进宿主生态与产品形态。暂不因为竞品具备某个功能就复制整个产品形态。

## 定位收敛：共享调度与可控多代理协作

产品定位收敛为：**引擎统一做共享调度，代理协作可控、证据可查**。围绕用户明确的痛点（模型限流/429、并行改动冲突、编译/浏览器重任务资源争抢、重复探索与预算/验收成本），判断标准不是“功能名是否出现在文档里”，而是有没有实现与可复现证据。

| 阶段 | 已确认能力与依据 | 可验证验收指标 | 未实现或未验证 |
| --- | --- | --- | --- |
| 一：核心防线与共享调度 | 进程内调度与跨进程协作配额（`src/scheduling.rs`/`src/coordinator.rs`/`src/shared_limits.rs`，模型/重任务 `flock` 配额与 429 冷却）；子代理 `Queued`/`Running` FIFO 与 `ToolEvidence`；Anthropic/Google/Ollama 原生协议路由（`tests/provider_routes.rs`）；项目 MCP 启动前指纹授权（`src/mcp.rs`）；编辑前内容/身份校验与跨进程 `Workspace` 路径租约（`src/workspace.rs`）；执行证据 journal（`src/journal.rs`）与结构化 `completion_call`/`diagnostic`；插件输出/MCP 帧/RPC 行大小限额；四项质量门禁与站点检查通过 | REQ-SCHEDULE：429 不成风暴、同一 Provider 冷却对父/子代理与 RPC 一致可见；REQ-CONFLICT：冲突任务排队、无关任务并行；REQ-QUOTA：重任务配额与取消回收；REQ-BUDGET：请求前预留、结算真实 usage，缺 usage 标 unknown；REQ-DEDUP：相同 key 只执行一次；REQ-ACCEPT：任务/配置/请求计数/usage/耗时/退出状态可复查 | 本地模拟端点/单元/多进程测试，不等于真实 Provider、真实崩溃或跨平台验收；显式 `dedup_key` 仅在 Coordinator API，CLI 未暴露 |
| 二：恢复、连接与事件 | 会话原子保存与同名/最新会话、编辑冲突检测；journal intent/result/unknown 与 `--no-session` 禁用；底层 observe/telemetry 能力可用 | REQ-RECOVERY：各阶段中断后证据完整、不重复副作用；REQ-POLICY/REQ-PROCESS：全入口统一门禁、拒绝时无未授权副作用；REQ-EVENTS/REQ-CONTEXT：ID 贯通、压缩后保留约束与未完目标 | 崩溃注入与端到端恢复未验收；无持久化 MCP trust store、required/optional 与重连；事件版本化与 ID 全链路贯通未完成；大输出未外置为文件引用 |
| 三：评测、检索与隔离 | 协议单测与离线评测 harness（自述为探针，非 benchmark）；CLI、Host 抽象与终端状态集成可运行 | REQ-EVAL：固定模型/任务集/预算对比正确性、成本、延迟、缓存、取消与恢复；REQ-ISOLATE：backend 状态可查询、越界测试按平台执行；REQ-SDK/REQ-DISTRIBUTION：最小与完整示例可编译、安装与发布闭包可核对 | 无同模型同预算对照；无 repo map/LSP/符号索引选型；无真实 OS 沙箱；`src/bin/mio.rs` 未拆分，helper crate 平台/feature 矩阵与 registry 发布未收口 |
| 四：宿主与产品 | CLI、Host 抽象与终端状态集成已有可运行基础 | REQ-PRODUCT：选定首要宿主与目标任务，各产品形态独立可回退 | 首要宿主未经用户调查确认；ACP/HTTP、会话树、Plan 模式、后台作业未排期 |

自动冲突合并、OS 内存/资源隔离与沙箱、精确费用硬限额（当前为 token 计数）、跨进程 token 预算持久化、真实 Provider 线上路径与认证、跨平台行为、真实崩溃恢复、同模型同预算任务对比均无实现或无证据，禁止在 README/TODO 中提前宣称。

## 调查口径

| 项目 | 口径 |
| --- | --- |
| 主样本 | Reasonix、Pi、OpenCode、Aider，以及 Codex 的开放 CLI/App Server 集成能力；这是代表性样本，不是按装机量验证的市场排名。 |
| 补充参照 | Claude Code / Claude Agent SDK 用于观察产品能力与宿主接口；不把 Claude Code 产品列为开源 coding agent。 |
| Reasonix 身份 | GitHub 搜索定位到 `esengine/DeepSeek-Reasonix`。其旧 `main` README 明确进入维护模式，当前比较采用 `main-v2` Go 主线。另有同名/衍生仓库，不与其混合。 |
| 证据等级 | Mio 是本地代码与离线复现；竞品是第一方 README、设计文档及官方说明，属于“文档声明”，没有全部安装或逐项运行。 |
| 时效 | 读取日期如上；外部链接多为可变分支/在线文档，未来复评需固定 commit、release 与配置。本轮不写未经核实的“最新版本号”。 |
| 效果比较 | 未做同模型 benchmark，不比较宣传分数、缓存命中率、价格优势、成功率或速度排名。 |
| 安全用语 | 工具规则、工作区检查、Git worktree、文件快照和 OS 沙箱分别记录；一个功能存在不代表所有平台/扩展均受约束。 |

Reasonix 特别注意：不能继续把它概括为“仅 DeepSeek、必须 Node 运行、没有多模型”。新主线说明多模型配置和 Go 二进制；迁移文档还明确旧语义索引尚未迁移，不能把两代功能相加后视为当前能力。[主线说明][S1]、[迁移说明][S2]。

## 竞品调查表

| 对象 / 本轮资料范围 | 文档可确认的长处 | Mio 相比可发挥的价值 | Mio 需要补齐 / 判断限制 |
| --- | --- | --- | --- |
| **Reasonix**，`esengine/DeepSeek-Reasonix/main-v2` | Go 单二进制，终端/桌面/浏览器/ACP 入口；配置式多模型、缓存感知上下文维护；文档提供权限、沙箱与检查点设计。[S1][S2][S3][S4] | 对已使用 Rust/Tokio 的 Ravomi 宿主，进程内接入 `mio-core → mio-agent → mio-coding-agent`，替换 Host 和自有工具较直接；这是架构适配判断。 | Mio 缺编辑检查点、产品用量/缓存事件和真实隔离验收；Reasonix Go SDK 文档是 **extension sidecar SDK**，不能直接等同于其核心进程内 SDK。其权限/沙箱也有平台限制。[S4][S5] |
| **Pi**，当前项目 `earendil-works/pi` | 极简可扩展 harness，skills/扩展/模板/包；TypeScript 进程内 SDK、会话树、RPC 与 settled 事件。README 明确子代理/plan 可通过扩展实现，默认不内建。[S6][S7] | Mio 的原生 Rust 子代理和重叠写调度适合 Rust 宿主；不要求宿主采用 TS/Node。 | 可嵌入和 headless 并非 Mio 独有；Mio 会话树、扩展生命周期和稳定集成文档尚需增强。Pi 无默认子代理不代表无法使用子代理。 |
| **OpenCode**，`anomalyco/opencode/dev` 与官方文档 | 开源 agent，build/plan 与子代理；独立 HTTP 服务提供 OpenAPI 与事件流；权限支持 allow/ask/deny 和外部目录规则。[S8][S9][S10] | Mio 可作为库集成至 Rust 进程，不强制宿主围绕 HTTP 服务建模；CLI 可作为单独适配层。 | Mio 尚无同等级服务协议/客户端生成契约；先修过滤与扩展信任，再讨论 ACP/HTTP。权限规则存在不能直接证明 OS 沙箱覆盖。 |
| **Aider**，`Aider-AI/aider/main` | 明确的 repo map 与图排序/预算机制，Git 工作流和自动 lint/test 反馈；代码理解、编辑与验证形成清楚产品闭环。[S11][S11a] | Mio 更侧重通用 Agent 引擎和宿主控制，并统一使用 jj 管理 change、workspace 与远程同步。 | Mio 缺经过评测的 repo map/符号检索及自动验证闭环；不要仅增加搜索工具名就宣称大仓库能力相当。Aider 的可扩展能力未在本轮完整核验。 |
| **Codex CLI / App Server / SDK**，官方文档 | App Server 提供会话、审批和流式事件集成；SDK 可启动、继续和恢复线程；官方单独描述审批与平台沙箱。[S12][S13][S14] | Mio 可控制自有运行循环、Provider 适配和 Rust 公共库边界，适合需要自主维护引擎的宿主。 | Rust、无头、SDK、审批和子代理等不应被描述为独有；Mio 缺统一执行门禁和版本化事件。开放组件不等于模型、账号、托管服务或全部产品形态都开放。 |
| **Claude Code / Claude Agent SDK**，补充参照 | 官方 SDK 暴露内置工具、hooks、MCP、权限、子代理与会话，并支持 Python/TS 宿主。[S15] | Mio 自有 MIT 项目代码和可维护的 Rust 引擎适合自主集成；具体第三方代码仍保留各自许可证。 | 比较集成设计，不归入开源主样本；账号授权、服务可用性及第三方产品集成受具体条款约束，不由“有 SDK”推导可任意复用账号。 |

上表都是能力与结构比较，没有运行证据支持“谁整体更强”。特别是模型本身的编程水平、harness 的工具策略与产品 UI 体验，应分别评价。

## Reasonix 值得借鉴的具体机制

| 机制 | 已读取的一手资料 | 转化为 Mio 需求 | 不应照搬的前提 |
| --- | --- | --- | --- |
| 可审查编辑与 rewind | 检查点文档按用户回合保存文件快照，覆盖可预览的写/编辑工具；明确 Bash 副作用不自动覆盖。[S3] | REQ-RECOVERY：记录工具证据，可选保存 pre-edit 快照，恢复时检测用户后续修改。 | 检查点不是任意命令的事务回滚；不碰用户 Git 历史，也不许把恢复等同于重放工具。 |
| 缓存感知维护 | 主线介绍稳定环境摘要、旧工具结果裁剪后再压缩。[S1] | REQ-CONTEXT：稳定工具 schema/指令前缀，外置大输出，记录实际 cache usage。 | 不采用旧版 README 的个案命中率作为 Mio 成本承诺；先测当前 Provider。 |
| 权限与执行隔离分层 | Spec 区分权限策略与沙箱执行；说明 macOS/Linux backend 与 Windows 限制，enforce 无 backend 时拒绝执行。[S4] | REQ-ISOLATE：宿主可插拔 backend，支持状态可查询，无 backend 的行为明确。 | 路径校验/独立 cwd/worktree 不能替代 OS 限制；外部 MCP 的权限也需独立定义。 |
| 可解释恢复 | Recovery 文档要求恢复依据执行证据，不能重新执行历史工具；保存失败时暂停相关操作。[S16] | REQ-RECOVERY：intent/result/unknown、原始 transcript 和恢复决策可查。 | 不能用一段压缩摘要充当审计日志；副作用未知必须保留 unknown。 |
| 扩展协议 | Go SDK 说明 sidecar、intercept、事件与上下文取消的契约；主 README 与 SDK 的协议代际表述存在差异，应以目标 release 核对。[S5][S1] | REQ-EXTENSIONS：版本协商、能力声明、conformance 和取消约束。 | 不直接把扩展 sidecar SDK 宣称为核心 SDK，也不凭 README 认定协议完全稳定。 |

## Mio 优势与不足的证据边界

| 判断 | 类型 | 依据与限制 |
| --- | --- | --- |
| 已有三层 Rust 源码和 Host 抽象，能作为库运行 | 事实 | [lib.rs](../src/lib.rs)、[host 示例](../examples/host.rs)、[分层说明](rig-source.md)；不是仅有 CLI 包装。 |
| 对 Ravomi 的 Rust 宿主有集成适配优势 | 推断 | 同语言、同异步生态及可替换 Host 减少跨进程接口需求；仍需 SDK 兼容与生命周期测试证明维护收益。 |
| 内置原生子代理有调度保护 | 事实 | [subagent.rs](../src/subagent.rs)：并发上限、`Queued` FIFO、只读并行与整工作区 writer 独占；子任务与父任务共享同一 Scheduler；不是操作系统、内存或凭据隔离。 |
| 对终端有具体状态集成 | 事实 | OSC 7501 规范与离线终端测试通过；跨终端支持范围需继续验证。 |
| 默认工作区工具有界、会话有原子写入 | 事实，有例外 | 内置读写/search 与 Shell 限额已实现；插件/MCP、取消线程及会话恢复缺口见审查，不可外推到全部工具。 |
| Rust 所以更快、更省内存、更安全 | 未证实 | 没有相同构建、任务、模型和平台的测量；Codex 也有 Rust 实现，Reasonix 也有原生二进制。 |
| Mio 当前更擅长复杂软件工程任务 | 未证实 | 现有测试覆盖大量框架/协议行为，不是任务成功率 benchmark。 |
| 先做执行控制、Provider 契约和恢复最有价值 | 建议 | 对应本次已复现缺陷与宿主引擎定位；优先级应在真实用户调查后复评。 |

## 需求表

规模为粗估相对工作量：小/中/大，不代表工期承诺。所有 owner 默认待指定，状态为待实现或待验收，未获需求证据的增强不得直接视为承诺。

| 需求标识 | 场景与目标 | 当前基础 / 关联问题 | 优先级 / 规模 / 依赖 | 可验收结果 |
| --- | --- | --- | --- | --- |
| REQ-POLICY | 宿主限制工具，扩展和子代理不得绕过 | Workspace 审批/ToolFilter 与 AUD-02/03/07 已落地；extra_tools 的 host-controlled 边界待明确 | P1 / 大 / 无 | 全入口经过统一决策；deny 优先；审批绑定规范化参数、cwd、工具身份；项目配置未信任不得启动；嵌套权限只收窄。 |
| REQ-PROCESS | 取消、超时后不残留计算和新副作用 | Shell/插件进程组、输出限额与 AUD-01/05/06/12 已落地；真实故障注入待验收 | P1 / 中 / POLICY | CLI/RPC/EOF、超时、父子任务、后代进程均有回收测试；输出与 CPU 有界；已发生修改与取消后的执行严格区分。 |
| REQ-SCHEDULE | 429 不成风暴，同一 Provider 共享冷却 | `Coordinator` 进程内准入 + `SharedLimits` 跨进程 `flock` 配额；429 `Retry-After` 冷却；`src/scheduling.rs` 真实 dispatch hook | P1 / 中 / PROCESS | 已实现并通过离线验收：模拟 429 / 并发槽位 / 跨进程配额用例已存在；未联网真实 Provider、跨平台未验证。 |
| REQ-CONFLICT | 冲突任务排队、无关任务并行 | 进程内路径作用域租约；子代理 FIFO 整工作区 writer 独占、只读并行；跨进程 workspace 租约已接入工作区执行路径 | P1 / 中 / SCHEDULE | 已实现并通过离线验收：进程内冲突/饥饿用例已存在；跨进程租约已接入工作区执行路径，无自动冲突合并。 |
| REQ-QUOTA | 重任务有独立并发/时间配额 | heavy 本地 + 跨进程默认 2；shell/plugin/mcp 保守计入 heavy | P1 / 中 / SCHEDULE | 已实现并通过离线验收：本地/跨进程 heavy 用例已存在；未接入 dispatch 的工具与直接启动进程不保证受限，无 OS 隔离。 |
| REQ-BUDGET | 请求前预留、请求后记录真实 usage | `--max-model-calls` 硬次数；`--max-total-tokens` 预留+真实 usage 结算，未知按预留、超预留阻止后续 | P1 / 中 / PROVIDER | 已实现并通过离线验收：预留/未知/overage 用例已存在；为 token 计数非费用硬限，跨进程预算未持久化。 |
| REQ-DEDUP | 显式任务 key 去重 | `Coordinator::dedup_key`；子代理相同 session + 相同活动请求复用 `run_id` | P2 / 小 / SCHEDULE | 已实现并通过离线验收：dedup 与 run_id 复用用例已存在；CLI 未暴露 dedup 参数。 |
| REQ-ACCEPT | 结构化验收证据可复查 | journal + `ToolEvidence` + `completion_call`/`diagnostic` 事件 + 离线评测 harness | P2 / 中 / EVENTS、EVAL | 已实现并通过离线验收：证据/事件脱敏用例已存在；harness 为探针非 benchmark，最终门禁已通过，真实任务对比待执行。 |
| REQ-PROVIDER | 用户选择官方供应商即可使用正确协议 | 原生路由 AUD-04 已落地（离线契约）；AUD-18 能力来源待统一 | P1 / 中 / 无 | 原生/兼容路由显式；路径、认证、工具流、thinking、usage、限额、断流和错误分类的离线契约通过；线上测试另行标记。 |
| REQ-RECOVERY | 长任务失败后知道做过什么，安全继续 | 会话原子快照与 AUD-10/11/13/19 已落地；崩溃注入与端到端恢复未验收 | P2 / 大 / PROCESS、POLICY | intent/result/unknown 持久化；崩溃注入不盲目重放；同名/最新会话准确；可选文件快照不覆盖用户后改内容。 |
| REQ-EVENTS | Ravomi/终端/其他宿主能显示并追踪运行 | Host/OSC/RPC、`completion_call`/`diagnostic` 与底层 observe 已有；AUD-15 贯通待完成 | P2 / 中 / POLICY | 稳定 session/run/turn/tool-call/subagent ID，事件序列、usage、审批/取消/错误状态；脱敏和慢消费者压力验收；协议版本/能力可查询。 |
| REQ-CONTEXT | 长任务保留约束且控制成本，大仓库能找到相关代码 | 指令/skill/模板/安全工具边界与摘要已有；AUD-06/14/18 | P2 / 大 / EVENTS、EVAL | 原始 transcript 可查；约束和未完成项评测；稳定前缀及实际 token/cache 指标；对 repo map/LSP/索引先做效果试验再选型。 |
| REQ-EXTENSIONS | 插件/MCP 可预测接入并被监督 | 动态工具桥与 AUD-02/03/05/07/08/09 已落地；AUD-16 生命周期/重连待补 | P2 / 中 / POLICY、PROCESS | 分页、冲突拒绝、required/optional、复用重连、协商/取消/通知和负向 conformance；错误可定位到服务与调用。 |
| REQ-ISOLATE | 宿主选择真实受限执行与并行修改环境 | 路径检查/重叠写限制与跨进程 workspace 租约模块已有；无 OS sandbox | P2 / 大 / POLICY、PROCESS | backend 状态可查询；无 backend 的失败行为明确；越界文件/网络/资源测试按平台执行；worktree 的冲突合并与安全隔离分别验收。跨进程租约已有取消、进程退出回收用例。 |
| REQ-SDK | 第三方 Rust 宿主稳定使用三个层次 | run/Host/hook/bus 已有；AUD-15/20 | P2 / 中 / EVENTS | 最小与完整宿主示例可编译；生命周期/取消/错误契约清晰；公共 API 兼容检查；先接底层能力，不复制另一套循环。 |
| REQ-EVAL | 用证据判断改进、选模型和比较竞品 | 协议单测较多，缺真实任务对照 | P2 / 中 / PROVIDER、EVENTS | 固定任务集、评估器、模型与预算；输出正确性、成本、时间、缓存、取消清理、恢复和修改冲突数据；保留失败案例和原始配置。 |
| REQ-DISTRIBUTION | 不同平台能可靠安装与升级 | npm/安装 fixtures 已有；AUD-17/20 | P2 / 中 / 无 | 平台与 feature 矩阵；helper crate 自身检查；下载/解压上限、校验失败不污染已有安装；registry 发布依赖闭包和许可证来源记录可核对。 |
| REQ-PRODUCT | 聚焦首要用户，避免引擎承载全部 UI | 已有终端和宿主示例；定位尚需确认 | P3 / 中 / 用户调查、EVAL | 选定首要宿主和目标任务；ACP/HTTP/桌面、plan、后台作业各自有用户证据和边界，形成独立可回退迭代。 |

## 用户需求调查表

本表是待访谈/测量的问题清单，**不是已经收集到的用户结论**。可由维护者、Ravomi 宿主开发者、CLI 使用者和第三方集成者分别填写。暂不阻塞近期 bug 修复。

| 调查项 | 待收集问题 | 当前合理假设 | 决策影响 / 关联需求 |
| --- | --- | --- | --- |
| 首要用户 | 最常用入口是 Ravomi/Alacritty、独立 CLI、IDE 还是第三方 SDK？谁负责部署和授权？ | 先服务现有 Rust 宿主，但尚未访谈确认 | 决定 SDK、CLI、ACP/HTTP 和 GUI 顺序；PRODUCT/SDK |
| 任务构成 | 修 bug、补测试、跨文件重构、陌生仓库阅读，各占多少？任务规模和持续时间？ | 开发维护和阅读为主 | 确定评测集和上下文策略；EVAL/CONTEXT |
| 连接环境 | 必须原生支持哪些 Provider？官方 API、兼容网关、本地模型或账号服务？离线要求是什么？ | 兼容端点和原生 Provider 要明确分开 | 决定协议适配优先级；PROVIDER |
| 权限信任 | 项目是否来自第三方？默认允许哪些文件/命令/网络？谁授权 MCP 启动？ | 项目配置不可天然等同可信本机配置 | 决定 trust store、审批和沙箱预设；POLICY/ISOLATE |
| 并发协作 | 是否同时有用户编辑、父代理、多个写子代理或其他 Codex 会话？ | 需要保留已有改动，默认保守串行写 | 决定租约、哈希前置条件和 worktree；PROCESS/RECOVERY |
| 中断恢复 | 最常见中断是 Ctrl-C、模型失败、断电、磁盘满还是进程崩溃？允许丢多少进度？ | 工具副作用证据不能丢，不能未知重放 | 决定日志和检查点粒度；RECOVERY |
| 规模与成本 | 仓库文件量、会话数、上下文长度、单任务预算和最大等待时间？ | 先收集基线，不预设“零成本”或命中率目标 | 决定索引、限额、缓存策略；CONTEXT/EVAL |
| 状态反馈 | 宿主需要 reasoning、文本、工具、审批、usage、成本、错误链中的哪些？可保存哪些内容？ | 默认脱敏，必须能区分等待/运行/取消 | 决定事件 schema 与存储；EVENTS |
| 安装平台 | Linux/macOS/Windows、架构、容器、代理、无管理员权限分别占比？ | 所有声称支持的平台需要验收 | 决定测试矩阵和分发；DISTRIBUTION |
| 扩展方式 | 需要 MCP tools 即可，还是 resources/prompts、Provider 扩展、hooks、长期后台工具？ | 先把现有工具生命周期做对 | 决定扩展协议范围；EXTENSIONS |
| 验收标准 | 哪些任务能客观判定成功？谁审核隐藏测试、修改范围和安全副作用？ | 自动测试与人工 diff 审核结合 | 决定通过门槛和回归策略；EVAL |
| 采用门槛 | 为什么选择 Mio 而非直接集成现有 agent？希望保留什么能力，愿意维护什么？ | Rust 嵌入和运行控制是候选价值，不是已证实市场需求 | 判断继续自研的收益；PRODUCT/SDK |

## 对比评测调查表

以下为未来实验设计，目标值待基线测量后确定，不预填“领先”。优先使用无真实凭据的模拟环境测试控制，再进行明确授权的真实模型任务。

| 维度 | 控制变量与数据 | 验收/比较方式 | 本轮状态 |
| --- | --- | --- | --- |
| 任务正确性 | 同一仓库 commit、提示、模型版本、thinking、工具/轮次/token 预算；隔离任务目录 | 隐藏测试、diff 范围和人工复核；记录成功率及样本量，多次运行和失败分类 | 未运行竞品 benchmark |
| 时间与成本 | 请求/首 token/工具/端到端延迟，输入/输出/cache token，实际 Provider 价格及日期 | 同平台 release 构建；冷启动/热缓存分开；缺 usage 标 unknown，不用字节估算冒充账单 | Mio usage 事件需补齐 |
| 缓存稳定性 | 相同初始历史、工具 schema、前缀、Provider、冷/热启动 | 分开报告 cache read/write 和任务成功率，不能只追求命中率 | 待实验 |
| 取消 | 插件、Shell、MCP、blocking 写操作和子代理分别注入取消/EOF/超时 | 取消确认延迟、残留进程、确认后新副作用、写租约释放次序 | 插件已复现缺陷，Shell 现有测试通过 |
| 恢复 | 请求前、执行中、结果返回后、保存前分别中断 | 执行证据完整性、unknown 比例、重复副作用、用户能否继续 | 当前只有回合成功快照保障 |
| 权限 | deny/ask/allow、别名、通配符、参数变化、项目 MCP 和嵌套调用 | 拒绝时无未授权新副作用；平台沙箱另测试路径/网络越界 | 插件过滤和 MCP 信任缺口已复现 |
| 限流与调度 | 模拟 Provider 的 429/`Retry-After`，并发父/子代理与 RPC 任务；请求计数与时间线 | 请求不放大、冷却共享、冲突排队/无关并行；跨进程 workspace 租约已有离线验证，线上表现仍需比较 | 已实现并通过离线验收（进程内+跨进程模块+用例）；真实 Provider 与跨平台未验证 |
| 资源 | 长行无匹配、大输出、超长协议帧、慢消费者、并发工具和大型历史 | RSS/CPU/吞吐/退出行为有上限且可解释；不在用户真实环境制造 OOM | 仅正则小规模复杂度探针已运行 |
| 上下文保真 | 早期用户约束、决策、未完目标、失败工具证据；长程任务 | 压缩后约束保持率和任务正确性；原始记录可查 | 现有摘要为规则截取，待专项 |
| SDK/协议 | 宿主最小示例、重连/取消、事件顺序、错误传播、API 变更 | 兼容性与 conformance；不把“能启动”视为完整集成 | Mio 当前基础可用，待版本化 |
| 分发与平台 | 相同 OS/架构、权限、shell、代理、断网/坏包配置 | 安装/升级/失败恢复、hash 校验、平台实际运行；记录例外 | Linux 本轮离线验证通过，其余未跑 |

## 一手资料与复核记录

本轮实际读取了下列正文。GitHub 搜索用于定位 Reasonix，表中能力依据项目自己的资料；官方文档通过网络直接抓取核对。外部功能属于文档声明，没有以宣传数字充当独立结果。

| 来源 | 本轮核对内容 |
| --- | --- |
| [S1：Reasonix main-v2 README][S1] | Go/多入口、多模型、缓存维护及当前文档索引；与旧 main 的维护提示交叉核对 |
| [S2：Reasonix migration][S2] | 两代分支、运行时及语义索引未迁移等差异 |
| [S3：Reasonix checkpoints][S3] | 文件快照覆盖范围和 Bash 不覆盖限制 |
| [S4：Reasonix Spec][S4] | 权限/沙箱分层、backend 与平台限制；属于项目规格说明 |
| [S5：Reasonix Go SDK][S5] | extension sidecar SDK，协议版本和取消契约；不认定为核心 SDK |
| [S6：Pi README][S6]、[S7：Pi SDK][S7] | 当前仓库、极简扩展定位、SDK/会话树/事件及生命周期 |
| [S8：OpenCode README][S8]、[S9：Server][S9]、[S10：Permissions][S10] | 运行模式、服务协议和权限规则 |
| [S11：Aider README][S11]、[S11a：Repo map][S11a] | Git/lint/test 集成及图排序、预算机制 |
| [S12：Codex App Server][S12]、[S13：SDK][S13]、[S14：审批与安全][S14] | 开放 App Server、宿主接口、会话和审批/沙箱；`/codex/security` 当前是 Security 产品页，不把它误当通用沙箱说明 |
| [S15：Claude Agent SDK][S15] | 官方 SDK 能力边界；只作产品参照 |
| [S16：Reasonix Recovery][S16] | 保存失败、执行证据与不重放工具的恢复规则 |

补充核对：读取 Reasonix 和 OpenCode 的 MIT LICENSE、Aider 的 Apache-2.0 LICENSE；Pi README 标 MIT。这里只描述查到的项目声明，不给出跨项目代码复制或重新许可的法律结论。Mio 的 Rig/协议参考来源继续依照 [CREDITS](../CREDITS.md) 与 [licenses](../licenses/) 维护。

[S1]: https://github.com/esengine/DeepSeek-Reasonix/blob/main-v2/README.md
[S2]: https://github.com/esengine/DeepSeek-Reasonix/blob/main-v2/docs/MIGRATING.md
[S3]: https://github.com/esengine/DeepSeek-Reasonix/blob/main-v2/docs/CHECKPOINTS.md
[S4]: https://github.com/esengine/DeepSeek-Reasonix/blob/main-v2/docs/SPEC.md
[S5]: https://github.com/esengine/DeepSeek-Reasonix/blob/main-v2/sdk/go/README.md
[S6]: https://github.com/earendil-works/pi/blob/main/packages/coding-agent/README.md
[S7]: https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/sdk.md
[S8]: https://github.com/anomalyco/opencode/blob/dev/README.md
[S9]: https://opencode.ai/docs/server/
[S10]: https://opencode.ai/docs/permissions/
[S11]: https://github.com/Aider-AI/aider/blob/main/README.md
[S11a]: https://aider.chat/docs/repomap.html
[S12]: https://developers.openai.com/codex/app-server
[S13]: https://developers.openai.com/codex/sdk
[S14]: https://developers.openai.com/codex/agent-approvals-security
[S15]: https://platform.claude.com/docs/en/agent-sdk/overview
[S16]: https://github.com/esengine/DeepSeek-Reasonix/blob/main-v2/docs/RECOVERY.md
