//! 有界、非阻塞的 CLI / RPC JSONL 输出传输。
//!
//! 问题：`Host::on_event` 等回调由 Agent 在同一个 Future 内同步调用。若回调直接
//! 写 stdout，而客户端不读 stdout，管道缓冲区写满后回调会永久阻塞，RPC 循环也
//! 就无法处理 `abort`。这里把写 stdout 移到一个独占线程，并用有界队列与显式丢弃
//! 计数把背压变成可观测、可判定的行为：
//!
//! - 低价值流式增量（`text`、`reasoning`）在队列满时被丢弃并计数，永不阻塞回调。
//! - 结构化状态/结束/错误/审批事件不被静默丢弃；队列满时先驱逐已排队的流式增量，
//!   仍装不下就立即 fail-closed，由调用方取消活动工作，不允许用缺失事件冒充成功。
//! - 所有入队都不等待消费者：同步 `Host` 回调（审批请求、工具与模型事件）不会因为
//!   客户端不读 stdout 而阻塞，RPC 的 `abort` 始终可被及时处理。
//! - 传输 fail-closed 或 writer 线程无法启动时，等待取消的 future 会被立即唤醒。
//! - 退出时只做有界 flush；stdout 阻塞时放弃等待并让进程退出，不永久挂起。
//!
//! 仅纯文本模式不创建该传输，因此不给 `mio` 文本交互增加任何界面或输出。

use mio_coding_agent::rpc::RPC_SCHEMA_VERSION;
use mio_coding_agent::run_observation::{RunObservation, RunObservationKind};
use mio_coding_agent::tool_identity::ToolIdentity;
use serde_json::{Map, Value, json};
use std::{
    collections::VecDeque,
    io::Write,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use uuid::Uuid;

/// 事件通道容量（条）。
pub(super) const EVENT_QUEUE_CAPACITY: usize = 256;
/// 单事件序列化上限。
pub(super) const MAX_EVENT_BYTES: usize = 1024 * 1024;
/// 队列内排队负载上限。
pub(super) const MAX_QUEUED_BYTES: usize = 8 * 1024 * 1024;
/// 进程退出时的有界 flush 时间。
pub(super) const EXIT_FLUSH: Duration = Duration::from_secs(5);
/// 保留的历史运行标识上限；只用于按真实 run UUID 查找身份，不伪造。
const MAX_CORRELATED_RUNS: usize = 128;
/// 等待准入的早期准备记录上限（拒绝后仍可能停留到结果/运行结束清理）。
const MAX_PREPARED_TOOLS: usize = 64;

/// 单条出站事件的有界关联字段。
///
/// 全部来自真实运行观察（`RunObservation`）或真实会话/审批绑定；引擎未提供
/// 的一律为 `None`，封套中据此写成 `null`，绝不编造。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct CorrelationFields {
    pub(super) session_id: Option<String>,
    pub(super) run_id: Option<Uuid>,
    pub(super) turn_id: Option<u64>,
    pub(super) tool_call_id: Option<String>,
    pub(super) subagent_id: Option<String>,
    pub(super) parent_run_id: Option<Uuid>,
}

/// 一次真实运行的关联身份。
#[derive(Clone, Debug)]
struct RunIdentity {
    run_id: Uuid,
    subagent_id: Option<String>,
    parent_run_id: Option<Uuid>,
    turn_id: Option<u64>,
    finished: bool,
}

/// 已准备但尚未确认真正派发的模型工具调用（准入/审批之前）。
///
/// 它只用于早期关联（例如审批仍在等待时补 turn/toolCallId）；拒绝的调用会
/// 从这里清理，绝不会冒充已派发的调用。
#[derive(Clone, Debug)]
struct PreparedTool {
    run_id: Uuid,
    turn_id: Option<u64>,
    tool_call_id: Option<String>,
}

/// 从真实观察累积出的关联状态。
#[derive(Default)]
struct CorrelationState {
    session_id: Option<String>,
    /// 最近一次从顶层运行开始的 run id；子代理运行不覆盖它。
    current_run: Option<Uuid>,
    runs: VecDeque<RunIdentity>,
    /// 早期准备记录；只服务审批关联，不参与工具事件的派发配对。
    prepared: VecDeque<PreparedTool>,
    overflowed: bool,
}

impl CorrelationState {
    fn identity(&self, run_id: Uuid) -> Option<&RunIdentity> {
        self.runs.iter().rev().find(|run| run.run_id == run_id)
    }

    fn fields_for(&self, run_id: Uuid) -> CorrelationFields {
        let identity = self.identity(run_id);
        CorrelationFields {
            session_id: self.session_id.clone(),
            run_id: Some(run_id),
            turn_id: identity.and_then(|run| run.turn_id),
            tool_call_id: None,
            subagent_id: identity.and_then(|run| run.subagent_id.clone()),
            parent_run_id: identity.and_then(|run| run.parent_run_id),
        }
    }

    fn current_fields(&self) -> CorrelationFields {
        match self.current_run {
            Some(run_id) => self.fields_for(run_id),
            None => CorrelationFields {
                session_id: self.session_id.clone(),
                ..CorrelationFields::default()
            },
        }
    }

    fn record_run(&mut self, identity: RunIdentity, top_level: bool) {
        if let Some(existing) = self
            .runs
            .iter_mut()
            .find(|run| run.run_id == identity.run_id)
        {
            if identity.subagent_id.is_some() {
                existing.subagent_id = identity.subagent_id.clone();
                existing.parent_run_id = identity.parent_run_id;
            }
            if identity.turn_id.is_some() {
                existing.turn_id = identity.turn_id;
            }
        } else {
            if self.runs.len() >= MAX_CORRELATED_RUNS {
                if let Some(index) = self.runs.iter().position(|run| run.finished) {
                    self.runs.remove(index);
                } else {
                    self.overflowed = true;
                    return;
                }
            }
            self.runs.push_back(identity.clone());
        }
        if top_level {
            self.current_run = Some(identity.run_id);
        }
    }

    fn observe(&mut self, observation: &RunObservation) {
        let top_level = observation.subagent_id.is_none();
        let identity = || RunIdentity {
            run_id: observation.run_id,
            subagent_id: observation.subagent_id.clone(),
            parent_run_id: observation.parent_run_id,
            turn_id: observation.turn_id,
            finished: false,
        };
        match observation.kind {
            RunObservationKind::RunStarted => {
                if observation.session_id.is_some() {
                    self.session_id = observation.session_id.clone();
                }
                self.record_run(identity(), top_level);
            }
            // 早期准备只为身份/轮次与审批关联服务，绝不登记为待消费派发。
            RunObservationKind::ModelPrepared => {
                self.record_run(identity(), top_level);
            }
            RunObservationKind::ToolPrepared => {
                self.record_run(identity(), top_level);
                if top_level {
                    if self.prepared.len() >= MAX_PREPARED_TOOLS {
                        self.prepared.pop_front();
                    }
                    self.prepared.push_back(PreparedTool {
                        run_id: observation.run_id,
                        turn_id: observation.turn_id,
                        tool_call_id: observation.tool_call_id.clone(),
                    });
                }
            }
            RunObservationKind::ModelDispatched | RunObservationKind::ToolDispatched => {
                self.record_run(identity(), top_level);
                // 真正派发意味着早期准备已兑现，清掉它的准备记录。工具事件本身
                // 不再入队等待配对：事件身份来自执行任务上真实绑定的
                // `ToolIdentity`，与派发顺序无关。
                if top_level {
                    self.remove_prepared(observation.tool_call_id.as_deref());
                }
            }
            RunObservationKind::ToolOutcome => {
                if top_level {
                    // 拒绝的调用从未进入任何配对队列，只清理它的准备记录，
                    // 绝不弹出无关调用。
                    self.remove_prepared(observation.tool_call_id.as_deref());
                }
            }
            RunObservationKind::RunFinished => {
                if let Some(run) = self
                    .runs
                    .iter_mut()
                    .find(|run| run.run_id == observation.run_id)
                {
                    run.finished = true;
                }
                if top_level {
                    self.prepared.clear();
                }
            }
        }
    }

    fn remove_prepared(&mut self, tool_call_id: Option<&str>) {
        let Some(id) = tool_call_id else {
            return;
        };
        if let Some(index) = self
            .prepared
            .iter()
            .position(|prepared| prepared.tool_call_id.as_deref() == Some(id))
        {
            self.prepared.remove(index);
        }
    }

    /// 当前任务真实工具身份对应的关联字段。
    ///
    /// `run_id`/`turn_id`/`tool_call_id` 只来自执行任务上真实绑定的
    /// [`ToolIdentity`]；`subagent_id`/`parent_run_id` 只来自该 run 已观测到的
    /// 委派身份。没有绑定 scope 时不拿队首、不按名字或参数猜，只诚实填 null。
    fn tool_identity_fields(&self, identity: Option<&ToolIdentity>) -> CorrelationFields {
        let Some(identity) = identity else {
            return CorrelationFields {
                session_id: self.session_id.clone(),
                ..CorrelationFields::default()
            };
        };
        let run_id = identity.run_id();
        let observed = self.identity(run_id);
        CorrelationFields {
            session_id: self.session_id.clone(),
            run_id: Some(run_id),
            turn_id: identity.turn_id(),
            tool_call_id: Some(identity.tool_call_id().to_owned()),
            subagent_id: observed.and_then(|run| run.subagent_id.clone()),
            parent_run_id: observed.and_then(|run| run.parent_run_id),
        }
    }

    /// 审批绑定自己的 `scope` 就是真实 run UUID；用它的身份补 turn，
    /// 并用绑定携带的模型 toolCallId（独立于审批尝试的 callId）。
    ///
    /// 绑定本身已携带审批对象；只有在绑定缺失字段时，才回退到该 run 最近的
    /// 早期准备记录。准备记录不是派发，只用于把审批对回真实 turn/toolCallId。
    fn approval_fields(&self, scope: &str, tool_call_id: Option<&str>) -> CorrelationFields {
        let run_id = Uuid::parse_str(scope).ok();
        let mut fields = match run_id {
            Some(run_id) => self.fields_for(run_id),
            None => self.current_fields(),
        };
        if let Some(run_id) = run_id
            && let Some(prepared) = self
                .prepared
                .iter()
                .rev()
                .find(|prepared| prepared.run_id == run_id)
        {
            if fields.turn_id.is_none() {
                fields.turn_id = prepared.turn_id;
            }
            if tool_call_id.is_none() {
                fields.tool_call_id = prepared.tool_call_id.clone();
            }
        }
        if let Some(tool_call_id) = tool_call_id {
            fields.tool_call_id = Some(tool_call_id.to_owned());
        }
        fields
    }
}

/// 事件封套家族。`Legacy` 保持既有输出字段不变；`V1` 追加关联 ID 字段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Envelope {
    Legacy,
    V1,
}

impl Envelope {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Envelope::Legacy => "legacy",
            Envelope::V1 => "v1",
        }
    }
}

/// 由 `initialize` 协商出的协议状态。默认是 legacy，未协商。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ProtocolState {
    pub(super) initialized: bool,
    pub(super) rejected: bool,
    pub(super) envelope: Envelope,
    pub(super) schema_version: u64,
}

impl Default for ProtocolState {
    fn default() -> Self {
        Self {
            initialized: false,
            rejected: false,
            envelope: Envelope::Legacy,
            schema_version: RPC_SCHEMA_VERSION,
        }
    }
}

/// 退出时返回的传输状态。
#[derive(Clone, Debug, Default)]
pub(super) struct OutputStatus {
    pub(super) failure: Option<String>,
    pub(super) dropped: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    /// 流式增量：队列满时丢弃并计数，不阻塞、不 fail-closed。
    Lossy,
    /// 结构化状态事件：绝不静默丢弃，无法入队（没有可驱逐的流式增量）则 fail-closed。
    Durable,
    /// 结束/错误/审批等关键事件：与 [`Class::Durable`] 一样不丢弃、不等待消费者，
    /// 但语义上必须尽快送达；无法入队时立即 fail-closed。
    Critical,
}

struct Queued {
    bytes: Vec<u8>,
    lossy: bool,
}

struct WriterState {
    queue: VecDeque<Queued>,
    queued_bytes: usize,
    dropped: u64,
    shutdown: bool,
    writer_done: bool,
    failure: Option<String>,
}

impl WriterState {
    fn fits(&self, len: usize) -> bool {
        self.queue.len() < EVENT_QUEUE_CAPACITY
            && self.queued_bytes.saturating_add(len) <= MAX_QUEUED_BYTES
    }

    fn push(&mut self, bytes: Vec<u8>, lossy: bool) {
        self.queued_bytes = self.queued_bytes.saturating_add(bytes.len());
        self.queue.push_back(Queued { bytes, lossy });
    }

    /// 丢弃一个已排队的流式增量并计数；没有可丢弃项时返回 false。
    ///
    /// 只驱逐 lossy 条目，绝不丢弃结构化事件。
    fn evict_lossy(&mut self) -> bool {
        let Some(index) = self.queue.iter().rposition(|item| item.lossy) else {
            return false;
        };
        if let Some(item) = self.queue.remove(index) {
            self.queued_bytes = self.queued_bytes.saturating_sub(item.bytes.len());
            self.dropped = self.dropped.saturating_add(1);
        }
        true
    }

    fn empty() -> Self {
        Self {
            queue: VecDeque::new(),
            queued_bytes: 0,
            dropped: 0,
            shutdown: false,
            writer_done: false,
            failure: None,
        }
    }
}

struct Inner {
    state: Mutex<WriterState>,
    /// writer 完成 / shutdown 状态变化；退出时的有界 flush 等待它。
    state_changed: Condvar,
    /// 队列非空 / shutdown 请求；writer 线程等待它。
    data_ready: Condvar,
    notify: tokio::sync::Notify,
    failed: AtomicBool,
    protocol: Mutex<ProtocolState>,
    correlation: Mutex<CorrelationState>,
    writer: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// 可克隆的 JSONL 输出句柄。所有写操作都经由一个独占 stdout 的 writer 线程。
#[derive(Clone)]
pub(super) struct JsonOutput {
    inner: Arc<Inner>,
}

impl Default for JsonOutput {
    fn default() -> Self {
        Self::new()
    }
}

impl JsonOutput {
    pub(super) fn new() -> Self {
        let inner = new_inner();
        let writer_inner = Arc::clone(&inner);
        match std::thread::Builder::new()
            .name("mio-json-output".into())
            .spawn(move || writer_loop(writer_inner))
        {
            Ok(handle) => *inner.writer.lock().unwrap() = Some(handle),
            Err(error) => {
                // writer 线程无法启动就没有消费者：不能悄悄假装事件会被写出。
                // 直接标记 fail-closed，让 emit 立刻报错，wait_failure 立刻完成。
                let message = format!("failed to start JSON output writer thread: {error}");
                {
                    let mut guard = inner.state.lock().unwrap();
                    guard.shutdown = true;
                    guard.writer_done = true;
                    guard.failure = Some(message);
                }
                inner.failed.store(true, Ordering::SeqCst);
                inner.notify.notify_waiters();
                inner.state_changed.notify_all();
                inner.data_ready.notify_all();
            }
        }
        Self { inner }
    }

    /// 仅用于单元测试：不启动 writer，便于在不依赖真实 stdout 的情况下验证有界
    /// 队列与 fail-closed 语义。
    #[cfg(test)]
    fn stalled() -> Self {
        Self { inner: new_inner() }
    }

    /// 结构化状态事件；入队失败 fail-closed。
    pub(super) fn emit(&self, value: Value) -> Result<(), String> {
        self.send(value, Class::Durable).map(|_| ())
    }

    /// 结束/错误/审批等关键事件；不等待消费者，无法入队时立即 fail-closed。
    pub(super) fn emit_critical(&self, value: Value) -> Result<(), String> {
        self.send(value, Class::Critical).map(|_| ())
    }

    /// 流式增量；返回是否真正入队（false 表示已按背压策略丢弃并计数）。
    #[cfg(test)]
    pub(super) fn emit_lossy(&self, value: Value) -> bool {
        matches!(self.send(value, Class::Lossy), Ok(true))
    }

    /// 携带显式关联字段的结构化事件。
    pub(super) fn emit_correlated(
        &self,
        value: Value,
        correlation: &CorrelationFields,
    ) -> Result<(), String> {
        self.send_with(value, Class::Durable, correlation)
            .map(|_| ())
    }

    /// 携带显式关联字段的关键事件。
    pub(super) fn emit_critical_correlated(
        &self,
        value: Value,
        correlation: &CorrelationFields,
    ) -> Result<(), String> {
        self.send_with(value, Class::Critical, correlation)
            .map(|_| ())
    }

    /// 携带显式关联字段的流式增量。
    pub(super) fn emit_lossy_correlated(
        &self,
        value: Value,
        correlation: &CorrelationFields,
    ) -> bool {
        matches!(self.send_with(value, Class::Lossy, correlation), Ok(true))
    }

    pub(super) fn protocol_state(&self) -> ProtocolState {
        self.inner.protocol.lock().unwrap().clone()
    }

    pub(super) fn set_protocol(&self, state: ProtocolState) {
        *self.inner.protocol.lock().unwrap() = state;
    }

    pub(super) fn set_session_id(&self, session_id: Option<String>) {
        self.inner.correlation.lock().unwrap().session_id = session_id;
    }

    pub(super) fn session_id(&self) -> Option<String> {
        self.inner.correlation.lock().unwrap().session_id.clone()
    }

    /// 记录一次真实运行观察；只从这些真实来源累积关联身份。
    pub(super) fn observe(&self, observation: &RunObservation) {
        let overflowed = {
            let mut state = self
                .inner
                .correlation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.observe(observation);
            state.overflowed
        };
        if overflowed {
            self.fail_closed("too many active runs to retain event correlation");
        }
    }

    /// 最近一次顶层运行的关联字段。
    pub(super) fn correlation(&self) -> CorrelationFields {
        self.inner
            .correlation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .current_fields()
    }

    /// 指定真实 run UUID 的关联字段；未知 run 只填 run id，不编造身份。
    pub(super) fn correlation_for_run(&self, run_id: Uuid) -> CorrelationFields {
        self.inner
            .correlation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .fields_for(run_id)
    }

    /// 审批绑定的关联字段：`scope` 就是真实 run UUID，`tool_call_id` 来自绑定。
    pub(super) fn approval_correlation(
        &self,
        scope: &str,
        tool_call_id: Option<&str>,
    ) -> CorrelationFields {
        self.inner
            .correlation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .approval_fields(scope, tool_call_id)
    }

    /// 当前任务真实工具身份对应的关联字段。
    ///
    /// 身份由执行任务上的 `ToolIdentity` 提供，不窥视、不弹出任何队列；没有
    /// 真实身份时 run/turn/toolCallId/subagent/parent 一律为 `None`（诚实 null）。
    pub(super) fn correlation_for_tool_identity(
        &self,
        identity: Option<&ToolIdentity>,
    ) -> CorrelationFields {
        self.inner
            .correlation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tool_identity_fields(identity)
    }

    pub(super) fn dropped_count(&self) -> u64 {
        self.inner.state.lock().unwrap().dropped
    }

    pub(super) fn failure(&self) -> Option<String> {
        self.inner.state.lock().unwrap().failure.clone()
    }

    pub(super) fn is_failed(&self) -> bool {
        self.inner.failed.load(Ordering::SeqCst)
    }

    /// 等待传输 fail-closed，供 `tokio::select!` 取消活动工作。
    ///
    /// 先注册等待再复查标志，避免与 `notify_waiters` 竞争造成丢失唤醒；因为
    /// `notify_waiters` 会唤醒所有已注册的等待者，多个并发等待者都能观察到失败。
    pub(super) async fn wait_failure(&self) {
        if self.is_failed() {
            return;
        }
        let notified = self.inner.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_failed() {
            return;
        }
        notified.await;
    }

    /// 有界 flush 并尽量回收 writer 线程。stdout 阻塞时不为它永久等待。
    pub(super) fn shutdown(&self, timeout: Duration) -> OutputStatus {
        let deadline = Instant::now() + timeout;
        {
            let mut guard = self.inner.state.lock().unwrap();
            guard.shutdown = true;
        }
        self.inner.data_ready.notify_all();
        {
            let mut guard = self.inner.state.lock().unwrap();
            while !guard.writer_done {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                let (next, _) = self
                    .inner
                    .state_changed
                    .wait_timeout(guard, deadline - now)
                    .unwrap();
                guard = next;
            }
        }
        let done = self.inner.state.lock().unwrap().writer_done;
        let handle = self.inner.writer.lock().unwrap().take();
        if let Some(handle) = handle
            && done
        {
            let _ = handle.join();
        }
        // 若 writer 仍阻塞在 stdout，这里直接放弃 join（detach）：进程退出即可，
        // 不允许坏 stdout 让退出永久挂起。但未 flush 的事件必须显式报告。
        if !done {
            let mut guard = self.inner.state.lock().unwrap();
            if guard.failure.is_none() {
                guard.failure = Some(format!(
                    "stdout did not drain within {timeout:?}; {} queued events were not delivered",
                    guard.queue.len()
                ));
            }
            drop(guard);
            self.inner.failed.store(true, Ordering::SeqCst);
            // 与 fail_closed 一致：取消等待者也一并唤醒，不能只置标志。
            self.inner.notify.notify_waiters();
        }
        self.status()
    }

    pub(super) fn status(&self) -> OutputStatus {
        let guard = self.inner.state.lock().unwrap();
        OutputStatus {
            failure: guard.failure.clone(),
            dropped: guard.dropped,
        }
    }

    fn record_drop(&self) {
        let mut guard = self.inner.state.lock().unwrap();
        guard.dropped = guard.dropped.saturating_add(1);
    }

    fn fail_closed(&self, message: &str) {
        {
            let mut guard = self.inner.state.lock().unwrap();
            if guard.failure.is_none() {
                guard.failure = Some(message.to_string());
            }
        }
        self.inner.failed.store(true, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
        self.inner.state_changed.notify_all();
        self.inner.data_ready.notify_all();
    }

    fn send(&self, value: Value, class: Class) -> Result<bool, String> {
        let correlation = self.correlation();
        self.send_with(value, class, &correlation)
    }

    fn send_with(
        &self,
        value: Value,
        class: Class,
        correlation: &CorrelationFields,
    ) -> Result<bool, String> {
        let dropped_before = self.dropped_count();
        let state = self.protocol_state();
        let value = prepare(
            value,
            state.envelope,
            correlation,
            dropped_before,
            class != Class::Lossy,
        );
        let bytes = serde_json::to_vec(&value)
            .map_err(|error| format!("failed to serialize JSON output event: {error}"))?;
        if bytes.len() > MAX_EVENT_BYTES {
            if class == Class::Lossy {
                self.record_drop();
                return Ok(false);
            }
            let message = format!(
                "JSON output event is {} bytes, exceeding the {} byte limit",
                bytes.len(),
                MAX_EVENT_BYTES
            );
            self.fail_closed(&message);
            return Err(message);
        }
        self.enqueue(value, bytes, class)
    }

    /// 单次入队，绝不等待消费者。
    ///
    /// - `Lossy`：队列满时丢弃并计数，返回 `Ok(false)`。
    /// - `Durable` / `Critical`：先驱逐已排队的流式增量；仍装不下就立即 fail-closed。
    ///
    /// 因为不做任何等待，同步 `Host` 回调不会被慢消费者拖住，`abort` 始终可响应。
    fn enqueue(&self, mut value: Value, mut bytes: Vec<u8>, class: Class) -> Result<bool, String> {
        let mut guard = self.inner.state.lock().unwrap();
        if let Some(failure) = guard.failure.clone() {
            return Err(failure);
        }
        if guard.writer_done {
            return Err("JSON output transport is closed".to_string());
        }
        if class == Class::Lossy {
            if guard.fits(bytes.len()) {
                guard.push(bytes, true);
                drop(guard);
                self.inner.data_ready.notify_one();
                return Ok(true);
            }
            guard.dropped = guard.dropped.saturating_add(1);
            return Ok(false);
        }
        loop {
            // Include losses caused by making room for this very event, not
            // just the count observed before enqueue. Re-check the byte bound
            // because adding the counter can itself require another eviction.
            if guard.dropped > 0
                && value.get("droppedEvents").and_then(Value::as_u64) != Some(guard.dropped)
            {
                value["droppedEvents"] = json!(guard.dropped);
                bytes = serde_json::to_vec(&value)
                    .expect("adding an integer to an already serialized JSON value");
            }
            if bytes.len() <= MAX_EVENT_BYTES && guard.fits(bytes.len()) {
                break;
            }
            if bytes.len() > MAX_EVENT_BYTES || !guard.evict_lossy() {
                let message = if bytes.len() > MAX_EVENT_BYTES {
                    "JSON output event exceeds the byte limit after loss accounting"
                } else {
                    "JSON output queue is full; refusing to drop a structured event"
                }
                .to_string();
                guard.failure = Some(message.clone());
                drop(guard);
                self.fail_closed(&message);
                return Err(message);
            }
        }
        guard.push(bytes, false);
        drop(guard);
        self.inner.data_ready.notify_one();
        Ok(true)
    }
}

fn new_inner() -> Arc<Inner> {
    Arc::new(Inner {
        state: Mutex::new(WriterState::empty()),
        state_changed: Condvar::new(),
        data_ready: Condvar::new(),
        notify: tokio::sync::Notify::new(),
        failed: AtomicBool::new(false),
        protocol: Mutex::new(ProtocolState::default()),
        correlation: Mutex::new(CorrelationState::default()),
        writer: Mutex::new(None),
    })
}

fn writer_loop(inner: Arc<Inner>) {
    let stdout = std::io::stdout();
    loop {
        let bytes = {
            let mut guard = inner.state.lock().unwrap();
            loop {
                if let Some(item) = guard.queue.pop_front() {
                    guard.queued_bytes = guard.queued_bytes.saturating_sub(item.bytes.len());
                    break item.bytes;
                }
                if guard.shutdown {
                    guard.writer_done = true;
                    drop(guard);
                    inner.state_changed.notify_all();
                    return;
                }
                guard = inner.data_ready.wait(guard).unwrap();
            }
        };
        let mut line = bytes;
        line.push(b'\n');
        let mut handle = stdout.lock();
        let result = handle.write_all(&line).and_then(|()| handle.flush());
        drop(handle);
        if let Err(error) = result {
            let message = format!("stdout write failed: {error}");
            let mut guard = inner.state.lock().unwrap();
            guard.queue.clear();
            guard.queued_bytes = 0;
            guard.writer_done = true;
            if guard.failure.is_none() {
                guard.failure = Some(message);
            }
            drop(guard);
            inner.failed.store(true, Ordering::SeqCst);
            inner.notify.notify_waiters();
            inner.state_changed.notify_all();
            inner.data_ready.notify_all();
            return;
        }
    }
}

/// 统一的出站封套：schemaVersion、可选的丢弃计数与 v1 关联 ID。
///
/// v1 关联字段全部来自真实运行观察或真实会话/审批绑定；引擎未提供的一律写成
/// `null`，绝不编造。事件自带的同名字段不被覆盖（legacy 字段语义不变）。
pub(super) fn prepare(
    value: Value,
    envelope: Envelope,
    correlation: &CorrelationFields,
    dropped: u64,
    durable: bool,
) -> Value {
    let mut object = match value {
        Value::Object(map) => map,
        other => {
            let mut map = Map::new();
            map.insert("value".to_string(), other);
            map
        }
    };
    object.insert("schemaVersion".to_string(), json!(RPC_SCHEMA_VERSION));
    if durable && dropped > 0 {
        object.insert("droppedEvents".to_string(), json!(dropped));
    }
    if envelope == Envelope::V1 {
        // 不覆盖事件自身字段；未知的一律诚实填 null，不编造关联。
        let correlation = [
            (
                "sessionId",
                correlation
                    .session_id
                    .as_ref()
                    .map_or(Value::Null, |id| json!(id)),
            ),
            (
                "runId",
                correlation
                    .run_id
                    .map_or(Value::Null, |id| json!(id.to_string())),
            ),
            (
                "turnId",
                correlation.turn_id.map_or(Value::Null, |turn| json!(turn)),
            ),
            (
                "toolCallId",
                correlation
                    .tool_call_id
                    .as_ref()
                    .map_or(Value::Null, |id| json!(id)),
            ),
            (
                "subagentId",
                correlation
                    .subagent_id
                    .as_ref()
                    .map_or(Value::Null, |id| json!(id)),
            ),
            (
                "parentRunId",
                correlation
                    .parent_run_id
                    .map_or(Value::Null, |id| json!(id.to_string())),
            ),
        ];
        for (key, value) in correlation {
            if !object.contains_key(key) {
                object.insert(key.to_string(), value);
            }
        }
    }
    Value::Object(object)
}

/// 无传输句柄时的回退（纯文本模式与单元测试）：仍打印 JSON 行。
pub(super) fn emit_or_print(
    output: &Option<JsonOutput>,
    value: Value,
    critical: bool,
) -> Result<(), String> {
    match output {
        Some(output) => {
            if critical {
                output.emit_critical(value)
            } else {
                output.emit(value)
            }
        }
        None => {
            println!(
                "{}",
                prepare(
                    value,
                    Envelope::Legacy,
                    &CorrelationFields::default(),
                    0,
                    true
                )
            );
            Ok(())
        }
    }
}

/// [`emit_or_print`] 的显式关联版本：把真实观察到的关联 ID 附到该条事件上。
pub(super) fn emit_or_print_correlated(
    output: &Option<JsonOutput>,
    value: Value,
    critical: bool,
    correlation: &CorrelationFields,
) -> Result<(), String> {
    match output {
        Some(output) => {
            if critical {
                output.emit_critical_correlated(value, correlation)
            } else {
                output.emit_correlated(value, correlation)
            }
        }
        None => {
            println!("{}", prepare(value, Envelope::Legacy, correlation, 0, true));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mio_coding_agent::run_observation::RunObservationStatus;

    fn observation(kind: RunObservationKind, run_id: Uuid) -> RunObservation {
        RunObservation {
            run_id,
            kind,
            session_id: None,
            subagent_id: None,
            parent_run_id: None,
            turn_id: None,
            tool_call_id: None,
            effect_id: None,
            status: None,
        }
    }

    /// 工具事件的身份来自执行任务上真实绑定的 `ToolIdentity`：run/turn/callId
    /// 逐项来自绑定，subagent/parent 来自该 run 已观测到的委派身份。
    #[test]
    fn tool_identity_lookup_carries_real_ids_and_observed_delegation() {
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        let mut state = CorrelationState::default();
        let mut started = observation(RunObservationKind::RunStarted, parent);
        started.session_id = Some("sess".into());
        state.observe(&started);

        let mut child_started = observation(RunObservationKind::RunStarted, child);
        child_started.subagent_id = Some("child-run".into());
        child_started.parent_run_id = Some(parent);
        state.observe(&child_started);

        let identity = ToolIdentity::new(parent, "call-a", 2, 7);
        let fields = state.tool_identity_fields(Some(&identity));
        assert_eq!(fields.session_id.as_deref(), Some("sess"));
        assert_eq!(fields.run_id, Some(parent));
        assert_eq!(fields.turn_id, Some(2));
        assert_eq!(fields.tool_call_id.as_deref(), Some("call-a"));
        assert!(fields.subagent_id.is_none());
        assert!(fields.parent_run_id.is_none());

        let child_identity = ToolIdentity::new(child, "call-b", 5, 11);
        let child_fields = state.tool_identity_fields(Some(&child_identity));
        assert_eq!(child_fields.run_id, Some(child));
        assert_eq!(child_fields.turn_id, Some(5));
        assert_eq!(child_fields.tool_call_id.as_deref(), Some("call-b"));
        assert_eq!(child_fields.subagent_id.as_deref(), Some("child-run"));
        assert_eq!(child_fields.parent_run_id, Some(parent));
    }

    #[test]
    fn completed_run_churn_does_not_evict_active_child_identity() {
        let mut state = CorrelationState::default();
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        let mut started = observation(RunObservationKind::RunStarted, child);
        started.subagent_id = Some("long-running-child".into());
        started.parent_run_id = Some(parent);
        state.observe(&started);
        for _ in 0..MAX_CORRELATED_RUNS * 2 {
            let run_id = Uuid::new_v4();
            state.observe(&observation(RunObservationKind::RunStarted, run_id));
            state.observe(&observation(RunObservationKind::RunFinished, run_id));
        }
        let identity = ToolIdentity::new(child, "late-result", 3, 9);
        let fields = state.tool_identity_fields(Some(&identity));
        assert_eq!(fields.subagent_id.as_deref(), Some("long-running-child"));
        assert_eq!(fields.parent_run_id, Some(parent));
        assert_eq!(state.runs.len(), MAX_CORRELATED_RUNS);
        assert!(!state.overflowed);
    }

    #[test]
    fn active_identity_capacity_exhaustion_is_explicit_without_eviction() {
        let mut state = CorrelationState::default();
        let first = Uuid::new_v4();
        state.observe(&observation(RunObservationKind::RunStarted, first));
        for _ in 1..MAX_CORRELATED_RUNS {
            state.observe(&observation(RunObservationKind::RunStarted, Uuid::new_v4()));
        }
        state.observe(&observation(RunObservationKind::RunStarted, Uuid::new_v4()));
        assert!(state.overflowed);
        assert_eq!(state.runs.len(), MAX_CORRELATED_RUNS);
        assert!(state.identity(first).is_some());
    }

    #[test]
    fn active_identity_overflow_fails_the_transport() {
        let output = JsonOutput::stalled();
        for _ in 0..=MAX_CORRELATED_RUNS {
            output.observe(&observation(RunObservationKind::RunStarted, Uuid::new_v4()));
        }
        assert!(output.is_failed());
        assert_eq!(
            output.failure().as_deref(),
            Some("too many active runs to retain event correlation")
        );
        assert!(output.emit(json!({"type":"done"})).is_err());
    }

    /// 没有真实 scope 时不得造 ID：run/turn/callId/subagent/parent 全部诚实 null。
    #[test]
    fn a_missing_identity_is_reported_as_honest_null() {
        let mut state = CorrelationState::default();
        let mut started = observation(RunObservationKind::RunStarted, Uuid::new_v4());
        started.session_id = Some("sess".into());
        state.observe(&started);

        let fields = state.tool_identity_fields(None);
        assert_eq!(fields.session_id.as_deref(), Some("sess"));
        assert!(fields.run_id.is_none());
        assert!(fields.turn_id.is_none());
        assert!(fields.tool_call_id.is_none());
        assert!(fields.subagent_id.is_none());
        assert!(fields.parent_run_id.is_none());
    }

    /// 拒绝只清掉自己的准备记录，不弹出无关调用；工具事件不再依赖任何队列
    /// 配对，因此拒绝不会让别人串号。
    #[test]
    fn a_denial_clears_only_its_own_prepared_record() {
        let run_id = Uuid::new_v4();
        let mut state = CorrelationState::default();
        state.observe(&observation(RunObservationKind::RunStarted, run_id));

        let mut allowed = observation(RunObservationKind::ToolPrepared, run_id);
        allowed.turn_id = Some(1);
        allowed.tool_call_id = Some("call-allowed".into());
        state.observe(&allowed);

        let mut refused = observation(RunObservationKind::ToolPrepared, run_id);
        refused.turn_id = Some(1);
        refused.tool_call_id = Some("call-refused".into());
        state.observe(&refused);

        // 被拒绝的调用只有 Outcome，没有 Dispatched。
        let mut denied = observation(RunObservationKind::ToolOutcome, run_id);
        denied.turn_id = Some(1);
        denied.tool_call_id = Some("call-refused".into());
        denied.status = Some(RunObservationStatus::Error);
        state.observe(&denied);

        assert_eq!(
            state.prepared.len(),
            1,
            "the unrelated prepared call remains"
        );
        assert_eq!(
            state.prepared.front().unwrap().tool_call_id.as_deref(),
            Some("call-allowed")
        );
    }

    /// 审批仍能从该 run 的真实准备记录得到 turn/toolCallId；真正派发后清理。
    #[test]
    fn approval_uses_the_prepared_call_and_a_dispatch_claims_it() {
        let run_id = Uuid::new_v4();
        let mut state = CorrelationState::default();
        state.observe(&observation(RunObservationKind::RunStarted, run_id));

        let mut prepared = observation(RunObservationKind::ToolPrepared, run_id);
        prepared.turn_id = Some(2);
        prepared.tool_call_id = Some("call-write".into());
        state.observe(&prepared);

        let fields = state.approval_fields(&run_id.to_string(), None);
        assert_eq!(fields.run_id, Some(run_id));
        assert_eq!(fields.turn_id, Some(2));
        assert_eq!(fields.tool_call_id.as_deref(), Some("call-write"));

        // 绑定自带的 toolCallId 优先，不被准备记录覆盖。
        let bound = state.approval_fields(&run_id.to_string(), Some("call-write"));
        assert_eq!(bound.tool_call_id.as_deref(), Some("call-write"));

        let mut dispatched = observation(RunObservationKind::ToolDispatched, run_id);
        dispatched.turn_id = Some(2);
        dispatched.tool_call_id = Some("call-write".into());
        state.observe(&dispatched);
        assert!(state.prepared.is_empty());
    }

    #[test]
    fn envelope_adds_schema_version_and_honest_null_ids() {
        let legacy = prepare(
            json!({"type": "text"}),
            Envelope::Legacy,
            &CorrelationFields {
                session_id: Some("s1".into()),
                ..CorrelationFields::default()
            },
            0,
            false,
        );
        assert_eq!(legacy["schemaVersion"], 1);
        assert!(legacy.get("sessionId").is_none());

        let v1 = prepare(
            json!({"type": "text"}),
            Envelope::V1,
            &CorrelationFields {
                session_id: Some("s1".into()),
                ..CorrelationFields::default()
            },
            3,
            true,
        );
        assert_eq!(v1["sessionId"], "s1");
        assert!(v1["runId"].is_null());
        assert!(v1["turnId"].is_null());
        assert!(v1["toolCallId"].is_null());
        assert!(v1["subagentId"].is_null());
        assert!(v1["parentRunId"].is_null());
        assert_eq!(v1["droppedEvents"], 3);
    }

    #[test]
    fn v1_envelope_carries_real_correlation_values() {
        let run_id = Uuid::new_v4();
        let parent = Uuid::new_v4();
        let v1 = prepare(
            json!({"type": "tool_finished"}),
            Envelope::V1,
            &CorrelationFields {
                session_id: Some("sess".into()),
                run_id: Some(run_id),
                turn_id: Some(2),
                tool_call_id: Some("call-a".into()),
                subagent_id: Some("child-run".into()),
                parent_run_id: Some(parent),
            },
            0,
            false,
        );
        assert_eq!(v1["runId"], json!(run_id.to_string()));
        assert_eq!(v1["turnId"], 2);
        assert_eq!(v1["toolCallId"], "call-a");
        assert_eq!(v1["subagentId"], "child-run");
        assert_eq!(v1["parentRunId"], json!(parent.to_string()));
    }

    #[test]
    fn existing_fields_are_not_overwritten_by_correlation() {
        let v1 = prepare(
            json!({"type": "response", "sessionId": "real"}),
            Envelope::V1,
            &CorrelationFields {
                session_id: Some("other".into()),
                ..CorrelationFields::default()
            },
            0,
            false,
        );
        assert_eq!(v1["sessionId"], "real");
    }

    #[test]
    fn writer_state_evicts_lossy_before_durable() {
        let mut state = WriterState::empty();
        state.push(b"stream".to_vec(), true);
        assert!(state.evict_lossy());
        assert_eq!(state.dropped, 1);
        assert!(state.queue.is_empty());
        assert!(!state.evict_lossy());
    }

    #[test]
    fn full_queue_fails_closed_instead_of_dropping_structured_events() {
        let output = JsonOutput::stalled();
        for _ in 0..EVENT_QUEUE_CAPACITY {
            output.emit(json!({"type": "tool_started"})).unwrap();
        }
        let error = output.emit(json!({"type": "tool_finished"})).unwrap_err();
        assert!(error.contains("queue is full"), "{error}");
        assert!(output.is_failed());
        // 一旦 fail-closed，后续事件不会被假装成功。
        assert!(output.emit(json!({"type": "agent_settled"})).is_err());
        let status = output.status();
        assert_eq!(status.dropped, 0);
        assert!(status.failure.is_some());
    }

    #[test]
    fn lossy_deltas_are_dropped_counted_and_do_not_fail_closed() {
        let output = JsonOutput::stalled();
        for _ in 0..EVENT_QUEUE_CAPACITY {
            assert!(output.emit_lossy(json!({"type": "text", "data": "x"})));
        }
        assert!(!output.emit_lossy(json!({"type": "text", "data": "y"})));
        assert_eq!(output.dropped_count(), 1);
        assert!(!output.is_failed());
        // 结构化事件到来时优先丢弃已排队的流式增量，而不是 fail-closed。
        output.emit(json!({"type": "tool_started"})).unwrap();
        assert!(output.dropped_count() >= 1);
        assert!(!output.is_failed());
    }

    #[test]
    fn oversized_durable_event_fails_closed() {
        let output = JsonOutput::stalled();
        let oversized = "x".repeat(MAX_EVENT_BYTES + 1);
        let error = output
            .emit(json!({"type": "tool_finished", "data": oversized}))
            .unwrap_err();
        assert!(error.contains("exceeding"), "{error}");
        assert!(output.is_failed());
    }

    /// 回归：停止消费的客户端不能把关键事件变成同步阻塞。旧实现在这里等待队列
    /// 空间最多 5 秒；现在必须不等待、立即 fail-closed。
    #[test]
    fn stalled_full_queue_fails_critical_without_waiting_for_the_consumer() {
        let output = JsonOutput::stalled();
        // 用结构化事件填满队列：没有可驱逐的流式增量，等价于完全不读 stdout。
        for _ in 0..EVENT_QUEUE_CAPACITY {
            output.emit(json!({"type": "tool_started"})).unwrap();
        }
        let started = Instant::now();
        let error = output
            .emit_critical(json!({"type": "agent_settled", "status": "cancelled"}))
            .unwrap_err();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "critical emit must not wait for the consumer: {elapsed:?}"
        );
        assert!(error.contains("queue is full"), "{error}");
        assert!(output.is_failed());
        // 关键事件没有被静默丢弃，也没有被伪装成成功。
        assert!(output.status().failure.is_some());
        assert_eq!(output.status().dropped, 0);
        // 一旦 fail-closed，后续关键事件同样立即返回错误，不再阻塞。
        let started = Instant::now();
        assert!(
            output
                .emit_critical(json!({"type": "error", "error": "x"}))
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn critical_event_evicts_lossy_instead_of_failing() {
        let output = JsonOutput::stalled();
        for _ in 0..EVENT_QUEUE_CAPACITY {
            assert!(output.emit_lossy(json!({"type": "text", "data": "x"})));
        }
        let started = Instant::now();
        output
            .emit_critical(json!({"type": "agent_settled", "status": "cancelled"}))
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(output.dropped_count(), 1);
        assert!(!output.is_failed());
        let guard = output.inner.state.lock().unwrap();
        let terminal: Value = serde_json::from_slice(&guard.queue.back().unwrap().bytes).unwrap();
        assert_eq!(terminal["droppedEvents"], guard.dropped);
    }

    #[test]
    fn large_durable_event_evicts_multiple_deltas_and_reports_all_losses() {
        let output = JsonOutput::stalled();
        let delta = json!({"type": "text", "data": "x".repeat(128 * 1024)});
        while output.emit_lossy(delta.clone()) {}
        let before = output.dropped_count();
        output
            .emit(json!({"type": "tool_finished", "data": "x".repeat(MAX_EVENT_BYTES / 2)}))
            .unwrap();
        let guard = output.inner.state.lock().unwrap();
        assert!(guard.dropped >= before + 2);
        assert!(guard.queued_bytes <= MAX_QUEUED_BYTES);
        assert!(guard.queue.len() <= EVENT_QUEUE_CAPACITY);
        let event: Value = serde_json::from_slice(&guard.queue.back().unwrap().bytes).unwrap();
        assert_eq!(event["droppedEvents"], guard.dropped);
        assert!(!output.is_failed());
    }

    /// Notify 丢失唤醒回归：多个并发等待者都必须被 fail-closed 唤醒。
    #[tokio::test]
    async fn fail_closed_wakes_every_waiter() {
        let output = JsonOutput::stalled();
        let waiters: Vec<_> = (0..4)
            .map(|_| {
                let output = output.clone();
                tokio::spawn(async move { output.wait_failure().await })
            })
            .collect();
        // 让所有等待者先完成注册，再制造失败。
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        output.fail_closed("test failure");
        tokio::time::timeout(Duration::from_secs(5), async {
            for waiter in waiters {
                waiter.await.unwrap();
            }
        })
        .await
        .expect("fail-closed must wake every waiter");
    }

    #[tokio::test]
    async fn full_queue_failure_wakes_waiter() {
        let output = JsonOutput::stalled();
        for _ in 0..EVENT_QUEUE_CAPACITY {
            output.emit(json!({"type": "tool_started"})).unwrap();
        }
        let waiter = {
            let output = output.clone();
            tokio::spawn(async move { output.wait_failure().await })
        };
        tokio::task::yield_now().await;
        assert!(output.emit_critical(json!({"type": "done"})).is_err());
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("fail-closed must wake the waiter")
            .unwrap();
    }
}
