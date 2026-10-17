# 2026-10-17 Mio v1 本机 Linux 固定验收

**结论：本机 Linux v1 固定门槛通过。**同一冻结二进制在原 `deepseek-chat`、thinking off、每任务最多 20 次模型调用及 180 秒的配置下，六任务 **6/6**、另三次独立 `retrieve-wire-spec` **3/3**；九次运行的 Agent 与独立产物验收均退出 0。没有把早期诊断或历史成功拼入本批次。尚未提交、推送或发布。

## 冻结范围与改动

- 二进制 SHA-256：`f7873aaf891d42c6276a4942a250b153f322d1b0c1a90516b30e613719cf5957`；原始报告、`PLAN.json`、`source-manifest.json` 和 QA 日志在本机 `target/evals/v1-inline8k-20261017T101526Z/`（`target/` 不随仓库分发）。运行后复核二进制及清单内 1139 个文件，摘要均未变。
- 相对前一冻结版 `3c196d26…`，产品源码仅改 `src/shell.rs`：Shell、插件及 MCP 完整输出外置阈值从 32 KiB 降为 8 KiB。溢出后的约 2 KiB 预览、完整输出引用和分页读取、8 MiB 文件上限保留；新增 8 KiB / 8 KiB+1 边界回归。任务、验收、预算、时限和 Agent 参数未变。
- 固定批次的两份配置仍使用每任务临时工作区及协调目录。请求别名是 `deepseek-chat`；未捕获服务端实际模型版本，不能从别名推断。

| 报告 | 通过数 | Agent / 独立验收 |
| --- | ---: | --- |
| `coding.json` | 3/3 | 均退出 0 / 0 |
| `workflows.json` | 3/3 | 均退出 0 / 0 |
| `wire-1.json`、`wire-2.json`、`wire-3.json` | 3/3 | 均退出 0 / 0 |

五份报告共记录 87 次完成模型调用、input 400108、output 12838、total 412946、cached input 250744 tokens。其他 usage 字段不全，覆盖级别为 `partial`；费用为 `null`，没有推算金额。

## 工程与宿主验收

- `cargo fmt --check`、`cargo check --all-targets --locked`、`cargo test --locked`、`cargo clippy --all-targets --locked -- -D warnings` 均通过。最终全量测试 3242 passed、0 failed、6 既有 ignored；首次全量测试只因旧 32 KiB 字面提示断言失败，更新断言后完整重跑通过。
- 上下文、压缩、调用配对、预算位置与输出分页定向回归 28 项通过；`MIO_REQUIRE_BWRAP=1` 下严格 Linux 执行后端及输出测试 42 项通过，均无失败或跳过。
- 离线 SDK 矩阵 27/27、CLI 参数语义 82 项、架构依赖及站点 6 页检查通过。冻结二进制与真实 Ravomi OSC 7501 解析器本地回环 7/7；该报告同时记录运行前后二进制 SHA 一致。
- 冻结二进制的真实 SIGKILL 恢复 12/12、检查点崩溃 5/5，无跳过。这些是进程崩溃证据，不代表硬件断电验证。

单次 8 KiB 候选诊断通过，但不计入上述 3/3。更早的预算提示时机候选诊断失败，改动已撤销；前一冻结版 5/6、独立长文 2/3 的失败证据见[前轮记录](2026-10-17-v1-live-acceptance.md)。本结果是本机、小规模自有任务的固定验收，不推断统计稳定性、公共 benchmark 排名、macOS/Windows 原生行为或完整 GUI 宿主接入。
