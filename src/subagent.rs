//! Native, in-memory child sessions. Task isolation is not process or OS isolation.
//!
//! Admission is queued instead of refused. Reaching the concurrency limit, an
//! overlapping writer or an active parent workspace operation registers the run
//! as [`Status::Queued`] and lets it start later. Queued runs are not spawned,
//! so they hold no model, heavy-task or workspace permit while waiting.
use crate::{
    AgentEvent, Config, Host, HostFuture, Message, Provider, RunOptions, RunTools, Services,
};
use futures_util::FutureExt;
use mio_core::tool::PortableTool;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
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

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub run_id: Uuid,
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
        crate::run_with_tools(
            task.config,
            task.history,
            &task.prompt,
            &task.cwd,
            task.host,
            Services {
                antigravity: antigravity.as_ref(),
                ..Services::default()
            },
            RunTools {
                options: task.options,
                read_only: task.read_only,
                subagents: None,
                tool_filter: task.tool_filter,
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
}

/// Drop cancels outstanding children; successful history lasts for this manager's lifetime.
pub struct SubagentManager {
    root: PathBuf,
    state: Arc<Mutex<State>>,
}
impl Drop for SubagentManager {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap();
        for record in state.runs.values_mut() {
            match record.snapshot.status {
                Status::Queued => {
                    // Clearing `pending` breaks the record -> host -> state cycle.
                    record.pending = None;
                    record.cancel = None;
                    record.snapshot.status = Status::Cancelled;
                    record.snapshot.error = Some("Child cancelled".into());
                    record.changed.send_replace(Status::Cancelled);
                }
                Status::Running => {
                    if let Some(cancel) = record.cancel.take() {
                        let _ = cancel.send(());
                    }
                }
                _ => {}
            }
        }
    }
}
impl SubagentManager {
    pub fn new(root: &Path, config: Config, options: RunOptions) -> Result<Self, String> {
        if !matches!(
            config.provider,
            Provider::Custom
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
        Ok(Self {
            root,
            state: Arc::new(Mutex::new(State {
                sessions: BTreeMap::new(),
                runs: BTreeMap::new(),
                next_seq: 0,
                parent_operations: 0,
                concurrency,
                timeout,
                config,
                options,
                executor,
                runtime: None,
            })),
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

    pub fn delegate(&self, request: Delegate, host: Arc<dyn Host>) -> Result<Snapshot, String> {
        self.delegate_with_timeout(request, host, None)
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
        validate_name(&request.session)?;
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
            goal: request.goal.clone(),
            cwd: cwd.clone(),
            read_only: request.read_only,
            prompt: request.prompt.clone(),
            tool_filter: tool_filter.clone(),
            timeout,
        };
        // Reuse the original run id for a structurally identical request while
        // that run is still queued or running. Different requests remain an
        // explicit error, and terminal runs are never replayed by this path.
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
        let seq = state.next_seq;
        state.next_seq += 1;
        let snapshot = Snapshot {
            run_id: id,
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
        };
        let (cancel, cancelled) = oneshot::channel();
        let (changed, _) = watch::channel(Status::Queued);
        // Bind the child's event/observation channel to this run before it is
        // queued, so evidence is captured and no child text leaks to the parent.
        let task_host: Arc<dyn Host> = Arc::new(ChildHost {
            inner: host,
            state: self.state.clone(),
            id,
            session: request.session.clone(),
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
        pump(&self.state);
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
        if (*changed.borrow_and_update()).is_active() {
            let _ = tokio::time::timeout(wait.min(MAX_WAIT), changed.changed()).await;
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
        let mut pump_needed = false;
        {
            let mut state = self.state.lock().unwrap();
            let record = state.runs.get_mut(&id).ok_or("Unknown child run")?;
            match record.snapshot.status {
                Status::Queued => {
                    record.pending = None;
                    record.cancel = None;
                    record.snapshot.status = Status::Cancelled;
                    record.snapshot.error = Some("Child cancelled".into());
                    record.changed.send_replace(Status::Cancelled);
                    let session = record.snapshot.session.clone();
                    if let Some(entry) = state.sessions.get_mut(&session)
                        && entry.active == Some(id)
                    {
                        entry.active = None;
                    }
                    pump_needed = true;
                }
                Status::Running => {
                    if let Some(cancel) = record.cancel.take() {
                        let _ = cancel.send(());
                    }
                }
                _ => {}
            }
        }
        if pump_needed {
            pump(&self.state);
        }
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
        Ok(ParentLease(self.state.clone()))
    }
}
pub(crate) struct ParentLease(Arc<Mutex<State>>);
impl Drop for ParentLease {
    fn drop(&mut self) {
        {
            let mut state = self.0.lock().unwrap();
            state.parent_operations -= 1;
        }
        // A parent workspace operation ending is an admission event.
        pump(&self.0);
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
            let Some(record) = self.runs.get_mut(&id) else {
                continue;
            };
            let Some(pending) = record.pending.take() else {
                continue;
            };
            let session = record.snapshot.session.clone();
            let cwd = record.snapshot.cwd.clone();
            let task = Task {
                config: config.clone(),
                history: pending.history,
                prompt: pending.prompt,
                cwd,
                host: pending.host,
                options: options.clone(),
                read_only,
                tool_filter: pending.tool_filter,
            };
            record.snapshot.status = Status::Running;
            record.changed.send_replace(Status::Running);
            jobs.push(Job {
                id,
                session,
                task,
                cancelled: pending.cancelled,
                timeout: pending.timeout,
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
/// blocking worker thread.
fn pump(state: &Arc<Mutex<State>>) {
    let (jobs, runtime) = {
        let mut state = state.lock().unwrap();
        let Some(runtime) = state.runtime.clone() else {
            return;
        };
        (state.take_startable(), runtime)
    };
    for job in jobs {
        let state = state.clone();
        runtime.spawn(async move { run_job(job, state).await });
    }
}

async fn run_job(job: Job, state: Arc<Mutex<State>>) {
    let Job {
        id,
        session,
        task,
        cancelled,
        timeout,
    } = job;
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
    {
        let mut state = state.lock().unwrap();
        let child = state
            .sessions
            .get_mut(&session)
            .expect("registered child session");
        child.active = None;
        let (status, error) = match outcome {
            Ok(history) => {
                child.history = history;
                (Status::Completed, None)
            }
            Err((status, error)) => (status, Some(error)),
        };
        let record = state.runs.get_mut(&id).expect("registered child run");
        record.snapshot.status = status;
        record.snapshot.error = error;
        record.cancel = None;
        record.pending = None;
        record.changed.send_replace(status);
    }
    // A terminal child frees its slot and may unblock the head of the queue.
    pump(&state);
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
    state: Arc<Mutex<State>>,
    id: Uuid,
    session: String,
}
impl Host for ChildHost {
    fn approve_tool(&self, operation: &str, target: &str) -> Result<(), String> {
        self.inner.approve_tool(operation, target)
    }

    fn approve_tool_async(&self, operation: &str, target: &str) -> HostFuture<Result<(), String>> {
        self.inner.approve_tool_async(operation, target)
    }

    fn shell(&self, command: &str, cwd: &Path) -> HostFuture<Result<String, String>> {
        self.inner.shell(command, cwd)
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
        if let Some(record) = self.state.lock().unwrap().runs.get_mut(&self.id) {
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

    /// Inherit the parent's observation channel, stamped with this child
    /// run's identity. No text, provider body or request content crosses here.
    fn on_completion_call(&self, call: &crate::CompletionCallObservation) {
        let mut observed = call.clone();
        observed.run_id = self.id;
        observed.agent = Some(self.session.clone());
        self.inner.on_completion_call(&observed);
    }

    /// Inherit the parent's diagnostic channel, stamped with this child run's
    /// identity. The diagnostic is already sanitized by the run.
    fn on_diagnostic(&self, diagnostic: &crate::RunDiagnostic) {
        let mut observed = diagnostic.clone();
        observed.run_id = self.id;
        observed.agent = Some(self.session.clone());
        self.inner.on_diagnostic(&observed);
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
}
impl SubagentTool {
    pub fn new(manager: Arc<SubagentManager>, host: Arc<dyn Host>) -> Self {
        Self { manager, host }
    }
    pub async fn execute(&self, args: ToolArgs) -> Result<Value, String> {
        match args.operation.as_str() {
            "delegate" => Ok(json!(
                self.manager
                    .delegate(args.task.ok_or("task required")?, self.host.clone())?
            )),
            "status" | "snapshot" => Ok(json!(
                self.manager
                    .status(
                        args.run_id.ok_or("run_id required")?,
                        Duration::from_millis(args.wait_ms.unwrap_or(0))
                    )
                    .await?
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
}
impl PortableTool for SubagentTool {
    const NAME: &'static str = "subagent";
    type Args = ToolArgs;
    type Output = Value;
    type Error = std::convert::Infallible;
    fn description(&self) -> String {
        "Delegate a self-contained task to a native child with independent history. Children cannot delegate. Default read-only; explicitly set read_only=false for edits. Keep session goal/path/access unchanged to resume. Poll status by run_id (wait_ms up to 30000), collect results before final answer; completed means execution ended, inspect the reported evidence and independently verify results. Tasks queue while concurrency or the workspace is busy, so a delegate may return queued; wait before parent writes or starting overlapping write tasks. Sessions live only in this process; fork clones last successful history. No OS sandbox.".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{
            "operation":{"type":"string","enum":["delegate","status","snapshot","list","kill","fork"]},
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
        let operation = args.operation.clone();
        self.host.on_event(AgentEvent::ToolStarted {
            operation: format!("subagent:{operation}"),
            target: args.session.clone().unwrap_or_default(),
        });
        let result = self.execute(args).await;
        self.host.on_event(AgentEvent::ToolFinished {
            operation: format!("subagent:{operation}"),
            target: String::new(),
            result: result.as_ref().map(Value::to_string).map_err(Clone::clone),
        });
        Ok(match result {
            Ok(value) => value,
            Err(error) => json!({"error":error}),
        })
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

    #[test]
    fn status_is_active_covers_queued_and_running() {
        assert!(Status::Queued.is_active());
        assert!(Status::Running.is_active());
        assert!(!Status::Completed.is_active());
        assert!(!Status::Failed.is_active());
        assert!(!Status::Cancelled.is_active());
        assert!(!Status::TimedOut.is_active());
    }
}
