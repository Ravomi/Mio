# AGENTS.md

## 项目定位与工作约定

- **项目**: `Mio` (`mio-coding-agent`，底层为 `mio-core` 与 `mio-agent`)，从 Ravomi 抽离的无头 Agent 引擎。
- **版本管理约定**: 本 repo 唯一上游和 push 目标为 `Ravomi/Mio`，默认远程主线为 `main`。严格仅使用 Jujutsu (`jj`) 管理工作副本、change、bookmark、同步与推送；不得直接执行会改变仓库状态的 `git` 或 GitButler 命令，不得推送到未授权远程。底层 Git backend 仅是 jj 与现有远程兼容的实现细节。
- **组件完整性**: 当前运行时使用 Mio 自维护组件，无外部 Rig 依赖。保留派生源码的原始版权与 MIT 许可声明；来源及第三方许可记录在 `licenses/` 与 `CREDITS.md`。
- **质量标准**: 提交前必须通过 `cargo fmt --check`、`cargo check --all-targets --locked`、`cargo test --locked` 以及 `cargo clippy --all-targets --locked -- -D warnings`。
