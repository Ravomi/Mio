//! 跨进程协作式准入配额（shared limits）。
//!
//! 该模块让同一台机器上的多个 Mio 进程**自愿**共享模型请求与重任务资源，
//! 用于收敛 429 风暴以及「编译 / 浏览器测试」互相争抢的情况。
//!
//! # 边界（务必阅读）
//!
//! - 只约束**接入本模块的协作进程**的准入，不限制操作系统内存、CPU、文件描述符，
//!   也不影响任何未接入本模块的工具或进程。
//! - 配额基于 Unix advisory file lock（`File::try_lock` / `flock`）实现。进程异常
//!   退出（含被 `SIGKILL`）时，内核会释放其持有的全部槽位。
//! - 只会读写调用者提供的 `root` 目录，不触碰其它配置或凭据。`root` 会被设为
//!   Unix `0700`，锁文件 / 策略文件 / cooldown 文件为 `0600`，并拒绝静态 symlink。
//! - `provider_key` 是不透明标识：只使用其 SHA-256 十六进制摘要作为文件名 / cooldown
//!   键，原文绝不写入磁盘或日志。
//!
//! # 不可变策略（版本化）
//!
//! 同一个 `root` 下持久化一份版本化的策略文件 `policy.json`（`0600`、有大小上限），
//! 记录全局 / 每 provider / 重任务并发上限。首次创建使用「临时文件 + `hard_link`」
//! 原子发布，绝不覆盖、绝不发布半写文件；因此并发首建时只有一个进程能胜出，其余
//! 进程会读到胜出者的策略。之后任何进程用同一 `root` 打开时都必须与之逐项一致，
//! 否则返回 [`SharedLimitsError::PolicyMismatch`]（fail-closed）。策略损坏、版本未知、
//! 超限或为 symlink 时同样拒绝。
//!
//! 因此**不允许**通过传入更大的并发上限来偷偷扩大全局限额；要变更策略必须由用户
//! 明确换用新目录，或停机后统一处理。
//!
//! # 跨进程预算账本（版本化）
//!
//! 同一个 `root` 下可持久化一份版本化的预算账本 `budget.json`（`0600`、有大小上限），
//! 记录跨进程的模型调用次数 / token 预算、已结算用量与仍未结算的在途预留。账本只在
//! 调用方显式设置 `max_model_calls` / `max_total_tokens` 时创建；从未设置预算的 root
//! 保持兼容（不创建文件、不写盘、不改变准入）。
//!
//! - **只能收窄**：账本记录的策略只能被更窄的请求逐项收窄（取 min）。请求比账本已记录
//!   的策略更宽时 fail-closed 报 [`SharedLimitsError::BudgetPolicyMismatch`]；因此更宽的
//!   本地参数不会悄悄放大已有共享策略，要放宽必须换用新 root。请求完全未设置时继承
//!   账本已记录的策略（重启后仍然一致生效）。首次打开用阻塞 `flock` 串行化首次创建 /
//!   收窄的读-改-写（仅启动阶段）；运行时的预留 / 结算仍用轮询 `try_lock`，不阻塞工作线程。
//! - **原子预留**：调用前在账本内原子占用「token 预留 + 一次调用次数」，预留落盘后才
//!   可能发出请求；账本损坏、丢失、超额或次数耗尽一律 fail-closed 报错，绝不降级放行。
//! - **真实 usage 结算**：结算写回 Provider 上报的真实 usage；未上报按预留额保守计费，
//!   绝不按 0 计。实际用量超过预留记 over-reservation，超过预算记 overage 并阻止后续请求。
//! - **取消 / 崩溃不退款**：只有在请求**尚未发出**时才能显式归还预留
//!   （[`SharedBudgetPermit::abandon`]）。已发出的调用即使取消、进程退出甚至被 `SIGKILL`，
//!   其预留仍保留在账本里（最多按预留额保守计入），因此无法靠反复取消绕过共享预算。
//! - **调用次数已发出才算**：次数只在预留成功（紧接真实 dispatch）时 +1；未发出的调用
//!   在放弃预留时回滚，保证不会为未发出的请求计数。
//! - **与本地预算的关系**：进程内 `Coordinator` 的预算只统计本进程，共享账本统计同一
//!   `root` 下的全部协作进程。一次调用会在两层各记一次（每层恰好一次，作用域不同，
//!   不是对同一层的重复计费），实际生效的上限是两者中更窄的一方；未启用共享 root 时
//!   只有本地预算。
//!
//! # 调度语义
//!
//! - 模型请求需要**同时**持有「全局」与「provider」两级槽位。若 provider 槽位不可用，
//!   会立即释放已拿到的全局槽位再等待，**绝不在等待时持有部分名额**（避免死锁）。
//! - 等待通过 `try_lock` 轮询 + `tokio::time::sleep` 实现，不阻塞 tokio 工作线程，
//!   并且可被 `drop`（如 `tokio::time::timeout`）取消；取消时不会泄漏槽位。
//! - cooldown 存储在有大小上限的小文件里，使用「临时文件 + rename」原子更新；
//!   并发更新取最大 deadline（`max`），损坏 / 超限的文件会 fail-closed 报错。
//!
//! # workspace 作用域租约
//!
//! [`SharedLimits::acquire_workspace`] 提供按目录作用域协作的路径租约：
//!
//! - `cwd` 先 canonicalize，再对**每个路径组件**做域分隔的 SHA-256 摘要，形成不泄漏
//!   原文、但保持祖先前缀关系的路径身份（Windows 下对组件做保守小写归一）。
//! - 两个路径存在祖先 / 相等关系且**任一为写**时冲突；读读可并发，互不相交的目录
//!   可并发。
//! - 一个有界（最多 `4096` 条 / `1 MiB`）的 registry 表由短暂的元锁保护；每条 claim
//!   都有独立唯一的私有 liveness 锁文件，持有者保持其 fd 锁。准入时逐个 `try_lock`
//!   liveness，回收已退出进程的 stale claim，再做祖先关系比较，创建 live 锁并原子登记。
//! - 元锁只在同步临界区内持有，**绝不跨 `await`**；等待以 25ms 轮询实现且可被 `drop`
//!   取消。`drop` 只做一次非阻塞清理尝试，失败则交由 liveness 回收。
//! - 这是**协作式路径锁，不是 OS 沙箱**，也只约束接入本模块的进程；不保证跨进程严格
//!   FIFO（公平性取决于轮询调度）。
//!
//! # 不可观测项
//!
//! 无法可靠枚举「其它进程持有哪些槽位」：持有者集合只能通过会改变状态的探测获得，
//! 且 PID/fd 归属不可信。因此 [`SharedLimits::snapshot`] 只返回配置、cooldown 与预算
//! 账本，不返回 active 槽位计数，也绝不伪造计数（账本里的在途预留在用户视角下是持久
//! 计数，含进程崩溃后遗留的预留，不是「当前活着的持有者」清单）。

use std::{
    collections::BTreeMap,
    fmt,
    fs::{File, OpenOptions, TryLockError},
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 槽位等待的轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// 策略文件格式版本。
const POLICY_VERSION: u32 = 1;
/// 策略文件字节上限。
const MAX_POLICY_FILE_BYTES: u64 = 4 * 1024;
/// cooldown 文件默认字节上限。
const DEFAULT_MAX_COOLDOWN_FILE_BYTES: u64 = 64 * 1024;
/// cooldown 默认最多保留的 provider 条目数。
const DEFAULT_MAX_COOLDOWN_ENTRIES: usize = 256;
/// 并发上限的合理上界，避免错误配置一次性打开过多文件。
const MAX_CONCURRENCY: usize = 1024;
/// cooldown 文件字节数下限（保证 `{}` 之类最小内容可写入）。
const MIN_COOLDOWN_FILE_BYTES: u64 = 64;
/// cooldown 文件条目数上限。
const MAX_COOLDOWN_ENTRIES_LIMIT: usize = 65_536;
/// workspace registry 版本。
const WORKSPACE_REGISTRY_VERSION: u32 = 1;
/// workspace registry 字节上限。
const MAX_WORKSPACE_REGISTRY_BYTES: u64 = 1024 * 1024;
/// workspace registry 最大 claim 数。
const MAX_WORKSPACE_CLAIMS: usize = 4096;
/// 单个路径身份的最大组件数。
const MAX_PATH_COMPONENTS: usize = 256;
/// 每组件哈希的十六进制长度（16 字节 / 32 hex）。
const PATH_COMPONENT_HEX_LEN: usize = 32;
/// claim liveness 锁文件名前缀。
const CLAIM_PREFIX: &str = "claim-";
/// 路径身份哈希的域分隔前缀。
const PATH_HASH_DOMAIN: &[u8] = b"mio-shared-limits-path-v1\0";
/// 预算账本格式版本。
const BUDGET_VERSION: u32 = 1;
/// 预算账本字节上限。
const MAX_BUDGET_FILE_BYTES: u64 = 8 * 1024;
/// `Drop` 里同步结算预留的重试次数（约 200ms）。
const SETTLE_SYNC_ATTEMPTS: usize = 200;
/// `Drop` 里同步结算预留的重试间隔。
const SETTLE_SYNC_INTERVAL: Duration = Duration::from_millis(1);

/// 临时文件命名的进程内自增序列，配合 PID / 纳秒避免碰撞。
static TMP_SERIAL: AtomicU64 = AtomicU64::new(0);

/// 跨进程共享的准入配置。
///
/// 默认值：全局模型并发 `2`、每 provider 模型并发 `2`、重任务并发 `2`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct SharedLimitsConfig {
    /// 所有 provider 合计的模型请求并发上限。
    pub global_model_concurrency: usize,
    /// 单个 provider 的模型请求并发上限。
    pub provider_model_concurrency: usize,
    /// 重任务（编译、浏览器测试等）并发上限。
    pub heavy_concurrency: usize,
    /// cooldown 文件最多保留的 provider 条目数。
    pub max_cooldown_entries: usize,
    /// cooldown 文件最大字节数。
    pub max_cooldown_file_bytes: u64,
    /// 可选的跨进程模型调用次数上限；持久化在共享预算账本，只会收窄不会放宽。
    pub max_model_calls: Option<u64>,
    /// 可选的跨进程 token 总数上限；持久化在共享预算账本，只会收窄不会放宽。
    pub max_total_tokens: Option<u64>,
}

impl Default for SharedLimitsConfig {
    fn default() -> Self {
        Self {
            global_model_concurrency: 2,
            provider_model_concurrency: 2,
            heavy_concurrency: 2,
            max_cooldown_entries: DEFAULT_MAX_COOLDOWN_ENTRIES,
            max_cooldown_file_bytes: DEFAULT_MAX_COOLDOWN_FILE_BYTES,
            max_model_calls: None,
            max_total_tokens: None,
        }
    }
}

impl SharedLimitsConfig {
    /// 校验配置是否合法；非法配置在 [`SharedLimits::new`] 阶段 fail-closed。
    pub fn validate(&self) -> Result<(), SharedLimitsError> {
        for (name, value) in [
            ("global_model_concurrency", self.global_model_concurrency),
            (
                "provider_model_concurrency",
                self.provider_model_concurrency,
            ),
            ("heavy_concurrency", self.heavy_concurrency),
        ] {
            if value == 0 {
                return Err(SharedLimitsError::InvalidConfig(format!(
                    "{name} must be >= 1"
                )));
            }
            if value > MAX_CONCURRENCY {
                return Err(SharedLimitsError::InvalidConfig(format!(
                    "{name} must be <= {MAX_CONCURRENCY}, got {value}"
                )));
            }
        }
        if self.max_cooldown_entries == 0 {
            return Err(SharedLimitsError::InvalidConfig(
                "max_cooldown_entries must be >= 1".into(),
            ));
        }
        if self.max_cooldown_entries > MAX_COOLDOWN_ENTRIES_LIMIT {
            return Err(SharedLimitsError::InvalidConfig(format!(
                "max_cooldown_entries must be <= {MAX_COOLDOWN_ENTRIES_LIMIT}"
            )));
        }
        if self.max_cooldown_file_bytes < MIN_COOLDOWN_FILE_BYTES {
            return Err(SharedLimitsError::InvalidConfig(format!(
                "max_cooldown_file_bytes must be >= {MIN_COOLDOWN_FILE_BYTES}"
            )));
        }
        if self.max_cooldown_file_bytes > 16 * 1024 * 1024 {
            return Err(SharedLimitsError::InvalidConfig(
                "max_cooldown_file_bytes must be <= 16 MiB".into(),
            ));
        }
        if self.max_model_calls == Some(0) {
            return Err(SharedLimitsError::InvalidConfig(
                "max_model_calls must be >= 1 when set".into(),
            ));
        }
        if self.max_total_tokens == Some(0) {
            return Err(SharedLimitsError::InvalidConfig(
                "max_total_tokens must be >= 1 when set".into(),
            ));
        }
        Ok(())
    }
}

/// `SharedLimits` 的错误类型。
#[derive(Debug)]
pub enum SharedLimitsError {
    /// 底层 IO 失败。
    Io(io::Error),
    /// 配置非法。
    InvalidConfig(String),
    /// 路径是 symlink 或不是目录 / 普通文件，拒绝使用。
    UnsafePath(PathBuf),
    /// cooldown 文件损坏、超限或包含非法键，fail-closed。
    CorruptCooldown(String),
    /// 策略文件损坏、版本未知或超限，fail-closed。
    CorruptPolicy(String),
    /// 同一 `root` 已存在另一份策略，与本次请求的并发上限不一致，fail-closed。
    PolicyMismatch(String),
    /// workspace registry 损坏、版本未知或超限，fail-closed。
    CorruptWorkspace(String),
    /// workspace registry 已达容量 / 大小上限。
    WorkspaceLimit(String),
    /// 预算账本损坏、版本未知、超限或已丢失，fail-closed。
    CorruptBudget(String),
    /// 请求的本地预算比共享账本已记录的策略更宽，fail-closed。
    BudgetPolicyMismatch(String),
    /// 共享 token 预算不足或已超额，拒绝本次请求。
    BudgetExhausted {
        /// 已结算的 token 数。
        tokens_used: u64,
        /// 当前仍未结算的在途预留。
        tokens_reserved_active: u64,
        /// 超过预算的累计 token 数。
        tokens_overage: u64,
        /// 配置上限（若设置）。
        limit: Option<u64>,
    },
    /// 共享模型调用次数预算已耗尽。
    ModelCallBudgetExceeded {
        /// 已占用的调用次数。
        used: u64,
        /// 配置上限。
        limit: u64,
    },
    /// 单次预留本身就超过共享 token 上限，无法满足。
    BudgetReservationTooLarge {
        /// 请求预留的 token 数。
        reservation: u64,
        /// 配置上限。
        limit: u64,
    },
}

impl fmt::Display for SharedLimitsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "shared limits IO error: {err}"),
            Self::InvalidConfig(msg) => write!(f, "invalid shared limits config: {msg}"),
            Self::UnsafePath(path) => write!(
                f,
                "refusing unsafe shared limits path (symlink or wrong file type): {}",
                path.display()
            ),
            Self::CorruptCooldown(msg) => write!(f, "corrupt shared limits cooldown: {msg}"),
            Self::CorruptPolicy(msg) => write!(f, "corrupt shared limits policy: {msg}"),
            Self::PolicyMismatch(msg) => write!(f, "shared limits policy mismatch: {msg}"),
            Self::CorruptWorkspace(msg) => write!(f, "corrupt shared limits workspace: {msg}"),
            Self::WorkspaceLimit(msg) => write!(f, "shared limits workspace limit: {msg}"),
            Self::CorruptBudget(msg) => write!(f, "corrupt shared limits budget: {msg}"),
            Self::BudgetPolicyMismatch(msg) => {
                write!(f, "shared limits budget policy mismatch: {msg}")
            }
            Self::BudgetExhausted {
                tokens_used,
                tokens_reserved_active,
                tokens_overage,
                limit,
            } => write!(
                f,
                "shared token budget exhausted: used {tokens_used}, in-flight {tokens_reserved_active}, overage {tokens_overage}, limit {limit:?}"
            ),
            Self::ModelCallBudgetExceeded { used, limit } => write!(
                f,
                "shared model call budget exceeded: used {used} >= limit {limit}"
            ),
            Self::BudgetReservationTooLarge { reservation, limit } => write!(
                f,
                "shared token reservation {reservation} exceeds budget limit {limit}"
            ),
        }
    }
}

impl std::error::Error for SharedLimitsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for SharedLimitsError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// 持久化的不可变策略。字段与并发上限一一对应，`version` 用于未来格式演进。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PolicyFile {
    version: u32,
    global_model_concurrency: usize,
    provider_model_concurrency: usize,
    heavy_concurrency: usize,
}

impl PolicyFile {
    fn from_config(config: &SharedLimitsConfig) -> Self {
        Self {
            version: POLICY_VERSION,
            global_model_concurrency: config.global_model_concurrency,
            provider_model_concurrency: config.provider_model_concurrency,
            heavy_concurrency: config.heavy_concurrency,
        }
    }

    /// 校验本策略合法（范围 / 版本），用于读取后防御坏文件。
    fn validate(&self) -> Result<(), SharedLimitsError> {
        if self.version != POLICY_VERSION {
            return Err(SharedLimitsError::CorruptPolicy(format!(
                "unsupported policy version {} (expected {POLICY_VERSION})",
                self.version
            )));
        }
        for (name, value) in [
            ("global_model_concurrency", self.global_model_concurrency),
            (
                "provider_model_concurrency",
                self.provider_model_concurrency,
            ),
            ("heavy_concurrency", self.heavy_concurrency),
        ] {
            if value == 0 || value > MAX_CONCURRENCY {
                return Err(SharedLimitsError::CorruptPolicy(format!(
                    "{name} out of range: {value}"
                )));
            }
        }
        Ok(())
    }

    /// 要求 `existing` 与本策略完全一致，否则 fail-closed。
    fn verify(&self, existing: &Self) -> Result<(), SharedLimitsError> {
        if self != existing {
            return Err(SharedLimitsError::PolicyMismatch(format!(
                "existing (global={}, provider={}, heavy={}) vs requested (global={}, provider={}, heavy={}); \
                 to change limits, use a different root or stop all users first",
                existing.global_model_concurrency,
                existing.provider_model_concurrency,
                existing.heavy_concurrency,
                self.global_model_concurrency,
                self.provider_model_concurrency,
                self.heavy_concurrency,
            )));
        }
        Ok(())
    }
}

/// 模型请求许可。`drop` 时通过关闭文件描述符（RAII）释放全局与 provider 槽位。
#[must_use = "dropping the permit immediately releases the model slots"]
pub struct ModelPermit {
    _global: File,
    _provider: File,
}

/// 重任务许可。`drop` 时释放重任务槽位。
#[must_use = "dropping the permit immediately releases the heavy slot"]
pub struct HeavyPermit {
    _slot: File,
}

/// workspace 作用域租约。
///
/// 持有期间保持自己的 liveness fd 锁。`drop` 时只做一次非阻塞的注销尝试；
/// 若拿不到 registry 元锁（返回前绝不等待），则留给后续准入通过 liveness 回收。
///
/// 该租约是协作式路径锁而非 OS 沙箱，也不保证跨进程严格 FIFO。
#[must_use = "dropping the permit releases the workspace lease"]
pub struct WorkspacePermit {
    limits: SharedLimits,
    id: String,
    _live: File,
}

impl Drop for WorkspacePermit {
    fn drop(&mut self) {
        // 单次非阻塞 try_lock；失败则保留 stale claim，由 liveness 回收。
        self.limits.try_release_workspace(&self.id);
    }
}

/// 某个 provider 的 cooldown 快照。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProviderCooldown {
    /// provider key 的 SHA-256 十六进制摘要（不含原文）。
    pub provider_hash: String,
    /// cooldown 截止时间的 Unix 毫秒时间戳。
    pub deferred_until_unix_millis: u64,
    /// 距离截止的剩余时长。
    pub remaining: Duration,
}

/// [`SharedLimits::snapshot`] 的返回值。
///
/// **只包含配置、cooldown 与预算账本**，不含 active 槽位计数：跨进程持有者无法被可靠枚举，
/// 因此不提供该字段，也不伪造计数。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SharedLimitsSnapshot {
    /// 当前生效的配置。
    pub config: SharedLimitsConfig,
    /// 当前仍未过期的 provider cooldown（按 hash 排序）。
    pub cooldowns: Vec<ProviderCooldown>,
    /// 当前跨进程预算账本快照（未启用时为空策略）。
    pub budget: SharedBudgetSnapshot,
}

/// registry 中的一条 workspace 租约记录。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct WorkspaceClaim {
    /// 唯一 claim id，用于定位 liveness 锁文件。
    id: String,
    /// 逐组件哈希后的路径身份（保持祖先前缀关系，不泄漏原文）。
    components: Vec<String>,
    /// 是否为写租约。
    write: bool,
}

impl WorkspaceClaim {
    fn validate(&self) -> Result<(), SharedLimitsError> {
        if self.id.is_empty()
            || self.id.len() > 64
            || !self
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(SharedLimitsError::CorruptWorkspace(
                "registry contains an invalid claim id".into(),
            ));
        }
        validate_components(&self.components)
    }
}

/// 有界的 workspace 租约表。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct WorkspaceRegistry {
    version: u32,
    claims: Vec<WorkspaceClaim>,
}

impl WorkspaceRegistry {
    fn empty() -> Self {
        Self {
            version: WORKSPACE_REGISTRY_VERSION,
            claims: Vec::new(),
        }
    }

    fn validate(&self) -> Result<(), SharedLimitsError> {
        if self.version != WORKSPACE_REGISTRY_VERSION {
            return Err(SharedLimitsError::CorruptWorkspace(format!(
                "unsupported workspace registry version {} (expected {WORKSPACE_REGISTRY_VERSION})",
                self.version
            )));
        }
        if self.claims.len() > MAX_WORKSPACE_CLAIMS {
            return Err(SharedLimitsError::CorruptWorkspace(format!(
                "workspace registry has too many claims: {}",
                self.claims.len()
            )));
        }
        for claim in &self.claims {
            claim.validate()?;
        }
        Ok(())
    }
}

fn validate_components(components: &[String]) -> Result<(), SharedLimitsError> {
    if components.is_empty() || components.len() > MAX_PATH_COMPONENTS {
        return Err(SharedLimitsError::CorruptWorkspace(
            "registry contains an invalid path identity".into(),
        ));
    }
    if components.iter().any(|component| {
        component.len() != PATH_COMPONENT_HEX_LEN
            || !component.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Err(SharedLimitsError::CorruptWorkspace(
            "registry contains an invalid path component hash".into(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 跨进程预算账本
// ---------------------------------------------------------------------------

/// 跨进程共享的预算策略。两个维度各自可选；全为 `None` 表示不启用持久化预算。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SharedBudgetPolicy {
    /// 模型调用次数上限。
    pub max_model_calls: Option<u64>,
    /// token 总数上限。
    pub max_total_tokens: Option<u64>,
}

impl SharedBudgetPolicy {
    /// 两个维度都为 `None` 时返回 `false`，表示不启用持久化预算。
    pub fn is_enabled(self) -> bool {
        self.max_model_calls.is_some() || self.max_total_tokens.is_some()
    }

    /// 逐项取更窄的上限：`None` 表示无限制，因此有值的一方受限。
    fn narrower(self, other: Self) -> Self {
        Self {
            max_model_calls: narrower_limit(self.max_model_calls, other.max_model_calls),
            max_total_tokens: narrower_limit(self.max_total_tokens, other.max_total_tokens),
        }
    }
}

fn narrower_limit(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, b) => b,
    }
}

/// 请求比账本已记录的策略更宽时返回 `(字段, 账本值, 请求值)`。
fn wider_than(
    recorded: SharedBudgetPolicy,
    requested: SharedBudgetPolicy,
) -> Option<(&'static str, u64, u64)> {
    if let (Some(recorded), Some(requested)) = (recorded.max_model_calls, requested.max_model_calls)
        && requested > recorded
    {
        return Some(("max_model_calls", recorded, requested));
    }
    if let (Some(recorded), Some(requested)) =
        (recorded.max_total_tokens, requested.max_total_tokens)
        && requested > recorded
    {
        return Some(("max_total_tokens", recorded, requested));
    }
    None
}

/// 持久化的跨进程预算账本。所有计数单调递增（除 `abandon` 对未发出调用的精确回滚）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct BudgetLedger {
    version: u32,
    max_model_calls: Option<u64>,
    max_total_tokens: Option<u64>,
    calls_started: u64,
    tokens_reserved_active: u64,
    tokens_used: u64,
    tokens_over_reservation: u64,
    tokens_overage: u64,
    blocked: bool,
}

impl BudgetLedger {
    fn new(policy: SharedBudgetPolicy) -> Self {
        Self {
            version: BUDGET_VERSION,
            max_model_calls: policy.max_model_calls,
            max_total_tokens: policy.max_total_tokens,
            calls_started: 0,
            tokens_reserved_active: 0,
            tokens_used: 0,
            tokens_over_reservation: 0,
            tokens_overage: 0,
            blocked: false,
        }
    }

    fn policy(&self) -> SharedBudgetPolicy {
        SharedBudgetPolicy {
            max_model_calls: self.max_model_calls,
            max_total_tokens: self.max_total_tokens,
        }
    }

    fn set_policy(&mut self, policy: SharedBudgetPolicy) {
        self.max_model_calls = policy.max_model_calls;
        self.max_total_tokens = policy.max_total_tokens;
    }

    fn validate(&self) -> Result<(), SharedLimitsError> {
        if self.version != BUDGET_VERSION {
            return Err(SharedLimitsError::CorruptBudget(format!(
                "unsupported budget ledger version {} (expected {BUDGET_VERSION})",
                self.version
            )));
        }
        for (name, value) in [
            ("max_model_calls", self.max_model_calls),
            ("max_total_tokens", self.max_total_tokens),
        ] {
            if value == Some(0) {
                return Err(SharedLimitsError::CorruptBudget(format!(
                    "{name} must be >= 1 when set"
                )));
            }
        }
        Ok(())
    }

    fn snapshot(&self) -> SharedBudgetSnapshot {
        SharedBudgetSnapshot {
            max_model_calls: self.max_model_calls,
            max_total_tokens: self.max_total_tokens,
            calls_started: self.calls_started,
            tokens_reserved_active: self.tokens_reserved_active,
            tokens_used: self.tokens_used,
            tokens_over_reservation: self.tokens_over_reservation,
            tokens_overage: self.tokens_overage,
            blocked: self.blocked,
        }
    }
}

/// 跨进程预算账本的只读快照（不含任何凭据或 provider 原文）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SharedBudgetSnapshot {
    /// 模型调用次数上限（若设置）。
    pub max_model_calls: Option<u64>,
    /// token 总数上限（若设置）。
    pub max_total_tokens: Option<u64>,
    /// 已占用的调用次数（已发出或仍在途）。
    pub calls_started: u64,
    /// 仍未结算的在途预留（含进程崩溃后遗留的预留）。
    pub tokens_reserved_active: u64,
    /// 已结算的 token 数（未知 usage 按预留额保守计入）。
    pub tokens_used: u64,
    /// 结算值超过预留额的累计 token 数。
    pub tokens_over_reservation: u64,
    /// 超过 token 预算的累计 token 数。
    pub tokens_overage: u64,
    /// 是否已因超额用量阻止后续请求。
    pub blocked: bool,
}

impl SharedBudgetSnapshot {
    fn disabled() -> Self {
        Self {
            max_model_calls: None,
            max_total_tokens: None,
            calls_started: 0,
            tokens_reserved_active: 0,
            tokens_used: 0,
            tokens_over_reservation: 0,
            tokens_overage: 0,
            blocked: false,
        }
    }
}

/// 一次跨进程预算预留（token 预留 + 一次调用次数）。
///
/// - 正常结算用 [`SharedBudgetPermit::settle`]（真实 usage，或 `None` 表示未知）。
/// - 只有在**请求尚未发出**时才能用 [`SharedBudgetPermit::abandon`] 归还预留；已发出的
///   调用绝不归还（取消、退出、`SIGKILL` 都只会保留在账本里）。
/// - 未显式结算就被 `drop` 时按未知 usage 保守结算，并做一次有界的同步尝试；若仍拿不到
///   账本锁，预留保持占用（fail-closed，绝不退款）。
#[must_use = "dropping the permit conservatively settles the shared reservation"]
pub struct SharedBudgetPermit {
    limits: SharedLimits,
    reservation: u64,
    settled: bool,
}

impl SharedBudgetPermit {
    /// 本次预留的 token 数。
    pub fn reservation(&self) -> u64 {
        self.reservation
    }

    /// 以真实 usage 结算；`None` 表示 Provider 未上报，按预留额保守计费。
    pub async fn settle(mut self, usage: Option<u64>) -> Result<(), SharedLimitsError> {
        self.settled = true;
        self.limits.settle_budget(self.reservation, usage).await
    }

    /// 请求尚未发出的失败路径：归还本次预留与调用次数。
    pub async fn abandon(mut self) -> Result<(), SharedLimitsError> {
        self.settled = true;
        self.limits.abandon_budget(self.reservation).await
    }
}

impl fmt::Debug for SharedBudgetPermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedBudgetPermit")
            .field("reservation", &self.reservation)
            .field("settled", &self.settled)
            .finish()
    }
}

impl Drop for SharedBudgetPermit {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        self.settled = true;
        self.limits
            .try_settle_budget_blocking(self.reservation, None);
    }
}

/// 预留 / 结算尝试的结果，用于把「账本锁忙」与「未启用预算」分开处理。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BudgetAttempt {
    /// 已在账本中完成写入。
    Applied,
    /// 该 root 未启用持久化预算。
    NoBudget,
    /// 账本锁忙，调用者可重试。
    Busy,
}

/// 在账本内原子占用一次预留，或在任何不满足条件时 fail-closed。
fn apply_budget_reservation(
    ledger: &mut BudgetLedger,
    tokens: u64,
) -> Result<(), SharedLimitsError> {
    if ledger.blocked {
        return Err(SharedLimitsError::BudgetExhausted {
            tokens_used: ledger.tokens_used,
            tokens_reserved_active: ledger.tokens_reserved_active,
            tokens_overage: ledger.tokens_overage,
            limit: ledger.max_total_tokens,
        });
    }
    if let Some(limit) = ledger.max_total_tokens {
        if tokens > limit {
            return Err(SharedLimitsError::BudgetReservationTooLarge {
                reservation: tokens,
                limit,
            });
        }
        // 已结算用量不会下降；在途预留只能通过结算或未发出的放弃而减少。
        let committed = ledger
            .tokens_used
            .saturating_add(ledger.tokens_reserved_active);
        if committed.saturating_add(tokens) > limit {
            return Err(SharedLimitsError::BudgetExhausted {
                tokens_used: ledger.tokens_used,
                tokens_reserved_active: ledger.tokens_reserved_active,
                tokens_overage: ledger.tokens_overage,
                limit: Some(limit),
            });
        }
    }
    if let Some(limit) = ledger.max_model_calls
        && ledger.calls_started >= limit
    {
        return Err(SharedLimitsError::ModelCallBudgetExceeded {
            used: ledger.calls_started,
            limit,
        });
    }
    ledger.calls_started = ledger.calls_started.saturating_add(1);
    ledger.tokens_reserved_active = ledger.tokens_reserved_active.saturating_add(tokens);
    Ok(())
}

/// 以真实 usage（或未知时按预留额）结算一次预留。
fn apply_budget_settlement(ledger: &mut BudgetLedger, reservation: u64, usage: Option<u64>) {
    ledger.tokens_reserved_active = ledger.tokens_reserved_active.saturating_sub(reservation);
    let actual = usage.unwrap_or(reservation);
    ledger.tokens_used = ledger.tokens_used.saturating_add(actual);
    if actual > reservation {
        ledger.tokens_over_reservation = ledger
            .tokens_over_reservation
            .saturating_add(actual - reservation);
    }
    if let Some(limit) = ledger.max_total_tokens
        && ledger.tokens_used > limit
    {
        ledger.tokens_overage = ledger.tokens_overage.max(ledger.tokens_used - limit);
        ledger.blocked = true;
    }
}

/// 归还一次**尚未发出**的预留（含调用次数）。已发出的调用绝不应走到这里。
fn apply_budget_abandon(ledger: &mut BudgetLedger, reservation: u64) {
    ledger.tokens_reserved_active = ledger.tokens_reserved_active.saturating_sub(reservation);
    ledger.calls_started = ledger.calls_started.saturating_sub(1);
}

/// 打开（必要时创建）版本化预算账本，并核对本地请求没有被放得更宽。
fn ensure_budget(
    path: &Path,
    lock_path: &Path,
    requested: SharedBudgetPolicy,
) -> Result<Option<SharedBudgetPolicy>, SharedLimitsError> {
    if !requested.is_enabled() && read_budget(path)?.is_none() {
        // 兼容：从未设置预算的 root 不创建账本文件，也不写盘。
        return Ok(None);
    }
    // 读-改-写必须串行化，否则并发收窄会丢失更新。
    let lock = open_lock_file(lock_path)?;
    lock.lock()?;
    match read_budget(path)? {
        None => {
            if !requested.is_enabled() {
                return Ok(None);
            }
            publish_budget(path, requested)?;
            Ok(Some(requested))
        }
        Some(mut ledger) => {
            let recorded = ledger.policy();
            if let Some((name, recorded_limit, requested_limit)) = wider_than(recorded, requested) {
                return Err(SharedLimitsError::BudgetPolicyMismatch(format!(
                    "{name}: ledger records {recorded_limit}, requested {requested_limit}; the shared budget can only be narrowed, use a different root to raise it"
                )));
            }
            let effective = recorded.narrower(requested);
            if effective != recorded {
                ledger.set_policy(effective);
                write_budget_file(path, &ledger)?;
            }
            Ok(Some(effective))
        }
    }
}

/// 首建账本：写私有临时文件后 `hard_link`，绝不覆盖并发创建者的账本。
fn publish_budget(path: &Path, policy: SharedBudgetPolicy) -> Result<(), SharedLimitsError> {
    let bytes = encode_budget(&BudgetLedger::new(policy))?;
    let tmp = temp_sibling(path);
    if let Err(err) = write_new_private_file(&tmp, &bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    match std::fs::hard_link(&tmp, path) {
        Ok(()) => {
            let _ = std::fs::remove_file(&tmp);
            sync_parent(path)
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(&tmp);
            // 所有协作进程都在 flock 之后创建；这里出现 AlreadyExists 说明有外部写入。
            Err(SharedLimitsError::BudgetPolicyMismatch(
                "budget ledger was created concurrently; reopen the shared limits root".into(),
            ))
        }
        Err(err) => {
            let _ = std::fs::remove_file(&tmp);
            Err(SharedLimitsError::Io(err))
        }
    }
}

fn read_budget(path: &Path) -> Result<Option<BudgetLedger>, SharedLimitsError> {
    reject_symlink(path)?;
    let bytes = match read_bounded(path, MAX_BUDGET_FILE_BYTES, || {
        SharedLimitsError::CorruptBudget(format!(
            "budget ledger exceeds {MAX_BUDGET_FILE_BYTES} bytes"
        ))
    })? {
        Some(bytes) => bytes,
        None => return Ok(None),
    };
    if bytes.is_empty() {
        return Err(SharedLimitsError::CorruptBudget(
            "budget ledger is empty".into(),
        ));
    }
    let ledger: BudgetLedger = serde_json::from_slice(&bytes).map_err(|err| {
        SharedLimitsError::CorruptBudget(format!("invalid budget ledger json: {err}"))
    })?;
    ledger.validate()?;
    Ok(Some(ledger))
}

fn encode_budget(ledger: &BudgetLedger) -> Result<Vec<u8>, SharedLimitsError> {
    let bytes = serde_json::to_vec(ledger).map_err(|err| {
        SharedLimitsError::CorruptBudget(format!("failed to encode budget ledger: {err}"))
    })?;
    if bytes.len() as u64 > MAX_BUDGET_FILE_BYTES {
        return Err(SharedLimitsError::CorruptBudget(
            "encoded budget ledger exceeds size limit".into(),
        ));
    }
    Ok(bytes)
}

fn write_budget_file(path: &Path, ledger: &BudgetLedger) -> Result<(), SharedLimitsError> {
    atomic_replace_private(path, &encode_budget(ledger)?)
}

#[derive(Debug)]
struct Inner {
    locks_dir: PathBuf,
    cooldown_file: PathBuf,
    cooldown_lock: PathBuf,
    registry_file: PathBuf,
    registry_lock: PathBuf,
    claims_dir: PathBuf,
    budget_file: PathBuf,
    budget_lock: PathBuf,
    /// 打开时账本已要求启用预算（账本消失后必须 fail-closed）。
    budget_required: bool,
    /// 运行时是否观察到过账本；一旦观察到，之后消失同样 fail-closed。
    budget_observed: AtomicBool,
    config: SharedLimitsConfig,
}

/// 跨进程共享准入配额的句柄。内部通过 `Arc` 共享，克隆开销极小。
#[derive(Clone)]
pub struct SharedLimits {
    inner: Arc<Inner>,
}

impl SharedLimits {
    /// 打开（必要时创建）一个共享配额根目录。
    ///
    /// `root` 由调用者提供，应当是专用于本模块的目录。Unix 下该目录会被设为 `0700`，
    /// 锁 / 策略 / cooldown 文件为 `0600`。若 `root`、其 `locks/` 子目录或策略路径是
    /// symlink / 错误类型，会报 [`SharedLimitsError::UnsafePath`]。配置非法会报
    /// [`SharedLimitsError::InvalidConfig`]。
    ///
    /// 首次调用会原子且不覆盖地发布版本化策略文件；此后同一 `root` 必须传入一致的并发
    /// 上限，否则返回 [`SharedLimitsError::PolicyMismatch`]。
    ///
    /// 若设置了 `max_model_calls` / `max_total_tokens`，同时会打开（必要时创建）版本化
    /// 预算账本；请求比账本已记录的策略更宽时返回
    /// [`SharedLimitsError::BudgetPolicyMismatch`]（只能收窄，不能放宽）。
    pub fn new(
        root: impl AsRef<Path>,
        config: SharedLimitsConfig,
    ) -> Result<Self, SharedLimitsError> {
        config.validate()?;
        let root = root.as_ref().to_path_buf();
        ensure_private_dir(&root)?;
        let locks_dir = root.join("locks");
        ensure_private_dir(&locks_dir)?;
        ensure_policy(&root.join("policy.json"), &PolicyFile::from_config(&config))?;
        let cooldown_file = root.join("cooldowns.json");
        let cooldown_lock = root.join("cooldowns.lock");
        let workspace_dir = root.join("workspace");
        ensure_private_dir(&workspace_dir)?;
        let claims_dir = workspace_dir.join("claims");
        ensure_private_dir(&claims_dir)?;
        let registry_file = workspace_dir.join("registry.json");
        let registry_lock = workspace_dir.join("registry.lock");
        let budget_file = root.join("budget.json");
        let budget_lock = root.join("budget.lock");
        let effective_budget = ensure_budget(
            &budget_file,
            &budget_lock,
            SharedBudgetPolicy {
                max_model_calls: config.max_model_calls,
                max_total_tokens: config.max_total_tokens,
            },
        )?;
        let mut config = config;
        if let Some(policy) = effective_budget {
            config.max_model_calls = policy.max_model_calls;
            config.max_total_tokens = policy.max_total_tokens;
        }
        let budget_required = effective_budget.is_some_and(SharedBudgetPolicy::is_enabled);
        Ok(Self {
            inner: Arc::new(Inner {
                locks_dir,
                cooldown_file,
                cooldown_lock,
                registry_file,
                registry_lock,
                claims_dir,
                budget_file,
                budget_lock,
                budget_required,
                budget_observed: AtomicBool::new(false),
                config,
            }),
        })
    }

    /// 返回当前配置。
    pub fn config(&self) -> SharedLimitsConfig {
        self.inner.config
    }

    /// 计算 provider key 的 SHA-256 十六进制摘要（与槽位文件名 / cooldown 键一致）。
    ///
    /// 只做单向摘要，调用方无需担心原文外泄。
    pub fn provider_key_hash(provider_key: &str) -> String {
        provider_hash(provider_key)
    }

    /// 预留跨进程 token 预算与一次模型调用。
    ///
    /// 未启用持久化预算时返回 `Ok(None)`（不创建账本、不写盘、不改变准入）。启用时先在
    /// 账本中原子占用预留并计数一次调用，落盘成功后调用者才应发出请求。账本损坏 / 丢失 /
    /// 超额 / 次数耗尽一律 fail-closed 返回 `Err`，绝不降级放行。
    ///
    /// 返回的 [`SharedBudgetPermit`] 必须以真实 usage 结算；只有在请求**尚未发出**的
    /// 失败路径才能调用 [`SharedBudgetPermit::abandon`] 归还预留。
    pub async fn reserve_budget(
        &self,
        tokens: u64,
    ) -> Result<Option<SharedBudgetPermit>, SharedLimitsError> {
        loop {
            match self.try_reserve_budget(tokens)? {
                BudgetAttempt::Busy => sleep(POLL_INTERVAL).await,
                BudgetAttempt::NoBudget => return Ok(None),
                BudgetAttempt::Applied => {
                    return Ok(Some(SharedBudgetPermit {
                        limits: self.clone(),
                        reservation: tokens,
                        settled: false,
                    }));
                }
            }
        }
    }

    /// 返回跨进程预算账本的只读快照；未启用时返回空策略。
    pub fn budget_snapshot(&self) -> Result<SharedBudgetSnapshot, SharedLimitsError> {
        match self.read_budget_ledger()? {
            Some(ledger) => Ok(ledger.snapshot()),
            None => Ok(SharedBudgetSnapshot::disabled()),
        }
    }

    fn try_reserve_budget(&self, tokens: u64) -> Result<BudgetAttempt, SharedLimitsError> {
        let Some(_guard) = self.try_acquire_single(&self.inner.budget_lock)? else {
            return Ok(BudgetAttempt::Busy);
        };
        let Some(mut ledger) = self.read_budget_ledger()? else {
            return Ok(BudgetAttempt::NoBudget);
        };
        apply_budget_reservation(&mut ledger, tokens)?;
        self.write_budget_ledger(&ledger)?;
        Ok(BudgetAttempt::Applied)
    }

    async fn settle_budget(
        &self,
        reservation: u64,
        usage: Option<u64>,
    ) -> Result<(), SharedLimitsError> {
        loop {
            match self.try_settle_budget(reservation, usage)? {
                BudgetAttempt::Busy => sleep(POLL_INTERVAL).await,
                BudgetAttempt::NoBudget | BudgetAttempt::Applied => return Ok(()),
            }
        }
    }

    fn try_settle_budget(
        &self,
        reservation: u64,
        usage: Option<u64>,
    ) -> Result<BudgetAttempt, SharedLimitsError> {
        let Some(_guard) = self.try_acquire_single(&self.inner.budget_lock)? else {
            return Ok(BudgetAttempt::Busy);
        };
        let Some(mut ledger) = self.read_budget_ledger()? else {
            return Ok(BudgetAttempt::NoBudget);
        };
        apply_budget_settlement(&mut ledger, reservation, usage);
        self.write_budget_ledger(&ledger)?;
        Ok(BudgetAttempt::Applied)
    }

    /// `Drop` 使用：有界重试的同步结算。拿不到锁 / 读写失败时保留预留（fail-closed）。
    fn try_settle_budget_blocking(&self, reservation: u64, usage: Option<u64>) {
        for _ in 0..SETTLE_SYNC_ATTEMPTS {
            match self.try_acquire_single(&self.inner.budget_lock) {
                Ok(Some(_guard)) => {
                    if let Ok(Some(mut ledger)) = self.read_budget_ledger() {
                        apply_budget_settlement(&mut ledger, reservation, usage);
                        let _ = self.write_budget_ledger(&ledger);
                    }
                    return;
                }
                Ok(None) => std::thread::sleep(SETTLE_SYNC_INTERVAL),
                Err(_) => return,
            }
        }
        // 超时仍拿不到账本锁：不做任何退款，预留保持占用。
    }

    async fn abandon_budget(&self, reservation: u64) -> Result<(), SharedLimitsError> {
        loop {
            match self.try_abandon_budget(reservation)? {
                BudgetAttempt::Busy => sleep(POLL_INTERVAL).await,
                BudgetAttempt::NoBudget | BudgetAttempt::Applied => return Ok(()),
            }
        }
    }

    fn try_abandon_budget(&self, reservation: u64) -> Result<BudgetAttempt, SharedLimitsError> {
        let Some(_guard) = self.try_acquire_single(&self.inner.budget_lock)? else {
            return Ok(BudgetAttempt::Busy);
        };
        let Some(mut ledger) = self.read_budget_ledger()? else {
            return Ok(BudgetAttempt::NoBudget);
        };
        apply_budget_abandon(&mut ledger, reservation);
        self.write_budget_ledger(&ledger)?;
        Ok(BudgetAttempt::Applied)
    }

    fn read_budget_ledger(&self) -> Result<Option<BudgetLedger>, SharedLimitsError> {
        let Some(ledger) = read_budget(&self.inner.budget_file)? else {
            if self.inner.budget_required || self.inner.budget_observed.load(Ordering::SeqCst) {
                return Err(SharedLimitsError::CorruptBudget(
                    "budget ledger disappeared after it was observed".into(),
                ));
            }
            return Ok(None);
        };
        self.inner.budget_observed.store(true, Ordering::SeqCst);
        Ok(Some(ledger))
    }

    fn write_budget_ledger(&self, ledger: &BudgetLedger) -> Result<(), SharedLimitsError> {
        write_budget_file(&self.inner.budget_file, ledger)
    }

    /// 获取一个模型请求许可：同时持有全局槽位与对应 provider 槽位。
    ///
    /// 若 provider 处于 cooldown 中会先等待其结束。等待过程可被 `drop` 取消，
    /// 且取消时不会持有任何部分名额。
    pub async fn acquire_model(
        &self,
        provider_key: &str,
    ) -> Result<ModelPermit, SharedLimitsError> {
        let hash = provider_hash(provider_key);
        let provider_prefix = format!("model-provider-{hash}");
        loop {
            // 1. 遵守 provider cooldown。
            if let Some(deadline) = self.cooldown_deadline(&hash)? {
                let now = now_millis();
                if deadline > now {
                    let remaining = Duration::from_millis(deadline - now);
                    sleep(remaining.min(POLL_INTERVAL)).await;
                    continue;
                }
            }
            // 2. 先取全局槽位。取不到则等待（此时未持有任何槽位）。
            let Some(global) =
                self.try_acquire_pool("model-global", self.inner.config.global_model_concurrency)?
            else {
                sleep(POLL_INTERVAL).await;
                continue;
            };
            // 3. 再取 provider 槽位；取不到则**立即释放全局槽位**再等待。
            match self.try_acquire_pool(
                &provider_prefix,
                self.inner.config.provider_model_concurrency,
            )? {
                Some(provider) => {
                    return Ok(ModelPermit {
                        _global: global,
                        _provider: provider,
                    });
                }
                None => {
                    drop(global);
                    sleep(POLL_INTERVAL).await;
                }
            }
        }
    }

    /// 获取一个重任务许可（编译、浏览器测试等）。
    ///
    /// 等待过程可被 `drop` 取消。
    pub async fn acquire_heavy(&self) -> Result<HeavyPermit, SharedLimitsError> {
        loop {
            if let Some(slot) =
                self.try_acquire_pool("heavy", self.inner.config.heavy_concurrency)?
            {
                return Ok(HeavyPermit { _slot: slot });
            }
            sleep(POLL_INTERVAL).await;
        }
    }

    /// 为 `provider_key` 设置 / 延长 cooldown。
    ///
    /// 并发更新（含跨进程）取最大 deadline。cooldown 文件损坏或超限时 fail-closed。
    pub async fn defer_provider(
        &self,
        provider_key: &str,
        duration: Duration,
    ) -> Result<(), SharedLimitsError> {
        let hash = provider_hash(provider_key);
        let now = now_millis();
        let until = now.saturating_add(duration.as_millis().min(u64::MAX as u128) as u64);
        loop {
            // cooldown 更新也需要锁；同样使用 try_lock 轮询避免阻塞工作线程。
            let Some(_guard) = self.try_acquire_single(&self.inner.cooldown_lock)? else {
                sleep(POLL_INTERVAL).await;
                continue;
            };
            let mut map = self.read_cooldowns()?;
            let entry = map.entry(hash.clone()).or_insert(0);
            if until > *entry {
                *entry = until;
            }
            prune_cooldowns(&mut map, self.inner.config.max_cooldown_entries, now);
            self.write_cooldowns(&map)?;
            return Ok(());
        }
    }

    /// 返回配置、当前未过期的 cooldown 与跨进程预算账本快照。
    ///
    /// 不返回 active 槽位计数（见模块文档）。账本损坏 / 丢失时 fail-closed 返回 `Err`。
    pub fn snapshot(&self) -> Result<SharedLimitsSnapshot, SharedLimitsError> {
        let now = now_millis();
        let map = self.read_cooldowns()?;
        let budget = self.budget_snapshot()?;
        let mut cooldowns: Vec<ProviderCooldown> = map
            .into_iter()
            .filter(|(_, deadline)| *deadline > now)
            .map(|(provider_hash, deadline)| ProviderCooldown {
                provider_hash,
                deferred_until_unix_millis: deadline,
                remaining: Duration::from_millis(deadline - now),
            })
            .collect();
        cooldowns.sort_by(|left, right| left.provider_hash.cmp(&right.provider_hash));
        Ok(SharedLimitsSnapshot {
            config: self.inner.config,
            cooldowns,
            budget,
        })
    }

    /// 获取一个 workspace 作用域租约。
    ///
    /// `cwd` 会被 canonicalize，并以逐组件哈希形成路径身份；当两者目录存在祖先 /
    /// 后代 / 相等关系且任一为写租约时冲突。读读可并发，互不相交目录可并发。等待以
    /// 25ms 轮询实现且可被 `drop` 取消；持有者进程崩溃后由 liveness 锁回收 stale claim。
    ///
    /// 这是协作式路径锁而非 OS 沙箱；不保证跨进程严格 FIFO。
    pub async fn acquire_workspace(
        &self,
        cwd: &Path,
        write: bool,
    ) -> Result<WorkspacePermit, SharedLimitsError> {
        let components = path_identity(cwd)?;
        loop {
            match self.try_workspace_admit(&components, write)? {
                Some(permit) => return Ok(permit),
                None => sleep(POLL_INTERVAL).await,
            }
        }
    }

    fn claim_path(&self, id: &str) -> PathBuf {
        self.inner
            .claims_dir
            .join(format!("{CLAIM_PREFIX}{id}.lock"))
    }

    /// 同步准入：元锁只在本函数内持有，绝不跨 `await`。
    fn try_workspace_admit(
        &self,
        components: &[String],
        write: bool,
    ) -> Result<Option<WorkspacePermit>, SharedLimitsError> {
        let Some(_meta) = self.try_acquire_single(&self.inner.registry_lock)? else {
            return Ok(None);
        };

        let mut registry = self.read_registry()?;
        let mut retained = Vec::with_capacity(registry.claims.len());
        let mut reclaimed = false;
        for claim in registry.claims.drain(..) {
            if self.claim_is_live(&claim)? {
                retained.push(claim);
            } else {
                reclaimed = true;
                let _ = std::fs::remove_file(self.claim_path(&claim.id));
            }
        }
        registry.claims = retained;

        let conflict = registry
            .claims
            .iter()
            .any(|claim| claims_conflict(components, write, &claim.components, claim.write));
        if conflict {
            if reclaimed {
                self.write_registry(&registry)?;
            }
            return Ok(None);
        }
        if registry.claims.len() >= MAX_WORKSPACE_CLAIMS {
            return Err(SharedLimitsError::WorkspaceLimit(format!(
                "workspace registry is full ({MAX_WORKSPACE_CLAIMS} claims)"
            )));
        }

        let id = new_claim_id();
        let live = open_lock_file(&self.claim_path(&id))?;
        match live.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(err)) => return Err(SharedLimitsError::Io(err)),
        }
        registry.claims.push(WorkspaceClaim {
            id: id.clone(),
            components: components.to_vec(),
            write,
        });
        if let Err(err) = self.write_registry(&registry) {
            let _ = std::fs::remove_file(self.claim_path(&id));
            return Err(err);
        }
        Ok(Some(WorkspacePermit {
            limits: self.clone(),
            id,
            _live: live,
        }))
    }

    /// 通过 liveness 锁判断 claim 是否仍被持有；能锁上即证明持有者已退出（stale）。
    fn claim_is_live(&self, claim: &WorkspaceClaim) -> Result<bool, SharedLimitsError> {
        let file = open_lock_file(&self.claim_path(&claim.id))?;
        match file.try_lock() {
            Ok(()) => Ok(false),
            Err(TryLockError::WouldBlock) => Ok(true),
            Err(TryLockError::Error(err)) => Err(SharedLimitsError::Io(err)),
        }
    }

    fn read_registry(&self) -> Result<WorkspaceRegistry, SharedLimitsError> {
        let path = &self.inner.registry_file;
        reject_symlink(path)?;
        let bytes = match read_bounded(path, MAX_WORKSPACE_REGISTRY_BYTES, || {
            SharedLimitsError::CorruptWorkspace(format!(
                "workspace registry exceeds {MAX_WORKSPACE_REGISTRY_BYTES} bytes"
            ))
        })? {
            Some(bytes) => bytes,
            None => return Ok(WorkspaceRegistry::empty()),
        };
        if bytes.is_empty() {
            return Err(SharedLimitsError::CorruptWorkspace(
                "workspace registry is empty".into(),
            ));
        }
        let registry: WorkspaceRegistry = serde_json::from_slice(&bytes).map_err(|err| {
            SharedLimitsError::CorruptWorkspace(format!("invalid workspace registry json: {err}"))
        })?;
        registry.validate()?;
        Ok(registry)
    }

    fn write_registry(&self, registry: &WorkspaceRegistry) -> Result<(), SharedLimitsError> {
        let bytes = serde_json::to_vec(registry).map_err(|err| {
            SharedLimitsError::CorruptWorkspace(format!(
                "failed to encode workspace registry: {err}"
            ))
        })?;
        if bytes.len() as u64 > MAX_WORKSPACE_REGISTRY_BYTES {
            return Err(SharedLimitsError::WorkspaceLimit(format!(
                "workspace registry would exceed {MAX_WORKSPACE_REGISTRY_BYTES} bytes"
            )));
        }
        atomic_replace_private(&self.inner.registry_file, &bytes)
    }

    /// `Drop` 使用：单次非阻塞尝试注销本 claim，失败则留给 liveness 回收。
    fn try_release_workspace(&self, id: &str) {
        let Ok(Some(_meta)) = self.try_acquire_single(&self.inner.registry_lock) else {
            return;
        };
        if let Ok(mut registry) = self.read_registry() {
            let before = registry.claims.len();
            registry.claims.retain(|claim| claim.id != id);
            if registry.claims.len() != before {
                let _ = self.write_registry(&registry);
            }
        }
        let _ = std::fs::remove_file(self.claim_path(id));
    }

    fn slot_path(&self, prefix: &str, index: usize) -> PathBuf {
        self.inner.locks_dir.join(format!("{prefix}-{index}.lock"))
    }

    /// 尝试从 `count` 个槽位中获取一个。取不到返回 `Ok(None)`；IO / 路径错误返回 `Err`。
    fn try_acquire_pool(
        &self,
        prefix: &str,
        count: usize,
    ) -> Result<Option<File>, SharedLimitsError> {
        for index in 0..count {
            let path = self.slot_path(prefix, index);
            let file = open_lock_file(&path)?;
            match file.try_lock() {
                Ok(()) => return Ok(Some(file)),
                Err(TryLockError::WouldBlock) => continue,
                Err(TryLockError::Error(err)) => return Err(SharedLimitsError::Io(err)),
            }
        }
        Ok(None)
    }

    fn try_acquire_single(&self, path: &Path) -> Result<Option<File>, SharedLimitsError> {
        let file = open_lock_file(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(err)) => Err(SharedLimitsError::Io(err)),
        }
    }

    /// 读取 cooldown 截止时间；文件不存在视为无 cooldown。损坏 / 超限 fail-closed。
    fn cooldown_deadline(&self, hash: &str) -> Result<Option<u64>, SharedLimitsError> {
        Ok(self.read_cooldowns()?.get(hash).copied())
    }

    fn read_cooldowns(&self) -> Result<BTreeMap<String, u64>, SharedLimitsError> {
        let path = &self.inner.cooldown_file;
        reject_symlink(path)?;
        let bytes = match read_bounded(path, self.inner.config.max_cooldown_file_bytes, || {
            SharedLimitsError::CorruptCooldown(format!(
                "cooldown file exceeds {} bytes",
                self.inner.config.max_cooldown_file_bytes
            ))
        })? {
            Some(bytes) => bytes,
            None => return Ok(BTreeMap::new()),
        };
        if bytes.is_empty() {
            return Err(SharedLimitsError::CorruptCooldown(
                "cooldown state is empty".into(),
            ));
        }
        let map: BTreeMap<String, u64> = serde_json::from_slice(&bytes).map_err(|err| {
            SharedLimitsError::CorruptCooldown(format!("invalid cooldown json: {err}"))
        })?;
        for key in map.keys() {
            if key.len() != 64 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(SharedLimitsError::CorruptCooldown(
                    "cooldown file contains a non-hash provider key".into(),
                ));
            }
        }
        Ok(map)
    }

    /// 原子写入 cooldown 文件：写临时文件（`0600`）后 `rename` 覆盖。
    fn write_cooldowns(&self, map: &BTreeMap<String, u64>) -> Result<(), SharedLimitsError> {
        let bytes = serde_json::to_vec(map).map_err(|err| {
            SharedLimitsError::CorruptCooldown(format!("failed to encode cooldown: {err}"))
        })?;
        if bytes.len() as u64 > self.inner.config.max_cooldown_file_bytes {
            return Err(SharedLimitsError::CorruptCooldown(format!(
                "cooldown file would exceed {} bytes",
                self.inner.config.max_cooldown_file_bytes
            )));
        }
        atomic_replace_private(&self.inner.cooldown_file, &bytes)
    }
}

/// 首次创建策略：写入私有临时文件后 `hard_link` 到目标路径。
///
/// `hard_link` 在目标已存在时会以 `AlreadyExists` 失败且**不覆盖**，从而保证并发首建
/// 只有一个胜出者，且目标要么不存在、要么指向完整内容，绝不发布半写文件。
fn ensure_policy(path: &Path, desired: &PolicyFile) -> Result<(), SharedLimitsError> {
    if let Some(existing) = read_policy(path)? {
        return desired.verify(&existing);
    }

    let bytes = serde_json::to_vec(desired).map_err(|err| {
        SharedLimitsError::CorruptPolicy(format!("failed to encode policy: {err}"))
    })?;
    if bytes.len() as u64 > MAX_POLICY_FILE_BYTES {
        return Err(SharedLimitsError::CorruptPolicy(
            "encoded policy exceeds size limit".into(),
        ));
    }

    let tmp = temp_sibling(path);
    if let Err(err) = write_new_private_file(&tmp, &bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }

    match std::fs::hard_link(&tmp, path) {
        Ok(()) => {
            let _ = std::fs::remove_file(&tmp);
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            // 另一个进程赢得了首建竞争；清理临时文件并核对它的策略。
            let _ = std::fs::remove_file(&tmp);
            match read_policy(path)? {
                Some(existing) => desired.verify(&existing),
                None => Err(SharedLimitsError::CorruptPolicy(
                    "policy vanished after concurrent creation".into(),
                )),
            }
        }
        Err(err) => {
            let _ = std::fs::remove_file(&tmp);
            Err(SharedLimitsError::Io(err))
        }
    }
}

fn read_policy(path: &Path) -> Result<Option<PolicyFile>, SharedLimitsError> {
    reject_symlink(path)?;
    let bytes = match read_bounded(path, MAX_POLICY_FILE_BYTES, || {
        SharedLimitsError::CorruptPolicy("policy file exceeds size limit".into())
    })? {
        Some(bytes) => bytes,
        None => return Ok(None),
    };
    let policy: PolicyFile = serde_json::from_slice(&bytes)
        .map_err(|err| SharedLimitsError::CorruptPolicy(format!("invalid policy json: {err}")))?;
    policy.validate()?;
    Ok(Some(policy))
}

/// 读取整个文件，但在读取前用元数据限制大小；`None` 表示文件不存在。
fn read_bounded(
    path: &Path,
    max_bytes: u64,
    too_large: impl FnOnce() -> SharedLimitsError,
) -> Result<Option<Vec<u8>>, SharedLimitsError> {
    match std::fs::metadata(path) {
        Ok(metadata) => {
            if metadata.len() > max_bytes {
                return Err(too_large());
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(SharedLimitsError::Io(err)),
    }
    Ok(Some(std::fs::read(path)?))
}

/// 将 deadline 已过期的条目删除；若仍超过条目上限，则保留 deadline 最大者。
fn prune_cooldowns(map: &mut BTreeMap<String, u64>, max_entries: usize, now: u64) {
    map.retain(|_, deadline| *deadline > now);
    if map.len() > max_entries {
        let mut entries: Vec<(String, u64)> = std::mem::take(map).into_iter().collect();
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        entries.truncate(max_entries);
        *map = entries.into_iter().collect();
    }
}

fn provider_hash(provider_key: &str) -> String {
    use fmt::Write as _;
    let digest = Sha256::digest(provider_key.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// 两个路径身份在「任一为写」且存在祖先 / 相等关系时冲突。
fn claims_conflict(a: &[String], a_write: bool, b: &[String], b_write: bool) -> bool {
    related(a, b) && (a_write || b_write)
}

/// 判断两个路径身份是否存在祖先 / 相等关系（较短者为较长者的前缀）。
fn related(a: &[String], b: &[String]) -> bool {
    let (shorter, longer) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    longer.starts_with(shorter)
}

/// 将 canonical 路径转为逐组件哈希身份；前缀关系被保留，原文不落盘。
fn path_identity(path: &Path) -> Result<Vec<String>, SharedLimitsError> {
    let canonical = std::fs::canonicalize(path)?;
    let mut components = Vec::new();
    for component in canonical.components() {
        match component {
            std::path::Component::RootDir => components.push(hash_component(b"/")),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                return Err(SharedLimitsError::InvalidConfig(
                    "workspace path must not contain '..'".into(),
                ));
            }
            std::path::Component::Normal(name) => {
                components.push(hash_component(&component_bytes(name)));
            }
            std::path::Component::Prefix(prefix) => {
                components.push(hash_component(&component_bytes(prefix.as_os_str())));
            }
        }
    }
    if components.is_empty() {
        components.push(hash_component(b"."));
    }
    validate_components(&components)?;
    Ok(components)
}

fn hash_component(bytes: &[u8]) -> String {
    use fmt::Write as _;
    let mut hasher = Sha256::new();
    hasher.update(PATH_HASH_DOMAIN);
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(PATH_COMPONENT_HEX_LEN);
    for byte in &digest[..16] {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(unix)]
fn component_bytes(name: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    name.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn component_bytes(name: &std::ffi::OsStr) -> Vec<u8> {
    // Windows 保守大小写归一，避免同一目录因大小写产生不同身份。
    name.to_string_lossy().to_lowercase().into_bytes()
}

fn new_claim_id() -> String {
    let serial = TMP_SERIAL.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0);
    format!("{}-{}-{}", std::process::id(), serial, nanos)
}

/// 原子替换一个私有文件：写入 `0600` 临时文件后 `rename`。
fn atomic_replace_private(path: &Path, bytes: &[u8]) -> Result<(), SharedLimitsError> {
    let tmp = temp_sibling(path);
    if let Err(err) = write_new_private_file(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(SharedLimitsError::Io(err));
    }
    sync_parent(path)
}

fn sync_parent(path: &Path) -> Result<(), SharedLimitsError> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

async fn sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

/// 生成同目录下的临时文件名，包含 PID / 序列 / 纳秒以避免碰撞。
fn temp_sibling(path: &Path) -> PathBuf {
    let serial = TMP_SERIAL.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "shared_limits".into());
    path.with_file_name(format!(
        ".{name}.tmp.{}.{}.{}",
        std::process::id(),
        serial,
        nanos
    ))
}

/// 创建目录（若不存在），拒绝 symlink / 非目录，并收紧到 `0700`。
fn ensure_private_dir(path: &Path) -> Result<(), SharedLimitsError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(SharedLimitsError::UnsafePath(path.to_path_buf()));
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)?;
            // 创建后再次确认不是 symlink（防御竞争）。
            let metadata = std::fs::symlink_metadata(path)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(SharedLimitsError::UnsafePath(path.to_path_buf()));
            }
        }
        Err(err) => return Err(SharedLimitsError::Io(err)),
    }
    set_dir_private(path)
}

/// 打开（或创建）锁文件：Unix 下 `0600` + `O_NOFOLLOW`，并确认目标是普通文件。
fn open_lock_file(path: &Path) -> Result<File, SharedLimitsError> {
    reject_symlink(path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(SharedLimitsError::UnsafePath(path.to_path_buf()));
    }
    Ok(file)
}

/// 以 `create_new` + `0600` 打开临时文件，避免跟随 symlink 或覆盖已有文件。
fn open_new_private(path: &Path) -> Result<File, SharedLimitsError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        options.custom_flags(libc::O_NOFOLLOW);
    }
    Ok(options.open(path)?)
}

/// 写入一个新的私有文件并 `sync_all`，用于发布前的临时文件。
fn write_new_private_file(path: &Path, bytes: &[u8]) -> Result<(), SharedLimitsError> {
    let mut file = open_new_private(path)?;
    {
        use io::Write as _;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    Ok(())
}

/// 若路径已是 symlink 则拒绝；不存在视为 OK。
fn reject_symlink(path: &Path) -> Result<(), SharedLimitsError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(SharedLimitsError::UnsafePath(path.to_path_buf()))
        }
        Ok(_) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(SharedLimitsError::Io(err)),
    }
}

#[cfg(unix)]
fn set_dir_private(path: &Path) -> Result<(), SharedLimitsError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_dir_private(_path: &Path) -> Result<(), SharedLimitsError> {
    Ok(())
}
