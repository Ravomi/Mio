//! DES-09 跨重启「未知副作用」保护的独立基础模块。
//!
//! `docs/design-v1.md` 的 DES-09 规定：`SideEffect::External`（插件/MCP）不自动重试；
//! `Unknown` 之后必须先由宿主或用户确认现场，再以**新的调用 ID** 重试，不能由模型
//! 自行绕过未知状态重放。运行内的闸门见 `unknown_effect`，但它随 run 结束一起消失。
//! 本模块补齐**跨重启**这一段：一次真实派发在结果确认之前进程就死了，重启后仍然要
//! 知道「这个调用可能已经产生了副作用」，绝不能当作「没发生过」直接重放。
//!
//! # 主键与身份
//!
//! 主键是本模块唯一的记录标识：**真实** `(scope, call_index)`。`scope` 是引擎 run
//! scope，`call_index` 是该 run 内单调的工具调用序号，与 journal / `tool_lifecycle`
//! 使用同一对身份字段。本模块只接受调用方给出的真实 scope，绝不自己生成、替换、
//! 归一化或猜测 scope；`call_index` 必须 ≥ 1。两处字段一致时即可与 journal 的
//! canonical 调用记录做关联，本模块**不重复**保存 journal 已有的 tool / 目标 / 参数
//! 摘要。
//!
//! # 状态机
//!
//! ```text
//! (无记录) --record_dispatched--> Outstanding
//! (无记录) --record_unknown-----> Unknown          （补漏：派发记录缺失也不能丢未知）
//! Outstanding --record_unknown--> Unknown          （结果不可确认）
//! Outstanding --resolve_confirmed-> Resolved        （结果被正常确认，不是未知副作用）
//! Outstanding --recover_outstanding-> Unknown       （重启恢复：owner 非正向存活）
//! Unknown --acknowledge----------> Acknowledged      （宿主确认现场；保留原始 unknown 事实）
//! ```
//!
//! - 终态是 `Resolved` 与 `Acknowledged`，**不回退**。对终态再次记录**新的** unknown
//!   证据是非法转移，明确报错，绝不悄悄丢弃证据、也绝不悄悄把终态改回未决。
//! - `Unknown` 只能由宿主持**精确快照键**显式确认后离开，不存在自动「解决」路径。
//!   证据不完整的记录（`facts_dropped > 0`）一律拒绝确认：宿主不能确认一份它没有完整
//!   看到过的证据。
//! - 一个被本地显式判定为**从未派发**的纯 `Outstanding` 意图可以被撤回（
//!   [`UnresolvedEffectLedger::release_undispatched`]）：它不携带任何未知证据，留着它
//!   只会在以后的崩溃恢复里报出一个假的 `Unknown`。任何携带证据的记录都必须走
//!   `resolve`/`acknowledge`，不能被撤回。
//! - 记录级 `revision` 单调递增，任何证据或状态变化都推进它；`facts` 超过上限时只
//!   增加 `facts_dropped` 计数，**绝不驱逐**更早的未知事实。
//! - 容量或字节压力下只裁剪终态 `Resolved`（结果已确认已知、不携带未知证据）并
//!   累计到 `resolved_trimmed`；`Outstanding`/`Unknown` 与仍带未知事实的
//!   `Acknowledged` 绝不驱逐。满员且无可裁剪的 `Resolved` 时新记录 fail-closed。
//!
//! # 精确确认：并发确认不能误 ack 新的证据
//!
//! `snapshot()` / `lookup()` 为每条记录返回一个 [`UnresolvedSnapshotKey`]：对
//! `(scope, call_index, revision, 事实集合, facts_dropped, 恢复记录)` 的域分隔
//! SHA-256 摘要。`acknowledge` 必须在账本独占 `flock` 内重新计算当前摘要并逐字
//! 比对，只有**完全相等**才落盘 ack。因此：
//!
//! - 宿主观测快照之后、确认之前又出现一条新的 unknown 证据（同进程或另一个进程），
//!   旧快照键立即失效，ack 得到 [`UnresolvedEffectError::StaleConfirmation`]，
//!   绝不会把新证据一起 ack 掉。
//! - 重复提交同一个（已生效的）快照键是幂等 no-op，不重复写盘、不重复计数。
//!
//! # 隐私：绝不拷贝原始 target 或参数
//!
//! [`UnknownEffectInput`] 可以携带原始 target，但账本只保存它的域分隔 SHA-256
//! 摘要（[`target_digest`]）与规范化操作标签。原始 target、命令、插件/MCP 参数和
//! 密钥从不写入账本文件；有界 `operation` 必须是不含参数的规范化工具身份。
//! `evidence_id` 只在调用方真实持有时记录，缺失就是 `None`，**绝不伪造**。
//!
//! # 持久化、边界与 fail-closed
//!
//! - 工作区级账本：`<workspace>/.mio/unresolved-effects/effects.json`；叶子锁文件
//!   `effects.lock`。目录 0700、文件 0600、`O_NOFOLLOW`，写入走同目录临时文件 +
//!   `fsync` + `rename` + 目录 `fsync`，跨进程临界区用 `flock` 串行。
//! - **覆盖 epoch**：账本额外持久化一个私有 `coverage_initialized` 标志。它只在
//!   **首次**对全工作区 journal 的有界扫描完整且无丢失/损坏、并且所有真实旧
//!   `Unknown` 都成功迁移写入时，才在同一个 `flock` 事务里原子置位；仅存在一个账本
//!   文件（只读打开、部分迁移失败创建）**不算**证明。`coverage_incomplete` 是单调
//!   latch：一旦扫描看见真实的丢失/裁剪或损坏/超限 source 就置位且永不被 ack 清除。
//!   两者任一为真时，[`UnresolvedEffectLedger::claim_dispatch`] 对写/外部调用返回
//!   [`DispatchAdmission::BlockedByCoverage`]；只读调用不受影响。
//! - 只读路径（`snapshot` / `lookup`）不加锁、**不创建任何文件或目录**：`rename`
//!   原子替换保证读到的是完整的旧文件或新文件。
//! - 损坏、未知版本、超限、符号链接、重复主键、坏 owner 元数据、ack 摘要与证据
//!   不一致，全部 fail-closed（返回错误），绝不静默返回空账本。
//! - 账本满员或有界上限被触及时 fail-closed，**不自动淘汰**已有条目（只允许裁剪
//!   终态 `Resolved` 并计数，绝不淘汰 `Outstanding`/`Unknown`/`Acknowledged`）。
//! - [`UnresolvedEffectLedger::disabled`] 完全不碰磁盘；[`UnresolvedEffectLedger::read_only`]
//!   只读且零创建，其 `recover_outstanding` 会给出恢复计划但报告 `persisted: false`，
//!   调用方不得据此放行新的写/外部副作用。
//!
//! # owner 与 liveness（观测信息，不是 ACL）
//!
//! `record_dispatched` 记录派发进程的 pid、启动时刻、host / boot / PID namespace
//! 摘要。恢复时只在**同 host、同 boot、同 PID namespace**内做正向判定：
//!
//! - 进程仍在且启动时刻一致 -> [`Liveness::Live`]：**不误判死亡**，`Outstanding`
//!   保持不动。
//! - 同 host 已换了 boot、PID 已被复用、或进程条目消失且 `/proc` 未隐藏进程 ->
//!   [`Liveness::Stale`]。
//! - 旧记录无 owner、跨 host、跨 namespace、`/proc` 不可读或平台不支持 ->
//!   [`Liveness::Unknown`]。**未知不等于死亡**，但恢复策略对「不确定」一律保守升级
//!   为 `Unknown` 副作用，绝不宣称「没发生过」。
//!
//! 这些字段是协作式观测，不构成认证、授权或防伪承诺，也不用于放行任何操作。
//!
//! 本模块自包含（不依赖 crate 内其它模块），通过 `#[path = "../src/unresolved_effect.rs"]`
//! 被 `tests/unresolved_effect.rs` 在 `lib.rs` 导出之前直接编译。

#![allow(dead_code)] // 独立基础模块：公共 API 供下一步 runner/policy/journal 接入，当前编译目标尚未全部引用。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use tempfile::NamedTempFile;
use uuid::Uuid;

/// 账本数据格式版本。读到其它版本一律视为不可信并 fail-closed。
pub const LEDGER_VERSION: u32 = 1;
/// 相对工作区根目录的账本目录。
pub const LEDGER_DIR: &str = ".mio/unresolved-effects";
/// 账本文件名。
pub const LEDGER_FILE: &str = "effects.json";
/// 与账本同目录的叶子锁文件名。
pub const LEDGER_LOCK_FILE: &str = "effects.lock";
/// 账本文件字节上限。
pub const MAX_LEDGER_FILE_SIZE: usize = 512 * 1024;
/// 账本最多保留的记录数。容量压力下只裁剪终态 `Resolved` 并计数；`Outstanding`、
/// `Unknown` 与仍带未知事实的 `Acknowledged` 一律不驱逐，满员时新记录 fail-closed。
pub const MAX_LEDGER_ENTRIES: usize = 2048;
/// 单条记录最多保留的 unknown 事实数；超出只累计 `facts_dropped`，不驱逐旧事实。
pub const MAX_FACTS_PER_RECORD: usize = 16;
/// `scope` 的最大字符数。
pub const MAX_SCOPE_CHARS: usize = 128;
/// 规范化操作标签的最大字符数。
pub const MAX_OPERATION_CHARS: usize = 128;
/// 确认备注的最大字符数。
pub const MAX_NOTE_CHARS: usize = 256;
/// 单次哈希的原始 target 字节上限；更长的目标应由调用方截断或省略。
pub const MAX_TARGET_BYTES: usize = 4096;
/// `sha256` 十六进制摘要的固定长度。owner 身份与 target 摘要都是这个长度。
pub const SHA256_HEX_CHARS: usize = 64;

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// 账本操作错误。任何错误都必须被调用方当作 fail-closed 信号。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnresolvedEffectError {
    /// 账本处于 disabled：不执行任何磁盘 I/O，也不提供任何未决保证。
    Disabled,
    /// 只读账本不能写入。
    ReadOnly,
    /// `(scope, call_index)` 主键或记录字段不合法。
    InvalidKey(String),
    /// 快照确认键不是合法的 sha256 摘要。
    InvalidSnapshotKey(String),
    /// 非法状态转移；终态不回退。
    IllegalTransition {
        /// 当前状态。
        from: UnresolvedEffectState,
        /// 被拒绝的目标状态。
        to: UnresolvedEffectState,
    },
    /// 快照确认键已陈旧：期间出现了新的 unknown 证据（或状态已改变）。
    StaleConfirmation,
    /// 证据不完整（该记录有被省略的事实）：不能凭一份不完整的快照放行，拒绝确认。
    /// 记录保持 `Unknown`，证据原样保留。
    IncompleteEvidence,
    /// 只允许撤回「从未派发」的纯 `Outstanding` 意图；携带证据的记录必须 resolve/ack。
    NotRetractable,
    /// 要操作的记录不存在。
    NotFound,
    /// 账本损坏、版本不符、超限、符号链接或 I/O 失败；一律 fail-closed。
    Ledger(String),
}

impl fmt::Display for UnresolvedEffectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => write!(
                formatter,
                "the unresolved effect ledger is disabled; no durable unknown-side-effect record exists"
            ),
            Self::ReadOnly => write!(
                formatter,
                "the unresolved effect ledger is read-only; refusing to write"
            ),
            Self::InvalidKey(reason) => {
                write!(formatter, "invalid unresolved effect key: {reason}")
            }
            Self::InvalidSnapshotKey(reason) => {
                write!(
                    formatter,
                    "invalid unresolved effect snapshot key: {reason}"
                )
            }
            Self::IllegalTransition { from, to } => write!(
                formatter,
                "illegal unresolved effect transition {} -> {}",
                from.as_str(),
                to.as_str()
            ),
            Self::StaleConfirmation => write!(
                formatter,
                "the unresolved effect snapshot key is stale: new unknown evidence appeared after it was taken"
            ),
            Self::IncompleteEvidence => write!(
                formatter,
                "the unresolved effect record dropped facts and cannot be acknowledged from an incomplete snapshot; the unknown and its evidence are preserved"
            ),
            Self::NotRetractable => write!(
                formatter,
                "the unresolved effect record carries unknown evidence or is not a pure outstanding intent; it must be resolved or acknowledged, not retracted"
            ),
            Self::NotFound => write!(formatter, "no unresolved effect record matches this key"),
            Self::Ledger(reason) => write!(formatter, "unresolved effect ledger: {reason}"),
        }
    }
}

impl std::error::Error for UnresolvedEffectError {}

// ---------------------------------------------------------------------------
// 公共类型
// ---------------------------------------------------------------------------

/// 账本工作模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerMode {
    /// 完全不碰磁盘；所有操作返回 [`UnresolvedEffectError::Disabled`]。
    Disabled,
    /// 只读：可以查询与计算恢复计划，但零创建、零写入。
    ReadOnly,
    /// 读写：唯一的正常模式。
    ReadWrite,
}

impl LedgerMode {
    /// 稳定的 snake_case 名称。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::ReadOnly => "read_only",
            Self::ReadWrite => "read_write",
        }
    }

    /// 是否为读写模式。
    pub const fn is_writable(self) -> bool {
        matches!(self, Self::ReadWrite)
    }
}

/// 一条未决副作用的权威状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnresolvedEffectState {
    /// 派发意图已持久记录，结果尚未确认。
    Outstanding,
    /// 结果不可确认：副作用可能已经发生。必须由宿主确认才能离开该状态。
    Unknown,
    /// 结果被正常确认为已知；不是未知副作用。终态。
    Resolved,
    /// 宿主在确认现场后显式 ack；原始 unknown 事实仍保留。终态。
    Acknowledged,
}

impl UnresolvedEffectState {
    /// 稳定的 snake_case 名称（与序列化一致）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Outstanding => "outstanding",
            Self::Unknown => "unknown",
            Self::Resolved => "resolved",
            Self::Acknowledged => "acknowledged",
        }
    }

    /// 是否为终态。终态幂等且不可回退。
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Resolved | Self::Acknowledged)
    }

    /// 是否为未决状态（可能仍有未确认的副作用）。
    pub const fn is_pending(self) -> bool {
        matches!(self, Self::Outstanding | Self::Unknown)
    }
}

/// owner 存活性的观测判定。`Unknown` 不等于存活，也不等于死亡。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Liveness {
    /// 证据不足：无 owner、跨 host、跨 PID namespace、`/proc` 不可读或平台不支持。
    Unknown,
    /// 同一 host、同一 boot、同一 PID namespace 内，owner 进程仍在运行。
    Live,
    /// 有正向证据表明 owner 已不存在（进程消失、同 host 重启）或身份已被替换（PID 复用）。
    Stale,
}

impl Liveness {
    /// 稳定的 snake_case 名称（与序列化一致）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Live => "live",
            Self::Stale => "stale",
        }
    }

    /// 是否被正向确认为存活。
    pub const fn is_live(self) -> bool {
        matches!(self, Self::Live)
    }
}

/// unknown 事实的类型化原因。封闭枚举，不从人类可读错误串反推。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownFactReason {
    /// 工具发布了类型化 unknown：结果不可确认。
    ResultUnknown,
    /// 派发在结果确认前被中断或取消。
    Interrupted,
    /// 传输/监督通道在请求可能已到达后断开。
    TransportLost,
    /// 从持久证据恢复的一次派发：在任何类型化结果被观测到之前就被中断，
    /// 这是它自己的真实原因，绝不冒充 MCP 传输失败或插件监督超时。
    InterruptedDispatch,
    /// 分类信息缺失或不完整；按 DES-09 保守视为未知。
    Unclassified,
}

impl UnknownFactReason {
    /// 稳定的 snake_case 名称（与序列化一致）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ResultUnknown => "result_unknown",
            Self::Interrupted => "interrupted",
            Self::TransportLost => "transport_lost",
            Self::InterruptedDispatch => "interrupted_dispatch",
            Self::Unclassified => "unclassified",
        }
    }
}

/// 派发时记录的 owner 身份元数据。
///
/// 这是**观测信息，不是安全 ACL**：只用于判断记录里的 owner 进程是否还能被正向确认
/// 为「同一台机器、同一次启动里的同一个进程」。所有标识字段都是 `sha256` 域分隔摘要，
/// 定长且不含机器原始标识。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectOwner {
    /// 捕获时的进程 id。
    pub pid: u32,
    /// 该进程的启动时刻（`/proc/<pid>/stat` 第 22 个字段，boot 内的时钟滴答）。
    pub start_time_ticks: u64,
    /// 主机标识摘要（`/etc/machine-id` 等来源的 sha256）。
    pub host_id: String,
    /// 启动标识摘要（`/proc/sys/kernel/random/boot_id` 的 sha256）。
    pub boot_id: String,
    /// 该进程所在 PID namespace 标识摘要。
    pub pid_namespace: String,
    /// 捕获时刻的 Unix 秒数。
    pub captured: u64,
}

/// 单条 unknown 事实。`evidence_id` 只在调用方真实持有时存在。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnknownEffectFact {
    /// 账本内单调的事实序号（从 1 开始、密集递增）；不是证据身份。
    pub fact_seq: u64,
    /// 类型化原因。
    pub reason: UnknownFactReason,
    /// 规范化工具身份（不含参数）。
    pub operation: String,
    /// 原始 target 的域分隔 `sha256` 摘要；空表示没有目标。绝不保存原始 target。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target_sha256: String,
    /// 调用方真实持有并原样传入的原始 evidence id；缺失就是 `None`，绝不伪造。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_id: Option<Uuid>,
    /// 观测时刻的 Unix 秒数。
    pub observed: u64,
}

/// 重启恢复把 `Outstanding` 保守升级为 `Unknown` 时留下的记录。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryRecord {
    /// 恢复探测时刻的 Unix 秒数。
    pub recovered_at: u64,
    /// 恢复时观测到的 owner 存活性（`Stale` 或 `Unknown`，绝不会是 `Live`）。
    pub liveness: Liveness,
}

/// 宿主的显式确认（ack）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Acknowledgement {
    /// ack 精确绑定的证据摘要；ack 本身不改变它。
    pub snapshot_key: String,
    /// 确认时刻的 Unix 秒数。
    pub confirmed_at: u64,
    /// 确认时观测到的 owner 存活性。
    pub liveness: Liveness,
    /// 有界确认备注；不得包含参数或密钥。空表示无备注。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// 账本中的单条记录。主键是 `(scope, call_index)`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnresolvedEffectRecord {
    /// 真实 run scope。
    pub scope: String,
    /// 该 run 内单调的工具调用序号（≥ 1）。
    pub call_index: u64,
    /// 权威状态。
    pub state: UnresolvedEffectState,
    /// 记录版本；任何证据或状态变化都递增。参与快照摘要。
    pub revision: u64,
    /// 首次持久化派发意图的 Unix 秒数。
    pub recorded_at: u64,
    /// 最近一次更新的 Unix 秒数。
    pub updated: u64,
    /// 派发时捕获的 owner 身份元数据；旧记录或采集失败时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<EffectOwner>,
    /// 有界的 unknown 事实列表（按 `fact_seq` 升序）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facts: Vec<UnknownEffectFact>,
    /// 超过事实上限后被省略的条数；> 0 表示证据不完整。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub facts_dropped: u64,
    /// 重启恢复升级记录。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<RecoveryRecord>,
    /// 宿主确认记录；原始 `facts` 仍保留。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledgement: Option<Acknowledgement>,
}

impl UnresolvedEffectRecord {
    /// 该记录的 `(scope, call_index)` 主键。
    pub fn key(&self) -> UnresolvedEffectKey {
        UnresolvedEffectKey {
            scope: self.scope.clone(),
            call_index: self.call_index,
        }
    }

    /// 是否仍有未决的未知副作用（`Outstanding` 或 `Unknown`）。
    pub const fn is_pending(&self) -> bool {
        self.state.is_pending()
    }

    /// 是否有任何未知证据（事实或被省略的事实）。
    pub fn has_unknown_evidence(&self) -> bool {
        !self.facts.is_empty() || self.facts_dropped > 0
    }

    /// 该记录当前的精确快照确认键。
    pub fn snapshot_key(&self) -> UnresolvedSnapshotKey {
        UnresolvedSnapshotKey::from_digest(evidence_digest(self))
    }
}

/// `(scope, call_index)` 主键。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct UnresolvedEffectKey {
    /// 真实 run scope。
    pub scope: String,
    /// 该 run 内单调的工具调用序号（≥ 1）。
    pub call_index: u64,
}

impl UnresolvedEffectKey {
    /// 校验并构造一个主键。
    pub fn new(scope: impl Into<String>, call_index: u64) -> Result<Self, UnresolvedEffectError> {
        let key = Self {
            scope: scope.into(),
            call_index,
        };
        validate_key(&key)?;
        Ok(key)
    }
}

impl fmt::Display for UnresolvedEffectKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}#{}", self.scope, self.call_index)
    }
}

/// 精确确认键：某个记录在某一证据版本上的域分隔 `sha256` 摘要。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnresolvedSnapshotKey(String);

impl UnresolvedSnapshotKey {
    fn from_digest(digest: String) -> Self {
        Self(digest)
    }

    /// 十六进制摘要文本。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for UnresolvedSnapshotKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for UnresolvedSnapshotKey {
    type Err = UnresolvedEffectError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if !is_sha256_hex(value) {
            return Err(UnresolvedEffectError::InvalidSnapshotKey(format!(
                "snapshot key must be a {SHA256_HEX_CHARS}-character lowercase sha256 digest"
            )));
        }
        Ok(Self(value.to_owned()))
    }
}

/// 一条记录的快照：记录内容 + 精确确认键 + owner 存活性观测。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnresolvedEffectObservation {
    /// 记录快照。
    pub record: UnresolvedEffectRecord,
    /// 与 `record` 当前证据版本精确对应的确认键。
    pub snapshot_key: UnresolvedSnapshotKey,
    /// owner 存活性观测；终态记录不参与探测，恒为 [`Liveness::Unknown`]。
    pub liveness: Liveness,
}

/// One durable record a real runtime `evidence_id` resolved to.
///
/// It carries the exact `(scope, call_index)` key and the snapshot digest of
/// the evidence version the host must acknowledge, plus the incomplete marker
/// so the caller can refuse a record whose facts were dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvidenceRecord {
    /// The record's real `(scope, call_index)` key.
    pub key: UnresolvedEffectKey,
    /// The record's exact snapshot digest at resolution time.
    pub snapshot_key: UnresolvedSnapshotKey,
    /// The record's current authoritative state.
    pub state: UnresolvedEffectState,
    /// Facts omitted after the per-record cap; `> 0` means incomplete evidence.
    pub facts_dropped: u64,
}

/// How one real runtime `evidence_id` maps onto the durable ledger.
///
/// A runtime marker carries the `evidence_id` the host already persisted, not
/// the ledger key. A host confirmation must be bound to exactly one record and
/// its exact snapshot digest *before* the host is asked; `Missing`,
/// `Ambiguous` and an incomplete record all fail closed. Nothing here guesses a
/// key and nothing re-snapshots the whole ledger after an approval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvidenceResolution {
    /// No record carries this evidence id.
    Missing,
    /// More than one record carries this evidence id: ambiguous.
    Ambiguous(usize),
    /// Exactly one record carries this evidence id.
    Unique(EvidenceRecord),
}

/// [`UnresolvedEffectLedger::record_dispatched`] 的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispatchClaim {
    /// 首次派发：已持久化为 `Outstanding`，调用方可以继续派发。
    Acquired,
    /// 该 `(scope, call_index)` 已经派发过：调用方**必须拒绝**本次重复派发，绝不重放。
    Existing(UnresolvedEffectState),
}

impl DispatchClaim {
    /// 本次申请是否拿到了派发权。
    pub const fn is_acquired(self) -> bool {
        matches!(self, Self::Acquired)
    }
}

/// [`UnresolvedEffectLedger::claim_dispatch`] 的结果。
///
/// 与 [`DispatchClaim`] 的区别在于：同一个 `flock` 事务里还重新检查了整个工作区是否
/// 仍有未确认的持久 `Unknown`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispatchAdmission {
    /// 首次派发：已持久化为 `Outstanding`，调用方可以继续派发。
    Acquired,
    /// 该 `(scope, call_index)` 已经派发过：调用方**必须拒绝**本次重复派发，绝不重放。
    Existing(UnresolvedEffectState),
    /// 工作区仍有未确认的持久 `Unknown`（可能是别的进程/session 在本次回合开始后新增
    /// 的）：拒绝新的写/外部派发，必须先由宿主持精确快照键确认。
    BlockedByUnknown {
        /// 当前仍未确认的 `Unknown` 记录数。
        pending: usize,
    },
    /// 账本尚未提交一次**完整**的全工作区覆盖初始化，或已持久记录了丢失/损坏的
    /// journal 证据（`coverage_incomplete`）。此时旧证据可能仍未被表示，拒绝任何新的
    /// 写/外部派发；这与 `BlockedByUnknown` 不同：它不是某条已知证据，而是「尚未证
    /// 明没有遗漏」的 fail-closed 状态，普通快照 ack 不能解除。
    BlockedByCoverage {
        /// 是否曾经成功提交过完整覆盖初始化。
        initialized: bool,
        /// 是否持久命中过丢失/损坏证据的 latch。
        incomplete: bool,
    },
}

impl DispatchAdmission {
    /// 本次申请是否拿到了派发权。
    pub const fn is_acquired(self) -> bool {
        matches!(self, Self::Acquired)
    }
}

/// 工作区账本的权威覆盖状态。
///
/// `initialized` 只在**首次**对全工作区 journal 的有界扫描完整且无丢失/损坏、并且
/// 所有真实旧 `Unknown` 都已成功迁移写入时，才在**同一个** `flock` 事务里原子置位。
/// 仅仅存在一个账本文件（例如只读打开或部分迁移失败创建了它）**不是**证据。
///
/// `incomplete` 是单调 latch：一旦有界扫描看见真实的丢失/裁剪证据或损坏/超限的
/// journal，就持久置位且**永不**被任何 ack 清除；新写/外部派发保持 fail-closed。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Coverage {
    /// 是否已提交过完整的初始覆盖迁移。
    pub initialized: bool,
    /// 是否持久命中过丢失/损坏证据的 latch。
    pub incomplete: bool,
}

impl Coverage {
    /// 新写/外部派发是否被覆盖状态拒绝。
    pub const fn blocks_new_side_effects(self) -> bool {
        self.incomplete || !self.initialized
    }
}

/// [`UnresolvedEffectLedger::commit_initial_coverage`] 的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoverageCommit {
    /// 本次事务提交了完整覆盖初始化。
    Initialized,
    /// 另一个写者（或更早的调用）已经提交过完整覆盖初始化。
    AlreadyInitialized,
    /// 本次事务持久化了一个新的丢失/损坏 latch。
    MarkedIncomplete,
    /// 丢失/损坏 latch 之前已经置位，本次不改动。
    AlreadyIncomplete,
}

/// 整个工作区账本的只读快照。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnresolvedEffectSnapshot {
    /// 账本文件是否存在。只读模式下不存在时不会创建任何东西。
    pub present: bool,
    /// 累计因容量/字节压力被裁剪的 `Resolved` 记录数（只裁剪终态 `Resolved`，
    /// 绝不裁剪 `Outstanding`/`Unknown`/`Acknowledged`）。
    pub resolved_trimmed: u64,
    /// 是否已提交一次完整覆盖初始化（见 [`Coverage`]）。
    pub coverage_initialized: bool,
    /// 是否持久命中丢失/损坏证据的 latch（见 [`Coverage`]）。
    pub coverage_incomplete: bool,
    /// 全部记录，按 `(scope, call_index)` 升序。
    pub entries: Vec<UnresolvedEffectObservation>,
}

impl UnresolvedEffectSnapshot {
    /// 记录条数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否没有任何记录。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 累计被裁剪的 `Resolved` 记录数。
    pub const fn resolved_trimmed(&self) -> u64 {
        self.resolved_trimmed
    }

    /// 是否已提交一次完整覆盖初始化。
    pub const fn coverage_initialized(&self) -> bool {
        self.coverage_initialized
    }

    /// 是否持久命中丢失/损坏证据的 fail-closed latch。
    pub const fn coverage_incomplete(&self) -> bool {
        self.coverage_incomplete
    }

    /// 必须由宿主确认才能继续写/外部副作用的条目（`state == Unknown`）。
    pub fn blocking(&self) -> impl Iterator<Item = &UnresolvedEffectObservation> + '_ {
        self.entries
            .iter()
            .filter(|entry| entry.record.state == UnresolvedEffectState::Unknown)
    }

    /// 是否存在必须由宿主确认的条目。
    pub fn is_blocked(&self) -> bool {
        self.blocking().next().is_some()
    }

    /// `state == Unknown` 的条目数。
    pub fn unknown_count(&self) -> usize {
        self.blocking().count()
    }

    /// `state == Outstanding` 的条目数。
    pub fn outstanding_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.record.state == UnresolvedEffectState::Outstanding)
            .count()
    }
}

/// 单条 `Outstanding` 记录的恢复结论。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// owner 已死或存活性不确定：保守升级为 `Unknown`。
    UpgradedToUnknown,
    /// owner 被正向确认为存活：保持 `Outstanding`，不误判死亡。
    KeptOutstanding,
    /// 已处于终态，不参与恢复。
    Terminal,
}

impl RecoveryOutcome {
    /// 稳定的 snake_case 名称。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UpgradedToUnknown => "upgraded_to_unknown",
            Self::KeptOutstanding => "kept_outstanding",
            Self::Terminal => "terminal",
        }
    }
}

/// 恢复报告中单条未决记录的结论。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutstandingRecovery {
    /// 主键。
    pub key: UnresolvedEffectKey,
    /// 恢复后的状态。
    pub state: UnresolvedEffectState,
    /// 恢复时的 owner 存活性观测。
    pub liveness: Liveness,
    /// 恢复结论。
    pub outcome: RecoveryOutcome,
    /// 恢复后该记录当前的精确快照确认键；用于把真实 ledger 身份与快照哈希播种到
    /// 运行内闸门，使宿主能对**精确现场** ack，而不是对重建的快照 ack。
    pub snapshot_key: UnresolvedSnapshotKey,
    /// 记录里最近一条真实事实的类型化原因；没有真实事实时为 `None`（绝不伪造）。
    pub last_reason: Option<UnknownFactReason>,
    /// 记录里最近一条真实事实的规范化操作标签；没有真实事实时为 `None`。
    pub last_operation: Option<String>,
}

/// [`UnresolvedEffectLedger::recover_outstanding`] 的报告。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryReport {
    /// 账本文件是否存在。
    pub present: bool,
    /// 本次报告中的升级是否**已经落盘**。只读模式恒为 `false`；没有升级需要落盘时
    /// 也是 `false`。为 `false` 时调用方不得把升级当作持久事实。
    pub persisted: bool,
    /// 累计因容量/字节压力被裁剪的 `Resolved` 记录数。
    pub resolved_trimmed: u64,
    /// 全部记录的结论，按 `(scope, call_index)` 升序。
    pub entries: Vec<OutstandingRecovery>,
}

impl RecoveryReport {
    /// 被保守升级为 `Unknown` 的条目。
    pub fn upgraded(&self) -> impl Iterator<Item = &OutstandingRecovery> + '_ {
        self.entries
            .iter()
            .filter(|entry| entry.outcome == RecoveryOutcome::UpgradedToUnknown)
    }

    /// 是否存在被升级的条目。
    pub fn has_upgrades(&self) -> bool {
        self.upgraded().next().is_some()
    }

    /// 被正向确认为存活、保持 `Outstanding` 的条目数。
    pub fn kept_outstanding(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.outcome == RecoveryOutcome::KeptOutstanding)
            .count()
    }
}

/// [`UnresolvedEffectLedger::record_unknown`] 的输入。
///
/// `target` 由本模块内部哈希，**原始值不会进入账本**。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownEffectInput {
    reason: UnknownFactReason,
    operation: String,
    target: Option<String>,
    evidence_id: Option<Uuid>,
}

impl UnknownEffectInput {
    /// 以规范化工具身份与类型化原因构造输入。
    pub fn new(operation: impl Into<String>, reason: UnknownFactReason) -> Self {
        Self {
            reason,
            operation: operation.into(),
            target: None,
            evidence_id: None,
        }
    }

    /// 附带原始 target；本模块只保存它的 `sha256` 摘要。
    #[must_use]
    pub fn with_target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }

    /// 附带调用方真实持有的原始 evidence id；不传即 `None`，绝不伪造。
    #[must_use]
    pub fn with_evidence_id(mut self, evidence_id: Uuid) -> Self {
        self.evidence_id = Some(evidence_id);
        self
    }

    /// 附带可选的原始 evidence id；`None` 表示没有真实标识可记录。
    #[must_use]
    pub fn with_optional_evidence_id(mut self, evidence_id: Option<Uuid>) -> Self {
        self.evidence_id = evidence_id;
        self
    }

    /// 类型化原因。
    pub fn reason(&self) -> UnknownFactReason {
        self.reason
    }

    /// 规范化操作标签。
    pub fn operation(&self) -> &str {
        &self.operation
    }

    /// 调用方提供的原始 evidence id（`None` 表示没有）。
    pub fn evidence_id(&self) -> Option<Uuid> {
        self.evidence_id
    }
}

/// 对原始 target 做域分隔 `sha256` 摘要。
///
/// 账本只保存该摘要，用于与 journal 的 canonical 目标做关联，绝不保存原始 target。
pub fn target_digest(target: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"mio-unresolved-effect-target-v1\0");
    hasher.update(target.as_bytes());
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// 账本
// ---------------------------------------------------------------------------

enum Backend {
    Disabled,
    ReadOnly {
        dir: PathBuf,
        file: PathBuf,
    },
    Persistent {
        dir: PathBuf,
        file: PathBuf,
        lock: PathBuf,
    },
}

impl Backend {
    fn data_file(&self) -> Option<&Path> {
        match self {
            Self::Disabled => None,
            Self::ReadOnly { file, .. } | Self::Persistent { file, .. } => Some(file),
        }
    }
}

/// 工作区级、有界的未决副作用账本。
///
/// 构造本身不产生任何磁盘写入；目录与文件在第一次写入时按需创建。同一实例内用
/// `Mutex` 串行，跨进程用账本叶子锁文件的 `flock` 串行。
pub struct UnresolvedEffectLedger {
    inner: Mutex<Backend>,
}

impl UnresolvedEffectLedger {
    /// 打开按工作区根目录持久化的读写账本。
    pub fn persistent(root: &Path) -> Self {
        let dir = root.join(LEDGER_DIR);
        let file = dir.join(LEDGER_FILE);
        let lock = dir.join(LEDGER_LOCK_FILE);
        Self {
            inner: Mutex::new(Backend::Persistent { dir, file, lock }),
        }
    }

    /// 打开只读账本：可以查询与计算恢复计划，但零创建、零写入。
    pub fn read_only(root: &Path) -> Self {
        let dir = root.join(LEDGER_DIR);
        let file = dir.join(LEDGER_FILE);
        Self {
            inner: Mutex::new(Backend::ReadOnly { dir, file }),
        }
    }

    /// 打开完全禁用的账本：不读写任何文件，所有操作返回 `Disabled`。
    pub fn disabled() -> Self {
        Self {
            inner: Mutex::new(Backend::Disabled),
        }
    }

    /// 当前工作模式。
    pub fn mode(&self) -> LedgerMode {
        match &*self.lock() {
            Backend::Disabled => LedgerMode::Disabled,
            Backend::ReadOnly { .. } => LedgerMode::ReadOnly,
            Backend::Persistent { .. } => LedgerMode::ReadWrite,
        }
    }

    /// 账本文件路径；禁用模式返回 `None`。
    pub fn path(&self) -> Option<PathBuf> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .data_file()
            .map(Path::to_path_buf)
    }

    /// 持久化一次派发意图，状态为 `Outstanding`。
    ///
    /// 必须在真实派发**之前**调用：记录先落盘，副作用后才可能发生。同一
    /// `(scope, call_index)` 重复调用返回 [`DispatchClaim::Existing`]，调用方必须拒绝
    /// 重放。账本不可写、损坏或满员时返回 `Err`，调用方应 fail-closed。
    pub fn record_dispatched(
        &self,
        key: &UnresolvedEffectKey,
    ) -> Result<DispatchClaim, UnresolvedEffectError> {
        validate_key(key)?;
        self.with_locked(|ledger| {
            if let Some(record) = ledger.find(key) {
                return Ok((DispatchClaim::Existing(record.state), false));
            }
            push_outstanding(ledger, key)?;
            Ok((DispatchClaim::Acquired, true))
        })
    }

    /// Claim a dispatch key and, for a write/external call, re-check the whole
    /// workspace's durable coverage epoch and pending unknowns inside the same
    /// `flock` transaction.
    ///
    /// A run-start seed is only a snapshot: another process or session can
    /// record a new `Unknown` while this process sits between turns. Checking
    /// the durable revision in the same locked transaction that claims the key
    /// closes that window, so a write/external dispatch is refused even when
    /// the in-memory guard was seeded before the new evidence existed. A record
    /// the host has already acknowledged does not block; an incomplete record
    /// (`facts_dropped > 0`) is `Unknown` and still blocks.
    ///
    /// Before a complete whole-workspace coverage epoch has been committed (or
    /// after a durable incomplete-coverage latch), **no** write/external
    /// dispatch is admitted at all: an un-migrated legacy unknown may still
    /// exist and no acknowledgement can clear that. A read-only call passes
    /// `false` because reads never carry an unknown side effect and a damaged
    /// or uninitialized ledger must never block them.
    pub fn claim_dispatch(
        &self,
        key: &UnresolvedEffectKey,
        check_durable_unknowns: bool,
    ) -> Result<DispatchAdmission, UnresolvedEffectError> {
        validate_key(key)?;
        self.with_locked(|ledger| {
            if let Some(record) = ledger.find(key) {
                return Ok((DispatchAdmission::Existing(record.state), false));
            }
            if check_durable_unknowns {
                if !ledger.coverage_initialized || ledger.coverage_incomplete {
                    return Ok((
                        DispatchAdmission::BlockedByCoverage {
                            initialized: ledger.coverage_initialized,
                            incomplete: ledger.coverage_incomplete,
                        },
                        false,
                    ));
                }
                let pending = ledger
                    .entries
                    .iter()
                    .filter(|record| record.state == UnresolvedEffectState::Unknown)
                    .count();
                if pending > 0 {
                    return Ok((DispatchAdmission::BlockedByUnknown { pending }, false));
                }
            }
            push_outstanding(ledger, key)?;
            Ok((DispatchAdmission::Acquired, true))
        })
    }

    /// The authoritative durable coverage state of the whole workspace.
    ///
    /// A fresh (or read-only) workspace reports `false`/`false`: only a real
    /// committed migration sets `initialized`. Merely finding a ledger file is
    /// never treated as proof of coverage.
    pub fn coverage(&self) -> Result<Coverage, UnresolvedEffectError> {
        Ok(self
            .load_current()?
            .map_or_else(Coverage::default, |ledger| Coverage {
                initialized: ledger.coverage_initialized,
                incomplete: ledger.coverage_incomplete,
            }))
    }

    /// Commit the initial whole-workspace coverage epoch, or latch durable
    /// incomplete coverage, in one `flock` transaction.
    ///
    /// The caller performs the bounded read-only journal scan first and passes
    /// the real migrated candidates plus whether that scan was *complete* and
    /// free of any loss/corruption. Only `complete` sets `initialized`;
    /// anything else persists the monotonic `incomplete` latch instead. A
    /// partial migration failure (for example the entry bound) returns `Err`
    /// with nothing persisted, so a later attempt cannot mistake it for a
    /// successful migration.
    pub fn commit_initial_coverage(
        &self,
        candidates: &[(UnresolvedEffectKey, UnknownEffectInput)],
        complete: bool,
    ) -> Result<CoverageCommit, UnresolvedEffectError> {
        for (key, input) in candidates {
            validate_key(key)?;
            validate_input(input)?;
        }
        self.with_locked(|ledger| {
            if ledger.coverage_incomplete {
                return Ok((CoverageCommit::AlreadyIncomplete, false));
            }
            if ledger.coverage_initialized {
                return Ok((CoverageCommit::AlreadyInitialized, false));
            }
            if !complete {
                ledger.coverage_incomplete = true;
                return Ok((CoverageCommit::MarkedIncomplete, true));
            }
            for (key, input) in candidates {
                if ledger.find(key).is_some() {
                    continue;
                }
                record_unknown_in(ledger, key, input.clone())?;
            }
            ledger.coverage_initialized = true;
            Ok((CoverageCommit::Initialized, true))
        })
    }

    /// Import already-scanned legacy unknowns after the coverage epoch is
    /// initialized.
    ///
    /// Keys already present in the ledger are skipped, so re-scanning a journal
    /// on every turn never grows a record or its evidence. A journal that could
    /// not be scanned completely is never passed here: the caller fails closed
    /// (and latches `incomplete`) instead.
    pub fn import_unknowns(
        &self,
        candidates: &[(UnresolvedEffectKey, UnknownEffectInput)],
    ) -> Result<usize, UnresolvedEffectError> {
        for (key, input) in candidates {
            validate_key(key)?;
            validate_input(input)?;
        }
        self.with_locked(|ledger| {
            let mut imported = 0usize;
            for (key, input) in candidates {
                if ledger.find(key).is_some() {
                    continue;
                }
                record_unknown_in(ledger, key, input.clone())?;
                imported += 1;
            }
            Ok((imported, imported > 0))
        })
    }

    /// Persist the monotonic incomplete-coverage latch.
    ///
    /// Called when a bounded whole-workspace scan cannot prove that no unknown
    /// was skipped (corruption, oversize, a bound exceeded). No acknowledgement
    /// ever clears it, and every new write/external dispatch stays refused.
    pub fn mark_coverage_incomplete(&self) -> Result<(), UnresolvedEffectError> {
        self.with_locked(|ledger| {
            if ledger.coverage_incomplete {
                return Ok(((), false));
            }
            ledger.coverage_incomplete = true;
            Ok(((), true))
        })
    }

    /// Retract an `Outstanding` dispatch intent that provably never reached
    /// the bus.
    ///
    /// The caller claimed the key and then failed before the engine dispatched
    /// it (the canonical journal write failed, say). Leaving the intent as
    /// `Outstanding` would let a later crash recovery report a false `Unknown`
    /// for a call that never ran. Retraction is allowed only for a pure
    /// `Outstanding` record with no unknown evidence, no recovery and no
    /// acknowledgement: any record that carries real evidence must be resolved
    /// or acknowledged instead. Nothing else is ever evicted.
    pub fn release_undispatched(
        &self,
        key: &UnresolvedEffectKey,
    ) -> Result<bool, UnresolvedEffectError> {
        validate_key(key)?;
        self.with_locked(|ledger| {
            let position = ledger.entries.iter().position(|record| {
                record.scope == key.scope && record.call_index == key.call_index
            });
            let Some(position) = position else {
                return Ok((false, false));
            };
            let record = &ledger.entries[position];
            if record.state != UnresolvedEffectState::Outstanding
                || record.has_unknown_evidence()
                || record.recovery.is_some()
                || record.acknowledgement.is_some()
            {
                return Err(UnresolvedEffectError::NotRetractable);
            }
            ledger.entries.remove(position);
            Ok((true, true))
        })
    }

    /// 记录一次不可确认的副作用：`Outstanding -> Unknown`（或补建记录直接进入
    /// `Unknown`）。
    ///
    /// 记录不存在时会补建：一次真实的 unknown 观测绝不能因为派发记录缺失而丢掉。
    /// 相同 `evidence_id` 的重复上报是幂等 no-op。对终态（`Resolved`/`Acknowledged`）
    /// 记录新的 unknown 证据返回 [`UnresolvedEffectError::IllegalTransition`]，调用方
    /// 必须 fail-closed，不得忽略该错误。
    pub fn record_unknown(
        &self,
        key: &UnresolvedEffectKey,
        input: UnknownEffectInput,
    ) -> Result<UnresolvedEffectObservation, UnresolvedEffectError> {
        validate_key(key)?;
        validate_input(&input)?;
        self.with_locked(|ledger| record_unknown_in(ledger, key, input))
    }

    /// 确认结果已知：`Outstanding -> Resolved`。
    ///
    /// 返回 `true` 表示本次完成了转移；`false` 表示无需处理（记录不存在或已是
    /// `Resolved`）。对 `Unknown`/`Acknowledged` 记录返回
    /// [`UnresolvedEffectError::IllegalTransition`]：未知副作用只能由宿主显式 ack 离开。
    pub fn resolve_confirmed(
        &self,
        key: &UnresolvedEffectKey,
    ) -> Result<bool, UnresolvedEffectError> {
        validate_key(key)?;
        self.with_locked(|ledger| {
            let Some(record) = ledger.find_mut(key) else {
                return Ok((false, false));
            };
            match record.state {
                UnresolvedEffectState::Outstanding => {
                    record.state = UnresolvedEffectState::Resolved;
                    record.revision = record.revision.saturating_add(1);
                    record.updated = now_secs();
                    Ok((true, true))
                }
                UnresolvedEffectState::Resolved => Ok((false, false)),
                from => Err(UnresolvedEffectError::IllegalTransition {
                    from,
                    to: UnresolvedEffectState::Resolved,
                }),
            }
        })
    }

    /// 宿主在确认现场后显式 ack 一条 `Unknown` 记录。
    ///
    /// `snapshot_key` 必须与账本当前证据版本**逐字相等**；否则返回
    /// [`UnresolvedEffectError::StaleConfirmation`]，绝不误 ack 之后新出现的证据。
    /// 证据不完整的记录（`facts_dropped > 0`）返回
    /// [`UnresolvedEffectError::IncompleteEvidence`]，保持 `Unknown` 与全部事实：
    /// 宿主不能确认一份它没有完整看到过的现场。原始 `facts` 保留；`note` 有界且不得
    /// 包含参数或密钥。
    pub fn acknowledge(
        &self,
        key: &UnresolvedEffectKey,
        snapshot_key: &UnresolvedSnapshotKey,
        note: Option<&str>,
    ) -> Result<UnresolvedEffectObservation, UnresolvedEffectError> {
        validate_key(key)?;
        let note = validate_note(note)?;
        self.with_locked(|ledger| {
            let Some(record) = ledger.find_mut(key) else {
                return Err(UnresolvedEffectError::NotFound);
            };
            let current = record.snapshot_key();
            if current != *snapshot_key {
                return Err(UnresolvedEffectError::StaleConfirmation);
            }
            match record.state {
                UnresolvedEffectState::Unknown => {
                    if record.facts_dropped > 0 {
                        // The host cannot confirm a site it never saw in full:
                        // a record with dropped facts is incomplete evidence.
                        // Keep the `Unknown` and every surviving fact.
                        return Err(UnresolvedEffectError::IncompleteEvidence);
                    }
                    let now = now_secs();
                    let liveness = probe_liveness(record.owner.as_ref());
                    record.acknowledgement = Some(Acknowledgement {
                        snapshot_key: current.as_str().to_owned(),
                        confirmed_at: now,
                        liveness,
                        note: note.unwrap_or_default(),
                    });
                    record.state = UnresolvedEffectState::Acknowledged;
                    // 只增加确认：facts / revision / 证据摘要都不变。
                    record.updated = now;
                    Ok((observe_record(record), true))
                }
                UnresolvedEffectState::Acknowledged => Ok((observe_record(record), false)),
                from => Err(UnresolvedEffectError::IllegalTransition {
                    from,
                    to: UnresolvedEffectState::Acknowledged,
                }),
            }
        })
    }

    /// 只读查询单条记录。
    pub fn lookup(
        &self,
        key: &UnresolvedEffectKey,
    ) -> Result<Option<UnresolvedEffectObservation>, UnresolvedEffectError> {
        validate_key(key)?;
        Ok(self
            .load_current()?
            .as_ref()
            .and_then(|ledger| ledger.find(key))
            .map(observe_record))
    }

    /// Resolve one real runtime `evidence_id` to its unique durable record.
    ///
    /// A runtime marker (an MCP/plugin transport or supervision unknown) carries
    /// the `evidence_id` the host already persisted, not the ledger key, so a
    /// host confirmation must be bound to the exact key and snapshot digest of
    /// the record that carries that id. The resolution is read-only and happens
    /// *before* the host is asked: it never re-snapshots the ledger and never
    /// invents an identity. No match, more than one match, or a record whose
    /// facts were dropped are reported distinctly so the caller can fail
    /// closed.
    pub fn resolve_evidence(
        &self,
        evidence_id: Uuid,
    ) -> Result<EvidenceResolution, UnresolvedEffectError> {
        let Some(ledger) = self.load_current()? else {
            return Ok(EvidenceResolution::Missing);
        };
        let matched: Vec<&UnresolvedEffectRecord> = ledger
            .entries
            .iter()
            .filter(|record| {
                record
                    .facts
                    .iter()
                    .any(|fact| fact.evidence_id == Some(evidence_id))
            })
            .collect();
        match matched.as_slice() {
            [] => Ok(EvidenceResolution::Missing),
            [record] => Ok(EvidenceResolution::Unique(EvidenceRecord {
                key: record.key(),
                snapshot_key: record.snapshot_key(),
                state: record.state,
                facts_dropped: record.facts_dropped,
            })),
            others => Ok(EvidenceResolution::Ambiguous(others.len())),
        }
    }

    /// 只读快照整个工作区账本。
    pub fn snapshot(&self) -> Result<UnresolvedEffectSnapshot, UnresolvedEffectError> {
        Ok(build_snapshot(self.load_current()?.as_ref()))
    }

    /// 跨重启恢复：把 owner 非正向存活的 `Outstanding` 保守升级为 `Unknown`。
    ///
    /// - owner 被正向确认为存活时保持 `Outstanding`，绝不误判死亡。
    /// - owner 已死或存活性不确定时一律升级为 `Unknown`，绝不宣称「没发生过」。
    /// - 只读账本会给出恢复计划但**不落盘**（`persisted: false`），调用方不得据此
    ///   放行新的写/外部副作用。
    pub fn recover_outstanding(&self) -> Result<RecoveryReport, UnresolvedEffectError> {
        let guard = self.lock();
        match &*guard {
            Backend::Disabled => Err(UnresolvedEffectError::Disabled),
            Backend::ReadOnly { file, .. } => {
                let file = file.clone();
                let mut ledger = load_optional_ledger(&file)?;
                let present = ledger.is_some();
                let resolved_trimmed = ledger.as_ref().map_or(0, |ledger| ledger.resolved_trimmed);
                let entries = match ledger.as_mut() {
                    Some(ledger) => recover_ledger(ledger, now_secs()).0,
                    None => Vec::new(),
                };
                Ok(RecoveryReport {
                    present,
                    persisted: false,
                    resolved_trimmed,
                    entries,
                })
            }
            Backend::Persistent { dir, file, lock } => {
                let (dir, file, lock) = (dir.clone(), file.clone(), lock.clone());
                // Nothing to recover and no ledger exists: do not create the
                // directory or file just because a turn started. Only a real
                // upgrade takes the lock and persists.
                if read_bounded_file(&file, MAX_LEDGER_FILE_SIZE)?.is_none() {
                    return Ok(RecoveryReport {
                        present: false,
                        persisted: false,
                        resolved_trimmed: 0,
                        entries: Vec::new(),
                    });
                }
                with_ledger_locked(&dir, &file, &lock, |ledger| {
                    let (mut entries, dirty) = recover_ledger(ledger, now_secs());
                    entries.sort_by(|left, right| left.key.cmp(&right.key));
                    Ok((
                        RecoveryReport {
                            present: true,
                            persisted: dirty,
                            resolved_trimmed: ledger.resolved_trimmed,
                            entries,
                        },
                        dirty,
                    ))
                })
            }
        }
    }

    fn with_locked<R>(
        &self,
        mutate: impl FnOnce(&mut LedgerFile) -> Result<(R, bool), UnresolvedEffectError>,
    ) -> Result<R, UnresolvedEffectError> {
        let mut guard = self.lock();
        match &mut *guard {
            Backend::Disabled => Err(UnresolvedEffectError::Disabled),
            Backend::ReadOnly { .. } => Err(UnresolvedEffectError::ReadOnly),
            Backend::Persistent { dir, file, lock } => {
                let (dir, file, lock) = (dir.clone(), file.clone(), lock.clone());
                with_ledger_locked(&dir, &file, &lock, mutate)
            }
        }
    }

    /// 只读加载当前账本：不加锁、不创建任何文件或目录。`rename` 原子替换保证读到的是
    /// 完整的旧文件或新文件，因此锁外读取不会看到半份数据。
    fn load_current(&self) -> Result<Option<LedgerFile>, UnresolvedEffectError> {
        let guard = self.lock();
        let Some(file) = guard.data_file().map(Path::to_path_buf) else {
            return Err(UnresolvedEffectError::Disabled);
        };
        load_optional_ledger(&file)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Backend> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

// ---------------------------------------------------------------------------
// 记录视图与摘要
// ---------------------------------------------------------------------------

fn observe_record(record: &UnresolvedEffectRecord) -> UnresolvedEffectObservation {
    UnresolvedEffectObservation {
        record: record.clone(),
        snapshot_key: record.snapshot_key(),
        // 终态不参与 owner 探测：终态不回退，也不再据此放行任何操作。
        liveness: if record.state.is_pending() {
            probe_liveness(record.owner.as_ref())
        } else {
            Liveness::Unknown
        },
    }
}

fn build_snapshot(ledger: Option<&LedgerFile>) -> UnresolvedEffectSnapshot {
    let Some(ledger) = ledger else {
        return UnresolvedEffectSnapshot {
            present: false,
            resolved_trimmed: 0,
            coverage_initialized: false,
            coverage_incomplete: false,
            entries: Vec::new(),
        };
    };
    let mut entries: Vec<UnresolvedEffectObservation> =
        ledger.entries.iter().map(observe_record).collect();
    entries.sort_by_key(|entry| entry.record.key());
    UnresolvedEffectSnapshot {
        present: true,
        resolved_trimmed: ledger.resolved_trimmed,
        coverage_initialized: ledger.coverage_initialized,
        coverage_incomplete: ledger.coverage_incomplete,
        entries,
    }
}

/// Append a fresh `Outstanding` dispatch-intent record for `key`.
///
/// Shared by [`UnresolvedEffectLedger::record_dispatched`],
/// [`UnresolvedEffectLedger::claim_dispatch`] and the implicit record created
/// by [`UnresolvedEffectLedger::record_unknown`], so every path has the same
/// identity and owner capture.
fn push_outstanding(
    ledger: &mut LedgerFile,
    key: &UnresolvedEffectKey,
) -> Result<(), UnresolvedEffectError> {
    ledger.reserve_entry()?;
    let now = now_secs();
    ledger.entries.push(UnresolvedEffectRecord {
        scope: key.scope.clone(),
        call_index: key.call_index,
        state: UnresolvedEffectState::Outstanding,
        revision: 1,
        recorded_at: now,
        updated: now,
        owner: capture_owner(),
        facts: Vec::new(),
        facts_dropped: 0,
        recovery: None,
        acknowledgement: None,
    });
    Ok(())
}

/// Record one unknown fact on `key` inside an already-locked ledger.
///
/// This is the shared body of [`UnresolvedEffectLedger::record_unknown`], the
/// initial migration commit, and the post-initialization legacy import, so all
/// three paths agree on identity, idempotence and the no-eviction rule. The
/// caller must have validated the key and input already.
fn record_unknown_in(
    ledger: &mut LedgerFile,
    key: &UnresolvedEffectKey,
    input: UnknownEffectInput,
) -> Result<(UnresolvedEffectObservation, bool), UnresolvedEffectError> {
    if ledger.find(key).is_none() {
        push_outstanding(ledger, key)?;
    }
    let Some(record) = ledger.find_mut(key) else {
        return Err(UnresolvedEffectError::Ledger(
            "the unresolved effect record disappeared inside the ledger lock".into(),
        ));
    };
    if let Some(evidence_id) = input.evidence_id
        && record
            .facts
            .iter()
            .any(|fact| fact.evidence_id == Some(evidence_id))
    {
        // 同一份证据重复上报：不改变证据版本，旧快照键仍然有效。
        return Ok((observe_record(record), false));
    }
    if let UnresolvedEffectState::Acknowledged | UnresolvedEffectState::Resolved = record.state {
        return Err(UnresolvedEffectError::IllegalTransition {
            from: record.state,
            to: UnresolvedEffectState::Unknown,
        });
    }
    let now = now_secs();
    if record.facts.len() >= MAX_FACTS_PER_RECORD {
        // 有界但绝不驱逐：只累计被省略的条数，并推进证据版本。
        record.facts_dropped = record.facts_dropped.saturating_add(1);
    } else {
        let fact_seq = record.facts.len() as u64 + 1;
        record.facts.push(UnknownEffectFact {
            fact_seq,
            reason: input.reason,
            operation: input.operation.clone(),
            target_sha256: input
                .target
                .as_deref()
                .map_or_else(String::new, target_digest),
            evidence_id: input.evidence_id,
            observed: now,
        });
    }
    record.state = UnresolvedEffectState::Unknown;
    record.revision = record.revision.saturating_add(1);
    record.updated = now;
    Ok((observe_record(record), true))
}

fn recover_ledger(ledger: &mut LedgerFile, now: u64) -> (Vec<OutstandingRecovery>, bool) {
    let mut entries = Vec::with_capacity(ledger.entries.len());
    let mut dirty = false;
    for record in &mut ledger.entries {
        let liveness = if record.state.is_pending() {
            probe_liveness(record.owner.as_ref())
        } else {
            Liveness::Unknown
        };
        let outcome = match record.state {
            UnresolvedEffectState::Outstanding if liveness.is_live() => {
                RecoveryOutcome::KeptOutstanding
            }
            UnresolvedEffectState::Outstanding => {
                record.state = UnresolvedEffectState::Unknown;
                record.recovery = Some(RecoveryRecord {
                    recovered_at: now,
                    liveness,
                });
                record.revision = record.revision.saturating_add(1);
                record.updated = now;
                dirty = true;
                RecoveryOutcome::UpgradedToUnknown
            }
            _ => RecoveryOutcome::Terminal,
        };
        let last = record.facts.last();
        entries.push(OutstandingRecovery {
            key: record.key(),
            state: record.state,
            liveness,
            outcome,
            snapshot_key: record.snapshot_key(),
            last_reason: last.map(|fact| fact.reason),
            last_operation: last.map(|fact| fact.operation.clone()),
        });
    }
    entries.sort_by(|left, right| left.key.cmp(&right.key));
    (entries, dirty)
}

/// 精确确认键的摘要输入：`(scope, call_index, revision, 事实集合, facts_dropped, 恢复记录)`。
///
/// **不包含** `state` 与 `acknowledgement`：ack 只增加确认，不改变被确认的证据，因此
/// ack 前后摘要一致，落盘后可在加载时重新校验。`revision` 保证任何证据或状态变化都会
/// 改变摘要，从而让旧快照键立刻失效。
fn evidence_digest(record: &UnresolvedEffectRecord) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"mio-unresolved-effect-ack-v1\0");
    hasher.update(record.scope.as_bytes());
    hasher.update(b"\0");
    hasher.update(record.call_index.to_be_bytes());
    hasher.update(b"\0");
    hasher.update(record.revision.to_be_bytes());
    hasher.update(b"\0");
    hasher.update(record.facts_dropped.to_be_bytes());
    hasher.update(b"\0");
    hasher.update((record.facts.len() as u64).to_be_bytes());
    hasher.update(b"\0");
    for fact in &record.facts {
        hasher.update(fact.fact_seq.to_be_bytes());
        hasher.update(b"\0");
        hasher.update(fact.reason.as_str().as_bytes());
        hasher.update(b"\0");
        hasher.update(fact.operation.as_bytes());
        hasher.update(b"\0");
        hasher.update(fact.target_sha256.as_bytes());
        hasher.update(b"\0");
        match fact.evidence_id {
            Some(evidence_id) => hasher.update(evidence_id.as_bytes()),
            None => hasher.update(b"-"),
        };
        hasher.update(b"\0");
        hasher.update(fact.observed.to_be_bytes());
        hasher.update(b"\0");
    }
    match &record.recovery {
        Some(recovery) => {
            hasher.update(b"recovery\0");
            hasher.update(recovery.liveness.as_str().as_bytes());
            hasher.update(b"\0");
            hasher.update(recovery.recovered_at.to_be_bytes());
            hasher.update(b"\0");
        }
        None => hasher.update(b"no-recovery\0"),
    }
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// 校验
// ---------------------------------------------------------------------------

fn validate_key(key: &UnresolvedEffectKey) -> Result<(), UnresolvedEffectError> {
    let scope = key.scope.trim();
    if scope.is_empty() {
        return Err(UnresolvedEffectError::InvalidKey(
            "scope must not be empty or whitespace-only".into(),
        ));
    }
    if key.scope.chars().count() > MAX_SCOPE_CHARS {
        return Err(UnresolvedEffectError::InvalidKey(format!(
            "scope must be at most {MAX_SCOPE_CHARS} characters"
        )));
    }
    if key.scope.chars().any(char::is_control) {
        return Err(UnresolvedEffectError::InvalidKey(
            "scope must not contain control characters".into(),
        ));
    }
    if key.call_index == 0 {
        return Err(UnresolvedEffectError::InvalidKey(
            "call_index must be at least 1".into(),
        ));
    }
    Ok(())
}

fn validate_operation(operation: &str) -> Result<(), UnresolvedEffectError> {
    if operation.trim().is_empty() {
        return Err(UnresolvedEffectError::InvalidKey(
            "operation must not be empty or whitespace-only".into(),
        ));
    }
    if operation.chars().count() > MAX_OPERATION_CHARS {
        return Err(UnresolvedEffectError::InvalidKey(format!(
            "operation must be at most {MAX_OPERATION_CHARS} characters"
        )));
    }
    if operation.chars().any(char::is_control) {
        return Err(UnresolvedEffectError::InvalidKey(
            "operation must not contain control characters".into(),
        ));
    }
    Ok(())
}

fn validate_input(input: &UnknownEffectInput) -> Result<(), UnresolvedEffectError> {
    validate_operation(&input.operation)?;
    if let Some(target) = &input.target
        && target.len() > MAX_TARGET_BYTES
    {
        return Err(UnresolvedEffectError::InvalidKey(format!(
            "target must be at most {MAX_TARGET_BYTES} bytes; hash or drop longer targets"
        )));
    }
    Ok(())
}

fn validate_note(note: Option<&str>) -> Result<Option<String>, UnresolvedEffectError> {
    let Some(note) = note else {
        return Ok(None);
    };
    if note.chars().count() > MAX_NOTE_CHARS {
        return Err(UnresolvedEffectError::InvalidKey(format!(
            "acknowledgement note must be at most {MAX_NOTE_CHARS} characters"
        )));
    }
    if note.chars().any(char::is_control) {
        return Err(UnresolvedEffectError::InvalidKey(
            "acknowledgement note must not contain control characters".into(),
        ));
    }
    Ok(Some(note.to_owned()))
}

/// 反序列化时校验 owner 字段，避免坏元数据伪装成旧记录或可信 owner。
fn validate_owner(owner: &EffectOwner) -> Result<(), UnresolvedEffectError> {
    if owner.pid == 0 {
        return Err(UnresolvedEffectError::Ledger(
            "owner pid must not be zero".into(),
        ));
    }
    if owner.start_time_ticks == 0 {
        return Err(UnresolvedEffectError::Ledger(
            "owner start time must not be zero".into(),
        ));
    }
    for (field, value) in [
        ("host_id", &owner.host_id),
        ("boot_id", &owner.boot_id),
        ("pid_namespace", &owner.pid_namespace),
    ] {
        if !is_sha256_hex(value) {
            return Err(UnresolvedEffectError::Ledger(format!(
                "owner {field} must be a {SHA256_HEX_CHARS}-character lowercase sha256 digest"
            )));
        }
    }
    Ok(())
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == SHA256_HEX_CHARS
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn ledger_full_error() -> UnresolvedEffectError {
    UnresolvedEffectError::Ledger(format!(
        "the unresolved effect ledger is full ({MAX_LEDGER_ENTRIES} entries of unresolved \
         outstanding/unknown effects); refusing a new dispatch so no unresolved evidence is \
         lost. Resolve or acknowledge the pending unknowns first, or archive the whole \
         workspace after exporting it; never delete {LEDGER_DIR}/{LEDGER_FILE} while it \
         still holds unresolved evidence, because that destroys the record of effects that \
         may already have happened"
    ))
}

/// 字节压力下只有终态 `Resolved` 可裁剪；连 `Resolved` 都没有时明确拒绝，
/// 绝不为腾空间而驱逐任何未决（`Outstanding`/`Unknown`）或仍带未知证据的
/// `Acknowledged` 记录。
fn ledger_too_large_error() -> UnresolvedEffectError {
    UnresolvedEffectError::Ledger(format!(
        "the unresolved effect ledger would exceed {MAX_LEDGER_FILE_SIZE} bytes even after \
         trimming every terminal resolved record; refusing to write so that no unresolved \
         (outstanding/unknown) evidence is evicted. Acknowledge or resolve the pending \
         records first"
    ))
}

// ---------------------------------------------------------------------------
// 持久化
// ---------------------------------------------------------------------------

/// 账本文件的持久化结构。
#[derive(Clone, Debug, Serialize, Deserialize)]
struct LedgerFile {
    version: u32,
    updated: u64,
    /// 累计被裁剪的 `Resolved` 记录数；旧账本缺省为 0。
    #[serde(default, skip_serializing_if = "is_zero")]
    resolved_trimmed: u64,
    /// 是否已提交完整覆盖初始化。旧账本缺省为 `false`（fail-closed）。
    #[serde(default, skip_serializing_if = "is_false")]
    coverage_initialized: bool,
    /// 丢失/损坏证据的单调 latch。旧账本缺省为 `false`。
    #[serde(default, skip_serializing_if = "is_false")]
    coverage_incomplete: bool,
    entries: Vec<UnresolvedEffectRecord>,
}

impl LedgerFile {
    fn empty() -> Self {
        Self {
            version: LEDGER_VERSION,
            updated: now_secs(),
            resolved_trimmed: 0,
            coverage_initialized: false,
            coverage_incomplete: false,
            entries: Vec::new(),
        }
    }

    /// 只裁剪终态 `Resolved` 记录并计入 `resolved_trimmed`，返回裁剪条数。
    ///
    /// `Resolved` 是「结果已确认已知」的终态，不携带任何未知证据，因此裁剪它们
    /// 不会丢失未决事实。`Outstanding`/`Unknown` 一律不裁剪；`Acknowledged` 仍携带
    /// 原始未知事实，也一律保留，优先裁剪 `Resolved`。
    fn trim_resolved_records(&mut self) -> u64 {
        let before = self.entries.len();
        self.entries
            .retain(|record| record.state != UnresolvedEffectState::Resolved);
        let dropped = (before - self.entries.len()) as u64;
        self.resolved_trimmed = self.resolved_trimmed.saturating_add(dropped);
        dropped
    }

    /// 为一条新记录预留槽位。
    ///
    /// 只在容量压力下先裁剪终态 `Resolved`（并计数）；若裁剪后仍满员，说明剩余条目
    /// 都是未决的 `Outstanding`/`Unknown` 或仍带证据的 `Acknowledged`，此时 fail-closed，
    /// 绝不驱逐未决。
    fn reserve_entry(&mut self) -> Result<(), UnresolvedEffectError> {
        if self.entries.len() >= MAX_LEDGER_ENTRIES {
            self.trim_resolved_records();
        }
        if self.entries.len() >= MAX_LEDGER_ENTRIES {
            return Err(ledger_full_error());
        }
        Ok(())
    }

    fn find(&self, key: &UnresolvedEffectKey) -> Option<&UnresolvedEffectRecord> {
        self.entries
            .iter()
            .find(|record| record.scope == key.scope && record.call_index == key.call_index)
    }

    fn find_mut(&mut self, key: &UnresolvedEffectKey) -> Option<&mut UnresolvedEffectRecord> {
        self.entries
            .iter_mut()
            .find(|record| record.scope == key.scope && record.call_index == key.call_index)
    }
}

fn validate_ledger(ledger: &LedgerFile) -> Result<(), UnresolvedEffectError> {
    if ledger.version != LEDGER_VERSION {
        return Err(UnresolvedEffectError::Ledger(format!(
            "unsupported ledger version {} (expected {LEDGER_VERSION})",
            ledger.version
        )));
    }
    if ledger.entries.len() > MAX_LEDGER_ENTRIES {
        return Err(ledger_full_error());
    }
    let mut keys = std::collections::BTreeSet::new();
    for record in &ledger.entries {
        validate_record(record)?;
        if !keys.insert(record.key()) {
            return Err(UnresolvedEffectError::Ledger(
                "duplicate (scope, call_index) in the unresolved effect ledger".into(),
            ));
        }
    }
    Ok(())
}

fn validate_record(record: &UnresolvedEffectRecord) -> Result<(), UnresolvedEffectError> {
    validate_key(&record.key())?;
    if record.revision == 0 {
        return Err(UnresolvedEffectError::Ledger(
            "record revision must be positive".into(),
        ));
    }
    if record.facts.len() > MAX_FACTS_PER_RECORD {
        return Err(UnresolvedEffectError::Ledger(format!(
            "record holds more than {MAX_FACTS_PER_RECORD} facts"
        )));
    }
    let mut evidence_ids = std::collections::BTreeSet::new();
    for (expected_seq, fact) in (1u64..).zip(record.facts.iter()) {
        if fact.fact_seq != expected_seq {
            return Err(UnresolvedEffectError::Ledger(
                "record facts must be dense, ascending fact_seq starting at 1".into(),
            ));
        }
        validate_operation(&fact.operation)?;
        if !fact.target_sha256.is_empty() && !is_sha256_hex(&fact.target_sha256) {
            return Err(UnresolvedEffectError::Ledger(
                "fact target digest must be a lowercase sha256 digest".into(),
            ));
        }
        if let Some(evidence_id) = fact.evidence_id
            && !evidence_ids.insert(evidence_id)
        {
            return Err(UnresolvedEffectError::Ledger(
                "record repeats the same evidence id".into(),
            ));
        }
    }
    if let Some(owner) = &record.owner {
        validate_owner(owner)?;
    }
    let carries_evidence = record.has_unknown_evidence() || record.recovery.is_some();
    if carries_evidence
        && matches!(
            record.state,
            UnresolvedEffectState::Resolved | UnresolvedEffectState::Outstanding
        )
    {
        return Err(UnresolvedEffectError::Ledger(
            "a resolved or outstanding record must not carry unknown evidence".into(),
        ));
    }
    match &record.acknowledgement {
        Some(acknowledgement) => {
            if record.state != UnresolvedEffectState::Acknowledged {
                return Err(UnresolvedEffectError::Ledger(
                    "acknowledgement on a non-acknowledged record".into(),
                ));
            }
            if !is_sha256_hex(&acknowledgement.snapshot_key) {
                return Err(UnresolvedEffectError::Ledger(
                    "acknowledgement snapshot key must be a lowercase sha256 digest".into(),
                ));
            }
            if acknowledgement.note.chars().count() > MAX_NOTE_CHARS
                || acknowledgement.note.chars().any(char::is_control)
            {
                return Err(UnresolvedEffectError::Ledger(
                    "acknowledgement note is over the bound or contains control characters".into(),
                ));
            }
            if acknowledgement.snapshot_key != evidence_digest(record) {
                return Err(UnresolvedEffectError::Ledger(
                    "acknowledgement does not match its evidence".into(),
                ));
            }
        }
        None if record.state == UnresolvedEffectState::Acknowledged => {
            return Err(UnresolvedEffectError::Ledger(
                "acknowledged record without an acknowledgement".into(),
            ));
        }
        None => {}
    }
    Ok(())
}

fn with_ledger_locked<R>(
    dir: &Path,
    file: &Path,
    lock: &Path,
    mutate: impl FnOnce(&mut LedgerFile) -> Result<(R, bool), UnresolvedEffectError>,
) -> Result<R, UnresolvedEffectError> {
    ensure_ledger_dir(dir)?;
    let lock_file = open_lock_file(lock)?;
    lock_file.lock().map_err(|error| {
        UnresolvedEffectError::Ledger(format!(
            "cannot lock the unresolved effect ledger {}: {error}",
            lock.display()
        ))
    })?;
    let mut ledger = load_optional_ledger(file)?.unwrap_or_else(LedgerFile::empty);
    let (result, dirty) = mutate(&mut ledger)?;
    if dirty {
        ledger.updated = now_secs();
        persist_ledger(dir, file, &mut ledger)?;
    }
    drop(lock_file);
    Ok(result)
}

fn ensure_ledger_dir(dir: &Path) -> Result<(), UnresolvedEffectError> {
    let product = dir.parent().ok_or_else(|| {
        UnresolvedEffectError::Ledger("the ledger directory has no parent".into())
    })?;
    for directory in [product, dir] {
        match fs::symlink_metadata(directory) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(UnresolvedEffectError::Ledger(
                    "the ledger directories must be real directories, not symlinks".into(),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // 并发首建时另一个进程可能已经创建；`create_dir_all` 对已存在目录幂等，
                // 随后重新确认不是 symlink。
                fs::create_dir_all(directory).map_err(|error| {
                    UnresolvedEffectError::Ledger(format!(
                        "cannot create the ledger directory {}: {error}",
                        directory.display()
                    ))
                })?;
                let metadata = fs::symlink_metadata(directory).map_err(|error| {
                    UnresolvedEffectError::Ledger(format!(
                        "cannot inspect the ledger directory {}: {error}",
                        directory.display()
                    ))
                })?;
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return Err(UnresolvedEffectError::Ledger(
                        "the ledger directories must be real directories, not symlinks".into(),
                    ));
                }
            }
            Err(error) => {
                return Err(UnresolvedEffectError::Ledger(format!(
                    "cannot inspect the ledger directory {}: {error}",
                    directory.display()
                )));
            }
        }
    }
    set_private_dir(dir)
}

/// 只读加载：文件不存在返回 `None`；损坏、版本不符、超限、符号链接一律 fail-closed。
fn load_optional_ledger(path: &Path) -> Result<Option<LedgerFile>, UnresolvedEffectError> {
    let Some(bytes) = read_bounded_file(path, MAX_LEDGER_FILE_SIZE)? else {
        return Ok(None);
    };
    let ledger: LedgerFile = serde_json::from_slice(&bytes).map_err(|error| {
        UnresolvedEffectError::Ledger(format!(
            "corrupt unresolved effect ledger {}: {error}",
            path.display()
        ))
    })?;
    validate_ledger(&ledger)?;
    Ok(Some(ledger))
}

fn read_bounded_file(path: &Path, max: usize) -> Result<Option<Vec<u8>>, UnresolvedEffectError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(UnresolvedEffectError::Ledger(format!(
                "cannot inspect the unresolved effect ledger {}: {error}",
                path.display()
            )));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(UnresolvedEffectError::Ledger(format!(
            "the unresolved effect ledger {} must be a regular file, not a symlink",
            path.display()
        )));
    }
    if metadata.len() > max as u64 {
        return Err(UnresolvedEffectError::Ledger(format!(
            "the unresolved effect ledger {} exceeds {max} bytes",
            path.display()
        )));
    }
    let file = open_read_no_follow(path)?;
    let mut bytes = Vec::new();
    file.take((max as u64) + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            UnresolvedEffectError::Ledger(format!(
                "cannot read the unresolved effect ledger {}: {error}",
                path.display()
            ))
        })?;
    if bytes.len() > max {
        return Err(UnresolvedEffectError::Ledger(format!(
            "the unresolved effect ledger {} exceeds {max} bytes",
            path.display()
        )));
    }
    Ok(Some(bytes))
}

fn persist_ledger(
    dir: &Path,
    path: &Path,
    ledger: &mut LedgerFile,
) -> Result<(), UnresolvedEffectError> {
    if ledger.entries.len() > MAX_LEDGER_ENTRIES {
        return Err(ledger_full_error());
    }
    // 字节压力下先裁剪终态 `Resolved`（并计数），再序列化；若连 `Resolved` 都
    // 没有可裁剪的，明确 fail-closed，绝不驱逐未决或 `Acknowledged` 事实。
    let payload = loop {
        let payload = serde_json::to_vec_pretty(ledger).map_err(|error| {
            UnresolvedEffectError::Ledger(format!(
                "cannot serialize the unresolved effect ledger: {error}"
            ))
        })?;
        if payload.len() <= MAX_LEDGER_FILE_SIZE {
            break payload;
        }
        if ledger.trim_resolved_records() == 0 {
            return Err(ledger_too_large_error());
        }
    };
    let mut temp = NamedTempFile::new_in(dir).map_err(|error| {
        UnresolvedEffectError::Ledger(format!(
            "cannot create a ledger tempfile in {}: {error}",
            dir.display()
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o600)).map_err(|error| {
            UnresolvedEffectError::Ledger(format!("cannot set the ledger mode 0600: {error}"))
        })?;
    }
    temp.write_all(&payload).map_err(|error| {
        UnresolvedEffectError::Ledger(format!(
            "cannot write the unresolved effect ledger: {error}"
        ))
    })?;
    temp.as_file().sync_all().map_err(|error| {
        UnresolvedEffectError::Ledger(format!("cannot sync the unresolved effect ledger: {error}"))
    })?;
    temp.persist(path).map_err(|error| {
        UnresolvedEffectError::Ledger(format!(
            "cannot persist the unresolved effect ledger to {}: {error}",
            path.display()
        ))
    })?;
    // rename 先于目录 fsync：掉电也不能让「派发记录先于副作用落盘」的承诺翻转。
    #[cfg(unix)]
    {
        File::open(dir)
            .and_then(|file| file.sync_all())
            .map_err(|error| {
                UnresolvedEffectError::Ledger(format!(
                    "cannot sync the ledger directory {}: {error}",
                    dir.display()
                ))
            })?;
    }
    Ok(())
}

fn open_lock_file(path: &Path) -> Result<File, UnresolvedEffectError> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        return Err(UnresolvedEffectError::Ledger(
            "the ledger lock must not be a symlink".into(),
        ));
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
        UnresolvedEffectError::Ledger(format!(
            "cannot open the ledger lock {}: {error}",
            path.display()
        ))
    })?;
    if !file
        .metadata()
        .map_err(|error| UnresolvedEffectError::Ledger(error.to_string()))?
        .is_file()
    {
        return Err(UnresolvedEffectError::Ledger(
            "the ledger lock must be a regular file".into(),
        ));
    }
    Ok(file)
}

fn open_read_no_follow(path: &Path) -> Result<File, UnresolvedEffectError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path).map_err(|error| {
        UnresolvedEffectError::Ledger(format!(
            "cannot read the unresolved effect ledger {}: {error}",
            path.display()
        ))
    })
}

#[cfg(unix)]
fn set_private_dir(dir: &Path) -> Result<(), UnresolvedEffectError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|error| {
        UnresolvedEffectError::Ledger(format!(
            "cannot set the ledger directory mode 0700: {error}"
        ))
    })
}

#[cfg(not(unix))]
fn set_private_dir(_dir: &Path) -> Result<(), UnresolvedEffectError> {
    Ok(())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------------------
// owner 身份与 liveness 观测
// ---------------------------------------------------------------------------

/// 采集**当前进程**的 owner 身份元数据。
///
/// 返回 `None` 表示当前平台不支持或身份不可得；对应记录只能按 [`Liveness::Unknown`]
/// 处理（恢复时保守升级为 `Unknown` 副作用），绝不猜测存活或死亡。
pub fn capture_owner() -> Option<EffectOwner> {
    capture_owner_for_pid(std::process::id())
}

/// 采集指定进程的 owner 身份元数据（只读观测，不写账本）。
///
/// Linux 上由 `pid` + 启动时刻 + host 摘要 + boot 摘要 + PID namespace 摘要组成。返回
/// 值只含定长摘要，不含机器原始标识。
#[cfg(target_os = "linux")]
pub fn capture_owner_for_pid(pid: u32) -> Option<EffectOwner> {
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
    Some(EffectOwner {
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
pub fn capture_owner_for_pid(_pid: u32) -> Option<EffectOwner> {
    None
}

/// 对一条 owner 做存活性探测。只读，不接触账本。
#[cfg(target_os = "linux")]
fn probe_liveness(owner: Option<&EffectOwner>) -> Liveness {
    let Some(owner) = owner else {
        // 旧记录没有 owner，或采集失败：只能是未知。未知不等于死亡。
        return Liveness::Unknown;
    };
    let Some(local) = LocalIdentity::resolve() else {
        return Liveness::Unknown;
    };
    if owner.host_id != local.host_id {
        // 工作区可能被多台机器共享：其它 host 上的 owner 不能被本机判死。
        return Liveness::Unknown;
    }
    if owner.boot_id != local.boot_id {
        // 同一 host 已经重启：上一个 boot 的进程必然不存在。
        return Liveness::Stale;
    }
    if owner.pid_namespace != local.pid_namespace {
        // PID 只在同一个 PID namespace 内可比，否则数字没有意义。
        return Liveness::Unknown;
    }
    probe_process_liveness(owner)
}

/// 非 Linux 平台不支持进程身份证据，一律返回未知。
#[cfg(not(target_os = "linux"))]
fn probe_liveness(_owner: Option<&EffectOwner>) -> Liveness {
    Liveness::Unknown
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
        Err(error) if error.kind() == io::ErrorKind::NotFound => return ProcProbe::Missing,
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
fn probe_process_liveness(owner: &EffectOwner) -> Liveness {
    match probe_process(owner.pid) {
        ProcProbe::Missing => {
            if proc_hidepid_active() {
                // /proc 隐藏了其它用户的进程：无法区分「已消失」和「不可见」。
                Liveness::Unknown
            } else {
                Liveness::Stale
            }
        }
        ProcProbe::Unreadable => Liveness::Unknown,
        ProcProbe::Found {
            state,
            start_time_ticks,
        } => {
            if start_time_ticks != owner.start_time_ticks {
                // 同一个 PID 已属于另一个进程：原 owner 已消失。
                Liveness::Stale
            } else if matches!(state, b'Z' | b'X' | b'x') {
                // zombie / dead 进程已经结束，不构成存活任务。
                Liveness::Stale
            } else {
                Liveness::Live
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn digest_identity(label: &[u8], value: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"mio-unresolved-effect-owner-v1\0");
    hasher.update(label);
    hasher.update(b"\0");
    hasher.update(value);
    format!("{:x}", hasher.finalize())
}

#[cfg(target_os = "linux")]
fn machine_id_digest() -> Option<String> {
    const SOURCES: [&str; 2] = ["/etc/machine-id", "/var/lib/dbus/machine-id"];
    for source in SOURCES {
        let Some(bytes) = read_bounded_path(Path::new(source), MAX_IDENTITY_SOURCE_BYTES) else {
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
    let bytes = read_bounded_path(
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
fn read_bounded_path(path: &Path, max: usize) -> Option<Vec<u8>> {
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
    let Some(bytes) = read_bounded_path(Path::new("/proc/self/mountinfo"), MAX_MOUNTINFO_BYTES)
    else {
        // 无法确认 /proc 可见性策略时，保守视为「可能隐藏」。
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
            // 只有 `hidepid=0` 表示关闭隐藏；其余取值（1、2、空值、…）都算有隐藏策略。
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
