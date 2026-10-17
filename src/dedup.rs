//! 显式任务 key 去重账本（REQ-DEDUP 的 CLI/RPC 暴露层）。
//!
//! 目标：让 CLI `--dedup-key` 与 RPC `prompt` / `follow_up` 的 `dedup_key`
//! 共用同一套语义：同一个 key 一旦被某个任务占用（无论该任务是排队、运行中，
//! 还是已经到达终态），再次提交都不会重新执行，而是返回第一次执行时生成的
//! 稳定任务标识（`task_id`）。
//!
//! # 作用域
//!
//! 账本按**工作区根目录**（canonical `--cwd`）划分，持久化在
//! `<workspace>/.mio/dedup/task-keys.json`。因此同一工作区的不同会话、不同
//! 进程、不同次运行共享同一个 key 命名空间；不同工作区互不影响。key 由调用方
//! 自行命名，需要区分会话或调用方时请在 key 内自行加前缀。
//!
//! # 状态与重放边界
//!
//! key 记录 `queued` / `active` / `completed` / `failed` / `cancelled` /
//! `interrupted` 六种状态，**任何一种状态都不会被重新执行**：
//!
//! - 已完成的 key 是幂等凭证：重复提交只回放任务标识，不再调用模型或工具。
//! - 失败或取消的任务同样保留；要重试必须换用新的 key。
//! - 进程被强杀（SIGKILL、断电）时没有代码能改写记录，key 会停留在 `active`；
//!   它同样会阻止重放。这是刻意的保守行为，符合「崩溃后不自动重放」。
//!
//! # 并发与持久化
//!
//! 持久化账本用 Unix advisory file lock（[`std::fs::File::lock`]，即 `flock`）
//! 串行化「读取 -> 判定 -> 写入 -> 落盘」整个临界区，因此跨进程准入是原子的：
//! 同一个 key 在并发提交下只有一个调用者拿到 [`TaskClaim::Acquired`]，其余拿到
//! [`TaskClaim::Existing`]。写入使用临时文件 + rename 原子替换，目录 0700、
//! 文件 0600，并设有字节与条目上限。解析失败、版本不符、symlink、超限或满员
//! 一律 fail-closed（拒绝执行），绝不静默丢弃已有证据。
//!
//! # `--no-session`
//!
//! [`TaskKeyLedger::ephemeral`] 只在进程内保存 key，不产生任何磁盘写入，用于
//! RPC `--no-session`。CLI 的 `--dedup-key` 承诺跨运行防重放，因此与
//! `--no-session` 互斥，由 CLI 参数校验直接拒绝。
//!
//! # owner 与 liveness 观测
//!
//! [`TaskKeyLedger::claim`] 在准入时把 owner 身份元数据写入
//! [`TaskKeyRecord::owner`]：pid、进程启动时刻、host 摘要、boot 摘要与 PID
//! namespace 摘要。摘要用 `sha256` 域分隔后取定长十六进制，只用于判断“记录的
//! owner 是否仍是同一台机器、同一次启动里的同一个进程”。这些字段是**观测信息，
//! 不是安全 ACL**：不构成认证、授权或防伪承诺，也不会被用来放行任何操作。
//!
//! [`TaskKeyLedger::observe`] 在账本独占锁内做只读探测，返回记录快照与
//! [`TaskLiveness`]：
//!
//! - 记录缺少 owner（旧 version 1 记录）、owner 与本机 host 不同（共享工作区属于
//!   另一台机器）、PID namespace 不同、`/proc` 不可读或平台不支持，一律
//!   [`TaskLiveness::Unknown`]。未知不等于死亡。
//! - 只有同 host 的正向证据才判定 [`TaskLiveness::Stale`]：同 host 的 boot 已改变
//!   （原进程必然不存在）、PID 存在但启动时刻已变（PID 复用）、或同一 namespace
//!   内进程条目已消失且 `/proc` 没有隐藏策略。zombie 进程已经结束，不算存活。
//! - 终态记录不参与 owner 探测，返回 [`TaskLiveness::Unknown`]；终态本身不回退。
//!
//! host 摘要来自 `/etc/machine-id`（缺失时 `None`）；克隆的机器镜像可能共享同一
//! machine-id，这是已知残余假设。liveness 是协作式观测，不承担防伪或跨机器仲裁。
//!
//! 探测只读：不会把 active/queued 自动改写成 failed，也不会删除 key 或重放任务。
//! liveness 只是给调用方的额外观测。

use serde::{Deserialize, Serialize};
#[cfg(target_os = "linux")]
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use tempfile::NamedTempFile;

/// 账本数据格式版本。读到其它版本一律视为不可信并 fail-closed。
pub const LEDGER_VERSION: u32 = 1;
/// 相对工作区根目录的账本目录。
pub const LEDGER_DIR: &str = ".mio/dedup";
/// 账本文件名。
pub const LEDGER_FILE: &str = "task-keys.json";
/// 与账本同目录的锁文件名。
pub const LEDGER_LOCK_FILE: &str = "task-keys.lock";
/// 账本文件字节上限。
pub const MAX_LEDGER_FILE_SIZE: usize = 512 * 1024;
/// 账本最多保留的 key 数量；满员后新 key fail-closed（远低于文件字节上限）。
pub const MAX_LEDGER_ENTRIES: usize = 2048;
/// 单个 dedup key 的最大字节数。
pub const MAX_DEDUP_KEY_BYTES: usize = 512;
/// owner 身份摘要字段的固定长度（sha256 十六进制）。身份值有界，不落原始标识。
pub const MAX_OWNER_IDENTITY_CHARS: usize = 64;

/// 任务 key 的生命周期状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKeyState {
    /// 已被接受但尚未开始执行（RPC `follow_up` 排队中）。
    Queued,
    /// 正在执行，或所属进程被强杀后停留在该状态。
    Active,
    /// 正常结束。
    Completed,
    /// 以错误结束。
    Failed,
    /// 被用户取消。
    Cancelled,
    /// 进程在任务结束前退出，由运行中的进程显式记录。
    Interrupted,
}

impl TaskKeyState {
    /// 稳定的 snake_case 名称（与序列化一致，便于文本输出）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    /// 是否为终态。终态记录一律保留，不再重新执行。
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Queued | Self::Active)
    }
}

/// 账本中的单条记录。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskKeyRecord {
    /// 稳定的任务标识；dedup 命中时返回该值。
    pub task_id: String,
    /// 任务当前状态。
    pub state: TaskKeyState,
    /// 原始 key（原样保存，只做字符串相等比较）。
    pub key: String,
    /// 最后一次更新的 Unix 秒数。
    pub updated: u64,
    /// 准入时捕获的 owner 身份元数据。旧 version 1 记录没有该字段，反序列化为
    /// `None`，对应 liveness 只能是 [`TaskLiveness::Unknown`]。
    ///
    /// 该字段用于**观测** owner 是否仍存活，不是安全 ACL，不能当作认证或授权依据。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<TaskKeyOwner>,
    /// 可选 canonical 任务阶段，由权威 tracker 投影写入。旧 v1 记录没有该字段，
    /// 反序列化为 `None`；读取方保持 unknown，绝不据旧格式伪造阶段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_phase: Option<String>,
    /// 真实 engine run UUID，由 tracker 的 set-once 投影写入；旧记录为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_run_id: Option<String>,
}

/// 账本文件的持久化结构。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LedgerFile {
    /// 数据格式版本。
    pub version: u32,
    /// 最后一次更新的 Unix 秒数。
    pub updated: u64,
    /// 按准入顺序保存的记录。
    pub entries: Vec<TaskKeyRecord>,
}

/// [`TaskKeyLedger::claim`] 的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskClaim {
    /// key 首次出现：调用方获得任务标识并负责执行。
    Acquired {
        /// 新生成的任务标识。
        task_id: String,
    },
    /// key 已被占用（排队、运行中或终态）：调用方不得重新执行。
    Existing {
        /// 首次执行时生成的任务标识。
        task_id: String,
        /// 已有任务的当前状态。
        state: TaskKeyState,
    },
}

impl TaskClaim {
    /// 本次申请是否拿到了执行权。
    pub fn is_acquired(&self) -> bool {
        matches!(self, Self::Acquired { .. })
    }

    /// 对应的任务标识（无论新占还是复用）。
    pub fn task_id(&self) -> &str {
        match self {
            Self::Acquired { task_id } | Self::Existing { task_id, .. } => task_id,
        }
    }
}

/// 校验显式 dedup key 是否可用。key 是不透明字符串，只做相等比较。
pub fn validate_key(key: &str) -> Result<(), String> {
    if key.trim().is_empty() {
        return Err("dedup key must not be empty or whitespace-only".into());
    }
    if key.len() > MAX_DEDUP_KEY_BYTES {
        return Err(format!(
            "dedup key must be at most {MAX_DEDUP_KEY_BYTES} bytes"
        ));
    }
    if key.chars().any(char::is_control) {
        return Err("dedup key must not contain control characters".into());
    }
    Ok(())
}

enum Backend {
    Persistent {
        dir: PathBuf,
        file: PathBuf,
        lock: PathBuf,
    },
    Ephemeral(BTreeMap<String, TaskKeyRecord>),
}

/// 显式任务 key 账本。
///
/// 通过 [`TaskKeyLedger::persistent`] 打开按工作区持久化的账本，或通过
/// [`TaskKeyLedger::ephemeral`] 获得仅在本进程内有效的账本。两种实现共享同一套
/// 准入语义；区别只在是否落盘。
pub struct TaskKeyLedger {
    inner: Mutex<Backend>,
}

impl TaskKeyLedger {
    /// 打开按工作区根目录持久化的账本。构造本身不产生任何磁盘写入。
    pub fn persistent(root: &Path) -> Self {
        let dir = root.join(LEDGER_DIR);
        let file = dir.join(LEDGER_FILE);
        let lock = dir.join(LEDGER_LOCK_FILE);
        Self {
            inner: Mutex::new(Backend::Persistent { dir, file, lock }),
        }
    }

    /// 打开进程内账本（`--no-session`）：不创建、不读写任何文件。
    pub fn ephemeral() -> Self {
        Self {
            inner: Mutex::new(Backend::Ephemeral(BTreeMap::new())),
        }
    }

    /// 是否为持久化账本。
    pub fn is_persistent(&self) -> bool {
        matches!(&*self.lock(), Backend::Persistent { .. })
    }

    /// 持久化账本的文件路径（进程内账本返回 `None`）。
    pub fn path(&self) -> Option<PathBuf> {
        match &*self.lock() {
            Backend::Persistent { file, .. } => Some(file.clone()),
            Backend::Ephemeral(_) => None,
        }
    }

    /// 申请一个显式任务 key。
    ///
    /// - key 首次出现时记录为 [`TaskKeyState::Queued`] 并返回
    ///   [`TaskClaim::Acquired`]。
    /// - key 已存在时返回 [`TaskClaim::Existing`]，**不做任何写入**，调用方必须
    ///   放弃执行并回放已有 `task_id`。
    ///
    /// 账本不可读、不可写、已满或损坏时返回 `Err`；调用方应 fail-closed，拒绝
    /// 执行任务，而不是绕过去重。
    pub fn claim(&self, key: &str) -> Result<TaskClaim, String> {
        validate_key(key)?;
        let mut guard = self.lock();
        match &mut *guard {
            Backend::Ephemeral(entries) => {
                if let Some(record) = entries.get(key) {
                    return Ok(TaskClaim::Existing {
                        task_id: record.task_id.clone(),
                        state: record.state,
                    });
                }
                if entries.len() >= MAX_LEDGER_ENTRIES {
                    return Err(ledger_full_error());
                }
                let task_id = new_task_id();
                entries.insert(
                    key.to_owned(),
                    TaskKeyRecord {
                        task_id: task_id.clone(),
                        state: TaskKeyState::Queued,
                        key: key.to_owned(),
                        updated: now_secs(),
                        owner: capture_owner(),
                        canonical_phase: None,
                        engine_run_id: None,
                    },
                );
                Ok(TaskClaim::Acquired { task_id })
            }
            Backend::Persistent { dir, file, lock } => {
                let (dir, file, lock) = (dir.clone(), file.clone(), lock.clone());
                with_ledger_locked(&dir, &file, &lock, |ledger| {
                    if let Some(record) = ledger.entries.iter().find(|record| record.key == key) {
                        return Ok((
                            TaskClaim::Existing {
                                task_id: record.task_id.clone(),
                                state: record.state,
                            },
                            false,
                        ));
                    }
                    if ledger.entries.len() >= MAX_LEDGER_ENTRIES {
                        return Err(ledger_full_error());
                    }
                    let task_id = new_task_id();
                    ledger.entries.push(TaskKeyRecord {
                        task_id: task_id.clone(),
                        state: TaskKeyState::Queued,
                        key: key.to_owned(),
                        updated: now_secs(),
                        owner: capture_owner(),
                        canonical_phase: None,
                        engine_run_id: None,
                    });
                    Ok((TaskClaim::Acquired { task_id }, true))
                })
            }
        }
    }

    /// 将已准入的任务标记为运行中。仅接受 `queued`（或幂等的 `active`）状态。
    pub fn begin(&self, task_id: &str) -> Result<(), String> {
        self.transition(task_id, TaskKeyState::Active)
    }

    /// 将任务标记为终态。已处于终态的调用是幂等的，不会覆盖第一次的终态。
    pub fn finish(&self, task_id: &str, state: TaskKeyState) -> Result<(), String> {
        if !state.is_terminal() {
            return Err("task key final state must be terminal".into());
        }
        self.transition(task_id, state)
    }

    /// 只读查询一个 key 的当前记录。
    pub fn lookup(&self, key: &str) -> Result<Option<TaskKeyRecord>, String> {
        let mut guard = self.lock();
        match &mut *guard {
            Backend::Ephemeral(entries) => Ok(entries.get(key).cloned()),
            Backend::Persistent { dir, file, lock } => {
                let (dir, file, lock) = (dir.clone(), file.clone(), lock.clone());
                with_ledger_locked(&dir, &file, &lock, |ledger| {
                    Ok((
                        ledger
                            .entries
                            .iter()
                            .find(|record| record.key == key)
                            .cloned(),
                        false,
                    ))
                })
            }
        }
    }

    /// 只读观测一个 key：返回记录快照与 owner 存活性判定。
    ///
    /// 探测在账本独占锁内进行（与 [`TaskKeyLedger::claim`] 同一把 `flock`），
    /// 因此不会与并发准入交错。查询**不改写**记录、状态或 `updated`：命中 stale
    /// 或 unknown 都不会触发任何转移，也不会删除 key。key 不存在时返回
    /// `Ok(None)`。
    pub fn observe(&self, key: &str) -> Result<Option<TaskObservation>, String> {
        validate_key(key)?;
        let mut guard = self.lock();
        match &mut *guard {
            Backend::Ephemeral(entries) => Ok(entries.get(key).map(|record| TaskObservation {
                record: record.clone(),
                liveness: probe_record_liveness(record),
            })),
            Backend::Persistent { dir, file, lock } => {
                let (dir, file, lock) = (dir.clone(), file.clone(), lock.clone());
                with_ledger_locked(&dir, &file, &lock, |ledger| {
                    let observation =
                        ledger
                            .entries
                            .iter()
                            .find(|record| record.key == key)
                            .map(|record| TaskObservation {
                                record: record.clone(),
                                liveness: probe_record_liveness(record),
                            });
                    Ok((observation, false))
                })
            }
        }
    }

    fn transition(&self, task_id: &str, state: TaskKeyState) -> Result<(), String> {
        let mut guard = self.lock();
        match &mut *guard {
            Backend::Ephemeral(entries) => {
                let Some(record) = entries
                    .values_mut()
                    .find(|record| record.task_id == task_id)
                else {
                    return Err(format!(
                        "task {task_id} is not present in the task key ledger"
                    ));
                };
                apply_transition(record, state)?;
                Ok(())
            }
            Backend::Persistent { dir, file, lock } => {
                let (dir, file, lock) = (dir.clone(), file.clone(), lock.clone());
                with_ledger_locked(&dir, &file, &lock, |ledger| {
                    let Some(record) = ledger
                        .entries
                        .iter_mut()
                        .find(|record| record.task_id == task_id)
                    else {
                        return Err(format!(
                            "task {task_id} is not present in the task key ledger"
                        ));
                    };
                    let changed = apply_transition(record, state)?;
                    Ok(((), changed))
                })
            }
        }
    }

    /// Project the authoritative tracker's canonical phase and engine run UUID
    /// onto an existing key record.
    ///
    /// This is a **projection**, not a second state machine: callers pass the
    /// tracker's own phase name. Legacy `state` is left untouched, so old
    /// readers keep their exact semantics. `engine_run_id` is set-once and an
    /// existing different value is refused rather than silently replaced.
    /// Records written before these fields existed keep `None`.
    pub fn record_canonical(
        &self,
        task_id: &str,
        phase: &str,
        engine_run_id: Option<&str>,
    ) -> Result<(), String> {
        if !matches!(
            phase,
            "queued" | "running" | "completed" | "failed" | "cancelled" | "timed_out"
        ) {
            return Err(format!("'{phase}' is not a canonical task phase"));
        }
        let mut guard = self.lock();
        match &mut *guard {
            Backend::Ephemeral(entries) => {
                let Some(record) = entries
                    .values_mut()
                    .find(|record| record.task_id == task_id)
                else {
                    return Err(format!(
                        "task {task_id} is not present in the task key ledger"
                    ));
                };
                apply_canonical(record, phase, engine_run_id)?;
                Ok(())
            }
            Backend::Persistent { dir, file, lock } => {
                let (dir, file, lock) = (dir.clone(), file.clone(), lock.clone());
                with_ledger_locked(&dir, &file, &lock, |ledger| {
                    let Some(record) = ledger
                        .entries
                        .iter_mut()
                        .find(|record| record.task_id == task_id)
                    else {
                        return Err(format!(
                            "task {task_id} is not present in the task key ledger"
                        ));
                    };
                    let changed = apply_canonical(record, phase, engine_run_id)?;
                    Ok(((), changed))
                })
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Backend> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

// ---------------------------------------------------------------------------
// Owner 身份与 liveness 观测
// ---------------------------------------------------------------------------

/// 准入时捕获的 owner 身份元数据。
///
/// 这是**观测信息，不是安全 ACL**：它只用于判断记录里的 owner 进程是否还能被
/// 正向确认为同一台机器、同一次启动里的同一个进程，不构成认证、授权或防伪承诺。
/// 所有标识字段都已经过 `sha256` 域分隔摘要，定长且不含机器原始标识。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskKeyOwner {
    /// 捕获时的进程 id。
    pub pid: u32,
    /// 该进程的启动时刻（`/proc/<pid>/stat` 第 22 个字段，boot 内的时钟滴答）。
    pub start_time_ticks: u64,
    /// 主机标识摘要（`/etc/machine-id` 等来源的 sha256）。克隆镜像可能共享同一
    /// machine-id，因此该字段只用于协作式观测，不承担防伪。
    pub host_id: String,
    /// 启动标识摘要（`/proc/sys/kernel/random/boot_id` 的 sha256）。
    pub boot_id: String,
    /// 该进程所在 PID namespace 标识摘要。
    pub pid_namespace: String,
    /// 捕获时刻的 Unix 秒数。
    pub captured: u64,
}

/// owner 存活性的观测判定。
///
/// 这是观测结果，不是安全判定：`Unknown` 只表示证据不足，不等价于存活或死亡。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskLiveness {
    /// 证据不足：旧记录缺少 owner、跨 host、跨 PID namespace、平台不支持或
    /// `/proc` 不可读。未知不等于死亡。
    Unknown,
    /// 同一 host、同一 boot、同一 PID namespace 内，owner 进程仍在运行。
    Live,
    /// 有正向证据表明 owner 已不存在（进程消失、同 host 重启）或身份已被替换
    /// （PID 复用）；zombie 进程按已结束处理。
    Stale,
}

impl TaskLiveness {
    /// 稳定的 snake_case 名称（与序列化一致，便于文本输出）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Live => "live",
            Self::Stale => "stale",
        }
    }
}

/// [`TaskKeyLedger::observe`] 的返回值：记录快照加 owner 存活性判定。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskObservation {
    /// 账本中的记录快照；观测不会改写它。
    pub record: TaskKeyRecord,
    /// owner 存活性判定。
    pub liveness: TaskLiveness,
}

impl TaskObservation {
    /// 记录的任务标识。
    pub fn task_id(&self) -> &str {
        &self.record.task_id
    }
}

/// 采集**当前进程**的 owner 身份元数据。
///
/// 返回 `None` 表示当前平台不支持或身份不可得；调用方应把对应记录视为
/// [`TaskLiveness::Unknown`]，而不是猜测存活或死亡。
pub fn capture_owner() -> Option<TaskKeyOwner> {
    capture_owner_for_pid(std::process::id())
}

/// 采集指定进程的 owner 身份元数据（只读观测，不写账本）。
///
/// Linux 上由 `pid` + 启动时刻 + host 摘要 + boot 摘要 + PID namespace 摘要组成；
/// 无法读取、权限不足或非 Linux 平台返回 `None`。返回值只含定长摘要，不含机器
/// 原始标识。
#[cfg(target_os = "linux")]
pub fn capture_owner_for_pid(pid: u32) -> Option<TaskKeyOwner> {
    if pid == 0 {
        return None;
    }
    let host_id = machine_id_digest()?;
    let boot_id = boot_id_digest()?;
    let ProcProbe::Found {
        start_time_ticks, ..
    } = probe_process(pid)
    else {
        return None;
    };
    let pid_namespace = pid_namespace_digest(pid)?;
    Some(TaskKeyOwner {
        pid,
        start_time_ticks,
        host_id,
        boot_id,
        pid_namespace,
        captured: now_secs(),
    })
}

/// 非 Linux 平台没有可移植的进程身份证据，一律返回 `None`（对应 liveness 未知）。
#[cfg(not(target_os = "linux"))]
pub fn capture_owner_for_pid(_pid: u32) -> Option<TaskKeyOwner> {
    None
}

/// 对一条记录做 owner 存活性探测。只读，不接触账本。
#[cfg(target_os = "linux")]
fn probe_record_liveness(record: &TaskKeyRecord) -> TaskLiveness {
    if record.state.is_terminal() {
        // 终态不回退：不把已完成/失败/取消的任务重新解释为存活或死亡。
        return TaskLiveness::Unknown;
    }
    let Some(owner) = record.owner.as_ref() else {
        // 旧 version 1 记录没有 owner，或采集失败：只能是未知。
        return TaskLiveness::Unknown;
    };
    let Some(local) = LocalIdentity::resolve() else {
        return TaskLiveness::Unknown;
    };
    if owner.host_id != local.host_id {
        // 工作区可能被多台机器共享：其它 host 上的 owner 不能被本机判死。
        return TaskLiveness::Unknown;
    }
    if owner.boot_id != local.boot_id {
        // 同一 host 已经重启：上一个 boot 的进程必然不存在。
        return TaskLiveness::Stale;
    }
    if owner.pid_namespace != local.pid_namespace {
        // PID 只在同一个 PID namespace 内可比，否则数字没有意义。
        return TaskLiveness::Unknown;
    }
    probe_process_liveness(owner)
}

/// 非 Linux 平台不支持进程身份证据，一律返回未知。
#[cfg(not(target_os = "linux"))]
fn probe_record_liveness(_record: &TaskKeyRecord) -> TaskLiveness {
    TaskLiveness::Unknown
}

/// 反序列化时校验 owner 字段，避免坏元数据伪装成旧记录或可信 owner。
fn validate_owner(owner: &TaskKeyOwner) -> Result<(), String> {
    if owner.pid == 0 {
        return Err("task key owner pid must not be zero".into());
    }
    if owner.start_time_ticks == 0 {
        return Err("task key owner start time must not be zero".into());
    }
    for (field, value) in [
        ("host_id", &owner.host_id),
        ("boot_id", &owner.boot_id),
        ("pid_namespace", &owner.pid_namespace),
    ] {
        if !is_identity_digest(value) {
            return Err(format!(
                "task key owner {field} must be a {MAX_OWNER_IDENTITY_CHARS}-character lowercase sha256 digest"
            ));
        }
    }
    Ok(())
}

fn is_identity_digest(value: &str) -> bool {
    value.len() == MAX_OWNER_IDENTITY_CHARS
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// 本机当前的 host / boot / PID namespace 摘要。
#[cfg(target_os = "linux")]
struct LocalIdentity {
    host_id: String,
    boot_id: String,
    pid_namespace: String,
}

#[cfg(target_os = "linux")]
impl LocalIdentity {
    fn resolve() -> Option<Self> {
        Some(Self {
            host_id: machine_id_digest()?,
            boot_id: boot_id_digest()?,
            pid_namespace: pid_namespace_digest(std::process::id())?,
        })
    }
}

/// `/proc/<pid>/stat` 单次读取上限（有界）。
#[cfg(target_os = "linux")]
const MAX_PROC_STAT_BYTES: usize = 4096;
/// machine-id / boot_id 等系统身份文件读取上限（有界）。
#[cfg(target_os = "linux")]
const MAX_IDENTITY_SOURCE_BYTES: usize = 256;
/// `/proc/self/mountinfo` 读取上限（有界，用于检测 hidepid）。
#[cfg(target_os = "linux")]
const MAX_MOUNTINFO_BYTES: usize = 128 * 1024;

#[cfg(target_os = "linux")]
enum ProcProbe {
    Missing,
    Unreadable,
    Found { state: u8, start_time_ticks: u64 },
}

#[cfg(target_os = "linux")]
fn probe_process(pid: u32) -> ProcProbe {
    let path = format!("/proc/{pid}/stat");
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return ProcProbe::Missing,
        Err(_) => return ProcProbe::Unreadable,
    };
    let mut bytes = Vec::new();
    if file
        .take((MAX_PROC_STAT_BYTES as u64) + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() > MAX_PROC_STAT_BYTES
    {
        return ProcProbe::Unreadable;
    }
    match parse_proc_stat(&bytes) {
        Some((state, start_time_ticks)) => ProcProbe::Found {
            state,
            start_time_ticks,
        },
        None => ProcProbe::Unreadable,
    }
}

#[cfg(target_os = "linux")]
fn probe_process_liveness(owner: &TaskKeyOwner) -> TaskLiveness {
    match probe_process(owner.pid) {
        ProcProbe::Missing => {
            if proc_hidepid_active() {
                // /proc 隐藏了其它用户的进程：无法区分“已消失”和“不可见”。
                TaskLiveness::Unknown
            } else {
                TaskLiveness::Stale
            }
        }
        ProcProbe::Unreadable => TaskLiveness::Unknown,
        ProcProbe::Found {
            state,
            start_time_ticks,
        } => {
            if start_time_ticks != owner.start_time_ticks {
                // 同一个 PID 已属于另一个进程：原 owner 已消失。
                TaskLiveness::Stale
            } else if matches!(state, b'Z' | b'X' | b'x') {
                // zombie / dead 进程已经结束，不构成存活任务。
                TaskLiveness::Stale
            } else {
                TaskLiveness::Live
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn digest_identity(label: &[u8], value: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"mio-dedup-owner-v1\0");
    hasher.update(label);
    hasher.update(b"\0");
    hasher.update(value);
    format!("{:x}", hasher.finalize())
}

#[cfg(target_os = "linux")]
fn machine_id_digest() -> Option<String> {
    const SOURCES: [&str; 2] = ["/etc/machine-id", "/var/lib/dbus/machine-id"];
    for source in SOURCES {
        let Some(bytes) = read_bounded_bytes(Path::new(source), MAX_IDENTITY_SOURCE_BYTES) else {
            continue;
        };
        let trimmed = bytes.trim_ascii();
        if !trimmed.is_empty() {
            return Some(digest_identity(b"host", trimmed));
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn boot_id_digest() -> Option<String> {
    let bytes = read_bounded_bytes(
        Path::new("/proc/sys/kernel/random/boot_id"),
        MAX_IDENTITY_SOURCE_BYTES,
    )?;
    let trimmed = bytes.trim_ascii();
    if trimmed.is_empty() {
        return None;
    }
    Some(digest_identity(b"boot", trimmed))
}

#[cfg(target_os = "linux")]
fn pid_namespace_digest(pid: u32) -> Option<String> {
    let link = fs::read_link(format!("/proc/{pid}/ns/pid")).ok()?;
    let text = link.to_str()?.trim();
    if text.is_empty() || text.len() > MAX_IDENTITY_SOURCE_BYTES {
        return None;
    }
    Some(digest_identity(b"pidns", text.as_bytes()))
}

#[cfg(target_os = "linux")]
fn read_bounded_bytes(path: &Path, max: usize) -> Option<Vec<u8>> {
    let file = File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take((max as u64) + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() > max {
        return None;
    }
    Some(bytes)
}

#[cfg(target_os = "linux")]
fn parse_proc_stat(bytes: &[u8]) -> Option<(u8, u64)> {
    let close = bytes.iter().rposition(|byte| *byte == b')')?;
    let rest = bytes.get(close + 1..)?;
    let mut fields = rest
        .split(|byte| *byte == b' ')
        .filter(|field| !field.is_empty());
    let state = *fields.next()?.first()?;
    // comm 之后 starttime 是整体第 22 个字段，即剩余字段中的第 19 个。
    let start_time_ticks = std::str::from_utf8(fields.nth(18)?).ok()?.parse().ok()?;
    Some((state, start_time_ticks))
}

#[cfg(target_os = "linux")]
fn proc_hidepid_active() -> bool {
    let Some(bytes) = read_bounded_bytes(Path::new("/proc/self/mountinfo"), MAX_MOUNTINFO_BYTES)
    else {
        // 无法确认 /proc 可见性策略时，保守视为“可能隐藏”。
        return true;
    };
    hidepid_from_mountinfo(&bytes)
}

#[cfg(target_os = "linux")]
fn hidepid_from_mountinfo(bytes: &[u8]) -> bool {
    const PROC_MOUNT: &[u8] = b" - proc ";
    const HIDEPID: &[u8] = b"hidepid=";
    for line in bytes.split(|byte| *byte == b'\n') {
        let Some(position) = find_bytes(line, PROC_MOUNT) else {
            continue;
        };
        let rest = &line[position + PROC_MOUNT.len()..];
        let mut fields = rest
            .split(|byte| *byte == b' ')
            .filter(|field| !field.is_empty());
        let _source = fields.next();
        let Some(options) = fields.next() else {
            continue;
        };
        for option in options.split(|byte| *byte == b',') {
            // 只有 `hidepid=0` 表示关闭隐藏；其余取值（1、2、空值、…）都算有隐藏
            // 策略。用 `strip_prefix` + 逐字节比较，避免整数切片 / 显式引用在
            // clippy `op_ref` / `redundant_slicing` 下被判定为多余引用。
            if let Some(value) = option.strip_prefix(HIDEPID)
                && (value.len() != 1 || value[0] != b'0')
            {
                return true;
            }
        }
    }
    false
}

#[cfg(target_os = "linux")]
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn apply_transition(record: &mut TaskKeyRecord, state: TaskKeyState) -> Result<bool, String> {
    if state == TaskKeyState::Active {
        match record.state {
            TaskKeyState::Queued => {}
            TaskKeyState::Active => return Ok(false),
            other => {
                return Err(format!(
                    "task {} is already terminal ({}); refusing to restart it",
                    record.task_id,
                    other.as_str()
                ));
            }
        }
    } else if record.state.is_terminal() {
        // 保留第一次记录的终态，重复 finalize 是幂等 no-op。
        return Ok(false);
    }
    record.state = state;
    record.updated = now_secs();
    Ok(true)
}

/// Project a canonical phase (and set-once engine run UUID) onto a record
/// without touching the legacy `state`. A different existing engine run id is
/// refused, never overwritten.
fn apply_canonical(
    record: &mut TaskKeyRecord,
    phase: &str,
    engine_run_id: Option<&str>,
) -> Result<bool, String> {
    let mut changed = false;
    if record.canonical_phase.as_deref() != Some(phase) {
        record.canonical_phase = Some(phase.to_owned());
        changed = true;
    }
    if let Some(run_id) = engine_run_id {
        // 校验必须无副作用：已有的不同真实 run id 一律拒绝，绝不能先把原值
        // `take()` 掉再返回错误，否则一次被拒的投影调用会抹掉已提交的身份。
        match record.engine_run_id.as_deref() {
            Some(existing) if existing == run_id => {}
            Some(_) => {
                return Err(format!(
                    "task {} already has a different real engine run id",
                    record.task_id
                ));
            }
            None => {
                record.engine_run_id = Some(run_id.to_owned());
                changed = true;
            }
        }
    }
    if changed {
        record.updated = now_secs();
    }
    Ok(changed)
}

fn ledger_full_error() -> String {
    format!(
        "task key ledger is full ({MAX_LEDGER_ENTRIES} entries); \
         remove {LEDGER_DIR}/{LEDGER_FILE} to start a new key namespace"
    )
}

fn new_task_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

// ---------------------------------------------------------------------------
// Persistent backend
// ---------------------------------------------------------------------------

fn with_ledger_locked<R>(
    dir: &Path,
    file: &Path,
    lock: &Path,
    mutate: impl FnOnce(&mut LedgerFile) -> Result<(R, bool), String>,
) -> Result<R, String> {
    ensure_ledger_dir(dir)?;
    let lock_file = open_lock_file(lock)?;
    lock_file
        .lock()
        .map_err(|error| format!("cannot lock task key ledger {}: {error}", lock.display()))?;
    let mut ledger = load_ledger(file)?;
    let (result, dirty) = mutate(&mut ledger)?;
    if dirty {
        ledger.updated = now_secs();
        persist_ledger(dir, file, &ledger)?;
    }
    drop(lock_file);
    Ok(result)
}

fn ensure_ledger_dir(dir: &Path) -> Result<(), String> {
    let product = dir
        .parent()
        .ok_or("task key ledger directory has no parent")?;
    for directory in [product, dir] {
        match fs::symlink_metadata(directory) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(
                    "task key ledger directories must be real directories, not symlinks".into(),
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // 并发首建时另一个进程可能已经创建，`create_dir_all` 对已存在目录
                // 幂等；随后重新确认不是 symlink。
                fs::create_dir_all(directory).map_err(|error| {
                    format!(
                        "cannot create task key ledger directory {}: {error}",
                        directory.display()
                    )
                })?;
                let metadata =
                    fs::symlink_metadata(directory).map_err(|error| error.to_string())?;
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return Err(
                        "task key ledger directories must be real directories, not symlinks".into(),
                    );
                }
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    set_private_dir(dir)
}

fn load_ledger(path: &Path) -> Result<LedgerFile, String> {
    let bytes = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(format!(
                    "task key ledger {} must be a regular file, not a symlink",
                    path.display()
                ));
            }
            if metadata.len() > MAX_LEDGER_FILE_SIZE as u64 {
                return Err(format!(
                    "task key ledger {} exceeds {MAX_LEDGER_FILE_SIZE} bytes",
                    path.display()
                ));
            }
            let mut bytes = Vec::new();
            File::open(path)
                .and_then(|file| {
                    file.take((MAX_LEDGER_FILE_SIZE + 1) as u64)
                        .read_to_end(&mut bytes)
                })
                .map_err(|error| {
                    format!("cannot read task key ledger {}: {error}", path.display())
                })?;
            bytes
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LedgerFile {
                version: LEDGER_VERSION,
                updated: now_secs(),
                entries: Vec::new(),
            });
        }
        Err(error) => {
            return Err(format!(
                "cannot inspect task key ledger {}: {error}",
                path.display()
            ));
        }
    };
    if bytes.len() > MAX_LEDGER_FILE_SIZE {
        return Err(format!(
            "task key ledger {} exceeds {MAX_LEDGER_FILE_SIZE} bytes",
            path.display()
        ));
    }
    let ledger: LedgerFile = serde_json::from_slice(&bytes)
        .map_err(|error| format!("corrupt task key ledger {}: {error}", path.display()))?;
    if ledger.version != LEDGER_VERSION {
        return Err(format!(
            "unsupported task key ledger version {} (expected {LEDGER_VERSION})",
            ledger.version
        ));
    }
    let mut keys = std::collections::BTreeSet::new();
    let mut ids = std::collections::BTreeSet::new();
    if ledger.entries.len() > MAX_LEDGER_ENTRIES {
        return Err(ledger_full_error());
    }
    for record in &ledger.entries {
        validate_key(&record.key)?;
        uuid::Uuid::parse_str(&record.task_id).map_err(|_| "invalid task id in task key ledger")?;
        if let Some(owner) = &record.owner {
            // 坏 owner 元数据不能伪装成旧记录：宁可 fail-closed，也不静默丢弃。
            validate_owner(owner)?;
        }
        if !keys.insert(&record.key) || !ids.insert(&record.task_id) {
            return Err("duplicate key or task id in task key ledger".into());
        }
    }
    Ok(ledger)
}

fn persist_ledger(dir: &Path, path: &Path, ledger: &LedgerFile) -> Result<(), String> {
    if ledger.entries.len() > MAX_LEDGER_ENTRIES {
        return Err(ledger_full_error());
    }
    let payload = serde_json::to_vec_pretty(ledger)
        .map_err(|error| format!("cannot serialize task key ledger: {error}"))?;
    if payload.len() > MAX_LEDGER_FILE_SIZE {
        return Err(format!(
            "task key ledger would exceed {MAX_LEDGER_FILE_SIZE} bytes"
        ));
    }
    let mut temp = NamedTempFile::new_in(dir).map_err(|error| {
        format!(
            "cannot create task key ledger tempfile in {}: {error}",
            dir.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("cannot set task key ledger mode 0600: {error}"))?;
    }
    temp.write_all(&payload)
        .map_err(|error| format!("cannot write task key ledger: {error}"))?;
    temp.as_file()
        .sync_all()
        .map_err(|error| format!("cannot sync task key ledger: {error}"))?;
    temp.persist(path).map_err(|error| {
        format!(
            "cannot persist task key ledger to {}: {error}",
            path.display()
        )
    })?;
    // 让 key 的准入先于任务执行落盘：目录项也 fsync，避免掉电丢失 rename。
    #[cfg(unix)]
    {
        File::open(dir)
            .and_then(|file| file.sync_all())
            .map_err(|error| format!("cannot sync task key ledger directory: {error}"))?;
    }
    Ok(())
}

fn open_lock_file(path: &Path) -> Result<File, String> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        return Err("task key ledger lock must not be a symlink".into());
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path).map_err(|error| {
        format!(
            "cannot open task key ledger lock {}: {error}",
            path.display()
        )
    })?;
    if !file
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err("task key ledger lock must be a regular file".into());
    }
    Ok(file)
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

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquired_key_blocks_replay_and_returns_stable_id() {
        let ledger = TaskKeyLedger::ephemeral();
        let claim = ledger.claim("job-1").unwrap();
        let TaskClaim::Acquired { task_id } = &claim else {
            panic!("first claim must acquire the key");
        };
        let task_id = task_id.clone();
        ledger.begin(&task_id).unwrap();
        ledger.finish(&task_id, TaskKeyState::Completed).unwrap();

        let again = ledger.claim("job-1").unwrap();
        assert_eq!(
            again,
            TaskClaim::Existing {
                task_id: task_id.clone(),
                state: TaskKeyState::Completed,
            }
        );
        // 终态重复 finalize 是幂等 no-op，不覆盖第一次终态。
        ledger.finish(&task_id, TaskKeyState::Failed).unwrap();
        assert_eq!(
            ledger.lookup("job-1").unwrap().unwrap().state,
            TaskKeyState::Completed
        );
    }

    #[test]
    fn queued_key_is_not_accepted_twice_and_begin_is_idempotent() {
        let ledger = TaskKeyLedger::ephemeral();
        let TaskClaim::Acquired { task_id } = ledger.claim("queued").unwrap() else {
            panic!("first claim must acquire");
        };
        assert!(!ledger.claim("queued").unwrap().is_acquired());
        ledger.begin(&task_id).unwrap();
        ledger.begin(&task_id).unwrap();
        assert_eq!(
            ledger.lookup("queued").unwrap().unwrap().state,
            TaskKeyState::Active
        );
        assert!(ledger.finish(&task_id, TaskKeyState::Queued).is_err());
    }

    #[test]
    fn persistent_ledger_survives_reopen_and_uses_private_files() {
        let dir = tempfile::tempdir().unwrap();
        let first = TaskKeyLedger::persistent(dir.path());
        assert!(first.is_persistent());
        let TaskClaim::Acquired { task_id } = first.claim("cross-run").unwrap() else {
            panic!("first claim must acquire");
        };
        first.begin(&task_id).unwrap();
        first.finish(&task_id, TaskKeyState::Completed).unwrap();
        drop(first);

        let second = TaskKeyLedger::persistent(dir.path());
        assert_eq!(
            second.claim("cross-run").unwrap(),
            TaskClaim::Existing {
                task_id: task_id.clone(),
                state: TaskKeyState::Completed,
            }
        );
        let path = second.path().unwrap();
        assert!(path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::symlink_metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn interrupted_key_still_blocks_replay() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = TaskKeyLedger::persistent(dir.path());
        let TaskClaim::Acquired { task_id } = ledger.claim("crashed").unwrap() else {
            panic!("first claim must acquire");
        };
        ledger.begin(&task_id).unwrap();
        ledger.finish(&task_id, TaskKeyState::Interrupted).unwrap();
        assert!(matches!(
            ledger.claim("crashed").unwrap(),
            TaskClaim::Existing {
                state: TaskKeyState::Interrupted,
                ..
            }
        ));
    }

    #[test]
    fn invalid_keys_are_rejected_without_touching_the_ledger() {
        let ledger = TaskKeyLedger::ephemeral();
        for key in ["", "   ", "\n", "a\nb", "\u{7}bell"] {
            assert!(ledger.claim(key).is_err(), "key {key:?} must be rejected");
        }
        let long = "x".repeat(MAX_DEDUP_KEY_BYTES + 1);
        assert!(ledger.claim(&long).is_err());
        assert!(validate_key("ok-key").is_ok());
    }

    #[test]
    fn concurrent_claims_across_instances_are_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let contenders = 8;
        let mut handles = Vec::new();
        for _ in 0..contenders {
            let root = dir.path().to_path_buf();
            handles.push(std::thread::spawn(move || {
                let ledger = TaskKeyLedger::persistent(&root);
                ledger.claim("shared").unwrap().task_id().to_owned()
            }));
        }
        let ids: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(
            ids.windows(2).all(|pair| pair[0] == pair[1]),
            "concurrent claims must converge on one task id: {ids:?}"
        );
        let ledger = TaskKeyLedger::persistent(dir.path());
        assert_eq!(ledger.lookup("shared").unwrap().unwrap().task_id, ids[0]);

        // 并发的不同 key 互不干扰，最终全部落盘。
        let mut handles = Vec::new();
        for index in 0..contenders {
            let root = dir.path().to_path_buf();
            handles.push(std::thread::spawn(move || {
                let ledger = TaskKeyLedger::persistent(&root);
                let claim = ledger.claim(&format!("key-{index}")).unwrap();
                assert!(claim.is_acquired());
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let ledger = TaskKeyLedger::persistent(dir.path());
        for index in 0..contenders {
            assert!(ledger.lookup(&format!("key-{index}")).unwrap().is_some());
        }
    }

    #[test]
    fn corrupt_or_versioned_mismatch_ledger_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let ledger_dir = dir.path().join(LEDGER_DIR);
        fs::create_dir_all(&ledger_dir).unwrap();
        let path = ledger_dir.join(LEDGER_FILE);

        fs::write(&path, b"{not json").unwrap();
        assert!(TaskKeyLedger::persistent(dir.path()).claim("k").is_err());

        fs::write(&path, br#"{"version":99,"updated":0,"entries":[]}"#).unwrap();
        assert!(TaskKeyLedger::persistent(dir.path()).claim("k").is_err());
    }

    #[test]
    fn symlinked_ledger_directory_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let ledger_dir = dir.path().join(LEDGER_DIR);
        fs::create_dir_all(ledger_dir.parent().unwrap()).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), &ledger_dir).unwrap();
        #[cfg(not(unix))]
        return;
        let ledger = TaskKeyLedger::persistent(dir.path());
        assert!(ledger.claim("k").is_err());
        assert!(!outside.path().join(LEDGER_FILE).exists());
    }

    #[test]
    fn ephemeral_ledger_never_writes_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = TaskKeyLedger::ephemeral();
        assert!(!ledger.is_persistent());
        assert!(ledger.path().is_none());
        let TaskClaim::Acquired { task_id } = ledger.claim("k").unwrap() else {
            panic!("first claim must acquire");
        };
        ledger.begin(&task_id).unwrap();
        ledger.finish(&task_id, TaskKeyState::Completed).unwrap();
        assert!(!dir.path().join(LEDGER_DIR).exists());
    }

    #[test]
    fn unknown_task_transition_is_reported() {
        let ledger = TaskKeyLedger::ephemeral();
        assert!(ledger.begin("missing").is_err());
        assert!(ledger.finish("missing", TaskKeyState::Failed).is_err());
    }

    #[test]
    fn observe_reports_liveness_without_mutating_ephemeral_records() {
        let ledger = TaskKeyLedger::ephemeral();
        let TaskClaim::Acquired { task_id } = ledger.claim("observed").unwrap() else {
            panic!("first claim must acquire");
        };
        let observation = ledger.observe("observed").unwrap().unwrap();
        assert_eq!(observation.task_id(), task_id.as_str());
        assert_eq!(observation.record.state, TaskKeyState::Queued);
        #[cfg(target_os = "linux")]
        assert_eq!(observation.liveness, TaskLiveness::Live);
        #[cfg(not(target_os = "linux"))]
        assert_eq!(observation.liveness, TaskLiveness::Unknown);

        assert!(ledger.observe("absent").unwrap().is_none());
        // 终态不参与 owner 探测，且观测本身不触发转移。
        ledger.finish(&task_id, TaskKeyState::Completed).unwrap();
        let observation = ledger.observe("observed").unwrap().unwrap();
        assert_eq!(observation.liveness, TaskLiveness::Unknown);
        assert_eq!(observation.record.state, TaskKeyState::Completed);
    }

    #[test]
    fn owner_metadata_validation_rejects_bad_values() {
        let digest = |byte: char| byte.to_string().repeat(MAX_OWNER_IDENTITY_CHARS);
        let valid = TaskKeyOwner {
            pid: 42,
            start_time_ticks: 7,
            host_id: digest('a'),
            boot_id: digest('b'),
            pid_namespace: digest('c'),
            captured: 0,
        };
        assert!(validate_owner(&valid).is_ok());

        let mut owner = valid.clone();
        owner.pid = 0;
        assert!(validate_owner(&owner).is_err());
        let mut owner = valid.clone();
        owner.start_time_ticks = 0;
        assert!(validate_owner(&owner).is_err());
        let mut owner = valid.clone();
        owner.host_id = "a".repeat(MAX_OWNER_IDENTITY_CHARS - 1);
        assert!(validate_owner(&owner).is_err());
        let mut owner = valid.clone();
        owner.boot_id = "A".repeat(MAX_OWNER_IDENTITY_CHARS);
        assert!(validate_owner(&owner).is_err());
        let mut owner = valid;
        owner.pid_namespace = "z".repeat(MAX_OWNER_IDENTITY_CHARS);
        assert!(validate_owner(&owner).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_stat_parsing_handles_spaces_and_parens_in_comm() {
        let line =
            b"1234 (weird ) name) S 1 1234 1234 0 -1 4194304 0 0 0 0 0 0 0 0 20 0 1 0 987654 0 0";
        assert_eq!(parse_proc_stat(line), Some((b'S', 987654)));
        assert_eq!(parse_proc_stat(b"garbage"), None);
        assert_eq!(parse_proc_stat(b"1 (x) R"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn hidepid_detection_reads_only_nonzero_proc_options() {
        assert!(!hidepid_from_mountinfo(
            b"53 44 0:25 / /proc rw,nosuid,nodev,noexec,relatime shared:13 - proc proc rw\n"
        ));
        assert!(hidepid_from_mountinfo(
            b"53 44 0:25 / /proc rw,hidepid=2 - proc proc rw,hidepid=2\n"
        ));
        assert!(!hidepid_from_mountinfo(
            b"53 44 0:25 / /proc rw,hidepid=0 - proc proc rw,hidepid=0\n"
        ));
        assert!(!hidepid_from_mountinfo(
            b"53 44 0:25 / /sys rw,hidepid=2 - sysfs sysfs rw,hidepid=2\n"
        ));
    }
}
