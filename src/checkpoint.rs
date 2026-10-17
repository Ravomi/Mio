//! 可选文件检查点与 rewind（REQ-RECOVERY 深化）。
//!
//! 目标：为 `write` / `edit` 提供可选的 pre-edit 快照，并允许随后回退单个文件。
//! 设计约束：
//!
//! - **默认关闭**：只有调用方显式把 [`CheckpointStore`] 接到 `Workspace` 上才会快照；
//!   未启用时 `write` / `edit` 行为与本模块引入前完全一致，也不产生任何磁盘写入。
//! - **工作区作用域**：索引与快照都落在 `<workspace>/.mio/checkpoints/`，不跨工作区。
//! - **写入前落盘**：`record_pre_edit` 在文件真正被替换前持久化原内容、原身份与
//!   「操作后指纹」；写入失败时返回 `Err`，调用方必须放弃这次文件写入。
//! - **rewind 先验证**：只有当前内容与快照记录的「操作后指纹」一致才回退，否则拒绝
//!   覆盖用户后续修改；由本模块创建的文件仅在内容仍匹配时删除。
//! - **不碰 Git 历史，不回滚 shell 副作用**：只恢复普通文件内容，不执行任何命令。
//! - **损坏 fail-closed**：索引损坏、版本不符或落点异常时拒绝新快照与 rewind，并保留
//!   磁盘上的原始证据。
//! - **私有权限与原子落盘**：目录 0700、文件 0600，`NamedTempFile` 原子替换，并在
//!   rename 后 `fsync` 目录项。
//! - **跨进程索引事务**：所有会改动索引的操作（`record_pre_edit` / `confirm` /
//!   `mark_failed` / `rewind`）都在同目录 `index.lock` 的独占 `flock` 内执行
//!   「重新加载 -> 合并 -> 原子落盘」，因此多个进程各自创建快照不会互相覆盖索引条目。
//!   锁只保护索引事务本身，不构成文件内容的事务，也不能替代工作区写租约。
//!
//! 与执行平面的一致性：快照只在 `Workspace` 的 `write` / `edit` 路径内、通过工具过滤、
//! 只读检查、工作区租约与统一审批之后发生。rewind 是宿主/用户侧的维护操作，不是模型
//! 可调用的工具；宿主应通过 `Workspace::rewind_checkpoint` 调用它，以便与 `write` /
//! `edit` 共用同一套租约、只读检查与审批。直接调用 [`CheckpointStore::rewind`] 只持有
//! 索引 `flock`，不参与工作区租约，因此不能替代合作进程间的写互斥。工作区路径校验
//! （拒绝 `..` 越界与符号链接）由 `workspace::checked_path` 统一提供。

use crate::workspace::{MAX_WRITE_BYTES, checked_path};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use tempfile::NamedTempFile;

/// 当前检查点索引格式版本。读取到不同版本时视为不可信证据。
pub const CHECKPOINT_VERSION: u32 = 1;
/// 相对于工作区根目录的检查点目录。
pub const CHECKPOINT_DIR: &str = ".mio/checkpoints";
/// 单个 pre-edit 快照的字节上限，与 `write` / `edit` 单次写入上限一致。
pub const MAX_CHECKPOINT_SNAPSHOT_BYTES: usize = MAX_WRITE_BYTES;
/// 索引文件的字节上限。超出时丢弃最旧检查点直到放得下。
pub const MAX_CHECKPOINT_INDEX_BYTES: usize = 512 * 1024;
/// 保留的最大检查点数量，超出时丢弃最旧记录。
pub const MAX_CHECKPOINT_ENTRIES: usize = 512;
/// 单个目标路径字段的最大字符数。超过时拒绝快照，而不是截断路径。
pub const MAX_CHECKPOINT_TARGET_CHARS: usize = 1024;
/// `rewind` 选择器允许的最短 ID 前缀长度。
pub const MIN_CHECKPOINT_ID_PREFIX: usize = 4;
/// 与索引同目录的跨进程索引锁文件名。
pub const CHECKPOINT_LOCK_FILE: &str = "index.lock";

/// 检查点状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointState {
    /// 已持久化快照，但对应写入尚未确认结果。
    Pending,
    /// 写入结果已确认，可通过 `rewind` 回退。
    Active,
    /// 快照后写入失败，或写入结果与预期指纹不符；拒绝 `rewind`。
    Failed,
    /// 已执行过 `rewind`，不重复回退。
    Rewound,
}

/// 文件身份：设备号、inode、权限位与长度。
///
/// 非 Unix 平台没有可移植的 dev/inode，两个字段记为 0；此时只有长度与权限位可用。
/// 身份用于审计与回退时恢复权限位，`rewind` 的一致性判定以内容指纹为准。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileIdentity {
    /// 设备号（非 Unix 为 0）。
    pub dev: u64,
    /// inode（非 Unix 为 0）。
    pub ino: u64,
    /// 权限位（非 Unix 为只读推导出的近似值）。
    pub mode: u32,
    /// 文件长度（字节）。
    pub len: u64,
}

/// 一条 pre-edit 检查点。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointEntry {
    /// 稳定标识；`rewind` 接受完整 ID 或唯一前缀。
    pub id: String,
    /// 触发快照的写操作标签（`write` / `edit`）。
    pub operation: String,
    /// 工作区相对路径，原样保存以便回退时复用同一套路径校验。
    pub target: String,
    /// 快照时间（Unix 秒）。
    pub created: u64,
    /// 写入前目标是否存在（`false` 表示这次操作创建了文件）。
    pub existed: bool,
    /// 写入前内容长度（不存在时省略）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_len: Option<u64>,
    /// 写入前内容 SHA-256（不存在时省略）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_sha256: Option<String>,
    /// 写入前内容的快照文件名（相对检查点目录）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_file: Option<String>,
    /// 写入前文件身份（不存在时省略）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_identity: Option<FileIdentity>,
    /// 写入后内容长度（预期的操作后状态）。
    pub after_len: u64,
    /// 写入后内容 SHA-256（预期的操作后指纹）。
    pub after_sha256: String,
    /// 写入后文件身份，仅在确认写入成功后记录，用于审计。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_identity: Option<FileIdentity>,
    /// 当前状态。
    pub state: CheckpointState,
    /// 回退时间（Unix 秒）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewound_at: Option<u64>,
}

/// 索引文件结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    updated: u64,
    #[serde(default)]
    dropped: u64,
    entries: Vec<CheckpointEntry>,
}

impl IndexFile {
    fn fresh() -> Self {
        Self {
            version: CHECKPOINT_VERSION,
            updated: now_secs(),
            dropped: 0,
            entries: Vec::new(),
        }
    }
}

/// `rewind` 实际执行的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RewindAction {
    /// 已把原内容写回目标文件。
    Restored,
    /// 目标文件由该次操作创建，且仍与预期一致，已删除。
    Deleted,
}

/// 一次成功 `rewind` 的结果。
#[derive(Debug, Clone, Serialize)]
pub struct RewindOutcome {
    /// 被回退的检查点 ID。
    pub id: String,
    /// 工作区相对路径。
    pub target: String,
    /// 实际动作。
    pub action: RewindAction,
}

#[derive(Debug)]
struct StoreData {
    root: PathBuf,
    index: IndexFile,
    /// 首次加载（或最近一次重载）时的诊断；存在时所有写入与回退都 fail-closed。
    load_error: Option<String>,
}

/// 工作区级文件检查点存储。
///
/// `inner == None` 表示禁用（`CheckpointStore::disabled()`，默认）：所有写入都是空操作，
/// 不会创建任何文件。
#[derive(Debug)]
pub struct CheckpointStore {
    inner: Mutex<Option<StoreData>>,
}

impl CheckpointStore {
    /// 默认关闭：不创建、不写入任何文件。
    pub fn disabled() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    /// 打开工作区级检查点存储。
    ///
    /// 索引损坏、版本不符或 `.mio/checkpoints` 不是真实目录时不会失败，而是记录诊断，
    /// 后续 `record_pre_edit` / `rewind` / `list` 都会返回 `Err`（fail-closed），并保留
    /// 磁盘上的原始证据。
    pub fn open(root: impl AsRef<Path>) -> Result<Self, String> {
        let root = root
            .as_ref()
            .canonicalize()
            .map_err(|error| format!("检查点工作区不可用: {error}"))?;
        if !root.is_dir() {
            return Err("检查点工作区不是目录".into());
        }
        let (index, load_error) = match store_dir(&root, false) {
            Ok(_) => load_index(&index_path(&root)),
            Err(error) => (IndexFile::fresh(), Some(error)),
        };
        Ok(Self {
            inner: Mutex::new(Some(StoreData {
                root,
                index,
                load_error,
            })),
        })
    }

    /// 是否启用持久化。
    pub fn is_enabled(&self) -> bool {
        self.inner.lock().unwrap().is_some()
    }

    /// 检查点目录（禁用时为 `None`）。
    pub fn dir(&self) -> Option<PathBuf> {
        self.inner
            .lock()
            .unwrap()
            .as_ref()
            .map(|data| data.root.join(CHECKPOINT_DIR))
    }

    /// 工作区根目录（禁用时为 `None`）。
    pub fn root(&self) -> Option<PathBuf> {
        self.inner
            .lock()
            .unwrap()
            .as_ref()
            .map(|data| data.root.clone())
    }

    /// 在写操作真正落盘前持久化 pre-edit 快照。
    ///
    /// `before` 是写入前的原文件内容（目标不存在时为 `None`），`after` 是本次写入预期
    /// 产生的内容。任何失败都返回 `Err`，调用方必须放弃写入。`Ok(None)` 表示存储已禁用。
    pub fn record_pre_edit(
        &self,
        operation: &str,
        target: &str,
        before: Option<&[u8]>,
        after: &[u8],
    ) -> Result<Option<String>, String> {
        let mut guard = self.inner.lock().unwrap();
        let Some(data) = guard.as_mut() else {
            return Ok(None);
        };
        // 先在锁外做不落盘的校验：被拒绝的快照不会创建检查点目录。
        let prepared = prepare_entry(&data.root, operation, target, before, after)?;
        // 索引是跨进程共享状态：从重新加载到原子落盘必须整体持有 flock，否则两个
        // 进程各自「读 -> 改 -> 写」会互相覆盖条目。
        let _index_lock = lock_index(&data.root)?;
        reload(data)?;
        let id = new_id();
        let mut entry = CheckpointEntry {
            id: id.clone(),
            operation: operation.to_owned(),
            target: target.to_owned(),
            created: now_secs(),
            existed: false,
            before_len: None,
            before_sha256: None,
            before_file: None,
            before_identity: None,
            after_len: after.len() as u64,
            after_sha256: content_fingerprint(after),
            after_identity: None,
            state: CheckpointState::Pending,
            rewound_at: None,
        };
        if let Some(bytes) = prepared.before {
            let name = format!("{id}.before");
            write_snapshot(data, &name, bytes)?;
            entry.existed = true;
            entry.before_len = Some(bytes.len() as u64);
            entry.before_sha256 = Some(content_fingerprint(bytes));
            entry.before_file = Some(name);
            entry.before_identity = prepared.before_identity;
        }
        let before_file = entry.before_file.clone();
        let mut candidate = data.index.clone();
        candidate.updated = now_secs();
        candidate.entries.push(entry);
        match persist(data, &candidate) {
            Ok(enforced) => {
                data.index = enforced;
                Ok(Some(id))
            }
            Err(error) => {
                if let Some(name) = &before_file {
                    remove_snapshot(&data.root, name);
                }
                Err(error)
            }
        }
    }

    /// 确认一次写入已按预期落盘，使检查点可用于 `rewind`。
    ///
    /// 确认时重新读取目标内容；与预期指纹不符（外部写入者介入）时把检查点标记为
    /// `Failed` 并返回 `Err`。已确认或已失效的检查点保持原状态。
    pub fn confirm(&self, id: &str) -> Result<(), String> {
        let mut guard = self.inner.lock().unwrap();
        let Some(data) = guard.as_mut() else {
            return Ok(());
        };
        let _index_lock = lock_index(&data.root)?;
        reload(data)?;
        let Some(position) = data.index.entries.iter().position(|entry| entry.id == id) else {
            return Ok(());
        };
        if data.index.entries[position].state != CheckpointState::Pending {
            return Ok(());
        }
        let entry = data.index.entries[position].clone();
        let path = checked_path(&data.root, &entry.target, true)?;
        let metadata =
            fs::symlink_metadata(&path).map_err(|error| format!("检查点确认失败: {error}"))?;
        let current = read_bounded(&path).map_err(|error| format!("检查点确认失败: {error}"))?;
        let mut candidate = data.index.clone();
        let confirmed = metadata.is_file()
            && !metadata.file_type().is_symlink()
            && content_fingerprint(&current) == entry.after_sha256;
        if !confirmed {
            if let Some(slot) = candidate.entries.get_mut(position) {
                slot.state = CheckpointState::Failed;
            }
            candidate.updated = now_secs();
            if let Ok(enforced) = persist(data, &candidate) {
                data.index = enforced;
            }
            return Err("文件写入结果与检查点记录的指纹不一致，已标记检查点失败".into());
        }
        if let Some(slot) = candidate.entries.get_mut(position) {
            slot.state = CheckpointState::Active;
            slot.after_identity = Some(identity_of(&metadata));
        }
        candidate.updated = now_secs();
        data.index = persist(data, &candidate)?;
        Ok(())
    }

    /// 写入失败时把仍在 `Pending` 的检查点标记为 `Failed`，避免它被误用于回退。
    pub fn mark_failed(&self, id: &str) -> Result<(), String> {
        let mut guard = self.inner.lock().unwrap();
        let Some(data) = guard.as_mut() else {
            return Ok(());
        };
        let _index_lock = lock_index(&data.root)?;
        reload(data)?;
        let Some(position) = data.index.entries.iter().position(|entry| entry.id == id) else {
            return Ok(());
        };
        let mut candidate = data.index.clone();
        if let Some(slot) = candidate.entries.get_mut(position)
            && slot.state == CheckpointState::Pending
        {
            slot.state = CheckpointState::Failed;
        }
        candidate.updated = now_secs();
        data.index = persist(data, &candidate)?;
        Ok(())
    }

    /// 列出工作区检查点（最旧在前）。索引不可用时返回 `Err`。
    pub fn list(&self) -> Result<Vec<CheckpointEntry>, String> {
        let mut guard = self.inner.lock().unwrap();
        let Some(data) = guard.as_mut() else {
            return Ok(Vec::new());
        };
        reload(data)?;
        Ok(data.index.entries.clone())
    }

    /// 回退到某个检查点记录的 pre-edit 状态（**宿主维护 API，不是原子 CAS**）。
    ///
    /// 调用方并发约束：
    ///
    /// - 它持有本工作区索引的独占 `flock`，覆盖「重新加载 -> 状态校验 -> 恢复目标 ->
    ///   落盘索引」整个过程，因此同一工作区内的其它 `CheckpointStore` 操作（含其它
    ///   进程的 `record_pre_edit` / `confirm` / `rewind`）会串行执行，不会有两个调用方
    ///   同时回退同一个检查点。
    /// - 它**不**获取 `Workspace` 的写租约，也不参与 `SharedLimits` 路径租约。宿主要与
    ///   `write` / `edit` 及合作进程互斥，必须通过
    ///   [`crate::workspace::Workspace::rewind_checkpoint`] 调用，而不是直接调用本方法。
    /// - 它只按内容指纹做乐观拒绝：内容预检与最终 rename 之间存在窗口，非协作的外部
    ///   写入者仍可在窗口内改动目标；本方法不保证能阻止这类竞态，也不是 CAS。
    ///
    /// 只有当前内容与「操作后指纹」一致时才执行，否则拒绝覆盖用户后续修改。由检查点
    /// 创建的文件仅在内容仍匹配时删除。不触碰 Git 历史，也不回滚任何 shell 副作用。
    pub fn rewind(&self, selector: &str) -> Result<RewindOutcome, String> {
        let mut guard = self.inner.lock().unwrap();
        let Some(data) = guard.as_mut() else {
            return Err("文件检查点已禁用，无法 rewind".into());
        };
        // 索引事务与目标恢复一起放在锁内，避免两个进程重复回退同一检查点。
        let _index_lock = lock_index(&data.root)?;
        reload(data)?;
        let selector = selector.trim();
        if selector.is_empty() {
            return Err("rewind 需要检查点 ID 或唯一前缀".into());
        }
        let position = resolve_selector(&data.index, selector)?;
        let entry = data.index.entries[position].clone();
        match entry.state {
            CheckpointState::Active => {}
            CheckpointState::Pending => {
                return Err(format!("检查点 {} 尚未确认写入结果，拒绝回退", entry.id));
            }
            CheckpointState::Failed => {
                return Err(format!("检查点 {} 已标记失败，拒绝回退", entry.id));
            }
            CheckpointState::Rewound => {
                return Err(format!("检查点 {} 已经回退过", entry.id));
            }
        }
        let path = checked_path(&data.root, &entry.target, true)
            .map_err(|error| format!("rewind 拒绝目标路径：{error}"))?;
        let outcome = apply_rewind(data, &entry, &path)?;
        let mut candidate = data.index.clone();
        if let Some(slot) = candidate.entries.get_mut(position) {
            slot.state = CheckpointState::Rewound;
            slot.rewound_at = Some(now_secs());
        }
        candidate.updated = now_secs();
        data.index = persist(data, &candidate)?;
        Ok(outcome)
    }
}

/// 内容指纹：SHA-256 的十六进制小写形式。
pub fn content_fingerprint(bytes: &[u8]) -> String {
    sha256_hex(bytes)
}

/// `record_pre_edit` 在落盘前的纯校验结果：原文件字节与其身份。
struct PreparedCheckpoint<'a> {
    before: Option<&'a [u8]>,
    before_identity: Option<FileIdentity>,
}

/// 校验一次 pre-edit 快照的入参与目标文件，但不写任何文件。
///
/// 所有拒绝都在此发生，因此被拒绝的快照不会创建 `.mio/checkpoints` 目录。快照文件
/// 与索引写入由 [`CheckpointStore::record_pre_edit`] 在索引锁内完成。
fn prepare_entry<'a>(
    root: &Path,
    operation: &str,
    target: &str,
    before: Option<&'a [u8]>,
    after: &[u8],
) -> Result<PreparedCheckpoint<'a>, String> {
    if !matches!(operation, "write" | "edit") {
        return Err(format!("检查点只覆盖 write/edit，收到 '{operation}'"));
    }
    if target.trim().is_empty() {
        return Err("检查点目标不能为空".into());
    }
    if target.chars().count() > MAX_CHECKPOINT_TARGET_CHARS {
        return Err(format!(
            "检查点目标路径超过 {MAX_CHECKPOINT_TARGET_CHARS} 字符上限"
        ));
    }
    if after.len() > MAX_CHECKPOINT_SNAPSHOT_BYTES {
        return Err(format!(
            "写入内容超过检查点快照上限 ({} 字节)",
            MAX_CHECKPOINT_SNAPSHOT_BYTES
        ));
    }
    let path = checked_path(root, target, true)?;
    if path.starts_with(root.join(CHECKPOINT_DIR)) {
        return Err("不允许对检查点存储目录内的文件建立检查点".into());
    }
    let Some(before) = before else {
        return Ok(PreparedCheckpoint {
            before: None,
            before_identity: None,
        });
    };
    if before.len() > MAX_CHECKPOINT_SNAPSHOT_BYTES {
        return Err(format!(
            "文件超过检查点快照上限 ({} 字节)，已拒绝在启用检查点时覆盖",
            MAX_CHECKPOINT_SNAPSHOT_BYTES
        ));
    }
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("检查点无法读取目标元数据: {error}"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("检查点只支持普通文件目标".into());
    }
    // 与调用方刚读到的内容再比对一次：读取到快照之间的变化同样拒绝写入。
    let current = read_bounded(&path)?;
    if current != before {
        return Err("检查点目标在读取后发生变化，已拒绝写入".into());
    }
    Ok(PreparedCheckpoint {
        before: Some(before),
        before_identity: Some(identity_of(&metadata)),
    })
}

fn apply_rewind(
    data: &StoreData,
    entry: &CheckpointEntry,
    path: &Path,
) -> Result<RewindOutcome, String> {
    let conflict = || {
        format!(
            "rewind 拒绝覆盖：{} 当前内容与检查点 {} 记录的操作后状态不一致（可能已被用户或其它进程修改）",
            entry.target, entry.id
        )
    };
    let current = match read_bounded_opt(path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            if entry.existed {
                return Err(format!("rewind 拒绝：{} 已不存在于工作区", entry.target));
            }
            // 由本次操作创建的文件已经不存在，删除动作无事可做。
            return Ok(RewindOutcome {
                id: entry.id.clone(),
                target: entry.target.clone(),
                action: RewindAction::Deleted,
            });
        }
        Err(error) => return Err(format!("rewind 无法读取目标：{error}")),
    };
    if content_fingerprint(&current) != entry.after_sha256 {
        return Err(conflict());
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("rewind 无法读取目标元数据: {error}"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("rewind 只支持普通文件目标".into());
    }
    if !entry.existed {
        fs::remove_file(path).map_err(|error| format!("rewind 删除创建的文件失败: {error}"))?;
        return Ok(RewindOutcome {
            id: entry.id.clone(),
            target: entry.target.clone(),
            action: RewindAction::Deleted,
        });
    }
    let name = entry.before_file.as_deref().ok_or("检查点缺少原文件快照")?;
    let before = read_snapshot(data, name)?;
    let digest = content_fingerprint(&before);
    if entry.before_len != Some(before.len() as u64)
        || entry.before_sha256.as_deref() != Some(digest.as_str())
    {
        return Err(format!("检查点 {} 的原文件快照已损坏，拒绝回退", entry.id));
    }
    restore_atomically(path, &before, entry.before_identity, &metadata)?;
    Ok(RewindOutcome {
        id: entry.id.clone(),
        target: entry.target.clone(),
        action: RewindAction::Restored,
    })
}

fn restore_atomically(
    path: &Path,
    bytes: &[u8],
    identity: Option<FileIdentity>,
    current: &fs::Metadata,
) -> Result<(), String> {
    let parent = path.parent().ok_or("无效父目录")?;
    let temp = NamedTempFile::new_in(parent)
        .map_err(|error| format!("rewind 创建临时文件失败: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = identity
            .map(|value| value.mode)
            .unwrap_or_else(|| current.permissions().mode());
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(mode & 0o7777))
            .map_err(|error| format!("rewind 设置权限失败: {error}"))?;
    }
    #[cfg(not(unix))]
    {
        let _ = identity;
        fs::set_permissions(temp.path(), current.permissions())
            .map_err(|error| format!("rewind 设置权限失败: {error}"))?;
    }
    fs::write(temp.path(), bytes).map_err(|error| format!("rewind 写入失败: {error}"))?;
    temp.as_file()
        .sync_all()
        .map_err(|error| format!("rewind 同步失败: {error}"))?;
    temp.persist(path)
        .map_err(|error| format!("rewind 原子替换失败: {error}"))?;
    Ok(())
}

fn resolve_selector(index: &IndexFile, selector: &str) -> Result<usize, String> {
    if let Some(position) = index.entries.iter().position(|entry| entry.id == selector) {
        return Ok(position);
    }
    if selector.chars().count() < MIN_CHECKPOINT_ID_PREFIX {
        return Err(format!(
            "检查点前缀至少需要 {MIN_CHECKPOINT_ID_PREFIX} 个字符"
        ));
    }
    let matches: Vec<usize> = index
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.id.starts_with(selector))
        .map(|(position, _)| position)
        .collect();
    match matches.len() {
        0 => Err(format!("未找到检查点 '{selector}'")),
        1 => Ok(matches[0]),
        count => Err(format!(
            "检查点前缀 '{selector}' 匹配 {count} 个检查点，请提供更长的 ID"
        )),
    }
}

fn reload(data: &mut StoreData) -> Result<(), String> {
    let (index, load_error) = match store_dir(&data.root, false) {
        Ok(_) => load_index(&index_path(&data.root)),
        Err(error) => (IndexFile::fresh(), Some(error)),
    };
    data.index = index;
    data.load_error = load_error;
    match &data.load_error {
        Some(error) => Err(format!("文件检查点索引不可用，已保留原证据：{error}")),
        None => Ok(()),
    }
}

fn read_index_bytes(path: &Path) -> io::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_CHECKPOINT_INDEX_BYTES as u64
    {
        return Err(io::Error::other(
            "checkpoint index must be a regular file within its size limit",
        ));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take((MAX_CHECKPOINT_INDEX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn load_index(path: &Path) -> (IndexFile, Option<String>) {
    match read_index_bytes(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => (IndexFile::fresh(), None),
        Err(error) => (
            IndexFile::fresh(),
            Some(format!("无法读取检查点索引 {}: {error}", path.display())),
        ),
        Ok(bytes) if bytes.len() > MAX_CHECKPOINT_INDEX_BYTES => (
            IndexFile::fresh(),
            Some(format!(
                "检查点索引 {} 超过 {MAX_CHECKPOINT_INDEX_BYTES} 字节",
                path.display()
            )),
        ),
        Ok(bytes) => match serde_json::from_slice::<IndexFile>(&bytes) {
            Ok(index) if index.version == CHECKPOINT_VERSION => (index, None),
            Ok(index) => (
                IndexFile::fresh(),
                Some(format!(
                    "不支持的检查点索引版本 {} (期望 {CHECKPOINT_VERSION})",
                    index.version
                )),
            ),
            Err(error) => (
                IndexFile::fresh(),
                Some(format!("检查点索引 {} 损坏: {error}", path.display())),
            ),
        },
    }
}

fn persist(data: &StoreData, candidate: &IndexFile) -> Result<IndexFile, String> {
    if let Some(error) = &data.load_error {
        return Err(format!("文件检查点索引需要人工检查，已保留原证据：{error}"));
    }
    let dir = store_dir(&data.root, true)?;
    set_private_dir(&dir)?;

    let mut enforced = candidate.clone();
    let mut orphaned: Vec<String> = Vec::new();
    while enforced.entries.len() > MAX_CHECKPOINT_ENTRIES {
        let removed = enforced.entries.remove(0);
        enforced.dropped += 1;
        if let Some(name) = removed.before_file {
            orphaned.push(name);
        }
    }
    let mut payload = serde_json::to_vec_pretty(&enforced)
        .map_err(|error| format!("无法序列化检查点索引: {error}"))?;
    while payload.len() > MAX_CHECKPOINT_INDEX_BYTES && enforced.entries.len() > 1 {
        let removed = enforced.entries.remove(0);
        enforced.dropped += 1;
        if let Some(name) = removed.before_file {
            orphaned.push(name);
        }
        payload = serde_json::to_vec_pretty(&enforced)
            .map_err(|error| format!("无法序列化检查点索引: {error}"))?;
    }
    if payload.len() > MAX_CHECKPOINT_INDEX_BYTES {
        return Err(format!(
            "检查点索引超过 {MAX_CHECKPOINT_INDEX_BYTES} 字节，即使裁剪后仍放不下"
        ));
    }

    let path = index_path(&data.root);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err("检查点索引必须是普通文件而不是符号链接".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }

    let mut temp = NamedTempFile::new_in(&dir)
        .map_err(|error| format!("无法在 {} 创建检查点临时文件: {error}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("无法设置检查点索引权限 0600: {error}"))?;
    }
    temp.write_all(&payload)
        .map_err(|error| format!("无法写入检查点索引: {error}"))?;
    temp.as_file()
        .sync_all()
        .map_err(|error| format!("无法同步检查点索引: {error}"))?;
    temp.persist(&path)
        .map_err(|error| format!("无法落盘检查点索引 {}: {error}", path.display()))?;
    sync_dir(&dir)?;

    // 只有索引落盘成功后才清理被裁剪的快照文件。
    for name in orphaned {
        remove_snapshot(&data.root, &name);
    }
    Ok(enforced)
}

fn store_dir(root: &Path, create: bool) -> Result<PathBuf, String> {
    let mut current = root.to_path_buf();
    for part in CHECKPOINT_DIR.split('/') {
        current.push(part);
        if create {
            match fs::create_dir(&current) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(format!("无法创建检查点目录 {}: {error}", current.display()));
                }
            }
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => return Err("检查点目录必须是真实目录而不是符号链接".into()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(current),
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(current)
}

fn index_path(root: &Path) -> PathBuf {
    root.join(CHECKPOINT_DIR).join("index.json")
}

/// 取得本工作区索引的跨进程独占锁，创建检查点目录并返回持有锁的句柄。
///
/// 返回的 [`File`] 被 drop 时释放 `flock`；调用方必须把整个「重新加载 -> 合并 ->
/// 原子落盘」临界区放在它的生命周期内。锁文件与索引同目录，0700 / 0600，拒绝符号
/// 链接。它只串行化索引事务，不串行化任意用户文件的读写。
fn lock_index(root: &Path) -> Result<File, String> {
    let dir = store_dir(root, true)?;
    set_private_dir(&dir)?;
    let path = dir.join(CHECKPOINT_LOCK_FILE);
    if let Ok(metadata) = fs::symlink_metadata(&path)
        && metadata.file_type().is_symlink()
    {
        return Err("检查点索引锁不能是符号链接".into());
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(&path)
        .map_err(|error| format!("无法打开检查点索引锁 {}: {error}", path.display()))?;
    if !file
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err("检查点索引锁必须是普通文件".into());
    }
    file.lock()
        .map_err(|error| format!("无法锁定检查点索引 {}: {error}", path.display()))?;
    Ok(file)
}

/// 在索引/快照 rename 之后同步目录项，避免掉电丢掉目录更新（非 Unix 平台跳过）。
fn sync_dir(dir: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        File::open(dir)
            .and_then(|file| file.sync_all())
            .map_err(|error| format!("无法同步检查点目录 {}: {error}", dir.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

fn write_snapshot(data: &StoreData, name: &str, bytes: &[u8]) -> Result<(), String> {
    let dir = store_dir(&data.root, true)?;
    set_private_dir(&dir)?;
    let mut temp = NamedTempFile::new_in(&dir)
        .map_err(|error| format!("无法创建检查点快照临时文件: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("无法设置检查点快照权限 0600: {error}"))?;
    }
    temp.write_all(bytes)
        .map_err(|error| format!("无法写入检查点快照: {error}"))?;
    temp.as_file()
        .sync_all()
        .map_err(|error| format!("无法同步检查点快照: {error}"))?;
    temp.persist(dir.join(name))
        .map_err(|error| format!("无法落盘检查点快照: {error}"))?;
    sync_dir(&dir)?;
    Ok(())
}

fn read_snapshot(data: &StoreData, name: &str) -> Result<Vec<u8>, String> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err("检查点快照文件名非法".into());
    }
    let _ = store_dir(&data.root, false)?;
    read_bounded(&data.root.join(CHECKPOINT_DIR).join(name))
        .map_err(|error| format!("无法读取检查点快照: {error}"))
}

fn remove_snapshot(root: &Path, name: &str) {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return;
    }
    let path = root.join(CHECKPOINT_DIR).join(name);
    if let Ok(metadata) = fs::symlink_metadata(&path)
        && metadata.is_file()
        && !metadata.file_type().is_symlink()
    {
        let _ = fs::remove_file(path);
    }
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, String> {
    let mut file = fs::File::open(path).map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    // 显式取 `&mut File`：`File` 同时实现 `Read` 与 `Write`，直接 `.by_ref()` 会有歧义。
    (&mut file)
        .take((MAX_CHECKPOINT_SNAPSHOT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_CHECKPOINT_SNAPSHOT_BYTES {
        return Err(format!(
            "文件超过检查点上限 ({} 字节)",
            MAX_CHECKPOINT_SNAPSHOT_BYTES
        ));
    }
    Ok(bytes)
}

fn read_bounded_opt(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(Some(read_bounded(path)?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn identity_of(metadata: &fs::Metadata) -> FileIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        FileIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
            mode: metadata.mode(),
            len: metadata.len(),
        }
    }
    #[cfg(not(unix))]
    {
        FileIdentity {
            dev: 0,
            ino: 0,
            mode: if metadata.permissions().readonly() {
                0o444
            } else {
                0o644
            },
            len: metadata.len(),
        }
    }
}

#[cfg(unix)]
fn set_private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|error| error.to_string())
}

#[cfg(not(unix))]
fn set_private_dir(_dir: &Path) -> Result<(), String> {
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_owned()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(root: &Path) -> CheckpointStore {
        CheckpointStore::open(root).unwrap()
    }

    #[test]
    fn disabled_store_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let disabled = CheckpointStore::disabled();
        assert!(!disabled.is_enabled());
        assert!(disabled.dir().is_none());
        assert!(disabled.root().is_none());
        assert_eq!(
            disabled
                .record_pre_edit("write", "a.txt", None, b"x")
                .unwrap(),
            None
        );
        assert!(disabled.list().unwrap().is_empty());
        assert!(disabled.confirm("missing").is_ok());
        assert!(disabled.mark_failed("missing").is_ok());
        assert!(disabled.rewind("abcd").is_err());
        assert!(
            !dir.path().join(".mio").exists(),
            "a disabled store must not create files"
        );
    }

    #[test]
    fn snapshots_and_rewinds_a_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join("a.txt"), "before\n").unwrap();
        let store = store(&root);

        let id = store
            .record_pre_edit("edit", "a.txt", Some(b"before\n"), b"after\n")
            .unwrap()
            .unwrap();
        // 快照阶段不得改动目标文件。
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "before\n");
        fs::write(root.join("a.txt"), "after\n").unwrap();
        store.confirm(&id).unwrap();

        let entries = store.list().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].state, CheckpointState::Active);
        assert_eq!(entries[0].operation, "edit");
        assert!(entries[0].existed);
        let expected = content_fingerprint(b"before\n");
        assert_eq!(entries[0].before_sha256.as_deref(), Some(expected.as_str()));

        let outcome = store.rewind(&id).unwrap();
        assert_eq!(outcome.action, RewindAction::Restored);
        assert_eq!(outcome.target, "a.txt");
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "before\n");
        // 已回退的检查点不可重复回退。
        assert!(store.rewind(&id).is_err());
        assert_eq!(store.list().unwrap()[0].state, CheckpointState::Rewound);
    }

    #[test]
    fn rewind_refuses_later_manual_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join("a.txt"), "before").unwrap();
        let store = store(&root);
        let id = store
            .record_pre_edit("write", "a.txt", Some(b"before"), b"after")
            .unwrap()
            .unwrap();
        fs::write(root.join("a.txt"), "after").unwrap();
        store.confirm(&id).unwrap();

        fs::write(root.join("a.txt"), "manual").unwrap();
        let error = store.rewind(&id).unwrap_err();
        assert!(error.contains("拒绝覆盖"), "{error}");
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "manual");
        // 失败的回退不改变检查点状态，用户恢复内容后仍可回退。
        assert_eq!(store.list().unwrap()[0].state, CheckpointState::Active);
        fs::write(root.join("a.txt"), "after").unwrap();
        assert_eq!(store.rewind(&id).unwrap().action, RewindAction::Restored);
    }

    #[test]
    fn rewind_deletes_a_created_file_only_while_matching() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = store(&root);
        let id = store
            .record_pre_edit("write", "new.txt", None, b"fresh\n")
            .unwrap()
            .unwrap();
        fs::write(root.join("new.txt"), "fresh\n").unwrap();
        store.confirm(&id).unwrap();
        assert!(!store.list().unwrap()[0].existed);

        fs::write(root.join("new.txt"), "user edit\n").unwrap();
        assert!(store.rewind(&id).is_err());
        assert!(root.join("new.txt").exists());

        fs::write(root.join("new.txt"), "fresh\n").unwrap();
        assert_eq!(store.rewind(&id).unwrap().action, RewindAction::Deleted);
        assert!(!root.join("new.txt").exists());
        assert_eq!(store.list().unwrap()[0].state, CheckpointState::Rewound);
    }

    #[test]
    fn corrupt_or_unknown_index_fails_closed_and_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let checkpoint_dir = root.join(CHECKPOINT_DIR);
        fs::create_dir_all(&checkpoint_dir).unwrap();
        let index = checkpoint_dir.join("index.json");
        fs::write(&index, "{ not json").unwrap();
        fs::write(root.join("a.txt"), "before").unwrap();
        let store = store(&root);

        assert!(
            store
                .record_pre_edit("write", "a.txt", Some(b"before"), b"after")
                .is_err()
        );
        assert!(store.list().is_err());
        assert!(store.rewind("deadbeef").is_err());
        assert_eq!(fs::read_to_string(&index).unwrap(), "{ not json");
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "before");

        // 版本不符同样 fail-closed。
        fs::write(&index, r#"{"version":99,"updated":0,"entries":[]}"#).unwrap();
        assert!(
            store
                .record_pre_edit("write", "a.txt", Some(b"before"), b"after")
                .is_err()
        );
        assert!(fs::read_to_string(&index).unwrap().contains("99"));
    }

    #[test]
    fn rejects_escapes_symlinks_oversized_and_unfinished_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = store(&root);

        assert!(
            store
                .record_pre_edit("write", "../escape.txt", None, b"x")
                .is_err()
        );
        assert!(
            store
                .record_pre_edit("write", "/absolute.txt", None, b"x")
                .is_err()
        );
        assert!(store.record_pre_edit("shell", "a.txt", None, b"x").is_err());
        let long = "a".repeat(MAX_CHECKPOINT_TARGET_CHARS + 1);
        assert!(store.record_pre_edit("write", &long, None, b"x").is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", root.join("link.txt")).unwrap();
            assert!(
                store
                    .record_pre_edit("write", "link.txt", Some(b"x"), b"y")
                    .is_err()
            );
        }
        fs::write(root.join("big.bin"), vec![b'x'; 16]).unwrap();
        let oversized = vec![b'x'; MAX_CHECKPOINT_SNAPSHOT_BYTES + 1];
        assert!(
            store
                .record_pre_edit("write", "big.bin", Some(&oversized), b"y")
                .is_err()
        );
        assert!(
            !root.join(CHECKPOINT_DIR).exists(),
            "被拒绝的快照不得创建检查点目录"
        );

        // Pending（未确认）与 Failed 的检查点都拒绝回退。
        let id = store
            .record_pre_edit("write", "pending.txt", None, b"content")
            .unwrap()
            .unwrap();
        assert!(store.rewind(&id).unwrap_err().contains("尚未确认"));
        fs::write(root.join("pending.txt"), "content").unwrap();
        store.confirm(&id).unwrap();
        let failed = store
            .record_pre_edit("write", "failed.txt", None, b"value")
            .unwrap()
            .unwrap();
        fs::write(root.join("failed.txt"), "other").unwrap();
        assert!(store.confirm(&failed).is_err());
        assert!(store.rewind(&failed).unwrap_err().contains("已标记失败"));
        assert_eq!(
            fs::read_to_string(root.join("failed.txt")).unwrap(),
            "other"
        );
    }

    #[test]
    fn rewind_accepts_unique_prefix_and_rejects_ambiguity() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = store(&root);
        let mut ids = Vec::new();
        for name in ["a.txt", "b.txt"] {
            let id = store
                .record_pre_edit("write", name, None, b"created")
                .unwrap()
                .unwrap();
            fs::write(root.join(name), "created").unwrap();
            store.confirm(&id).unwrap();
            ids.push(id);
        }
        assert!(store.rewind(&ids[0][..2]).is_err(), "too short");
        let first = &ids[0];
        assert!(first.len() >= MIN_CHECKPOINT_ID_PREFIX);
        // 取首个检查点的最短唯一前缀，避免随机 ID 前缀冲突造成用例不稳定。
        let prefix_len = (MIN_CHECKPOINT_ID_PREFIX..=first.len())
            .find(|len| {
                ids.iter()
                    .filter(|id| id.starts_with(&first[..*len]))
                    .count()
                    == 1
            })
            .expect("完整 ID 自身就是一个唯一前缀");
        let outcome = store.rewind(&first[..prefix_len]).unwrap();
        assert_eq!(outcome.id, *first);
        assert!(store.rewind("zzzz").unwrap_err().contains("未找到"));
    }

    #[cfg(unix)]
    #[test]
    fn checkpoint_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join("a.txt"), "before").unwrap();
        fs::set_permissions(root.join("a.txt"), fs::Permissions::from_mode(0o640)).unwrap();
        let store = store(&root);
        let id = store
            .record_pre_edit("edit", "a.txt", Some(b"before"), b"after")
            .unwrap()
            .unwrap();
        fs::write(root.join("a.txt"), "after").unwrap();
        store.confirm(&id).unwrap();

        let checkpoint_dir = root.join(CHECKPOINT_DIR);
        let index = checkpoint_dir.join("index.json");
        let snapshot = checkpoint_dir.join(format!("{id}.before"));
        assert_eq!(
            fs::metadata(&checkpoint_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&index).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&snapshot).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(checkpoint_dir.join(CHECKPOINT_LOCK_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "the cross-process index lock must be private"
        );
        store.rewind(&id).unwrap();
        assert_eq!(
            fs::metadata(root.join("a.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640,
            "the restored file keeps its original mode"
        );
    }

    #[test]
    fn index_lock_serializes_whole_read_modify_write() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = std::sync::Arc::new(store(&root));
        // 先记录一个检查点，确保目录与锁文件已存在。
        store
            .record_pre_edit("write", "seed.txt", None, b"seed")
            .unwrap()
            .unwrap();

        // 独立打开锁文件并持锁，模拟另一个进程正处在索引事务中。
        let lock_path = root.join(CHECKPOINT_DIR).join(CHECKPOINT_LOCK_FILE);
        let held = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        held.lock().unwrap();

        let worker = {
            let store = store.clone();
            std::thread::spawn(move || {
                store.record_pre_edit("write", "blocked.txt", None, b"blocked")
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(
            !worker.is_finished(),
            "record_pre_edit must wait for the cross-process index lock"
        );
        drop(held);
        worker.join().unwrap().unwrap().unwrap();

        let entries = store.list().unwrap();
        assert_eq!(
            entries.len(),
            2,
            "both snapshots must survive the lock handoff"
        );
        assert!(entries.iter().any(|entry| entry.target == "blocked.txt"));
    }

    #[test]
    fn unrelated_workspaces_have_independent_index_locks() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let first_root = first.path().canonicalize().unwrap();
        let second_root = second.path().canonicalize().unwrap();
        let first_store = store(&first_root);
        let second_store = store(&second_root);

        first_store
            .record_pre_edit("write", "only-first.txt", None, b"a")
            .unwrap()
            .unwrap();
        second_store
            .record_pre_edit("write", "only-second.txt", None, b"b")
            .unwrap()
            .unwrap();

        let first_entries = first_store.list().unwrap();
        let second_entries = second_store.list().unwrap();
        assert_eq!(first_entries.len(), 1);
        assert_eq!(second_entries.len(), 1);
        assert_eq!(first_entries[0].target, "only-first.txt");
        assert_eq!(second_entries[0].target, "only-second.txt");
        assert!(
            first_root
                .join(CHECKPOINT_DIR)
                .join(CHECKPOINT_LOCK_FILE)
                .exists()
        );
        assert!(
            second_root
                .join(CHECKPOINT_DIR)
                .join(CHECKPOINT_LOCK_FILE)
                .exists()
        );
    }

    #[test]
    fn list_does_not_create_the_checkpoint_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = store(&root);
        assert!(store.list().unwrap().is_empty());
        assert!(
            !root.join(CHECKPOINT_DIR).exists(),
            "listing must stay read-only and not create directories"
        );
    }
}
