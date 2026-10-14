# AGENTS.md

## 项目定位与工作约定

- **项目**: `Mio` (`mio-coding-agent`，底层为 `mio-core` 与 `mio-agent`)，从 Ravomi 抽离的无头 Agent 引擎。
- **Git 仓库与分支约定**: 本 repo 唯一上游和 push 目标为 `Ravomi/Mio`，默认分支为 `main`。严格仅使用 GitButler 管理；不得推送到未授权远程。
- **组件完整性**: 保留与尊重 Rig 框架依赖及相关许可证（MIT 主许可，第三方依赖与参考项在 `licenses/` 与 `CREDITS.md` 完整声明）。
- **质量标准**: 提交前必须通过 `cargo fmt --check`、`cargo check --all-targets --locked`、`cargo test --locked` 以及 `cargo clippy --all-targets --locked -- -D warnings`。
