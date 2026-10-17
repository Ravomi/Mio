//! Native, in-memory child sessions. Task isolation is not process or OS isolation.
//!
//! Admission is queued instead of refused. Reaching the concurrency limit, an
//! overlapping writer or an active parent workspace operation registers the run
//! as [`Status::Queued`] and lets it start later. Queued runs are not spawned,
//! so they hold no model, heavy-task or workspace permit while waiting.
use crate::task_lifecycle::{
    ActiveLease, ProcessGroupReap, ProviderToolCallId, Settlement, TaskHandle, TaskId,
    TaskIdentity, TaskLifecycleError, TaskLifecycleSink, TaskLifecycleTracker, TaskRecord,
};
use crate::{
    AgentEvent, Config, Host, HostFuture, Message, Provider, RunOptions, RunTools, Services,
};
use futures_util::FutureExt;
use mio_agent::agent::ToolInvocation;
use mio_core::tool::{PortableTool, Tool, ToolContext, ToolExecutionError, ToolOutput};
use mio_core::wasm_compat::WasmCompatSend;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{oneshot, watch};
use uuid::Uuid;

const MAX_RECORDS: usize = 128;
const MAX_OUTPUT: usize = 32 * 1024;
const MAX_HISTORY: usize = 2 * 1024 * 1024;
const MAX_WAIT: Duration = Duration::from_secs(30);
/// Bounded structured execution evidence retained per child run.
const MAX_EVIDENCE: usize = 32;
const MAX_EVIDENCE_TOOL: usize = 128;
const MAX_EVIDENCE_TEXT: usize = 256;
const MAX_EVIDENCE_TARGET: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Registered but not yet admitted: waiting for a slot or the workspace.
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

impl Status {
    /// True while the run still owns its session and must be accounted for.
    /// Hosts cancel active runs uniformly; every terminal state returns false.
    pub fn is_active(self) -> bool {
        matches!(self, Status::Queued | Status::Running)
    }
}

/// Bounded, structured evidence derived only from real `ToolFinished` events.
///
/// `ok` reflects the tool's own reported result and is deliberately not a test
/// verdict; a shell command that exits zero is not labelled as a passing test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolEvidence {
    pub tool: String,
    pub target: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Stable FNV-1a hash of the full, untruncated result text.
    pub result_hash: String,
}

/// A canonical pre-dispatch denial. This records an attempted call, not a
/// completed tool operation or evidence of a side effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeniedToolCall {
    pub call_index: u64,
    pub tool: String,
    pub operation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub run_id: Uuid,
    /// The real run that dispatched this child, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<Uuid>,
    pub session: String,
    pub goal: String,
    pub cwd: PathBuf,
    pub read_only: bool,
    pub status: Status,
    pub output: String,
    pub output_truncated: bool,
    pub last_tool: Option<String>,
    pub error: Option<String>,
    pub evidence: Vec<ToolEvidence>,
    pub evidence_truncated: bool,
    /// Canonical policy denials; separate from real `ToolFinished` evidence.
    pub denied_calls: Vec<DeniedToolCall>,
    pub denied_calls_truncated: bool,
    /// Projected canonical phase from the child's own task tracker, when one is
    /// wired. `None` means unknown; it is never derived from `status`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_phase: Option<String>,
    /// The child's real engine run UUID (distinct from the delegation
    /// `run_id`). `None` until the child engine run attaches it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_run_id: Option<Uuid>,
    /// The real provider tool-call id of the parent dispatch that created this
    /// child. `None` means unknown; it is never derived from the child's own
    /// tool calls or from completion order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_tool_call_id: Option<String>,
}

/// Optional terminal result envelope. `unverified` means the child answer and
/// tool events are evidence for the parent to check, not an acceptance verdict.
#[derive(Debug, Clone, Serialize)]
pub struct SubagentResult {
    pub schema: &'static str,
    pub verification: &'static str,
    #[serde(flatten)]
    pub snapshot: Snapshot,
}

/// Cheap live counters without cloning child output or execution evidence.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Activity {
    pub running: usize,
    pub queued: usize,
    pub limit: usize,
}

#[derive(Clone, Deserialize)]
pub struct Delegate {
    pub session: String,
    pub goal: String,
    pub prompt: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default = "default_read_only")]
    pub read_only: bool,
}
fn default_read_only() -> bool {
    true
}

/// Owned inputs permit host executors and deterministic offline tests.
/// An executor must honor read_only and cancellation by dropping its future.
pub struct Task {
    pub config: Config,
    pub history: Vec<Message>,
    pub prompt: String,
    pub cwd: PathBuf,
    pub host: Arc<dyn Host>,
    pub options: RunOptions,
    pub read_only: bool,
    pub tool_filter: crate::pattern::ToolFilter,
    /// The child's own canonical task, bound into the child engine run so the
    /// child's [`TaskLifecycleTracker`] (never this manager's cached `status`)
    /// is the sole phase writer for the delegated run.
    pub task_handle: Option<TaskHandle>,
}
pub trait Executor: Send + Sync {
    fn execute(&self, task: Task) -> HostFuture<Result<Vec<Message>, String>>;
}
pub struct NativeExecutor;
impl Executor for NativeExecutor {
    fn execute(&self, task: Task) -> HostFuture<Result<Vec<Message>, String>> {
        execute_task(task, None)
    }
}
struct AntigravityExecutor(crate::antigravity::Antigravity);
impl Executor for AntigravityExecutor {
    fn execute(&self, task: Task) -> HostFuture<Result<Vec<Message>, String>> {
        execute_task(task, Some(self.0.clone()))
    }
}
fn execute_task(
    task: Task,
    antigravity: Option<crate::antigravity::Antigravity>,
) -> HostFuture<Result<Vec<Message>, String>> {
    Box::pin(async move {
        let Task {
            config,
            history,
            prompt,
            cwd,
            host,
            mut options,
            read_only,
            tool_filter,
            task_handle,
        } = task;
        // The child engine run must use the manager's canonical task, so its
        // phase transitions (admission, workers, terminal) go through the same
        // tracker instead of creating a second, unrelated task.
        options.task = task_handle;
        crate::run_with_tools(
            config,
            history,
            &prompt,
            &cwd,
            host,
            Services {
                antigravity: antigravity.as_ref(),
                ..Services::default()
            },
            RunTools {
                options,
                read_only,
                subagents: None,
                tool_filter,
                ..RunTools::default()
            },
        )
        .await
    })
}

struct Session {
    goal: String,
    cwd: PathBuf,
    read_only: bool,
    history: Vec<Message>,
    /// Set while a queued or running run owns this session.
    active: Option<Uuid>,
}

/// Identical active/queued request identity used to reuse an existing run id.
#[derive(Clone, PartialEq, Eq)]
struct RequestKey {
    parent_run_id: Option<Uuid>,
    goal: String,
    cwd: PathBuf,
    read_only: bool,
    prompt: String,
    tool_filter: crate::pattern::ToolFilter,
    timeout: Duration,
}

/// Everything needed to start a queued run once admission allows it.
struct PendingRun {
    prompt: String,
    history: Vec<Message>,
    host: Arc<dyn Host>,
    tool_filter: crate::pattern::ToolFilter,
    timeout: Duration,
    cancelled: oneshot::Receiver<()>,
}

struct Record {
    seq: usize,
    snapshot: Snapshot,
    key: RequestKey,
    cancel: Option<oneshot::Sender<()>>,
    changed: watch::Sender<Status>,
    pending: Option<PendingRun>,
}

struct State {
    sessions: BTreeMap<String, Session>,
    runs: BTreeMap<Uuid, Record>,
    next_seq: usize,
    parent_operations: usize,
    concurrency: usize,
    /// Real, owned admission slots. Every started child must hold one of these
    /// `OwnedSemaphorePermit`s, which is the actual owner behind its canonical
    /// `Queued -> Running` evidence.
    slots: Arc<tokio::sync::Semaphore>,
    timeout: Duration,
    config: Config,
    options: RunOptions,
    executor: Arc<dyn Executor>,
    runtime: Option<tokio::runtime::Handle>,
}

/// Captured admission decision for one queued run, moved off the lock before
/// it is spawned so no mutex is held across an await or a task start.
struct Job {
    id: Uuid,
    session: String,
    task: Task,
    cancelled: oneshot::Receiver<()>,
    timeout: Duration,
    /// The canonical child task id (derived from the delegation run id).
    task_id: TaskId,
    /// The real held subagent slot lease that backs this child's `Running`.
    lease: Arc<ActiveLease>,
}

/// The child-side legacy projection.
///
/// The canonical tracker is the sole phase writer; this sink mirrors every
/// committed phase into the manager's cached `Snapshot.status` (the legacy
/// field). It runs on the tracker's persistence path, never holding the
/// tracker's own state lock, and it briefly takes the manager's state lock
/// only for the field update, so a callback that reads a snapshot cannot
/// re-enter the tracker.
struct SubagentCanonicalSink {
    state: Arc<Mutex<State>>,
}

impl TaskLifecycleSink for SubagentCanonicalSink {
    fn on_task_lifecycle(&self, record: &TaskRecord) -> Result<(), String> {
        let Ok(id) = Uuid::parse_str(record.task_id.as_str()) else {
            // A canonical task that is not one of our delegation run ids is not
            // ours to project; it is never guessed into a child record.
            return Ok(());
        };
        let mut state = self.state.lock().unwrap();
        if let Some(entry) = state.runs.get_mut(&id) {
            let status: Status = record.phase.into();
            let changed = entry.snapshot.status != status;
            entry.snapshot.status = status;
            entry.snapshot.canonical_phase = Some(record.phase.as_str().to_owned());
            if let Some(run) = &record.engine_run_id {
                entry.snapshot.engine_run_id = Uuid::parse_str(run.as_str()).ok();
            }
            entry.snapshot.provider_tool_call_id = record
                .provider_tool_call_id
                .as_ref()
                .map(|call| call.as_str().to_owned());
            // Only a real phase change wakes waiters; a same-phase projection
            // (for example attaching the engine UUID) must not be mistaken for
            // settlement by an earlier waiter.
            if changed {
                entry.changed.send_replace(status);
            }
        }
        Ok(())
    }
}

/// Drop cancels outstanding children; successful history lasts for this manager's lifetime.
pub struct SubagentManager {
    root: PathBuf,
    state: Arc<Mutex<State>>,
    /// Monotonic permission ceiling for delegated children.
    ceiling: Arc<Mutex<Option<PermissionCeiling>>>,
    /// The single phase writer for every child task. `Snapshot.status` and
    /// `canonical_phase` are projections of this tracker, never an independent
    /// state machine.
    canonical: Arc<TaskLifecycleTracker>,
}

/// The most permissive tool set and access mode a child may ever receive.
#[derive(Clone)]
struct PermissionCeiling {
    filter: crate::pattern::ToolFilter,
    read_only: bool,
}
impl Drop for SubagentManager {
    fn drop(&mut self) {
        // Deliver cancellation to live runs and to still-queued records. The
        // canonical tracker (not the cached snapshot) remains the sole phase
        // writer: queued records are cancelled through it, so `Snapshot.status`
        // cannot silently diverge from the canonical phase just because the
        // manager is going away. The manager's state lock is released before
        // the canonical callback runs, because that callback re-enters `State`.
        let mut queued = Vec::new();
        let mut running = Vec::new();
        {
            let mut state = self.state.lock().unwrap();
            for (id, record) in state.runs.iter_mut() {
                match record.snapshot.status {
                    Status::Queued => {
                        // Clearing `pending` breaks the record -> host -> state cycle.
                        record.pending = None;
                        record.cancel = None;
                        queued.push(*id);
                    }
                    Status::Running => {
                        if let Some(cancel) = record.cancel.take() {
                            running.push(cancel);
                        }
                    }
                    _ => {}
                }
            }
        }
        for cancel in running {
            let _ = cancel.send(());
        }
        for id in queued {
            if let Ok(task_id) = TaskId::new(id.to_string()) {
                let _ = self.canonical.request_cancel(
                    &task_id,
                    Some("child cancelled before the manager was dropped"),
                );
            }
        }
    }
}
impl SubagentManager {
    pub fn new(root: &Path, config: Config, options: RunOptions) -> Result<Self, String> {
        if !matches!(
            config.provider,
            Provider::Custom
                | Provider::CustomResponses
                | Provider::Deepseek
                | Provider::Anthropic
                | Provider::Google
                | Provider::Ollama
        ) {
            return Err("This provider requires an injected account service; use new_with_antigravity for Antigravity".into());
        }
        Self::with_executor(
            root,
            config,
            options,
            4,
            Duration::from_secs(600),
            Arc::new(NativeExecutor),
        )
    }
    /// Share one account service with the parent; token refresh is serialized by the service.
    pub fn new_with_antigravity(
        root: &Path,
        config: Config,
        options: RunOptions,
        account: crate::antigravity::Antigravity,
    ) -> Result<Self, String> {
        if config.provider != Provider::Antigravity {
            return Err("Antigravity account requires the Antigravity provider".into());
        }
        Self::with_executor(
            root,
            config,
            options,
            4,
            Duration::from_secs(600),
            Arc::new(AntigravityExecutor(account)),
        )
    }
    pub fn with_executor(
        root: &Path,
        config: Config,
        options: RunOptions,
        concurrency: usize,
        timeout: Duration,
        executor: Arc<dyn Executor>,
    ) -> Result<Self, String> {
        let root = root.canonicalize().map_err(|e| e.to_string())?;
        if !root.is_dir() {
            return Err("Invalid subagent workspace, model, limits or instructions".into());
        }
        validate_limits(&config, &options, concurrency, timeout)?;
        let state = Arc::new(Mutex::new(State {
            sessions: BTreeMap::new(),
            runs: BTreeMap::new(),
            next_seq: 0,
            parent_operations: 0,
            concurrency,
            slots: Arc::new(tokio::sync::Semaphore::new(concurrency)),
            timeout,
            config,
            options,
            executor,
            runtime: None,
        }));
        let canonical = TaskLifecycleTracker::with_capacity(
            root.display().to_string(),
            Arc::new(SubagentCanonicalSink {
                state: state.clone(),
            }),
            MAX_RECORDS,
        );
        Ok(Self {
            root,
            state,
            ceiling: Arc::new(Mutex::new(None)),
            canonical,
        })
    }
    /// Configure admission concurrency (1..=16) before any task starts.
    /// Intended for the CLI `--max-subagents` flag; refusing while a task or a
    /// parent operation is active keeps the bound stable for live queues.
    pub fn with_concurrency(self, concurrency: usize) -> Result<Self, String> {
        let mut this = self;
        this.set_concurrency(concurrency)?;
        Ok(this)
    }
    /// In-place equivalent of [`Self::with_concurrency`] that never drops the
    /// manager, so a rejected resize cannot cancel live children.
    pub fn set_concurrency(&mut self, concurrency: usize) -> Result<(), String> {
        if !(1..=16).contains(&concurrency) {
            return Err("Subagent concurrency must be between 1 and 16".into());
        }
        let mut state = self.state.lock().unwrap();
        if state.parent_operations != 0
            || state
                .runs
                .values()
                .any(|run| run.snapshot.status.is_active())
        {
            return Err("Cannot change subagent concurrency while tasks are active".into());
        }
        state.concurrency = concurrency;
        // No run is active, so replacing the slot admission source cannot
        // strand a held permit; new admissions use the new real bound.
        state.slots = Arc::new(tokio::sync::Semaphore::new(concurrency));
        Ok(())
    }
    /// Update host-owned pinned instructions without changing existing task
    /// snapshots. Conflicting changes while children run fail closed.
    pub fn set_pinned_context(
        &self,
        pinned: crate::context_memory::PinnedContext,
    ) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        if state.options.pinned_context == pinned {
            return Ok(());
        }
        if state.parent_operations != 0
            || state
                .runs
                .values()
                .any(|run| run.snapshot.status.is_active())
        {
            return Err("Cannot change pinned context while child tasks are active".into());
        }
        state.options.pinned_context = pinned;
        Ok(())
    }

    /// Change the model for future tasks without discarding child sessions.
    /// Requires exclusive ownership and no active task or parent operation.
    pub fn reconfigure(
        &mut self,
        config: Config,
        options: RunOptions,
        account: Option<crate::antigravity::Antigravity>,
    ) -> Result<(), String> {
        let executor: Arc<dyn Executor> = match (config.provider, account) {
            (Provider::Antigravity, Some(account)) => Arc::new(AntigravityExecutor(account)),
            (
                Provider::Custom
                | Provider::CustomResponses
                | Provider::Deepseek
                | Provider::Anthropic
                | Provider::Google
                | Provider::Ollama,
                _,
            ) => Arc::new(NativeExecutor),
            _ => return Err("Provider requires an available account service".into()),
        };
        let mut state = self.state.lock().unwrap();
        validate_limits(&config, &options, state.concurrency, state.timeout)?;
        if state.parent_operations != 0
            || state
                .runs
                .values()
                .any(|run| run.snapshot.status.is_active())
        {
            return Err("Cannot switch model while child tasks are running".into());
        }
        state.config = config;
        state.options = options;
        state.executor = executor;
        Ok(())
    }

    pub fn root_cwd(&self) -> &Path {
        &self.root
    }

    /// Install the parent run's permission ceiling for delegated children.
    ///
    /// Ceilings compose monotonically: a later call can only narrow the set,
    /// never widen it, so nested delegation can lose permissions but never
    /// gain them. The ceiling lives outside `state`, so it cannot disturb a
    /// live queue.
    pub fn set_permission_ceiling(&self, filter: crate::pattern::ToolFilter, read_only: bool) {
        let mut guard = self.ceiling.lock().unwrap();
        let next = match guard.as_ref() {
            // The parent filter repeats every turn; never nest it again.
            Some(existing) if existing.filter.includes_constraints(&filter) => PermissionCeiling {
                filter: existing.filter.clone(),
                read_only: existing.read_only || read_only,
            },
            Some(existing) => PermissionCeiling {
                filter: filter.narrowed_by(&existing.filter),
                read_only: read_only || existing.read_only,
            },
            None => PermissionCeiling { filter, read_only },
        };
        *guard = Some(next);
    }

    pub fn delegate(&self, request: Delegate, host: Arc<dyn Host>) -> Result<Snapshot, String> {
        self.delegate_with_timeout(request, host, None)
    }

    /// Delegate a task from a known parent run.
    ///
    /// The parent's real run UUID is carried to the child host so the child's
    /// observations can link back to it. The child keeps its own engine run
    /// UUID: this never rewrites the child's run identity.
    pub fn delegate_child(
        &self,
        request: Delegate,
        host: Arc<dyn Host>,
        parent_run_id: Option<Uuid>,
    ) -> Result<Snapshot, String> {
        self.delegate_inner(
            request,
            host,
            None,
            crate::pattern::ToolFilter::default(),
            parent_run_id,
            None,
        )
    }

    /// Delegate from a known parent run **and** record the real provider
    /// tool-call id of the dispatch that created the child.
    ///
    /// The source call id is attached to the child's canonical record as the
    /// `provider_tool_call` identity. It is never derived from the child's own
    /// tool calls or from completion order.
    pub fn delegate_child_with_source(
        &self,
        request: Delegate,
        host: Arc<dyn Host>,
        parent_run_id: Option<Uuid>,
        source_call: Option<String>,
    ) -> Result<Snapshot, String> {
        self.delegate_inner(
            request,
            host,
            None,
            crate::pattern::ToolFilter::default(),
            parent_run_id,
            source_call,
        )
    }

    pub fn delegate_with_timeout(
        &self,
        request: Delegate,
        host: Arc<dyn Host>,
        task_timeout: Option<Duration>,
    ) -> Result<Snapshot, String> {
        self.delegate_with_options(
            request,
            host,
            task_timeout,
            crate::pattern::ToolFilter::default(),
        )
    }

    pub fn delegate_with_options(
        &self,
        request: Delegate,
        host: Arc<dyn Host>,
        task_timeout: Option<Duration>,
        tool_filter: crate::pattern::ToolFilter,
    ) -> Result<Snapshot, String> {
        self.delegate_inner(request, host, task_timeout, tool_filter, None, None)
    }

    fn delegate_inner(
        &self,
        mut request: Delegate,
        host: Arc<dyn Host>,
        task_timeout: Option<Duration>,
        tool_filter: crate::pattern::ToolFilter,
        parent_run_id: Option<Uuid>,
        source_call: Option<String>,
    ) -> Result<Snapshot, String> {
        validate_name(&request.session)?;
        // Nested delegation can only narrow: the child's effective filter is
        // the requested one intersected with every installed ceiling, and a
        // read-only ceiling cannot be lifted by a `read_only: false` request.
        let (tool_filter, read_only) = {
            let ceiling = self.ceiling.lock().unwrap().clone();
            match ceiling {
                Some(ceiling) => (
                    tool_filter.narrowed_by(&ceiling.filter),
                    request.read_only || ceiling.read_only,
                ),
                None => (tool_filter, request.read_only),
            }
        };
        request.read_only = read_only;
        if request.prompt.trim().is_empty()
            || request.prompt.len() > 65536
            || request.goal.trim().is_empty()
            || request.goal.len() > 2048
        {
            return Err("A bounded, nonempty goal and prompt are required".into());
        }
        let runtime = tokio::runtime::Handle::try_current().map_err(|e| e.to_string())?;
        let mut state = self.state.lock().unwrap();
        // The first dispatch installs the runtime handle the queue uses to
        // spawn admitted runs from lease drops and child completions.
        state.runtime = Some(runtime);
        let cwd = match &request.path {
            Some(path) => crate::checked_path(&self.root, path, false)?,
            None => state
                .sessions
                .get(&request.session)
                .map(|s| s.cwd.clone())
                .unwrap_or(self.root.clone()),
        }
        .canonicalize()
        .map_err(|e| e.to_string())?;
        if !cwd.is_dir() || !cwd.starts_with(&self.root) {
            return Err("Child workspace must be inside the parent root".into());
        }
        if state.runs.len() >= MAX_RECORDS
            || (!state.sessions.contains_key(&request.session)
                && state.sessions.len() >= MAX_RECORDS)
        {
            return Err("Subagent record limit reached; create a new manager".into());
        }
        let timeout = match task_timeout {
            Some(t) => t.min(state.timeout),
            None => state.timeout,
        };
        let key = RequestKey {
            parent_run_id,
            goal: request.goal.clone(),
            cwd: cwd.clone(),
            read_only: request.read_only,
            prompt: request.prompt.clone(),
            tool_filter: tool_filter.clone(),
            timeout,
        };
        // Reuse the original run id for a structurally identical request from
        // the same parent while that run is still queued or running. Different
        // requests remain an explicit error; terminal runs are never replayed.
        if let Some(session) = state.sessions.get(&request.session)
            && let Some(active) = session.active
            && let Some(record) = state.runs.get(&active)
            && record.snapshot.status.is_active()
        {
            if record.key == key {
                return Ok(record.snapshot.clone());
            }
            return Err("Session already has a different queued or running task; wait, kill it, or use a new session".into());
        }
        let history = if let Some(session) = state.sessions.get(&request.session) {
            if session.cwd != cwd
                || session.read_only != request.read_only
                || session.goal != request.goal
            {
                return Err(
                    "Session workspace, access mode and goal are immutable; use a new session"
                        .into(),
                );
            }
            session.history.clone()
        } else {
            Vec::new()
        };
        let id = Uuid::new_v4();
        let session_name = request.session.clone();
        let seq = state.next_seq;
        state.next_seq += 1;
        let snapshot = Snapshot {
            run_id: id,
            parent_run_id,
            session: request.session.clone(),
            goal: request.goal.clone(),
            cwd: cwd.clone(),
            read_only: request.read_only,
            status: Status::Queued,
            output: String::new(),
            output_truncated: false,
            last_tool: None,
            error: None,
            evidence: Vec::new(),
            evidence_truncated: false,
            denied_calls: Vec::new(),
            denied_calls_truncated: false,
            canonical_phase: None,
            engine_run_id: None,
            provider_tool_call_id: None,
        };
        let (cancel, cancelled) = oneshot::channel();
        let (changed, _) = watch::channel(Status::Queued);
        // Bind the child's event/observation channel to this run before it is
        // queued, so evidence is captured and no child text leaks to the parent.
        let task_host: Arc<dyn Host> = Arc::new(ChildHost {
            inner: host,
            state: Arc::downgrade(&self.state),
            id,
            session: request.session.clone(),
            parent_run_id,
        });
        state.sessions.insert(
            request.session.clone(),
            Session {
                goal: request.goal,
                cwd,
                read_only: request.read_only,
                history: history.clone(),
                active: Some(id),
            },
        );
        state.runs.insert(
            id,
            Record {
                seq,
                snapshot: snapshot.clone(),
                key,
                cancel: Some(cancel),
                changed,
                pending: Some(PendingRun {
                    prompt: request.prompt,
                    history,
                    host: task_host,
                    tool_filter,
                    timeout,
                    cancelled,
                }),
            },
        );
        drop(state);
        // Register the child's canonical task only after its record exists, so
        // the sink can project the `Queued` phase into the snapshot. A register
        // failure rolls the record back rather than leaving an untracked child.
        let task_id = TaskId::new(id.to_string()).map_err(|error| error.to_string())?;
        let mut identity = TaskIdentity::new(task_id);
        // Only a real, well-formed provider call id is attached; an invalid one
        // is left unknown instead of being replaced by a placeholder.
        if let Some(source) = source_call.and_then(|value| ProviderToolCallId::new(value).ok()) {
            identity = identity.with_provider_tool_call(source);
        }
        if let Err(error) = self.canonical.register(identity, None) {
            let mut state = self.state.lock().unwrap();
            state.runs.remove(&id);
            if let Some(entry) = state.sessions.get_mut(&session_name)
                && entry.active == Some(id)
            {
                entry.active = None;
            }
            state.sessions.remove(&session_name);
            return Err(error.to_string());
        }
        pump(&self.state, &self.canonical);
        self.snapshot(id)
    }
    pub fn snapshot(&self, id: Uuid) -> Result<Snapshot, String> {
        self.state
            .lock()
            .unwrap()
            .runs
            .get(&id)
            .map(|r| r.snapshot.clone())
            .ok_or("Unknown child run".into())
    }
    /// Return a versioned outcome only after the child's canonical run settles.
    pub fn result(&self, id: Uuid) -> Result<SubagentResult, String> {
        let snapshot = self.snapshot(id)?;
        if snapshot.status.is_active() {
            return Err("Child result is not ready; poll status first".into());
        }
        Ok(SubagentResult {
            schema: "mio.subagent.result/v1",
            verification: "unverified",
            snapshot,
        })
    }
    pub fn list(&self) -> Vec<Snapshot> {
        let state = self.state.lock().unwrap();
        let mut records: Vec<_> = state.runs.values().collect();
        records.sort_by_key(|r| r.seq);
        records.into_iter().map(|r| r.snapshot.clone()).collect()
    }
    pub fn activity(&self) -> Activity {
        let state = self.state.lock().unwrap();
        Activity {
            running: state
                .runs
                .values()
                .filter(|run| run.snapshot.status == Status::Running)
                .count(),
            queued: state
                .runs
                .values()
                .filter(|run| run.snapshot.status == Status::Queued)
                .count(),
            limit: state.concurrency,
        }
    }
    pub async fn status(&self, id: Uuid, wait: Duration) -> Result<Snapshot, String> {
        let mut changed = self
            .state
            .lock()
            .unwrap()
            .runs
            .get(&id)
            .ok_or("Unknown child run")?
            .changed
            .subscribe();
        // Wait for the run to become terminal, not merely for any single
        // notification: a same-run intermediate projection must never let an
        // early waiter observe a still-active phase as if it had settled.
        let deadline = tokio::time::Instant::now() + wait.min(MAX_WAIT);
        while (*changed.borrow_and_update()).is_active() {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            if tokio::time::timeout(remaining, changed.changed())
                .await
                .is_err()
            {
                break;
            }
        }
        self.snapshot(id)
    }
    /// Cancel a queued or running run and wait until it is terminal.
    ///
    /// A queued run is removed from the queue immediately, releasing its
    /// session; a running run receives cancellation. A cancelled blocking
    /// workspace operation may still be writing until its worker returns, so
    /// this never implies rollback.
    pub async fn kill(&self, id: Uuid) -> Result<Snapshot, String> {
        {
            let mut state = self.state.lock().unwrap();
            let record = state.runs.get_mut(&id).ok_or("Unknown child run")?;
            match record.snapshot.status {
                Status::Queued => {
                    record.pending = None;
                    record.cancel = None;
                    record.snapshot.error = Some("Child cancelled".into());
                    let session = record.snapshot.session.clone();
                    if let Some(entry) = state.sessions.get_mut(&session)
                        && entry.active == Some(id)
                    {
                        entry.active = None;
                    }
                }
                Status::Running => {
                    if let Some(cancel) = record.cancel.take() {
                        let _ = cancel.send(());
                    }
                }
                _ => {}
            }
        }
        // Canonical cancellation happens outside the state lock: its sink
        // re-locks state, so holding it here would deadlock.
        if let Ok(task_id) = TaskId::new(id.to_string()) {
            let _ = self
                .canonical
                .request_cancel(&task_id, Some("child cancelled"));
        }
        pump(&self.state, &self.canonical);
        self.status(id, MAX_WAIT).await
    }
    /// Fork the last successful history of an idle session; no task starts automatically.
    pub fn fork(&self, source: &str, target: &str) -> Result<(), String> {
        validate_name(target)?;
        let mut state = self.state.lock().unwrap();
        if state.sessions.contains_key(target) || state.sessions.len() >= MAX_RECORDS {
            return Err("Target session exists or session limit reached".into());
        }
        let source = state.sessions.get(source).ok_or("Unknown child session")?;
        if source.active.is_some() {
            return Err("Cannot fork an active child session".into());
        }
        let session = Session {
            goal: source.goal.clone(),
            cwd: source.cwd.clone(),
            read_only: source.read_only,
            history: source.history.clone(),
            active: None,
        };
        state.sessions.insert(target.to_owned(), session);
        Ok(())
    }
    pub(crate) fn parent_operation(&self, write: bool) -> Result<ParentLease, String> {
        let mut state = self.state.lock().unwrap();
        // Only a *running* child conflicts: a queued run cannot start while
        // `parent_operations > 0`, so it never overlaps the parent operation.
        let running_conflict = state.sessions.values().any(|session| {
            session.active.is_some_and(|id| {
                state.runs.get(&id).is_some_and(|record| {
                    record.snapshot.status == Status::Running
                        && (write || !record.snapshot.read_only)
                })
            })
        });
        if running_conflict {
            return Err("Wait for active children before accessing their workspace".into());
        }
        state.parent_operations += 1;
        Ok(ParentLease {
            state: self.state.clone(),
            canonical: self.canonical.clone(),
        })
    }
}
pub(crate) struct ParentLease {
    state: Arc<Mutex<State>>,
    canonical: Arc<TaskLifecycleTracker>,
}
impl Drop for ParentLease {
    fn drop(&mut self) {
        {
            let mut state = self.state.lock().unwrap();
            state.parent_operations -= 1;
        }
        // A parent workspace operation ending is an admission event.
        pump(&self.state, &self.canonical);
    }
}

impl State {
    /// Mark every admission-eligible queued run Running, oldest first.
    ///
    /// Strict head-of-line FIFO: when the oldest queued run cannot start, no
    /// younger run may jump ahead of it. This keeps an existing writer from
    /// being starved by a stream of newer read-only tasks while still letting
    /// read-only runs share the workspace.
    fn take_startable(&mut self) -> Vec<Job> {
        let mut jobs = Vec::new();
        if self.parent_operations > 0 {
            return jobs;
        }
        let mut running = self
            .runs
            .values()
            .filter(|run| run.snapshot.status == Status::Running)
            .count();
        let mut writer_running = self
            .runs
            .values()
            .any(|run| run.snapshot.status == Status::Running && !run.snapshot.read_only);
        let mut queued: Vec<Uuid> = self
            .runs
            .iter()
            .filter(|(_, run)| run.snapshot.status == Status::Queued)
            .map(|(id, _)| *id)
            .collect();
        queued.sort_by_key(|id| self.runs.get(id).map(|run| run.seq).unwrap_or(usize::MAX));
        let config = self.config.clone();
        let options = self.options.clone();
        for id in queued {
            if running >= self.concurrency {
                break;
            }
            let read_only = self.runs[&id].snapshot.read_only;
            if read_only && writer_running {
                break;
            }
            if !read_only && running > 0 {
                break;
            }
            // Acquire the real slot permit that will back this child's
            // `Queued -> Running` evidence. If no permit is free the queue
            // stays queued; the permit is never fabricated from the counter.
            let Ok(permit) = self.slots.clone().try_acquire_owned() else {
                break;
            };
            let Some(record) = self.runs.get_mut(&id) else {
                continue;
            };
            let Some(pending) = record.pending.take() else {
                continue;
            };
            let session = record.snapshot.session.clone();
            let cwd = record.snapshot.cwd.clone();
            let Ok(task_id) = TaskId::new(id.to_string()) else {
                continue;
            };
            let task = Task {
                config: config.clone(),
                history: pending.history,
                prompt: pending.prompt,
                cwd,
                host: pending.host,
                options: options.clone(),
                read_only,
                tool_filter: pending.tool_filter,
                task_handle: None,
            };
            jobs.push(Job {
                id,
                session,
                task,
                cancelled: pending.cancelled,
                timeout: pending.timeout,
                task_id,
                lease: ActiveLease::from_semaphore_permit(permit),
            });
            running += 1;
            if !read_only {
                writer_running = true;
            }
        }
        jobs
    }
}

/// Try to admit queued runs. The lock is released before anything is spawned,
/// and no waiter is polled here, so this is safe from a lease drop or a
/// blocking worker thread. The real `Queued -> Running` is recorded with the
/// held slot lease *before* the run is spawned; the tracker's sink (never this
/// function) then updates the cached snapshot, so no state lock is held across
/// the host callback.
fn pump(state: &Arc<Mutex<State>>, canonical: &Arc<TaskLifecycleTracker>) {
    let (jobs, runtime) = {
        let mut state = state.lock().unwrap();
        let Some(runtime) = state.runtime.clone() else {
            return;
        };
        (state.take_startable(), runtime)
    };
    for mut job in jobs {
        match canonical.admit(&job.task_id, job.lease.as_ref()) {
            Ok(_) => {
                job.task.task_handle =
                    Some(TaskHandle::new(canonical.clone(), job.task_id.clone()));
                let state = state.clone();
                let canonical = canonical.clone();
                runtime.spawn(async move { run_job(job, state, canonical).await });
            }
            Err(error) => {
                // The tracker refused a real `Running`; never spawn work under a
                // phase it did not accept. The slot returns when the lease drops.
                let _ =
                    canonical.finish(&job.task_id, Settlement::Failed, Some(&error.to_string()));
                let mut state = state.lock().unwrap();
                if let Some(entry) = state.runs.get_mut(&job.id) {
                    entry.snapshot.error = Some(format!("canonical admission failed: {error}"));
                    entry.cancel = None;
                    entry.pending = None;
                }
            }
        }
    }
}

/// Positive process-group reclamation evidence for a timed-out child run.
///
/// The child task's worker guards are released only after the direct `wait()`
/// returns in the reaper, so `workers == 0` is real reclamation evidence, not a
/// timestamp or a fabricated flag.
struct SubagentReap {
    handle: TaskHandle,
}

impl ProcessGroupReap for SubagentReap {
    fn is_reaped(&self) -> bool {
        self.handle
            .snapshot()
            .map(|record| record.workers == 0)
            .unwrap_or(true)
    }
}

async fn run_job(job: Job, state: Arc<Mutex<State>>, canonical: Arc<TaskLifecycleTracker>) {
    let Job {
        id,
        session,
        task,
        cancelled,
        timeout,
        task_id,
        lease,
    } = job;
    let handle = task.task_handle.clone();
    let executor = { state.lock().unwrap().executor.clone() };
    // Drop the execution future (including shell guards) before releasing the workspace lease.
    let outcome = {
        let work = AssertUnwindSafe(async move { executor.execute(task).await }).catch_unwind();
        tokio::select! {
            biased;
            _ = cancelled => Err((Status::Cancelled, "Child cancelled".to_owned())),
            _ = tokio::time::sleep(timeout) => Err((Status::TimedOut, "Child timed out".to_owned())),
            result = work => match result {
                Ok(Ok(history)) => match serde_json::to_vec(&history) {
                    Ok(bytes) if bytes.len() <= MAX_HISTORY => Ok(history),
                    _ => Err((Status::Failed, "Child history exceeded the 2 MiB limit".to_owned())),
                },
                Ok(Err(error)) => Err((Status::Failed, bounded(&error))),
                Err(_) => Err((Status::Failed, "Child executor panicked".to_owned())),
            }
        }
    };
    let (status, error) = {
        let mut state = state.lock().unwrap();
        let child = state
            .sessions
            .get_mut(&session)
            .expect("registered child session");
        let (status, error) = match outcome {
            Ok(history) => {
                child.history = history;
                (Status::Completed, None)
            }
            Err((status, error)) => (status, Some(error)),
        };
        let record = state.runs.get_mut(&id).expect("registered child run");
        record.snapshot.error = error.clone();
        record.cancel = None;
        record.pending = None;
        (status, error)
    };
    // Commit the canonical terminal only after the executor future (and every
    // real worker it spawned) has been reclaimed, while the slot lease is still
    // held. The sink updates the cached snapshot from outside this lock.
    if let Some(handle) = &handle {
        let detail = error.as_deref();
        match status {
            Status::TimedOut => {
                let reap = SubagentReap {
                    handle: handle.clone(),
                };
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                loop {
                    match canonical.finish(&task_id, Settlement::TimedOut { reap: &reap }, detail) {
                        Ok(_) => break,
                        Err(TaskLifecycleError::ProcessGroupAlive(_))
                            if tokio::time::Instant::now() < deadline =>
                        {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                        Err(_) => {
                            // Reclamation is not confirmed; never fake TimedOut.
                            let _ = canonical.finish(
                                &task_id,
                                Settlement::Failed,
                                Some("child timed out; process group reclamation unconfirmed"),
                            );
                            break;
                        }
                    }
                }
            }
            Status::Completed => {
                let _ = canonical.finish(&task_id, Settlement::Completed, detail);
            }
            Status::Cancelled => {
                let _ = canonical.finish(&task_id, Settlement::Cancelled, detail);
            }
            Status::Failed => {
                let _ = canonical.finish(&task_id, Settlement::Failed, detail);
            }
            Status::Queued | Status::Running => {}
        }
        // `finish` may be `Deferred` while a cancelled blocking worker or an
        // async reaper still holds a worker guard. Wait for actual
        // settlement/reclamation before clearing the session, releasing the
        // slot and pumping: the guard's release commits the terminal through
        // the canonical sink but does not pump, so releasing the slot while the
        // phase is still `Running` would leave a queued child with no admission
        // event. `wait_settled` also returns on a latched persistence failure,
        // so a broken sink cannot hang the child forever.
        let _ = handle.wait_settled().await;
    }
    // The session is only free after real settlement/reclamation.
    {
        let mut state = state.lock().unwrap();
        if let Some(child) = state.sessions.get_mut(&session)
            && child.active == Some(id)
        {
            child.active = None;
        }
    }
    // Release the slot only after the terminal commit, then admit the next run.
    drop(lease);
    pump(&state, &canonical);
}

fn validate_limits(
    config: &Config,
    options: &RunOptions,
    concurrency: usize,
    timeout: Duration,
) -> Result<(), String> {
    if !(1..=16).contains(&concurrency)
        || timeout.is_zero()
        || timeout > Duration::from_secs(3600)
        || options.instructions.len() > 128 * 1024
        || !(1..=256).contains(&options.max_turns)
        || config.model.trim().is_empty()
    {
        return Err("Invalid subagent workspace, model, limits or instructions".into());
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    {
        return Err(
            "Session names must use 1–128 ASCII letters, digits, underscores or hyphens".into(),
        );
    }
    Ok(())
}
fn bounded(text: &str) -> String {
    bounded_to(text, MAX_OUTPUT)
}
fn bounded_to(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
/// Stable FNV-1a hash so a host can compare evidence against a claimed result.
fn result_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}
struct ChildHost {
    inner: Arc<dyn Host>,
    /// Weak so a queued/running child record (which owns the pending host) can
    /// never keep the manager's `State` alive. The record -> pending -> host ->
    /// state cycle is broken here: with a strong reference, dropping the manager
    /// would leak every record, permit and host resource in that cycle.
    state: std::sync::Weak<Mutex<State>>,
    id: Uuid,
    session: String,
    parent_run_id: Option<Uuid>,
}
impl ChildHost {
    /// A child observation's run id is the child's own real engine run UUID.
    /// Only an observation that reports no run identity at all (a fabricated or
    /// pre-engine value) is attributed to the delegation run, so a real run id
    /// is never rewritten into a different scope.
    fn observed_run_id(&self, run_id: Uuid) -> Uuid {
        if run_id.is_nil() { self.id } else { run_id }
    }
}
impl Host for ChildHost {
    /// Children inherit the parent host's execution policy. The engine starts
    /// every supervised child process (shell, plugin tools, MCP servers) under
    /// this policy, so a delegated child cannot silently lose the isolation the
    /// parent selected.
    fn execution_policy(&self) -> crate::execution_backend::ExecutionPolicy {
        self.inner.execution_policy()
    }

    fn on_context_compaction(
        &self,
        observation: &crate::compact::ContextCompactionObservation,
    ) -> Result<(), String> {
        let mut observation = observation.clone();
        observation.run_id = self.observed_run_id(observation.run_id);
        observation.agent = Some(self.session.clone());
        self.inner.on_context_compaction(&observation)
    }

    fn approve_tool(&self, operation: &str, target: &str) -> Result<(), String> {
        self.inner.approve_tool(operation, target)
    }

    fn approve_tool_async(&self, operation: &str, target: &str) -> HostFuture<Result<(), String>> {
        self.inner.approve_tool_async(operation, target)
    }

    fn approve_tool_binding(
        &self,
        operation: &str,
        target: &str,
        binding: &crate::policy::ApprovalBinding,
    ) -> HostFuture<Result<(), String>> {
        self.inner.approve_tool_binding(operation, target, binding)
    }

    fn allow_persisted_mcp_trust(&self) -> bool {
        self.inner.allow_persisted_mcp_trust()
    }
    fn mcp_trust_directory(&self) -> Option<std::path::PathBuf> {
        self.inner.mcp_trust_directory()
    }

    /// A delegated child shares its parent run's unknown-effect fence, so an
    /// unknown observed anywhere in the run tree blocks new write/external
    /// dispatches everywhere in it. The child's own run scope is aliased to the
    /// shared fence by the engine, so a lazily loaded child tool resolves the
    /// same guard.
    fn unknown_effect_fence(&self) -> Option<Arc<crate::unknown_effect::UnknownEffectFence>> {
        match self.parent_run_id {
            Some(parent) => Some(crate::unknown_effect::fence_for_scope(&parent.to_string())),
            None => self.inner.unknown_effect_fence(),
        }
    }

    fn on_unknown_side_effect(
        &self,
        effect: &crate::unknown_effect::UnknownSideEffect,
    ) -> Result<(), String> {
        self.inner.on_unknown_side_effect(effect)
    }

    /// Capture bounded pre-dispatch denials and forward the child's canonical
    /// tool-call lifecycle to the parent host.
    ///
    /// The record keeps the child's own real engine run scope, so a delegated
    /// call is persisted with its real identity instead of being attributed to
    /// the parent's scope.
    fn on_tool_call_lifecycle(
        &self,
        call: &crate::tool_lifecycle::ToolCallRecord,
    ) -> Result<(), String> {
        if call.phase == crate::tool_lifecycle::ToolCallPhase::Denied
            && let Some(state) = self.state.upgrade()
            && let Some(record) = state.lock().unwrap().runs.get_mut(&self.id)
            && !record
                .snapshot
                .denied_calls
                .iter()
                .any(|item| item.call_index == call.call_index)
        {
            if record.snapshot.denied_calls.len() < MAX_EVIDENCE {
                record.snapshot.denied_calls.push(DeniedToolCall {
                    call_index: call.call_index,
                    tool: bounded_to(&call.tool, MAX_EVIDENCE_TOOL),
                    operation: bounded_to(&call.operation, MAX_EVIDENCE_TOOL),
                    provider_tool_call_id: call.tool_call_id.clone(),
                });
            } else {
                record.snapshot.denied_calls_truncated = true;
            }
        }
        self.inner.on_tool_call_lifecycle(call)
    }

    /// Forward the explicit unknown-effect confirmation to the parent host, so
    /// a host that can confirm a new side effect after an unknown result can do
    /// so in a delegated child too. The default trait implementation still
    /// refuses, so a host that never implemented it keeps the fail-closed
    /// behaviour in the child run.
    fn confirm_unknown_side_effect(
        &self,
        call: &crate::policy::NormalizedCall,
        unknown: &[crate::unknown_effect::UnknownSideEffect],
    ) -> HostFuture<Result<(), String>> {
        self.inner.confirm_unknown_side_effect(call, unknown)
    }

    fn shell(&self, command: &str, cwd: &Path) -> HostFuture<Result<String, String>> {
        self.inner.shell(command, cwd)
    }

    /// Forward the policy-aware shell entry point unchanged, so a delegated
    /// child inherits the parent's isolation decision (including a parent that
    /// fails closed) instead of silently running the command another way.
    fn shell_with_policy(
        &self,
        command: &str,
        cwd: &Path,
        policy: &crate::execution_backend::ExecutionPolicy,
    ) -> HostFuture<Result<String, String>> {
        self.inner.shell_with_policy(command, cwd, policy)
    }
    fn on_event(&self, event: AgentEvent) {
        if let AgentEvent::ToolFinished {
            operation,
            target,
            result,
        } = &event
        {
            self.inner.on_tool_result(operation, target, result);
        }
        if let Some(state) = self.state.upgrade()
            && let Some(record) = state.lock().unwrap().runs.get_mut(&self.id)
        {
            match event {
                AgentEvent::Text(text) => {
                    let remaining = MAX_OUTPUT - record.snapshot.output.len();
                    let mut end = text.len().min(remaining);
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    record.snapshot.output.push_str(&text[..end]);
                    record.snapshot.output_truncated |= end < text.len();
                }
                // Snapshot output is the child's answer, not its reasoning.
                AgentEvent::ToolStarted { operation, .. } => {
                    record.snapshot.last_tool = Some(bounded(&operation));
                }
                AgentEvent::ToolFinished {
                    operation,
                    target,
                    result,
                } => {
                    record.snapshot.last_tool = Some(bounded(&operation));
                    let (ok, summary, error, detail) = match &result {
                        Ok(text) => (
                            true,
                            Some(bounded_to(text, MAX_EVIDENCE_TEXT)),
                            None,
                            text.as_str(),
                        ),
                        Err(text) => (
                            false,
                            None,
                            Some(bounded_to(text, MAX_EVIDENCE_TEXT)),
                            text.as_str(),
                        ),
                    };
                    let evidence = ToolEvidence {
                        tool: bounded_to(&operation, MAX_EVIDENCE_TOOL),
                        target: bounded_to(&target, MAX_EVIDENCE_TARGET),
                        ok,
                        summary,
                        error,
                        result_hash: result_hash(detail),
                    };
                    if record.snapshot.evidence.len() < MAX_EVIDENCE {
                        record.snapshot.evidence.push(evidence);
                    } else {
                        record.snapshot.evidence_truncated = true;
                    }
                }
            }
        }
    }

    fn on_reasoning(&self, run_id: Uuid, part: usize, text: &str) {
        // Keep the child's real engine run identity. The parent host can
        // correlate it with the child's RunStarted observation, while the
        // child's answer snapshot remains free of reasoning text.
        self.inner.on_reasoning(run_id, part, text);
    }

    /// Inherit the parent's observation channel, stamped with this child
    /// run's identity. No text, provider body or request content crosses here.
    fn on_completion_call(&self, call: &crate::CompletionCallObservation) {
        let mut observed = call.clone();
        observed.run_id = self.observed_run_id(call.run_id);
        observed.agent = Some(self.session.clone());
        self.inner.on_completion_call(&observed);
    }

    /// Inherit the parent's diagnostic channel, stamped with this child run's
    /// identity. The diagnostic is already sanitized by the run.
    fn on_diagnostic(&self, diagnostic: &crate::RunDiagnostic) {
        let mut observed = diagnostic.clone();
        observed.run_id = self.observed_run_id(diagnostic.run_id);
        observed.agent = Some(self.session.clone());
        self.inner.on_diagnostic(&observed);
    }

    /// Inherit the parent's session identity, so a child run reports the
    /// session that owns the run tree.
    fn session_id(&self) -> Option<String> {
        self.inner.session_id()
    }

    /// Stamp this child's delegation identity onto the run observation while
    /// keeping the child's own real engine run UUID.
    ///
    /// `subagent_id` carries the delegated run marker and `parent_run_id` links
    /// the child to the run that started it. The child's approval scope and its
    /// run id therefore refer to the same real run.
    fn on_run_observation(&self, observation: &crate::RunObservation) {
        let mut observed = observation.clone();
        observed.subagent_id = Some(self.id.to_string());
        observed.parent_run_id = self.parent_run_id;
        self.inner.on_run_observation(&observed);
    }

    /// Inherit the parent's read-observation channel, keeping the child's own
    /// real engine run UUID and marking its delegation identity.
    ///
    /// The observation only carries a workspace-relative path, the actual
    /// returned range and the file total; file contents and absolute paths
    /// never cross it. A real child run id is preserved and a genuinely
    /// absent identity stays absent rather than being filled in with a guess.
    fn on_read_observation(&self, observation: &crate::ReadObservation) {
        let mut observed = observation.clone();
        observed.subagent_id = Some(self.id.to_string());
        observed.parent_run_id = self.parent_run_id;
        self.inner.on_read_observation(&observed);
    }
}

#[derive(Deserialize)]
pub struct ToolArgs {
    pub operation: String,
    pub task: Option<Delegate>,
    pub run_id: Option<Uuid>,
    pub session: Option<String>,
    pub target: Option<String>,
    pub wait_ms: Option<u64>,
}
#[derive(Clone)]
pub struct SubagentTool {
    manager: Arc<SubagentManager>,
    host: Arc<dyn Host>,
    /// The parent run that owns this tool, when known.
    parent_run_id: Option<Uuid>,
}
impl SubagentTool {
    pub fn new(manager: Arc<SubagentManager>, host: Arc<dyn Host>) -> Self {
        Self {
            manager,
            host,
            parent_run_id: None,
        }
    }

    /// Bind the tool to the run that dispatches it, so a delegated child can
    /// report its real parent run.
    pub fn with_parent_run_id(mut self, run_id: Uuid) -> Self {
        self.parent_run_id = Some(run_id);
        self
    }

    pub async fn execute(&self, args: ToolArgs) -> Result<Value, String> {
        self.execute_with_source(args, None).await
    }

    /// Execute a control call, recording the real provider tool-call id of the
    /// dispatch that created a delegated child (when the engine supplies one).
    async fn execute_with_source(
        &self,
        args: ToolArgs,
        source_call: Option<&str>,
    ) -> Result<Value, String> {
        match args.operation.as_str() {
            "delegate" => Ok(json!(self.manager.delegate_child_with_source(
                args.task.ok_or("task required")?,
                self.host.clone(),
                self.parent_run_id,
                source_call.map(str::to_owned),
            )?)),
            "status" | "snapshot" => Ok(json!(
                self.manager
                    .status(
                        args.run_id.ok_or("run_id required")?,
                        Duration::from_millis(args.wait_ms.unwrap_or(0))
                    )
                    .await?
            )),
            "result" => Ok(json!(
                self.manager.result(args.run_id.ok_or("run_id required")?)?
            )),
            "kill" => Ok(json!(
                self.manager
                    .kill(args.run_id.ok_or("run_id required")?)
                    .await?
            )),
            "list" => Ok(json!(self.manager.list())),
            "fork" => {
                self.manager.fork(
                    &args.session.ok_or("session required")?,
                    &args.target.ok_or("target required")?,
                )?;
                Ok(json!({"forked":true}))
            }
            _ => Err("Unknown subagent operation".into()),
        }
    }

    /// Emit the control call's lifecycle events and return its raw result.
    ///
    /// This is the single implementation the public `PortableTool` adapter and
    /// the typed engine adapter share, so both observe exactly the same events
    /// and the same `Result<Value, String>`:
    ///
    /// - `Ok(snapshot)` is a successful control call. A top-level `error` field
    ///   belongs to the **child task's** business result (for example a failed
    ///   child run) and never means this query failed.
    /// - `Err(message)` is a real failure of this control call (unknown
    ///   operation, missing argument, unknown run id).
    async fn run(&self, args: ToolArgs) -> Result<Value, String> {
        self.run_with_source(args, None).await
    }

    /// Same as [`Self::run`], carrying the real provider call id of this
    /// dispatch so a created child records its true source identity.
    async fn run_with_source(
        &self,
        args: ToolArgs,
        source_call: Option<String>,
    ) -> Result<Value, String> {
        let operation = args.operation.clone();
        self.host.on_event(AgentEvent::ToolStarted {
            operation: format!("subagent:{operation}"),
            target: args.session.clone().unwrap_or_default(),
        });
        let result = self.execute_with_source(args, source_call.as_deref()).await;
        self.host.on_event(AgentEvent::ToolFinished {
            operation: format!("subagent:{operation}"),
            target: String::new(),
            result: result.as_ref().map(Value::to_string).map_err(Clone::clone),
        });
        result
    }
}
impl PortableTool for SubagentTool {
    const NAME: &'static str = "subagent";
    type Args = ToolArgs;
    type Output = Value;
    type Error = std::convert::Infallible;
    fn description(&self) -> String {
        "Delegate a self-contained task to a native child with independent history. Children cannot delegate. Default read-only; explicitly set read_only=false for edits. Keep session goal/path/access unchanged to resume. Poll status by run_id (wait_ms up to 30000); result returns a versioned envelope only after settlement and marks child claims unverified. Inspect evidence and independently verify results before the final answer. Tasks queue while concurrency or the workspace is busy, so a delegate may return queued; wait before parent writes or starting overlapping write tasks. Sessions live only in this process; fork clones last successful history. Child subprocesses inherit the host's execution policy; the engine adds no separate sandbox of its own.".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{
            "operation":{"type":"string","enum":["delegate","status","snapshot","result","list","kill","fork"]},
            "task":{"type":"object","properties":{
                "session":{"type":"string"},"goal":{"type":"string"},"prompt":{"type":"string"},
                "path":{"type":"string","description":"Directory inside parent workspace; omitted means existing session directory or root"},
                "read_only":{"type":"boolean","default":true}
            },"required":["session","goal","prompt"]},
            "run_id":{"type":"string"},"session":{"type":"string"},"target":{"type":"string"},
            "wait_ms":{"type":"integer","minimum":0,"maximum":30000}
        },"required":["operation"]})
    }
    async fn call(&self, args: ToolArgs) -> Result<Value, Self::Error> {
        // `Error = Infallible`: this public convenience channel keeps its
        // original encoding, a top-level `{"error": ...}` JSON value. The
        // engine never sees a failure as success because it registers the
        // typed [`SubagentToolAdapter`] instead of this value channel.
        Ok(match self.run(args).await {
            Ok(value) => value,
            Err(error) => json!({"error":error}),
        })
    }
}

/// 子代理控制调用的专用类型化 `Tool` 适配器。
///
/// 它用 [`SubagentTool::run`] 的真实 `Result<Value, String>` 区分两类情况，不解析任何
/// 业务 JSON：
///
/// - `Ok(snapshot)`：控制调用成功。snapshot 顶层 `error` 是**子任务**的业务结果
///   （例如子运行失败），必须作为成功结果交给引擎。
/// - `Err(message)`：本次控制调用真的失败（未知 operation、缺少参数、未知 run id），
///   转成 [`ToolExecutionError`]，模型可见输出保持原来的 `{"error": message}` JSON。
///
/// 通用 [`crate::BuiltinTool`] 的失败判据是「顶层带 `error` 字符串」，对成功的
/// `status` / `snapshot` / `kill` 结果会误报；子代理工具因此改用本适配器注册。
#[derive(Clone)]
pub struct SubagentToolAdapter {
    tool: SubagentTool,
}

impl SubagentToolAdapter {
    pub fn new(manager: Arc<SubagentManager>, host: Arc<dyn Host>) -> Self {
        Self {
            tool: SubagentTool::new(manager, host),
        }
    }

    /// Bind the tool to the run that dispatches it, so a delegated child can
    /// report its real parent run.
    pub fn with_parent_run_id(mut self, run_id: Uuid) -> Self {
        self.tool = self.tool.with_parent_run_id(run_id);
        self
    }

    /// The wrapped control tool; its public `PortableTool` behaviour is unchanged.
    pub fn inner(&self) -> &SubagentTool {
        &self.tool
    }
}

impl Tool for SubagentToolAdapter {
    const NAME: &'static str = "subagent";
    type Args = ToolArgs;
    type Output = Value;
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        PortableTool::description(&self.tool)
    }

    fn parameters(&self) -> Value {
        PortableTool::parameters(&self.tool)
    }

    fn call(
        &self,
        context: &mut ToolContext,
        args: Self::Args,
    ) -> impl Future<Output = Result<Value, ToolExecutionError>> + WasmCompatSend {
        let tool = &self.tool;
        // The engine attaches the inbound call's real wire id as a live scope,
        // never through arguments or a synthesized value. A missing scope (for
        // example a direct SDK call) leaves the source unknown and is not
        // replaced by a fabricated id.
        let source_call = context
            .scope::<ToolInvocation>()
            .map(|invocation| invocation.tool_call_id.clone());
        async move {
            match tool.run_with_source(args, source_call).await {
                // The control call succeeded. Whatever the snapshot says about
                // the child is the caller's business data, not an engine error.
                Ok(value) => Ok(value),
                Err(message) => {
                    let model_output = ToolOutput::json(json!({"error": &message}));
                    Err(ToolExecutionError::other(message).with_model_output(model_output))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Pending;
    impl Executor for Pending {
        fn execute(&self, _: Task) -> HostFuture<Result<Vec<Message>, String>> {
            Box::pin(std::future::pending())
        }
    }
    impl Host for Pending {
        fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
            Box::pin(std::future::pending())
        }
        fn on_event(&self, _: AgentEvent) {}
    }

    fn manager(root: &Path) -> Arc<SubagentManager> {
        Arc::new(
            SubagentManager::with_executor(
                root,
                Config {
                    model: "mock".into(),
                    ..Config::default()
                },
                RunOptions::default(),
                4,
                Duration::from_secs(5),
                Arc::new(Pending),
            )
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn terminal_result_keeps_parent_identity_without_claiming_verification() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(dir.path());
        let parent = Uuid::new_v4();
        let _parent_operation = manager.parent_operation(true).unwrap();
        let child = manager
            .delegate_child_with_source(
                Delegate {
                    session: "review-child".into(),
                    goal: "review".into(),
                    prompt: "review this change".into(),
                    path: None,
                    read_only: true,
                },
                Arc::new(Pending),
                Some(parent),
                Some("provider-call-42".into()),
            )
            .unwrap();
        assert_eq!(child.status, Status::Queued);
        assert_eq!(child.parent_run_id, Some(parent));
        assert!(manager.result(child.run_id).is_err());
        let result_tool = SubagentTool::new(manager.clone(), Arc::new(Pending));
        let result_args = || ToolArgs {
            operation: "result".into(),
            task: None,
            run_id: Some(child.run_id),
            session: None,
            target: None,
            wait_ms: None,
        };
        assert!(result_tool.execute(result_args()).await.is_err());

        manager.kill(child.run_id).await.unwrap();
        let result = manager.result(child.run_id).unwrap();
        let value = serde_json::to_value(result).unwrap();
        assert_eq!(value["schema"], "mio.subagent.result/v1");
        assert_eq!(value["verification"], "unverified");
        assert_eq!(value["status"], "cancelled");
        assert_eq!(value["parent_run_id"], parent.to_string());
        assert_eq!(value["provider_tool_call_id"], "provider-call-42");
        assert!(value["error"].as_str().is_some());
        assert_eq!(result_tool.execute(result_args()).await.unwrap(), value);
    }

    #[test]
    fn child_reasoning_keeps_the_engine_run_identity() {
        struct ReasoningHost(Mutex<Vec<(Uuid, usize, String)>>);
        impl Host for ReasoningHost {
            fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
                Box::pin(async { Err("unused".into()) })
            }
            fn on_event(&self, _: AgentEvent) {}
            fn on_reasoning(&self, run_id: Uuid, part: usize, text: &str) {
                self.0.lock().unwrap().push((run_id, part, text.into()));
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let manager = manager(dir.path());
        let parent = Arc::new(ReasoningHost(Mutex::new(Vec::new())));
        let child = ChildHost {
            inner: parent.clone(),
            state: Arc::downgrade(&manager.state),
            id: Uuid::new_v4(),
            session: "child".into(),
            parent_run_id: Some(Uuid::new_v4()),
        };
        let engine_run = Uuid::new_v4();
        child.on_reasoning(engine_run, 2, "real reasoning");
        assert_eq!(
            *parent.0.lock().unwrap(),
            vec![(engine_run, 2, "real reasoning".into())]
        );
    }

    #[tokio::test]
    async fn parent_lease_drop_wakes_queued_child() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(dir.path());
        let task = Delegate {
            session: "child".into(),
            goal: "review".into(),
            prompt: "review".into(),
            path: None,
            read_only: true,
        };
        let guard = manager.parent_operation(true).unwrap();
        let queued = manager.delegate(task.clone(), Arc::new(Pending)).unwrap();
        assert_eq!(queued.status, Status::Queued);
        assert!(queued.status.is_active());
        // Re-delegating the exact same request reuses the queued run.
        let duplicate = manager.delegate(task.clone(), Arc::new(Pending)).unwrap();
        assert_eq!(duplicate.run_id, queued.run_id);
        // A different prompt is still an explicit rejection.
        let different = Delegate {
            session: "child".into(),
            goal: "review".into(),
            prompt: "other".into(),
            path: None,
            read_only: true,
        };
        assert!(manager.delegate(different, Arc::new(Pending)).is_err());
        drop(guard);
        assert_eq!(
            manager.snapshot(queued.run_id).unwrap().status,
            Status::Running
        );
        manager.kill(queued.run_id).await.unwrap();
    }

    #[tokio::test]
    async fn active_child_reuse_is_scoped_to_parent_run() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(dir.path());
        let _parent_operation = manager.parent_operation(true).unwrap();
        let task = Delegate {
            session: "shared-name".into(),
            goal: "review".into(),
            prompt: "review this change".into(),
            path: None,
            read_only: true,
        };
        let first_parent = Uuid::new_v4();
        let first = manager
            .delegate_child_with_source(
                task.clone(),
                Arc::new(Pending),
                Some(first_parent),
                Some("first-call".into()),
            )
            .unwrap();
        assert_eq!(first.status, Status::Queued);
        let duplicate = manager
            .delegate_child_with_source(
                task.clone(),
                Arc::new(Pending),
                Some(first_parent),
                Some("retry-call".into()),
            )
            .unwrap();
        assert_eq!(duplicate.run_id, first.run_id);
        assert_eq!(duplicate.parent_run_id, Some(first_parent));
        assert_eq!(
            duplicate.provider_tool_call_id.as_deref(),
            Some("first-call")
        );
        assert!(
            manager
                .delegate_child_with_source(
                    task,
                    Arc::new(Pending),
                    Some(Uuid::new_v4()),
                    Some("other-parent-call".into()),
                )
                .is_err()
        );
        manager.kill(first.run_id).await.unwrap();
    }

    #[tokio::test]
    async fn manager_drop_breaks_queued_host_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(dir.path());
        let weak = Arc::downgrade(&manager.state);
        let guard = manager.parent_operation(true).unwrap();
        let task = Delegate {
            session: "child".into(),
            goal: "review".into(),
            prompt: "review".into(),
            path: None,
            read_only: true,
        };
        let queued = manager.delegate(task, Arc::new(Pending)).unwrap();
        assert_eq!(queued.status, Status::Queued);
        drop(manager);
        assert!(weak.upgrade().is_some());
        drop(guard);
        assert!(weak.upgrade().is_none(), "queued host cycle must not leak");
    }

    #[tokio::test]
    async fn parent_and_child_workspace_operations_share_leases() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(dir.path());
        let task = Delegate {
            session: "child".into(),
            goal: "review".into(),
            prompt: "review".into(),
            path: None,
            read_only: true,
        };
        let guard = manager.parent_operation(true).unwrap();
        let queued = manager.delegate(task.clone(), Arc::new(Pending)).unwrap();
        assert_eq!(queued.status, Status::Queued);
        drop(guard);
        let child = manager.delegate(task, Arc::new(Pending)).unwrap();
        assert_eq!(child.run_id, queued.run_id);
        assert_eq!(child.status, Status::Running);
        let mut workspace = crate::Workspace::new(dir.path(), Arc::new(Pending)).unwrap();
        workspace.subagents = Some(manager.clone());
        assert!(
            workspace
                .execute(crate::Arguments::write("result", "blocked"))
                .await
                .is_err()
        );
        assert!(manager.parent_operation(false).is_ok());
        manager.kill(child.run_id).await.unwrap();
        workspace
            .execute(crate::Arguments::write("result", "allowed"))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("result")).unwrap(),
            "allowed"
        );
    }

    #[tokio::test]
    async fn child_host_forwards_read_observations_with_child_identity() {
        struct Recording {
            reads: Mutex<Vec<crate::ReadObservation>>,
        }
        impl Host for Recording {
            fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
                Box::pin(async { Ok(String::new()) })
            }
            fn on_event(&self, _: AgentEvent) {}
            fn on_read_observation(&self, observation: &crate::ReadObservation) {
                self.reads.lock().unwrap().push(observation.clone());
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let manager = manager(dir.path());
        let recorder = Arc::new(Recording {
            reads: Mutex::new(Vec::new()),
        });
        let child_id = Uuid::new_v4();
        let parent_id = Uuid::new_v4();
        let child_marker = child_id.to_string();
        let child = ChildHost {
            inner: recorder.clone(),
            state: Arc::downgrade(&manager.state),
            id: child_id,
            session: "child".into(),
            parent_run_id: Some(parent_id),
        };

        let real_run_id = Uuid::new_v4();
        let mut with_real_run = crate::ReadObservation::new(
            None,
            "f.txt".into(),
            crate::ReadRange::Lines {
                offset: 1,
                limit: 2,
            },
            Some(2),
            false,
        );
        with_real_run.run_id = Some(real_run_id);
        child.on_read_observation(&with_real_run);

        let without_run = crate::ReadObservation::new(
            None,
            "g.txt".into(),
            crate::ReadRange::Unknown,
            Some(0),
            false,
        );
        child.on_read_observation(&without_run);

        let reads = recorder.reads.lock().unwrap().clone();
        assert_eq!(reads.len(), 2);
        assert_eq!(
            reads[0].run_id,
            Some(real_run_id),
            "a real child run id must be preserved"
        );
        assert_eq!(reads[0].subagent_id.as_deref(), Some(child_marker.as_str()));
        assert_eq!(reads[0].parent_run_id, Some(parent_id));
        assert!(
            reads[1].run_id.is_none(),
            "a genuinely absent run id stays absent rather than being fabricated"
        );
        assert_eq!(reads[1].subagent_id.as_deref(), Some(child_marker.as_str()));
        assert_eq!(reads[1].parent_run_id, Some(parent_id));
        // No contents or absolute paths crossed the child boundary.
        assert_eq!(reads[0].path, "f.txt");
        assert_eq!(reads[1].path, "g.txt");
    }

    /// A host that can explicitly confirm a new side effect after an unknown
    /// result must be reachable through a delegated child too; a host that never
    /// implemented the confirmation keeps the fail-closed default.
    #[tokio::test]
    async fn child_host_forwards_the_unknown_effect_confirmation() {
        struct Confirming {
            confirms: Mutex<Vec<(String, usize)>>,
        }
        impl Host for Confirming {
            fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
                Box::pin(async { Ok(String::new()) })
            }
            fn on_event(&self, _: AgentEvent) {}
            fn confirm_unknown_side_effect(
                &self,
                call: &crate::policy::NormalizedCall,
                unknown: &[crate::unknown_effect::UnknownSideEffect],
            ) -> HostFuture<Result<(), String>> {
                self.confirms
                    .lock()
                    .unwrap()
                    .push((call.operation.clone(), unknown.len()));
                Box::pin(async { Ok(()) })
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let manager = manager(dir.path());
        let inner = Arc::new(Confirming {
            confirms: Mutex::new(Vec::new()),
        });
        let call = crate::policy::NormalizedCall {
            entry: crate::policy::EntryKind::Mcp,
            tool: "mcp__srv__probe".into(),
            operation: "mcp__srv__probe".into(),
            display_target: "{}".into(),
            side_effect: crate::policy::SideEffect::External,
            requires_approval: true,
            aliases: vec!["mcp__srv__probe".into()],
            params_sha256: "0".repeat(64),
        };
        let unknown = [crate::unknown_effect::UnknownSideEffect::new(
            "mcp__srv__probe",
            "{}",
            crate::unknown_effect::UnknownReason::McpTransport,
            None,
        )];

        let child = ChildHost {
            inner: inner.clone(),
            state: Arc::downgrade(&manager.state),
            id: Uuid::new_v4(),
            session: "child".into(),
            parent_run_id: Some(Uuid::new_v4()),
        };
        child
            .confirm_unknown_side_effect(&call, &unknown)
            .await
            .expect("the child must forward the explicit confirmation");
        assert_eq!(
            inner.confirms.lock().unwrap().clone(),
            vec![("mcp__srv__probe".to_owned(), 1)]
        );

        let refusing = ChildHost {
            inner: Arc::new(Pending),
            state: Arc::downgrade(&manager.state),
            id: Uuid::new_v4(),
            session: "child".into(),
            parent_run_id: Some(Uuid::new_v4()),
        };
        assert!(
            refusing
                .confirm_unknown_side_effect(&call, &unknown)
                .await
                .is_err(),
            "a host without the confirmation must still refuse in a child"
        );
    }

    #[test]
    fn status_is_active_covers_queued_and_running() {
        assert!(Status::Queued.is_active());
        assert!(Status::Running.is_active());
        assert!(!Status::Completed.is_active());
        assert!(!Status::Failed.is_active());
        assert!(!Status::Cancelled.is_active());
        assert!(!Status::TimedOut.is_active());
    }

    /// ROOT-REVIEW 5：排队记录的 pending host 只能用 `Weak` 回指 manager state，
    /// 否则 record -> pending -> ChildHost -> state 的 Arc 环会让 manager drop 后
    /// 记录、许可与 host 资源永远不释放。这里在真实并发子任务下验证：manager 被
    /// drop、运行中子任务被取消并 settle 后，底层 `State` 必须真正析构。
    #[tokio::test]
    async fn dropping_the_manager_releases_state_after_children_settle() {
        use std::sync::Weak as StdWeak;
        fn delegate(session: &str) -> Delegate {
            Delegate {
                session: session.into(),
                goal: "review".into(),
                prompt: "review".into(),
                path: None,
                read_only: true,
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(
            SubagentManager::with_executor(
                dir.path(),
                Config {
                    model: "mock".into(),
                    ..Config::default()
                },
                RunOptions::default(),
                1,
                Duration::from_secs(5),
                Arc::new(Pending),
            )
            .unwrap(),
        );
        let running = manager
            .delegate(delegate("running"), Arc::new(Pending))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while manager.activity().running != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the first child must start");
        let queued = manager
            .delegate(delegate("queued"), Arc::new(Pending))
            .unwrap();
        assert_eq!(queued.status, Status::Queued);
        let weak: StdWeak<Mutex<State>> = Arc::downgrade(&manager.state);
        drop(manager);
        // The running child is cancelled by the drop; once its `run_job`
        // releases the strong `State`, the whole manager state must be gone.
        tokio::time::timeout(Duration::from_secs(5), async {
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("state, records and pending hosts must be released after settlement");
        assert_ne!(running.run_id, queued.run_id);
    }
}
