# 跨重启未知副作用账本（DES-09）

本文说明 Mio 的跨重启「未知副作用」账本：它记录一次真实派发在结果确认之前进程就
结束的事实，使重启后仍能知道「这个调用可能已经产生了副作用」，不会当作「没发生过」
直接重放。

## 存储位置与权限

- 工作区级账本：`<workspace>/.mio/unresolved-effects/effects.json`
- 叶子锁文件：`<workspace>/.mio/unresolved-effects/effects.lock`
- 目录 `0700`、文件 `0600`、`O_NOFOLLOW`；写入走同目录临时文件 + `fsync` +
  `rename` + 目录 `fsync`；跨进程临界区用 `flock` 串行。
- 只读路径（`snapshot` / `lookup` / `--recovery`）**不加锁、不创建任何文件或目录**：
  `rename` 原子替换保证读到完整的旧文件或新文件。

## 主键与状态机

主键是真实 `(scope, call_index)`：`scope` 是引擎 run scope，`call_index` 是该 run
内单调的工具调用序号，与 journal / `tool_lifecycle` 使用同一对身份字段。

```text
(无记录) --record_dispatched--> Outstanding
(无记录) --record_unknown-----> Unknown
Outstanding --record_unknown--> Unknown
Outstanding --resolve_confirmed-> Resolved
Outstanding --recover_outstanding-> Unknown   （owner 非正向存活）
Unknown --acknowledge----------> Acknowledged
Outstanding --release_undispatched-> (无记录)   （本地确证从未派发；纯意图撤回）
```

终态是 `Resolved` 与 `Acknowledged`，不回退。`Unknown` 只能由宿主持**精确快照键**
显式确认后离开。证据不完整的记录（`facts_dropped > 0`）一律拒绝确认，保持 `Unknown`
与全部事实：宿主不能确认一份它没有完整看到过的现场。

`Outstanding --release_undispatched` 只用于本地已经确证**从未派发**的纯意图（例如派发
意图落盘后、真正交给 bus 之前 journal 写入失败）。它不携带任何未知证据，留着只会在以后
的恢复里报出一个假的 `Unknown`。任何携带事实/恢复/确认的记录都不能被撤回，只能
`resolve` 或 `acknowledge`。

## 覆盖 epoch（coverage）

账本是**权威**的：一次真实的新 runtime 写/外部副作用在派发前先过账本，所以 journal
的保留裁剪（`dropped_runs` / `truncated_operations` / `dropped_calls`）在 epoch 之后
**不再**凭空生成「未知」。但在 epoch 之前，旧二进制没有写账本，journal 里的缺失可能
对应真实的、未被表示的旧副作用。为区分两者，账本持久化一个私有的覆盖 epoch 状态：

- `coverage_initialized`（旧账本 serde 缺省 `false`）：**首次**对全工作区 journal 的
  有界扫描完整且无丢失/损坏、并且所有真实旧 `Unknown` 都成功迁移写入时，才在同一个
  `flock` 事务里原子置位。只读打开、或部分迁移失败创建的账本文件**不是**证明。
- `coverage_incomplete`（单调 latch）：一旦扫描看见真实的丢失/裁剪（`dropped_*` /
  `truncated_operations` 非零）或损坏/超限/版本不符的 source，就置位且**永不**被任何
  ack 清除。
- 初始化未完成或 incomplete 为真时，`claim_dispatch` 对写/外部调用返回 `BlockedByCoverage`：不完整/
  缺失证据不可普通确认，也不会被伪造的 `(scope, call_index)` 代替。只读调用（read /
  grep / find / ls，以及 subagent 的 status / snapshot / list）**完全不进**效果账本，
  因此 damaged/uninitialized ledger 永远不会挡住读。
- epoch 提交后每回合仍扫描可见的、账本中未表示的旧 `Unknown`（含真实
  `journal-legacy:` origin）并迁移；journal 保留裁剪被忽略（新 runtime 的副作用早已是
  账本权威）；损坏/超限 source 仍 fail-closed 并 latched。
- 跨进程竞态：只有一个进程能把 epoch 从「未提交」原子提交为「已初始化」；落败者拿到
  `AlreadyInitialized` 后会继续导入自己扫描到的真实未知，不会因为它们而放行一次写。
  部分迁移失败不会置位成功。`--no-session` 不增加该账本的读写；只读查询不创建或修改文件。
- 该保证限于遵守账本协议的 runtime 与协作写者；绕过协议的旧二进制不作为安全兼容。

## 入口接线

- `Host::on_tool_call_lifecycle` 在真实派发边界处理 `Dispatched`：先在账本 `flock`
  事务里 `claim_dispatch` 真实 `(call.scope, call.call_index)`，**同一个事务**先确认
  工作区覆盖 epoch 已完整提交（否则 `BlockedByCoverage`，写/外部 fail-closed），再重
  新检查整个工作区是否仍有未确认的 `Unknown`；然后才由 journal 写 canonical
  `Dispatched`。因此另一个进程/session 在本回合 seed 之后新增的 `Unknown` 也会拒绝
  本次写/外部派发，而拒绝不会在 journal 里留下一条从未派发的 `Dispatched`。若 journal
  写入随后失败，本进程可以把这条从未派发的纯意图 `release_undispatched` 撤回，绝不
  留下假 `Unknown`。
- 纯读调用根本不会调用账本：`read` / `grep` / `find` / `ls` 以及 subagent 的
  `status` / `snapshot` / `list` 不写效果账本、不进回合未决列表，绝不 fence 后面的写；
  journal 仍如实记录它自己的 `Dispatched` / `Unknown` 相位。合格 plugin/MCP 操作与
  subagent `spawn` 仍是写/外部，不会被误当读。
- 同一主键重复派发被拒绝，绝不重放。
- 终态 `Executed` / `Failed` 调用 `resolve_confirmed`；`Unknown` 从不会被改写成
  `Executed` 或 `Resolved`。
- 工具体内发布的类型化 unknown 先落 `record_unknown` 再继续；落盘失败时运行内闸门
  `mark_persist_failed`，后续 write/external 一律拒绝。
- 读取工具不会因为账本错误被阻止；只有 write/external 由 `Policy` 的现成闸门拦截。
- 回合结束时，本回合自己取得派发权但仍 `Outstanding` 的记录会被保守转为 `Unknown`
  （`finalize_turn_dispatches`）：同进程 owner 可能仍存活，恢复不会升级它，但未确认的
  派发必须阻塞后续写。只覆盖本回合自己的真实 `(scope, call_index)`，不升级别的进程/
  子代理仍在执行的调用。

## 恢复与播种

- 顶层 `CliHost` 每回合执行前先 `recover_outstanding`，把 owner 非正向存活的
  `Outstanding` 保守升级为 `Unknown`（owner 正常存活时保持 `Outstanding`，不误判
  死亡），并把结果播种到**该回合新建的**运行内 `UnknownEffectFence`。每回合一个新闸门
  是真实的 top-run 生命周期：长期 session 不会因为累积已 ack 历史而把有界闸门撑到
  overflow，旧 run-scope 别名也会随旧闸门一起消亡，使有界 scope registry 能被裁剪。
  子代理在这一次运行内仍共享父级同一个 `Arc`。持久写失败 latch 是工作区属性，会跨
  回合保留。
- 播种保留真实 ledger key 与 snapshot hash；恢复的 dispatch 没有类型化原因时记为
  真实的 `interrupted_dispatch`，绝不冒充 MCP 传输失败。缺失的 legacy id 保持
  `None`，不伪造。
- 同一 ledger key 的新 snapshot（旧记录后来追加了新事实）会**更新**已播种记录并推进
  revision，不会被 key-only 去重吞掉：旧确认不再放行，宿主看到的是新的精确摘要。
- 旧 journal（`v1` 或 `v2`）里仍 `Dispatched`/`Unknown` 的 canonical 调用如果在新
  ledger 里不存在，会用真实 `(scope, call_index)` 迁移成 `Unknown`。迁移是**整工作区
  有界的只读扫描**：不只当前 session，其他旧 session 的 journal 也会被检查，因此
  「先开一个新 session」不能绕过旧的未决记录。
- 没有 canonical key 的 legacy operation 用**由真实 journal 身份派生的独立 origin**
  （`journal-legacy:<session_id>:<run_id>` + 真实 `op_id`）迁移成可精确 inspect/ack
  的记录，绝不伪造 engine run UUID、provider id 或 evidence id，也不是「永久
  fail-closed 且无可恢复动作」。缺失/截断的证据**不会**被造成可 ack 的假记录：
  epoch 之前它让覆盖初始化 fail-closed 并 latched，epoch 之后它就是新 runtime 的
  正常保留裁剪，由权威账本覆盖。
- 损坏、超限、版本不符或结构非法的 journal fail-closed，绝不当作空集；扫描的文件数
  与总字节都有上限，超过即 fail-closed（并 latch）。只读查询与 `--no-session` 不做
  任何迁移写入（`--no-session` 的 ledger 为 disabled，直接零 I/O 返回）。
- 新 provider id、新 run 或新 session 都不会清除未知。
- 派发所有权按真实 `RunObservation` 作用域管理：只有被观测到真正结束（终态观测）的
  run 遗留的 `Outstanding` 才会在回合结束被保守转为 `Unknown`；仍在运行的子 run（可
  跨父回合存活）不会被误标 `Unknown`，其 `(scope, tool_call_id)` 关联也不会被新回合
  清掉。

## 宿主确认（ack）

`confirm_unknown_side_effect` 展示精确的记录 key 与 snapshot hash；批准后按该快照
键持久 ack。已 ack 的记录可直接复用；确认期间追加了新证据会让旧键陈旧，此时拒绝
确认，绝不按提示后重新取快照并 ack 全部。ack 只确认现场，不修改 journal 中原有的
`Unknown`。证据不完整（`facts_dropped > 0`）的记录一律拒绝 ack，保持 `Unknown` 与
全部事实。

## 维护通道：精确 inspection / ack

当未决 `Unknown` 数超过运行内闸门上限（`MAX_UNKNOWN_EFFECTS`）时，普通确认无法放行，
因为闸门看到的是不完整快照。此时提供一条**不属于模型可自动调用工具**的维护路径：

- `mio --list-unresolved-effects [--json]`：只读列出每条持久记录的精确
  `(scope, call_index)`、状态、`factsDropped` 与 snapshot digest，并标注哪条
  `acknowledgeable`。绝不建议或执行删除账本。
- `mio --ack-unresolved-effect '{"scope":"...","call_index":1,"snapshot":"<sha256>","note":"..."}'`：
  只确认操作者实际看到的那一条精确记录。它拒绝陈旧 digest、`facts_dropped > 0`
  的非完整记录、非 `Unknown` 状态和不存在的 key；绝不写 journal、绝不删除证据。

这条通道让操作者逐条恢复，同时不把确认权交给模型自动调用。

## 容量、字节与 fail-closed

- 容量或字节压力下只裁剪终态 `Resolved`（结果已确认已知、不携带未知证据）并累计到
  `resolved_trimmed`；`Outstanding`、`Unknown` 与仍带未知事实的 `Acknowledged`
  一律不驱逐。满员且无可裁剪的 `Resolved` 时新记录 fail-closed。
- 损坏、未知版本、超限、符号链接、重复主键、坏 owner 元数据、ack 摘要与证据不一致
  全部 fail-closed，绝不静默返回空账本。
- 容量压力的诊断只说明「需先解决或 ack 未决记录」，绝不建议删除账本文件，因为删除
  会摧毁可能已经发生的副作用的记录。

## `--no-session` 与 `--recovery`

- `--no-session`：账本完全禁用，不执行任何磁盘 I/O，也不提供任何未决保证；运行内
  闸门仍在本进程内生效，但零额外落盘。
- `--recovery`：只读查看，不创建、不迁移、不做 sweep。RPC `get_state.data.unresolvedEffects`
  以及 CLI `--recovery --json` 的 `unresolvedEffects` 返回持久摘要：

```json
{
  "mode": "read_write",
  "enabled": true,
  "present": true,
  "entries": 3,
  "unknownCount": 1,
  "outstandingCount": 2,
  "resolvedTrimmed": 12,
  "coverageInitialized": true,
  "coverageIncomplete": false,
  "error": null
}
```

账本损坏时 `error` 为错误文本，`present` / `entries` 等为 `null`，绝不降级为「安全空集」。
`--list-unresolved-effects --json` 同样返回 `coverageInitialized` / `coverageIncomplete`，
方便操作者看出写为何被 `BlockedByCoverage` 拒绝。

## 运行内追踪的有界容量（round 4）

- run 存活表（`MAX_TRACKED_RUNS` = 4096）满且**全部为活跃**时，绝不 clear 表、也绝不
  悄悄增长：新写/外部派发 fail-closed（读仍可用），直到某个真实结束的 run 被裁剪腾位。
- `(scope, tool_call_id) -> call_index` 关联表（`MAX_TRACKED_DISPATCHES` = 8192）只裁
  剪已被真实 `RunFinished` 观测为结束的 run；满且全为活跃时额外写被拒绝，已在派发
  中、但尚未写入关联的账本 claim 会被 `release_undispatched` 撤回，不留下假 `Unknown`。
- 终态 `Executed`/`Failed`/`Unknown` 的回合 claim 立即从 `turn_claims` 删除，不会等到
  回合结束才累积；仍在运行子 run 的关联与 claim 跨父回合保留，直到它真正结束。
- 不在持有关联表锁时再去取存活表（反之亦然），避免反向锁序死锁。

## 运行中未知事实的确认

运行时 marker 的真实 `evidence_id` 必须在询问批准前唯一解析为已持久化记录的
`(scope, call_index)` 与 snapshot；marker 已知真实 engine scope 时，还须与记录一致。
缺少记录、多个匹配或证据不完整均拒绝，不发出可误导的批准请求。批准后只确认这份
预先捕获的快照；等待期间证据改变则确认失败。已从恢复查询取得的 key/snapshot 不换成
批准时的新快照。JSON 非交互运行不会自动批准；`--no-session` 禁用持久账本时不能使用
CLI 的这条持久证据确认路径，未知结果仍阻断新的副作用。

## 验收状态

持久未知账本、运行内证据唯一解析、engine scope 一致性检查及 broker 快照确认已整合到主树，并纳入全量 Rust 门禁。真实 SIGKILL 12 项与 CLI/RPC 原生进程 Unknown 恢复链已有专项验收；各项证据绑定的源码、二进制和命令见 [主树验收记录](reviews/2026-10-16-v1-acceptance.md)。早期隔离候选 round5/round6 的结果保留作历史，不替代主树验收。

这些结果不代表跨平台、硬件断电或真实 wait(2) 失败注入已完成。coverage incomplete 是不可由普通 ack 清除的证据缺失状态；当前没有重建已丢失证据的自动修复命令，不应通过删除账本冒充恢复。
