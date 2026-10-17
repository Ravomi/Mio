# 依赖许可证清单（元数据与文本，非法律审核）

> 本文档收口 TODO「CLI 拆分、SDK 契约与分发」（`REQ-SDK` / `REQ-DISTRIBUTION`）中「依赖闭包与许可证来源可审核」的**清单基础**部分，并明确其边界。
>
> - 本清单只是**元数据与文本清单**：把 `cargo metadata` 解析出的真实依赖闭包里每个包的许可证声明与原始文本逐字节留证。**它不是法律义务审核完成，不是 SPDX 归一化，也不代表分发/署名义务已满足。**
> - 无法证明的内容一律记 `unknown`，**未知不会被标成 verified / 完整**。
> - 采集器只写指定的新证据目录；分发正文由独立打包器生成至 `licenses/dependencies/`。项目原有许可证与版权声明保留。
> - crates.io / npm / GitHub Release 发布未执行、未宣称完成；macOS / Windows 真机未验证。本次快照仅为本机 Linux 环境。

工具与产物：

| 角色 | 路径 |
| --- | --- |
| 采集脚本 | `scripts/collect-licenses.py` |
| 离线单测 | `tests/license_inventory.py` |
| 本文件 | `docs/licenses.md` |
| 公开索引（产物） | `target/license-inventory/index.json` |
| 原始文本副本（产物） | `target/license-inventory/text/` |
| 私有本地报告（产物） | `target/license-inventory/private/local-paths.json` |

`target/` 被 `.gitignore` 忽略；产物不随仓库分发。

全部 feature 的非 dev 依赖正文另打包至 [`licenses/dependencies/`](../licenses/dependencies/index.json)：316 个包、215 份去重原文。Rust 1.95.0 重新解析的清单与当前 `Cargo.lock` 绑定；其中 7 个包使用发布归档中记录的上游提交补充正文，来源 URL、提交及文本 SHA-256 均保存在索引。现有 Rig 等独立声明保持原样。

采集器也识别带授权正文的 `AUTHORS`（如 r-efi）；只有贡献者姓名的文件不会替代许可证证据。完整开发依赖清单仍有缺项，包括未能绑定发布源提交的 difflib 0.4.0；不把非 dev 原始文本清单扩展为开发依赖或法律义务已审核。全 feature 原始采集为 379 个包、366 个包含归档正文、13 个包缺正文；其中 7 个非 dev 缺项已用固定上游提交补齐，其余开发缺项仍未纳入分发包。SPDX 原文保留，不通过改写声明消除未知。

```bash
# 重新采集必须写入新目录，不覆盖旧证据。
RUSTUP_TOOLCHAIN=1.95.0 python3 scripts/collect-licenses.py --all-features --fetch \
  --output target/license-inventory-new --no-private-report

# 分发清单只读验收：原文哈希、文件集合及当前 lockfile 均须匹配。
python3 scripts/bundle-licenses.py --verify
python3 -B tests/license_bundle.py
```

`scripts/bundle-licenses.py` 可从采集索引及人工核对的补充来源索引生成新目录；缺正文、摘要不符、非固定提交 URL 或覆盖已有目录均拒绝。它不联网，也不根据 SPDX 名称猜造授权文本。

---

## 1. 是什么 / 不是什么

| 是 | 不是 |
| --- | --- |
| 用 `cargo metadata --locked --offline` 解析出的**真实 resolve 依赖闭包**（只解析图，不编译） | 手工维护的依赖清单，也不是 manifest 文本的转发 |
| 每个包 manifest **同目录**内 `LICENSE` / `LICENCE` / `COPYING` / `NOTICE` / `UNLICENSE` / `PATENTS` 及常见后缀变体的原始文本副本 | 递归扫描 crate 源码目录，也不收集业务源码 |
| 逐文件 SHA-256，并校验落盘副本与读到的字节一致 | 许可证内容的语义判断、SPDX 归一化、条款适用性解释 |
| 逐包记录元数据 SPDX 声明原文（不改写）、来源类别、闭包归属 | 对 "MIT OR Apache-2.0" 之类的选择/义务做法律结论 |
| 缺失/不支持/裁断的情况给显式 `unknown` 与原因 | 用猜测、常识或上游 README 补全未找到的许可证 |
| 公开索引不含任何本地绝对路径；本地路径只进显式标记的私有报告 | 把本机 cargo cache 路径、凭据或环境变量写进可分发清单 |

## 2. 复现命令

```bash
# 1) 真实依赖闭包（仅解析图，不编译；离线）
cargo metadata --locked --offline --format-version 1

# 2) 采集清单（默认写 target/license-inventory，拒绝覆盖已有证据）
python3 scripts/collect-licenses.py --output target/license-inventory

# 3) 离线单测（真实临时文件，无需网络与 cargo）
python3 -B tests/license_inventory.py
```

可选参数：

| 参数 | 作用 |
| --- | --- |
| `--all-features` | 用全 feature 解析图（仍不编译）。本地 cache 不全时配合 `--fetch` |
| `--features a,b` | 指定 feature 集合解析图 |
| `--fetch` | 先执行 `cargo fetch --locked`（需要网络，仅拉公共 crate），再离线解析 |
| `--online` | 去掉 `--offline`（默认离线） |
| `--no-private-report` | 不写私有本地路径报告 |
| `--max-file-bytes` / `--max-total-bytes` | 覆盖单文件 / 总字节上限 |

`index.json` 的 `scope.cargo_metadata_command`、`scope.cargo_version`、`reproduce` 字段固化了本次快照的确切命令，便于核对。

## 3. 输出布局

```
target/license-inventory/
├── index.json                     # 公开安全：无本地绝对路径
├── text/                          # 原始文本逐字节副本
│   └── 0001-mio-core-0.1.0-LICENSE
└── private/
    └── local-paths.json           # 仅本地：manifest 绝对路径映射，禁止分发
```

文本文件名格式为 `NNNN-包名-版本-原文件名`，`NNNN` 为按 `(name, version, source)` 排序后的稳定序号；包名/版本/文件名会被安全字符过滤，防止路径穿越。同名不同版本的包各自独立成条、独立落盘。

`index.json` 顶层字段：

| 字段 | 含义 |
| --- | --- |
| `schema` | `mio-license-inventory/1` |
| `generated_at` | UTC 时间戳 |
| `tool` | 脚本、Python 版本、`inventory_only: true` |
| `disclaimer` | 与本文一致的非法律审核声明 |
| `scope` | `cargo metadata` 命令、cargo 版本、feature 模式、包/成员/闭包计数 |
| `limits` | 本次生效的上限 |
| `reproduce` | 可复现命令 |
| `totals` | 汇总计数（见第 7 节） |
| `packages` | 逐包记录（见第 4 节） |

`private/local-paths.json` 带 `"private": true` 与禁止发布提示，内容是本机 manifest 绝对路径映射；**不得提交、发布或随包分发**。

## 4. 逐包记录字段

| 字段 | 说明 |
| --- | --- |
| `name` / `version` | 包名与版本 |
| `source_kind` | `registry` / `git` / `path` / `workspace_or_path` / `other`（不含本地路径） |
| `source_id` | cargo 的 source 标识（如 `registry+https://github.com/rust-lang/crates.io-index`） |
| `ship_scope` | `workspace` / `shipping` / `dev_only` / `unreachable`（见下） |
| `license_declared` | manifest 中的 `license` 原文，**逐字记录，不重写** |
| `license_file_declared` | manifest 中的 `license_file` 原文（相对路径） |
| `spdx` | `{status, expression, recognized_tokens, unrecognized_tokens}` |
| `text_status` / `text_unknown_reason` | 文本采集状态与 unknown 原因 |
| `files[]` | 每个候选文本文件的 `name` / `role` / `status` / `reason` / `copied_as` / `bytes` / `sha256` / `copy_hash_match` |

`ship_scope` 由 resolve 边计算，`dev` 边不算 shipping：

- `workspace`：workspace 成员；
- `shipping`：从成员出发、只走非 dev 边可达；
- `dev_only`：只在含 dev 边的闭包里可达；
- `unreachable`：在 `cargo metadata` 的 packages 里但不在 resolve 闭包内（如目标相关可选依赖）。

## 5. 状态语义：未知永不 verified

`text_status`：

| 值 | 含义 |
| --- | --- |
| `collected` | 该包所有候选文本都成功逐字节落盘且校验通过 |
| `missing` | manifest 同目录内找不到任何许可证类文本（`no_license_text_in_manifest_dir`） |
| `unknown` | 存在无法证明的候选，原因取首个：`symlink_not_followed`、`path_escape`、`not_a_regular_file`、`empty_file`、`binary_not_text`、`file_exceeds_limit`、`total_bytes_budget_exhausted`、`too_many_license_files`、`manifest_dir_unreadable`、`stat_failed`、`read_failed` |

只要有一个候选不可证明，**整包记 `unknown`**，即使其余文件已复制；截断与超限不会被当作完整。

`spdx.status`：

| 值 | 含义 |
| --- | --- |
| `declared` | 表达式全部 token 命中本地允许清单，且无 `/` |
| `nonstandard_separator` | 使用 `/` 等非严格 SPDX 写法（如 `MIT/Apache-2.0`、`Unlicense/MIT`、`Apache-2.0 / MIT`） |
| `unrecognized_tokens` | 出现允许清单外的 token，逐一列出供人工复核 |
| `license_ref` | 含 `LicenseRef-...` |
| `missing` | 既无 `license` 也无 `license_file` |

允许清单与 `scripts/check-sdk.py` 保持一致；脚本**不猜测、不映射、不改写**表达式。

## 6. 安全与有界性

默认上限（可通过参数覆盖）：

| 上限 | 默认值 | 触达行为 |
| --- | --- | --- |
| `max_packages` | 2000 | **硬失败**，不做静默截断 |
| `max_files_per_package` | 8 | 该包 `unknown`（`too_many_license_files`） |
| `max_file_bytes` | 1 MiB | 该候选 `unknown`（`file_exceeds_limit`），不复制截断内容 |
| `max_total_bytes` | 64 MiB | 之后的候选 `unknown`（`total_bytes_budget_exhausted`） |
| `max_filename_chars` | 80 | 文件名安全截断 |

拒绝行为：

- 输出目录必须是仓库内路径；输出目录本身或其下任何符号链接组件、仓库外路径一律拒绝；
- 已存在的 `index.json` 直接拒绝再次生成；每个文件用 `O_EXCL` 创建，不覆盖已有证据；
- 许可证文本候选若是符号链接、或 realpath 逃出该包 manifest 目录，记 `unknown` 并拒绝读取；
- 落盘后重新哈希，若与源字节不一致则硬失败（不产生不可信证据）；
- 只读 manifest 同目录的许可证类文件名，不递归、不读 `src/` 业务源码；
- 不读取环境变量中的凭据，也不把环境内容写入任何产物。

## 7. 原始采集快照（2026-10-15，AUTHORS 支持与上游补充前）

环境：Fedora Linux 44（本机），`cargo 1.98.1`，`--locked --offline`，默认 feature 解析图。产物：`target/license-inventory/`。

| 指标 | 数值 |
| --- | --- |
| 依赖包 | 330 |
| workspace 成员 | 7 |
| 非 dev 闭包（含成员） | 268 |
| 仅 dev 可达 | 62 |
| 不可达 | 0 |
| 文本 `collected` | 316（582 个文件，2,857,376 字节） |
| 文本 `missing` | 14 |
| 文本 `unknown` | 0 |
| SPDX 标准表达式 | 316 |
| SPDX 非标准 `/` 写法 | 14 |
| SPDX 缺元数据 / 未识别 token / LicenseRef | 0 / 0 / 0 |

hash 复核：582 个副本全部与 `index.json` 中记录的 SHA-256 一致（脚本内建校验 + 本次独立复核），无不一致。

文本 `missing` 的 14 个包（manifest 同目录无许可证类文本文件；其 SPDX 声明仍逐字记录）：

`difflib 0.4.0`、`gloo-timers 0.4.0`、`jni 0.22.4`、`jni-macros 0.22.4`、`jni-sys-macros 0.4.1`、`opentelemetry 0.32.0`、`opentelemetry-http 0.32.0`、`opentelemetry-otlp 0.32.0`、`opentelemetry-proto 0.32.0`、`opentelemetry_sdk 0.32.1`、`r-efi 5.3.0`、`r-efi 6.0.0`、`rustls-platform-verifier-android 0.2.0`、`valuable 0.1.1`。

SPDX 非标准 `/` 写法的 14 个包（与 `docs/sdk.md` §5.1 的 review 列表一致）：

`core_detect MIT/Apache-2.0`、`fnv Apache-2.0 / MIT`、`futures-timer MIT/Apache-2.0`、`generator MIT/Apache-2.0`、`minicov Apache-2.0/MIT`、`quick-error MIT/Apache-2.0`、`rusty-fork MIT/Apache-2.0`、`same-file Unlicense/MIT`、`scoped-tls MIT/Apache-2.0`、`send_wrapper MIT/Apache-2.0`、`serde_urlencoded MIT/Apache-2.0`、`version_check MIT/Apache-2.0`、`wait-timeout MIT/Apache-2.0`、`walkdir Unlicense/MIT`。

该原始索引保留不覆盖。加入 AUTHORS 支持后，Rust 1.95.0 重采集为 318 个包 / 584 文件、12 个缺正文包；其中默认非 dev 依赖的缺项已通过核对发布归档记录的上游提交补齐，并组成上文分发清单。完整 dev 清单仍有未知项，不据此宣称所有 feature 或所有义务已审核。

## 8. 与 `licenses/`、`CREDITS.md` 及 `check-sdk.py` 的关系

- 本清单**不修改** `licenses/`、`CREDITS.md`、`LICENSE`，也不自动并入任何文本；`licenses/` 中的 Rig / Antigravity / DeepSeek-PoW 等人工声明保持原样。
- Rig 各 fork 的 MIT 原始声明由本工具从 `crates/*/LICENSE` **逐字节复制**，不做改写；`CREDITS.md` 与 `docs/rig-source.md` 的来源与归档 SHA-256 声明继续有效。
- 与 `scripts/check-sdk.py` 的 license 闭包互补：那个门禁只检查**元数据**是否每包都有 `license`/`license_file`（330 包 0 缺失），本清单补上**文本副本、逐文件 SHA-256 与缺失/不支持的显式 unknown**。二者都以"未验证即不声称"为准则。
- `scripts/bundle-licenses.py` 独立生成已审查的非 dev 分发清单；采集器本身不自动修改原有许可证。补充正文的 URL/提交/哈希随分发清单保存，缺证据则拒绝生成。

## 9. 未完成与不宣称

- 未做 SPDX 归一化，未解释 `OR`/`AND`/`WITH` 的义务含义，未做法律审核。
- 未在 macOS / Windows 运行（这些平台的依赖闭包与文本可能不同）。
- 未执行 crates.io / npm / GitHub Release 发布，也不声称发布就绪。
- 本次快照本地 cargo cache 完整，未触发 `cargo fetch --locked`；换机或 cache 不全时需加 `--fetch`（仅拉公共 crate，不编译）。
- 未对全部 dev 依赖、全部 optional feature 或非标准 SPDX 写法作完整审核；缺证据不猜补。
