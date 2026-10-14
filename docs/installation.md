# Mio 安装与分发指南 (Installation Guide)

本文档说明 Mio（`mio-agent`）的各种安装方式、平台支持矩阵、离线安全机制以及自动化发布说明。

> **当前发布状态说明**：
> - Mio 处于发布筹备阶段。GitHub 预编译 Release 资产与 npm 包（`@ravomi/mio` 为拟发布包名）**目前等待首次正式发布上线**，尚未发布云端产物。
> - Windows 平台的 MSVC 归档与 PowerShell 安装逻辑已按标准规范构建，但**在实际 Windows 真机环境中尚未实测**。
> - 在正式标签发布上线前，稳定可用的方式为**通过 Cargo 锁定依赖从源码编译安装**。
> - Node.js 运行环境要求：Node.js `>=18.0.0`。如果使用 Bun（如 `bun add -g @ravomi/mio`），wrapper 脚本解释执行仍需要系统中存在 `node`。

---

## 1. 安装方式概览（等待 Release 标签发布后生效）

### 方式一：独立 Shell 脚本（Linux & macOS）
脚本自动检测架构、经 HTTPS 与严格重定向保护下载对应压缩包与 `SHA256SUMS`，执行精确文件名 64 位 SHA256 哈希校验（拒绝重复与混淆条目），解压根目录二进制并通过专用临时目录原子替换安装至 `$HOME/.local/bin/mio`（无需 `sudo`，退出时信号捕获自动清理临时资源）。

```bash
# 安装最新发布版（待发布后生效）
curl -fsSL https://raw.githubusercontent.com/Ravomi/Mio/main/docs/install.sh | bash

# 安装指定版本
curl -fsSL https://raw.githubusercontent.com/Ravomi/Mio/main/docs/install.sh | MIO_VERSION=v0.1.0 bash
```

### 方式二：PowerShell 脚本（Windows x64 - 等待发布且未实测）
通过 TLS 1.2+ 安全通道下载并校验 SHA256（使用 `-LiteralPath` 规避通配符解析并显式拒绝重复同名条目），解压根目录 `mio.exe` 原子安装至 `$env:USERPROFILE\.mio\bin\mio.exe`，并自动精准维护分号分割的 User 与当前进程 PATH：

```powershell
# Windows PowerShell（等待发布，Windows 未实测）
irm https://raw.githubusercontent.com/Ravomi/Mio/main/docs/install.ps1 | iex
```

### 方式三：包管理器（npm / pnpm / Bun - 等待发布）
npm 包装器 `@ravomi/mio`（拟发布包名）采用**首启懒加载安全下载**策略，完全兼容 `pnpm` 和 `Bun` 默认关闭 `postinstall` 钩子的行为，避免安装期网络死锁，并将二进制安全按版本与平台目标独立缓存在用户私有缓存目录（权限 `0700`，临时构建阶段采用私有独立前缀），避免跨平台混用与并发多进程冲突。

```bash
# 使用 npm 全局安装
npm install -g @ravomi/mio

# 使用 pnpm 全局安装
pnpm add -g @ravomi/mio

# 使用 Bun 全局安装（wrapper 执行需要宿主存在 node）
bun add -g @ravomi/mio
```

首次执行 `mio` 命令时将自动从官方 GitHub Releases 获取对应平台的原生二进制并校验 SHA256；后续调用由 Node 进程直接透传参数、标准输入输出与退出返回码。

---

## 2. 源码编译安装（当前推荐，稳定可用）

准备 Rust 1.95 或更新版本，使用 `--locked` 保证编译依赖与 Rig 依赖完全一致：

```bash
git clone https://github.com/Ravomi/Mio.git
cd Mio
cargo install --path . --locked
mio --help
```

---

## 3. 平台支持矩阵

官方 Release 矩阵配置如下，归档根目录统一直接包含 `mio`（或 `mio.exe`）、`LICENSE`、`CREDITS.md` 与 `licenses/`。Linux x64 与 ARM64 统一基于 Ubuntu 24.04 (glibc 2.39+)，不包含 Alpine Linux 等 musl 环境：

| 操作系统 | 架构 | Target Triple | 状态 / 备注 |
|---|---|---|---|
| **Linux** | x86_64 | `x86_64-unknown-linux-gnu` | ubuntu-24.04 原生构建（glibc 2.39+，不含 Alpine，等待首发） |
| **Linux** | aarch64 (ARM64) | `aarch64-unknown-linux-gnu` | ubuntu-24.04-arm 原生构建（glibc 2.39+，不含 Alpine，无需 cross，等待首发） |
| **macOS** | x86_64 (Intel) | `x86_64-apple-darwin` | macos-15-intel runner 构建（等待首发） |
| **macOS** | aarch64 (Apple Silicon) | `aarch64-apple-darwin` | macos-15 runner 构建（等待首发） |
| **Windows** | x64 (AMD64) | `x86_64-pc-windows-msvc` | windows-latest 构建（等待首发，Windows 未实测） |
| **Windows** | arm64 | - | **未支持**（暂不提供预编译，请使用源码编译） |

每个预编译归档（`.tar.gz` 或 `.zip`）根目录结构：
- `mio` / `mio.exe`（可执行二进制）
- `LICENSE`（MIT 主许可）
- `CREDITS.md`（完整致谢与 Rig 框架声明）
- `licenses/`（第三方依赖许可证副本）

---

## 4. npm 发布说明（独立手动授权）

为确保供应链安全并防止 CI/CD 流程中自动冒用 NPM 凭据，拟发布包 `@ravomi/mio` 不放入无人值守自动工作流，须由维护人员在完成 GitHub Release 后手动进行本地授权发布：

```bash
cd npm
# 1. 确认版本与 Cargo.toml 一致
node -e "console.log('Publishing version:', require('./package.json').version)"

# 2. 登录 NPM（需 2FA 认证）
npm login

# 3. 校验并公开发布
npm publish --access public
```
