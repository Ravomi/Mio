//! Native, in-memory child sessions. Task isolation is not process or OS isolation.
use crate::{
    AgentEvent, Config, Host, HostFuture, Message, Provider, RunOptions, RunTools, Services,
};
use futures_util::FutureExt;
use rig_core::tool::PortableTool;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Running,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
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
}
pub trait Executor: Send + Sync {
    fn execute(&self, task: Task) -> HostFuture<Result<Vec<Message>, String>>;
}
pub struct NativeExecutor;
impl Executor for NativeExecutor {
    fn execute(&self, task: Task) -> HostFuture<Result<Vec<Message>, String>> {
        Box::pin(async move {
            crate::run_with_tools(
                task.config,
                task.history,
                &task.prompt,
                &task.cwd,
                task.host,
                Services::default(),
                RunTools {
                    options: task.options,
                    read_only: task.read_only,
                    subagents: None,
                },
            )
            .await
        })
    }
}

struct Session {
    goal: String,
    cwd: PathBuf,
    read_only: bool,
    history: Vec<Message>,
    active: Option<Uuid>,
}
struct Record {
    snapshot: Snapshot,
    cancel: Option<oneshot::Sender<()>>,
    changed: watch::Sender<Status>,
}
#[derive(Default)]
struct State {
    sessions: BTreeMap<String, Session>,
    runs: BTreeMap<Uuid, Record>,
    parent_operations: usize,
}

/// Drop cancels outstanding children; successful history lasts for this manager's lifetime.
pub struct SubagentManager {
    root: PathBuf,
    config: Config,
    options: RunOptions,
    concurrency: usize,
    timeout: Duration,
    executor: Arc<dyn Executor>,
    state: Arc<Mutex<State>>,
}
impl Drop for SubagentManager {
    fn drop(&mut self) {
        for record in self.state.lock().unwrap().runs.values_mut() {
            if let Some(cancel) = record.cancel.take() {
                let _ = cancel.send(());
            }
        }
    }
}
impl SubagentManager {
    pub fn new(root: &Path, config: Config, options: RunOptions) -> Result<Self, String> {
        Self::with_executor(
            root,
            config,
            options,
            4,
            Duration::from_secs(600),
            Arc::new(NativeExecutor),
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
        if !root.is_dir()
            || !(1..=16).contains(&concurrency)
            || timeout.is_zero()
            || timeout > Duration::from_secs(3600)
            || options.instructions.len() > 128 * 1024
            || !(1..=256).contains(&options.max_turns)
            || config.model.trim().is_empty()
        {
            return Err("Invalid subagent workspace, model, limits or instructions".into());
        }
        if !matches!(config.provider, Provider::Custom | Provider::Deepseek) {
            return Err(
                "Native subagents currently support Custom and Deepseek API providers only".into(),
            );
        }
        Ok(Self {
            root,
            config,
            options,
            concurrency,
            timeout,
            executor,
            state: Arc::new(Mutex::new(State::default())),
        })
    }
    pub fn root_cwd(&self) -> &Path {
        &self.root
    }

    pub fn delegate(&self, request: Delegate, host: Arc<dyn Host>) -> Result<Snapshot, String> {
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
        if state.parent_operations > 0 {
            return Err("Parent workspace operation is still running".into());
        }
        let active: Vec<_> = state
            .sessions
            .values()
            .filter(|s| s.active.is_some())
            .collect();
        if active.len() >= self.concurrency {
            return Err("Subagent concurrency limit reached".into());
        }
        for s in active {
            if (cwd.starts_with(&s.cwd) || s.cwd.starts_with(&cwd))
                && (!request.read_only || !s.read_only)
            {
                return Err("An active task holds an overlapping workspace".into());
            }
        }
        let history = if let Some(session) = state.sessions.get(&request.session) {
            if session.active.is_some() {
                return Err("Session already running".into());
            }
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
        let snapshot = Snapshot {
            run_id: id,
            session: request.session.clone(),
            goal: request.goal.clone(),
            cwd: cwd.clone(),
            read_only: request.read_only,
            status: Status::Running,
            output: String::new(),
            output_truncated: false,
            last_tool: None,
            error: None,
        };
        let (cancel, cancelled) = oneshot::channel();
        let (changed, _) = watch::channel(Status::Running);
        state.sessions.insert(
            request.session.clone(),
            Session {
                goal: request.goal,
                cwd: cwd.clone(),
                read_only: request.read_only,
                history: history.clone(),
                active: Some(id),
            },
        );
        state.runs.insert(
            id,
            Record {
                snapshot: snapshot.clone(),
                cancel: Some(cancel),
                changed,
            },
        );
        drop(state);

        let task_host = Arc::new(ChildHost {
            inner: host,
            state: self.state.clone(),
            id,
        });
        let task = Task {
            config: self.config.clone(),
            history,
            prompt: request.prompt,
            cwd,
            host: task_host,
            options: self.options.clone(),
            read_only: request.read_only,
        };
        let executor = self.executor.clone();
        let state = self.state.clone();
        let timeout = self.timeout;
        runtime.spawn(async move {
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
            let mut state = state.lock().unwrap();
            let session = state.sessions.get_mut(&request.session).expect("registered child session");
            session.active = None;
            let (status, error) = match outcome {
                Ok(history) => { session.history = history; (Status::Completed, None) }
                Err((status, error)) => (status, Some(error)),
            };
            let record = state.runs.get_mut(&id).expect("registered child run");
            record.snapshot.status = status;
            record.snapshot.error = error;
            record.cancel = None;
            record.changed.send_replace(status);
        });
        Ok(snapshot)
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
        self.state
            .lock()
            .unwrap()
            .runs
            .values()
            .map(|r| r.snapshot.clone())
            .collect()
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
        if *changed.borrow_and_update() == Status::Running {
            let _ = tokio::time::timeout(wait.min(MAX_WAIT), changed.changed()).await;
        }
        self.snapshot(id)
    }
    /// Waits until the execution future has been dropped; a new run cannot steal its lease.
    pub async fn kill(&self, id: Uuid) -> Result<Snapshot, String> {
        {
            let mut state = self.state.lock().unwrap();
            let record = state.runs.get_mut(&id).ok_or("Unknown child run")?;
            if let Some(cancel) = record.cancel.take() {
                let _ = cancel.send(());
            }
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
        if state
            .sessions
            .values()
            .any(|s| s.active.is_some() && (write || !s.read_only))
        {
            return Err("Wait for active children before accessing their workspace".into());
        }
        state.parent_operations += 1;
        Ok(ParentLease(self.state.clone()))
    }
}
pub(crate) struct ParentLease(Arc<Mutex<State>>);
impl Drop for ParentLease {
    fn drop(&mut self) {
        self.0.lock().unwrap().parent_operations -= 1;
    }
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
    let mut end = text.len().min(MAX_OUTPUT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
struct ChildHost {
    inner: Arc<dyn Host>,
    state: Arc<Mutex<State>>,
    id: Uuid,
}
impl Host for ChildHost {
    fn shell(&self, command: &str, cwd: &Path) -> HostFuture<Result<String, String>> {
        self.inner.shell(command, cwd)
    }
    fn on_event(&self, event: AgentEvent) {
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
                AgentEvent::ToolStarted { operation, .. }
                | AgentEvent::ToolFinished { operation, .. } => {
                    record.snapshot.last_tool = Some(bounded(&operation));
                }
            }
        }
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
        "Delegate a self-contained task to a native child with independent history. Children cannot delegate. Default read-only; explicitly set read_only=false for edits. Keep session goal/path/access unchanged to resume. Poll status by run_id (wait_ms up to 30000), collect results before final answer; completed means execution ended, independently verify results. Wait before parent writes or starting overlapping write tasks. Sessions live only in this process; fork clones last successful history. No OS sandbox.".into()
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
    #[tokio::test]
    async fn parent_and_child_workspace_operations_share_leases() {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(
            SubagentManager::with_executor(
                dir.path(),
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
        );
        let task = Delegate {
            session: "child".into(),
            goal: "review".into(),
            prompt: "review".into(),
            path: None,
            read_only: true,
        };
        let guard = manager.parent_operation(true).unwrap();
        assert!(manager.delegate(task.clone(), Arc::new(Pending)).is_err());
        drop(guard);
        let child = manager.delegate(task, Arc::new(Pending)).unwrap();
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
}
