# Harbor 适配（待接入）

**状态：未接入。** 本目录目前只有计划，没有任何 Adapter 代码、没有在 Harbor 上跑过的结果，也没有可复现的分数。请勿把本文件当作「已支持公共 benchmark」。

## 为什么需要适配

Harbor 是外部公共评测框架（由 Terminal-Bench 作者维护，用沙箱环境并行跑数据集，公开用法形如 `harbor run --dataset <dataset> --agent <agent> --model <model>`）。它的任务格式、沙箱与判分器都在 Harbor 侧维护，和 `evals/tasks/coding/` 的自有验收探针是两套东西：

- 自有验收：本仓库维护、判定口径自持，回答「这次改动有没有回归」。
- Harbor 数据集：外部维护、跨模型排名，回答「在公共任务分布上表现如何」。

因此适配层负责 **Mio Agent 接入与报告映射**，不改变上层两份报告的语义，也不让两边分数混用。

## 接口计划（尚未实现）

1. **Agent 适配**：按所选 Harbor 版本的扩展接口，在其环境内安装并调用 Mio CLI，传入任务指令并采集运行轨迹。
2. **运行入口**：未来提供运行包装，固定 Harbor 与数据集版本，使用 Harbor 原生任务、环境与官方判分器；不把公共任务转入本地 Python 探针执行器。
3. **报告映射**：把 Harbor 结果写入本仓库报告 schema（`report_version: mio-eval-report-v1`），并保留 `report_type` 区分来源，例如 `public-benchmark` 与 `self-acceptance-probe`。

| 本仓库报告字段 | 计划来源 | 备注 |
| --- | --- | --- |
| `task_set.id` / `task_set.version` | 数据集名与版本 | 以 Harbor 实际输出的数据集标识为准 |
| `results[].task` | 数据集任务标识 | |
| `results[].status` / `failure_class` | Harbor 判分结果 | 需先对齐两边的失败分类，未知一律走 `unknown` |
| `results[].duration_seconds` | Harbor 运行统计 | |
| `results[].agent.exit_code` | Harbor Agent 进程结果 | Harbor 若不暴露则记 `null` |
| `results[].acceptance.exit_code` | 官方判分器结果 | |
| `results[].usage.*` / `usage.*` | Harbor 暴露的用量 | 未暴露或未验证时保持 `null`，不估算 |
| `budget.version` / `budget.model` | 本次运行声明 | 与自有验收使用同一预算版本命名 |

## 接入前置条件（均未验证）

- Harbor 可安装、可离线/在线运行，以及其版本与锁定方式。
- Mio 在 Harbor 中的 Agent 适配方式（如何把 `mio` CLI 暴露给 Harbor）。
- 沙箱/并行 provider 的可用性与配额。
- 数据集许可证与本仓库 `licenses/`、`CREDITS.md` 的登记方式。
- 官方判分器退出码到本仓库 `failure_class` 的逐项对齐。

在上述条件逐项验证并写入测试之前，本仓库不声明 Harbor 适配已完成，也不发布任何 Harbor 分数。
