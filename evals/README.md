# Mio 评测目录

本目录存放 Mio 的**自有验收探针**（self-acceptance probes）：用固定任务集验证 Agent 在真实工具循环下能否改对文件。它是回归信号，不是行业 benchmark，分数不能与公共榜单或竞品宣传数字直接对比。

## 自有验收与公共 benchmark 的区别

| 维度 | 自有验收（本目录） | 公共 benchmark（如 Harbor 上的数据集） |
| --- | --- | --- |
| 目的 | 同一模型、同一预算下自己对自己的回归对比 | 按官方协议进行跨模型/跨框架比较 |
| 任务来源 | 本仓库手工维护的小任务，可读、可审计 | 外部维护的数据集，任务不可改动 |
| 判定方式 | 任务自带的独立验收脚本，通过为 1、失败为 0 | 数据集官方判分器，通常含隐藏测试 |
| 隔离 | 每个任务一个临时目录；验收脚本仅需标准库，Agent 可访问网络 | 沙箱/容器环境，按数据集要求 |
| 发布口径 | 只描述本任务集结果 | 发布榜单分数与置信区间 |

本目录**不宣称**：跨厂商性能对比、质量领先或任何排行榜位置。token/cache 数据只在显式打开 `--event-format mio-jsonl` 或 `pi-jsonl` 时从对应 Agent 事件聚合，仍是本机单模型回归信号，不是计费口径；`cost_usd` 永不推算。原因见 [指标限制](#指标限制)。

## 目录结构

```
evals/
  README.md                     # 本文件
  configs/                      # 可直接使用的运行配置
    mio-local.json              # 默认：target/debug/mio
    mio-deepseek.json           # DeepSeek 回归：--json 事件 + 真实 token usage
    mio-deepseek-workflows.json # DeepSeek 工作流回归
    mio-deepseek-comparison.json # 独立 profile：Mio 工作流对照
    pi-deepseek-comparison.json  # 独立 profile：pi 工作流对照
    pi-example.json             # 示例：跨 Agent 对比
  tasks/workflows/              # 多文件、任务内错误恢复、长文件检索：mio-workflows-v1
  tasks/coding/                 # 旧版小型回归：mio-python-acceptance-v1
  tasks/review/                 # 结构化代码审查试验：mio-review-v1（不计入 v1 六任务门槛）
    <task-id>/
      task.json                 # 任务元数据（id、task_set、验收脚本与超时）
      instruction.md            # 发给 Agent 的指令
      initial/                  # 初始文件，原样复制进任务工作区
      acceptance.py             # 独立验收脚本，工作区在 sys.path 首位
  adapters/
    harbor/README.md            # 公共 benchmark 适配计划（尚未接入）
    pi-fixed-sampling.ts        # pi 对照的显式采样适配器
```

每个任务目录自包含：初始文件、指令与验收脚本都在目录内，评测器只从磁盘加载，没有内嵌 Python 字符串。新增任务 = 新增一个目录，不需要改 `scripts/agent_eval.py`。

`tasks/review/` 包含 5 个独立审查任务（`review-session-cache` 两处缺陷、`review-invoice-totals` 三个独立计费成因、`review-retry-backoff` 单缺陷边界、两个无缺陷等价重构）。Agent 只能写 `review.json`，使用 `findings` 的严重程度、置信度、路径、行号、证据和解释，以及总体结论；独立验收检查源文件保持不变、缺陷没有漏报/重复、证据引用改动表达式并解释后果、无缺陷任务没有误报。报告限 16 KiB，单条证据与解释各限 2048 字符。判分修正后的任务集版本为 2；旧版五任务报告原始 3/5、仅重判 4/5，版本 2 第二次独立运行 3/5，见[试验记录](../docs/reviews/2026-10-17-p2-review-five.md)。不同判分版本不可混算。它只是小规模自有试验，不等于 Mio 已有内建审查模式。运行前可用 `python3 -B tests/agent_review_tasks.py` 验证反向 diff 与正确/错误报告的判分方向；真实模型配置为 `evals/configs/mio-deepseek-review.json`，会产生费用，凭据只从环境读取。

## 运行命令

```bash
# 1. 离线 harness 自测：不调用真实模型，只验证评测器本身
python3 -B tests/agent_eval_harness.py

# 2. 查看可覆盖的 CLI 参数
python3 scripts/agent_eval.py --help

# 3. 用默认配置跑 Mio（报告写到 target/evals/mio.json）
python3 scripts/agent_eval.py --config evals/configs/mio-local.json

# 4. 显式参数覆盖配置：CLI 永远优先于 --config
python3 scripts/agent_eval.py --config evals/configs/mio-local.json \
  --label mio-debug --timeout 240 --output target/evals/mio-debug.json

# 5. 单任务调试（可重复 --task）
python3 scripts/agent_eval.py --task stable-toposort --output target/evals/one.json

# 6. DeepSeek 回归：打开 --json 事件解析，报告聚合后的真实 token usage
python3 scripts/agent_eval.py --config evals/configs/mio-deepseek.json

# 7. 隔离的对照运行：清除子进程的 MIO_*/PI_* 覆盖并保留原始产物
python3 scripts/agent_eval.py --config evals/configs/mio-deepseek-comparison.json \
  --isolate-agent-env --artifacts-dir target/evals/artifacts/mio-comparison
python3 scripts/agent_eval.py --config evals/configs/pi-deepseek-comparison.json \
  --isolate-agent-env --artifacts-dir target/evals/artifacts/pi-comparison

# 8. 两侧都完成后才比较：报告必须同 profile 才能产出对照
python3 scripts/compare_agent_eval.py \
  --baseline target/evals/mio-deepseek-comparison.json \
  --candidate target/evals/pi-deepseek-comparison.json \
  --output target/evals/comparison.json
```

运行真实 Mio 前需已有 `target/debug/mio`（可用 `cargo build --locked --bin mio` 构建），并配置好模型凭证；这些命令会调用真实模型。离线自测无需构建。

退出码：全部任务通过为 `0`，否则为 `1`；参数错误为 `2`。

`--config` 是可选入口，可覆盖的键为 `label`、`agent_command`、`timeout_seconds`、`output`、`tasks_dir`、`tasks`、`event_format`、`isolate_agent_env`、`artifacts_dir`、`diagnostics_dir`、`diagnostics_constraints`。CLI 显式参数覆盖同名配置键；`budget` 只能在配置文件设置，包含模型、thinking、调用/token 上限声明及可选 `temperature` / `max_output_tokens`。配置文件里的相对路径（`tasks_dir`、`output`、`artifacts_dir`、`diagnostics_dir`）相对**配置文件所在目录**解析；CLI 路径相对当前工作目录。没有写 `isolate_agent_env` / `artifacts_dir` / `diagnostics_dir` 的既有配置行为完全不变：子进程继承当前环境，任务目录随运行结束删除，不写任何诊断证据。

请通过环境变量或 Agent 自身的凭证存储配置认证，不要把密钥写入配置或命令。报告会写入 `argv` 模板，但密钥类参数值在落盘前被替换为 `[redacted]`；进程环境不会写入报告。

## 真实 usage（可选，默认关闭）

默认情况下 runner 不读取 Agent 的任何输出：`usage` 各项为 `null`、`source` 为 `unknown`、`coverage` 为 `unknown`、`model_calls` 为 `null`，行为与旧版一致。显式打开后，runner 按所选格式解析 JSONL。以下为 Mio 的 `mio-jsonl` 规则：

- CLI：`--event-format mio-jsonl`
- 配置：`"event_format": "mio-jsonl"`

两者都要求被测命令带 `--json`（或 `--mode json`），让 Mio 把每个完成的模型调用写成一行 `{"type":"completion_call","data":{"runId":...,"callIndex":...,"usage":{...}}}`。`evals/configs/mio-deepseek.json` 就是这种配置（模型 `deepseek-chat`、thinking off、`--max-model-calls 20`、`timeout 180`）；凭证只从环境读取（`DEEPSEEK_API_KEY`），不写入配置或命令。

解析与聚合规则：

- 只接受顶层 `type` 为 `completion_call` 且带 `data` 对象的行；其它事件、普通文本、缺字段或畸形行一律安全忽略，不会进入报告。
- 以 `(runId, callIndex)` 去重；同一个调用的重复行只计一次；缺少可用 `runId`/`callIndex` 的 `completion_call` 视为畸形。
- 每个 token 计数只在**所有**任务的所有调用都上报过时求和；任何缺失让该项保持 `null`，绝不用 `input_tokens + output_tokens` 之类的推算补齐，也不推算价格（`cost_usd` 恒为 `null`）。
- `coverage`：`unknown`（没有可用计数）/ `partial`（部分调用或计数缺失）/ `complete`（每个调用都上报了全部计数）。缺事件报 `unknown`，不报 0。
- `model_calls` 记录去重后观察到的已完成调用数（不包含未发出完成事件的失败尝试）；没有观察到任何 `completion_call` 时为 `null`（不报 0，避免把未采集当成零调用，原始扫描次数在 `events.accepted`）；`events` 记录本次有界扫描的分类计数（`scanned` / `accepted` / `duplicates` / `ignored` / `malformed` / `truncated`）。
- cache 字段按 mio-core `Usage` 的真实定义映射：`cached_input_tokens`（缓存读）与 `cache_creation_input_tokens`（缓存写）都是 `input_tokens` 的组成部分，不是额外 token。
- 临时 `agent.log` 逐行有界解析（总字节、总行数、单行长度、事件数都有上限）；行内容分类后立即丢弃，报告只保留聚合数字，不保留原始回复、日志或密钥。
- 每行 `completion_call` 的 `finishReason` 会按 mio-core 的归一化词汇聚合到 `finish_reasons`：`stop` / `length` / `tool_calls` / `content_filter` / `other`（provider 自己的拼写，报告只记 `other` 桶，不复制原始拼写）/ `unreported`（provider 未上报，不等于 `stop`）。无法识别的拼写使整个 `completion_call` 记为畸形，而不是被平滑成 `stop`。

Mio 与 Pi 的终止原因词汇**不是同一把尺子**。报告分别用 `finish_reasons`（Mio）与 `stop_reasons`（Pi）记录，从不互相换算；`compare_agent_eval.py` 只承认 `stop`↔`stop`、`length`↔`length`、`tool_calls`↔`toolUse` 三对。Pi 的 `error` / `aborted` / `deferred` 与 Mio 的 `content_filter` / `other` / `unreported` 没有对应项：`completion_call.finishReason` 只承载 mio-core 归一化的 `FinishReason`，取消读取记录在消息层的 `StopReason::Aborted` 而不是完成调用的终止原因，所以对照里 `aborted_model_calls` 显式标为不可比、保持 `null`（表示“报不出来”，不是 0），也不计算 delta。

扫描触及字节、行数或事件数上限，或发现畸形调用事件时，完整调用数与 Token 总量均标为 `null`；已观察的事件数保留在 `events.accepted`，防止把部分数据当成完整用量。

另支持 `--event-format pi-jsonl`：从 pi 的 assistant `message_end` 采集真实 usage，将 pi 的非缓存 input 与 cacheRead/cacheWrite 合为统一的 inclusive input；不推算缺失的 total 或价格。`stop_reasons` 记录已观察终止类别（`stop` / `toolUse` / `length` / `error` / `aborted` / `deferred`），`tool_use_prompt_tokens` 不纳入 pi coverage 完整度判定。pi 的 `usage.cost` 是目录估算而非实付，始终丢弃。Mio 的 `execution_timing` 与诊断类别规则不据此套用给 pi。其它 Agent 格式未支持。

### pi 固定采样适配器

pi 没有 `--temperature` 参数，所以 `evals/configs/pi-deepseek-comparison.json` 用 `--no-extensions -e {repo}/evals/adapters/pi-fixed-sampling.ts` 只加载一个测试扩展：它在 `before_provider_request` 钩子里先核对 `ctx.model.provider/id/api` 与输出上限字段名，再强制 `temperature: 0`、`max_tokens: 8192`；条件不符时抛错并把请求的 model 改成一个非法哨兵 id，使请求直接被 Provider 拒绝，而不是静默按默认采样发出。`agent_eval.py` 的 `verify_declared_sampling` 会拒绝“声明了固定采样却没有任何机制生效”的配置：命令既不传对应参数、也不加载该扩展时报参数错误。pi 的委派入口要求见下节。

## 工作流任务与耗时观测

```bash
python3 -B tests/agent_workflow_tasks.py
python3 scripts/agent_eval.py --config evals/configs/mio-deepseek-workflows.json
```

`mio-workflows-v1` 当前 `task_set_version: 2`（修正了恢复任务中“修复前首次失败、修复后空状态首次成功”的说明歧义）。它是独立任务集，保留原 `coding` 任务集及其版本：

| 任务 | 验收内容 |
| --- | --- |
| `migrate-line-item-api` | 实现、包内调用方和 CLI 的 API 迁移；整数税额与边界行为 |
| `resume-cache-build` | 运行任务内脚本得到真实错误、修复库、再次成功；检查执行记录与重新计算的产物 |
| `retrieve-wire-spec` | 从约 61 KiB 的长文档定位分散规则，完成解析与渲染，并保留无关文件 |

恢复任务只测任务内脚本失败后的处理，不测 Mio 重启、Provider 断线或进程崩溃。其执行记录是可修改的工作区文件，不是防伪审计凭证。检索任务只测磁盘文件检索与约束应用，不测跨轮上下文压缩。验收脚本位于工作区外，但临时目录不是 OS 沙箱，不应把该布局宣称为恶意 Agent 无法访问的隔离边界。

Agent 运行时的 `read` / workspace `read` 单次输出现在按已解析输入预算的四分之一收窄，限制在 256 字节至 64 KiB 之间；完整行分页与 `Next offset` 提示保持不变。直接创建 `Workspace` 的调用者默认仍为 64 KiB，可显式设置实例上限。该策略不保证多个并行结果、JSON 转义开销或其它工具的大输出一定能放入上下文，超长单行仍返回分段读取提示。

启用 `mio-jsonl` 且使用支持计时的新 Mio 二进制时，报告还汇总 `execution_timing` 事件到每任务及顶层 `timing`：

- `model` / `tool`：观察到的完成调用数、outcome 层错误数和 `duration_seconds_sum`。按 `(runId, effectId)` 去重，不保留工具参数、回复或路径。
- 时间来自单调时钟，覆盖实际 dispatch 到 outcome；模型跨度包括请求构造、网络、服务端生成与流式接收，不单独推断服务端计算耗时。工具跨度也可能包含工具内部等待。
- `errors` 依据底层 outcome disposition，不等同于命令退出码：内置工具可能把命令失败编码在成功返回的文本中，此时该计数仍为 0。判断任务恢复须同时依赖任务验收，不把工具 success 当作任务成功。
- 前置调度排队与上下文准备不在这些跨度内，`context_seconds` / `queue_seconds` 保持 `null`；没有直接测量，不能从剩余 wall time 冒充这些阶段。
- 当前内置子代理 Host 尚未转发此计时回调，子代理的 usage 与父侧计时不应直接对应计算耗时占比。
- 并发跨度可能重叠，时长之和不能直接当作总 wall time 或相加计算占比。被取消或未到 outcome 的请求不产生完成跨度（包括部分失败模型请求）；`coverage: observed_outcomes` 只表示已观测完成事件，不表示全路径覆盖。
- 默认模式、旧二进制或未观察到事件时为 `unknown`；畸形事件或扫描上限使汇总保持未知，并标记 `partial`。

Mio 示例配置为每个任务使用 `{cwd}/../coordination` 临时协调目录。跨进程预算账本不会串入下一任务或用户的正常会话；同一任务的子进程仍可共享该目录。DeepSeek 配置预算版本为 `deepseek-chat-off-20calls-180s-isolated-v2`，旧的共用目录结果不能直接当成每任务独立 20 次调用的对照。

`diagnostic_categories` 仅保存 Mio 上报的已知错误类别（如 `context_budget`、`scheduling`），不保存诊断消息或原始请求；未开启采集或扫描不完整时为 `null`。

## 跨 Agent 对照入口

`evals/configs/mio-deepseek-comparison.json` 与 `pi-deepseek-comparison.json` 使用 `deepseek-flash`、thinking off、temperature 0、单次输出上限 8192、相同工作流任务和 180s wall timeout；双方均无模型调用或总 token 上限。pi 配置通过显式采样适配器固定参数并启用 `pi-jsonl`，两侧配置均固定 `isolate_agent_env: true`。对照必须两侧都开启隔离：只隔一侧会把环境差异混进结果。这些配置与 `deepseek-chat` / 20 次调用回归属于不同 profile，结果不得混用；配置就绪不等于实测完成。

比较器会检查诊断开关、采集上限与约束白名单；不一致即拒绝比较。旧报告缺少诊断字段按关闭处理，畸形声明保持未知。

对照还必须保持**相同的诊断开启状态**：两侧要么都开 `--diagnostics-dir`（各自独立目录、相同 event format），要么都关。只开一侧会把诊断开销与被测对象无关的状态差异混进结果；`mio` 侧只支持 `mio-jsonl` 诊断，`pi` 侧诊断恒为 `unsupported`，因此跨 Agent 对照默认两侧都不开启诊断，只有同 Agent 前后对照才同时开启。

### Pi 委派入口

正式跑 pi 必须经过统一委派入口，不能直接调用 `pi`，也不得删掉 `PI_DELEGATE_ACTIVE` 规避递归保护：

```bash
/home/waya/.local/bin/pi-delegate --owner codex --heavy -- --mode json <pi 参数...>
```

`pi-delegate` 只共享并发槽位，然后把 `--` 之后的参数原样交给 `pi-run` → `pi`，所以采样适配器与 `--mode json` 等参数位置不变。它在检测到已设的 `PI_DELEGATE_ACTIVE` 时直接报错，因此**必须从未经委派的环境启动 runner**（也就是先跑 runner，再由 runner 启动委派的 pi）。`--isolate-agent-env` 故意保留这个标记，不会把它删掉来绕过递归保护。

`pi-deepseek-comparison.json` 的默认命令是本机绝对路径，因此官方入口在别的机器上不可分发。若要做可分发或离线自检，可用 `--agent-command` 覆盖成普通 `pi` argv（保留 `--mode json`、采样适配器与其余 profile 参数）；但这样跑出来的就不是官方委派入口，报告与文档必须按此说明，不能把两者混为一谈。

### 执行顺序

1. `python3 -B tests/agent_eval_harness.py` 与 `python3 -B tests/agent_eval_comparison.py` 离线自检 runner 与比较门禁，不调用模型。
2. 冻结二进制并校验 SHA，再分别跑两侧配置（两个报告都写进 `target/evals/`，建议同时 `--artifacts-dir` 保留失败样本）。
3. 两侧都完成后跑 `scripts/compare_agent_eval.py`；不同 profile 会被拒。
4. 保留全部报告、对照 JSON 与原始产物，不覆盖、不删改初次失败样本。

## Agent 环境隔离（`--isolate-agent-env`，默认关闭）

默认 runner 让子进程继承当前环境。开启后，每个任务的子进程拿到一份净化过的环境：

- 删除所有 `MIO_*` 与 `PI_*` 变量，包括 `MIO_MODEL` / `MIO_PROVIDER` / `MIO_BASE_URL` / `MIO_API_KEY` / `MIO_TEMP_DIR` 等模型与运行时覆盖，以及 `PI_SESSION_ID` / `PI_PROVIDER` / `PI_MODEL` / `PI_REASONING_LEVEL` / `PI_CODING_AGENT` 等 pi session 与 provider 标记；因此环境里的旧模型或旧会话不会悄悄改变被测对象。
- **唯一例外是 `PI_DELEGATE_ACTIVE`，故意保留**：`pi-delegate` 用它拒绝嵌套委派，而豁免它会把“在已委派会话里跑 runner”变成可行的绕过手段。正确做法是从未经委派的环境启动 runner（见上节），而不是删掉标记。
- 把 `PI_CODING_AGENT_DIR` 与 `PI_CODING_AGENT_SESSION_DIR` 指向任务私有临时目录下的空目录（目录 0700），pi 读不到用户的 `~/.pi/agent`（`auth.json` / `models.json` / `settings.json`）也不会复用真实 session。
- `DEEPSEEK_API_KEY` 等 provider 凭据、代理设置与 `PATH` 照常继承：凭据本来就应从环境读取，而且官方 `pi-delegate` 需要真实 `HOME` 才能找到 `~/.local/bin/pi-run`，所以 runner 不动 `HOME`。
- 验收脚本与 Agent 用同一份环境；隔离目录随任务临时根目录一起删除（除非同时开了 `--artifacts-dir`）。

隔离只影响被测 Agent 的进程环境，不是 OS 沙箱，也不提供内存/网络/文件描述符限制。对照运行必须**两侧都开启**（pi 配置已内置，Mio 侧用 CLI 参数），否则环境差异会变成无法解释的变量。

## 原始产物保留（`--artifacts-dir DIR`，默认关闭）

默认任务目录随运行结束销毁，失败样本无法回看。指定 `--artifacts-dir` 后，每个任务的工作区、`agent.log` 与 `check.log` 会被拷到 `DIR/<task-id>/`：

- 权限私有：目录 0700、文件 0600，不跟随符号链接。
- 有界：单个日志与单个工作区文件最多各 4 MiB；每个任务最多保留 32 MiB 工作区内容与 1024 个文件，最多扫描 10000 个条目。超限通过 `truncated` / `workspace_truncated` 明确报告。这些上限约束产物拷贝，不限制 Agent 原运行的磁盘写入。
- 每次运行使用新的产物目录；已有同名任务目录一律拒绝覆盖，避免混入旧证据。符号链接目标也不能重定向写入或权限修改。
- 报告只在每个 result 的 `artifacts` 块记录目录、字节数与 `truncated` 标记，**不拷入文件内容**，因此报告不会因为开启产物保留而带出原始模型输出或密钥；顶层 `artifacts` 块只记录开关与根目录。
- 产物本身是原始 Agent 输出，可能含模型回复、文件内容或密钥，因此 **不提交仓库、不共享**；`target/` 已被忽略。

## 有界诊断采集（`--diagnostics-dir DIR`，默认关闭）

默认不采集诊断：runner 不写任何证据文件，也不额外读取 agent 日志。指定 `--diagnostics-dir` 后，每个任务在 agent 与验收脚本执行完、临时日志被删除前，把 `scripts/eval_diagnostics.py` 的有界脱敏输出写到 `DIR/<task-id>.json`：

- 只支持 `--event-format mio-jsonl`。`pi-jsonl` 与未指定事件格式时，诊断状态显式记为 `unsupported`（原因分别是 `pi_jsonl_out_of_scope` 与 `event_format_not_selected`），不写文件，也不假装采集 Mio 范围。
- 目录 0700、文件 0600；证据用 `O_EXCL` 独占创建。目标已存在时**在启动 agent 之前**就拒绝该任务（`status: error`、`failure_class: diagnostics_evidence_exists`），因此不会重复产生 agent 副作用，也不会覆盖旧证据。写入阶段才发现竞争（文件被并发创建）同样记为错误状态，绝不覆盖。
- 证据只保留 collector 允许的内容：workspace 相对 `read` 路径与显式上报的行/字节区间、来源与语义固定标签（`source`/`semantics`，以及每条记录的 `origin`）、重复读取计数、完成调用数与 `finishReason` 固定桶、压缩的 replaced-prefix source 范围计数（`source_messages`/`source_bytes`/`summary_bytes`）与完整 history 前后计数（`messages_before`/`messages_after`/`bytes_before`/`bytes_after`）及固定类别、`run_observation` 的模型准入后派发数（`kind=model_dispatched`）与准入前准备数（`kind=model_prepared`）、固定白名单的内置工具活动计数，以及调用方白名单内的 constraint id（goal id 只计数、绝不混入 constraint coverage）。宿主绝对路径、越界/符号链接路径、工具或模型文本、argv/命令、任意异常正文与密钥都不落盘；真实 `runId`/`effectId` 只作瞬态去重键，绝不写入证据。
- 内置工具活动口径（证据文件的 `tool_activity` 块）：真实 CLI 只把工具操作放在 `tool_started`/`tool_finished` 的 `data.operation`，所以 `request` 计 `tool_started` 观测，`result`/`error` 按 `tool_finished` 的 `Ok`/`Err` 判别键计数（**绝不读取结果正文**），`dispatch` 计准入后 `run_observation` 的 `kind=tool_dispatched`。这些是**观察到的 CLI 事件，不是执行次数、不是 unique call、不是账单**；`tool_started` 也不等于模型的 tool-call 请求，更不是准入记录。固定白名单为 `read` / `write` / `edit` / `grep` / `find` / `ls` / `shell` / `bash` / `workspace`，任何其它或扩展名（插件/MCP/子代理/未来工具/含密钥的字符串）一律折叠进 `other`，原名绝不保存；`names` 只列固定词表。`workspace` 是容器工具自身名字，当前 CLI 从不把它当作 `operation` 上报（容器子操作会落在对应读/写/grep 桶里），所以它在 legacy 流里恒为 0；`bash` 工具的 `operation` 实际是 `shell`，两桶都保留。某阶段的事件族从未出现时 `events` 与 `by_name` 保持 `null`（unknown，不猜 0）；只有该族确实出现过才给出真实 0。`run_observation` 的 `tool_dispatched` 不携带工具名，因此 `dispatch.events` 只能是总数、`by_name` 恒为 `null`，且当前 CLI 不发射它时保持 `null`。逐任务 `diagnostics.coverage` 额外给出 `tool_request_events`（观察到的 `tool_started` 总数，未观测为 `null`）。
- 模型派发口径：`model_dispatched` 是引擎在准入通过后真正交给 dispatch 总线的模型效应，**不是 wire/HTTP 请求、不是 Provider 回执、也不是账单**；被拒绝的 `model_prepared` 只计入 `prepared`，绝不计入 `dispatched`。三族事件各自独立：`requests`（legacy `model_request`）不与 `prepared`/`dispatched` 相加。三者都只在对应事件真实出现时给出数字，否则保持 `null`（例如日志只有 `completion_call` 时 `prepared`/`dispatched` 仍为 `null`，只观察到 `model_prepared` 时 `dispatched` 是真实的 0）。这些计数写在证据文件的 `model_calls` 块中；逐任务 `diagnostics.coverage` 同时提供 `model_calls_prepared` 与 `model_calls_dispatched`，未观测保持 null。CLI 普通 JSON 与 v1 均输出准入后模型派发事件，准备事件不作为派发公开。
- 报告新增顶层 `diagnostics` 块：`enabled`、`event_format`、`supported`、`constraints`、`limits`（collector 的字面上限）、`coverage`、`status_counts`、`requires_instrumentation` 与说明；每个 result 另带 `diagnostics` 块（`status` / `reason` / `written` / `source` / `coverage` / `truncation`），不包含证据文件路径。
- `status` 取值：`collected`（有界完整扫描）、`truncated`（触及事件/字节/字段/输出上限）、`unknown`（源缺失、不可读或日志没有可用事件）、`unsupported`（事件格式不在范围内）、`error`（目录或任务名非法、证据已存在、写入失败或 collector 故障）。任务 correctness 与诊断状态分开记录：诊断失败不会伪装成完整，也不改变任务通过判定。
- `--diagnostics-constraint ID`（可重复）或配置 `diagnostics_constraints` 提供显式离线 constraint 白名单；id 必须匹配 collector 的标识符规则且不超过 256 个（超限或非标识符报参数错误，不回显原值）。白名单内的 id 未被任何结构化事件命中时 coverage 保持 `unknown`。
- `--diagnostics-dir` 与 `--artifacts-dir` 完全独立：开启诊断**不会**保留原始工作区或日志，也不会为诊断偷偷打开 raw artifacts。
- 诊断状态只在前后对照一致时才可比：对照两侧必须同为开启或同为关闭（见下节）。

固定 spec 预算命令（示例；这些命令会调用真实模型，仓库离线门禁只跑 harness，**不实际发请求**）：

```bash
python3 scripts/agent_eval.py \
  --config evals/configs/mio-deepseek.json \
  --event-format mio-jsonl \
  --diagnostics-dir target/evals/diagnostics/mio-deepseek-iteration-03 \
  --diagnostics-constraint retain-wire-rules \
  --output target/evals/iteration-03/mio-diagnostics.json
```

必须为每轮重复运行使用新的 `diagnostics` 目录：已有同名任务证据会被拒绝而不是覆盖，**全部失败样本原样保留**。若某轮已有证据但需要重跑，请换新目录或显式移走旧证据，不要删除或改写初次失败结果。

`requires_instrumentation` 记录当前仍缺失或口径受限的真实事件：`model_dispatched`（`run_observation` 已定义该观察，但当前 CLI 的 V1 封套只上浮 `run_started`/`run_finished`，日志里没有它时 `dispatched`/`prepared` 保持 `null`；它有别于 wire 请求）、`model_request`（legacy 事件，引擎当前不产出，`requests` 保持 `null` 且绝不并入 `dispatched`/`prepared`）、结构化 constraint 证据，以及压缩 report 中的完整 history 前后计数与 constraint/goal id（snake_case 与 camelCase 都消费）。在这些事件真正接入生产 CLI 之前，相关字段保持 `null`/`unknown`，不得当作完整采集；**新增的完整 history 与 constraint/goal 字段必须由 Rust 侧真实发射并通过验收，Python collector 的兼容消费不能代替该验收**。

### 成功 `read` 观测与 legacy 尝试

一次成功的 `read` 可能会在同一个日志里出现两次：引擎为真实成功的读取发出结构化 `read_observation`，同时也发出旧的 `tool_started(operation=read)` 记录。为避免把同一次读取算两遍，`read_observation` 与 `tool_started` 分开记账，并在整份流层面做一次互斥选择：**只要流里出现过任何结构化 `read_observation`，整份流就以这些成功返回为准，所有 legacy `tool_started` 读取候选一律不计**；这不取决于事件顺序，先出现的 legacy 候选同样会被后出现的结构化观测抑制（反之亦然）。只有完全没有结构化观测时，才退回 legacy 尝试，并把它们的范围标为 `unknown`。

证据中新增固定标签字段区分两者：`reads.source` 取 `read_observation` / `legacy_tool_started` / `unknown`，`reads.semantics` 取 `successful_read_return` / `legacy_read_attempt` / `unknown`，每条记录另有 `origin`（`read_observation` / `tool_started`）与同名 `semantics`；`reads.legacy_attempts_suppressed` 记录因结构化观测存在而未计入的 legacy 候选数。扫描被截断（事件/字节上限）时整族选择本身也视为不可靠，`source`/`semantics` 保持 `unknown`，计数保持 `null`。

范围只来自事件显式上报的字段：行范围用 `offset`/`limit`（1 基起点与实际返回行数），字节范围用 `byteOffset`/`byteLimit`（实际返回字节区间），其余拼写不识别；数字出现在文件名里不会被当成范围。空文件或零长度返回（引擎以无边界、`total: 0` 的 `unknown` 观测上报）以及 `limit`/`byteLimit` 为 0 的畸形记录都保持 `unknown` 且边界为 `null`，绝不编造一行或一字节的范围。`read_observation` 只在读取真正成功后发出，失败读取不产生该事件，因此失败不会被标成成功返回；只有 legacy 尝试会被标为 `legacy_read_attempt`，不冒充成功。

collector 已按上述规则消费结构化 `read_observation`，但把它接入生产 CLI JSONL 封套的发布路径仍需另行验收：在真实事件落盘并复核前，不能把本目录的读取区间字段当作已完整的生产采集。

### 压缩前后计数与字段拼写兼容

真实的 `ContextCompactionObservation` 外层是 camelCase（`runId`/`kind`/`report`），其嵌套 `CompactionLossReport`（即 `report`）当前是 **snake_case**；旧测试封套曾用 camelCase。collector 对同一逻辑字段同时接受 `snake_case` 与 `camelCase` 两种拼写：只出现一种时按该值读取，两种都出现且等价时只接受一次，两种都出现但**冲突**时把该记录判为 malformed/unknown，**绝不挑选其中一个值猜测**。相反拼写的实际取值需由 Rust 侧统一，collector 不要求 Rust 改成 camelCase。

压缩报告里必须区分两类不同语义的计数：

- **replaced-prefix source 范围**（原有语义，保留不变）：`source_start`/`source_end`/`source_bytes`/`summary_bytes` 描述被摘要替换掉的那段半开前缀范围；`source_messages` 只等于 `source_end - source_start`，是该前缀内的消息数，**不是完整 history 长度**。
- **完整 history 前后计数**（新增字段，语义不同）：`before_messages`/`after_messages`/`before_bytes`/`after_bytes` 描述压缩前/后整个 history 的消息数与序列化字节数。它们**不是 prompt、tool、schema、token 计数，也不是某种语义损失**，只表示压缩前后的大小。

`messages_before`/`messages_after`/`bytes_before`/`bytes_after` 只来自显式的完整 history 字段；当报告没有这些字段（旧报告或未审计路径）或任一记录缺字段时，它们保持 `null`，**绝不用 `source_end - source_start` 冒充完整 history**。前缀范围计数同时以 `source_messages`/`source_bytes`/`summary_bytes` 单独呈现，便于区分这两种口径。扫描被截断或出现冲突拼写（`truncation.conflicting_fields`）时全部聚合保持 `null`。

压缩 report 中的 `preserved_constraint_ids`/`dropped_constraint_ids` 只用于填充调用方 constraint 白名单 coverage（仅约束 id 进入 coverage）；`preserved_goal_ids` 只按有界数量计数（`compaction.goal_ids`），**绝不与 constraint 混计**，也不存储任何 goal id 字符串。旧报告或未审计路径缺这些 Option 字段时保持 `unknown`，不伪零。

## 固定模型与预算版本

对比只有在「同模型、同任务集、同预算」时才成立，因此配置里有一个显式的预算版本块：

```json
"budget": {
  "version": "mio-eval-budget-v1",
  "model": null,
  "thinking": null,
  "max_model_calls": null,
  "max_total_tokens": null
}
```

- `version` 命名这套预算口径（`mio-eval-budget-v1` 仅为默认/示例 profile；DeepSeek 回归使用 `deepseek-chat-off-20calls-180s-isolated-v2`），报告原样记录；不同版本的结果不可直接混用。
- `model` / `thinking` / `max_model_calls` / `max_total_tokens` 是**声明元数据**：runner 把它们写进报告，但既不注入 Agent 命令，也不拦截调用。要让它们生效，需要同时把它们写进 `agent_command` 或环境变量（`mio-deepseek.json` 就同时镜像了 `--max-model-calls`）。
- 打开 `event_format: "mio-jsonl"` 后，报告 `usage` 会给出真实 token 聚合，可与这套声明对照，但 runner 仍不强制限额。
- 报告会写入 `"enforced_by_runner": false`，避免把声明误读成已生效限额。
- 任务集版本 `task_set` / `task_set_version` 定义在 `task.json`；同一 `tasks_dir` 下必须一致，否则加载即报错。

## 报告格式

下面是省略字段的版本化 JSON 示例；实际还包含 `enforced`、顶层 `diagnostics`、顶层及逐任务 `timing`、逐任务 `diagnostic_categories` 等字段，完整定义以 `scripts/agent_eval.py::build_report` 为准。示例配置将报告写到 `target/evals/<label>.json`（`target/` 已被 git 忽略，不会提交）。

```json
{
  "schema_version": 2,
  "report_version": "mio-eval-report-v1",
  "report_type": "self-acceptance-probe",
  "label": "mio",
  "generated_at": "2026-01-01T00:00:00Z",
  "started_at": "2026-01-01T00:00:00Z",
  "duration_seconds": 12.5,
  "task_set": { "id": "mio-python-acceptance-v1", "version": 1, "tasks": ["csv-reader", "..."] },
  "budget": { "version": "mio-eval-budget-v1", "enforced_by_runner": false },
  "agent_command": ["target/debug/mio", "...", "[redacted]"],
  "event_format": null,
  "isolation": {
    "agent_env_isolated": false, "cleared_env_prefixes": [],
    "kept_env_vars": [], "pi_agent_home": null
  },
  "artifacts": { "enabled": false, "root": null },
  "accepted": 3,
  "total": 3,
  "results": [
    {
      "task": "median-boundaries",
      "status": "passed",
      "failure_class": null,
      "accepted": true,
      "duration_seconds": 4.1,
      "agent": { "exit_code": 0, "timed_out": false, "seconds": 4.0 },
      "acceptance": { "exit_code": 0, "timed_out": false, "seconds": 0.1 },
      "usage": {
        "input_tokens": null, "output_tokens": null, "total_tokens": null,
        "cached_input_tokens": null, "cache_creation_input_tokens": null,
        "tool_use_prompt_tokens": null, "reasoning_tokens": null,
        "cost_usd": null, "source": "unknown", "coverage": "unknown",
        "model_calls": null,
        "events": {
          "scanned": 0, "accepted": 0, "duplicates": 0,
          "ignored": 0, "malformed": 0, "truncated": false
        }
      },
      "artifacts": null
    }
  ],
  "usage": {
    "input_tokens": null, "output_tokens": null, "total_tokens": null,
    "cost_usd": null, "source": "unknown", "coverage": "unknown",
    "model_calls": null, "note": "cross-task aggregate; same rules as per-task usage"
  },
  "unavailable_metrics": ["input_tokens", "...", "cost_usd", "quality_leadership", "cross_vendor_comparison"],
  "limits": "..."
}
```

每个结果都记录：任务标识、运行状态、失败分类、总耗时、Agent 退出码/是否超时、验收脚本退出码/是否超时，以及一个 usage 块。未开启事件解析、或未完整上报时，usage 各计数为 `null`；顶层 `usage` 只对各任务都完整上报过的计数求和，缺任何一项就保持 `null`。开启 `--artifacts-dir` 后，每个 result 额外带有上文所述的 `artifacts` 块（路径、字节数、`truncated`）；开启 `--isolate-agent-env` 后，顶层 `isolation.agent_env_isolated` 为 `true`。两者都不写入原始文件内容或环境变量值。开启 `--diagnostics-dir` 后，顶层 `diagnostics` 记录 `enabled` / `limits` / `coverage` / `status_counts`，每个 result 的 `diagnostics` 块记录该任务的 `status`（`collected` / `truncated` / `unknown` / `unsupported` / `error`）、固定 `reason`、是否写入，以及不含宿主路径的 `source` / `coverage` / `truncation` 计数。

开启 `--diagnostics-dir` 时，报告的每项 `results[]` 另有 `acceptance_evidence`（不在单独的工具诊断文件内）：只保存验收源码 SHA-256/行数、最多16个合法范围内的报告行号、固定异常类别及 unknown/truncated 等状态；输入日志最多256 KiB、源码最多8 MiB、该块最多4096字节。行号只匹配已配置验收脚本的路径，报告不保留路径、源码、断言消息或日志正文。`check.log` 合并 stdout/stderr，故标记为 `untrusted_reported_check_log`，可被失败程序伪造，不证明哪个检查实际执行或失败根因。默认不增加该字段、不调用采集器，执行与验收判定保持原规则。


### 运行状态与失败分类

| `status` | `failure_class` | 含义 |
| --- | --- | --- |
| `passed` | `null` | Agent 退出码 0 且验收脚本通过 |
| `failed` | `agent_exit_nonzero` | Agent 非零退出，验收不再作为通过依据 |
| `timeout` | `agent_timeout` | Agent 超过 `--timeout`，进程组被回收 |
| `failed` | `acceptance_failed` | Agent 正常退出，但验收脚本失败 |
| `failed` | `acceptance_timeout` | 验收脚本超过任务声明的超时 |
| `error` | `environment_error` | 进程无法启动等环境错误 |
| `error` | `diagnostics_evidence_exists` | 开启 `--diagnostics-dir` 且目标证据已存在；任务在 agent 启动前被拒，不产生重复副作用、不覆盖旧证据 |

失败原因不互相覆盖：即使 Agent 失败，验收脚本仍会独立运行并记录，便于区分「没改对」与「没改」。

## 指标限制

- **只测通过/失败**：任务集是少量确定性 Python 任务，用于回归，新增 workflows 覆盖小型跨文件改动、任务内错误恢复与长文件检索，仍不覆盖大规模重构、复杂陌生仓库或真实进程恢复。
- **token / cache 默认不采集**：默认不解析 Agent 输出；只有显式 `--event-format mio-jsonl` / `pi-jsonl` 时才从对应的 `completion_call` / assistant `message_end` 事件聚合，且只在完整上报时求和，`coverage` 标记完整度。`cost_usd` 恒为 `null`：不推算价格，也绝不用字节数或时长冒充账单。
- **`unavailable_metrics` 随格式变化**：默认模式下列出全部 token 计数；打开事件采集后按格式列出缺少的指标；`cost_usd` 始终不可用，pi 还列出 `tool_use_prompt_tokens`，并保留对比性声明。
- **不构成排名**：不同 `budget.version`、不同任务集版本、不同模型的结果不可比；样本量小，不计算置信区间。
- **日志默认不落盘，产物保留与诊断需显式 opt-in**：Agent 与验收日志写在临时目录，随运行结束删除；打开事件解析时日志在临时目录内逐行有界解析，只保留聚合结果，不持久化模型输出与凭证。`--artifacts-dir` 会主动保留原始工作区与日志（0700/0600、每文件 4 MiB 上限），产物可能含模型输出或密钥，需自行保管；报告仍只记路径、大小与截断标记，不拷入内容。`--diagnostics-dir` 在临时日志删除前另写一份 collector 脱敏证据（`DIR/<task-id>.json`，0700/0600、`O_EXCL` 拒绝覆盖），只含 workspace 相对路径、显式区间与允许计数，不打开 raw artifacts，也不把证据文件路径写回报告。
- **无 OS 沙箱**：任务在临时目录中执行，但 Agent 拥有当前用户权限；本目录不是安全边界。

## 公共 benchmark 适配

`adapters/harbor/README.md` 只描述待接入范围与接口计划，尚未有可用适配。当前报告格式保留 `report_type` 与 `unavailable_metrics` 字段，便于未来把外部数据集结果与本目录结果区分开，而不是混用同一份通过率。
