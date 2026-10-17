# P2 宿主结构化输出接入（2026-10-17）

Mio 的 Rust 宿主接口新增默认方法 `Host::output_schema() -> Option<OutputSchema>`。旧宿主默认返回 `None`，不需要修改实现或 `RunOptions` 字面构造；选择 schema 的宿主在一次运行开始时由引擎读取一次。Schema 序列化后超过 32 KiB 会在模型请求前被拒绝。该值进入已有 `mio-agent` 的结构化输出路径，由运行时与 Provider 能力选择实际协议；它约束最终输出格式，不证明内容正确。

本地真实请求回环 `tests/output_schema_sdk.rs` 的 3 项均通过：无工具时请求携带原生 `response_format`；允许 `read` 时先得到真实文件内容和工具响应，下一轮请求携带原生 schema 并返回结构化结果；超限 schema 在请求前失败。测试使用 `Provider::Custom` 与本机回环服务器，不涉及真实账号或联网 Provider；其它方言和合成结果工具分支没有在本轮逐一实测。CLI 一次性运行与 `--mode rpc` 随后也通过 `--output-schema PATH` 接入同一 `Host::output_schema` 路径（`tests/output_schema_cli.rs` 9 项本地回环验证文件读取、符号链接拒绝、32 KiB 引擎上限与原生 `response_format`），但仍无内建代码审查模式。

最终源码质量门禁：`cargo fmt --check`、`cargo check --all-targets --locked`、`cargo test --locked`、`cargo clippy --all-targets --locked -- -D warnings` 全部通过。全量测试 134 组、3245 passed、0 failed、6 既有 ignored；离线 SDK Rust 1.95.0 本地矩阵 27/27、0 失败/跳过。完整日志保存在本机 `/tmp/mio-schema-host-*-final-20261017.log`，SDK 报告在 `target/sdk-output-schema-host-qa/report.json`；本页保留可携带结论，临时与 `target/` 文件不随仓库分发。

前一实现曾把 schema 加为 `RunOptions` 公共字段；虽通过四门禁，却会破坏显式字段构造的宿主源码兼容性，故改为具有默认实现的 `Host` 方法，并在最终代码上重跑门禁。本轮没有提交、推送或发布；此前 `f7873aaf…` 的固定模型 6/6 + 3/3 门槛绑定旧冻结二进制，不能自动移作本轮改动后的真实模型分数。
