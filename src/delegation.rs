//! Adaptation layer for native delegation operations.
//!
//! Provides serde-compatible request/response data structures and helper methods
//! over [`SubagentManager`] without introducing external agent frameworks or MCP runtimes.

use crate::{
    Host,
    subagent::{
        Delegate, DeniedToolCall, Snapshot, Status, SubagentManager, SubagentResult, ToolEvidence,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use uuid::Uuid;

/// Delegation execution mode: asynchronous return vs synchronous waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DelegationMode {
    #[default]
    Async,
    Sync,
}

/// Execution constraints.
///
/// Only whitelist of tools is supported in this round.
/// Unsupported options such as `noSkills` or `thinking` are explicitly rejected.
/// Security instructions such as AGENTS.md cannot be bypassed by `noSkills`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DelegationConstraints {
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub no_skills: Option<bool>,
    #[serde(default)]
    pub thinking: Option<serde_json::Value>,
}

/// Request parameters for delegating a subagent task.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegateParams {
    pub session: String,
    pub goal: String,
    pub prompt: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub mode: DelegationMode,
    #[serde(default)]
    pub run_timeout_ms: Option<u64>,
    #[serde(default)]
    pub wait_timeout_ms: Option<u64>,
    #[serde(default)]
    pub read_only: Option<bool>,
    #[serde(default)]
    pub constraints: Option<DelegationConstraints>,
}

/// Request parameters for querying status of a run.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusParams {
    pub run_id: Uuid,
    #[serde(default)]
    pub wait_timeout_ms: Option<u64>,
}

/// Request parameters for session operations (snapshot, kill, etc.).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSnapshotParams {
    pub session: String,
}

/// Request parameters for killing a run.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KillParams {
    pub run_id: Uuid,
    #[serde(default)]
    pub wait_timeout_ms: Option<u64>,
}

/// Concise metadata view for a run, avoiding large output strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunMeta {
    pub run_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_run_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_phase: Option<String>,
    pub session: String,
    pub goal: String,
    pub cwd: PathBuf,
    pub read_only: bool,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Bounded structured evidence from real tool completions, for host review.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<ToolEvidence>,
    #[serde(default)]
    pub evidence_truncated: bool,
    /// Canonical pre-dispatch denials, separate from tool execution evidence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denied_calls: Vec<DeniedToolCall>,
    #[serde(default)]
    pub denied_calls_truncated: bool,
}

impl From<&Snapshot> for RunMeta {
    fn from(s: &Snapshot) -> Self {
        Self {
            run_id: s.run_id,
            parent_run_id: s.parent_run_id,
            engine_run_id: s.engine_run_id,
            provider_tool_call_id: s.provider_tool_call_id.clone(),
            canonical_phase: s.canonical_phase.clone(),
            session: s.session.clone(),
            goal: s.goal.clone(),
            cwd: s.cwd.clone(),
            read_only: s.read_only,
            status: s.status,
            last_tool: s.last_tool.clone(),
            error: s.error.clone(),
            evidence: s.evidence.clone(),
            evidence_truncated: s.evidence_truncated,
            denied_calls: s.denied_calls.clone(),
            denied_calls_truncated: s.denied_calls_truncated,
        }
    }
}

/// Detailed run snapshot, including output for terminal states or explicit queries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunSnapshot {
    pub run_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_run_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_phase: Option<String>,
    pub session: String,
    pub goal: String,
    pub cwd: PathBuf,
    pub read_only: bool,
    pub status: Status,
    pub output: String,
    pub output_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Bounded structured evidence from real tool completions, for host review.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<ToolEvidence>,
    #[serde(default)]
    pub evidence_truncated: bool,
    /// Canonical pre-dispatch denials, separate from tool execution evidence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denied_calls: Vec<DeniedToolCall>,
    #[serde(default)]
    pub denied_calls_truncated: bool,
}

impl From<Snapshot> for RunSnapshot {
    fn from(s: Snapshot) -> Self {
        Self {
            run_id: s.run_id,
            parent_run_id: s.parent_run_id,
            engine_run_id: s.engine_run_id,
            provider_tool_call_id: s.provider_tool_call_id,
            canonical_phase: s.canonical_phase,
            session: s.session,
            goal: s.goal,
            cwd: s.cwd,
            read_only: s.read_only,
            status: s.status,
            output: s.output,
            output_truncated: s.output_truncated,
            last_tool: s.last_tool,
            error: s.error,
            evidence: s.evidence,
            evidence_truncated: s.evidence_truncated,
            denied_calls: s.denied_calls,
            denied_calls_truncated: s.denied_calls_truncated,
        }
    }
}

/// Delegation response outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DelegateResponse {
    /// In async mode, returns concise run metadata immediately upon dispatch.
    Async(RunMeta),
    /// In sync mode, waits until terminal state and returns full output snapshot.
    Sync(RunSnapshot),
}

/// Native delegation adapter wrapping a [`SubagentManager`] and parent [`Host`].
#[derive(Clone)]
pub struct DelegationAdapter {
    manager: Arc<SubagentManager>,
    host: Arc<dyn Host>,
}

impl DelegationAdapter {
    pub fn new(manager: Arc<SubagentManager>, host: Arc<dyn Host>) -> Self {
        Self { manager, host }
    }

    pub fn manager(&self) -> &Arc<SubagentManager> {
        &self.manager
    }

    /// Delegate a task with synchronous or asynchronous execution.
    pub async fn delegate(&self, params: DelegateParams) -> Result<DelegateResponse, String> {
        validate_constraints(params.constraints.as_ref())?;

        let relative_path = match params.cwd {
            Some(ref cwd_str) => Some(resolve_cwd_to_relative(self.manager.root_cwd(), cwd_str)?),
            None => None,
        };

        let request = Delegate {
            session: params.session,
            goal: params.goal,
            prompt: params.prompt,
            path: relative_path,
            read_only: params.read_only.unwrap_or(true),
        };

        let run_timeout = params.run_timeout_ms.map(Duration::from_millis);
        let tool_filter = params
            .constraints
            .as_ref()
            .and_then(|c| c.tools.as_ref())
            .map(|w| crate::pattern::ToolFilter::from_whitelist(w))
            .transpose()?
            .unwrap_or_default();
        let snapshot = self.manager.delegate_with_options(
            request,
            self.host.clone(),
            run_timeout,
            tool_filter,
        )?;

        match params.mode {
            DelegationMode::Async => Ok(DelegateResponse::Async(RunMeta::from(&snapshot))),
            DelegationMode::Sync => {
                let wait = params
                    .wait_timeout_ms
                    .map(Duration::from_millis)
                    // If wait_timeout_ms is not specified, wait up to task timeout or default
                    .unwrap_or(run_timeout.unwrap_or_else(|| Duration::from_secs(3600)));
                let terminal = wait_until_done(&self.manager, snapshot.run_id, wait).await?;
                Ok(DelegateResponse::Sync(RunSnapshot::from(terminal)))
            }
        }
    }

    /// Query the status of a specific run.
    /// Returns concise metadata if still running or when querying general status,
    /// but includes final output if terminal.
    pub async fn status(&self, params: StatusParams) -> Result<RunMeta, String> {
        let wait = params
            .wait_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(Duration::ZERO);
        let snapshot = self.manager.status(params.run_id, wait).await?;
        Ok(RunMeta::from(&snapshot))
    }

    /// Collect the same versioned terminal result available to the native tool.
    pub fn result(&self, run_id: Uuid) -> Result<SubagentResult, String> {
        self.manager.result(run_id)
    }

    /// Snapshot query: finds the latest run for a session and returns its full snapshot.
    pub fn session_snapshot(&self, params: SessionSnapshotParams) -> Result<RunSnapshot, String> {
        let runs = self.manager.list();
        let latest = runs
            .into_iter()
            .rev()
            .find(|r| r.session == params.session)
            .ok_or_else(|| format!("No runs found for session '{}'", params.session))?;
        Ok(RunSnapshot::from(latest))
    }

    /// List all runs with concise metadata without whole outputs.
    pub fn session_list(&self) -> Vec<RunMeta> {
        self.manager.list().iter().map(RunMeta::from).collect()
    }

    /// Kill an active run and wait until termination.
    pub async fn kill(&self, params: KillParams) -> Result<RunSnapshot, String> {
        let snapshot = self.manager.kill(params.run_id).await?;
        Ok(RunSnapshot::from(snapshot))
    }
}

/// Validates constraints:
/// - Tools must only contain valid workspace tool whitelists: `workspace` or read-only subsets.
/// - Explicitly rejects `noSkills` / `no_skills` if set to true.
/// - Explicitly rejects `thinking` if specified.
fn validate_constraints(constraints: Option<&DelegationConstraints>) -> Result<(), String> {
    let Some(c) = constraints else {
        return Ok(());
    };

    if let Some(true) = c.no_skills {
        return Err("Constraint 'noSkills' is not supported; AGENTS instructions and security rules cannot be bypassed".into());
    }

    if let Some(ref thinking) = c.thinking
        && !thinking.is_null()
    {
        return Err("Constraint 'thinking' is not supported in this delegation round".into());
    }

    if let Some(ref tools) = c.tools {
        const ALLOWED_TOOLS: &[&str] = &[
            "workspace",
            "read",
            "write",
            "edit",
            "grep",
            "find",
            "ls",
            "shell",
            "bash",
        ];
        for tool in tools {
            if !ALLOWED_TOOLS.contains(&tool.as_str()) {
                return Err(format!(
                    "Unsupported tool '{tool}' in constraints whitelist"
                ));
            }
        }
    }

    Ok(())
}

/// Converts a cwd path (potentially absolute or relative) safely into a relative path
/// within parent root, or returns error if it escapes or is outside parent root.
pub fn resolve_cwd_to_relative(root: &Path, cwd_str: &str) -> Result<String, String> {
    let trimmed = cwd_str.trim();
    if trimmed.is_empty() {
        return Ok(".".into());
    }

    let p = Path::new(trimmed);
    let resolved = if p.is_absolute() {
        let canonical_root = root.canonicalize().map_err(|e| e.to_string())?;
        let canonical_cwd = p
            .canonicalize()
            .map_err(|e| format!("Invalid cwd path: {e}"))?;
        if !canonical_cwd.starts_with(&canonical_root) {
            return Err("cwd must be inside parent workspace root".into());
        }
        let stripped = canonical_cwd
            .strip_prefix(&canonical_root)
            .map_err(|e| e.to_string())?;
        if stripped.as_os_str().is_empty() {
            return Ok(".".into());
        }
        stripped.to_path_buf()
    } else {
        // Relative path: check for '..' components escaping root
        for comp in p.components() {
            if matches!(comp, Component::ParentDir) {
                return Err("Relative cwd cannot contain '..' parent traversal".into());
            }
        }
        p.to_path_buf()
    };

    Ok(resolved.to_string_lossy().into_owned())
}

async fn wait_until_done(
    manager: &SubagentManager,
    id: Uuid,
    total_wait: Duration,
) -> Result<Snapshot, String> {
    let start = tokio::time::Instant::now();
    loop {
        let elapsed = start.elapsed();
        if elapsed >= total_wait {
            return manager.snapshot(id);
        }
        let remaining = total_wait - elapsed;
        let poll_duration = remaining.min(Duration::from_millis(500));
        let snapshot = manager.status(id, poll_duration).await?;
        if !snapshot.status.is_active() {
            return Ok(snapshot);
        }
    }
}
