//! 任务权威状态机（DES-04 的生产接线）。
//!
//! 本模块是「一个真实任务」在单次运行内的权威阶段定义与跟踪器。`lib.rs` 通过
//! `pub mod task_lifecycle;` 注册它；引擎在真实调度许可处用真实 permit 完成
//! `Queued -> Running`，并通过 task-local [`TaskHandle`] 让 workspace / shell 把
//! 真实 worker（阻塞盘读写、子进程回收）计入同一任务。CLI/RPC 的 dedup 账本与
//! journal、子代理快照只是它的投影。
//!
//! 子代理为每个 child 使用自己的 tracker（子运行时在独立 tokio task 中，不继承父
//! 任务 scope），child 的 `Queued -> Running` 由真实子代理 slot permit 准入；父任务
//! 与 child 的规范身份严格分离。跨重启的持久权威仍由 journal 的 legacy 字段承载。
//! 本模块不新建持久账本；[`ActiveLease`] 的审计编号只是运行内计数，不是磁盘权威。
//!
//! 下面的阶段图描述运行内跟踪器：
//!
//! ```text
//! Queued -> Running -> Completed | Failed | Cancelled | TimedOut
//!        \-> Failed        (准入前准备失败，绝不伪造 Running)
//!        \-> Cancelled     (排队取消，零 admit)
//! ```
//!
//! 权威阶段只有 [`TaskPhase`] 六个取值。`Interrupted` 与 `liveness` 是**独立观测**，
//! 绝不新增终态：`Interrupted` 只表示「owner 在任务结束前退出」，它不改变任务阶段；
//! `TaskLiveness`（来自 [`crate::dedup`]）只描述「记录里的 owner 是否仍是同一进程」，
//! `Unknown` 不等于死亡，也不会把任何阶段改写为失败或取消。
//!
//! # 对原规格的两处校正
//!
//! DES-04 原文只写了 `Queued -> Running`。这在实现上过严：一个已登记但尚未取得调度
//! 租约的任务，可能因为参数校验、依赖准备或唤醒失败而永远无法运行。若只允许
//! `Queued -> Running`，调用方要么伪造一次 `Running`（谎称任务真的开始过），要么把记录
//! 永久卡在排队。本模块因此加入 `Queued -> Failed`（[`TaskLifecycleTracker::fail_before_admit`]）
//! 作为真实、可审计的准备失败终态，同时保持「未取租约不得进入 Running」这条不变量。
//!
//! 第二处校正：`Queued -> TimedOut` 被有意禁止。超时语义要求「同时销毁进程组」
//! （[`Settlement::TimedOut`] 必须携带已回收证据），排队中的任务没有进程组可销毁；
//! 排队等待的截止时间应记为一次准备失败（`Failed`），而不是 `TimedOut`。
//!
//! # 与其他子系统的关系
//!
//! 本模块**不**新建磁盘账本，也不复制既有账本。它只提供：
//!
//! - 权威枚举与转移规则（全入口共用）；
//! - 运行内跟踪器（唯一运行内权威）；
//! - 供 `dedup` / `journal` / `subagent` 作为投影接入的显式映射 API
//!   （[`TaskPhase::from_subagent_status`]、[`TaskPhase::from_task_key_state`]、
//!   [`TaskPhase::from_run_state`] 及其反向映射）。legacy 原字段保持不变，通过映射读写，
//!   迁移期两个方向都返回 `Option`，无法表达时返回 `None` 而不是伪造一个相近状态。
//! - 一个宿主持久化 sink（trait 或闭包），由 root 未来的 `Host` 回调实现；模块自身不做 I/O。
//!
//! # 取消请求与 settled 分离
//!
//! [`TaskLifecycleTracker::request_cancel`] 只表达「请求取消」。排队任务立即进入
//! `Cancelled`（没有 worker 需要回收，也没有模型调用被准入）；运行中任务只记录请求，
//! 终态由 [`TaskLifecycleTracker::finish`] 单独提交。运行中的阻塞 worker 未回收前不允许
//! 提前终态：worker 持有计数 guard（[`TaskWorkerGuard`]），`finish` 请求被延迟到最后一个
//! guard 释放时提交。**任何单个 `WorkspaceLease` 的 drop 都不是终态触发器**——本模块根本
//! 不接受租约对象来结束回合，只接受显式 `finish` 与显式 worker guard。shell、多个工具调用
//! 与子任务使用同一套 guard/计数管理。

use crate::coordinator::{HeavyPermit, ModelPermit, TaskPermit};
use crate::dedup::{TaskKeyState, TaskLiveness};
use crate::journal::RunState;
use crate::subagent::Status as SubagentStatus;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::OwnedSemaphorePermit;

/// 单个跟踪器保留的任务记录上限。
///
/// 与 tool lifecycle 的 4096 上界一致；跟踪器满员即 fail-closed 拒绝新任务，绝不驱逐
/// 已跟踪任务去腾位置。
pub const MAX_TASKS: usize = 4096;

/// 单个身份字段的最大字节数（与 dedup key 上界一致）。
pub const MAX_IDENTITY_CHARS: usize = 512;
/// 单条任务 detail 的最大字符数。
pub const MAX_DETAIL_CHARS: usize = 512;
/// 准入证据 holder 标签的最大字符数。
pub const MAX_HOLDER_CHARS: usize = 128;

// ---------------------------------------------------------------------------
// 权威阶段
// ---------------------------------------------------------------------------

/// 任务的权威阶段。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskPhase {
    /// 已登记但尚未取得调度租约；不占用模型、heavy 或工作区许可。
    Queued,
    /// 已真实取得调度租约并开始执行。
    Running,
    /// 正常结束。
    Completed,
    /// 以错误结束，或准入前准备失败。
    Failed,
    /// 被取消。排队取消立即生效；运行中取消在 worker 回收后 settled。
    Cancelled,
    /// 超时结束；必须同时确认进程组已回收。
    TimedOut,
}

impl TaskPhase {
    /// 是否为终态。终态幂等且不可回退到非终态，也不可换成另一个终态。
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::TimedOut
        )
    }

    /// 是否仍占用运行资源（排队或运行中）。
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }

    /// 是否允许 `from -> to`。相同阶段永远幂等；终态只接受自身。
    ///
    /// `Queued -> Failed` 是准入前准备失败的合法终态；`Queued -> TimedOut` 被有意
    /// 禁止（排队任务没有进程组可销毁，截止时间应记为 `Failed`）。
    pub const fn transition_allowed(from: Self, to: Self) -> bool {
        if from as u8 == to as u8 {
            return true;
        }
        matches!(
            (from, to),
            (Self::Queued, Self::Running | Self::Failed | Self::Cancelled)
                | (
                    Self::Running,
                    Self::Completed | Self::Failed | Self::Cancelled | Self::TimedOut
                )
        )
    }

    /// 稳定的 snake_case 名称（与序列化一致）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
        }
    }
}

impl std::fmt::Display for TaskPhase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// 身份：任务 / engine run / provider tool call 严格区分
// ---------------------------------------------------------------------------

macro_rules! identity_type {
    ($name:ident, $label:literal) => {
        #[doc = concat!($label, "的规范化身份。")]
        ///
        /// 只有真实值才能构造：空串、纯空白、控制字符或超过 [`MAX_IDENTITY_CHARS`]
        /// 字节都会在构造与反序列化时被拒绝，绝不会用占位符伪造一个不存在的身份。
        /// 与 dedup 的记录 ID 一样，身份原样保存、不做前缀截断，避免共享前缀的
        /// 两个 ID 被当成同一个。
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// 用真实身份值构造。
            pub fn new(value: impl Into<String>) -> Result<Self, TaskLifecycleError> {
                let value = value.into();
                validate_identity(&value).map_err(TaskLifecycleError::InvalidIdentity)?;
                Ok(Self(value))
            }

            /// 原样返回身份字符串。
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serde::Serializer::serialize_str(serializer, &self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

identity_type!(TaskId, "规范任务身份（dedup `task_id`）");
identity_type!(EngineRunId, "真实 engine run 身份（run UUID）");
identity_type!(
    ProviderToolCallId,
    "创建该子任务的**来源** provider tool-call id（父任务中发起这次派发的调用，而非任务内每个工具调用的当前 ID）"
);

/// 一个任务在登记时携带的真实身份。
///
/// 三个层级严格区分：`task` 是去重账本的规范任务身份，`engine_run` 是执行该任务的
/// engine run UUID，`provider_tool_call` 是**创建该子任务的来源调用** id（父任务
/// 发起这次派发的那次 provider tool call），**不是**该任务内每个工具调用各自的当前
/// ID——一个子任务内部可能有多次工具调用，它们各有自己的 id，不能写进这个字段。
/// 缺省字段保持 `None`，绝不从参数、目标或完成顺序推断一个值来填充。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskIdentity {
    /// 规范任务身份。
    pub task: TaskId,
    /// 真实 engine run UUID；未知时 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_run: Option<EngineRunId>,
    /// 创建该子任务的**来源** provider tool-call id（父任务中的派发调用），不是
    /// 子任务内每个工具调用的当前 ID；未知时 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_tool_call: Option<ProviderToolCallId>,
}

impl TaskIdentity {
    /// 只带规范任务身份，其余保持未知。
    pub fn new(task: TaskId) -> Self {
        Self {
            task,
            engine_run: None,
            provider_tool_call: None,
        }
    }

    /// 追加真实 engine run 身份。
    pub fn with_engine_run(mut self, run: EngineRunId) -> Self {
        self.engine_run = Some(run);
        self
    }

    /// 追加**创建该子任务的来源** provider tool-call 身份。
    pub fn with_provider_tool_call(mut self, call: ProviderToolCallId) -> Self {
        self.provider_tool_call = Some(call);
        self
    }
}

fn validate_identity(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err("identity must not be empty or whitespace-only".into());
    }
    if value.len() > MAX_IDENTITY_CHARS {
        return Err(format!(
            "identity must be at most {MAX_IDENTITY_CHARS} bytes"
        ));
    }
    if value.chars().any(char::is_control) {
        return Err("identity must not contain control characters".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 调度租约证据
// ---------------------------------------------------------------------------

/// 调度租约的类别。
///
/// `Shared` / `Exclusive` 描述的是**工作区路径租约**：只读可并行、写独占。
/// 模型请求、重任务与宿主信号量名额是单占用的**非路径**许可，统一记为
/// [`LeaseKind::Slot`]，绝不误读成工作区路径租约。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseKind {
    /// 只读路径租约，多个任务可并行持有。
    Shared,
    /// 写独占路径租约，同一工作区同一时刻只有一个。
    Exclusive,
    /// 非路径的单占用槽位（模型 / 重任务 / 宿主信号量名额）。
    ///
    /// 它是真实 owned 许可，但不承诺任何路径级共享/独占语义，宿主不得据此推断
    /// 工作区并发。
    Slot,
}

/// 调度方对「仍持有一次已取得的许可」的可信声明。
///
/// 引擎在 `Queued -> Running` 的准入点调用 [`SchedulingPermit::is_held`] 复核，并把
/// `lease_id` / `kind` / `holder` 记入审计证据。
///
/// # 边界：trait 本身**不能**防伪
///
/// 这个 trait 不能证明许可真的被持有：外部 `impl` 完全可以谎称 `is_held()` 为真而
/// 不持有任何真实资源，引擎也无法对任意第三方实现做资源级验证。因此「真实持有」
/// 只能由**具体实现**保证——本模块的 [`ActiveLease`] 真正 owner 住
/// [`crate::coordinator`] 的 permit 或 `tokio::sync::OwnedSemaphorePermit`，其
/// `is_held` 直接来自真实 owner 是否仍在。是否防伪由宿主调度方负责；引擎把传入的
/// 许可当作可信 Host 契约。
pub trait SchedulingPermit: Send + Sync {
    /// 审计关联编号：把同一次准入与释放串到同一条证据上。
    ///
    /// 它**不是**许可本身，不能据此重建或取得任何许可；真实 permit 可能根本没有
    /// 数值 id（如 [`crate::coordinator`] 的 permit）。纯值编号不构成准入证据。
    fn lease_id(&self) -> u64;
    /// 许可类别。
    fn kind(&self) -> LeaseKind;
    /// 许可持有者标签（有界），仅用于审计。
    fn holder(&self) -> &str;
    /// 该许可当前是否**真实仍被持有**。释放后必须返回 `false`。
    fn is_held(&self) -> bool;
}

/// [`ActiveLease`] 真正 owner 住的许可资源；`Some` 即持有，drop 即释放。
///
/// 字段本身只用于 RAII 持有与释放，不参与读取判定（[`SchedulingPermit::is_held`]
/// 看 owner 是否存在），因此对每个 variant 的“从未读取”死代码告警是设计使然，
/// 而不是遗漏的使用点。
#[allow(dead_code)]
enum LeaseOwner {
    /// [`crate::coordinator`] 的任务（工作区范围）许可。
    Task(TaskPermit),
    /// 模型请求许可。
    Model(ModelPermit),
    /// 重任务许可。
    Heavy(HeavyPermit),
    /// 任意宿主并发槽信号量名额。
    Semaphore(OwnedSemaphorePermit),
}

/// 一个**真实持有**调度许可的 RAII 载体。
///
/// 只能用真实许可对象构造（[`Self::from_task_permit`]、[`Self::from_model_permit`]、
/// [`Self::from_heavy_permit`]、[`Self::from_semaphore_permit`]）；没有接受纯
/// `lease_id` / `bool` 的构造，因此它不会在生产路径上伪造一个不存在于任何调度器的许可。
///
/// 真实许可 owner 在内部 `Mutex<Option<_>>` 中：
///
/// - [`SchedulingPermit::is_held`] 直接来自「真实 owner 是否仍在」，不是独立布尔标志；
/// - [`Self::release`] 会 **drop 真实许可**，立刻归还 coordinator 槽位 / 信号量名额；
/// - [`Drop`] 同样释放，不会因忘记显式 `release` 而泄漏名额。
///
/// `lease_id` / `kind` / `holder` 只是从真实许可推导出的审计元数据，不参与持有判定，
/// 也不能替代许可本身。它不假装自己是工作区路径租约或安全边界。
pub struct ActiveLease {
    lease_id: u64,
    kind: LeaseKind,
    holder: String,
    owner: Mutex<Option<LeaseOwner>>,
}

/// 进程内单调递增的审计关联编号。
///
/// 只是把一次准入与一次释放关联起来的标签，不是持久账本，也不代表任何真实许可身份。
static NEXT_LEASE_AUDIT_ID: AtomicU64 = AtomicU64::new(1);

fn next_lease_audit_id() -> u64 {
    NEXT_LEASE_AUDIT_ID.fetch_add(1, Ordering::Relaxed)
}

impl ActiveLease {
    /// 包装一个真实 [`TaskPermit`]：写范围按 [`LeaseKind::Exclusive`]、只读按
    /// [`LeaseKind::Shared`] 记录。
    pub fn from_task_permit(permit: TaskPermit) -> Arc<Self> {
        let kind = if permit.is_write() {
            LeaseKind::Exclusive
        } else {
            LeaseKind::Shared
        };
        let role = if kind == LeaseKind::Exclusive {
            "write"
        } else {
            "read"
        };
        let holder = bounded(
            &format!("coordinator-task:{}({role})", permit.path()),
            MAX_HOLDER_CHARS,
        );
        Self::seal(kind, holder, LeaseOwner::Task(permit))
    }

    /// 包装一个真实 [`ModelPermit`]。
    ///
    /// 模型槽位是单占用的 provider 并发名额（不是工作区路径租约），按
    /// [`LeaseKind::Slot`] 记录。释放会真实归还全局 / provider 模型槽位。
    pub fn from_model_permit(permit: ModelPermit) -> Arc<Self> {
        let holder = bounded(
            &format!("coordinator-model:{}", permit.provider()),
            MAX_HOLDER_CHARS,
        );
        Self::seal(LeaseKind::Slot, holder, LeaseOwner::Model(permit))
    }

    /// 包装一个真实 [`HeavyPermit`]（重任务槽位单占用）。
    pub fn from_heavy_permit(permit: HeavyPermit) -> Arc<Self> {
        Self::seal(
            LeaseKind::Slot,
            "coordinator-heavy".to_string(),
            LeaseOwner::Heavy(permit),
        )
    }

    /// 包装一个真实 `tokio::sync::OwnedSemaphorePermit`（宿主并发槽单占用）。
    pub fn from_semaphore_permit(permit: OwnedSemaphorePermit) -> Arc<Self> {
        Self::seal(
            LeaseKind::Slot,
            "semaphore".to_string(),
            LeaseOwner::Semaphore(permit),
        )
    }

    fn seal(kind: LeaseKind, holder: String, owner: LeaseOwner) -> Arc<Self> {
        Arc::new(Self {
            lease_id: next_lease_audit_id(),
            kind,
            holder,
            owner: Mutex::new(Some(owner)),
        })
    }

    /// 显式释放：**drop 真实许可**并归还底层槽位（幂等）。释放后不再构成准入证据。
    pub fn release(&self) {
        let owner = self
            .owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        // `owner` 在此真实析构：coordinator 的 permit / 信号量名额立即归还。
        drop(owner);
    }

    /// 以真实 usage 结算一个持有的 [`ModelPermit`]（若有）。
    ///
    /// 这是包装真实模型许可后仍能按 provider 报告 usage 结算的唯一入口：
    /// 它把真实 permit 交给 [`ModelPermit::complete`]，不会丢失结算。
    /// owner 不是模型许可（或已释放）时是 no-op，绝不重建或伪造许可。
    pub fn complete_model_usage(&self, usage: Option<u64>) {
        let mut guard = self
            .owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if matches!(guard.as_ref(), Some(LeaseOwner::Model(_)))
            && let Some(LeaseOwner::Model(permit)) = guard.take()
        {
            permit.complete(usage);
        }
    }
}

impl Drop for ActiveLease {
    fn drop(&mut self) {
        self.release();
    }
}

impl SchedulingPermit for ActiveLease {
    fn lease_id(&self) -> u64 {
        self.lease_id
    }
    fn kind(&self) -> LeaseKind {
        self.kind
    }
    fn holder(&self) -> &str {
        &self.holder
    }
    fn is_held(&self) -> bool {
        // 真实性来自「真实 owner 是否仍在」，而不是独立的布尔标志。
        self.owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }
}

/// 记录在任务上的准入证据：一次真实 `Queued -> Running` 的审计依据。
///
/// `lease_id` 只是把准入与释放关联起来的审计编号，不代表许可本身；真实许可由提交
/// 准入时传入的 [`SchedulingPermit`] owner 在外部生命周期中持有。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionEvidence {
    /// 审计关联编号（来自传入许可的 [`SchedulingPermit::lease_id`]，不代表许可本身）。
    pub lease_id: u64,
    /// 许可类别。
    pub kind: LeaseKind,
    /// 许可持有者标签（有界）。
    pub holder: String,
    /// 准入时刻的 Unix 秒。
    pub admitted_at: u64,
}

// ---------------------------------------------------------------------------
// 超时证据
// ---------------------------------------------------------------------------

/// 「超时任务的进程组已被回收」的正向证据。
///
/// `Running -> TimedOut` 必须同时销毁进程组。只有真正完成回收的 supervisor 才能让
/// [`ProcessGroupReap::is_reaped`] 返回真；纯时间戳或布尔标志不构成证据。
pub trait ProcessGroupReap: Send + Sync {
    /// 进程组是否已经被回收。
    fn is_reaped(&self) -> bool;
}

/// 一次终态提交请求。
#[derive(Clone, Copy)]
pub enum Settlement<'a> {
    /// 正常结束。
    Completed,
    /// 以错误结束；准入前的准备失败也走这条路径。
    Failed,
    /// 取消结束。
    Cancelled,
    /// 超时结束，必须携带已回收证据。
    TimedOut {
        /// 进程组回收证据。
        reap: &'a dyn ProcessGroupReap,
    },
}

impl Settlement<'_> {
    /// 该请求对应的终态阶段。
    pub const fn phase(self) -> TaskPhase {
        match self {
            Self::Completed => TaskPhase::Completed,
            Self::Failed => TaskPhase::Failed,
            Self::Cancelled => TaskPhase::Cancelled,
            Self::TimedOut { .. } => TaskPhase::TimedOut,
        }
    }
}

// ---------------------------------------------------------------------------
// 记录与观测
// ---------------------------------------------------------------------------

/// 一条规范任务记录。只保存真实身份与有界元数据。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRecord {
    /// 运行内单调登记序号。
    pub seq: u64,
    /// 规范任务身份。
    pub task_id: TaskId,
    /// 权威阶段。
    pub phase: TaskPhase,
    /// 真实 engine run UUID；未知时 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_run_id: Option<EngineRunId>,
    /// 创建该子任务的**来源** provider tool-call id（父任务中的派发调用），不是
    /// 该任务内每个工具调用的当前 ID；未知时 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_tool_call_id: Option<ProviderToolCallId>,
    /// 真实准入证据；未取租约时 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission: Option<AdmissionEvidence>,
    /// 是否收到过取消请求。
    #[serde(default)]
    pub cancel_requested: bool,
    /// 已被请求但尚未提交的终态（有 worker 未回收时会延迟提交）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_terminal: Option<TaskPhase>,
    /// 当前未回收的 worker guard 数。运行内观测，不落盘（崩溃后不存在存活 worker）。
    #[serde(default)]
    pub workers: usize,
    /// 有界的人类可读 detail。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// 首次登记的 Unix 秒。
    pub registered_at: u64,
    /// 最近一次更新的 Unix 秒。
    pub updated: u64,
}

impl TaskRecord {
    /// 是否真的取得过调度租约（即出现过一次合法 `Queued -> Running`）。
    pub fn was_admitted(&self) -> bool {
        self.admission.is_some()
    }

    /// 投影回 `subagent::Status`（六态一一对应）。
    pub fn legacy_subagent_status(&self) -> SubagentStatus {
        self.phase.into()
    }

    /// 投影回 dedup `TaskKeyState`；`TimedOut` 在旧账本中没有对应值，返回 `None`。
    pub fn legacy_task_key_state(&self) -> Option<TaskKeyState> {
        self.phase.to_task_key_state()
    }

    /// 投影回 journal `RunState`；`Queued` / `TimedOut` 在旧 schema 中没有对应值。
    pub fn legacy_run_state(&self) -> Option<RunState> {
        self.phase.to_run_state()
    }
}

/// 把权威记录与**独立观测**组合起来的视图。
///
/// `interrupted` 与 `liveness` 永远不参与阶段判定：`Interrupted` 不是终态，
/// `liveness = unknown` 也不等于死亡。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskView {
    /// 权威记录。
    pub record: TaskRecord,
    /// owner 存活性观测（来自 dedup 账本或等价观测）。
    pub liveness: TaskLiveness,
    /// 恢复期观测：owner 在任务结束前退出。独立于阶段。
    pub interrupted: bool,
}

impl TaskView {
    /// 组合权威记录与独立观测。
    pub fn new(record: TaskRecord, liveness: TaskLiveness, interrupted: bool) -> Self {
        Self {
            record,
            liveness,
            interrupted,
        }
    }

    /// 权威阶段。
    pub fn phase(&self) -> TaskPhase {
        self.record.phase
    }
}

// ---------------------------------------------------------------------------
// 持久化 sink
// ---------------------------------------------------------------------------

/// 任务状态变化的宿主持久化关口。
///
/// 引擎自身不做 I/O；root 未来的 `Host::on_task_lifecycle` 实现它，测试可直接传闭包。
/// 返回 `Err` 表示状态无法持久化：跟踪器随后 latch fail-closed，拒绝继续提交，
/// 绝不假装状态已成功落盘。
pub trait TaskLifecycleSink: Send + Sync {
    /// 提交一条已经过合法转移校验的任务记录。
    fn on_task_lifecycle(&self, record: &TaskRecord) -> Result<(), String>;
}

impl<F> TaskLifecycleSink for F
where
    F: Fn(&TaskRecord) -> Result<(), String> + Send + Sync,
{
    fn on_task_lifecycle(&self, record: &TaskRecord) -> Result<(), String> {
        self(record)
    }
}

/// 不做任何持久化的 sink（用于无宿主或纯内存场景）。它不是「已落盘」的证明。
#[derive(Clone, Copy, Debug, Default)]
pub struct NullTaskSink;

impl TaskLifecycleSink for NullTaskSink {
    fn on_task_lifecycle(&self, _record: &TaskRecord) -> Result<(), String> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// 状态机、身份或持久化失败。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskLifecycleError {
    /// 跟踪器满员，拒绝新任务而不驱逐已跟踪任务。
    Full,
    /// 一次持久化失败后跟踪器已 fail-closed。
    FailClosed(String),
    /// 找不到匹配的任务。
    NotFound(TaskId),
    /// 任务已存在，或已记录另一个真实身份/许可，不能静默覆盖。
    Duplicate(TaskId),
    /// 身份值非法（空、纯空白、含控制字符或超长）。
    InvalidIdentity(String),
    /// 非法转移。
    Illegal {
        /// 原阶段（或已请求的终态）。
        from: TaskPhase,
        /// 目标阶段。
        to: TaskPhase,
    },
    /// 调度许可已被释放，不再构成准入证据。
    LeaseNotHeld {
        /// 许可编号。
        lease_id: u64,
    },
    /// 超时任务尚未确认进程组回收。
    ProcessGroupAlive(TaskId),
    /// 已请求取消，拒绝非取消终态或新 worker。
    CancelRequested {
        /// 被拒绝的目标阶段。
        attempted: TaskPhase,
    },
    /// 宿主持久化失败。
    Persist(String),
}

impl std::fmt::Display for TaskLifecycleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => write!(
                formatter,
                "the task lifecycle tracker is full ({MAX_TASKS} tasks); refusing a new task"
            ),
            Self::FailClosed(reason) => {
                write!(
                    formatter,
                    "the task lifecycle tracker is fail-closed: {reason}"
                )
            }
            Self::NotFound(task_id) => write!(formatter, "task '{task_id}' is not tracked"),
            Self::Duplicate(task_id) => write!(
                formatter,
                "task '{task_id}' already has a different real identity or lease"
            ),
            Self::InvalidIdentity(reason) => write!(formatter, "invalid identity: {reason}"),
            Self::Illegal { from, to } => {
                write!(formatter, "illegal task transition {from} -> {to}")
            }
            Self::LeaseNotHeld { lease_id } => write!(
                formatter,
                "scheduling lease {lease_id} is no longer held; it cannot admit a task"
            ),
            Self::ProcessGroupAlive(task_id) => write!(
                formatter,
                "task '{task_id}' cannot be TimedOut before its process group is reaped"
            ),
            Self::CancelRequested { attempted } => write!(
                formatter,
                "task cancellation was requested; refusing transition to {attempted}"
            ),
            Self::Persist(error) => {
                write!(formatter, "the task state could not be persisted: {error}")
            }
        }
    }
}

impl std::error::Error for TaskLifecycleError {}

// ---------------------------------------------------------------------------
// 跟踪器
// ---------------------------------------------------------------------------

/// 终态提交结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinishOutcome {
    /// 终态已提交。
    Committed(TaskPhase),
    /// 仍有 worker 未回收：终态已记录但延迟提交，最后一个 guard 释放时落地。
    Deferred {
        /// 已请求的终态。
        phase: TaskPhase,
        /// 尚未回收的 worker 数。
        workers: usize,
    },
}

/// 取消请求结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelOutcome {
    /// 排队任务立即取消（零 admit）。
    Cancelled,
    /// 运行中任务：已记录取消请求，终态由 `finish(Cancelled)` 单独提交。
    Requested {
        /// 尚未回收的 worker 数。
        workers: usize,
    },
    /// 已是终态，取消是空操作。
    AlreadySettled(TaskPhase),
}

struct TaskEntry {
    record: TaskRecord,
    /// 该任务的提交锁：同一任务串行，不同任务互不阻塞。sink 回调在持有该锁、
    /// 但不持有 `TrackerState` 时执行，因此回调里查询 snapshot 不会死锁。
    lock: Arc<Mutex<()>>,
}

struct TrackerState {
    tasks: BTreeMap<TaskId, TaskEntry>,
    /// 已预留但尚未提交的任务身份：并发登记同一 `task_id` 只能有一个成功。
    reserved: BTreeSet<TaskId>,
    next_seq: u64,
    /// 已预留序号/容量槽但尚未提交的登记数。
    pending: usize,
}

/// 一次运行的权威任务跟踪器。
///
/// 克隆共享同一份状态；它只是同一运行内的转移守卫，durable 记录由 sink 写入既有
/// journal / 账本，不是第二套磁盘权威。
pub struct TaskLifecycleTracker {
    scope: String,
    sink: Arc<dyn TaskLifecycleSink>,
    capacity: usize,
    state: Mutex<TrackerState>,
    fail_closed: Mutex<Option<String>>,
    latched: AtomicBool,
    /// 终态提交（含延迟提交落地）时唤醒 `wait_settled` 等待者。运行内观测，不落盘。
    settled: tokio::sync::Notify,
}

impl TaskLifecycleTracker {
    /// 为一次运行创建跟踪器。
    pub fn new(scope: impl Into<String>, sink: Arc<dyn TaskLifecycleSink>) -> Arc<Self> {
        Self::with_capacity(scope, sink, MAX_TASKS)
    }

    /// 指定容量的跟踪器（测试与更小的宿主）。
    pub fn with_capacity(
        scope: impl Into<String>,
        sink: Arc<dyn TaskLifecycleSink>,
        capacity: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            scope: scope.into(),
            sink,
            capacity,
            state: Mutex::new(TrackerState {
                tasks: BTreeMap::new(),
                reserved: BTreeSet::new(),
                next_seq: 0,
                pending: 0,
            }),
            fail_closed: Mutex::new(None),
            latched: AtomicBool::new(false),
            settled: tokio::sync::Notify::new(),
        })
    }

    /// 不落盘的跟踪器（无宿主或纯内存场景）。
    pub fn detached(scope: impl Into<String>) -> Arc<Self> {
        Self::new(scope, Arc::new(NullTaskSink))
    }

    /// 本跟踪器的运行 scope。
    pub fn scope(&self) -> &str {
        &self.scope
    }

    /// 当前是否已经 fail-closed。
    pub fn is_fail_closed(&self) -> bool {
        self.latched.load(Ordering::SeqCst)
    }

    /// 已跟踪任务数。
    pub fn len(&self) -> usize {
        self.lock().tasks.len()
    }

    /// 是否没有任何已跟踪任务。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 按登记序号升序返回已跟踪任务。
    pub fn records(&self) -> Vec<TaskRecord> {
        let state = self.lock();
        let mut records: Vec<TaskRecord> = state
            .tasks
            .values()
            .map(|entry| entry.record.clone())
            .collect();
        records.sort_by_key(|record| record.seq);
        records
    }

    /// 查询单个任务的当前记录。
    pub fn snapshot(&self, task_id: &TaskId) -> Option<TaskRecord> {
        self.lock()
            .tasks
            .get(task_id)
            .map(|entry| entry.record.clone())
    }

    /// 组合权威记录与独立观测（`liveness` / `interrupted` 不改变阶段）。
    pub fn view(
        &self,
        task_id: &TaskId,
        liveness: TaskLiveness,
        interrupted: bool,
    ) -> Option<TaskView> {
        self.snapshot(task_id)
            .map(|record| TaskView::new(record, liveness, interrupted))
    }

    /// 登记一个排队任务（`Queued`）。
    pub fn register(
        &self,
        identity: TaskIdentity,
        detail: Option<&str>,
    ) -> Result<TaskRecord, TaskLifecycleError> {
        if let Some(reason) = self.fail_closed_reason() {
            return Err(TaskLifecycleError::FailClosed(reason));
        }
        let lock = Arc::new(Mutex::new(()));
        // 预留身份、序号与容量槽：并发登记不会一起越过上界，也不会拿到同一个 seq，
        // 同一 task_id 的并发登记只有一个能成功。
        let seq = {
            let mut state = self.lock();
            if state.tasks.contains_key(&identity.task) || state.reserved.contains(&identity.task) {
                return Err(TaskLifecycleError::Duplicate(identity.task.clone()));
            }
            if state.tasks.len().saturating_add(state.pending) >= self.capacity {
                return Err(TaskLifecycleError::Full);
            }
            let seq = state.next_seq + 1;
            state.next_seq = seq;
            state.pending += 1;
            state.reserved.insert(identity.task.clone());
            seq
        };
        let now = now_secs();
        let record = TaskRecord {
            seq,
            task_id: identity.task.clone(),
            phase: TaskPhase::Queued,
            engine_run_id: identity.engine_run,
            provider_tool_call_id: identity.provider_tool_call,
            admission: None,
            cancel_requested: false,
            pending_terminal: None,
            workers: 0,
            detail: detail.map(|value| bounded(value, MAX_DETAIL_CHARS)),
            registered_at: now,
            updated: now,
        };
        if let Err(error) = self.persist(&record) {
            let mut state = self.lock();
            state.pending = state.pending.saturating_sub(1);
            state.reserved.remove(&identity.task);
            return Err(error);
        }
        let mut state = self.lock();
        state.pending = state.pending.saturating_sub(1);
        state.reserved.remove(&identity.task);
        state.tasks.insert(
            identity.task,
            TaskEntry {
                record: record.clone(),
                lock,
            },
        );
        Ok(record)
    }

    /// 记录一次真实 `Queued -> Running` 准入。
    ///
    /// 只接受一个声明仍被真实持有的 [`SchedulingPermit`]。对 [`ActiveLease`] 这类
    /// 实现，`is_held` 来自真实 owner；但外部 `impl` 是宿主可信契约，引擎无法验证其
    /// 是否真的持有资源（见 [`SchedulingPermit`] 的边界说明）。许可已释放（`is_held()`
    /// 为假）返回 [`TaskLifecycleError::LeaseNotHeld`]；纯布尔、时间戳或 lease 编号都
    /// 不能构成本接口的入参。
    pub fn admit(
        &self,
        task_id: &TaskId,
        permit: &dyn SchedulingPermit,
    ) -> Result<TaskRecord, TaskLifecycleError> {
        if !permit.is_held() {
            return Err(TaskLifecycleError::LeaseNotHeld {
                lease_id: permit.lease_id(),
            });
        }
        if let Some(reason) = self.fail_closed_reason() {
            return Err(TaskLifecycleError::FailClosed(reason));
        }
        let evidence = AdmissionEvidence {
            lease_id: permit.lease_id(),
            kind: permit.kind(),
            holder: bounded(permit.holder(), MAX_HOLDER_CHARS),
            admitted_at: now_secs(),
        };
        let lock = self.task_lock(task_id);
        let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let candidate = {
            let state = self.lock();
            let entry = state
                .tasks
                .get(task_id)
                .ok_or_else(|| TaskLifecycleError::NotFound(task_id.clone()))?;
            let record = &entry.record;
            if let Some(existing) = &record.admission {
                if record.phase == TaskPhase::Running && existing.lease_id == evidence.lease_id {
                    return Ok(record.clone());
                }
                return Err(TaskLifecycleError::Duplicate(task_id.clone()));
            }
            if !TaskPhase::transition_allowed(record.phase, TaskPhase::Running) {
                return Err(TaskLifecycleError::Illegal {
                    from: record.phase,
                    to: TaskPhase::Running,
                });
            }
            let mut candidate = record.clone();
            candidate.phase = TaskPhase::Running;
            candidate.admission = Some(evidence);
            candidate.updated = now_secs();
            candidate
        };
        self.persist_then_store(task_id, candidate.clone())?;
        Ok(candidate)
    }

    /// 记录准入前的准备失败（`Queued -> Failed`），不伪造 `Running`。
    pub fn fail_before_admit(
        &self,
        task_id: &TaskId,
        reason: &str,
    ) -> Result<TaskRecord, TaskLifecycleError> {
        match self.finish_phase(task_id, TaskPhase::Failed, Some(reason))? {
            FinishOutcome::Committed(_) => self
                .snapshot(task_id)
                .ok_or_else(|| TaskLifecycleError::NotFound(task_id.clone())),
            FinishOutcome::Deferred { .. } => Err(TaskLifecycleError::Illegal {
                from: TaskPhase::Queued,
                to: TaskPhase::Failed,
            }),
        }
    }

    /// 请求取消。
    ///
    /// - `Queued`：立即进入 `Cancelled`（零 admit，无可回收 worker）。
    /// - `Running`：只记录请求，**不提交终态**；终态由 `finish(Settlement::Cancelled)`
    ///   在 worker 回收后单独提交。
    /// - 终态：空操作。
    pub fn request_cancel(
        &self,
        task_id: &TaskId,
        reason: Option<&str>,
    ) -> Result<CancelOutcome, TaskLifecycleError> {
        if let Some(reason) = self.fail_closed_reason() {
            return Err(TaskLifecycleError::FailClosed(reason));
        }
        let lock = self.task_lock(task_id);
        let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let (candidate, outcome) = {
            let state = self.lock();
            let entry = state
                .tasks
                .get(task_id)
                .ok_or_else(|| TaskLifecycleError::NotFound(task_id.clone()))?;
            let record = &entry.record;
            if record.phase.is_terminal() {
                return Ok(CancelOutcome::AlreadySettled(record.phase));
            }
            let mut candidate = record.clone();
            candidate.cancel_requested = true;
            candidate.updated = now_secs();
            if let Some(reason) = reason {
                candidate.detail = Some(bounded(reason, MAX_DETAIL_CHARS));
            }
            if record.phase == TaskPhase::Queued {
                candidate.phase = TaskPhase::Cancelled;
                (candidate, CancelOutcome::Cancelled)
            } else {
                (
                    candidate,
                    CancelOutcome::Requested {
                        workers: record.workers,
                    },
                )
            }
        };
        self.persist_then_store(task_id, candidate)?;
        Ok(outcome)
    }

    /// 提交一个终态。
    ///
    /// 若仍有 worker guard 未回收，终态被记录为 `pending_terminal` 并在最后一个 guard
    /// 释放时提交；在此之前任务保持原阶段，绝不提前终态。取消请求会否决非取消终态；
    /// 取消可以取代一个尚未提交的非取消终态。已提交的终态永不回退或改换。
    pub fn finish(
        &self,
        task_id: &TaskId,
        settlement: Settlement<'_>,
        detail: Option<&str>,
    ) -> Result<FinishOutcome, TaskLifecycleError> {
        let phase = settlement.phase();
        if let Settlement::TimedOut { reap } = settlement
            && !reap.is_reaped()
        {
            return Err(TaskLifecycleError::ProcessGroupAlive(task_id.clone()));
        }
        self.finish_phase(task_id, phase, detail)
    }

    /// 开始一个运行中任务的 worker，返回回收前都不能终态的计数 guard。
    ///
    /// shell、批次中的每个工具调用、子任务都应用同一套 guard；任何单个租约的 drop
    /// 都不经过这里，因此不会误结束整个回合。取消请求之后不再允许新 worker；非
    /// `Running` 阶段（含已终态）一律报告 `Cancelled -> Running` 之类的 `Illegal`
    /// 非法转移，`CancelRequested` 只用于仍在 `Running` 的任务。
    pub fn begin_worker(
        self: &Arc<Self>,
        task_id: &TaskId,
    ) -> Result<TaskWorkerGuard, TaskLifecycleError> {
        if let Some(reason) = self.fail_closed_reason() {
            return Err(TaskLifecycleError::FailClosed(reason));
        }
        let lock = self.task_lock(task_id);
        let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        {
            let mut state = self.lock();
            let entry = state
                .tasks
                .get_mut(task_id)
                .ok_or_else(|| TaskLifecycleError::NotFound(task_id.clone()))?;
            let record = &mut entry.record;
            // 先校验阶段合法性，再判取消请求——与 `finish_phase` 的优先级一致：
            // 已终态（如排队取消后的 `Cancelled`）是非法转移 `Cancelled -> Running`，
            // 必须报告真实 from 阶段，而不是拿 `CancelRequested` 盖掉终态事实。
            // `CancelRequested` 只用于仍在 `Running`、正在排空 worker 的任务。
            if record.phase != TaskPhase::Running {
                return Err(TaskLifecycleError::Illegal {
                    from: record.phase,
                    to: TaskPhase::Running,
                });
            }
            if record.cancel_requested {
                return Err(TaskLifecycleError::CancelRequested {
                    attempted: TaskPhase::Running,
                });
            }
            record.workers = record.workers.saturating_add(1);
            record.updated = now_secs();
        }
        Ok(TaskWorkerGuard {
            tracker: Arc::clone(self),
            task_id: task_id.clone(),
            released: false,
        })
    }

    /// 记录真实 engine run 身份（set-once，未知时不写）。
    pub fn attach_engine_run(
        &self,
        task_id: &TaskId,
        run: EngineRunId,
    ) -> Result<TaskRecord, TaskLifecycleError> {
        self.attach_identity(task_id, IdentityField::EngineRun(run))
    }

    /// 记录**创建该子任务的来源** provider tool-call 身份（父任务中的派发调用，
    /// 不是子任务内每个工具调用的当前 ID；set-once，未知时不写）。
    pub fn attach_provider_tool_call(
        &self,
        task_id: &TaskId,
        call: ProviderToolCallId,
    ) -> Result<TaskRecord, TaskLifecycleError> {
        self.attach_identity(task_id, IdentityField::ProviderToolCall(call))
    }

    fn attach_identity(
        &self,
        task_id: &TaskId,
        field: IdentityField,
    ) -> Result<TaskRecord, TaskLifecycleError> {
        if let Some(reason) = self.fail_closed_reason() {
            return Err(TaskLifecycleError::FailClosed(reason));
        }
        let lock = self.task_lock(task_id);
        let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let candidate = {
            let state = self.lock();
            let entry = state
                .tasks
                .get(task_id)
                .ok_or_else(|| TaskLifecycleError::NotFound(task_id.clone()))?;
            let record = &entry.record;
            let mut candidate = record.clone();
            match &field {
                IdentityField::EngineRun(run) => {
                    if candidate.engine_run_id.as_ref() == Some(run) {
                        return Ok(candidate);
                    }
                    if candidate.engine_run_id.is_some() {
                        return Err(TaskLifecycleError::Duplicate(task_id.clone()));
                    }
                    candidate.engine_run_id = Some(run.clone());
                }
                IdentityField::ProviderToolCall(call) => {
                    if candidate.provider_tool_call_id.as_ref() == Some(call) {
                        return Ok(candidate);
                    }
                    if candidate.provider_tool_call_id.is_some() {
                        return Err(TaskLifecycleError::Duplicate(task_id.clone()));
                    }
                    candidate.provider_tool_call_id = Some(call.clone());
                }
            }
            candidate.updated = now_secs();
            candidate
        };
        self.persist_then_store(task_id, candidate.clone())?;
        Ok(candidate)
    }

    fn finish_phase(
        &self,
        task_id: &TaskId,
        phase: TaskPhase,
        detail: Option<&str>,
    ) -> Result<FinishOutcome, TaskLifecycleError> {
        debug_assert!(phase.is_terminal());
        if let Some(reason) = self.fail_closed_reason() {
            return Err(TaskLifecycleError::FailClosed(reason));
        }
        let lock = self.task_lock(task_id);
        let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let detail = detail.map(|value| bounded(value, MAX_DETAIL_CHARS));
        let (candidate, outcome) = {
            let state = self.lock();
            let entry = state
                .tasks
                .get(task_id)
                .ok_or_else(|| TaskLifecycleError::NotFound(task_id.clone()))?;
            let record = &entry.record;
            if record.phase.is_terminal() {
                return if record.phase == phase {
                    Ok(FinishOutcome::Committed(record.phase))
                } else {
                    Err(TaskLifecycleError::Illegal {
                        from: record.phase,
                        to: phase,
                    })
                };
            }
            if record.cancel_requested && phase != TaskPhase::Cancelled {
                return Err(TaskLifecycleError::CancelRequested { attempted: phase });
            }
            if let Some(pending) = record.pending_terminal {
                if pending != phase && phase != TaskPhase::Cancelled {
                    return Err(TaskLifecycleError::Illegal {
                        from: pending,
                        to: phase,
                    });
                }
            } else if !TaskPhase::transition_allowed(record.phase, phase) {
                return Err(TaskLifecycleError::Illegal {
                    from: record.phase,
                    to: phase,
                });
            }
            let mut candidate = record.clone();
            candidate.updated = now_secs();
            if detail.is_some() {
                candidate.detail = detail;
            }
            if record.workers == 0 {
                candidate.phase = phase;
                candidate.pending_terminal = None;
                (candidate, FinishOutcome::Committed(phase))
            } else {
                candidate.pending_terminal = Some(phase);
                (
                    candidate,
                    FinishOutcome::Deferred {
                        phase,
                        workers: record.workers,
                    },
                )
            }
        };
        self.persist_then_store(task_id, candidate)?;
        Ok(outcome)
    }

    /// 由 [`TaskWorkerGuard`] 调用：递减 worker 计数，并在归零时提交被延迟的终态。
    fn release_worker(&self, task_id: &TaskId) {
        let lock = self.task_lock(task_id);
        let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let candidate = {
            let state = self.lock();
            let Some(entry) = state.tasks.get(task_id) else {
                return;
            };
            let record = &entry.record;
            let workers = record.workers.saturating_sub(1);
            if workers > 0 {
                let mut candidate = record.clone();
                candidate.workers = workers;
                candidate.updated = now_secs();
                Some((candidate, false))
            } else if let Some(pending) = record.pending_terminal {
                let mut candidate = record.clone();
                candidate.workers = 0;
                candidate.phase = pending;
                candidate.pending_terminal = None;
                candidate.updated = now_secs();
                Some((candidate, true))
            } else {
                let mut candidate = record.clone();
                candidate.workers = 0;
                candidate.updated = now_secs();
                Some((candidate, false))
            }
        };
        let Some((candidate, commits_terminal)) = candidate else {
            return;
        };
        if commits_terminal {
            // 终态承诺必须落盘成功才提交；失败时 latch fail-closed 并保持非终态，
            // 绝不谎称已结束。
            if self.persist(&candidate).is_ok() {
                let mut state = self.lock();
                if let Some(entry) = state.tasks.get_mut(task_id) {
                    entry.record = candidate;
                }
                drop(state);
                self.settled.notify_waiters();
            }
        } else {
            // worker 计数是运行内观测，不落盘。
            let mut state = self.lock();
            if let Some(entry) = state.tasks.get_mut(task_id) {
                entry.record = candidate;
            }
        }
    }

    fn persist_then_store(
        &self,
        task_id: &TaskId,
        candidate: TaskRecord,
    ) -> Result<(), TaskLifecycleError> {
        self.persist(&candidate)?;
        let terminal = candidate.phase.is_terminal();
        let mut state = self.lock();
        if let Some(entry) = state.tasks.get_mut(task_id) {
            entry.record = candidate;
        }
        drop(state);
        if terminal {
            self.settled.notify_waiters();
        }
        Ok(())
    }

    /// 等待一个任务真正 settled：已提交终态，或最后一个 worker guard 已回收后
    /// 延迟终态落地。用于取消/EOF 路径在**真实回收之后**再对外声明终态。
    ///
    /// 任务不被跟踪时返回 `None`；取消请求本身不构成 settled，因此这里只认
    /// [`TaskPhase::is_terminal`]，不把 `cancel_requested` 当作结束。
    pub async fn wait_settled(&self, task_id: &TaskId) -> Option<TaskRecord> {
        loop {
            let notified = self.settled.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.snapshot(task_id) {
                Some(record) if record.phase.is_terminal() => return Some(record),
                // A latched persistence failure can never commit the deferred
                // terminal; return the last committed record instead of waiting
                // forever.
                Some(record) if self.is_fail_closed() => return Some(record),
                Some(_) => notified.await,
                None => return None,
            }
        }
    }

    fn persist(&self, record: &TaskRecord) -> Result<(), TaskLifecycleError> {
        if self.is_fail_closed() {
            return Err(TaskLifecycleError::FailClosed(
                self.fail_closed_reason()
                    .unwrap_or_else(|| "a previous persistence failure".into()),
            ));
        }
        match self.sink.on_task_lifecycle(record) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.latch(&error);
                Err(TaskLifecycleError::Persist(error))
            }
        }
    }

    fn latch(&self, reason: &str) {
        if !self.latched.swap(true, Ordering::SeqCst) {
            *self
                .fail_closed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some(bounded(reason, MAX_DETAIL_CHARS));
        }
    }

    fn fail_closed_reason(&self) -> Option<String> {
        self.fail_closed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 取该任务自己的提交锁；不存在时返回一个临时锁（随后会被 NotFound 拒绝）。
    fn task_lock(&self, task_id: &TaskId) -> Arc<Mutex<()>> {
        let state = self.lock();
        state
            .tasks
            .get(task_id)
            .map(|entry| Arc::clone(&entry.lock))
            .unwrap_or_else(|| Arc::new(Mutex::new(())))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TrackerState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

// ---------------------------------------------------------------------------
// 运行内任务句柄与 task-local
// ---------------------------------------------------------------------------

/// 一个运行任务的携带式句柄：真实跟踪器 + 规范任务身份。
///
/// 它不新建状态：所有读写都回到 [`TaskLifecycleTracker`]。引擎在真实调度许可处
/// 从当前 task-local 读取它并准入；workspace / shell 也可在同一 scope 内读到它
/// （[`scope_task`]），从而把 `spawn_blocking` / 子进程回收计入真实 worker。
///
/// `spawn_blocking` 不继承 task-local，因此阻塞闭包必须**显式 move** 该句柄
/// （或它开启的 [`TaskWorkerGuard`]）；引擎不会为阻塞线程重新注入。
#[derive(Clone)]
pub struct TaskHandle {
    tracker: Arc<TaskLifecycleTracker>,
    task_id: TaskId,
}

impl TaskHandle {
    /// 组合真实跟踪器与已有任务身份。
    pub fn new(tracker: Arc<TaskLifecycleTracker>, task_id: TaskId) -> Self {
        Self { tracker, task_id }
    }

    /// 共享的真实跟踪器。
    pub fn tracker(&self) -> &Arc<TaskLifecycleTracker> {
        &self.tracker
    }

    /// 规范任务身份。
    pub fn task_id(&self) -> &TaskId {
        &self.task_id
    }

    /// 登记一次 `Queued`（调用方负责在取得真实许可前完成）。
    pub fn register(&self, detail: Option<&str>) -> Result<TaskRecord, TaskLifecycleError> {
        self.tracker
            .register(TaskIdentity::new(self.task_id.clone()), detail)
    }

    /// 当前记录快照（可在宿主持久化回调内安全调用，不持状态锁）。
    pub fn snapshot(&self) -> Option<TaskRecord> {
        self.tracker.snapshot(&self.task_id)
    }

    /// 追加真实 engine run 身份（set-once）。
    pub fn attach_engine_run(&self, run: EngineRunId) -> Result<TaskRecord, TaskLifecycleError> {
        self.tracker.attach_engine_run(&self.task_id, run)
    }

    /// 追加**创建该子任务的来源** provider tool-call 身份（set-once）。
    pub fn attach_provider_tool_call(
        &self,
        call: ProviderToolCallId,
    ) -> Result<TaskRecord, TaskLifecycleError> {
        self.tracker.attach_provider_tool_call(&self.task_id, call)
    }

    /// 请求取消（排队立即生效；运行中只记录请求）。
    pub fn request_cancel(
        &self,
        reason: Option<&str>,
    ) -> Result<CancelOutcome, TaskLifecycleError> {
        self.tracker.request_cancel(&self.task_id, reason)
    }

    /// 提交终态（仍有 worker 时会被延迟到最后一个 guard 释放）。
    pub fn finish(
        &self,
        settlement: Settlement<'_>,
        detail: Option<&str>,
    ) -> Result<FinishOutcome, TaskLifecycleError> {
        self.tracker.finish(&self.task_id, settlement, detail)
    }

    /// 开始一个真实 worker（阻塞盘读写、shell 回收等）。
    pub fn begin_worker(&self) -> Result<TaskWorkerGuard, TaskLifecycleError> {
        self.tracker.begin_worker(&self.task_id)
    }

    /// 等待任务真正 settled。
    pub async fn wait_settled(&self) -> Option<TaskRecord> {
        self.tracker.wait_settled(&self.task_id).await
    }
}

impl std::fmt::Debug for TaskHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TaskHandle")
            .field("scope", &self.tracker.scope())
            .field("task_id", &self.task_id)
            .finish()
    }
}

tokio::task_local! {
    /// 当前真实运行任务的句柄。只在引擎为一次真实 run 创建的 scope 内有效；
    /// 直接调用 workspace/shell（无引擎 run）时不存在，读取方须如实返回 None。
    static CURRENT_TASK: TaskHandle;
}

/// 当前异步任务上由引擎绑定的真实任务句柄；未绑定（直接 SDK 调用）时为 `None`。
///
/// 这不是身份猜测：没有真实绑定就返回 `None`，调用方不得用 run/tool id 冒充。
pub fn current_task_handle() -> Option<TaskHandle> {
    CURRENT_TASK.try_with(TaskHandle::clone).ok()
}

/// 在真实任务 scope 内运行 future。
///
/// 只有引擎为一次真实 run 调用它；`spawn_blocking` 不继承该 scope，阻塞闭包须
/// 显式 move [`TaskHandle`] 或它开启的 worker guard。
pub fn scope_task<F: Future>(handle: TaskHandle, future: F) -> impl Future<Output = F::Output> {
    CURRENT_TASK.scope(handle, future)
}

enum IdentityField {
    EngineRun(EngineRunId),
    ProviderToolCall(ProviderToolCallId),
}

/// 一个真实 worker 的计数 guard。
///
/// 只要存在未回收的 guard，任务就不会提前进入终态。guard 由 shell / 每个工具调用 /
/// 子任务等真实 worker 持有，**不是** `WorkspaceLease`：任意单个租约的 drop 不会调用
/// 这里，因此不会误结束整个回合。drop 或显式 [`TaskWorkerGuard::release`] 都只递减一次。
pub struct TaskWorkerGuard {
    tracker: Arc<TaskLifecycleTracker>,
    task_id: TaskId,
    released: bool,
}

impl TaskWorkerGuard {
    /// 该 guard 所属的任务。
    pub fn task_id(&self) -> &TaskId {
        &self.task_id
    }

    /// 显式回收（与 drop 等价，只生效一次）。
    pub fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        self.tracker.release_worker(&self.task_id);
    }
}

impl Drop for TaskWorkerGuard {
    fn drop(&mut self) {
        self.release_inner();
    }
}

impl std::fmt::Debug for TaskWorkerGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TaskWorkerGuard")
            .field("scope", &self.tracker.scope())
            .field("task_id", &self.task_id)
            .field("released", &self.released)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// legacy 投影映射
// ---------------------------------------------------------------------------

impl TaskPhase {
    /// 从 `subagent::Status` 投影（六态一一对应）。
    pub const fn from_subagent_status(status: SubagentStatus) -> Self {
        match status {
            SubagentStatus::Queued => Self::Queued,
            SubagentStatus::Running => Self::Running,
            SubagentStatus::Completed => Self::Completed,
            SubagentStatus::Failed => Self::Failed,
            SubagentStatus::Cancelled => Self::Cancelled,
            SubagentStatus::TimedOut => Self::TimedOut,
        }
    }

    /// 投影到 `subagent::Status`（六态一一对应）。
    pub const fn to_subagent_status(self) -> SubagentStatus {
        match self {
            Self::Queued => SubagentStatus::Queued,
            Self::Running => SubagentStatus::Running,
            Self::Completed => SubagentStatus::Completed,
            Self::Failed => SubagentStatus::Failed,
            Self::Cancelled => SubagentStatus::Cancelled,
            Self::TimedOut => SubagentStatus::TimedOut,
        }
    }

    /// 从 dedup `TaskKeyState` 投影。
    ///
    /// `Active` 表示运行中；旧账本的 `Interrupted` 是恢复观测而非终态，返回 `None`
    /// （调用方应设置 [`TaskView::interrupted`]），绝不把它翻译成某个阶段。
    pub const fn from_task_key_state(state: TaskKeyState) -> Option<Self> {
        Some(match state {
            TaskKeyState::Queued => Self::Queued,
            TaskKeyState::Active => Self::Running,
            TaskKeyState::Completed => Self::Completed,
            TaskKeyState::Failed => Self::Failed,
            TaskKeyState::Cancelled => Self::Cancelled,
            TaskKeyState::Interrupted => return None,
        })
    }

    /// 投影到 dedup `TaskKeyState`。
    ///
    /// 旧 schema 没有 `TimedOut`；返回 `None` 而不是把它谎报成 `Failed`。需要 durable
    /// 记录时应显式写一条带超时 detail 的失败，或升级账本 schema。
    pub const fn to_task_key_state(self) -> Option<TaskKeyState> {
        Some(match self {
            Self::Queued => TaskKeyState::Queued,
            Self::Running => TaskKeyState::Active,
            Self::Completed => TaskKeyState::Completed,
            Self::Failed => TaskKeyState::Failed,
            Self::Cancelled => TaskKeyState::Cancelled,
            Self::TimedOut => return None,
        })
    }

    /// 从 journal `RunState` 投影。
    ///
    /// journal 的 `Active` 表示「运行中或意外中断」；本模块把它映射为 `Running`
    /// （journal 运行在开始后才建立）。`Interrupted` 是恢复观测，返回 `None`。
    pub const fn from_run_state(state: RunState) -> Option<Self> {
        Some(match state {
            RunState::Active => Self::Running,
            RunState::Completed => Self::Completed,
            RunState::Failed => Self::Failed,
            RunState::Cancelled => Self::Cancelled,
            RunState::Interrupted => return None,
        })
    }

    /// 投影到 journal `RunState`。
    ///
    /// journal 没有 `Queued` 与 `TimedOut`；两者返回 `None`，不伪造 `Active`/`Failed`。
    pub const fn to_run_state(self) -> Option<RunState> {
        Some(match self {
            Self::Running => RunState::Active,
            Self::Completed => RunState::Completed,
            Self::Failed => RunState::Failed,
            Self::Cancelled => RunState::Cancelled,
            Self::Queued | Self::TimedOut => return None,
        })
    }
}

impl From<SubagentStatus> for TaskPhase {
    fn from(status: SubagentStatus) -> Self {
        Self::from_subagent_status(status)
    }
}

impl From<TaskPhase> for SubagentStatus {
    fn from(phase: TaskPhase) -> Self {
        phase.to_subagent_status()
    }
}

// ---------------------------------------------------------------------------
// 工具函数
// ---------------------------------------------------------------------------

fn bounded(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let mut out: String = value.chars().take(max_chars).collect();
    out.push_str("...");
    out
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
    fn transition_table_is_exact() {
        use TaskPhase::*;
        for (from, to, expected) in [
            (Queued, Queued, true),
            (Queued, Running, true),
            (Queued, Completed, false),
            (Queued, Failed, true),
            (Queued, Cancelled, true),
            (Queued, TimedOut, false),
            (Running, Queued, false),
            (Running, Running, true),
            (Running, Completed, true),
            (Running, Failed, true),
            (Running, Cancelled, true),
            (Running, TimedOut, true),
            (Completed, Running, false),
            (Completed, Queued, false),
            (Completed, Completed, true),
            (Failed, Cancelled, false),
            (Cancelled, Running, false),
            (TimedOut, Completed, false),
            (TimedOut, TimedOut, true),
        ] {
            assert_eq!(
                TaskPhase::transition_allowed(from, to),
                expected,
                "{from:?} -> {to:?}"
            );
        }
        for terminal in [Completed, Failed, Cancelled, TimedOut] {
            assert!(terminal.is_terminal());
            assert!(!terminal.is_active());
            for other in [Queued, Running, Completed, Failed, Cancelled, TimedOut] {
                assert_eq!(
                    TaskPhase::transition_allowed(terminal, other),
                    other == terminal
                );
            }
        }
    }
}
