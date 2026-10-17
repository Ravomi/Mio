# Mio v1 完成度审计（持续更新）

> 历史审查记录。下方“当前事实/未完成”表反映审查时的阻塞，不能代替最新源码状态。2026-10-16 统一候选已通过四项 Rust 门禁（3109 passed / 0 failed / 6 既有 ignored）、真实 CLI 原生未知副作用确认闭环、12 项 SIGKILL 和 Linux SDK 27 组编译；最后收尾及最终主树验收仍在进行。当前工作项以 [TODO](../../TODO.md) 为准，设计规范以 [v1 设计规格](../design-v1.md) 为准。历史失败与旧快照保留，不用新结果回写旧分数。


范围依据：[v1 设计规格](../design-v1.md) DES-01—DES-22。本文记录当前缺口，不能代替运行结果；存在源码或测试不代表已验收。明确延期的 ACP/HTTP、会话树、Plan、后台作业与其他平台 backend 不据此标为 v1 已完成。

## 当前证据

- 三层依赖静态检查与 3 项回归通过；SDK 静态检查及 30 项自检通过。feature 元数据存在不等于全部平台已编译。
- Pinned 注册表、固定指令成本、SDK/Session/CLI/RPC、压缩结构化审计与 journal/恢复查询已接入；在此前编译窗口中，注册表 14 项、上下文预算 11 项、journal 13 项通过。这些计数绑定各次当时源码，不能覆盖随后新增的全部用例。
- 定向 RPC 曾因测试错误假设系统 content 为字符串而失败；按 OpenAI text parts 修复后，context_fidelity 7 / RPC 32 / RPC 协议 4 项通过，CLI 单元 21 项通过。fmt/check/Clippy 通过；全量 test 在 sandbox_cli 的 /tmp 只读授权 fixture 处停止。现盘 fixture 已换 /var/tmp，仍待复验；随后追加的恢复查询断言和压缩历史清理保护尚未验证。各项来自滚动工作树，不合并宣称一个固定版本全绿。
- 历史模型评测仍为 5/6；新候选尚未以相同模型/预算完成验收。不得把离线摘要缺陷直接认定为长文任务实测失败根因。
- 新一轮实现已返回：真实 run/turn/tool/审批/子代理关联、owner 存活观测、生产输出索引与显式清理。关联实现曾在滚动源码窗口通过全量测试，但随后输出/存活模块仍在变化，不能合并为当前版本全绿。审查另发现准入前误报 `ToolDispatched`，已增加 `tests/dispatch_observation_denial.rs` 并安排修复。三项均以最终整合结果为准。
- 拒绝误报已修复为 Prepared/Dispatched 分离；随后关联/去重/存活/CLI/RPC/工具策略 7 个 target 共 84 项通过，check-all 与 Clippy-all 通过。另有 146 项架构/SDK/评测器自检及站点 6 页面、79 参数校验通过。仍不等于当前源码全量 test 通过：同名工具并发逆序关联随后已由真实 ToolInvocation/task scope 修复并通过（见 `/tmp/mio-identity-runtime-integrated-qa/` 的原始日志）；新增 `session_output_atomicity`（来源登记失败保留旧快照）尚待执行，输出整合仍在进行。
- 随后的 `session_output_atomicity` 1 项与 `MIO_REQUIRE_BWRAP=1` 下 backend/routing/CLI/Host 边界 39 项全部通过，0 ignored。固定二进制 SHA-256 `60bc67318a215e198fee90dcb6c4cdd19a578db1da872ac2daa06aaa9c61673b` 的 checkpoint+SIGKILL 组合 5/5、原四时点与 dedup 崩溃恢复 10/10，通过且无跳过。原始报告位于 `target/evals/design-implementation-20261015/*crash*candidate.json`。这证明 Linux/glibc 上的进程崩溃一致性，不证明断电或其他平台；完整 Rust 门禁另行执行。
- 同一固定候选的真实 DeepSeek 原预算回归：coding 3/3、workflows 2/3，六任务 5/6；长文独立重复 1/3。两个验收门槛均未达到，不能判定 v1 完成。报告为 `target/evals/design-implementation-20261015/live-candidate/{coding-live,workflows-live,wire-1,wire-2,wire-3}.json`。独立失败诊断包含 context_budget 与 scheduling（20 次调用耗尽）；没有完整工具轨迹，不据此把某个输出入口认作已证实根因。版本比较的二进制哈希不同是预期事实，限制在于小样本、非交错运行及历史内容哈希证据不足，不能仅因二进制不同断言不可比较。
- 完整滚动门禁报告：`target/evals/design-implementation-20261015/final-gates-rolling.json`。r1 全量 2800 passed、0 failed、5 ignored，但 Clippy 被随后已修的测试 lint 阻断；r2 fmt/check/Clippy 通过，test 出现 `compaction_observation`、`read_byte_budget`、`output_model_paging` 三个新回归失败。运行期间 14 文件改变、2 文件新增，不能拼接为固定版本全绿。终端 3、取消 2、OSC 7 项对另一固定二进制通过。已建立隔离源码快照验证三项回归；位置记录在本地 `isolated-source-location.txt`，修复须审查后回迁，不能覆盖真实工作区的并行改动。
- 随后隔离快照四项 Rust 门禁通过，只有 compact.rs 相对原 SHA 改变；主目录已有另一份保留摘录记录的压缩修复，未覆盖它。主目录强制真实重编译后，压缩观测 4、byte 预算 2、模型分页 3、真实输出调用身份 2 项通过。journal 绑定/恢复/CLI/RPC 等定向 110 项通过；仍非新结构化引用与未知保护整合后的全量证据。
- DES-09 审查证实反例：MCP Transport 经字符串错误转成 Ok JSON，模型可换 toolCallId 重派发，journal 又可能记为 Failed。现有“client 内部不重发”测试不能证明模型重放被阻止。正在实现类型化 Unknown、同运行树保护和显式宿主确认；跨重启持久化仍须单独实现与验证。
- 2026-10-16 延续验收：结构化引用、真实调用 ID、RPC/journal 反序配对、清理及真实磁盘满共 76 项通过；Unknown 主体与错误结果规范化相关 91 项串行通过。新增边界先复现 0/3，随后修复证据版本、溢出及注册表逐出，8 项边界通过。默认并行的容量夹具干扰已用独立测试子进程修复（原断言不变），连续 10 次默认运行全通过、相关 60 项通过；没有修改全局 cargo 并行配置。仍待后续权威状态机与跨重启保护整合，不据此标为全部完成。

- 2026-10-16 canonical 整合：修复任意状态转移被误接受、Host 回调持锁死锁、扩展参数正文泄露及缺少审批摘要；真实 ToolContext 传递已提交输出引用至终态。18 个定向 suite 及相关 lib tests 通过，fmt/check/Clippy rc=0。证据为 `target/evals/design-implementation-20261015/canonical-integration-qa/`。此记录不代表全量 test 已通过；另发现成功查询失败子任务会被通用 JSON error 适配器误报，正以 typed 结果修复。跨重启 ledger 仍在实现，尚不声称恢复保护闭环。

- 2026-10-16 子代理控制调用已改用真实 `Result` 的 typed adapter，失败子任务的成功 status 查询不再误报工具失败；修复前真实 loopback 回归为红，修复后为绿。主目录四项 Cargo 门禁 rc=0，默认并行 2910 passed / 0 failed / 6 既有 ignored（116 suites），日志 `target/evals/design-implementation-20261015/main-combined-qa/`。随后新改动须独立复验，不能沿用此结果宣称整个目标完成。

- 后续基础验收：`task-foundation-qa/` 的真实许可/worker 模块 23 项通过；永久 legacy/v1 输出失败回归同时断言 NotDispatched 且无 Unknown，通过。该模块仍由测试按真实源码路径编译，主目录生产入口尚未接线。两处账本 lint 已修，后续统一 Clippy 待复验。
- DES-12 首轮整批外置通过 4 项真实 loopback 与相关模块回归，但整合 check 曾被在途基础测试的编译错误阻断（随后已修）。引用缓存的磁盘复核、登记前回滚、高扇出预览分配及 Host 持久化失败边界正在补齐；证据 `batch-output-budget-qa/` 不等于该功能已完成。
- DES-09 隔离副本首轮 check/Clippy 与 11 项 SIGKILL 恢复通过，证据 `durable-first-integration-qa/`；尚未合回主树。跨进程运行期间新增未知、溢出确认、取消后的 live-owner 未决及跨回合 fence 生命周期仍需修正和复验，不把首次重启闭环当作全部边界完成。

- DES-03 单项收口：`--no-shell` 由公共 `ToolFilter::without_shell()` 实施；聚合 shell/bash 在同一 policy 被拒，命名工具不再广告，文件读写/原 deny/父上限保持。CLI/SDK 真实 loopback 与策略共 21 项通过，证据 `shared-no-shell-qa/`。任务状态隔离实现虽通过大量回归，审查仍发现并发准入提前放行及 deferred worker 的队列唤醒/返回时点问题，未据此回迁或标完成。

## 必须继续完成的实现与验收

| 决策 | 当前事实 | 未完成工作 | 需要的证明 |
| --- | --- | --- | --- |
| DES-03 | CLI/SDK 共用 run* 与 Host；已有分层检查 | 语义参数映射机检、Ravomi 接入对照 | 相同配置下准入/取消/恢复一致，无 CLI 私有执行语义 |
| DES-04 | CLI/dedup 在租约前写 active，journal/engine run ID 不同，阻塞 worker 回收与终态脱节已确认；基础模块未接线 | 将 canonical task tracker 接入真实租约、CLI/RPC、子代理及持久化投影；移除运行路径独立状态写者 | 排队不伪称运行，取消应答不提前 settled，恢复观测不伪造终态或 run ID |
| DES-05 | 权威工具状态机、v2 canonical calls、真实审批摘要/身份/typed 输出引用已接线，定向及本轮主树门禁通过 | 后续预算/持久保护改动完成后重新冻结验收 | 非法转移拒绝、终态不回退、Denied 零派发、回调快照无死锁、扩展参数正文不落盘 |
| DES-06/07/08/10 | 门禁、绑定、预算与取消主体及测试已有 | 对当前统一版本重跑 | 全入口拒绝零副作用、预算与取消收敛、失败证据完整 |
| DES-09 | 类型化 Unknown、同运行树 fence、证据 revision/容量 fail-closed 与显式确认已通过定向验收 | 跨重启持久化未决账本、精确确认与真实重启闭环 | 读错误有界重试，未知写/外部副作用不自动重放 |
| DES-11/13 | 显式 Pinned 和结构化报告已接入 | 完成当前整合回归、确认持续兼容 | 实际请求原文不丢；不足则零 Provider 请求；报告可恢复且不复制正文 |
| DES-12/17 | 分页、外置、生产登记/回滚、activity 租约及 CLI dry-run/confirm 已接入 | 整合编译与回归、结构化引用与实际 call_id、磁盘满故障 | 活动捕获排斥清理、引用保护、陈旧计划拒绝、失败不留孤儿、真实任务分页读完 |
| DES-14/16 | initialize、版本字段、有界队列已实现 | 重跑慢消费者/失败断连回归 | 容量有界、终态/审批不静默丢失、连接失败阻断后续副作用 |
| DES-15 | 真实 Host/dispatch/子代理 ID、v1 封套及 loopback/RPC 测试已接入 | 准入拒绝不误报 dispatch、拒绝后配对不污染、统一版本复验 | run/turn/工具/审批/错误可追踪；子代理不改写真实 engine runId |
| DES-18 | checkpoint/rewind、用户后改保护与真实 SIGKILL 组合 5/5 已验证 | 最终整合复验；其他平台和断电不作通过声明 | 不覆盖后续改动、不重复副作用、不改 Git（当前为自建元数据哨兵） |
| DES-19 | SDK/feature/版本/许可证元数据检查已有 | 统一生命周期/API/示例编译证据 | 最小/完整宿主与配置矩阵通过，平台未运行不作通过声明 |
| DES-20 | Shell、插件、本地 MCP 路由、PolicyBoundTools 与未绑定 extra_tools 拒绝已接入；严格 Linux 实测 39 项通过 | 最终整合门禁；macOS/Windows 未实现 OS 沙箱 | 不可用不降级、未覆盖入口不能绕过，跳过用例不当成功 |

## 执行与记录规则

重型任务按 pi-delegate 配额排队，保留已确认存活的句柄，不因等候重复启动。并发编辑导致验证源码漂移时，记录具体范围，不能把各次绿色拼成不可变版本的全绿。实现完成后须对稳定源码完整跑 Rust 四门禁及相关终端/恢复/评测自检；评测继续沿用 PRD 的两套任务集、同预算 6/6 与长文独立至少 3 次、至少 2/3 通过门槛。所有失败与未运行项如实保留，不扩大预算、不放宽验收。
