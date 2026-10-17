# 阶段二进展与本地验收（2026-10-15）

本记录对应 [TODO](../../TODO.md) 与 [PRD](../../PRD.md)。它证明下列本地结果，不表示整个路线图完成。

## 实现与验证范围

- 统一工具 dispatch 门禁：过滤、只读限制、审批超时、参数与运行上下文绑定；每次审批有独立标识。子代理权限只收窄，动态工具不能冒用内置名称。
- 同一 SharedLimits root 的持久预算：文件锁内预留和结算，重启继承；未知 usage 保守计，SIGKILL 遗留预留不自动退款；损坏和策略放宽时拒绝执行。文件内容与目录项均同步后再继续。
- CLI 按参数、终端、RPC、审批与 Runner 拆成五个模块。`mio` 保留简洁终端对话、补全、历史、`-p`、管道和 JSON/RPC，不输出装饰性面板或进入全屏。
- 会话扫描超限时拒绝猜测最新会话；压缩保留开头与近期摘录并报告损失；补测直接子进程回收。

完整源码与测试清单见当前工作区；验收二进制 SHA-256：

```text
f90e4527eb627cda62720e1d024922214240ed35403cac90a000cff1c15b0680
```

## 实跑结果

环境：Linux x86_64、glibc 2.43、Rust 1.95.0、Python 3.14.8。所有重任务串行执行。

| 命令 | 结果 |
| --- | --- |
| `cargo +1.95.0 fmt --check` | rc=0 |
| `cargo +1.95.0 check --all-targets --locked` | rc=0 |
| `cargo +1.95.0 test --locked` | rc=0；2446 passed / 0 failed / 5 ignored |
| `cargo +1.95.0 clippy --all-targets --locked -- -D warnings` | rc=0 |
| `cargo +1.95.0 build --locked --bin mio` | rc=0 |
| `python3 -B tests/cli_terminal.py target/debug/mio` | 3 项通过 |
| `python3 -B tests/osc7501.py` | 7 项通过 |
| `python3 -B tests/cli_cancellation.py target/debug/mio` | 2 项通过 |
| `python3 -B tests/agent_eval_harness.py` | 38 项通过 |
| `python3 -B tests/agent_workflow_tasks.py` | 12 项通过 |
| `python3 -B scripts/check-site.py` | 6 页面、67 CLI 参数及别名通过 |
| `python3 -B tests/crash_recovery.py --binary target/debug/mio --json` | 10 passed / 0 failed / 0 skipped |

5 项 ignored 包括需真实凭据的测试、文档测试与手动性能探针，不计为通过。原始日志和源码哈希快照位于本机 `/tmp/mio-phase2-qa/final/`，本记录保留命令、结果和范围，避免依赖临时路径理解结论。

## 真实崩溃注入

CLI 与 RPC 分别在请求发出前、真实 Shell 执行中、工具返回后、会话保存前发送 SIGKILL，共八项。额外两项验证带 key 的任务强杀后重新提交。

测试使用 loopback 模型端点和测试专用 LD_PRELOAD 探针。请求前与工具返回后在真实 `connect()` 边界暂停；保存前在会话临时文件 `fsync()` 边界暂停。生产程序没有加入故障注入开关。

- 执行中中断：未记录结果的操作保持 unknown。
- 工具返回后和保存前中断：已记录的操作保持 executed。
- 重启恢复：中断运行可查，不重复执行已有副作用。
- 保存前中断后：用户随后修改的会话名称和产物仍保留。
- key 记录由被杀进程真实生成；再次提交无新增模型请求或工具执行，task_id 和账本保持不变。

该结果只覆盖 Linux/glibc 动态链接和 SIGKILL。测试会清理自身启动的 Shell 进程组，不证明父进程遭 SIGKILL 后操作系统会自动清理所有后代；也不证明硬件断电、跨平台或恶意文件替换竞态。

## 真实 Provider 单次探针

相同二进制通过 `scripts/provider-live.py` 的一次 DeepSeek `deepseek-chat`、thinking off 请求：认证与流式完成成功，Provider 上报 input=64、output=5、total=69、cached_input=0，未上报 cache_creation。报告记录二进制 SHA-256，不保存密钥或原始回复。

这只证明单次握手与 usage，不覆盖其他 Provider、thinking 模式、429、持续压力或线上取消恢复。

## 仍未完成

插件/MCP 路由与门禁的完整端到端证据、统一恢复视图、文件检查点、TOCTOU、MCP 生命周期、大输出外置、事件版本化、完整上下文保真、同条件评测、OS 沙箱、多平台 SDK 分发及产品需求验证仍按 TODO 跟踪。
