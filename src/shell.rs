//! Noninteractive shell execution for native hosts. This is not a sandbox.
//! Unix children run in a separate process group, killed on cancellation or exit.
//! On other platforms only the direct child is terminated; detached processes
//! require host-specific supervision.
use crate::unknown_effect::{UnknownEffectFence, UnknownReason, UnknownSideEffect};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
};

// The DES-17 output lifecycle modules live next to this file. They are declared
// here (not in `lib.rs`) so this integration stays inside the files this change
// owns; `crate::journal` and `crate::session` still resolve to the real modules.
//
// `workspace_activity` is the shared run/reference-writer/cleanup lock; declaring
// it as a sibling of `output_store` lets the latter reach it through `super::`
// without a second `#[path]` indirection.
#[path = "output_store.rs"]
pub mod output_store;
#[path = "workspace_activity.rs"]
pub mod workspace_activity;

use self::output_store::{CaptureLease, OutputEntry, OutputReference, OutputStore};

const STREAM_LIMIT: usize = 8 * 1024;
// A spilled stream needs a locator, not another full inline-sized result.
// Keep room for tools and the next request in the default context window.
const EXTERNAL_PREVIEW_BYTES: usize = 2 * 1024;

/// Captured result of a supervised external command.
#[derive(Debug)]
pub struct CommandOutput {
    pub status: std::process::ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    /// Concatenate stdout and stderr with a single separating newline.
    pub fn combined(&self) -> String {
        let mut combined = String::new();
        if !self.stdout.is_empty() {
            combined.push_str(&self.stdout);
        }
        if !self.stderr.is_empty() {
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str(&self.stderr);
        }
        combined
    }
}

pub(crate) struct Process(
    Option<Child>,
    Option<u32>,
    Option<crate::task_lifecycle::TaskWorkerGuard>,
);

impl Process {
    /// The child must have been spawned in its own process group on Unix.
    pub(crate) fn new(child: Child) -> Self {
        let pid = child.id();
        Self(Some(child), pid, None)
    }

    /// Attach a real worker guard that stays alive until the direct child is
    /// reaped. Sending a kill is not a reap: the guard moves into the async
    /// reaper, so the bound task cannot settle before `wait()` returns.
    pub(crate) fn with_worker(
        child: Child,
        worker: Option<crate::task_lifecycle::TaskWorkerGuard>,
    ) -> Self {
        let pid = child.id();
        Self(Some(child), pid, worker)
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.1 {
            // SAFETY: the child was spawned in its own process group with this ID.
            // A group may already have exited, in which case kill returns ESRCH.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        if let Some(mut child) = self.0.take() {
            let _ = child.start_kill();
            // The worker guard travels with the reaper: the task is not settled
            // until the direct child is actually reaped, not merely signalled.
            let worker = self.2.take();
            // Keep polling wait even when the RPC runtime is idle after abort.
            // kill_on_drop remains the fallback during runtime shutdown.
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = child.wait().await;
                    drop(worker);
                });
            } else {
                drop(worker);
            }
        } else {
            drop(self.2.take());
        }
    }
}

/// Complete output files have an explicit disk bound. Exceeding it fails the
/// tool and terminates its process rather than reporting a truncated success.
pub const MAX_OUTPUT_FILE_BYTES: usize = 8 * 1024 * 1024;

/// Shared, lazily initialized capture state for one supervised command.
///
/// `.mio/outputs`, the versioned output index and the shared cross-process
/// capture lease are created only after a stream crosses the 8 KiB inline
/// limit, so a small output never touches `.mio`. stdout and stderr share one
/// session, which holds the lease until the whole dual-stream capture commits
/// or is dropped. The lease also keeps a concurrent cleanup from deleting a
/// file between `persist` and index registration.
struct PublishedFile {
    path: PathBuf,
    bytes: u64,
    hash: String,
}

struct CaptureState {
    store: Option<OutputStore>,
    lease: Option<CaptureLease>,
    /// Files published but not yet committed, in publication order.
    published: Vec<PublishedFile>,
    /// Model-visible references for registered files, held until `commit`.
    /// A failed or cancelled commit never publishes them.
    pending_references: Vec<OutputReference>,
    committed: bool,
}

struct CaptureSession {
    cwd: PathBuf,
    /// The per-execution collector bound by the real tool wrapper, if any. A
    /// direct, engine-less `shell::execute` has none, so it keeps the original
    /// `String` plus index behaviour and never publishes a model reference.
    references: Option<crate::tool_identity::OutputReferenceCollector>,
    state: std::sync::Mutex<CaptureState>,
}

impl CaptureSession {
    fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            references: crate::tool_identity::current_output_reference_collector(),
            state: std::sync::Mutex::new(CaptureState {
                store: None,
                lease: None,
                published: Vec::new(),
                pending_references: Vec::new(),
                committed: false,
            }),
        }
    }

    /// Create the output store and the shared capture lease on first spill,
    /// then return the real output directory. This is synchronous: its guard is
    /// never held across an await, so the capture futures stay `Send`.
    fn ensure_lease(&self) -> Result<PathBuf, String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.lease.is_none() {
            let store = OutputStore::open(self.cwd.as_path()).map_err(|error| error.to_string())?;
            let lease = store.begin_capture().map_err(|error| error.to_string())?;
            // Reject a corrupt index before publishing a new artifact.
            store.load_index().map_err(|error| error.to_string())?;
            state.store = Some(store);
            state.lease = Some(lease);
        }
        state
            .store
            .as_ref()
            .map(OutputStore::output_dir)
            .ok_or_else(|| "output store was not initialized".to_string())
    }

    /// Enroll a just-published file for rollback *before* registering it, then
    /// register its real path, byte count, hash, stream and creation time.
    ///
    /// The `call_id` is the model's own tool-call id read from the real identity
    /// bound to this task by the tool wrapper. A direct call with no engine
    /// scope has no identity, so the id stays JSON `null`, never fabricated.
    fn register_published(
        &self,
        path: &Path,
        stream: &str,
        total: u64,
        hash: String,
    ) -> Result<(), String> {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("output file has no usable name")?;
        let relative = format!(".mio/outputs/{name}");
        let call_id = crate::tool_identity::current_tool_call_id();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Rollback enrollment first: if registration fails from here on, `Drop`
        // still removes the file that would otherwise be orphaned.
        state.published.push(PublishedFile {
            path: path.to_path_buf(),
            bytes: total,
            hash: hash.clone(),
        });
        let lease = state
            .lease
            .as_ref()
            .ok_or("capture lease was not initialized")?;
        let store = state
            .store
            .as_ref()
            .ok_or("output store was not initialized")?;
        let stored = store
            .register(
                lease,
                OutputEntry {
                    path: relative,
                    bytes: Some(total),
                    hash: Some(hash),
                    stream: stream.to_owned(),
                    created: 0,
                    call_id,
                },
            )
            .map_err(|error| error.to_string())?;
        // The typed reference is projected from the registered entry, so its
        // byte count and SHA-256 are the ones read back from disk. It stays
        // pending until the whole capture transaction commits.
        let reference = OutputReference::from_entry(&stored).map_err(|error| error.to_string())?;
        state.pending_references.push(reference);
        Ok(())
    }

    /// Commit the whole capture: publish the pending references atomically to
    /// the per-execution collector, then mark the transaction committed.
    ///
    /// Capacity or metadata failure returns `Err` and leaves `committed`
    /// false, so `Drop` rolls the published files and index entries back: a
    /// reference is never silently dropped and never published for a failed
    /// or cancelled capture.
    fn commit(&self) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.committed {
            return Ok(());
        }
        let pending = std::mem::take(&mut state.pending_references);
        if let Some(collector) = self.references.as_ref() {
            collector
                .try_publish(pending)
                .map_err(|error| error.to_string())?;
        }
        state.committed = true;
        Ok(())
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.committed {
            return;
        }
        let published = std::mem::take(&mut state.published);
        if published.is_empty() {
            return;
        }
        let store = state.store.as_ref();
        let lease = state.lease.as_ref();
        for published in published {
            let (Some(store), Some(lease)) = (store, lease) else {
                continue;
            };
            let Some(name) = published.path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            // A mismatch or unreadable index is uncertainty, never permission
            // to delete. Only an explicitly absent entry takes the fallback.
            if matches!(
                store.cancel(lease, &format!(".mio/outputs/{name}")),
                Ok(false)
            ) {
                rollback_unregistered(&published);
            }
        }
    }
}

/// A registration failure may leave a new file outside the index. Remove it
/// only if its bytes still match the stream this capture actually produced.
fn rollback_unregistered(published: &PublishedFile) {
    use std::io::Read;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let Ok(mut file) = options.open(&published.path) else {
        return;
    };
    let Ok(metadata) = file.metadata() else {
        return;
    };
    if !metadata.is_file() || metadata.len() != published.bytes {
        return;
    }
    let mut digest = Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0u8; 8192];
    loop {
        let Ok(read) = file.read(&mut buffer) else {
            return;
        };
        if read == 0 {
            break;
        }
        count = count.saturating_add(read as u64);
        if count > MAX_OUTPUT_FILE_BYTES as u64 {
            return;
        }
        digest.update(&buffer[..read]);
    }
    if count == published.bytes && format!("{:x}", digest.finalize()) == published.hash {
        let _ = std::fs::remove_file(&published.path);
    }
}

async fn capture(
    reader: impl AsyncRead + Unpin,
    cwd: &Path,
    stream: &str,
) -> Result<String, String> {
    let session = CaptureSession::new(cwd);
    let output = capture_transactional(reader, &session, stream).await?;
    session.commit()?;
    Ok(output)
}

async fn capture_transactional(
    mut reader: impl AsyncRead + Unpin,
    session: &CaptureSession,
    stream: &str,
) -> Result<String, String> {
    let mut output = Vec::new();
    let mut total = 0usize;
    let mut digest = Sha256::new();
    let mut spool: Option<(tempfile::NamedTempFile, tokio::fs::File)> = None;
    let mut buffer = [0; 8192];
    loop {
        let count = reader.read(&mut buffer).await.map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        total = total.saturating_add(count);
        if total > MAX_OUTPUT_FILE_BYTES {
            return Err(format!(
                "{stream} exceeds complete output file limit ({MAX_OUTPUT_FILE_BYTES} bytes); process terminated"
            ));
        }
        digest.update(&buffer[..count]);
        if total > STREAM_LIMIT && spool.is_none() {
            // First spill: create `.mio/outputs`, the output index and the
            // shared capture lease together. A small output never reaches this
            // path, so it never creates `.mio`.
            let directory = session.ensure_lease()?;
            let temporary =
                tempfile::NamedTempFile::new_in(directory).map_err(|e| e.to_string())?;
            let mut file =
                tokio::fs::File::from_std(temporary.reopen().map_err(|e| e.to_string())?);
            file.write_all(&output).await.map_err(|e| e.to_string())?;
            spool = Some((temporary, file));
        }
        if let Some((_, file)) = spool.as_mut() {
            file.write_all(&buffer[..count])
                .await
                .map_err(|e| e.to_string())?;
        }
        let keep = count.min(STREAM_LIMIT.saturating_sub(output.len()));
        output.extend_from_slice(&buffer[..keep]);
    }
    let mut output = String::from_utf8_lossy(&output).into_owned();
    if let Some((temporary, mut file)) = spool {
        let mut end = output.len().min(EXTERNAL_PREVIEW_BYTES);
        while !output.is_char_boundary(end) {
            end -= 1;
        }
        output.truncate(end);
        file.flush().await.map_err(|e| e.to_string())?;
        file.sync_all().await.map_err(|e| e.to_string())?;
        drop(file);
        let name = format!("{}.{}", uuid::Uuid::new_v4(), stream);
        let final_path = temporary
            .path()
            .parent()
            .ok_or("output file has no parent")?
            .join(&name);
        temporary
            .persist_noclobber(&final_path)
            .map_err(|e| e.to_string())?;
        session.register_published(
            &final_path,
            stream,
            total as u64,
            format!("{:x}", digest.finalize()),
        )?;
        output.push_str(&format!("\n[TRUNCATED: inline output exceeds 8 KiB; complete output: .mio/outputs/{name}; {total} bytes]"));
    }
    Ok(output)
}

/// Preserve a large external tool response and return a bounded preview plus
/// a workspace-relative reference. Small responses do not touch the disk.
pub async fn externalize_output(text: &str, cwd: &Path) -> Result<String, String> {
    capture(text.as_bytes(), cwd, "tool").await
}

/// Which supervision step lost a confirmable result *after* the process was
/// started. Every variant means the effect may already have happened, so it is
/// typed as an unknown side effect rather than a confirmed failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupervisionLoss {
    /// The supervision timeout fired while the process may still have run.
    Timeout,
    /// Waiting for the child's exit status failed; the exit is not confirmable.
    Wait,
    /// Reading a captured stream failed, or it crossed the complete-output
    /// bound and the process was terminated with an incomplete result.
    Capture,
}

impl SupervisionLoss {
    /// Stable machine-readable label, never parsed out of an error string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Wait => "wait",
            Self::Capture => "capture",
        }
    }

    /// The typed unknown reason this loss maps to.
    pub const fn unknown_reason(self) -> UnknownReason {
        match self {
            Self::Timeout => UnknownReason::NativeTimeout,
            Self::Wait => UnknownReason::NativeWait,
            Self::Capture => UnknownReason::NativeCapture,
        }
    }
}

/// A supervised process failure, typed by whether the process could have run.
///
/// The distinction is a fact the supervisor observed, never something inferred
/// from a human-readable error string: [`Self::PreSpawn`] means the command was
/// never started (a rejected backend or a failed spawn), so its result is a
/// known failure and no unknown fence is touched. [`Self::PostSpawn`] means a
/// process was started and its result cannot be confirmed, so it must be
/// reported as `Unknown`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SupervisionError {
    /// The command was never started; nothing ran.
    PreSpawn(String),
    /// The command was started and its result is not confirmable.
    PostSpawn {
        /// Which supervision step lost the result.
        loss: SupervisionLoss,
        /// A bounded human-readable detail, never used for classification.
        detail: String,
    },
}

impl SupervisionError {
    /// The legacy `String` rendering of this failure.
    pub fn into_message(self) -> String {
        match self {
            Self::PreSpawn(detail) | Self::PostSpawn { detail, .. } => detail,
        }
    }

    /// Whether the process may already have run.
    pub fn is_post_spawn(&self) -> bool {
        matches!(self, Self::PostSpawn { .. })
    }

    /// The typed loss, when the process may already have run.
    pub fn loss(&self) -> Option<SupervisionLoss> {
        match self {
            Self::PreSpawn(_) => None,
            Self::PostSpawn { loss, .. } => Some(*loss),
        }
    }
}

impl std::fmt::Display for SupervisionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PreSpawn(detail) => formatter.write_str(detail),
            Self::PostSpawn { loss, detail } => {
                write!(
                    formatter,
                    "supervision lost the result ({loss:?}), so the effect is unknown: {detail}"
                )
            }
        }
    }
}

impl std::error::Error for SupervisionError {}

/// A bounded, non-reversible summary of a shell command for durable unknown
/// evidence. The raw command is never copied here; the canonical lifecycle
/// record keeps only the exact `params_sha256` plus this summary, while the
/// real command stays available to the approval/execution UI.
pub fn native_shell_target(command: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(command.as_bytes());
    let hex = format!("{:x}", digest.finalize());
    format!(
        "shell command (sha256 {}; {} chars)",
        &hex[..16],
        command.chars().count()
    )
}

#[cfg(unix)]
fn configure_process_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.as_std_mut().process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_cmd: &mut Command) {}

/// Run an already-configured [`Command`] with bounded output capture (legacy
/// `String` API).
///
/// The child is placed in its own process group on Unix, killed on drop
/// (including when the returned future is cancelled), and both streams are
/// shown inline up to 8 KiB each; larger streams are preserved under
/// `.mio/outputs/` (up to 8 MiB per stream). Callers are expected to set `stdin`, `current_dir`
/// and environment; `stdout`/`stderr` are forced to pipes here. Callers apply
/// their own timeout around the returned future so cancellation is propagated
/// while the process guard is still alive.
pub async fn run_supervised(cmd: &mut Command) -> Result<CommandOutput, String> {
    run_supervised_typed(cmd, None)
        .await
        .map_err(SupervisionError::into_message)
}

/// Run under an explicit supervision timeout, sharing the single production
/// supervision path.
///
/// The timeout is not a new user/eval budget: production always passes
/// [`SHELL_TIMEOUT`], while a private test uses the same code path with a short
/// duration to drive a real subprocess sentinel into a real timeout.
pub async fn run_supervised_with_timeout(
    cmd: &mut Command,
    timeout: Duration,
) -> Result<CommandOutput, SupervisionError> {
    run_supervised_typed(cmd, Some(timeout)).await
}

/// The typed supervision entry point. `timeout: None` keeps the caller's own
/// timeout around the returned future; `Some` applies it here so a timeout is
/// provably after spawn.
pub async fn run_supervised_typed(
    cmd: &mut Command,
    timeout: Option<Duration>,
) -> Result<CommandOutput, SupervisionError> {
    configure_process_group(cmd);
    run_supervised_inner(cmd, timeout).await
}

/// Wrap a stream-capture error as a typed post-spawn loss.
async fn capture_lossy(
    reader: impl AsyncRead + Unpin,
    session: &CaptureSession,
    stream: &str,
) -> Result<String, (SupervisionLoss, String)> {
    capture_transactional(reader, session, stream)
        .await
        .map_err(|error| (SupervisionLoss::Capture, error))
}

async fn run_supervised_inner(
    cmd: &mut Command,
    timeout: Option<Duration>,
) -> Result<CommandOutput, SupervisionError> {
    let cwd = cmd
        .as_std()
        .get_current_dir()
        .map(Path::to_path_buf)
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)
        .map_err(|e| SupervisionError::PreSpawn(e.to_string()))?;
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Acquire the worker guard before spawning anything. Every refusal -- a
    // cancellation request, a terminal or still-queued phase, a latched
    // persistence failure, or an untracked task -- must stop the spawn instead
    // of being silently dropped: otherwise a task could settle with
    // `workers == 0` while a process is created immediately afterwards. The
    // guard is held by `Process` until the async reaper actually reaps the
    // direct child, and is released on a spawn failure.
    let worker = match crate::task_lifecycle::current_task_handle() {
        Some(handle) => Some(handle.begin_worker().map_err(|error| {
            let detail = match error {
                crate::task_lifecycle::TaskLifecycleError::CancelRequested { .. } => {
                    "task cancellation requested; refusing to start a new shell process".to_string()
                }
                other => {
                    format!(
                        "task worker admission failed; refusing to start a shell process: {other}"
                    )
                }
            };
            SupervisionError::PreSpawn(detail)
        })?),
        None => None,
    };
    // A failed spawn is a known failure: nothing ran, so no unknown fence is
    // touched and the legacy message is preserved verbatim.
    let child = cmd
        .spawn()
        .map_err(|e| SupervisionError::PreSpawn(format!("Cannot start process: {e}")))?;
    let mut process = if worker.is_some() {
        Process::with_worker(child, worker)
    } else {
        Process::new(child)
    };
    let child = process.0.as_mut().expect("owned child");
    // The process is already running: a missing pipe can no longer be a
    // pre-spawn failure, so it is a lost capture.
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| SupervisionError::PostSpawn {
            loss: SupervisionLoss::Capture,
            detail: "Missing stdout pipe".to_string(),
        })?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| SupervisionError::PostSpawn {
            loss: SupervisionLoss::Capture,
            detail: "Missing stderr pipe".to_string(),
        })?;
    let session = CaptureSession::new(&cwd);
    let supervise = async {
        let (status, stdout, stderr) = tokio::try_join!(
            async {
                child
                    .wait()
                    .await
                    .map_err(|e| (SupervisionLoss::Wait, e.to_string()))
            },
            capture_lossy(stdout, &session, "stdout"),
            capture_lossy(stderr, &session, "stderr")
        )?;
        Ok::<_, (SupervisionLoss, String)>((status, stdout, stderr))
    };
    let (status, stdout, stderr) = match timeout {
        Some(timeout) => match tokio::time::timeout(timeout, supervise).await {
            Ok(result) => result,
            Err(_) => {
                // The future is dropped here, so `Process::drop` kills the
                // group and reaps as before; only the classification is new.
                return Err(SupervisionError::PostSpawn {
                    loss: SupervisionLoss::Timeout,
                    detail: format!("timed out after {} seconds", timeout.as_secs()),
                });
            }
        },
        None => supervise.await,
    }
    .map_err(|(loss, detail)| SupervisionError::PostSpawn { loss, detail })?;
    // Both streams landed and were registered; the published files stay. On
    // any error above the session is dropped, rolling back uncommitted files
    // and their index entries without touching existing references.
    session
        .commit()
        .map_err(|detail| SupervisionError::PostSpawn {
            loss: SupervisionLoss::Capture,
            detail,
        })?;
    Ok(CommandOutput {
        status,
        stdout,
        stderr,
    })
}

/// The production shell supervision budget. Tests share this exact path with a
/// short private duration instead of a separate, weaker code path.
pub const SHELL_TIMEOUT: Duration = Duration::from_secs(120);

/// Publish a typed native supervision unknown for a real (possibly) executed
/// command. No-op for a direct, engine-less call: absence of identity stays
/// absence rather than being filled with a guess.
fn publish_shell_supervision_unknown(
    command: &str,
    loss: SupervisionLoss,
) -> Option<Arc<UnknownEffectFence>> {
    let effect = UnknownSideEffect::new(
        "shell",
        native_shell_target(command),
        loss.unknown_reason(),
        crate::tool_identity::current_tool_call_id(),
    );
    crate::unknown_effect::publish_supervision_unknown(effect)
}

/// Render the legacy shell result string and publish a typed unknown when the
/// command was started but its result is not confirmable.
async fn run_shell_command(
    command: &str,
    cmd: &mut Command,
    timeout: Duration,
) -> Result<String, String> {
    let output = match run_supervised_with_timeout(cmd, timeout).await {
        Ok(output) => output,
        Err(error) => {
            // Only an after-spawn loss is unknown; a pre-spawn failure is a
            // known failure and leaves the run fence untouched.
            if let Some(loss) = error.loss() {
                publish_shell_supervision_unknown(command, loss);
            }
            let message = match error.loss() {
                Some(SupervisionLoss::Timeout) => {
                    format!("Shell timed out after {} seconds", timeout.as_secs())
                }
                _ => error.into_message(),
            };
            return Err(message);
        }
    };
    let result = format!(
        "{}\nstdout:\n{}\nstderr:\n{}",
        output.status, output.stdout, output.stderr
    );
    if output.status.success() {
        Ok(result)
    } else {
        Err(result)
    }
}

/// Execute with null stdin, bounded captured output and the production
/// [`SHELL_TIMEOUT`] budget.
pub async fn execute(command: &str, cwd: &Path) -> Result<String, String> {
    execute_with_timeout(command, cwd, SHELL_TIMEOUT).await
}

/// Execute with an explicit private supervision timeout on the same path as
/// production. The timeout is a supervision detail, not a new user/eval budget:
/// production always passes [`SHELL_TIMEOUT`].
pub async fn execute_with_timeout(
    command: &str,
    cwd: &Path,
    timeout: Duration,
) -> Result<String, String> {
    let mut cmd = shell_command(command);
    cmd.current_dir(cwd).stdin(Stdio::null());
    run_shell_command(command, &mut cmd, timeout).await
}

/// Execute using an explicitly requested backend, without silently falling
/// back when the backend or policy is unavailable. This covers this shell
/// call only, not arbitrary host tools or external MCP servers.
pub async fn execute_with_policy(
    command: &str,
    cwd: &Path,
    policy: &crate::execution_backend::ExecutionPolicy,
) -> Result<String, String> {
    execute_with_policy_and_timeout(command, cwd, policy, SHELL_TIMEOUT).await
}

/// [`execute_with_policy`] with an explicit private supervision timeout.
pub async fn execute_with_policy_and_timeout(
    command: &str,
    cwd: &Path,
    policy: &crate::execution_backend::ExecutionPolicy,
    timeout: Duration,
) -> Result<String, String> {
    use crate::execution_backend::{ExecutionPolicy, SupervisedCommand};
    if matches!(policy, ExecutionPolicy::Unrestricted) {
        return execute_with_timeout(command, cwd, timeout).await;
    }
    // A sandboxed shell never inherits the supervisor environment: it gets the
    // policy's own environment (SandboxDefaults for the CLI). `command` fails
    // closed when the backend is unavailable, so there is no silent fallback.
    #[cfg(unix)]
    let request = SupervisedCommand::new("/bin/sh", cwd).args(["-c", command]);
    #[cfg(not(unix))]
    let request = SupervisedCommand::new("cmd.exe", cwd).args(["/D", "/S", "/C", command]);
    // A backend refusal is pre-spawn: nothing ran, so it stays a known error.
    let native = policy
        .command(&request)
        .map_err(|error| error.to_string())?;
    let mut cmd = Command::from(native);
    cmd.stdin(Stdio::null());
    run_shell_command(command, &mut cmd, timeout).await
}

#[cfg(unix)]
fn shell_command(command: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd
}

#[cfg(not(unix))]
fn shell_command(command: &str) -> Command {
    let mut cmd = Command::new("cmd");
    cmd.args(["/D", "/S", "/C", command]);
    cmd
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn rollback_preserves_a_registered_output_changed_after_capture() {
        let root = tempfile::tempdir().unwrap();
        let session = CaptureSession::new(root.path());
        let directory = session.ensure_lease().unwrap();
        let path = directory.join(format!("{}.stdout", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"before").unwrap();
        session
            .register_published(
                &path,
                "stdout",
                6,
                format!("{:x}", Sha256::digest(b"before")),
            )
            .unwrap();
        // Same byte count, different content: size checks alone cannot protect it.
        std::fs::write(&path, b"manual").unwrap();
        drop(session);
        assert_eq!(std::fs::read(&path).unwrap(), b"manual");
        let store = OutputStore::open(root.path().to_path_buf()).unwrap();
        assert_eq!(store.load_index().unwrap().entries.len(), 1);
    }

    #[tokio::test]
    async fn corrupt_output_index_refuses_capture_before_publishing_a_file() {
        let root = tempfile::tempdir().unwrap();
        let store = OutputStore::open(root.path().to_path_buf()).unwrap();
        std::fs::write(store.index_path(), b"corrupt evidence").unwrap();
        assert!(
            externalize_output(&"x".repeat(40 * 1024), root.path())
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_dir(store.output_dir()).unwrap().count(), 0);
        assert_eq!(
            std::fs::read(store.index_path()).unwrap(),
            b"corrupt evidence"
        );
    }

    #[tokio::test]
    async fn invalid_sandbox_policy_never_falls_back_to_running_the_command() {
        use crate::execution_backend::{BubblewrapPolicy, ExecutionPolicy, FilesystemPolicy};
        let root = tempfile::tempdir().unwrap();
        let policy = ExecutionPolicy::Bubblewrap(BubblewrapPolicy {
            filesystem: FilesystemPolicy::ExplicitMounts {
                read_only: vec![PathBuf::from("/")],
            },
            ..BubblewrapPolicy::default()
        });
        assert!(
            execute_with_policy("touch forbidden-fallback", root.path(), &policy)
                .await
                .is_err()
        );
        assert!(!root.path().join("forbidden-fallback").exists());
    }

    #[tokio::test]
    async fn typed_supervision_separates_pre_spawn_from_after_spawn() {
        let mut command = Command::new("/mio-no-such-shell-binary");
        let error = run_supervised_with_timeout(&mut command, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(error.loss(), None);
        assert!(matches!(error, SupervisionError::PreSpawn(_)));

        let mut command = shell_command("sleep 30");
        let error = run_supervised_with_timeout(&mut command, Duration::from_millis(200))
            .await
            .unwrap_err();
        assert_eq!(error.loss(), Some(SupervisionLoss::Timeout));
        assert!(error.is_post_spawn());
    }

    /// Every after-spawn loss maps to its own typed reason, so the canonical
    /// evidence never has to parse a message. A real subprocess can drive
    /// `Timeout` and `Capture` (the tests here and in `native_supervision.rs`
    /// do); a `Wait` loss means `wait(2)` itself failed, which no portable,
    /// deterministic real-process setup produces (a process reaped by a third
    /// party is ECHILD, not something the supervisor can arrange), so its
    /// mapping and stable label are asserted directly here.
    #[test]
    fn every_post_spawn_loss_maps_to_its_own_typed_reason() {
        assert_eq!(
            SupervisionLoss::Timeout.unknown_reason(),
            UnknownReason::NativeTimeout
        );
        assert_eq!(
            SupervisionLoss::Wait.unknown_reason(),
            UnknownReason::NativeWait
        );
        assert_eq!(
            SupervisionLoss::Capture.unknown_reason(),
            UnknownReason::NativeCapture
        );
        assert_eq!(SupervisionLoss::Timeout.as_str(), "timeout");
        assert_eq!(SupervisionLoss::Wait.as_str(), "wait");
        assert_eq!(SupervisionLoss::Capture.as_str(), "capture");
    }

    #[test]
    fn native_shell_target_never_copies_the_command() {
        let target = native_shell_target("printf MIO-SYNTHETIC-SHELL-CANARY");
        assert!(!target.contains("MIO-SYNTHETIC-SHELL-CANARY"));
        assert!(target.contains("sha256"));
        assert!(target.contains("chars"));
    }

    #[tokio::test]
    async fn captures_both_streams_failure_and_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let output = execute("printf '你好'; printf 'problem' >&2; exit 7", dir.path())
            .await
            .unwrap_err();
        assert!(output.contains("你好") && output.contains("problem") && output.contains('7'));
        let output = execute("head -c 70000 /dev/zero", dir.path())
            .await
            .unwrap();
        assert!(output.contains("TRUNCATED"));
        assert!(output.len() < 34000);
    }

    #[tokio::test]
    async fn cancellation_kills_background_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let task_dir = dir.path().to_path_buf();
        let task = tokio::spawn(async move {
            execute("sleep 30 & echo $! > child.pid; wait", &task_dir).await
        });
        let pid_file = dir.path().join("child.pid");
        let pid = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(text) = tokio::fs::read_to_string(&pid_file).await
                    && let Ok(pid) = text.trim().parse::<i32>()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        // Linux can briefly retain a killed descendant as a zombie until init reaps it.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                // SAFETY: signal 0 only checks existence of the test-created process.
                if unsafe { libc::kill(pid, 0) } != 0 {
                    break;
                }
                #[cfg(target_os = "linux")]
                if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    && stat
                        .split_once(") ")
                        .is_some_and(|(_, s)| s.starts_with('Z'))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("background child survived cancellation");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_process_reaps_the_direct_child() {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new("sleep");
        command.arg("60").kill_on_drop(true);
        command.as_std_mut().process_group(0);
        let child = command.spawn().unwrap();
        let pid = child.id().unwrap() as libc::pid_t;
        let process = Process::new(child);
        drop(process);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                // Signal zero does not reap the child. ESRCH proves that it
                // is gone, rather than accepting a zombie as a successful reap.
                let result = unsafe { libc::kill(pid, 0) };
                if result == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("direct child was not reaped");
        let mut status = 0;
        // The supervisor/Tokio reaper must already have collected this child.
        assert_eq!(
            unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[tokio::test]
    async fn complete_large_output_is_preserved_with_a_readable_reference() {
        let directory = tempfile::tempdir().unwrap();
        let original = "原始输出\n".repeat(20_000);
        let preview = capture(original.as_bytes(), directory.path(), "stdout")
            .await
            .unwrap();
        assert!(preview.len() < 40 * 1024);
        let stored = stored_outputs(directory.path());
        assert_eq!(stored.len(), 1);
        assert_eq!(std::fs::read(&stored[0]).unwrap(), original.as_bytes());
        assert!(preview.contains(&stored[0].file_name().unwrap().to_string_lossy().to_string()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&stored[0]).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn small_output_does_not_create_artifact_directories() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            capture(&b"small"[..], directory.path(), "stdout")
                .await
                .unwrap(),
            "small"
        );
        assert!(!directory.path().join(".mio").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn output_storage_rejects_linked_directories() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), directory.path().join(".mio")).unwrap();
        let text = "x".repeat(STREAM_LIMIT + 1);
        assert!(
            capture(text.as_bytes(), directory.path(), "stdout")
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    /// Artifact files only. `.mio/outputs` is reserved for products, so the
    /// index and lock files live under `.mio/output-index` and never show up
    /// here; these assertions stay about products, not directory bookkeeping.
    fn stored_outputs(cwd: &Path) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(cwd.join(".mio/outputs"))
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().path())
                    .filter(|path| path.is_file())
                    .collect()
            })
            .unwrap_or_default();
        paths.sort();
        paths
    }

    fn artifact_count(outputs: &Path) -> usize {
        std::fs::read_dir(outputs)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| entry.path().is_file())
                    .count()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn complete_process_output_is_stored_byte_for_byte() {
        let directory = tempfile::tempdir().unwrap();
        let size = STREAM_LIMIT * 3;
        let mut command = shell_command(&format!("head -c {size} /dev/zero | tr '\\0' 'a'"));
        command.current_dir(directory.path()).stdin(Stdio::null());
        let output = run_supervised(&mut command).await.unwrap();
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        assert!(
            output
                .stdout
                .contains("[TRUNCATED: inline output exceeds 8 KiB")
        );
        // The reported byte count must describe the stored artifact, not the preview.
        assert!(output.stdout.contains(&format!("{size} bytes]")));
        let stored = stored_outputs(directory.path());
        assert_eq!(stored.len(), 1, "empty stderr must not spill a file");
        assert_eq!(
            stored[0].extension().and_then(|e| e.to_str()),
            Some("stdout")
        );
        let name = stored[0].file_name().unwrap().to_string_lossy().to_string();
        assert!(output.stdout.contains(&name));
        assert_eq!(std::fs::read(&stored[0]).unwrap(), vec![b'a'; size]);
    }

    #[tokio::test]
    async fn oversized_output_fails_without_leaving_a_stored_file() {
        let directory = tempfile::tempdir().unwrap();
        let oversized = vec![b'a'; MAX_OUTPUT_FILE_BYTES + 1];
        let error = capture(&oversized[..], directory.path(), "stdout")
            .await
            .unwrap_err();
        assert!(
            error.contains("exceeds complete output file limit"),
            "unexpected error: {error}"
        );
        // The first chunks cross the inline limit, so the spool existed before
        // the hard limit was hit and must be removed when the capture fails.
        let outputs = directory.path().join(".mio/outputs");
        assert!(outputs.is_dir(), "spill directory was never created");
        assert_eq!(
            artifact_count(&outputs),
            0,
            "a failed capture must not leave a stored file"
        );
    }

    #[tokio::test]
    async fn cancelling_a_spilling_capture_removes_the_temporary_file() {
        let directory = tempfile::tempdir().unwrap();
        let (mut writer, reader) = tokio::io::duplex(128 * 1024);
        // Fits in the duplex buffer, so the reader spills and then blocks for more.
        writer
            .write_all(&vec![b'x'; STREAM_LIMIT * 3])
            .await
            .unwrap();
        let cwd = directory.path().to_path_buf();
        let task = tokio::spawn(async move { capture(reader, &cwd, "stdout").await });
        let outputs = directory.path().join(".mio/outputs");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if std::fs::read_dir(&outputs)
                    .map(|mut entries| entries.next().is_some())
                    .unwrap_or(false)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("spill file was never created");
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(
            artifact_count(&outputs),
            0,
            "a cancelled capture must remove its temporary file"
        );
        drop(writer);
    }

    #[tokio::test]
    async fn stdout_and_stderr_spill_to_independent_files() {
        let directory = tempfile::tempdir().unwrap();
        let stdout = "o".repeat(STREAM_LIMIT + 1);
        let stderr = "e".repeat(STREAM_LIMIT + 1);
        let stdout_preview = capture(stdout.as_bytes(), directory.path(), "stdout")
            .await
            .unwrap();
        let stderr_preview = capture(stderr.as_bytes(), directory.path(), "stderr")
            .await
            .unwrap();
        let stored = stored_outputs(directory.path());
        assert_eq!(stored.len(), 2, "each stream owns its own artifact");
        let named = |stream: &str| {
            stored
                .iter()
                .find(|path| path.extension().and_then(|e| e.to_str()) == Some(stream))
                .unwrap()
                .clone()
        };
        let stdout_file = named("stdout");
        let stderr_file = named("stderr");
        assert_ne!(stdout_file, stderr_file);
        assert_eq!(std::fs::read(&stdout_file).unwrap(), stdout.as_bytes());
        assert_eq!(std::fs::read(&stderr_file).unwrap(), stderr.as_bytes());
        let stdout_name = stdout_file
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let stderr_name = stderr_file
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(stdout_preview.contains(&stdout_name));
        assert!(!stdout_preview.contains(&stderr_name));
        assert!(stderr_preview.contains(&stderr_name));
        assert!(!stderr_preview.contains(&stdout_name));
    }

    #[tokio::test]
    async fn failed_capture_cleans_an_already_completed_sibling_stream() {
        let directory = tempfile::tempdir().unwrap();
        let mut command = Command::new("sh");
        command.current_dir(directory.path()).args([
            "-c",
            "head -c 100000 /dev/zero >&2; exec 2>&-; sleep 0.1; head -c 8388609 /dev/zero",
        ]);
        assert!(
            run_supervised(&mut command)
                .await
                .unwrap_err()
                .contains("limit")
        );
        assert_eq!(artifact_count(&directory.path().join(".mio/outputs")), 0);
    }
}
