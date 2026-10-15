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

本目录**不宣称**：token/成本数据、缓存命中率、跨厂商性能对比、质量领先或任何排行榜位置。原因见 [指标限制](#指标限制)。

## 目录结构

```
evals/
  README.md                     # 本文件
  configs/                      # 可直接使用的运行配置
    mio-local.json              # 默认：target/debug/mio
    pi-example.json             # 示例：跨 Agent 对比
  tasks/coding/                 # 任务集 mio-python-acceptance-v1
    <task-id>/
      task.json                 # 任务元数据（id、task_set、验收脚本与超时）
      instruction.md            # 发给 Agent 的指令
      initial/                  # 初始文件，原样复制进任务工作区
      acceptance.py             # 独立验收脚本，工作区在 sys.path 首位
  adapters/
    harbor/README.md            # 公共 benchmark 适配计划（尚未接入）
```

每个任务目录自包含：初始文件、指令与验收脚本都在目录内，评测器只从磁盘加载，没有内嵌 Python 字符串。新增任务 = 新增一个目录，不需要改 `scripts/agent_eval.py`。

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
```

运行真实 Mio 前需已有 `target/debug/mio`（可用 `cargo build --locked --bin mio` 构建），并配置好模型凭证；这些命令会调用真实模型。离线自测无需构建。

退出码：全部任务通过为 `0`，否则为 `1`；参数错误为 `2`。

`--config` 是可选入口，可覆盖的键为 `label`、`agent_command`、`timeout_seconds`、`output`、`tasks_dir`、`tasks`、`budget`。CLI 显式参数覆盖同名配置键。配置文件里的相对路径（`tasks_dir`、`output`）相对**配置文件所在目录**解析；CLI 路径相对当前工作目录。

请通过环境变量或 Agent 自身的凭证存储配置认证，不要把密钥写入配置或命令。报告会写入 `argv` 模板，但密钥类参数值在落盘前被替换为 `[redacted]`；进程环境不会写入报告。

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

- `version` 命名这套预算口径（当前为 `mio-eval-budget-v1`），报告原样记录；不同版本的结果不可直接混用。
- `model` / `thinking` / `max_model_calls` / `max_total_tokens` 是**声明元数据**：runner 把它们写进报告，但既不注入 Agent 命令，也不测量实际用量。要让它们生效，需要同时把它们写进 `agent_command` 或环境变量。
- 报告会写入 `"enforced_by_runner": false`，避免把声明误读成已生效限额。
- 任务集版本 `task_set` / `task_set_version` 定义在 `task.json`；同一 `tasks_dir` 下必须一致，否则加载即报错。

## 报告格式

报告统一为版本化 JSON，示例配置写到 `target/evals/<label>.json`（`target/` 已被 git 忽略，不会提交）。

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
      "usage": { "total_tokens": null, "cost_usd": null, "source": "unknown" }
    }
  ],
  "usage": { "total_tokens": null, "cost_usd": null, "source": "unknown" },
  "unavailable_metrics": ["input_tokens", "output_tokens", "total_tokens", "cost_usd"],
  "limits": "..."
}
```

每个结果都记录：任务标识、运行状态、失败分类、总耗时、Agent 退出码/是否超时、验收脚本退出码/是否超时、以及逐项 `null` 的 usage。

### 运行状态与失败分类

| `status` | `failure_class` | 含义 |
| --- | --- | --- |
| `passed` | `null` | Agent 退出码 0 且验收脚本通过 |
| `failed` | `agent_exit_nonzero` | Agent 非零退出，验收不再作为通过依据 |
| `timeout` | `agent_timeout` | Agent 超过 `--timeout`，进程组被回收 |
| `failed` | `acceptance_failed` | Agent 正常退出，但验收脚本失败 |
| `failed` | `acceptance_timeout` | 验收脚本超过任务声明的超时 |
| `error` | `environment_error` | 进程无法启动等环境错误 |

失败原因不互相覆盖：即使 Agent 失败，验收脚本仍会独立运行并记录，便于区分「没改对」与「没改」。

## 指标限制

- **只测通过/失败**：任务集是少量确定性 Python 任务，用于回归，不覆盖大规模重构、陌生仓库阅读、跨文件改动。
- **token / cost / cache 明确为 `null`**：runner 只能看到 argv 模板的退出码，拿不到 Provider 用量。缺少真实 usage 时标记 `unknown`，绝不用字节数或时长冒充账单。
- **不构成排名**：不同 `budget.version`、不同任务集版本、不同模型的结果不可比；样本量小，不计算置信区间。
- **日志不落盘**：Agent 与验收日志写在临时目录，随运行结束删除，避免持久化模型输出与凭证。
- **无 OS 沙箱**：任务在临时目录中执行，但 Agent 拥有当前用户权限；本目录不是安全边界。

## 公共 benchmark 适配

`adapters/harbor/README.md` 只描述待接入范围与接口计划，尚未有可用适配。当前报告格式保留 `report_type` 与 `unavailable_metrics` 字段，便于未来把外部数据集结果与本目录结果区分开，而不是混用同一份通过率。
