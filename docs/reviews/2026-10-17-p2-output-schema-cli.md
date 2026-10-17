# P2 CLI/RPC 输出 Schema 接入验收（2026-10-17）

`--output-schema PATH` 在普通 CLI 和 `--mode rpc` 启动时读取 JSON Schema，并通过同一个 `CliHost::output_schema` 入口交给引擎。选项默认关闭；相对路径以工作区为基准。原始文件读取上限为 256 KiB，引擎仍在模型请求前执行 32 KiB 的序列化 Schema 上限。缺文件、非普通文件、叶子符号链接、非法 JSON 和非 Schema 形状均返回明确错误。Unix 打开时使用 `O_NOFOLLOW | O_NONBLOCK`，并按打开后的文件句柄复核类型和大小，防止路径检查后被换成特殊文件而阻塞启动。

`tests/output_schema_cli.rs` 的 9 项本地回环通过：普通 CLI 和 RPC 请求带原生 `response_format.json_schema`；无选项时不带该字段；文件错误、超限、独立命令组合及符号链接拒绝发生在模型请求前。`tests/output_schema_sdk.rs` 继续覆盖 Rust 宿主及真实 `read` 工具续轮。

当前工作树实跑通过：`python3 scripts/check-cli-semantics.py`（83 flags、73 entries、无 incomplete）、`cargo fmt --check`、`cargo check --all-targets --locked`、`cargo test --locked`、`cargo clippy --all-targets --locked -- -D warnings`、`git diff --check`。这些是本机工程和模拟 Provider 请求证据；未调用真实 Provider，也未验证全部 Provider 的 Schema 方言。该选项只约束最终输出格式，不构成内建代码审查模式或结论正确性证明。
