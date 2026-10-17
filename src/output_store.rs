//! DES-17 外置输出生命周期与显式离线清理基础。
//!
//! 目标：为 `.mio/outputs/<uuid>.<stream>` 这类似的外置产物提供一份**可复用、有界、
//! 保守**的完成索引与两阶段清理 API，并把它接到生产捕获与引用写入路径上（见下文）。
//!
//! 关键约束（与 `docs/design-v1.md` 的 DES-17 对齐）：
//!
//! - **默认不自动删除**：本模块只提供显式的 `plan_cleanup`（dry-run）与
//!   `confirm_cleanup`（确认）两个阶段，没有任何后台线程或隐式清理。删除只发生在
//!   调用方显式确认某一份计划之后。
//! - **发布登记与失败回滚**：成功发布的外置文件通过 [`OutputStore::register`] 写入
//!   版本化索引；捕获失败时用 [`OutputStore::cancel`]（或
//!   [`CaptureTransaction`] 的 `Drop`）删除刚发布但未提交的文件并回滚索引。
//! - **引用发布意图账本（DES-17）**：[`OutputStore::begin_publication_intent`] 在
//!   **文件/index 已登记、真实 Host 回调之前**写入一条 durable `Pending` 意图；Host
//!   成功后 [`PublicationResolution::Published`] 把意图移交常规 journal/session 引用
//!   扫描，Host `Err` 保持 `PendingUncertain` 并由
//!   [`OutputStore::reconcile_publication_intent`] 显式对账。未决意图精确保护其 typed
//!   引用，保留期不会静默 GC；账本损坏/版本不符/超限 fail-closed。该 API 不在模型
//!   工具面内，只能由宿主显式调用。
//! - **产物租约**：捕获期间持有产物 **shared** `flock`，清理持有产物 **exclusive**
//!   `flock`。清理在存在活动捕获租约时直接拒绝（`Busy`），而不是只靠进程内互斥。
//!   索引与锁位于 [`STATE_DIR`]，与产物目录 [`OUTPUT_DIR`] 分离：`.mio/outputs`
//!   只包含真正的产物文件，直接枚举它的既有调用方不会看到索引或锁文件。非 Unix 平台
//!   没有可用的 `flock` 语义，因此[`CLEANUP_SUPPORTED`] 为 `false` 且
//!   [`OutputStore::begin_cleanup`] 明确返回 [`OutputStoreError::Unsupported`]。
//! - **共同 activity 租约**：除产物租约外，本模块还通过 `super::workspace_activity`
//!   持有一把按工作区定位、位于项目之外的 activity 锁（canonical 工作区原生路径字节 +
//!   euid 的 SHA-256；见 `super::workspace_activity::shared_for_workspace`）。生产 run
//!   （`Workspace::new`）、捕获事务、`guard_existing` 都取 shared；`begin_cleanup` 先取
//!   exclusive activity、再取 exclusive 产物租约，最后才碰索引锁。固定顺序为
//!   `activity → cleanup → index`，且所有获取都是非阻塞，因此不存在环状等待；清理永远
//!   先于扫描取得独占保护。activity 锁不在项目目录里，所以纯问答/未发布输出的工作区
//!   不会因此新增任何 `.mio` 文件。
//! - **引用保护（真实 schema，不是镜像）**：清理前用 `src/session.rs` 的
//!   `crate::session::Session` 与 `src/journal.rs` 的 `crate::journal::JournalFile`
//!   （含 `RunState` / `OpStatus` 真实枚举）解析工作区内有界的 session/journal JSON，
//!   再对**原始已校验的 serde 值**做有界递归扫描：任何字符串（含对象键）里出现
//!   `.mio/outputs/<完整文件名>` 的外置文件一律不删。递归覆盖 `history`、
//!   `pinned_context`、`last_compaction_loss`、`goal`、`target`、`detail` 以及任何
//!   未知/新增字段，所以没有哪个可保存引用的位置会被漏掉。
//! - **证据缺失即拒绝**：解析失败、schema 版本不符、缺少必需字段、run 仍 active、
//!   `Pending`/`Unknown` 副作用、`dropped_runs`/`truncated_operations` 非零、`goal`
//!   被标记截断、`operation`/`target`/`detail` 带 `src/journal.rs::truncate` 的真实
//!   截断标记（保留上限个字符后追加 `...`），以及 `.mio/outputs/` 后面没有合法完整
//!   文件名的可疑文本，都会整体拒绝清理。**被截断的前缀不是完整引用证据**，无法证明
//!   “无引用”时绝不删除。
//! - **符号链接与越界拒绝**：session/journal/outputs 目录项是符号链接、非普通文件、
//!   路径穿越或文件名非法时同样拒绝。
//! - **只删已登记且超过保留期**：磁盘上未登记的旧文件只被列出，永不删除；索引外的
//!   文件、被引用文件、未满 7 天的文件都不进入删除集合。
//! - **有界与保守**：索引条目数、单个索引字节数、扫描文件数与扫描字节数都有上限；
//!   路径穿越、非普通文件、符号链接与内容不匹配都会被拒绝。
//!
//! 本模块（与它的兄弟模块 `src/workspace_activity.rs`）通过 `#[path]` 被
//! `tests/output_cleanup.rs` / `tests/output_lifecycle.rs` 直接编译，它引用 crate 自身的
//! `crate::session` / `crate::journal`，而不是在本文件里镜像一份会漂移的 schema：
//! 测试 crate 在根部写 `use mio_coding_agent::{journal, session};`，于是同一个
//! `crate::` 路径也解析到被测试的库。依赖不变（只用 `serde` / `serde_json` /
//! `sha2` / `tempfile` / `libc`，均为既有依赖），也不做关键词匹配原始字节。
//!
//! **已接入的写入保护**：`src/shell.rs` 的生产捕获路径现在用 [`OutputStore`] 懒初始化
//! 登记产物（同一事务内 stdout/stderr 共享一个 shared 捕获租约，失败或取消时回滚未提交
//! 的文件与索引项），`Session::save` 与 `src/journal.rs` 的 `persist` 在从快照/候选到
//! 落盘整个区间持有 [`OutputStore::guard_existing`] 的 shared 非阻塞租约。生产 run 的
//! `Workspace` 在**任何** run 中都持有同一 shared activity 租约整个 run 存续期（即使
//! `.mio/outputs` 尚不存在，因此「run 构造之后才出现第一次 capture」也在保护期内）。
//! 因此清理的 exclusive 租约与这些引用写入者互斥：清理进行时它们立即以 `Busy` 拒绝，
//! 不会在扫描之后再写入一个不受保护的引用。`externalize_output`（MCP 外置结果）走同一
//! 事务路径。
//!
//! **外部 session 来源**：`Session::save` 在目标不在 `.mio/sessions` / `.mio/journal`
//! 时通过 [`OutputStore::register_external_reference_source`] 登记来源。清理扫描会枚举
//! 工作区内的已登记来源；工作区外的来源不会被读取（不扫描宿主私密文件），而是整体拒绝
//! 清理并把来源写进错误信息。
//!
//! **仍存在的边界（如实记录）**：非协作的外部写入者（例如直接编辑
//! `.mio/sessions/*.json` 的第三方进程）不持有任何 shared 租约，不在协作保证范围内；
//! [`CONCURRENCY_WARNING`] 会把这一点写进每次计划与报告。删除与索引更新仍不是一次原子
//! 事务，中断只会留下可被下一次计划回收的「索引里多出不存在的条目」。

#![allow(dead_code)] // 平台分支变体、显式清理 API 与供后续 run/CLI 接入的片段在当前编译目标中可能未被全部引用。

use super::workspace_activity::{self, ActivityError, ActivityLease};
use crate::journal::{
    JOURNAL_VERSION, JOURNAL_VERSION_V1, JournalFile, MAX_DETAIL_CHARS, MAX_JOURNAL_FILE_SIZE,
    MAX_TARGET_CHARS, OpStatus, RunState, ToolCallPhase,
};
use crate::session::Session;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

/// 工作区内的 Mio 产品目录（相对工作区根）。
pub const PRODUCT_DIR: &str = ".mio";
/// 外置产物目录（相对工作区根）。它只保存产物文件本身，索引与锁不放在此处。
pub const OUTPUT_DIR: &str = ".mio/outputs";
/// 输出索引、锁与引用来源登记表所在的目录（相对工作区根）。
///
/// 与 [`OUTPUT_DIR`] 分离是刻意的：直接枚举 `.mio/outputs` 的调用方（例如 `read` 的
/// byte 分页与 MCP 外置结果）只会看到真正的产物文件。
pub const STATE_DIR: &str = ".mio/output-index";
/// 索引文件名，位于 [`STATE_DIR`] 内。
pub const INDEX_FILE_NAME: &str = "index.json";
/// 捕获/清理共享的跨进程锁文件名，位于 [`STATE_DIR`] 内。
pub const CLEANUP_LOCK_FILE: &str = "cleanup.lock";
/// 索引读改写使用的进程内 + 跨进程锁文件名，位于 [`STATE_DIR`] 内。
pub const INDEX_LOCK_FILE: &str = "index.lock";
/// 外部 session 引用来源登记表的文件名，位于 [`STATE_DIR`] 内。
pub const SOURCE_FILE_NAME: &str = "reference-sources.json";
/// 引用发布意图账本的文件名，位于 [`STATE_DIR`] 内。
pub const PUBLICATION_FILE_NAME: &str = "publication-intents.json";

/// 当前输出索引 schema 版本。
pub const INDEX_VERSION: u32 = 1;

/// 默认保留期：只清理严格超过该时长（7 天）的已完成输出。
pub const DEFAULT_RETENTION_SECS: u64 = 7 * 24 * 60 * 60;

/// 外部引用来源登记表的字节上限。
pub const MAX_SOURCE_BYTES: usize = 256 * 1024;
/// 外部引用来源最多登记的条目数。
pub const MAX_SOURCE_ENTRIES: usize = 256;

/// 索引最多容纳的条目数。
pub const MAX_INDEX_ENTRIES: usize = 4096;
/// 单个索引文件的字节上限。
pub const MAX_INDEX_BYTES: usize = 4 * 1024 * 1024;
/// 引用扫描最多查看的文件/目录项数。
pub const MAX_SCAN_FILES: usize = 20_000;
/// 引用扫描读取的总字节上限。
pub const MAX_SCAN_BYTES: u64 = 256 * 1024 * 1024;
/// 单个外置文件的字节上限，与 `src/shell.rs` 的 `MAX_OUTPUT_FILE_BYTES` 一致。
pub const MAX_OUTPUT_BYTES: u64 = 8 * 1024 * 1024;
/// 单个 session JSON 的字节上限，与 `src/session.rs` 一致。
pub const MAX_SESSION_BYTES: u64 = 16 * 1024 * 1024;
/// 单个 journal JSON 的字节上限，直接取 `src/journal.rs` 的真实常量，避免镜像漂移。
pub const MAX_JOURNAL_BYTES: u64 = MAX_JOURNAL_FILE_SIZE as u64;

/// 文件名单个组件的字符上限。
pub const MAX_NAME_CHARS: usize = 128;
/// stream 名称的字符上限。
pub const MAX_STREAM_CHARS: usize = 32;
/// `call_id` 的字符上限。
pub const MAX_CALL_ID_CHARS: usize = 512;
/// 引用发布意图账本版本。
pub const PUBLICATION_INTENT_VERSION: u32 = 1;
/// 发布意图账本最多容纳的未决条目数。
pub const MAX_PUBLICATION_INTENTS: usize = 256;
/// 发布意图账本的字节上限。
pub const MAX_PUBLICATION_BYTES: usize = 512 * 1024;
/// 发布 attempt id 的字符上限。
pub const MAX_PUBLICATION_ATTEMPT_CHARS: usize = 128;
/// 发布意图记录的真实 run scope 字符上限。
pub const MAX_INTENT_SCOPE_CHARS: usize = 256;

/// 随计划和报告一起返回的并发边界说明。
///
/// 清理持有 activity + cleanup 两把 exclusive 租约；生产 run、捕获事务与引用写入者
/// 持有匹配的 shared 租约。这里如实说明仍存在的边界：非协作的外部写入者不受约束。
pub const CONCURRENCY_WARNING: &str = "output cleanup holds the exclusive workspace activity lease and the exclusive output lease while it scans and deletes; production runs (Workspace), capture transactions, Session::save and journal persistence hold the matching shared lease, so a reference written through those paths cannot appear after the reference scan. A writer that does not take the shared lease (for example an external non-Mio process editing .mio/sessions) is still outside this cooperative guarantee.";

/// 外置文件路径前缀（[`OUTPUT_DIR`] 加一个分隔符）。
const OUTPUT_PREFIX: &str = ".mio/outputs/";
/// session 目录（相对工作区根）；`src/journal.rs` 有一份等价的公开常量，session 没有。
const SESSION_DIR: &str = ".mio/sessions";
/// `src/journal.rs::truncate` 追加的截断标记。
const TRUNCATE_MARKER: &str = "...";
/// session/journal 引用递归扫描的最大嵌套层数（`serde_json` 的解析上限也是 128）。
const MAX_VALUE_DEPTH: usize = 128;
/// `src/journal.rs::begin_run_with_goal` 对 goal 的字符上限（该文件的私有字面量；
/// 权威标记仍是 `RunRecord::goal_truncated`，这里只是保守兜底）。
const JOURNAL_GOAL_CHARS: usize = 4096;

/// 当前平台是否支持显式离线清理（需要 Unix `flock`）。
pub const CLEANUP_SUPPORTED: bool = cfg!(unix);

/// 输出索引操作失败的原因。所有变体都是保守拒绝，不包含“部分成功”。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputStoreError {
    /// 底层文件系统错误（含路径与上下文）。
    Io(String),
    /// 存储内容无法可信解析，或与声明的 schema 不一致。
    Corrupt(String),
    /// 路径/字段非法（含路径穿越、符号链接、非普通文件、非法名称）。
    Invalid(String),
    /// 已登记文件的内容与索引记录不一致，无法安全删除。
    ContentMismatch(String),
    /// 超过条目数 / 字节数 / 扫描预算等有界上限。
    Bounds(String),
    /// 存在活动的捕获租约或其他清理，无法取得独占租约。
    Busy(String),
    /// dry-run 计划与当前索引/引用状态不一致，拒绝用旧计划删除文件。
    StalePlan(String),
    /// 存在仍在进行的 run，引用关系无法确认。
    ActiveRun(String),
    /// 引用证据已经被写入端丢弃或截断（dropped/truncated 计数、未决副作用、截断标记），
    /// 无法证明“无引用”。
    IncompleteEvidence(String),
    /// 清理已经发生，但删除或索引更新只完成了一部分；错误里带着如实记录的部分状态。
    PartialCleanup(Box<CleanupReport>, String),
    /// 当前平台不支持该操作（例如非 Unix 的独占清理租约）。
    Unsupported(String),
}

impl std::fmt::Display for OutputStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(message)
            | Self::Corrupt(message)
            | Self::Invalid(message)
            | Self::ContentMismatch(message)
            | Self::Bounds(message)
            | Self::Busy(message)
            | Self::StalePlan(message)
            | Self::ActiveRun(message)
            | Self::IncompleteEvidence(message)
            | Self::PartialCleanup(_, message)
            | Self::Unsupported(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for OutputStoreError {}

impl From<ActivityError> for OutputStoreError {
    fn from(error: ActivityError) -> Self {
        match error {
            ActivityError::Io(message) => Self::Io(message),
            ActivityError::Invalid(message) => Self::Invalid(message),
            ActivityError::Busy(message) => Self::Busy(message),
            ActivityError::Unsupported(message) => Self::Unsupported(message),
        }
    }
}

/// 一条已发布、已完成的外置输出记录。
///
/// `bytes` 与 `hash` 在成功登记后一定非 `None`（登记时以磁盘内容为准补齐并校验）；
/// `call_id` 允许为 `None`，序列化为 JSON `null`，代表产生该输出的调用 ID 未知。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputEntry {
    /// 工作区相对路径，必须形如 `.mio/outputs/<name>`。
    pub path: String,
    #[serde(default)]
    pub bytes: Option<u64>,
    #[serde(default)]
    pub hash: Option<String>,
    /// 产生该输出的流标识（如 `stdout` / `stderr` / `tool`）。
    pub stream: String,
    /// 发布时间的 Unix 秒；`0` 在登记时被替换为当前时间。
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub call_id: Option<String>,
}

/// 模型可见的结构化外置输出引用（DES-12）。
///
/// 它是 [`OutputEntry`] 的**有界、只读投影**：只在一次捕获真正提交成功后由
/// `src/shell.rs` 依据登记成功的索引条目构造。模型不会看到未经提交的临时文件，也不会
/// 通过解析 `[TRUNCATED: ...]` 文本去猜引用。字段与磁盘内容一一对应：`total_bytes`
/// 与 `sha256` 来自登记时对磁盘内容的真实读数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputReference {
    /// 工作区相对路径，必须形如 `.mio/outputs/<name>`。
    pub path: String,
    /// 外置文件的真实字节数。
    pub total_bytes: u64,
    /// 产生该输出的流标识（如 `stdout` / `stderr` / `tool`）。
    pub stream: String,
    /// 产生该输出的调用 ID；未知时为 JSON `null`，绝不伪造。
    pub call_id: Option<String>,
    /// 外置文件内容的 SHA-256（64 位小写十六进制）。
    pub sha256: String,
}

impl OutputReference {
    /// 由一条已登记成功的索引条目构造引用。
    ///
    /// `bytes` / `hash` 缺失或字段越界时 fail-closed：调用方必须把该错误当作捕获
    /// 失败，而不是发布一个不完整的引用。
    pub fn from_entry(entry: &OutputEntry) -> Result<Self, OutputStoreError> {
        validate_entry_path(&entry.path)?;
        validate_stream(&entry.stream)?;
        let total_bytes = entry.bytes.ok_or_else(|| {
            OutputStoreError::Corrupt(format!(
                "output index entry {} has no byte count; refusing to publish a reference",
                entry.path
            ))
        })?;
        let sha256 = entry.hash.clone().ok_or_else(|| {
            OutputStoreError::Corrupt(format!(
                "output index entry {} has no content hash; refusing to publish a reference",
                entry.path
            ))
        })?;
        let reference = Self {
            path: entry.path.clone(),
            total_bytes,
            stream: entry.stream.clone(),
            call_id: entry.call_id.clone(),
            sha256,
        };
        reference.validate()?;
        Ok(reference)
    }

    /// 提交前重新校验全部有界字段；任何越界都拒绝发布。
    pub fn validate(&self) -> Result<(), OutputStoreError> {
        validate_entry_path(&self.path)?;
        validate_stream(&self.stream)?;
        if !is_sha256_hex(&self.sha256) {
            return Err(OutputStoreError::Corrupt(format!(
                "output reference {} has a malformed sha256",
                self.path
            )));
        }
        if self.total_bytes > MAX_OUTPUT_BYTES {
            return Err(OutputStoreError::Bounds(format!(
                "output reference {} reports {} bytes, above the {MAX_OUTPUT_BYTES} limit",
                self.path, self.total_bytes
            )));
        }
        if let Some(call_id) = &self.call_id
            && call_id.chars().count() > MAX_CALL_ID_CHARS
        {
            return Err(OutputStoreError::Corrupt(format!(
                "output reference {} has an oversized call_id",
                self.path
            )));
        }
        Ok(())
    }
}

/// 版本化的外置输出索引。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputIndex {
    pub version: u32,
    #[serde(default)]
    pub entries: Vec<OutputEntry>,
}

impl OutputIndex {
    /// 空索引。
    pub fn empty() -> Self {
        Self {
            version: INDEX_VERSION,
            entries: Vec::new(),
        }
    }

    fn validate(&self) -> Result<(), OutputStoreError> {
        if self.version != INDEX_VERSION {
            return Err(OutputStoreError::Corrupt(format!(
                "unsupported output index version {} (expected {INDEX_VERSION})",
                self.version
            )));
        }
        if self.entries.len() > MAX_INDEX_ENTRIES {
            return Err(OutputStoreError::Bounds(format!(
                "output index holds {} entries, above the {MAX_INDEX_ENTRIES} limit",
                self.entries.len()
            )));
        }
        let mut seen = BTreeSet::new();
        for entry in &self.entries {
            validate_entry_path(&entry.path)?;
            validate_stream(&entry.stream)?;
            if let Some(hash) = &entry.hash
                && !is_sha256_hex(hash)
            {
                return Err(OutputStoreError::Corrupt(format!(
                    "output index entry {} has a malformed hash",
                    entry.path
                )));
            }
            if let Some(call_id) = &entry.call_id
                && call_id.chars().count() > MAX_CALL_ID_CHARS
            {
                return Err(OutputStoreError::Corrupt(format!(
                    "output index entry {} has an oversized call_id",
                    entry.path
                )));
            }
            if !seen.insert(entry.path.clone()) {
                return Err(OutputStoreError::Corrupt(format!(
                    "output index contains duplicate entry {}",
                    entry.path
                )));
            }
        }
        Ok(())
    }
}

/// 一个在标准扫描目录之外保存、但可能持有外置输出引用的 session 来源。
///
/// `path` 是登记时的规范化绝对路径。存在于登记表中就表示：要么它仍然持有引用，要么
/// 已经无法证明它不持有引用；两种情况下清理都不应假装引用已经枚举完全。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceSource {
    /// 规范化绝对路径。
    pub path: String,
    /// 登记时间的 Unix 秒。
    #[serde(default)]
    pub registered: u64,
}

/// 版本化的外部引用来源登记表。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceSourceIndex {
    pub version: u32,
    #[serde(default)]
    pub sources: Vec<ReferenceSource>,
}

impl ReferenceSourceIndex {
    fn empty() -> Self {
        Self {
            version: INDEX_VERSION,
            sources: Vec::new(),
        }
    }

    fn validate(&self) -> Result<(), OutputStoreError> {
        if self.version != INDEX_VERSION {
            return Err(OutputStoreError::Corrupt(format!(
                "unsupported reference source index version {} (expected {INDEX_VERSION})",
                self.version
            )));
        }
        if self.sources.len() > MAX_SOURCE_ENTRIES {
            return Err(OutputStoreError::Bounds(format!(
                "reference source index holds {} entries, above the {MAX_SOURCE_ENTRIES} limit",
                self.sources.len()
            )));
        }
        let mut seen = BTreeSet::new();
        for source in &self.sources {
            if source.path.trim().is_empty() || !Path::new(&source.path).is_absolute() {
                return Err(OutputStoreError::Corrupt(format!(
                    "reference source '{}' must be an absolute path",
                    source.path
                )));
            }
            if !seen.insert(source.path.clone()) {
                return Err(OutputStoreError::Corrupt(format!(
                    "reference source index contains a duplicate entry {}",
                    source.path
                )));
            }
        }
        Ok(())
    }
}

/// 一次真实本地引用发布的意图状态。
///
/// 只有 `Pending` 与 `PendingUncertain` 会持久化在账本里；`Published`/`Unpublished`
/// 是 Host 对账给出的终局判定，判定成功后账本条目被移交或注销，不会永久膨胀。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationIntentState {
    /// 文件/index 已登记，但 Host 回调尚未返回：发布结果未知，引用必须保护。
    Pending,
    /// Host 回调返回了 `Err`，无法判断是否已部分持久化：保守保持不确定并保护引用，
    /// 直到显式 Host 对账。
    PendingUncertain,
}

/// Host 对一次引用发布意图的显式对账结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationResolution {
    /// Host 已把所有引用 durable 持久化（`Committed`）：注销意图，交给常规
    /// journal/session 引用扫描继续保护。
    Published,
    /// Host 明确证明没有持久化任何引用（`Abandoned`）：注销意图，未发布的新文件按
    /// 既有保留期回收。
    Unpublished,
}

/// 一条本地引用发布意图（DES-17）。
///
/// `attempt_id` 是本地生成的发布尝试身份，独立于 run/tool/approval ID；`run_id` 与
/// `call_id` 记录真实 run scope 与 Provider 调用 ID；`references` 只保存 typed 引用
/// 元数据（路径/长度/SHA-256/stream/call_id），**绝不保存正文或工具参数**。
/// `snapshot` 绑定该意图的完整身份，Host 对账必须回传同一 `snapshot`，旧快照无法
/// 解决新意图。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationIntent {
    pub attempt_id: String,
    pub run_id: String,
    #[serde(default)]
    pub call_id: Option<String>,
    pub state: PublicationIntentState,
    pub created: u64,
    #[serde(default)]
    pub references: Vec<OutputReference>,
    pub snapshot: String,
}

impl PublicationIntent {
    /// 校验全部有界字段与身份绑定；任何不一致都 fail-closed。
    pub fn validate(&self) -> Result<(), OutputStoreError> {
        validate_attempt_id(&self.attempt_id)?;
        if self.run_id.is_empty() || self.run_id.chars().count() > MAX_INTENT_SCOPE_CHARS {
            return Err(OutputStoreError::Invalid(
                "publication intent run scope must be a bounded non-empty string".into(),
            ));
        }
        if let Some(call_id) = &self.call_id
            && (call_id.is_empty() || call_id.chars().count() > MAX_CALL_ID_CHARS)
        {
            return Err(OutputStoreError::Invalid(
                "publication intent call_id must be a bounded non-empty string".into(),
            ));
        }
        if self.references.is_empty() {
            return Err(OutputStoreError::Invalid(
                "publication intent must reference at least one externalized file".into(),
            ));
        }
        for reference in &self.references {
            reference.validate()?;
        }
        if !is_sha256_hex(&self.snapshot) {
            return Err(OutputStoreError::Corrupt(
                "publication intent snapshot is malformed".into(),
            ));
        }
        if publication_snapshot(self) != self.snapshot {
            return Err(OutputStoreError::Corrupt(
                "publication intent snapshot does not match its recorded identity".into(),
            ));
        }
        Ok(())
    }
}

/// 版本化的引用发布意图账本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationIntentIndex {
    pub version: u32,
    #[serde(default)]
    pub intents: Vec<PublicationIntent>,
}

impl PublicationIntentIndex {
    fn empty() -> Self {
        Self {
            version: PUBLICATION_INTENT_VERSION,
            intents: Vec::new(),
        }
    }

    fn validate(&self) -> Result<(), OutputStoreError> {
        if self.version != PUBLICATION_INTENT_VERSION {
            return Err(OutputStoreError::Corrupt(format!(
                "unsupported publication intent index version {} (expected {PUBLICATION_INTENT_VERSION})",
                self.version
            )));
        }
        if self.intents.len() > MAX_PUBLICATION_INTENTS {
            return Err(OutputStoreError::Bounds(format!(
                "publication intent index holds {} entries, above the {MAX_PUBLICATION_INTENTS} limit",
                self.intents.len()
            )));
        }
        let mut seen = BTreeSet::new();
        for intent in &self.intents {
            intent.validate()?;
            if !seen.insert(intent.attempt_id.clone()) {
                return Err(OutputStoreError::Corrupt(format!(
                    "publication intent index contains a duplicate attempt {}",
                    intent.attempt_id
                )));
            }
        }
        Ok(())
    }
}

/// 持有 shared activity + shared cleanup 租约的守卫；`Drop`（关闭文件描述符）即释放。
///
/// 它服务两类调用方：捕获事务（`begin_capture`）与引用写入者（`guard_existing`，即
/// `Session::save` / journal 持久化）。两者都让清理无法取得 exclusive activity 租约。
/// 生产 run 的整段生命周期现在直接持有 `workspace_activity::shared_for_workspace`，
/// 不再经由本类型。
///
/// 在非 Unix 平台上不调用 `flock`，但清理本身也不可用，因此不会出现无保护的清理。
#[must_use = "dropping the lease releases the shared capture lock"]
pub struct CaptureLease {
    dir: PathBuf,
    _activity: ActivityLease,
    _file: File,
}

impl CaptureLease {
    /// 该租约所属的输出状态目录（`.mio/output-index`），便于调用方核对。
    pub fn directory(&self) -> &Path {
        &self.dir
    }

    fn acquire(root: &Path, dir: &Path) -> Result<Self, OutputStoreError> {
        // 固定顺序：先 activity，再 cleanup。两者都是非阻塞获取。
        // activity 是按工作区定位的项目外唯一锁（与 run、清理、引用写入者同一把）。
        let activity = workspace_activity::shared_for_workspace(root)?.ok_or_else(|| {
            OutputStoreError::Busy(
                "an output cleanup holds the exclusive workspace activity lease".into(),
            )
        })?;
        let file = open_lock_file(&dir.join(CLEANUP_LOCK_FILE))?;
        #[cfg(unix)]
        flock_shared(&file)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            _activity: activity,
            _file: file,
        })
    }
}

/// 持有 exclusive activity + exclusive cleanup 租约的守卫；`Drop` 即释放。
///
/// 只在 Unix 上可构造；非 Unix 平台由 [`OutputStore::begin_cleanup`] 返回
/// [`OutputStoreError::Unsupported`]。
#[must_use = "dropping the lease releases the exclusive cleanup lock"]
pub struct CleanupLease {
    dir: PathBuf,
    _activity: ActivityLease,
    _file: File,
}

impl CleanupLease {
    /// 该租约所属的输出状态目录（`.mio/output-index`）。
    pub fn directory(&self) -> &Path {
        &self.dir
    }
}

/// dry-run 清理计划。`digest` 绑定生成时的规范工作区、保留期、完整索引与候选集合；
/// 确认阶段必须重新计算一致，否则拒绝（包括把计划拿到另一个工作区使用）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupPlan {
    pub version: u32,
    /// 生成计划时工作区根（已规范化）的绝对路径。缺失该字段的旧计划无法反序列化，
    /// 不会被当成有效计划接受。
    pub workspace: String,
    pub retention_secs: u64,
    pub generated: u64,
    /// 生成时的索引快照 + 候选路径的 SHA-256，用于拒绝陈旧或跨工作区的计划。
    pub digest: String,
    /// 计划删除的已登记、无引用且超过保留期的输出。
    pub candidates: Vec<OutputEntry>,
    /// 因仍被 session/journal 引用而保留的输出。
    pub retained_referenced: Vec<String>,
    /// 因未满保留期而保留的输出。
    pub retained_recent: Vec<String>,
    /// 索引中存在但磁盘文件已缺失的条目（确认时会从索引中清理）。
    pub pruned_missing: Vec<String>,
    /// 磁盘上存在但未登记的输出（只列出，永不删除）。
    pub unregistered: Vec<String>,
    pub warnings: Vec<String>,
}

/// 确认阶段无法删除的一个已登记输出。
///
/// 失败时索引仍保留该条目，下一次 `plan_cleanup` 会重新把它列为候选，因此在修复原因
/// （权限、占用等）后可以继续清理。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupFailure {
    pub path: String,
    pub message: String,
}

/// 清理确认结果。
///
/// `removed` 只包含本次真正从磁盘删掉的文件；`pruned_missing` 额外包含“校验之后已经
/// 不在磁盘上”的条目。`failed` 与 `warnings` 如实记录部分状态与并发边界。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupReport {
    pub removed: Vec<OutputEntry>,
    pub freed_bytes: u64,
    pub retained_referenced: Vec<String>,
    pub retained_recent: Vec<String>,
    pub pruned_missing: Vec<String>,
    pub unregistered: Vec<String>,
    /// 本次无法删除的已登记输出（索引条目被保留以便重试）。
    #[serde(default)]
    pub failed: Vec<CleanupFailure>,
    /// 本次清理需要注意的事项（含并发边界与部分状态说明）。
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// 外置输出索引与显式清理入口。
#[derive(Debug, Clone)]
pub struct OutputStore {
    root: PathBuf,
}

impl OutputStore {
    /// 打开某个工作区的外置输出存储，确保 `.mio`、`.mio/outputs` 与 `.mio/output-index`
    /// 是真实的 0700 目录。
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, OutputStoreError> {
        let root = root.into();
        let metadata = fs::symlink_metadata(&root).map_err(io_error)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(OutputStoreError::Invalid(
                "workspace root must be a real directory, not a symlink".into(),
            ));
        }
        let store = Self { root };
        store.ensure_output_dir()?;
        store.ensure_state_dir()?;
        Ok(store)
    }

    /// 以只读恢复模式打开工作区：只验证工作区根是真实目录，**不创建 `.mio` 或任何
    /// 文件/目录**。
    ///
    /// 供跨进程崩溃后枚举未决发布意图；搭配的读取 API 本身也不写盘。
    pub fn open_readonly(root: impl Into<PathBuf>) -> Result<Self, OutputStoreError> {
        let root = root.into();
        let metadata = fs::symlink_metadata(&root).map_err(io_error)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(OutputStoreError::Invalid(
                "workspace root must be a real directory, not a symlink".into(),
            ));
        }
        Ok(Self { root })
    }

    /// 工作区根。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 工作区根的规范化绝对路径。
    ///
    /// 计划摘要绑定它，因此两个内容完全相同但位于不同目录的工作区会得到不同摘要，
    /// 某个工作区的计划不可能被另一个工作区复用。
    pub fn canonical_root(&self) -> Result<PathBuf, OutputStoreError> {
        self.root.canonicalize().map_err(|error| {
            OutputStoreError::Io(format!(
                "cannot canonicalize workspace root {}: {error}",
                self.root.display()
            ))
        })
    }

    /// 外置输出目录的绝对路径。
    pub fn output_dir(&self) -> PathBuf {
        self.root.join(OUTPUT_DIR)
    }

    /// 输出状态目录（索引、三把锁与来源登记表）的绝对路径。
    pub fn state_dir(&self) -> PathBuf {
        self.root.join(STATE_DIR)
    }

    /// 索引文件的绝对路径。
    pub fn index_path(&self) -> PathBuf {
        self.state_dir().join(INDEX_FILE_NAME)
    }

    /// 仅在产物目录已经存在时，持有一份 shared、非阻塞的活动+产物租约。
    ///
    /// 供生产 run（`Workspace::new`）与 `Session::save` / `journal::persist` 这类同步
    /// 引用写入者在整个存续/落盘区间使用。activity 租约按工作区定位在项目外（见
    /// `super::workspace_activity::shared_for_workspace`），产物租约仍是本模块状态目录内
    /// 的 `cleanup.lock`；两者按固定顺序 activity → cleanup 非阻塞获取：
    ///
    /// - `.mio` 或 `.mio/outputs` 不存在时直接返回 `Ok(None)`，**不创建 `.mio`**，因此
    ///   从未发生捕获的工作区仍保持零磁盘写入。
    /// - 产物目录存在时创建状态目录并按固定顺序（activity → cleanup）打开两把锁，
    ///   以 `LOCK_SH | LOCK_NB` 立即尝试；清理持有 exclusive 租约时返回
    ///   [`OutputStoreError::Busy`]，不会阻塞同步 Host 回调。
    /// - 工作区根、`.mio` 或 `.mio/outputs` 是符号链接/非目录时 fail-closed 返回
    ///   [`OutputStoreError::Invalid`]。
    pub fn guard_existing(
        root: impl Into<PathBuf>,
    ) -> Result<Option<CaptureLease>, OutputStoreError> {
        let root = root.into();
        let metadata = fs::symlink_metadata(&root).map_err(|error| {
            OutputStoreError::Io(format!(
                "cannot access workspace root {}: {error}",
                root.display()
            ))
        })?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(OutputStoreError::Invalid(
                "workspace root must be a real directory, not a symlink".into(),
            ));
        }
        let product = root.join(PRODUCT_DIR);
        match fs::symlink_metadata(&product) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error(error)),
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(OutputStoreError::Invalid(
                    ".mio must be a real directory, not a symlink".into(),
                ));
            }
        }
        let outputs = root.join(OUTPUT_DIR);
        match fs::symlink_metadata(&outputs) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error(error)),
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(OutputStoreError::Invalid(
                    ".mio/outputs must be a real directory, not a symlink".into(),
                ));
            }
        }
        let store = Self { root };
        store.ensure_state_dir()?;
        let directory = store.state_dir();
        // 与清理、捕获、run 相同的固定顺序：activity（按工作区定位）→ cleanup。
        Ok(Some(CaptureLease::acquire(&store.root, &directory)?))
    }

    // ---------------------------------------------------------------- 捕获

    /// 取得 shared 捕获租约。清理无法在它存活期间取得 exclusive 租约。
    pub fn begin_capture(&self) -> Result<CaptureLease, OutputStoreError> {
        self.ensure_output_dir()?;
        self.ensure_state_dir()?;
        CaptureLease::acquire(&self.root, &self.state_dir())
    }

    /// 登记一个已成功发布的外置文件。以磁盘内容为准补齐并校验 `bytes` / `hash`。
    ///
    /// 仅当调用方持有本 store 的 [`CaptureLease`] 时才接受登记，避免绕过捕获租约。
    pub fn register(
        &self,
        lease: &CaptureLease,
        mut entry: OutputEntry,
    ) -> Result<OutputEntry, OutputStoreError> {
        self.check_capture_lease(lease)?;
        let name = validate_entry_path(&entry.path)?;
        validate_stream(&entry.stream)?;
        if let Some(call_id) = &entry.call_id
            && call_id.chars().count() > MAX_CALL_ID_CHARS
        {
            return Err(OutputStoreError::Invalid(
                "call_id exceeds the output index limit".into(),
            ));
        }
        let path = self.resolve_name(&name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(OutputStoreError::Invalid(format!(
                    "{} is not a published file",
                    entry.path
                )));
            }
            Err(error) => return Err(io_error(error)),
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(OutputStoreError::Invalid(format!(
                "{} must be a regular file, not a symlink",
                entry.path
            )));
        }
        let (bytes, hash) = read_and_hash(&path)?;
        if entry.bytes.is_some_and(|declared| declared != bytes) {
            return Err(OutputStoreError::ContentMismatch(format!(
                "{} is {bytes} bytes on disk but {} was declared",
                entry.path,
                entry.bytes.unwrap_or_default()
            )));
        }
        if let Some(declared) = &entry.hash
            && declared != &hash
        {
            return Err(OutputStoreError::ContentMismatch(format!(
                "{} content hash does not match the declared hash",
                entry.path
            )));
        }
        entry.bytes = Some(bytes);
        entry.hash = Some(hash);
        if entry.created == 0 {
            entry.created = now_secs();
        }

        let _guard = self.lock_index()?;
        let mut index = self.load_index()?;
        if index
            .entries
            .iter()
            .any(|existing| existing.path == entry.path)
        {
            return Err(OutputStoreError::Invalid(format!(
                "{} is already registered",
                entry.path
            )));
        }
        index.entries.push(entry.clone());
        self.write_index_locked(&index)?;
        Ok(entry)
    }

    /// 取消一次已发布（但未提交）的输出：校验内容一致后删除文件并回滚索引条目。
    ///
    /// 返回 `Ok(false)` 表示该路径本来就不在索引中。内容不一致时拒绝删除。
    pub fn cancel(&self, lease: &CaptureLease, path: &str) -> Result<bool, OutputStoreError> {
        self.check_capture_lease(lease)?;
        let name = validate_entry_path(path)?;
        let _guard = self.lock_index()?;
        let mut index = self.load_index()?;
        let Some(position) = index.entries.iter().position(|entry| entry.path == path) else {
            return Ok(false);
        };
        let entry = index.entries[position].clone();
        let file_path = self.resolve_name(&name);
        match fs::symlink_metadata(&file_path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                let (bytes, hash) = read_and_hash(&file_path)?;
                if entry.bytes.is_some_and(|declared| declared != bytes)
                    || entry
                        .hash
                        .as_deref()
                        .is_some_and(|declared| declared != hash)
                {
                    return Err(OutputStoreError::ContentMismatch(format!(
                        "{path} changed after registration; refusing rollback"
                    )));
                }
                fs::remove_file(&file_path).map_err(io_error)?;
            }
            Ok(_) => {
                return Err(OutputStoreError::Invalid(format!(
                    "{path} must be a regular file, not a symlink"
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(error)),
        }
        index.entries.remove(position);
        self.write_index_locked(&index)?;
        Ok(true)
    }

    /// 证明一个已登记路径在磁盘上**确实是**普通（非符号链接）文件，且长度与
    /// SHA-256 与声明完全一致，必须在存活捕获租约下调用。
    ///
    /// 索引与内存缓存都只是“声称的字节”，不能证明模型引用指向的正文真实存在。
    /// 文件缺失、是符号链接/非普通文件、长度不符或内容 hash 不符时一律 fail-closed，
    /// 调用方不得据此发布陈旧引用。
    pub fn verify_published_file(
        &self,
        lease: &CaptureLease,
        path: &str,
        expected_bytes: u64,
        expected_sha256: &str,
    ) -> Result<(), OutputStoreError> {
        self.check_capture_lease(lease)?;
        let name = validate_entry_path(path)?;
        if !is_sha256_hex(expected_sha256) {
            return Err(OutputStoreError::Corrupt(format!(
                "expected content hash for {path} is malformed"
            )));
        }
        if expected_bytes > MAX_OUTPUT_BYTES {
            return Err(OutputStoreError::Bounds(format!(
                "{path} declares {expected_bytes} bytes, above the {MAX_OUTPUT_BYTES} limit"
            )));
        }
        let file_path = self.resolve_name(&name);
        let metadata = match fs::symlink_metadata(&file_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(OutputStoreError::Invalid(format!(
                    "{path} is missing on disk; refusing to publish a stale output reference"
                )));
            }
            Err(error) => return Err(io_error(error)),
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(OutputStoreError::Invalid(format!(
                "{path} must be a regular file, not a symlink"
            )));
        }
        if metadata.len() != expected_bytes {
            return Err(OutputStoreError::ContentMismatch(format!(
                "{path} is {} bytes on disk but {expected_bytes} were expected",
                metadata.len()
            )));
        }
        let (bytes, hash) = read_and_hash(&file_path)?;
        if bytes != expected_bytes || hash != expected_sha256 {
            return Err(OutputStoreError::ContentMismatch(format!(
                "{path} content does not match the declared hash; refusing to publish a stale output reference"
            )));
        }
        Ok(())
    }

    /// 删除一个**本次事务新建、但从未登记成功**的文件，用于回滚。
    ///
    /// 返回 `Ok(false)` 表示该路径已经在索引里（另一个写入者拥有它，本事务不得删除），
    /// 或文件已不存在。内容与 `expected_sha256` 不符、是符号链接或非普通文件时拒绝
    /// 删除：绝不覆盖或删除来路不明的既有文件。
    pub fn discard_fresh(
        &self,
        lease: &CaptureLease,
        path: &str,
        expected_sha256: &str,
    ) -> Result<bool, OutputStoreError> {
        self.check_capture_lease(lease)?;
        let name = validate_entry_path(path)?;
        if !is_sha256_hex(expected_sha256) {
            return Err(OutputStoreError::Corrupt(format!(
                "expected content hash for {path} is malformed"
            )));
        }
        let _guard = self.lock_index()?;
        let index = self.load_index()?;
        if index.entries.iter().any(|entry| entry.path == path) {
            return Ok(false);
        }
        let file_path = self.resolve_name(&name);
        match fs::symlink_metadata(&file_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(io_error(error)),
            Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
                Err(OutputStoreError::Invalid(format!(
                    "{path} must be a regular file, not a symlink; refusing to remove an unknown file"
                )))
            }
            Ok(_) => {
                let (_bytes, hash) = read_and_hash(&file_path)?;
                if hash != expected_sha256 {
                    return Err(OutputStoreError::ContentMismatch(format!(
                        "{path} content does not match this transaction's fresh file; refusing to remove it"
                    )));
                }
                fs::remove_file(&file_path).map_err(io_error)?;
                Ok(true)
            }
        }
    }

    // ---------------------------------------------------------------- 索引

    /// 读取并校验索引；索引缺失视为空索引。
    pub fn load_index(&self) -> Result<OutputIndex, OutputStoreError> {
        let path = self.index_path();
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(OutputIndex::empty());
            }
            Err(error) => return Err(io_error(error)),
            Ok(metadata) => {
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    return Err(OutputStoreError::Invalid(
                        "output index must be a regular file, not a symlink".into(),
                    ));
                }
                if metadata.len() > MAX_INDEX_BYTES as u64 {
                    return Err(OutputStoreError::Corrupt(format!(
                        "output index is {} bytes, above the {MAX_INDEX_BYTES} limit",
                        metadata.len()
                    )));
                }
            }
        }
        let bytes = fs::read(&path).map_err(io_error)?;
        if bytes.len() > MAX_INDEX_BYTES {
            return Err(OutputStoreError::Corrupt(format!(
                "output index is {} bytes, above the {MAX_INDEX_BYTES} limit",
                bytes.len()
            )));
        }
        let index: OutputIndex = serde_json::from_slice(&bytes).map_err(|error| {
            OutputStoreError::Corrupt(format!("corrupt output index {}: {error}", path.display()))
        })?;
        index.validate()?;
        Ok(index)
    }

    // ------------------------------------------------------------ 引用来源

    /// 外部 session 来源登记表的绝对路径。
    pub fn source_index_path(&self) -> PathBuf {
        self.state_dir().join(SOURCE_FILE_NAME)
    }

    /// 登记一个位于标准扫描目录之外、可能持有外置输出引用的 session 来源。
    ///
    /// 返回 `Ok(false)` 表示目标就在 `.mio/sessions` / `.mio/journal` 标准扫描目录内，
    /// 无需单独登记。登记表有界、版本化、原子替换；损坏时 fail-closed。
    ///
    /// 要求目标路径已经存在（会 `canonicalize` 它）。发布之前就要登记的调用方应改用
    /// [`OutputStore::register_pending_reference_source`]。
    pub fn register_external_reference_source(
        &self,
        path: &Path,
    ) -> Result<bool, OutputStoreError> {
        let canonical = path.canonicalize().map_err(|error| {
            OutputStoreError::Io(format!(
                "cannot canonicalize reference source {}: {error}",
                path.display()
            ))
        })?;
        self.register_canonical_reference_source(&canonical)
    }

    /// 登记一个**尚未落盘、但即将发布**的 session 引用来源。
    ///
    /// 与 [`OutputStore::register_external_reference_source`] 相同，只是允许目标文件还
    /// 不存在：用「规范化后的父目录 + 文件名」得到与发布后 `canonicalize` 一致的路径。
    /// 因此 `Session::save` 可以在覆盖上一个快照之前完成登记，登记失败（例如登记表已满）
    /// 时不会发布一个引用无法枚举的半更新会话。
    ///
    /// 若登记之后发布本身失败，登记表会保留目标路径；清理扫描对不存在的来源 fail-closed，
    /// 即多保留、不误删，重试保存成功后会自愈。
    pub fn register_pending_reference_source(&self, path: &Path) -> Result<bool, OutputStoreError> {
        let file_name = path.file_name().ok_or_else(|| {
            OutputStoreError::Invalid(format!(
                "reference source {} has no file name",
                path.display()
            ))
        })?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let canonical_parent = parent.canonicalize().map_err(|error| {
            OutputStoreError::Io(format!(
                "cannot canonicalize reference source directory {}: {error}",
                parent.display()
            ))
        })?;
        self.register_canonical_reference_source(&canonical_parent.join(file_name))
    }

    /// 已规范化引用来源的共同登记路径。
    fn register_canonical_reference_source(
        &self,
        canonical: &Path,
    ) -> Result<bool, OutputStoreError> {
        if self.is_standard_reference_path(canonical)? {
            return Ok(false);
        }
        let _guard = self.lock_index()?;
        let mut index = self.load_sources()?;
        let text = canonical.to_string_lossy().into_owned();
        if index.sources.iter().any(|source| source.path == text) {
            return Ok(true);
        }
        if index.sources.len() >= MAX_SOURCE_ENTRIES {
            return Err(OutputStoreError::Bounds(format!(
                "reference source index already holds {MAX_SOURCE_ENTRIES} entries; move external sessions under .mio/sessions or remove {}",
                self.source_index_path().display()
            )));
        }
        index.sources.push(ReferenceSource {
            path: text,
            registered: now_secs(),
        });
        self.write_sources_locked(&index)?;
        Ok(true)
    }

    /// 读取并校验外部来源登记表；缺失视为空。
    fn load_sources(&self) -> Result<ReferenceSourceIndex, OutputStoreError> {
        let path = self.source_index_path();
        let bytes = match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ReferenceSourceIndex::empty());
            }
            Err(error) => return Err(io_error(error)),
            Ok(metadata) => {
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    return Err(OutputStoreError::Invalid(
                        "reference source index must be a regular file, not a symlink".into(),
                    ));
                }
                if metadata.len() > MAX_SOURCE_BYTES as u64 {
                    return Err(OutputStoreError::Corrupt(format!(
                        "reference source index is {} bytes, above the {MAX_SOURCE_BYTES} limit",
                        metadata.len()
                    )));
                }
                fs::read(&path).map_err(io_error)?
            }
        };
        if bytes.len() > MAX_SOURCE_BYTES {
            return Err(OutputStoreError::Corrupt(format!(
                "reference source index is {} bytes, above the {MAX_SOURCE_BYTES} limit",
                bytes.len()
            )));
        }
        let index: ReferenceSourceIndex = serde_json::from_slice(&bytes).map_err(|error| {
            OutputStoreError::Corrupt(format!(
                "corrupt reference source index {}: {error}",
                path.display()
            ))
        })?;
        index.validate()?;
        Ok(index)
    }

    fn write_sources_locked(&self, index: &ReferenceSourceIndex) -> Result<(), OutputStoreError> {
        index.validate()?;
        let payload = serde_json::to_vec_pretty(index).map_err(|error| {
            OutputStoreError::Io(format!(
                "cannot serialize the reference source index: {error}"
            ))
        })?;
        if payload.len() > MAX_SOURCE_BYTES {
            return Err(OutputStoreError::Bounds(format!(
                "reference source index would be {} bytes, above the {MAX_SOURCE_BYTES} limit",
                payload.len()
            )));
        }
        let dir = self.state_dir();
        self.ensure_state_dir()?;
        let mut temp = tempfile::NamedTempFile::new_in(&dir).map_err(io_error)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o600))
                .map_err(io_error)?;
        }
        temp.write_all(&payload).map_err(io_error)?;
        temp.as_file().sync_all().map_err(io_error)?;
        temp.persist(self.source_index_path()).map_err(|error| {
            OutputStoreError::Io(format!(
                "cannot persist the reference source index: {error}"
            ))
        })?;
        sync_dir(&dir)?;
        Ok(())
    }

    /// 该规范路径是否位于标准 session/journal 扫描目录内。
    fn is_standard_reference_path(&self, canonical: &Path) -> Result<bool, OutputStoreError> {
        let root = self.canonical_root()?;
        Ok(canonical.starts_with(root.join(SESSION_DIR))
            || canonical.starts_with(root.join(crate::journal::JOURNAL_DIR)))
    }

    // ------------------------------------------------------------ 发布意图

    /// 引用发布意图账本的绝对路径。
    pub fn publication_intent_path(&self) -> PathBuf {
        self.state_dir().join(PUBLICATION_FILE_NAME)
    }

    /// 只读枚举未决引用发布意图；**不创建任何目录或文件**。
    ///
    /// 供跨进程崩溃后的恢复读取使用。账本缺失视为空；损坏、版本不符、超限或身份
    /// 快照不匹配一律 fail-closed。
    pub fn load_publication_intents(&self) -> Result<PublicationIntentIndex, OutputStoreError> {
        let path = self.publication_intent_path();
        let bytes = match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PublicationIntentIndex::empty());
            }
            Err(error) => return Err(io_error(error)),
            Ok(metadata) => {
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    return Err(OutputStoreError::Invalid(
                        "publication intent index must be a regular file, not a symlink".into(),
                    ));
                }
                if metadata.len() > MAX_PUBLICATION_BYTES as u64 {
                    return Err(OutputStoreError::Corrupt(format!(
                        "publication intent index is {} bytes, above the {MAX_PUBLICATION_BYTES} limit",
                        metadata.len()
                    )));
                }
                fs::read(&path).map_err(io_error)?
            }
        };
        if bytes.len() > MAX_PUBLICATION_BYTES {
            return Err(OutputStoreError::Corrupt(format!(
                "publication intent index is {} bytes, above the {MAX_PUBLICATION_BYTES} limit",
                bytes.len()
            )));
        }
        let index: PublicationIntentIndex = serde_json::from_slice(&bytes).map_err(|error| {
            OutputStoreError::Corrupt(format!(
                "corrupt publication intent index {}: {error}",
                path.display()
            ))
        })?;
        index.validate()?;
        Ok(index)
    }

    /// 有界的未决引用发布意图摘要（只读，不创建目录/文件）。
    pub fn publication_intents(&self) -> Result<Vec<PublicationIntent>, OutputStoreError> {
        Ok(self.load_publication_intents()?.intents)
    }

    /// 在**写盘与索引登记之后、真实 Host 回调之前**记录一条 durable `Pending`
    /// 发布意图。
    ///
    /// 调用方必须已经持有同一捕获租约登记了 `references` 指向的文件。写入失败返回
    /// `Err`，调用方必须在任何 Host 尝试之前回滚本次新建文件（`CaptureTransaction`
    /// 的 `Drop` 会做）；此时没有任何 Host 副作用，因此不会保留 Pending 证据。
    pub fn begin_publication_intent(
        &self,
        lease: &CaptureLease,
        run_id: &str,
        call_id: Option<&str>,
        references: &[OutputReference],
    ) -> Result<PublicationIntent, OutputStoreError> {
        self.check_capture_lease(lease)?;
        if references.is_empty() {
            return Err(OutputStoreError::Invalid(
                "publication intent must reference at least one externalized file".into(),
            ));
        }
        let mut intent = PublicationIntent {
            attempt_id: Uuid::new_v4().to_string(),
            run_id: run_id.to_owned(),
            call_id: call_id.map(str::to_owned),
            state: PublicationIntentState::Pending,
            created: now_secs(),
            references: references.to_vec(),
            snapshot: String::new(),
        };
        intent.snapshot = publication_snapshot(&intent);
        intent.validate()?;
        let _guard = self.lock_index()?;
        let mut index = self.load_publication_intents()?;
        if index.intents.len() >= MAX_PUBLICATION_INTENTS {
            return Err(OutputStoreError::Bounds(format!(
                "publication intent index already holds {MAX_PUBLICATION_INTENTS} unresolved attempts; reconcile or clean up before publishing more"
            )));
        }
        index.intents.push(intent.clone());
        self.write_publication_intents_locked(&index)?;
        Ok(intent)
    }

    /// 把一条 `Pending` 意图标记为 `PendingUncertain`（Host 回调返回 `Err`，无法判断
    /// 是否已部分持久化）。要求 `snapshot` 完全匹配。
    pub fn mark_publication_uncertain(
        &self,
        lease: &CaptureLease,
        attempt_id: &str,
        snapshot: &str,
    ) -> Result<Option<PublicationIntent>, OutputStoreError> {
        self.check_capture_lease(lease)?;
        validate_attempt_id(attempt_id)?;
        if !is_sha256_hex(snapshot) {
            return Err(OutputStoreError::Invalid(
                "publication intent snapshot is malformed".into(),
            ));
        }
        let _guard = self.lock_index()?;
        let mut index = self.load_publication_intents()?;
        let Some(position) = index
            .intents
            .iter()
            .position(|intent| intent.attempt_id == attempt_id)
        else {
            return Ok(None);
        };
        if index.intents[position].snapshot != snapshot {
            return Err(OutputStoreError::StalePlan(format!(
                "publication intent {attempt_id} does not match the supplied snapshot; refusing to change a different intent"
            )));
        }
        index.intents[position].state = PublicationIntentState::PendingUncertain;
        self.write_publication_intents_locked(&index)?;
        Ok(Some(index.intents[position].clone()))
    }

    /// 在捕获租约下注销一条已对账的意图（`Published` 移交常规引用扫描，`Unpublished`
    /// 视为已证明未发布）。要求 `snapshot` 完全匹配；旧快照一律拒绝。
    pub fn resolve_publication_intent(
        &self,
        lease: &CaptureLease,
        attempt_id: &str,
        snapshot: &str,
        resolution: PublicationResolution,
    ) -> Result<Option<PublicationIntent>, OutputStoreError> {
        self.check_capture_lease(lease)?;
        self.mutate_publication_intent(attempt_id, snapshot, resolution)
    }

    /// 显式的 Host 对账入口：不需要捕获租约，供 run 之外的持久化现场核对使用。
    ///
    /// 与 [`OutputStore::resolve_publication_intent`] 相同，但额外要求工作区**静止**：
    /// 先以 `LOCK_NB` 取得 workspace 级 exclusive activity 租约，再取索引叶锁，锁顺序
    /// 固定为 `activity -> index`，不会与捕获或清理形成嵌套等待。捕获事务
    /// （`begin_capture`）与引用写入者（`Session::save` / journal 持久化）从头到尾持有
    /// 同一把 shared activity 租约，**包括真实 Host 回调仍在阻塞的整个区间**；只要工作区
    /// 里还可能有发布者存活，这里就立即 [`OutputStoreError::Busy`]，而不是用时间或 PID
    /// 去猜它已经死亡。缺少身份返回 `Ok(None)`，绝不伪造；`snapshot` 不匹配返回
    /// [`OutputStoreError::StalePlan`]。**只有 Host 明确判定后才能调用**；本 API 不会依据
    /// 时间或 PID 死亡自动断言未发布。
    ///
    /// 发布者要在自己仍持有 capture 租约时注销意图，请改用
    /// [`OutputStore::resolve_publication_intent`] 或
    /// [`CaptureTransaction::resolve_publication_intent`]：那条路径已经证明"是本次事务
    /// 自己"，因此不要求 exclusive activity，成功的发布流水线仍能在自己的租约内完成
    /// handoff。
    pub fn reconcile_publication_intent(
        &self,
        attempt_id: &str,
        snapshot: &str,
        resolution: PublicationResolution,
    ) -> Result<Option<PublicationIntent>, OutputStoreError> {
        // 真实的 quiescence，而不是时间/PID 猜测：exclusive activity 与捕获/写入者的
        // shared activity 互斥，所以任何仍可能持久化引用的发布者都会让这里 fail closed。
        let _quiescent = workspace_activity::exclusive_for_workspace(&self.root)?.ok_or_else(
            || {
                OutputStoreError::Busy(
                    "an active run, capture transaction or reference writer still holds the workspace activity lease; refusing to reconcile a publication intent while a publisher may still persist its references"
                        .into(),
                )
            },
        )?;
        self.mutate_publication_intent(attempt_id, snapshot, resolution)
    }

    fn mutate_publication_intent(
        &self,
        attempt_id: &str,
        snapshot: &str,
        resolution: PublicationResolution,
    ) -> Result<Option<PublicationIntent>, OutputStoreError> {
        validate_attempt_id(attempt_id)?;
        if !is_sha256_hex(snapshot) {
            return Err(OutputStoreError::Invalid(
                "publication intent snapshot is malformed".into(),
            ));
        }
        // Both terminal resolutions retire the persisted entry. `Published`
        // hands the reference off to the real journal/session scan;
        // `Unpublished` records that the Host proved no reference was persisted.
        // Physical reclamation is always left to the normal index retention, so
        // a content-addressed file another reference owns is never deleted here.
        let _ = resolution;
        let _guard = self.lock_index()?;
        let mut index = self.load_publication_intents()?;
        let Some(position) = index
            .intents
            .iter()
            .position(|intent| intent.attempt_id == attempt_id)
        else {
            return Ok(None);
        };
        if index.intents[position].snapshot != snapshot {
            return Err(OutputStoreError::StalePlan(format!(
                "publication intent {attempt_id} does not match the supplied snapshot; refusing to resolve a different intent"
            )));
        }
        let resolved = index.intents.remove(position);
        self.write_publication_intents_locked(&index)?;
        Ok(Some(resolved))
    }

    fn write_publication_intents_locked(
        &self,
        index: &PublicationIntentIndex,
    ) -> Result<(), OutputStoreError> {
        index.validate()?;
        let payload = serde_json::to_vec_pretty(index).map_err(|error| {
            OutputStoreError::Io(format!(
                "cannot serialize the publication intent index: {error}"
            ))
        })?;
        if payload.len() > MAX_PUBLICATION_BYTES {
            return Err(OutputStoreError::Bounds(format!(
                "publication intent index would be {} bytes, above the {MAX_PUBLICATION_BYTES} limit",
                payload.len()
            )));
        }
        let dir = self.state_dir();
        self.ensure_state_dir()?;
        let mut temp = tempfile::NamedTempFile::new_in(&dir).map_err(io_error)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o600))
                .map_err(io_error)?;
        }
        temp.write_all(&payload).map_err(io_error)?;
        temp.as_file().sync_all().map_err(io_error)?;
        temp.persist(self.publication_intent_path())
            .map_err(|error| {
                OutputStoreError::Io(format!(
                    "cannot persist the publication intent index: {error}"
                ))
            })?;
        sync_dir(&dir)?;
        Ok(())
    }

    // ---------------------------------------------------------------- 清理

    /// 取得 exclusive 清理租约。存在活动 run、捕获或引用写入者时返回
    /// [`OutputStoreError::Busy`]；非 Unix 平台返回 [`OutputStoreError::Unsupported`]。
    ///
    /// 顺序固定为 activity（exclusive）→ 产物租约（exclusive）；两者都非阻塞，
    /// 存在 shared 租约时立即 Busy，不排队等待。
    #[cfg(unix)]
    pub fn begin_cleanup(&self) -> Result<CleanupLease, OutputStoreError> {
        self.ensure_output_dir()?;
        self.ensure_state_dir()?;
        let dir = self.state_dir();
        let activity =
            workspace_activity::exclusive_for_workspace(&self.root)?.ok_or_else(|| {
                OutputStoreError::Busy(
                    "an active run, capture or reference writer holds the shared workspace activity lease"
                        .into(),
                )
            })?;
        let file = open_lock_file(&dir.join(CLEANUP_LOCK_FILE))?;
        if flock_try_exclusive(&file)? {
            Ok(CleanupLease {
                dir,
                _activity: activity,
                _file: file,
            })
        } else {
            Err(OutputStoreError::Busy(
                "an output capture or another cleanup holds the output lease".into(),
            ))
        }
    }

    /// 非 Unix：清理明确不受支持，不静默降级。
    #[cfg(not(unix))]
    pub fn begin_cleanup(&self) -> Result<CleanupLease, OutputStoreError> {
        Err(OutputStoreError::Unsupported(
            "offline output cleanup requires Unix flock; this platform is not supported".into(),
        ))
    }

    /// 只读 dry-run：扫描索引与有界 session/journal 引用，产出待删计划与摘要。
    ///
    /// 不获取 exclusive 租约，也不删除任何文件。摘要绑定规范工作区路径、保留期与
    /// 完整索引，因此该计划无法在另一个工作区或另一个保留策略下确认。
    ///
    /// 因为不删除任何文件，dry-run 可以与活动 run 并存；真正删除的
    /// [`OutputStore::confirm_cleanup`] 才会取得 exclusive activity 租约。
    pub fn plan_cleanup(&self, now: SystemTime) -> Result<CleanupPlan, OutputStoreError> {
        let now = unix_secs(now)?;
        let workspace = self.canonical_root()?.to_string_lossy().into_owned();
        let references = self.collect_references()?;
        let index = self.load_index()?;
        let mut selection = self.select(&index, &references, now)?;
        let candidate_paths = paths_of(&selection.candidates);
        let digest = plan_digest(&workspace, DEFAULT_RETENTION_SECS, &index, &candidate_paths);
        selection.warnings.push(CONCURRENCY_WARNING.to_owned());
        Ok(CleanupPlan {
            version: INDEX_VERSION,
            workspace,
            retention_secs: DEFAULT_RETENTION_SECS,
            generated: now,
            digest,
            candidates: selection.candidates,
            retained_referenced: selection.retained_referenced,
            retained_recent: selection.retained_recent,
            pruned_missing: selection.pruned_missing,
            unregistered: selection.unregistered,
            warnings: selection.warnings,
        })
    }

    /// 显式确认并执行清理。
    ///
    /// 在 exclusive 租约下重新扫描引用、重算计划摘要；与 `plan.digest` 不一致（索引、
    /// 候选集合、保留期或工作区发生变化）时以 [`OutputStoreError::StalePlan`] 拒绝，
    /// 因此旧计划或另一个工作区的计划都不可能删掉本工作区的文件。
    ///
    /// 删除前的整体验证（普通文件、内容 hash 一致）是原子的：任何不一致都在任何删除
    /// 发生前返回错误。删除与索引更新无法合成一次原子事务，顺序固定为“先删文件、后写
    /// 索引”：任何中断只留下“索引里多出已不存在的条目”，下一次 `plan_cleanup` 会把它
    /// 当作 `pruned_missing` 回收（反过来先写索引会留下永远不会再被清理的孤儿文件）。
    /// 删除失败或索引写失败时返回 [`OutputStoreError::PartialCleanup`]，其中携带如实
    /// 记录的部分状态（`failed` 条目、成功删除的条目、未更新的索引说明）。
    ///
    /// **并发边界**：本调用先取得 exclusive activity 租约，再取得 exclusive 产物租约
    /// （见 [`OutputStore::begin_cleanup`]），并在扫描之前拒绝任何仍持有 shared 租约的
    /// 活动 run、捕获事务或引用写入者（`Session::save` / journal 持久化）。非协作的
    /// 外部写入者不持有租约，不在保证范围内（见 [`CONCURRENCY_WARNING`]）。
    pub fn confirm_cleanup(
        &self,
        plan: &CleanupPlan,
        now: SystemTime,
    ) -> Result<CleanupReport, OutputStoreError> {
        let now = unix_secs(now)?;
        let workspace = self.canonical_root()?.to_string_lossy().into_owned();
        if plan.version != INDEX_VERSION || plan.retention_secs != DEFAULT_RETENTION_SECS {
            return Err(OutputStoreError::StalePlan(
                "cleanup plan was produced by a different index version or retention policy".into(),
            ));
        }
        if plan.workspace != workspace {
            return Err(OutputStoreError::StalePlan(format!(
                "cleanup plan was produced for workspace '{}' but this store owns '{workspace}'",
                plan.workspace
            )));
        }
        let _lease = self.begin_cleanup()?;
        let references = self.collect_references()?;
        let index = self.load_index()?;
        let selection = self.select(&index, &references, now)?;
        let candidate_paths = paths_of(&selection.candidates);
        if plan_digest(&workspace, plan.retention_secs, &index, &candidate_paths) != plan.digest {
            return Err(OutputStoreError::StalePlan(
                "output index, workspace or references changed since the dry-run plan; re-run the plan"
                    .into(),
            ));
        }

        // 先整体校验，再统一删除：任何内容不一致都在删除发生前拒绝（无部分状态）。
        let mut verified = Vec::new();
        for entry in &selection.candidates {
            let name = validate_entry_path(&entry.path)?;
            if references.contains(&name) {
                continue;
            }
            let path = self.resolve_name(&name);
            let (bytes, hash) = read_and_hash(&path)?;
            if entry.bytes.is_some_and(|declared| declared != bytes)
                || entry
                    .hash
                    .as_deref()
                    .is_some_and(|declared| declared != hash)
            {
                return Err(OutputStoreError::ContentMismatch(format!(
                    "{} changed since it was registered; refusing to delete it",
                    entry.path
                )));
            }
            verified.push((entry.clone(), path, bytes));
        }

        let mut removed = Vec::new();
        let mut failed = Vec::new();
        let mut pruned_missing = selection.pruned_missing.clone();
        let mut freed_bytes = 0u64;
        for (entry, path, bytes) in verified {
            match fs::remove_file(&path) {
                Ok(()) => {
                    freed_bytes = freed_bytes.saturating_add(bytes);
                    removed.push(entry);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // 校验之后被别人删掉了：索引条目仍需回收，但没有释放空间。
                    pruned_missing.push(entry.path);
                }
                Err(error) => failed.push(CleanupFailure {
                    path: entry.path,
                    message: error.to_string(),
                }),
            }
        }
        pruned_missing.sort();
        pruned_missing.dedup();

        let mut warnings = selection.warnings.clone();
        warnings.push(CONCURRENCY_WARNING.to_owned());
        if !failed.is_empty() {
            warnings.push(format!(
                "{} registered output(s) could not be deleted; their index entries were kept so the next plan retries them",
                failed.len()
            ));
        }

        let before = index.entries.len();
        let mut next = index;
        let removed_paths: BTreeSet<&str> =
            removed.iter().map(|entry| entry.path.as_str()).collect();
        let prune_set: BTreeSet<&str> = pruned_missing.iter().map(String::as_str).collect();
        next.entries.retain(|entry| {
            !removed_paths.contains(entry.path.as_str()) && !prune_set.contains(entry.path.as_str())
        });
        let index_changed = next.entries.len() != before;
        let index_error = if index_changed {
            let outcome = match self.lock_index() {
                Ok(_index_guard) => self.write_index_locked(&next),
                Err(error) => Err(error),
            };
            match outcome {
                Ok(()) => None,
                Err(error) => {
                    warnings.push(format!(
                        "the output index was not updated after deleting {} file(s): {error}; the next plan will prune the now-missing entries",
                        removed.len()
                    ));
                    Some(error)
                }
            }
        } else {
            None
        };

        let report = CleanupReport {
            removed,
            freed_bytes,
            retained_referenced: selection.retained_referenced,
            retained_recent: selection.retained_recent,
            pruned_missing,
            unregistered: selection.unregistered,
            failed,
            warnings,
        };
        let failed_count = report.failed.len();
        if failed_count > 0 {
            return Err(OutputStoreError::PartialCleanup(
                Box::new(report),
                format!(
                    "{failed_count} registered output(s) could not be deleted; the index keeps them for a retry and the attached report records what was removed"
                ),
            ));
        }
        if let Some(error) = index_error {
            if report.removed.is_empty() {
                return Err(error);
            }
            let message = format!(
                "{} output file(s) were deleted but the output index was not updated: {error}",
                report.removed.len()
            );
            return Err(OutputStoreError::PartialCleanup(Box::new(report), message));
        }
        Ok(report)
    }

    // ---------------------------------------------------------------- 内部

    fn ensure_output_dir(&self) -> Result<(), OutputStoreError> {
        let product = self.root.join(".mio");
        let outputs = product.join("outputs");
        for dir in [&product, &outputs] {
            match fs::create_dir(dir) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(OutputStoreError::Io(format!(
                        "cannot create output directory {}: {error}",
                        dir.display()
                    )));
                }
            }
            let metadata = fs::symlink_metadata(dir).map_err(io_error)?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(OutputStoreError::Invalid(format!(
                    "{} must be a real directory, not a symlink",
                    dir.display()
                )));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(io_error)?;
            }
        }
        Ok(())
    }

    /// 确保 `.mio/output-index`（索引、锁与来源登记表）是真实的 0700 目录，但**不**创建
    /// `.mio/outputs`，因此守卫一个尚未发生捕获的工作区不会新建产物目录。
    fn ensure_state_dir(&self) -> Result<(), OutputStoreError> {
        let product = self.root.join(PRODUCT_DIR);
        let state = self.state_dir();
        for dir in [&product, &state] {
            match fs::create_dir(dir) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(OutputStoreError::Io(format!(
                        "cannot create output state directory {}: {error}",
                        dir.display()
                    )));
                }
            }
            let metadata = fs::symlink_metadata(dir).map_err(io_error)?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(OutputStoreError::Invalid(format!(
                    "{} must be a real directory, not a symlink",
                    dir.display()
                )));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(io_error)?;
            }
        }
        Ok(())
    }

    fn check_capture_lease(&self, lease: &CaptureLease) -> Result<(), OutputStoreError> {
        if lease.directory() != self.state_dir() {
            return Err(OutputStoreError::Invalid(
                "capture lease belongs to a different output store".into(),
            ));
        }
        Ok(())
    }

    fn resolve_name(&self, name: &str) -> PathBuf {
        self.output_dir().join(name)
    }

    fn lock_index(&self) -> Result<IndexLock, OutputStoreError> {
        static PROCESS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let guard = PROCESS_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.ensure_state_dir()?;
        let file = open_lock_file(&self.state_dir().join(INDEX_LOCK_FILE))?;
        flock_index(&file)?;
        Ok(IndexLock {
            _process: guard,
            _file: file,
        })
    }

    fn write_index_locked(&self, index: &OutputIndex) -> Result<(), OutputStoreError> {
        index.validate()?;
        let payload = serde_json::to_vec_pretty(index).map_err(|error| {
            OutputStoreError::Io(format!("cannot serialize the output index: {error}"))
        })?;
        if payload.len() > MAX_INDEX_BYTES {
            return Err(OutputStoreError::Bounds(format!(
                "output index would be {} bytes, above the {MAX_INDEX_BYTES} limit",
                payload.len()
            )));
        }
        let dir = self.state_dir();
        self.ensure_state_dir()?;
        let mut temp = tempfile::NamedTempFile::new_in(&dir).map_err(io_error)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o600))
                .map_err(io_error)?;
        }
        temp.write_all(&payload).map_err(io_error)?;
        temp.as_file().sync_all().map_err(io_error)?;
        temp.persist(self.index_path()).map_err(|error| {
            OutputStoreError::Io(format!("cannot persist the output index: {error}"))
        })?;
        sync_dir(&dir)?;
        Ok(())
    }

    /// 选取候选删除集合并同时计算保留原因与未登记文件。
    fn select(
        &self,
        index: &OutputIndex,
        references: &BTreeSet<String>,
        now: u64,
    ) -> Result<Selection, OutputStoreError> {
        let mut selection = Selection::default();
        let unresolved = self.load_publication_intents()?;
        if !unresolved.intents.is_empty() {
            selection.warnings.push(format!(
                "{} unresolved output publication intent(s) are retained until an explicit host reconciliation; retention alone never garbage-collects them",
                unresolved.intents.len()
            ));
        }
        for entry in &index.entries {
            let name = validate_entry_path(&entry.path)?;
            if references.contains(&name) {
                selection.retained_referenced.push(entry.path.clone());
                continue;
            }
            // 严格超过保留期才进入候选；恰好等于阈值的条目保留。
            if entry.created.saturating_add(DEFAULT_RETENTION_SECS) >= now {
                selection.retained_recent.push(entry.path.clone());
                continue;
            }
            let path = self.resolve_name(&name);
            match fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    selection.pruned_missing.push(entry.path.clone());
                    selection.warnings.push(format!(
                        "registered output {} is missing and will be pruned from the index",
                        entry.path
                    ));
                    continue;
                }
                Err(error) => return Err(io_error(error)),
                Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
                    return Err(OutputStoreError::Invalid(format!(
                        "registered output {} is not a regular file",
                        entry.path
                    )));
                }
                Ok(_) => {}
            }
            selection.candidates.push(entry.clone());
        }
        selection.candidates.sort_by(|a, b| a.path.cmp(&b.path));
        selection.retained_referenced.sort();
        selection.retained_recent.sort();
        selection.pruned_missing.sort();
        selection.warnings.sort();
        selection.unregistered = self.list_unregistered(index)?;
        Ok(selection)
    }

    /// 列出磁盘上存在但未登记的普通文件；只列出，绝不删除。
    fn list_unregistered(&self, index: &OutputIndex) -> Result<Vec<String>, OutputStoreError> {
        let known: BTreeSet<&str> = index
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        let mut unregistered = Vec::new();
        let read_dir = match fs::read_dir(self.output_dir()) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(unregistered),
            Err(error) => return Err(io_error(error)),
            Ok(read_dir) => read_dir,
        };
        let mut budget = ScanBudget::default();
        for entry in read_dir {
            let entry = entry.map_err(io_error)?;
            budget.count_entry()?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if reserved_output_name(&name) {
                continue;
            }
            let file_type = entry.file_type().map_err(io_error)?;
            if file_type.is_symlink() {
                return Err(OutputStoreError::Invalid(format!(
                    "output directory entry {name} must not be a symlink"
                )));
            }
            if !file_type.is_file() {
                continue;
            }
            let path = format!("{OUTPUT_PREFIX}{name}");
            if !known.contains(path.as_str()) {
                unregistered.push(path);
            }
        }
        unregistered.sort();
        Ok(unregistered)
    }

    /// 有界扫描 session、journal 与已登记的外部来源，收集仍被引用的外置文件名。
    ///
    /// 任何无法确认引用关系的情况（解析失败、schema 版本不符、缺必需字段、活动 run、
    /// 未决/未知副作用、证据被截断或丢弃、符号链接、工作区外的外部来源、超限）都返回
    /// `Err`，由调用方拒绝清理：无法证明“无引用”时绝不删除。
    fn collect_references(&self) -> Result<BTreeSet<String>, OutputStoreError> {
        let expected_session_version = self.current_session_version()?;
        let mut references = BTreeSet::new();
        let mut budget = ScanBudget::default();
        self.scan_sessions(expected_session_version, &mut references, &mut budget)?;
        self.scan_journals(&mut references, &mut budget)?;
        self.scan_registered_sources(expected_session_version, &mut references, &mut budget)?;
        self.scan_publication_intents(&mut references)?;
        Ok(references)
    }

    /// 未决（`Pending`/`PendingUncertain`）引用发布意图精确保护它们引用的全部文件。
    ///
    /// 账本损坏/版本不符/超限时 `load_publication_intents` 已经 fail-closed；这里只把
    /// 真实 typed 引用的文件名并入引用集合，因此不会在保留期后静默 GC，也不会因为
    /// 一个不完整意图删除其它文件。
    fn scan_publication_intents(
        &self,
        references: &mut BTreeSet<String>,
    ) -> Result<(), OutputStoreError> {
        let index = self.load_publication_intents()?;
        for intent in &index.intents {
            for reference in &intent.references {
                let name = validate_entry_path(&reference.path)?;
                references.insert(name);
            }
        }
        Ok(())
    }

    /// 当前 crate 真实的 session schema 版本。
    ///
    /// `src/session.rs` 把 `CURRENT_SESSION_VERSION` 保持为私有，因此这里通过公开的
    /// `Session::new` 读出同一个常量，而不是在本模块镜像一个会漂移的数字。
    fn current_session_version(&self) -> Result<u32, OutputStoreError> {
        Session::new(&self.root)
            .map(|session| session.version)
            .map_err(|error| {
                OutputStoreError::Corrupt(format!(
                    "cannot determine the supported session schema version from {}: {error}",
                    self.root.display()
                ))
            })
    }

    fn scan_sessions(
        &self,
        expected_version: u32,
        references: &mut BTreeSet<String>,
        budget: &mut ScanBudget,
    ) -> Result<(), OutputStoreError> {
        let dir = self.root.join(SESSION_DIR);
        let read_dir = match fs::read_dir(&dir) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io_error(error)),
            Ok(read_dir) => read_dir,
        };
        require_real_directory(&dir, "session")?;
        for entry in read_dir {
            let entry = entry.map_err(io_error)?;
            budget.count_entry()?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".json") {
                continue;
            }
            let file_type = entry.file_type().map_err(io_error)?;
            if file_type.is_symlink() {
                return Err(OutputStoreError::Invalid(format!(
                    "session entry {name} must not be a symlink"
                )));
            }
            if !file_type.is_file() {
                continue;
            }
            self.scan_session_file(&entry.path(), expected_version, references, budget)?;
        }
        Ok(())
    }

    /// 扫描已登记的外部引用来源。
    ///
    /// - 位于标准扫描目录内的条目已经被 `scan_sessions` / `scan_journals` 覆盖，跳过。
    /// - 位于工作区内的条目按真实 `Session` schema 解析，与标准目录相同地 fail-closed。
    /// - 位于工作区外的条目**不被读取**（不扫描宿主私密文件），而是整体拒绝清理并把
    ///   来源路径写进错误信息。
    fn scan_registered_sources(
        &self,
        expected_version: u32,
        references: &mut BTreeSet<String>,
        budget: &mut ScanBudget,
    ) -> Result<(), OutputStoreError> {
        let index = self.load_sources()?;
        if index.sources.is_empty() {
            return Ok(());
        }
        let root = self.canonical_root()?;
        for source in &index.sources {
            let path = PathBuf::from(&source.path);
            if !path.is_absolute() {
                return Err(OutputStoreError::Corrupt(format!(
                    "registered reference source '{}' is not absolute",
                    source.path
                )));
            }
            if path.starts_with(root.join(SESSION_DIR))
                || path.starts_with(root.join(crate::journal::JOURNAL_DIR))
            {
                continue;
            }
            if !path.starts_with(&root) {
                return Err(OutputStoreError::IncompleteEvidence(format!(
                    "session '{}' was saved outside this workspace; its output references cannot be enumerated without reading host files, so cleanup is refused",
                    source.path
                )));
            }
            budget.count_entry()?;
            let metadata = match fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(OutputStoreError::IncompleteEvidence(format!(
                        "registered reference source '{}' is missing; its references cannot be verified",
                        source.path
                    )));
                }
                Err(error) => return Err(io_error(error)),
                Ok(metadata) => metadata,
            };
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(OutputStoreError::Invalid(format!(
                    "registered reference source '{}' must be a regular file",
                    source.path
                )));
            }
            self.scan_session_file(&path, expected_version, references, budget)?;
        }
        Ok(())
    }

    /// 按真实 `Session` schema 解析单个 session 文件并递归扫描引用。
    ///
    /// 调用方负责有界计数与路径合法性；本函数只做内容层校验，保证标准目录与外部来源
    /// 使用完全相同的证据标准。
    fn scan_session_file(
        &self,
        path: &Path,
        expected_version: u32,
        references: &mut BTreeSet<String>,
        budget: &mut ScanBudget,
    ) -> Result<(), OutputStoreError> {
        let bytes = read_bounded(path, MAX_SESSION_BYTES)?;
        budget.add_bytes(bytes.len())?;

        // 权威解析：真实的 `Session` schema。缺少必需字段（比如 `history`）、类型
        // 不符或未知枚举值都会失败；它绝不会像字段带 `#[serde(default)]` 的镜像那样
        // 把损坏证据当成“一个引用都没有”。
        let session: Session = serde_json::from_slice(&bytes).map_err(|error| {
            OutputStoreError::Corrupt(format!("corrupt session {}: {error}", path.display()))
        })?;
        if session.version != expected_version {
            return Err(OutputStoreError::Corrupt(format!(
                "unsupported session version {} in {} (expected {expected_version})",
                session.version,
                path.display()
            )));
        }
        if !session.cwd.is_absolute() {
            return Err(OutputStoreError::Corrupt(format!(
                "session {} declares a relative workspace",
                path.display()
            )));
        }
        if session.updated < session.created {
            return Err(OutputStoreError::Corrupt(format!(
                "session {} has timestamps that go backwards",
                path.display()
            )));
        }
        // Valid schema is not proof that the saved history is complete:
        // extractive compaction may have removed a reference from its
        // middle. Without a complete reference ledger, refuse deletion.
        let known_compaction = session
            .last_compaction_loss
            .as_ref()
            .is_some_and(|report| report.extractive)
            || session.history.iter().any(|message| match message {
                crate::Message::User { content } => content.iter().any(|part| {
                    matches!(
                        part, mio_core::message::UserContent::Text(text)
                        if text.text.starts_with("[System Context Compaction:")
                            || text.text.starts_with("[Earlier context summary;")
                    )
                }),
                _ => false,
            });
        if known_compaction {
            return Err(OutputStoreError::IncompleteEvidence(format!(
                "session {} contains extractively compacted history; original output references may be missing",
                path.display()
            )));
        }

        // 原始已校验值：递归扫描 `history`、`pinned_context`、
        // `last_compaction_loss` 以及任何未知/新增字段里的字符串。
        let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
            OutputStoreError::Corrupt(format!("corrupt session {}: {error}", path.display()))
        })?;
        scan_value_references(&value, 0, budget, references)?;
        Ok(())
    }

    fn scan_journals(
        &self,
        references: &mut BTreeSet<String>,
        budget: &mut ScanBudget,
    ) -> Result<(), OutputStoreError> {
        let dir = self.root.join(crate::journal::JOURNAL_DIR);
        let read_dir = match fs::read_dir(&dir) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io_error(error)),
            Ok(read_dir) => read_dir,
        };
        require_real_directory(&dir, "journal")?;
        for entry in read_dir {
            let entry = entry.map_err(io_error)?;
            budget.count_entry()?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".json") {
                continue;
            }
            let file_type = entry.file_type().map_err(io_error)?;
            if file_type.is_symlink() {
                return Err(OutputStoreError::Invalid(format!(
                    "journal entry {name} must not be a symlink"
                )));
            }
            if !file_type.is_file() {
                continue;
            }
            let path = entry.path();
            let bytes = read_bounded(&path, MAX_JOURNAL_BYTES)?;
            budget.add_bytes(bytes.len())?;
            let journal: JournalFile = serde_json::from_slice(&bytes).map_err(|error| {
                OutputStoreError::Corrupt(format!("corrupt journal {}: {error}", path.display()))
            })?;
            if !matches!(journal.version, JOURNAL_VERSION | JOURNAL_VERSION_V1) {
                return Err(OutputStoreError::Corrupt(format!(
                    "unsupported journal version {} in {} (supported: {JOURNAL_VERSION_V1}, {JOURNAL_VERSION})",
                    journal.version,
                    path.display()
                )));
            }
            if journal.session_id.trim().is_empty() {
                return Err(OutputStoreError::Corrupt(format!(
                    "journal {} declares no session id",
                    path.display()
                )));
            }
            // 写入端已经把旧证据丢掉时，无法再证明“没有引用”。
            if journal.dropped_runs != 0 {
                return Err(OutputStoreError::IncompleteEvidence(format!(
                    "journal {} dropped {} old run record(s); reference evidence is incomplete",
                    path.display(),
                    journal.dropped_runs
                )));
            }
            if journal.truncated_operations != 0 {
                return Err(OutputStoreError::IncompleteEvidence(format!(
                    "journal {} truncated {} old operation record(s); reference evidence is incomplete",
                    path.display(),
                    journal.truncated_operations
                )));
            }
            for run in &journal.runs {
                let label = format!("run {} in {}", run.run_id, path.display());
                match run.state {
                    RunState::Active => {
                        return Err(OutputStoreError::ActiveRun(format!(
                            "journal {label} is still active"
                        )));
                    }
                    RunState::Completed
                    | RunState::Failed
                    | RunState::Cancelled
                    | RunState::Interrupted => {}
                }
                if run.goal_truncated {
                    return Err(OutputStoreError::IncompleteEvidence(format!(
                        "journal {label} recorded a truncated goal; reference evidence is incomplete"
                    )));
                }
                if let Some(goal) = &run.goal {
                    ensure_not_truncated(goal, JOURNAL_GOAL_CHARS, "goal", &label)?;
                }
                for operation in &run.operations {
                    match operation.status {
                        OpStatus::Executed | OpStatus::Failed => {}
                        OpStatus::Pending | OpStatus::Unknown => {
                            return Err(OutputStoreError::IncompleteEvidence(format!(
                                "journal {label} has an unresolved side effect (operation {}, status {:?}); reference evidence is incomplete",
                                operation.op_id, operation.status
                            )));
                        }
                    }
                    // `src/journal.rs::truncate` 在写入时截断这三个字段；带截断标记就不再
                    // 是完整证据，也不能拿前缀当引用。
                    ensure_not_truncated(
                        &operation.operation,
                        MAX_TARGET_CHARS,
                        "operation",
                        &label,
                    )?;
                    ensure_not_truncated(&operation.target, MAX_TARGET_CHARS, "target", &label)?;
                    if let Some(detail) = &operation.detail {
                        ensure_not_truncated(detail, MAX_DETAIL_CHARS, "detail", &label)?;
                    }
                    for (field, value) in [
                        ("scope", operation.correlation.scope.as_deref()),
                        (
                            "tool_call_id",
                            operation.correlation.tool_call_id.as_deref(),
                        ),
                        ("call_id", operation.correlation.call_id.as_deref()),
                    ] {
                        if let Some(value) = value {
                            ensure_not_truncated(value, MAX_TARGET_CHARS, field, &label)?;
                        }
                    }
                }
                // Canonical calls (v2) use their own authoritative phases: a
                // `Denied` or `NotDispatched` call has zero effect and never
                // blocks cleanup, while a dispatched/unknown or dropped call
                // makes the reference evidence incomplete and refuses it.
                if run.dropped_calls != 0 {
                    return Err(OutputStoreError::IncompleteEvidence(format!(
                        "journal {label} dropped {} canonical tool call record(s); reference evidence is incomplete",
                        run.dropped_calls
                    )));
                }
                for call in &run.calls {
                    match call.phase {
                        ToolCallPhase::Denied | ToolCallPhase::NotDispatched => {}
                        ToolCallPhase::Executed | ToolCallPhase::Failed => {}
                        ToolCallPhase::Normalized
                        | ToolCallPhase::Admitted
                        | ToolCallPhase::Dispatched
                        | ToolCallPhase::Unknown => {
                            return Err(OutputStoreError::IncompleteEvidence(format!(
                                "journal {label} has an unresolved canonical tool call {} ({}), phase {:?}; reference evidence is incomplete",
                                call.call_index, call.tool, call.phase
                            )));
                        }
                    }
                    if call.references_truncated {
                        return Err(OutputStoreError::IncompleteEvidence(format!(
                            "journal {label} truncated output references for canonical call {}; reference evidence is incomplete",
                            call.call_index
                        )));
                    }
                    ensure_not_truncated(&call.tool, MAX_TARGET_CHARS, "tool", &label)?;
                    ensure_not_truncated(&call.operation, MAX_TARGET_CHARS, "operation", &label)?;
                    if !call.target.is_empty() {
                        ensure_not_truncated(&call.target, MAX_TARGET_CHARS, "target", &label)?;
                    }
                    if let Some(detail) = &call.detail {
                        ensure_not_truncated(detail, MAX_DETAIL_CHARS, "detail", &label)?;
                    }
                    for (field, value) in [
                        ("scope", Some(call.scope.as_str())),
                        ("tool_call_id", call.tool_call_id.as_deref()),
                        ("call_id", call.call_id.as_deref()),
                    ] {
                        if let Some(value) = value {
                            ensure_not_truncated(value, MAX_TARGET_CHARS, field, &label)?;
                        }
                    }
                }
            }

            // 原始已校验值：覆盖 `goal` / `target` / `detail` 以及任何未知字段。
            let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
                OutputStoreError::Corrupt(format!("corrupt journal {}: {error}", path.display()))
            })?;
            scan_value_references(&value, 0, budget, references)?;
        }
        Ok(())
    }
}

/// 已登记的输出目录中的保留文件名（不属于外置产物）。
fn reserved_output_name(name: &str) -> bool {
    name == INDEX_FILE_NAME
        || name == CLEANUP_LOCK_FILE
        || name == INDEX_LOCK_FILE
        || name.starts_with('.')
}

/// 选取结果：候选删除集合与各类保留原因。
#[derive(Default)]
struct Selection {
    candidates: Vec<OutputEntry>,
    retained_referenced: Vec<String>,
    retained_recent: Vec<String>,
    pruned_missing: Vec<String>,
    unregistered: Vec<String>,
    warnings: Vec<String>,
}

/// 索引读改写期间的进程内 + 跨进程守卫。
struct IndexLock {
    _process: std::sync::MutexGuard<'static, ()>,
    _file: File,
}

/// 有界扫描预算。
#[derive(Default)]
struct ScanBudget {
    entries: usize,
    bytes: u64,
}

impl ScanBudget {
    fn count_entry(&mut self) -> Result<(), OutputStoreError> {
        self.entries += 1;
        if self.entries > MAX_SCAN_FILES {
            return Err(OutputStoreError::Bounds(format!(
                "reference scan exceeded {MAX_SCAN_FILES} directory entries"
            )));
        }
        Ok(())
    }

    fn add_bytes(&mut self, count: usize) -> Result<(), OutputStoreError> {
        self.bytes = self.bytes.saturating_add(count as u64);
        if self.bytes > MAX_SCAN_BYTES {
            return Err(OutputStoreError::Bounds(format!(
                "reference scan exceeded {MAX_SCAN_BYTES} bytes"
            )));
        }
        Ok(())
    }
}

// --- 通用辅助 ---------------------------------------------------------------

fn io_error(error: std::io::Error) -> OutputStoreError {
    OutputStoreError::Io(error.to_string())
}

fn unix_secs(now: SystemTime) -> Result<u64, OutputStoreError> {
    now.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| OutputStoreError::Invalid("system time is before the Unix epoch".into()))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn paths_of(entries: &[OutputEntry]) -> Vec<String> {
    entries.iter().map(|entry| entry.path.clone()).collect()
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_attempt_id(attempt_id: &str) -> Result<(), OutputStoreError> {
    if attempt_id.is_empty()
        || attempt_id.chars().count() > MAX_PUBLICATION_ATTEMPT_CHARS
        || !attempt_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err(OutputStoreError::Invalid(
            "publication attempt id must be a bounded identifier".into(),
        ));
    }
    Ok(())
}

/// 绑定一条发布意图身份的 SHA-256。`state` 刻意不参与：`Pending -> PendingUncertain`
/// 不得让对账快照失效；`attempt_id`/`created`/`references` 任一变化都会改变它，因此
/// 旧快照无法解决新意图。
fn publication_snapshot(intent: &PublicationIntent) -> String {
    let mut material = String::from("mio-publication-intent-v1\n");
    material.push_str(&format!("attempt\t{}\n", intent.attempt_id));
    material.push_str(&format!("run\t{}\n", intent.run_id));
    material.push_str(&format!(
        "call\t{}\n",
        intent.call_id.as_deref().unwrap_or("null")
    ));
    material.push_str(&format!("created\t{}\n", intent.created));
    let mut references: Vec<&OutputReference> = intent.references.iter().collect();
    references.sort_by(|a, b| a.path.cmp(&b.path));
    for reference in references {
        material.push_str(&format!(
            "reference\t{}\t{}\t{}\t{}\t{}\n",
            reference.path,
            reference.total_bytes,
            reference.sha256,
            reference.stream,
            reference.call_id.as_deref().unwrap_or("null"),
        ));
    }
    hex_sha256(material.as_bytes())
}

/// 计划摘要：绑定规范工作区、保留期、完整索引快照与候选路径。
///
/// 任何新增/变更/移除（包括把计划拿到另一个工作区使用）都会改变摘要。计划里的候选
/// 列表本身只是提示：确认阶段永远按重新扫描得到的候选集合计算摘要。
fn plan_digest(
    workspace: &str,
    retention_secs: u64,
    index: &OutputIndex,
    candidate_paths: &[String],
) -> String {
    let mut entries: Vec<&OutputEntry> = index.entries.iter().collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let mut material = String::from("mio-output-cleanup-v2\n");
    material.push_str(&format!("workspace\t{workspace}\n"));
    material.push_str(&format!("retention\t{retention_secs}\n"));
    for entry in entries {
        let bytes = entry
            .bytes
            .map(|value| value.to_string())
            .unwrap_or_else(|| "null".into());
        material.push_str(&format!(
            "entry\t{}\t{bytes}\t{}\t{}\t{}\t{}\n",
            entry.path,
            entry.hash.as_deref().unwrap_or("null"),
            entry.created,
            entry.stream,
            entry.call_id.as_deref().unwrap_or("null"),
        ));
    }
    let mut candidates = candidate_paths.to_vec();
    candidates.sort();
    for path in &candidates {
        material.push_str(&format!("candidate\t{path}\n"));
    }
    hex_sha256(material.as_bytes())
}

/// 校验并返回 `.mio/outputs/<name>` 中的单组件文件名。
fn validate_entry_path(path: &str) -> Result<String, OutputStoreError> {
    let Some(name) = path.strip_prefix(OUTPUT_PREFIX) else {
        return Err(OutputStoreError::Invalid(format!(
            "output path '{path}' must be relative and start with {OUTPUT_PREFIX}"
        )));
    };
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || name.starts_with('.')
        || name.chars().count() > MAX_NAME_CHARS
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        return Err(OutputStoreError::Invalid(format!(
            "output path '{path}' contains an illegal or traversal-prone name"
        )));
    }
    Ok(name.to_owned())
}

fn validate_stream(stream: &str) -> Result<(), OutputStoreError> {
    if stream.is_empty()
        || stream.chars().count() > MAX_STREAM_CHARS
        || !stream
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err(OutputStoreError::Invalid(
            "stream must be a short alphanumeric identifier".into(),
        ));
    }
    Ok(())
}

/// 外置产物文件名的合法字符集，与 [`validate_entry_path`] 保持一致。
fn is_output_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')
}

/// 校验从 session/journal 文本中提取出的名字是否是我们真正会写出的产物名
/// （`.mio/outputs/<单个文件组件>`）。可疑名称一律拒绝。
fn validate_reference_name(name: &str) -> Result<(), OutputStoreError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.starts_with('.')
        || name.contains("..")
        || name.contains('/')
        || name.contains('\\')
        || name.chars().count() > MAX_NAME_CHARS
        || !name.chars().all(is_output_name_char)
    {
        return Err(OutputStoreError::Invalid(format!(
            "unrecognized output reference '{name}' in a session or journal"
        )));
    }
    Ok(())
}

/// 在单个字符串中查找 `.mio/outputs/<name>` 引用。
///
/// 遇到前缀但拿不到合法完整文件名（空名字、过长名字、以 `.` 开头的名字、被路径分隔符
/// 截断的前缀）时一律返回 `Err`，而不是跳过继续：跳过等于把无法确认的引用当成不存在。
fn extract_reference_names(
    text: &str,
    references: &mut BTreeSet<String>,
) -> Result<(), OutputStoreError> {
    for (index, _) in text.match_indices(OUTPUT_PREFIX) {
        let after = &text[index + OUTPUT_PREFIX.len()..];
        let name: String = after
            .chars()
            .take_while(|c| is_output_name_char(*c))
            .collect();
        if name.is_empty() {
            return Err(OutputStoreError::Invalid(format!(
                "reference scan found '{OUTPUT_PREFIX}' followed by no complete file name; refusing to treat the output as unreferenced"
            )));
        }
        // 名字后面紧跟路径分隔符说明这不是单组件产物名（或者名字就是被截断的路径）。
        let rest = &after[name.len()..];
        if rest.starts_with('/') || rest.starts_with('\\') {
            return Err(OutputStoreError::Invalid(format!(
                "reference scan found a path-like '{OUTPUT_PREFIX}{name}...' token; refusing cleanup"
            )));
        }
        validate_reference_name(&name)?;
        references.insert(name);
    }
    Ok(())
}

/// `src/journal.rs::truncate` 的真实截断规则：超过上限时保留上限个字符并追加 `...`。
///
/// 带这种标记的字段不再持有写入时的完整文本，因此可能藏着（或者只剩后半截）
/// `.mio/outputs/<name>` 引用：无法证明“无引用”，保守拒绝清理。
fn ensure_not_truncated(
    text: &str,
    max_chars: usize,
    field: &str,
    label: &str,
) -> Result<(), OutputStoreError> {
    let chars = text.chars().count();
    if chars >= max_chars + TRUNCATE_MARKER.len() && text.ends_with(TRUNCATE_MARKER) {
        return Err(OutputStoreError::IncompleteEvidence(format!(
            "journal {field} in {label} carries a truncation marker ({chars} chars, limit {max_chars}); it no longer holds the full text"
        )));
    }
    Ok(())
}

/// 对已经用真实 schema 校验过的原始 JSON 值做有界递归扫描。
///
/// 任何字符串（含对象键）中的 `.mio/outputs/<name>` 都会被登记，因此 `history`、
/// `pinned_context`、`goal`、`target`、`detail` 以及未来新增的未知字段里的引用都不会
/// 被漏掉。嵌套层数与扫描字节都有上限。
fn scan_value_references(
    value: &serde_json::Value,
    depth: usize,
    budget: &mut ScanBudget,
    references: &mut BTreeSet<String>,
) -> Result<(), OutputStoreError> {
    if depth > MAX_VALUE_DEPTH {
        return Err(OutputStoreError::Bounds(format!(
            "reference scan exceeded {MAX_VALUE_DEPTH} levels of JSON nesting"
        )));
    }
    match value {
        serde_json::Value::String(text) => {
            budget.add_bytes(text.len())?;
            extract_reference_names(text, references)
        }
        serde_json::Value::Array(items) => {
            for item in items {
                scan_value_references(item, depth + 1, budget, references)?;
            }
            Ok(())
        }
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                budget.add_bytes(key.len())?;
                extract_reference_names(key, references)?;
                scan_value_references(item, depth + 1, budget, references)?;
            }
            Ok(())
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            Ok(())
        }
    }
}

fn require_real_directory(dir: &Path, label: &str) -> Result<(), OutputStoreError> {
    let metadata = fs::symlink_metadata(dir).map_err(io_error)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(OutputStoreError::Invalid(format!(
            "{label} directory {} must be a real directory, not a symlink",
            dir.display()
        )));
    }
    Ok(())
}

fn read_bounded(path: &Path, max_bytes: u64) -> Result<Vec<u8>, OutputStoreError> {
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(OutputStoreError::Invalid(format!(
            "{} must be a regular file, not a symlink",
            path.display()
        )));
    }
    if metadata.len() > max_bytes {
        return Err(OutputStoreError::Corrupt(format!(
            "{} is {} bytes, above the {max_bytes} limit",
            path.display(),
            metadata.len()
        )));
    }
    let file = File::open(path).map_err(io_error)?;
    let mut bytes = Vec::new();
    (&file)
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > max_bytes {
        return Err(OutputStoreError::Corrupt(format!(
            "{} is larger than the {max_bytes} byte limit",
            path.display()
        )));
    }
    Ok(bytes)
}

fn read_and_hash(path: &Path) -> Result<(u64, String), OutputStoreError> {
    let bytes = read_bounded(path, MAX_OUTPUT_BYTES)?;
    Ok((bytes.len() as u64, hex_sha256(&bytes)))
}

fn open_lock_file(path: &Path) -> Result<File, OutputStoreError> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        return Err(OutputStoreError::Invalid(format!(
            "{} must not be a symlink",
            path.display()
        )));
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path).map_err(io_error)?;
    if !file.metadata().map_err(io_error)?.is_file() {
        return Err(OutputStoreError::Invalid(format!(
            "{} must be a regular file",
            path.display()
        )));
    }
    Ok(file)
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> Result<(), OutputStoreError> {
    File::open(dir)
        .and_then(|file| file.sync_all())
        .map_err(io_error)
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> Result<(), OutputStoreError> {
    Ok(())
}

#[cfg(unix)]
fn flock_shared(file: &File) -> Result<(), OutputStoreError> {
    use std::os::unix::io::AsRawFd;
    loop {
        // SAFETY: `file` keeps a valid descriptor alive for the whole call.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH) };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(OutputStoreError::Io(format!(
            "cannot acquire the shared output lease: {error}"
        )));
    }
}

#[cfg(unix)]
fn flock_try_exclusive(file: &File) -> Result<bool, OutputStoreError> {
    use std::os::unix::io::AsRawFd;
    // SAFETY: `file` keeps a valid descriptor alive for the whole call.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EAGAIN) {
        return Ok(false);
    }
    Err(OutputStoreError::Io(format!(
        "cannot acquire the exclusive output lease: {error}"
    )))
}

#[cfg(unix)]
fn flock_try_shared(file: &File) -> Result<bool, OutputStoreError> {
    use std::os::unix::io::AsRawFd;
    // SAFETY: `file` keeps a valid descriptor alive for the whole call.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Ok(false);
    }
    Err(OutputStoreError::Io(format!(
        "cannot acquire the shared output lease: {error}"
    )))
}

#[cfg(not(unix))]
fn flock_try_shared(_file: &File) -> Result<bool, OutputStoreError> {
    // Non-Unix platforms have no cleanup lease either, so nothing can conflict.
    Ok(true)
}

#[cfg(unix)]
fn flock_index(file: &File) -> Result<(), OutputStoreError> {
    use std::os::unix::io::AsRawFd;
    loop {
        // SAFETY: `file` keeps a valid descriptor alive for the whole call.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(OutputStoreError::Io(format!(
            "cannot lock the output index: {error}"
        )));
    }
}

#[cfg(not(unix))]
fn flock_index(_file: &File) -> Result<(), OutputStoreError> {
    Ok(())
}

/// 一次捕获的事务守卫：`commit` 之前 `Drop` 会回滚已登记文件。
///
/// `src/shell.rs` 的生产捕获在单次 supervised 命令内共享自己的
/// `CaptureSession`；这个类型保留给需要显式事务句柄的调用方，回滚同样同时更新输出索引。
pub struct CaptureTransaction<'a> {
    store: &'a OutputStore,
    lease: CaptureLease,
    /// 已成功登记、仍会在 `Drop` 时回滚的路径。
    registered: Vec<String>,
    /// 本事务新建但尚未登记成功的文件：`(工作区相对路径, 内容 sha256)`。
    ///
    /// 所有权在**写盘之前**登记，因此“写文件成功但登记失败”不会留下无人认领的
    /// 孤儿文件；只登记本事务新建的内容寻址文件，绝不认领既有文件。
    fresh: Vec<(String, String)>,
    committed: bool,
}

impl<'a> CaptureTransaction<'a> {
    /// 开启一次事务并持有 shared 捕获租约。
    pub fn begin(store: &'a OutputStore) -> Result<Self, OutputStoreError> {
        Ok(Self {
            store,
            lease: store.begin_capture()?,
            registered: Vec::new(),
            fresh: Vec::new(),
            committed: false,
        })
    }

    /// 记录一条 durable `Pending` 引用发布意图（必须在真实 Host 回调之前）。
    pub fn begin_publication_intent(
        &mut self,
        run_id: &str,
        call_id: Option<&str>,
        references: &[OutputReference],
    ) -> Result<PublicationIntent, OutputStoreError> {
        self.store
            .begin_publication_intent(&self.lease, run_id, call_id, references)
    }

    /// Host 回调返回 `Err`：保持不确定并保护引用，直到显式 Host 对账。
    pub fn mark_publication_uncertain(
        &mut self,
        intent: &PublicationIntent,
    ) -> Result<Option<PublicationIntent>, OutputStoreError> {
        self.store
            .mark_publication_uncertain(&self.lease, &intent.attempt_id, &intent.snapshot)
    }

    /// 注销一条已对账的意图。
    pub fn resolve_publication_intent(
        &mut self,
        intent: &PublicationIntent,
        resolution: PublicationResolution,
    ) -> Result<Option<PublicationIntent>, OutputStoreError> {
        self.store.resolve_publication_intent(
            &self.lease,
            &intent.attempt_id,
            &intent.snapshot,
            resolution,
        )
    }

    /// 在写盘/登记之前登记本事务即将创建的文件所有权。
    ///
    /// 只对**本事务新建**的内容寻址文件调用；写入时发现文件已存在（另一写入者先
    /// 发布了相同内容）应改用 [`CaptureTransaction::forget_fresh`] 撤销认领。
    pub fn enroll_fresh(&mut self, path: impl Into<String>, sha256: impl Into<String>) {
        self.fresh.push((path.into(), sha256.into()));
    }

    /// 撤销一个尚未登记的新建文件认领（文件本来已存在）。
    pub fn forget_fresh(&mut self, path: &str) {
        self.fresh.retain(|(candidate, _)| candidate != path);
    }

    /// 登记一个已发布文件；未 `commit`/`keep` 前失败会回滚它。
    ///
    /// 登记失败时会立即尝试删除本事务新建、且未被任何索引条目认领的文件，
    /// 避免留下未登记孤儿。
    pub fn register(&mut self, entry: OutputEntry) -> Result<OutputEntry, OutputStoreError> {
        let path = entry.path.clone();
        let fresh_sha = entry.hash.clone();
        match self.store.register(&self.lease, entry) {
            Ok(stored) => {
                self.registered.push(stored.path.clone());
                self.fresh
                    .retain(|(candidate, _)| candidate != &stored.path);
                Ok(stored)
            }
            Err(error) => {
                self.rollback_fresh(&path, fresh_sha.as_deref());
                Err(error)
            }
        }
    }

    /// 在事务自身的捕获租约下证明一个已登记文件真实存在且长度/hash 一致。
    ///
    /// 供需要复用索引/缓存条目的调用方在发布引用前调用；失败即拒绝发布陈旧引用。
    pub fn verify(
        &self,
        path: &str,
        expected_bytes: u64,
        expected_sha256: &str,
    ) -> Result<(), OutputStoreError> {
        self.store
            .verify_published_file(&self.lease, path, expected_bytes, expected_sha256)
    }

    /// 保留已经由**成功持久化的 canonical 记录**真正引用的路径。
    ///
    /// 同一事务里更晚的调用失败时，这些文件不能被回滚：Host 回调可能已经部分持久化，
    /// 保守保留有真实证据者，只有尚未发布/尚未被引用的新建文件才回滚。
    pub fn keep(&mut self, paths: &[String]) {
        self.registered
            .retain(|path| !paths.iter().any(|kept| kept == path));
        self.fresh
            .retain(|(path, _)| !paths.iter().any(|kept| kept == path));
    }

    /// 提交事务：已登记文件保留，不再回滚。
    pub fn commit(mut self) {
        self.committed = true;
    }

    fn rollback_fresh(&mut self, path: &str, sha256: Option<&str>) {
        // Prefer the hash enrolled before the write: a declaration that was
        // itself wrong must not stop the rollback of this transaction's file.
        let sha256 = self
            .fresh
            .iter()
            .find(|(candidate, _)| candidate == path)
            .map(|(_, sha)| sha.clone())
            .or_else(|| sha256.map(str::to_owned));
        if let Some(sha256) = sha256 {
            let _ = self.store.discard_fresh(&self.lease, path, &sha256);
        }
        self.fresh.retain(|(candidate, _)| candidate != path);
    }
}

impl Drop for CaptureTransaction<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for path in std::mem::take(&mut self.registered) {
            // 回滚是尽力而为：内容已被外部改写时 `cancel` 会拒绝删除并保留证据。
            let _ = self.store.cancel(&self.lease, &path);
        }
        for (path, sha256) in std::mem::take(&mut self.fresh) {
            // 登记失败或从未登记的**本事务新建文件**：删除它，但不碰索引里已存在的
            // 内容寻址文件（`discard_fresh` 先查索引并校验真实内容）。
            let _ = self.store.discard_fresh(&self.lease, &path, &sha256);
        }
    }
}

// ---------------------------------------------------------------------------
// 接入状态（本轮已完成生产接入；不把显式清理夸大为自动清理）
// ---------------------------------------------------------------------------
//
// 已接入：
//
// 0. 模块注册：`src/shell.rs` 声明 `#[path = "output_store.rs"] pub mod output_store;`
//    与 `#[path = "workspace_activity.rs"] pub mod workspace_activity;`（未改
//    `lib.rs` 导出），因此本模块现在是生产库的一部分；它引用 crate 自身的
//    `crate::session::Session` 与 `crate::journal` 的 `JournalFile` / `RunState` /
//    `OpStatus` / `truncate` 上限常量，而不是镜像 schema。`tests/output_cleanup.rs`
//    与 `tests/output_lifecycle.rs` 用 `#[path]` 直接编译这两个文件，并用
//    `use mio_coding_agent::{journal, session};` 让同一个 `crate::` 路径解析到被测库。
// 1. 捕获：`src/shell.rs` 的 `capture_transactional` 在首次越过 32 KiB 时懒初始化
//    `OutputStore` 与 shared `CaptureLease`，stdout/stderr 共享同一个
//    `CaptureSession`；`persist_noclobber` 后先纳入回滚、再 `register`（`bytes` 用
//    `total`，`hash` 由磁盘内容补齐，`call_id` 保持 `null`），双流都成功后 `commit()`。
//    任何登记/索引写/取消失败或取消都会回滚未提交的新文件与索引项，不删除既有引用。
//    `externalize_output`（MCP 外置结果）走同一事务路径。
// 2. 引用写入保护：`Session::save` 与 `src/journal.rs::persist` 在从快照/候选到落盘
//    整个区间持有 `OutputStore::guard_existing` 的 shared activity + shared cleanup
//    非阻塞租约；`.mio/outputs` 不存在时不创建 `.mio`。清理的 exclusive 租约与它们互斥。
// 3. run 生命周期：`src/workspace.rs::Workspace::new` **无论** `.mio/outputs` 是否
//    已存在都持有项目外的同一 shared activity 租约直到该 run 的 `Workspace` 被丢弃，
//    因此清理在整个活动 run 期间都以 `Busy` 拒绝，包括「run 构造之后才发生第一次
//    capture」的缺口。宿主若不经过 `Workspace`，必须自己持有公开的
//    `workspace_activity::shared_for_workspace`。
// 4. 外部 session 来源：`Session::save` 在目标不在 `.mio/sessions` / `.mio/journal`
//    时通过 `register_external_reference_source` 登记来源；`scan_registered_sources`
//    扫描工作区内来源，对工作区外来源不读宿主文件而是整体拒绝清理。
// 5. CLI：`src/bin/mio.rs` 已接线 `src/bin/mio_cli/output_cleanup.rs` 的
//    `--output-cleanup-plan` / `--output-cleanup-confirm`，默认不删除、无需 Provider。
// 6. 部分状态（已如实记录，可恢复）：删除与索引更新不是一次原子事务；任何中断只
//    会留下“索引里多出已不存在的条目”，下一次 `plan_cleanup` 把它当作 `pruned_missing`
//    回收。删除失败或索引写失败时返回 `OutputStoreError::PartialCleanup`，其中携带
//    `CleanupReport`（`failed` 条目、成功删除的条目、未更新的索引说明），失败条目的
//    索引记录被保留，修好原因后可以直接重新计划重试。
// 7. 引用发布意图账本（DES-17）：`.mio/output-index/publication-intents.json` 在
//    文件/index 登记之后、真实 Host 回调之前写入 durable `Pending` 意图。Host 成功
//    后注销意图并把引用移交常规 journal/session 扫描；Host `Err` 则保持
//    `PendingUncertain`，未决意图的 typed 引用在清理扫描里精确保留（保留期不会静默
//    GC），只能由 `reconcile_publication_intent` 显式对账。账本损坏/版本不符/超限
//    fail-closed；读取路径不创建任何目录或文件（`open_readonly`）。
//
// 仍然保留的边界（不声称已消除）：
//
// - 非协作的外部写入者（例如直接编辑 `.mio/sessions/*.json` 的第三方进程）不持有
//   shared 租约，不在协作保证内；`CONCURRENCY_WARNING` 会把这一点写进计划与报告。
// - 本模块的删除语义只在显式 `confirm_cleanup` 时生效；接入点没有后台自动清理。
