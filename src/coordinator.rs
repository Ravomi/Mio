//! 进程内协作准入控制（Coordinator）。
//!
//! 本模块解决同一 Mio 进程内多代理并发时的四类问题：
//!
//! 1. **限流 / 429 请求风暴**：模型请求需要同时拿到「全局模型槽位」与「provider 槽位」，
//!    并支持共享的 provider cooldown（[`Coordinator::defer_provider`]）。
//! 2. **多代理写同一文件冲突**：任务按显式的相对工作区路径申请写范围，
//!    目录祖先 / 子路径视为冲突；只读任务之间可以并发。
//! 3. **编译 / 浏览器测试争资源**：重任务通过 [`Coordinator::acquire_heavy`] 单独限流。
//! 4. **重复探索与预算失控**：显式 `dedup_key` 只合并 / 拒绝同 key 的活动或排队任务；
//!    可选的 `max_model_calls` / `max_total_tokens` 在调用前原子预留，不会并发超发。
//!
//! # 设计边界（务必阅读）
//!
//! - 只约束**接入本模块的协作调用者**。它不感知操作系统内存 / CPU / 文件描述符，
//!   也不是资源沙箱；`heavy` 只是调用者显式声明的逻辑标记。
//! - 所有状态是**进程内**的（`Arc` 共享），不做跨进程协调。
//! - 任务 / 模型 / 重任务使用**互相独立**的队列和计数。在任务队列里等待不会持有
//!   模型或重任务名额，因此一个等待文件冲突的任务不会阻塞另一个任务获取模型 /
//!   重任务许可（避免循环等待）。
//! - `provider_key` 由调用者提供，必须是**不透明的安全标识**（如 `"openai"`、
//!   内部路由名），不得传入 API key / token / 凭据；本模块只把它当作队列分组与
//!   cooldown 键，不写日志。
//! - `dedup_key` 是**显式**的：只比较字符串相等，不做任何语义理解。相同 key 的活动
//!   或排队任务会被拒绝，并在错误中返回已有任务的 [`TaskId`] 供宿主复用。
//! - token 预算是**令牌数量**的硬上限，不是精确的美元费用；本模块不承诺任何价格换算。
//! - 取消安全：等待中的 future 被 `drop` 时会从队列移除并唤醒后继；已获得的 permit
//!   通过 RAII 释放，不会泄漏名额。
//!
//! 通过 `#[path = "../src/coordinator.rs"]` 被测试与 `bin/mio.rs` 引用，避免与并行
//! 修改 `lib.rs` 的变更冲突。

use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    path::{Component, Path},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use tokio::sync::Notify;

/// 默认的任务 / 代理并发上限。
pub const DEFAULT_AGENT_CONCURRENCY: usize = 4;
/// 默认的重任务并发上限。
pub const DEFAULT_HEAVY_CONCURRENCY: usize = 2;
/// 默认的全局模型请求并发上限。
pub const DEFAULT_MODEL_CONCURRENCY: usize = 2;
/// 默认的单 provider 模型请求并发上限。
pub const DEFAULT_PROVIDER_MODEL_CONCURRENCY: usize = 2;
/// 默认的每个独立队列容量上限。
pub const DEFAULT_QUEUE_CAPACITY: usize = 64;

/// 单个并发上限的合理上界，避免错误配置一次性铺开。
const MAX_CONCURRENCY: usize = 1024;
/// 队列容量的合理上界。
const MAX_QUEUE_CAPACITY: usize = 65_536;
/// 极端 `Duration` 下的 cooldown 兜底（一天）。
const COOLDOWN_FALLBACK: Duration = Duration::from_secs(86_400);

fn lock_state<T>(state: &Mutex<T>) -> MutexGuard<'_, T> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// 准入配置。默认值：`agent=4`、`heavy=2`、`model=2`、`provider_model=2`、
/// 队列容量 `64`，无预算上限。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    /// 同时活动的任务（代理）数量上限。
    pub agent: usize,
    /// 同时活动的重任务数量上限。
    pub heavy: usize,
    /// 所有 provider 合计的模型请求并发上限。
    pub model: usize,
    /// 单个 provider 的模型请求并发上限（可被 `provider_model_overrides` 覆盖）。
    pub provider_model: usize,
    /// 任务 / 模型 / 重任务各自队列的容量上限；超出时返回
    /// [`CoordinatorError::QueueFull`]。
    pub queue_capacity: usize,
    /// 可选的模型调用次数硬上限；每次授予模型许可时占用一次，结算时不退还。
    pub max_model_calls: Option<u64>,
    /// 可选的 token 总数硬上限；调用前原子预留，结算时按真实 usage 结算。
    pub max_total_tokens: Option<u64>,
    /// 针对特定 provider 的模型并发覆盖值。
    pub provider_model_overrides: BTreeMap<String, usize>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            agent: DEFAULT_AGENT_CONCURRENCY,
            heavy: DEFAULT_HEAVY_CONCURRENCY,
            model: DEFAULT_MODEL_CONCURRENCY,
            provider_model: DEFAULT_PROVIDER_MODEL_CONCURRENCY,
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            max_model_calls: None,
            max_total_tokens: None,
            provider_model_overrides: BTreeMap::new(),
        }
    }
}

impl Limits {
    /// 校验配置；非法配置在 [`Coordinator::new`] 阶段 fail-closed。
    pub fn validate(&self) -> Result<(), CoordinatorError> {
        for (name, value) in [
            ("agent", self.agent),
            ("heavy", self.heavy),
            ("model", self.model),
            ("provider_model", self.provider_model),
        ] {
            if value == 0 {
                return Err(CoordinatorError::InvalidLimits(format!(
                    "{name} must be >= 1"
                )));
            }
            if value > MAX_CONCURRENCY {
                return Err(CoordinatorError::InvalidLimits(format!(
                    "{name} must be <= {MAX_CONCURRENCY}, got {value}"
                )));
            }
        }
        if self.queue_capacity == 0 {
            return Err(CoordinatorError::InvalidLimits(
                "queue_capacity must be >= 1".into(),
            ));
        }
        if self.queue_capacity > MAX_QUEUE_CAPACITY {
            return Err(CoordinatorError::InvalidLimits(format!(
                "queue_capacity must be <= {MAX_QUEUE_CAPACITY}, got {}",
                self.queue_capacity
            )));
        }
        for (provider, value) in &self.provider_model_overrides {
            if provider.trim().is_empty() {
                return Err(CoordinatorError::InvalidLimits(
                    "provider_model_overrides key must not be empty".into(),
                ));
            }
            if *value == 0 || *value > MAX_CONCURRENCY {
                return Err(CoordinatorError::InvalidLimits(format!(
                    "provider_model_overrides[{provider}] must be in 1..={MAX_CONCURRENCY}"
                )));
            }
        }
        Ok(())
    }

    /// 返回某个 provider 实际生效的模型并发上限。
    pub fn provider_limit(&self, provider: &str) -> usize {
        self.provider_model_overrides
            .get(provider)
            .copied()
            .unwrap_or(self.provider_model)
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// [`Coordinator`] 的错误类型。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoordinatorError {
    /// 配置非法。
    InvalidLimits(String),
    /// 任务路径非法（绝对路径、包含 `..`、为空等）。
    InvalidScope(String),
    /// 显式 dedup key 已存在活动或排队任务；`existing` 供宿主复用。
    DuplicateTask {
        /// 冲突的 dedup key（原样回传，不作解析）。
        dedup_key: String,
        /// 已有任务的稳定标识。
        existing: TaskId,
    },
    /// 目标队列已达容量上限。
    QueueFull {
        /// 队列名称：`task` / `model` / `heavy`。
        queue: &'static str,
        /// 队列容量。
        capacity: usize,
    },
    /// 模型调用次数预算已耗尽。
    ModelCallBudgetExceeded {
        /// 已占用的调用次数。
        used: u64,
        /// 配置上限。
        limit: u64,
    },
    /// token 硬预算已耗尽（曾发生超额用量）。
    BudgetExhausted {
        /// 已结算的 token 数。
        tokens_used: u64,
        /// 超出预算的 token 数。
        tokens_overage: u64,
        /// 配置上限（若设置）。
        limit: Option<u64>,
    },
    /// 单次预留本身就超过 token 上限，无法满足。
    TokenReservationTooLarge {
        /// 请求预留的 token 数。
        reservation: u64,
        /// 配置上限。
        limit: u64,
    },
}

impl fmt::Display for CoordinatorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits(msg) => write!(f, "invalid coordinator limits: {msg}"),
            Self::InvalidScope(msg) => write!(f, "invalid task scope: {msg}"),
            Self::DuplicateTask {
                dedup_key,
                existing,
            } => write!(
                f,
                "duplicate task dedup_key {dedup_key:?}; existing task {existing}"
            ),
            Self::QueueFull { queue, capacity } => {
                write!(f, "{queue} queue is full (capacity {capacity})")
            }
            Self::ModelCallBudgetExceeded { used, limit } => {
                write!(
                    f,
                    "model call budget exceeded: used {used} >= limit {limit}"
                )
            }
            Self::BudgetExhausted {
                tokens_used,
                tokens_overage,
                limit,
            } => write!(
                f,
                "token budget exhausted: used {tokens_used}, overage {tokens_overage}, limit {limit:?}"
            ),
            Self::TokenReservationTooLarge { reservation, limit } => write!(
                f,
                "token reservation {reservation} exceeds budget limit {limit}"
            ),
        }
    }
}

impl std::error::Error for CoordinatorError {}

// ---------------------------------------------------------------------------
// Task identity and scope
// ---------------------------------------------------------------------------

/// 任务稳定标识。用于 dedup 命中时供宿主复用已有任务。
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskId(String);

impl TaskId {
    fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// 以字符串形式返回标识。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 任务申请。`path` 是相对于工作区根目录的路径，逐组件校验。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskRequest {
    /// 可选的显式去重键；只做字符串相等比较。
    pub dedup_key: Option<String>,
    /// 相对工作区路径（不得为绝对路径，不得包含 `..`，不得为空）。
    pub path: String,
    /// `true` 表示写范围，`false` 表示只读。
    pub write: bool,
}

impl TaskRequest {
    /// 申请对 `path` 的写范围。
    pub fn write(path: impl Into<String>) -> Self {
        Self {
            dedup_key: None,
            path: path.into(),
            write: true,
        }
    }

    /// 申请对 `path` 的只读范围（读任务之间可以并发）。
    pub fn read(path: impl Into<String>) -> Self {
        Self {
            dedup_key: None,
            path: path.into(),
            write: false,
        }
    }

    /// 附加显式去重键。
    pub fn with_dedup_key(mut self, key: impl Into<String>) -> Self {
        self.dedup_key = Some(key.into());
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TaskScope {
    components: Vec<String>,
    write: bool,
}

impl TaskScope {
    fn parse(path: &str, write: bool) -> Result<Self, CoordinatorError> {
        if path.is_empty() {
            return Err(CoordinatorError::InvalidScope(
                "empty path not allowed".into(),
            ));
        }
        let raw = Path::new(path);
        if raw.is_absolute() {
            return Err(CoordinatorError::InvalidScope(format!(
                "absolute path not allowed: {path:?}"
            )));
        }
        let mut components = Vec::new();
        for component in raw.components() {
            match component {
                Component::ParentDir => {
                    return Err(CoordinatorError::InvalidScope(format!(
                        "parent directory navigation not allowed: {path:?}"
                    )));
                }
                Component::Normal(part) => components.push(part.to_string_lossy().into_owned()),
                Component::CurDir => {}
                _ => {
                    return Err(CoordinatorError::InvalidScope(format!(
                        "absolute path not allowed: {path:?}"
                    )));
                }
            }
        }
        // `"."`（或等价的纯 CurDir 序列）代表工作区根：空组件列表与所有子路径冲突。
        Ok(Self { components, write })
    }

    fn rendered(&self) -> String {
        if self.components.is_empty() {
            ".".to_string()
        } else {
            self.components.join("/")
        }
    }
}

fn is_ancestor_or_equal(a: &[String], b: &[String]) -> bool {
    a.len() <= b.len() && a.iter().zip(b.iter()).all(|(x, y)| x == y)
}

fn scopes_conflict(a: &TaskScope, b: &TaskScope) -> bool {
    if !a.write && !b.write {
        return false;
    }
    is_ancestor_or_equal(&a.components, &b.components)
        || is_ancestor_or_equal(&b.components, &a.components)
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

struct Inner {
    limits: Limits,
    state: Mutex<State>,
    notify: Notify,
}

#[derive(Default)]
struct State {
    next_seq: u64,
    tasks_active: Vec<ActiveTask>,
    tasks_queued: Vec<TaskWaiter>,
    heavy_active: usize,
    heavy_queued: Vec<HeavyWaiter>,
    model_active_total: usize,
    model_active_by_provider: HashMap<String, usize>,
    model_queued: Vec<ModelWaiter>,
    cooldowns: HashMap<String, Instant>,
    model_calls_started: u64,
    tokens_reserved_active: u64,
    tokens_used: u64,
    tokens_over_reservation: u64,
    tokens_overage: u64,
    budget_blocked: bool,
}

struct ActiveTask {
    id: TaskId,
    dedup_key: Option<String>,
    scope: TaskScope,
}

struct TaskWaiter {
    seq: u64,
    id: TaskId,
    dedup_key: Option<String>,
    scope: TaskScope,
}

struct ModelWaiter {
    seq: u64,
    provider: String,
    reservation: u64,
}

struct HeavyWaiter {
    seq: u64,
}

impl State {
    fn find_duplicate(&self, dedup_key: &str, exclude_seq: Option<u64>) -> Option<TaskId> {
        if let Some(existing) = self
            .tasks_active
            .iter()
            .find(|task| task.dedup_key.as_deref() == Some(dedup_key))
        {
            return Some(existing.id.clone());
        }
        self.tasks_queued
            .iter()
            .filter(|waiter| Some(waiter.seq) != exclude_seq)
            .find(|waiter| waiter.dedup_key.as_deref() == Some(dedup_key))
            .map(|waiter| waiter.id.clone())
    }

    fn next_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        seq
    }
}

// ---------------------------------------------------------------------------
// Permits
// ---------------------------------------------------------------------------

/// 任务许可。`drop` 时释放活动任务名额并唤醒排队者。
#[must_use = "dropping the permit immediately releases the task slot"]
pub struct TaskPermit {
    inner: Arc<Inner>,
    id: TaskId,
    dedup_key: Option<String>,
    scope: TaskScope,
}

impl TaskPermit {
    /// 任务稳定标识。
    pub fn id(&self) -> &TaskId {
        &self.id
    }

    /// 附加的显式 dedup key（若有）。
    pub fn dedup_key(&self) -> Option<&str> {
        self.dedup_key.as_deref()
    }

    /// 是否为写范围。
    pub fn is_write(&self) -> bool {
        self.scope.write
    }

    /// 规范化后的相对路径。
    pub fn path(&self) -> String {
        self.scope.rendered()
    }
}

impl fmt::Debug for TaskPermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskPermit")
            .field("id", &self.id)
            .field("path", &self.scope.rendered())
            .field("write", &self.scope.write)
            .finish()
    }
}

impl Drop for TaskPermit {
    fn drop(&mut self) {
        {
            let mut state = lock_state(&self.inner.state);
            state.tasks_active.retain(|task| task.id != self.id);
        }
        self.inner.notify.notify_waiters();
    }
}

/// 模型请求许可。`drop` 时保守结算（未知 usage 视为占满预留额）。
#[must_use = "dropping the permit immediately releases the model slots and settles the budget"]
pub struct ModelPermit {
    inner: Arc<Inner>,
    provider: String,
    reservation: u64,
    released: bool,
}

impl ModelPermit {
    /// provider 标识（原样回传调用者传入的不透明安全标识）。
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// 本次调用预留的 token 数。
    pub fn reservation(&self) -> u64 {
        self.reservation
    }

    /// 以真实 usage 结算。`None` 表示 usage 未知：保守按预留额计费，绝不按 0 计。
    /// 超过总预算时记录超额并阻止后续模型请求。
    pub fn complete(mut self, usage: Option<u64>) {
        self.settle(usage);
    }

    fn settle(&mut self, usage: Option<u64>) {
        if self.released {
            return;
        }
        self.released = true;
        {
            let mut state = lock_state(&self.inner.state);
            if let Some(active) = state.model_active_by_provider.get_mut(&self.provider) {
                *active = active.saturating_sub(1);
                if *active == 0 {
                    state.model_active_by_provider.remove(&self.provider);
                }
            }
            state.model_active_total = state.model_active_total.saturating_sub(1);
            state.tokens_reserved_active = state
                .tokens_reserved_active
                .saturating_sub(self.reservation);
            let actual = usage.unwrap_or(self.reservation);
            state.tokens_used = state.tokens_used.saturating_add(actual);
            if actual > self.reservation {
                state.tokens_over_reservation = state
                    .tokens_over_reservation
                    .saturating_add(actual - self.reservation);
            }
            if let Some(limit) = self.inner.limits.max_total_tokens
                && state.tokens_used > limit
            {
                state.tokens_overage = state.tokens_overage.max(state.tokens_used - limit);
                state.budget_blocked = true;
            }
        }
        self.inner.notify.notify_waiters();
    }
}

impl fmt::Debug for ModelPermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelPermit")
            .field("provider", &self.provider)
            .field("reservation", &self.reservation)
            .field("released", &self.released)
            .finish()
    }
}

impl Drop for ModelPermit {
    fn drop(&mut self) {
        self.settle(None);
    }
}

/// 重任务许可。`drop` 时释放名额。
#[must_use = "dropping the permit immediately releases the heavy slot"]
pub struct HeavyPermit {
    inner: Arc<Inner>,
    released: bool,
}

impl fmt::Debug for HeavyPermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HeavyPermit")
            .field("released", &self.released)
            .finish()
    }
}

impl Drop for HeavyPermit {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        {
            let mut state = lock_state(&self.inner.state);
            state.heavy_active = state.heavy_active.saturating_sub(1);
        }
        self.inner.notify.notify_waiters();
    }
}

// ---------------------------------------------------------------------------
// Coordinator
// ---------------------------------------------------------------------------

/// 进程内协作准入控制器；内部状态 `Arc` 共享，可廉价克隆。
#[derive(Clone)]
pub struct Coordinator {
    inner: Arc<Inner>,
}

impl Coordinator {
    /// 用给定配置创建控制器；非法配置 fail-closed。
    pub fn new(limits: Limits) -> Result<Self, CoordinatorError> {
        limits.validate()?;
        Ok(Self {
            inner: Arc::new(Inner {
                limits,
                state: Mutex::new(State::default()),
                notify: Notify::new(),
            }),
        })
    }

    /// 使用默认配置创建控制器。
    pub fn with_defaults() -> Self {
        Self::new(Limits::default()).expect("default limits are always valid")
    }

    /// 当前生效的配置。
    pub fn limits(&self) -> &Limits {
        &self.inner.limits
    }

    /// 查找活动或排队的 dedup key 对应的任务标识，供宿主复用。
    pub fn lookup_dedup(&self, dedup_key: &str) -> Option<TaskId> {
        let state = lock_state(&self.inner.state);
        state.find_duplicate(dedup_key, None)
    }

    /// 为某个 provider 设置共享 cooldown，取「已有 deadline」与「新 deadline」的较大值。
    ///
    /// `provider_key` 必须是不透明的安全标识，不得包含凭据。
    pub fn defer_provider(&self, provider_key: &str, duration: Duration) {
        let deadline = Instant::now()
            .checked_add(duration)
            .unwrap_or_else(|| Instant::now() + COOLDOWN_FALLBACK);
        {
            let mut state = lock_state(&self.inner.state);
            match state.cooldowns.get(provider_key).copied() {
                Some(existing) if existing >= deadline => {}
                _ => {
                    state.cooldowns.insert(provider_key.to_string(), deadline);
                }
            }
        }
        self.inner.notify.notify_waiters();
    }

    /// 申请任务许可。显式 `dedup_key` 命中活动或排队任务时返回
    /// [`CoordinatorError::DuplicateTask`]。
    ///
    /// 队列公平策略：更早的等待者优先，且更早且与自身作用域冲突的等待者（即使它
    /// 当前因活动锁而不可授予）会阻止后来者越过，避免写者被持续到达的读者饿死；
    /// 与自身不相交的更早等待者不阻塞，因此不相交任务仍可并发进入。
    pub async fn acquire_task(&self, request: TaskRequest) -> Result<TaskPermit, CoordinatorError> {
        let scope = TaskScope::parse(&request.path, request.write)?;
        let mut seq: Option<u64> = None;
        let mut guard: Option<QueueGuard> = None;
        loop {
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = lock_state(&self.inner.state);
                if let Some(key) = request.dedup_key.as_deref()
                    && let Some(existing) = state.find_duplicate(key, seq)
                {
                    return Err(CoordinatorError::DuplicateTask {
                        dedup_key: key.to_string(),
                        existing,
                    });
                }
                if seq.is_none() {
                    if state.tasks_queued.len() >= self.inner.limits.queue_capacity {
                        return Err(CoordinatorError::QueueFull {
                            queue: "task",
                            capacity: self.inner.limits.queue_capacity,
                        });
                    }
                    let next = state.next_seq();
                    state.tasks_queued.push(TaskWaiter {
                        seq: next,
                        id: TaskId::generate(),
                        dedup_key: request.dedup_key.clone(),
                        scope: scope.clone(),
                    });
                    seq = Some(next);
                    guard = Some(QueueGuard::new(&self.inner, next, QueueKind::Task));
                }
                let current = seq.unwrap();
                if let Some(permit) = try_grant_task(&mut state, &self.inner, current) {
                    if let Some(gate) = guard.as_mut() {
                        gate.disarm();
                    }
                    drop(state);
                    self.inner.notify.notify_waiters();
                    return Ok(permit);
                }
            }
            notified.await;
        }
    }

    /// 申请模型请求许可。`token_reservation` 是调用前预留的 token 数。
    ///
    /// 需要同时满足全局与 provider 并发上限、provider cooldown（若在冷却中会等待），
    /// 以及可选的 `max_model_calls` / `max_total_tokens` 硬预算。
    pub async fn acquire_model(
        &self,
        provider_key: &str,
        token_reservation: u64,
    ) -> Result<ModelPermit, CoordinatorError> {
        let provider = provider_key.to_string();
        if let Some(limit) = self.inner.limits.max_total_tokens
            && token_reservation > limit
        {
            return Err(CoordinatorError::TokenReservationTooLarge {
                reservation: token_reservation,
                limit,
            });
        }
        let mut seq: Option<u64> = None;
        let mut guard: Option<QueueGuard> = None;
        loop {
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let mut cooldown_wait: Option<Duration> = None;
            {
                let mut state = lock_state(&self.inner.state);
                if state.budget_blocked {
                    return Err(CoordinatorError::BudgetExhausted {
                        tokens_used: state.tokens_used,
                        tokens_overage: state.tokens_overage,
                        limit: self.inner.limits.max_total_tokens,
                    });
                }
                // Settled usage never decreases. Refunds only reduce active
                // reservations, so they cannot make this condition feasible.
                if let Some(limit) = self.inner.limits.max_total_tokens
                    && state.tokens_used.saturating_add(token_reservation) > limit
                {
                    return Err(CoordinatorError::BudgetExhausted {
                        tokens_used: state.tokens_used,
                        tokens_overage: state.tokens_overage,
                        limit: Some(limit),
                    });
                }
                if let Some(limit) = self.inner.limits.max_model_calls
                    && state.model_calls_started >= limit
                {
                    return Err(CoordinatorError::ModelCallBudgetExceeded {
                        used: state.model_calls_started,
                        limit,
                    });
                }
                if seq.is_none() {
                    if state.model_queued.len() >= self.inner.limits.queue_capacity {
                        return Err(CoordinatorError::QueueFull {
                            queue: "model",
                            capacity: self.inner.limits.queue_capacity,
                        });
                    }
                    let next = state.next_seq();
                    state.model_queued.push(ModelWaiter {
                        seq: next,
                        provider: provider.clone(),
                        reservation: token_reservation,
                    });
                    seq = Some(next);
                    guard = Some(QueueGuard::new(&self.inner, next, QueueKind::Model));
                }
                let current = seq.unwrap();
                if let Some(permit) = try_grant_model(&mut state, &self.inner, current) {
                    if let Some(gate) = guard.as_mut() {
                        gate.disarm();
                    }
                    drop(state);
                    self.inner.notify.notify_waiters();
                    return Ok(permit);
                }
                if let Some(deadline) = state.cooldowns.get(&provider).copied() {
                    let now = Instant::now();
                    if deadline > now {
                        cooldown_wait = Some(deadline - now);
                    }
                }
            }
            match cooldown_wait {
                Some(remaining) => {
                    tokio::select! {
                        _ = notified.as_mut() => {}
                        _ = tokio::time::sleep(remaining) => {}
                    }
                }
                None => notified.await,
            }
        }
    }

    /// 申请重任务许可。重任务队列独立，与其他队列不互相阻塞。
    pub async fn acquire_heavy(&self) -> Result<HeavyPermit, CoordinatorError> {
        let mut seq: Option<u64> = None;
        let mut guard: Option<QueueGuard> = None;
        loop {
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = lock_state(&self.inner.state);
                if seq.is_none() {
                    if state.heavy_queued.len() >= self.inner.limits.queue_capacity {
                        return Err(CoordinatorError::QueueFull {
                            queue: "heavy",
                            capacity: self.inner.limits.queue_capacity,
                        });
                    }
                    let next = state.next_seq();
                    state.heavy_queued.push(HeavyWaiter { seq: next });
                    seq = Some(next);
                    guard = Some(QueueGuard::new(&self.inner, next, QueueKind::Heavy));
                }
                let current = seq.unwrap();
                if let Some(permit) = try_grant_heavy(&mut state, &self.inner, current) {
                    if let Some(gate) = guard.as_mut() {
                        gate.disarm();
                    }
                    drop(state);
                    self.inner.notify.notify_waiters();
                    return Ok(permit);
                }
            }
            notified.await;
        }
    }

    /// 返回不含任何凭据的状态快照。
    pub fn snapshot(&self) -> Snapshot {
        let state = lock_state(&self.inner.state);
        let now = Instant::now();
        let cooling = state
            .cooldowns
            .iter()
            .filter_map(|(provider, deadline)| {
                if *deadline > now {
                    Some((provider.clone(), *deadline - now))
                } else {
                    None
                }
            })
            .collect();
        let active_by_provider = state
            .model_active_by_provider
            .iter()
            .filter(|(_, count)| **count > 0)
            .map(|(provider, count)| (provider.clone(), *count))
            .collect();
        Snapshot {
            limits: self.inner.limits.clone(),
            tasks: TaskSnapshot {
                active: state.tasks_active.len(),
                queued: state.tasks_queued.len(),
                limit: self.inner.limits.agent,
            },
            heavy: HeavySnapshot {
                active: state.heavy_active,
                queued: state.heavy_queued.len(),
                limit: self.inner.limits.heavy,
            },
            model: ModelSnapshot {
                active_total: state.model_active_total,
                queued: state.model_queued.len(),
                limit: self.inner.limits.model,
                active_by_provider,
                cooling,
            },
            budget: BudgetSnapshot {
                model_calls_started: state.model_calls_started,
                max_model_calls: self.inner.limits.max_model_calls,
                tokens_reserved_active: state.tokens_reserved_active,
                tokens_used: state.tokens_used,
                tokens_over_reservation: state.tokens_over_reservation,
                tokens_overage: state.tokens_overage,
                max_total_tokens: self.inner.limits.max_total_tokens,
                blocked: state.budget_blocked,
            },
        }
    }
}

impl Default for Coordinator {
    fn default() -> Self {
        Self::with_defaults()
    }
}

// ---------------------------------------------------------------------------
// Granting
// ---------------------------------------------------------------------------

fn task_eligible(state: &State, limits: &Limits, scope: &TaskScope) -> bool {
    state.tasks_active.len() < limits.agent
        && !state
            .tasks_active
            .iter()
            .any(|active| scopes_conflict(&active.scope, scope))
}

fn try_grant_task(state: &mut State, inner: &Arc<Inner>, seq: u64) -> Option<TaskPermit> {
    let index = state.tasks_queued.iter().position(|w| w.seq == seq)?;
    let scope = state.tasks_queued[index].scope.clone();
    if !task_eligible(state, &inner.limits, &scope) {
        return None;
    }
    // 任何更早且与自身作用域冲突的等待者（即使它当前不可授予，例如被活动读锁
    // 挡住）都必须先处理，防止后来者持续越过导致写者饿死；与自身不相交的更早
    // 等待者不阻塞。
    if state.tasks_queued[..index]
        .iter()
        .any(|earlier| scopes_conflict(&earlier.scope, &scope))
    {
        return None;
    }
    let waiter = state.tasks_queued.remove(index);
    let permit = TaskPermit {
        inner: Arc::clone(inner),
        id: waiter.id.clone(),
        dedup_key: waiter.dedup_key.clone(),
        scope: waiter.scope.clone(),
    };
    state.tasks_active.push(ActiveTask {
        id: waiter.id,
        dedup_key: waiter.dedup_key,
        scope: waiter.scope,
    });
    Some(permit)
}

fn try_grant_model(state: &mut State, inner: &Arc<Inner>, seq: u64) -> Option<ModelPermit> {
    let index = state.model_queued.iter().position(|w| w.seq == seq)?;
    let provider = state.model_queued[index].provider.clone();
    let reservation = state.model_queued[index].reservation;
    // 同 provider 的 FIFO：只允许该 provider 队列头部获得许可。
    let provider_head = state
        .model_queued
        .iter()
        .filter(|w| w.provider == provider)
        .map(|w| w.seq)
        .min()?;
    if provider_head != seq {
        return None;
    }
    if let Some(deadline) = state.cooldowns.get(&provider)
        && *deadline > Instant::now()
    {
        return None;
    }
    if state.model_active_total >= inner.limits.model {
        return None;
    }
    let active_provider = state
        .model_active_by_provider
        .get(&provider)
        .copied()
        .unwrap_or(0);
    if active_provider >= inner.limits.provider_limit(&provider) {
        return None;
    }
    if let Some(limit) = inner.limits.max_total_tokens {
        let committed = state
            .tokens_used
            .saturating_add(state.tokens_reserved_active);
        if committed.saturating_add(reservation) > limit {
            return None;
        }
    }
    state.model_queued.remove(index);
    state.model_calls_started = state.model_calls_started.saturating_add(1);
    state.tokens_reserved_active = state.tokens_reserved_active.saturating_add(reservation);
    state.model_active_total = state.model_active_total.saturating_add(1);
    *state
        .model_active_by_provider
        .entry(provider.clone())
        .or_insert(0) += 1;
    Some(ModelPermit {
        inner: Arc::clone(inner),
        provider,
        reservation,
        released: false,
    })
}

fn try_grant_heavy(state: &mut State, inner: &Arc<Inner>, seq: u64) -> Option<HeavyPermit> {
    let index = state.heavy_queued.iter().position(|w| w.seq == seq)?;
    if state.heavy_queued[0].seq != seq {
        return None;
    }
    if state.heavy_active >= inner.limits.heavy {
        return None;
    }
    state.heavy_queued.remove(index);
    state.heavy_active = state.heavy_active.saturating_add(1);
    Some(HeavyPermit {
        inner: Arc::clone(inner),
        released: false,
    })
}

// ---------------------------------------------------------------------------
// Cancellation guard
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum QueueKind {
    Task,
    Model,
    Heavy,
}

/// 等待期间的取消保护：future 被 `drop` 时把自身从对应队列移除并唤醒后继。
/// 授予成功后会 `disarm`，此时等待者已从队列移除。
struct QueueGuard {
    inner: Arc<Inner>,
    seq: u64,
    queue: QueueKind,
    active: bool,
}

impl QueueGuard {
    fn new(inner: &Arc<Inner>, seq: u64, queue: QueueKind) -> Self {
        Self {
            inner: Arc::clone(inner),
            seq,
            queue,
            active: true,
        }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for QueueGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        {
            let mut state = lock_state(&self.inner.state);
            match self.queue {
                QueueKind::Task => state.tasks_queued.retain(|w| w.seq != self.seq),
                QueueKind::Model => state.model_queued.retain(|w| w.seq != self.seq),
                QueueKind::Heavy => state.heavy_queued.retain(|w| w.seq != self.seq),
            }
        }
        self.inner.notify.notify_waiters();
    }
}

// ---------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------

/// [`Coordinator::snapshot`] 的结果；只包含计数、配置与冷却剩余时间。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// 当前配置。
    pub limits: Limits,
    /// 任务队列状态。
    pub tasks: TaskSnapshot,
    /// 模型队列状态。
    pub model: ModelSnapshot,
    /// 重任务队列状态。
    pub heavy: HeavySnapshot,
    /// 预算状态。
    pub budget: BudgetSnapshot,
}

/// 任务队列快照。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskSnapshot {
    /// 活动任务数。
    pub active: usize,
    /// 排队任务数。
    pub queued: usize,
    /// 活动任务上限。
    pub limit: usize,
}

/// 模型队列快照。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelSnapshot {
    /// 全局活动模型请求数。
    pub active_total: usize,
    /// 排队模型请求数。
    pub queued: usize,
    /// 全局模型并发上限。
    pub limit: usize,
    /// 按 provider 分组的活动请求数（provider 为不透明安全标识）。
    pub active_by_provider: BTreeMap<String, usize>,
    /// 仍在冷却中的 provider 及其剩余时间。
    pub cooling: BTreeMap<String, Duration>,
}

/// 重任务队列快照。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeavySnapshot {
    /// 活动重任务数。
    pub active: usize,
    /// 排队重任务数。
    pub queued: usize,
    /// 重任务并发上限。
    pub limit: usize,
}

/// 预算快照。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct BudgetSnapshot {
    /// 已开始的模型调用次数。
    pub model_calls_started: u64,
    /// 调用次数硬上限（若设置）。
    pub max_model_calls: Option<u64>,
    /// 当前活动的预留 token 总数。
    pub tokens_reserved_active: u64,
    /// 已结算的 token 数（含未知 usage 的保守计费）。
    pub tokens_used: u64,
    /// 结算值超过预留额的累计 token 数。
    pub tokens_over_reservation: u64,
    /// 超过 token 硬预算的累计 token 数。
    pub tokens_overage: u64,
    /// token 硬预算上限（若设置）。
    pub max_total_tokens: Option<u64>,
    /// 是否已因超额用量而阻止后续模型请求。
    pub blocked: bool,
}
