//! mio-coding-agent: Headless coding agent engine extracted from Ravomi.
//!
//! Mio uses the workspace's `mio-agent` runtime for multi-turn conversations
//! and tool calls. [`Host`] abstracts terminal and environment interaction;
//! the engine applies workspace checks, execution policy and bounded output.
//! Source provenance and retained copyright notices are recorded in `CREDITS.md`.

pub mod accounts;
pub mod antigravity;
pub mod checkpoint;
pub mod compact;
pub mod context;
pub mod context_budget;
pub mod context_memory;
pub mod coordinator;
pub mod dedup;
pub mod deepseek;
pub mod deepseek_pow;
pub mod delegation;
pub mod execution_backend;
pub mod execution_timing;
pub mod file_page;
pub mod image_input;
pub mod journal;
pub mod mcp;
pub mod output_budget;
pub mod pattern;
pub mod plugin;
pub mod policy;
pub mod profile;
pub mod program_status;
pub mod read_observation;
pub mod run_observation;
pub use program_status::{BlockedKind, ProgramState, format_osc7501};
pub mod rpc;
pub mod scheduling;
pub mod session;
pub mod shared_limits;
pub mod shell;
pub mod subagent;
pub mod task_lifecycle;
#[cfg(test)]
pub mod test_http;
pub mod tool_identity;
pub mod tool_lifecycle;
pub mod unknown_effect;
pub mod unresolved_effect;
pub mod workspace;

pub use execution_timing::{ExecutionTiming, ExecutionTimingKind, ExecutionTimingStatus};
pub use pattern::ToolFilter;
pub use read_observation::{ReadObservation, ReadRange, ReadRangeKind};
pub use run_observation::{RunObservation, RunObservationKind, RunObservationStatus};
pub use workspace::{
    Arguments, BashTool, BuiltinTool, EditTool, FindTool, GrepTool, LsTool, ReadTool, Workspace,
    WriteTool, checked_path,
};

use futures_util::StreamExt;
use mio_agent::{AgentBuilder, agent::MultiTurnStreamItem};
pub use mio_core::message::Message;
pub use mio_core::schemars::Schema as OutputSchema;
use mio_core::{
    DynModel,
    completion::{FinishReason, Usage},
    operation::Completion,
    providers::openai::{OpenAIConfig, wire::Route},
    streaming::{Item, StreamEvent},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, future::Future, path::Path, pin::Pin, str::FromStr, sync::Arc};
use tool_identity::ScopedTool;
use uuid::Uuid;

/// 异步 Shell 任务的 Boxed Future。
pub type HostFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Sanitized observation of one completed completion request.
///
/// It carries what the provider *reported*: an unreported counter stays
/// `None` (never fabricated as zero). The provider's raw reply, request body,
/// headers and credentials are never part of it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionCallObservation {
    /// UUID of the agent run that made the call.
    pub run_id: Uuid,
    /// Zero-based index of the request within the run.
    pub call_index: usize,
    /// Token usage as reported; every unreported counter is `None`.
    pub usage: Usage,
    /// Provider-assigned response-scoped id, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// Transport request id, when the provider reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_request_id: Option<String>,
    /// Why the call stopped generating, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<FinishReason>,
    /// Subagent session that owns the run; `None` for the main run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// Stable, sanitized category of a failed run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCategory {
    /// Transport failure with no provider reply, or a timeout status.
    Network,
    /// Rejected credentials (HTTP 401/403).
    Authentication,
    /// Throttling or quota exhaustion (HTTP 429).
    RateLimit,
    /// Provider-side failure (HTTP 5xx).
    Service,
    /// Rejected or unsupported request.
    Request,
    /// Unparseable or incomplete reply.
    Response,
    /// Provider-reported failure without a classifiable status.
    Provider,
    /// The model called a tool this run does not expose.
    ToolCall,
    /// The context budget cannot hold the request.
    ContextBudget,
    /// The run exhausted its model-call budget.
    MaxTurns,
    /// Admission, shared quota, or aggregate budget refused the operation.
    Scheduling,
    /// The run was cancelled.
    Cancelled,
    /// Conversation memory failed.
    Memory,
    /// Internal invariant failure.
    Internal,
}

/// Sanitized diagnosis of a failed run.
///
/// The message is either a fixed human-readable sentence or a local
/// configuration reason (an unsupported option or a context-budget refusal).
/// It never carries a provider body, URL, response headers, source chain,
/// request id or credential.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunDiagnostic {
    /// UUID of the agent run that failed.
    pub run_id: Uuid,
    /// Stable failure category.
    pub category: DiagnosticCategory,
    /// Safe human-readable summary.
    pub message: String,
    /// Whether the same request may reasonably be retried.
    pub retryable: bool,
    /// The HTTP status, when the failure carried a non-2xx one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    /// Subagent session that owns the run; `None` for the main run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// The safe human-readable summary for `category`.
fn diagnostic_message(category: DiagnosticCategory) -> &'static str {
    match category {
        DiagnosticCategory::Network => "网络连接失败或中断",
        DiagnosticCategory::Authentication => "身份验证失败，请检查 API 密钥",
        DiagnosticCategory::RateLimit => "请求过于频繁或超出配额",
        DiagnosticCategory::Service => "模型服务暂时不可用",
        DiagnosticCategory::Request => "请求被拒绝或参数不受支持",
        DiagnosticCategory::Response => "模型响应无法解析或不完整",
        DiagnosticCategory::Provider => "供应商报告了错误",
        DiagnosticCategory::ToolCall => "模型调用了当前不可用的工具",
        DiagnosticCategory::ContextBudget => "上下文预算不足，无法容纳本次请求",
        DiagnosticCategory::MaxTurns => "已达到最大模型调用轮次",
        DiagnosticCategory::Scheduling => "调度或共享预算限制阻止本次操作",
        DiagnosticCategory::Cancelled => "运行已取消",
        DiagnosticCategory::Memory => "会话记忆读写失败",
        DiagnosticCategory::Internal => "内部错误",
    }
}

/// Classify a non-2xx HTTP status.
fn status_category(status: u16) -> DiagnosticCategory {
    match status {
        401 | 403 => DiagnosticCategory::Authentication,
        408 | 425 => DiagnosticCategory::Network,
        429 => DiagnosticCategory::RateLimit,
        500..=599 => DiagnosticCategory::Service,
        _ => DiagnosticCategory::Request,
    }
}

/// Classify a protocol error with no usable status.
fn kind_category(kind: mio_core::error::ErrorKind) -> DiagnosticCategory {
    use mio_core::error::ErrorKind;
    match kind {
        ErrorKind::Http | ErrorKind::Timeout => DiagnosticCategory::Network,
        ErrorKind::Json | ErrorKind::Response => DiagnosticCategory::Response,
        ErrorKind::Url | ErrorKind::Request => DiagnosticCategory::Request,
        ErrorKind::Provider | ErrorKind::ProviderResponse => DiagnosticCategory::Provider,
        ErrorKind::MemoryBackend | ErrorKind::MemoryPolicy => DiagnosticCategory::Memory,
        ErrorKind::Cancelled => DiagnosticCategory::Cancelled,
        ErrorKind::Internal => DiagnosticCategory::Internal,
        _ => DiagnosticCategory::Provider,
    }
}

/// Classify a provider failure without reading its body, URL or source chain.
fn provider_error_diagnostic(
    run_id: Uuid,
    error: &mio_core::error::ProviderError,
) -> RunDiagnostic {
    let http_status = error
        .provider_response_status()
        .map(|status| status.as_u16())
        .filter(|status| !(200..=299).contains(status));
    let category = match http_status {
        Some(status) => status_category(status),
        None => kind_category(error.kind()),
    };
    RunDiagnostic {
        run_id,
        category,
        message: diagnostic_message(category).to_owned(),
        retryable: error.is_retryable(),
        http_status,
        agent: None,
    }
}

/// Recover only allowlisted option names and known effort labels from an
/// erased request error. A Request classification alone does not make its
/// message safe: invalid headers, for example, can contain credentials.
fn safe_report_message(report: &mio_core::error::ErrorReport) -> Option<String> {
    if report.kind != mio_core::error::ErrorKind::Request
        || report.provider_response.is_some()
        || report.http_status.is_some()
    {
        return None;
    }
    for option in [
        "reasoning",
        "cache",
        "service_tier",
        "verbosity",
        "parallel_tool_calls",
        "seed",
        "stop",
        "temperature",
        "max_tokens",
        "tools",
    ] {
        if report
            .message
            .starts_with(&format!("RequestError: `{option}` is not supported by "))
        {
            if option == "reasoning" {
                for level in ["xhigh", "minimal", "max", "medium", "high", "low", "off"] {
                    if report
                        .message
                        .contains(&format!("no `{level}` thinking level"))
                    {
                        return Some(format!("模型不支持思考等级 {level}"));
                    }
                }
            }
            return Some(format!("模型不支持参数 `{option}`"));
        }
    }
    None
}

#[cfg(test)]
mod request_diagnostic_privacy_tests {
    use super::*;
    #[test]
    fn request_classification_does_not_authorize_copying_messages() {
        let report = mio_core::error::ErrorReport::new(
            mio_core::error::ErrorKind::Request,
            "invalid authorization header: Bearer PRIVATE-CREDENTIAL",
        );
        assert!(safe_report_message(&report).is_none());
        let report = mio_core::error::ErrorReport::new(
            mio_core::error::ErrorKind::Request,
            "RequestError: `reasoning` is not supported by private-provider model `private-model`: Ollama has no `xhigh` thinking level; PRIVATE-CREDENTIAL",
        );
        assert_eq!(
            safe_report_message(&report).as_deref(),
            Some("模型不支持思考等级 xhigh")
        );
    }
}

/// Classify a run failure. Only local reasons (unsupported option, context
/// budget) are copied into the message; provider internals are not.
fn classify_run_error(run_id: Uuid, error: &mio_agent::run::PromptError) -> RunDiagnostic {
    use mio_agent::run::PromptError;
    match error {
        PromptError::Provider(mio_core::error::ProviderError::UnsupportedOption(option)) => {
            RunDiagnostic {
                run_id,
                category: DiagnosticCategory::Request,
                message: option.to_string(),
                retryable: false,
                http_status: None,
                agent: None,
            }
        }
        PromptError::Provider(provider) => provider_error_diagnostic(run_id, provider),
        PromptError::Report(report) => {
            let http_status = report
                .http_status
                .filter(|status| !(200..=299).contains(status));
            let category = match http_status {
                Some(status) => status_category(status),
                None => kind_category(report.kind),
            };
            let message = safe_report_message(report)
                .unwrap_or_else(|| diagnostic_message(category).to_owned());
            RunDiagnostic {
                run_id,
                category,
                message,
                retryable: report.retryable,
                http_status,
                agent: None,
            }
        }
        PromptError::Cancelled { reason, .. } if reason.starts_with("Context budget:") => {
            RunDiagnostic {
                run_id,
                category: DiagnosticCategory::ContextBudget,
                message: reason.clone(),
                retryable: false,
                http_status: None,
                agent: None,
            }
        }
        PromptError::Cancelled { reason, .. } if reason.starts_with("Scheduling:") => {
            RunDiagnostic {
                run_id,
                category: DiagnosticCategory::Scheduling,
                message: reason.clone(),
                retryable: false,
                http_status: None,
                agent: None,
            }
        }
        PromptError::Cancelled { .. } => RunDiagnostic {
            run_id,
            category: DiagnosticCategory::Cancelled,
            message: diagnostic_message(DiagnosticCategory::Cancelled).to_owned(),
            retryable: false,
            http_status: None,
            agent: None,
        },
        PromptError::MaxTurns { .. } => RunDiagnostic {
            run_id,
            category: DiagnosticCategory::MaxTurns,
            message: diagnostic_message(DiagnosticCategory::MaxTurns).to_owned(),
            retryable: false,
            http_status: None,
            agent: None,
        },
        PromptError::Memory(_) => RunDiagnostic {
            run_id,
            category: DiagnosticCategory::Memory,
            message: diagnostic_message(DiagnosticCategory::Memory).to_owned(),
            retryable: false,
            http_status: None,
            agent: None,
        },
        PromptError::UnknownToolCall { .. } => RunDiagnostic {
            run_id,
            category: DiagnosticCategory::ToolCall,
            message: diagnostic_message(DiagnosticCategory::ToolCall).to_owned(),
            retryable: false,
            http_status: None,
            agent: None,
        },
        _ => RunDiagnostic {
            run_id,
            category: DiagnosticCategory::Internal,
            message: diagnostic_message(DiagnosticCategory::Internal).to_owned(),
            retryable: false,
            http_status: None,
            agent: None,
        },
    }
}

/// The public `Result<String>` message, kept unchanged for host compatibility.
fn public_run_error(error: &mio_agent::run::PromptError) -> String {
    use mio_agent::run::PromptError;
    match error {
        PromptError::Provider(mio_core::error::ProviderError::UnsupportedOption(option)) => {
            option.to_string()
        }
        PromptError::Cancelled { reason, .. } if reason.starts_with("Context budget:") => {
            reason.clone()
        }
        PromptError::Cancelled { reason, .. } if reason.starts_with("Scheduling:") => {
            reason.clone()
        }
        PromptError::Report(report) => safe_report_message(report)
            .unwrap_or_else(|| "模型请求失败，请检查地址、模型、密钥和服务端状态。".to_string()),
        _ => "模型请求失败，请检查地址、模型、密钥和服务端状态。".to_string(),
    }
}

/// 宿主环境接口：负责命令执行及事件通知，生命周期由 Arc 持有。
///
/// 注意事项：
/// - 事件处理回调 (`on_event`) 必须是 drop-safe 并且是非阻塞（nonblocking）的。
/// - 工作目录限制不是容器安全沙箱，命令执行权限由宿主自身环境决定。
/// - Future 取消不会自动回滚任何已写入的文件。
pub trait Host: Send + Sync {
    /// Optional JSON Schema for this host's final assistant result. The engine
    /// snapshots it once at run start, before any model dispatch. Existing
    /// hosts keep the ordinary answer format via the default `None`.
    fn output_schema(&self) -> Option<OutputSchema> {
        None
    }

    /// Requested policy for supervised processes the engine starts itself: the
    /// `shell` tool, declarative plugin tools and MCP servers. Child hosts
    /// inherit the parent policy, so delegated runs never silently lose it.
    ///
    /// The default is [`execution_backend::ExecutionPolicy::Unrestricted`], so
    /// existing hosts keep working unchanged.
    ///
    /// This covers only the engine's own child-process entry points. Two
    /// things stay outside the child-process sandbox and are reported as such:
    ///
    /// - arbitrary `RunTools::extra_tools` and unknown host tools are refused
    ///   before registration when the requested policy is not `Unrestricted`;
    ///   only engine-loaded, policy-bound plugin and MCP tools
    ///   ([`mcp::PolicyBoundTools`]) are accepted;
    /// - the host's own Provider/model network client runs in the host process,
    ///   not inside a sandboxed child.
    fn execution_policy(&self) -> execution_backend::ExecutionPolicy {
        execution_backend::ExecutionPolicy::Unrestricted
    }

    /// Execute a command under an explicitly requested policy.
    ///
    /// [`Workspace`] calls this instead of [`Host::shell`] so the requested
    /// policy reaches the shell entry point. The default is deliberately
    /// fail-closed: it forwards to [`Host::shell`] only for
    /// [`execution_backend::ExecutionPolicy::Unrestricted`] and otherwise
    /// returns an error without calling the legacy method, so a host that never
    /// heard of isolation is never silently treated as sandboxed. The engine
    /// never runs a native fallback here, because that could bypass a host's
    /// own `no_shell` decision. A host that implements isolation overrides this
    /// method; the delegated `ChildHost` forwards it to its parent.
    fn shell_with_policy(
        &self,
        command: &str,
        cwd: &Path,
        policy: &execution_backend::ExecutionPolicy,
    ) -> HostFuture<Result<String, String>> {
        if matches!(policy, execution_backend::ExecutionPolicy::Unrestricted) {
            return self.shell(command, cwd);
        }
        let message = format!(
            "host does not implement policy-bound shell execution; refusing to run a command that requires isolation (policy '{}')",
            policy.name()
        );
        Box::pin(async move { Err(message) })
    }

    /// 执行命令并返回输出。返回的 future 由调用方取消（通过 drop）。宿主负责子进程实际回收。
    ///
    /// 受管入口请通过 [`Host::shell_with_policy`] 调用，以便执行策略生效。
    fn shell(&self, command: &str, cwd: &Path) -> HostFuture<Result<String, String>>;

    /// 接收 Agent 派发的结构化事件。
    fn on_event(&self, event: AgentEvent);

    /// Receive actual Provider reasoning deltas, separate from answer text.
    /// Existing hosts may ignore them. `part` is the response content-block index.
    fn on_reasoning(&self, _run_id: Uuid, _part: usize, _text: &str) {}

    /// 轮询用户在当前工具循环中注入的实时引导消息（steering）。
    fn poll_steering(&self) -> Option<String> {
        None
    }

    /// 可选的写文件/执行命令前审批钩子。
    fn approve_tool(&self, _operation: &str, _target: &str) -> Result<(), String> {
        Ok(())
    }

    /// 异步审批钩子，默认调用同步 `approve_tool`。
    fn approve_tool_async(&self, operation: &str, target: &str) -> HostFuture<Result<(), String>> {
        let result = self.approve_tool(operation, target);
        Box::pin(async move { result })
    }

    /// Approve one tool call, bound to its normalized parameters and dispatch
    /// context.
    ///
    /// `operation` and `target` keep their historical meaning, so existing
    /// hosts, interactive prompts and journal intents are unchanged. `binding`
    /// additionally pins the approval to the exact tool, canonical workspace
    /// root, run scope and a SHA-256 over the call's parameters. The default
    /// forwards to [`Host::approve_tool_async`], so an existing host keeps
    /// compiling and behaving unchanged; a host that wants to reject a
    /// tampered or replayed binding can override this method.
    fn approve_tool_binding(
        &self,
        operation: &str,
        target: &str,
        _binding: &policy::ApprovalBinding,
    ) -> HostFuture<Result<(), String>> {
        self.approve_tool_async(operation, target)
    }

    /// Opt in to reusing project MCP approvals from the user's trust store.
    /// By default each embedding host must approve its own server starts.
    fn allow_persisted_mcp_trust(&self) -> bool {
        false
    }

    /// Optional trusted configuration directory, outside the project. Hosts
    /// may supply an isolated directory; None uses the user's Mio profile.
    fn mcp_trust_directory(&self) -> Option<std::path::PathBuf> {
        None
    }

    /// Observe one completed completion request with sanitized metadata.
    ///
    /// The observation carries only reported usage, call identity and finish
    /// reason plus this run's UUID. It never carries the provider's raw reply,
    /// request body, headers or credentials. The default is a no-op, so an
    /// existing host keeps compiling and behaving unchanged.
    fn on_completion_call(&self, _call: &CompletionCallObservation) {}

    /// Observe a classified run failure.
    ///
    /// The diagnostic carries a stable category, a bounded human-readable
    /// message, a retryability hint and an optional HTTP status. It never
    /// carries the provider body, request URL, response headers, source chain
    /// or credentials. The default is a no-op, so an existing host keeps
    /// compiling and behaving unchanged.
    fn on_diagnostic(&self, _diagnostic: &RunDiagnostic) {}

    /// The host's current session identity, when it has one.
    ///
    /// It is only used to stamp correlation observations with the host's own
    /// real session; a host without sessions returns `None` and the field stays
    /// `null` rather than being fabricated. The default returns `None`, so an
    /// existing host keeps compiling and behaving unchanged.
    fn session_id(&self) -> Option<String> {
        None
    }

    /// Observe one bounded, content-free run correlation event.
    ///
    /// The observation carries real identities only: the run's UUID (shared
    /// with approval scope, completion calls, diagnostics and timing), the
    /// engine turn, the model's own tool-call id and the dispatcher's effect
    /// id. A delegated child keeps its own engine run UUID and adds its
    /// delegation identity separately, so approval scope and run identity stay
    /// aligned. It never carries tool arguments, target paths, model replies,
    /// request or response bodies, headers or credentials. The default is a
    /// no-op, so an existing host keeps compiling and behaving unchanged.
    fn on_run_observation(&self, _observation: &RunObservation) {}

    /// Observe one successful `read` call's actual returned range.
    ///
    /// Unlike [`Host::on_event`], which only sees the requested operation and
    /// target, this carries what the tool really returned: a normalized
    /// workspace-relative path plus the actual returned line range
    /// (`offset`/`limit`) or byte range (`byteOffset`/`byteLimit`), the file
    /// total and whether the result was truncated. Identity fields are the
    /// engine's real dispatch values when it supplied them; an absent identity
    /// stays `None` rather than being fabricated. It never carries file
    /// contents, absolute paths or the caller's original request parameters,
    /// and a rejected or failed read emits nothing. The default is a no-op, so
    /// an existing host keeps compiling and behaving unchanged.
    fn on_read_observation(&self, _observation: &ReadObservation) {}

    /// Observe a tool's actual outcome without forwarding a child's display
    /// events into the parent conversation. Durable hosts can record execution
    /// evidence here; this is not a test verdict or a rollback guarantee.
    fn on_tool_result(&self, _operation: &str, _target: &str, _result: &Result<String, String>) {}

    /// Persist one authoritative tool-call lifecycle record (DES-05).
    ///
    /// The engine's pure state machine calls this at every real transition:
    /// `Normalized` after final parameter normalization and before approval,
    /// `Admitted`/`Denied` at the policy decision, `Dispatched` before the bus
    /// is handed the effect, and `Executed`/`Failed`/`Unknown` at the outcome.
    /// Returning `Err` means the state could not be persisted; the engine then
    /// refuses the dispatch (or latches fail-closed) instead of pretending the
    /// call succeeded. The default is a no-op `Ok`, so the SDK/engine performs
    /// no I/O of its own and an existing host keeps compiling and behaving
    /// unchanged.
    ///
    /// The record carries only real identity and bounded metadata: it never
    /// contains raw parameters, credentials or the full model output.
    fn on_tool_call_lifecycle(&self, _call: &tool_lifecycle::ToolCallRecord) -> Result<(), String> {
        Ok(())
    }

    /// The unknown-effect fence that gates this run's write/external
    /// dispatches (DES-09).
    ///
    /// The top-level run owns its fence and the engine reaches it by the run
    /// scope; a delegated child host returns its parent's fence so the whole
    /// run tree shares one guard. The default returns `None`, so an existing
    /// host keeps compiling and the engine creates the run's own fence.
    fn unknown_effect_fence(&self) -> Option<std::sync::Arc<unknown_effect::UnknownEffectFence>> {
        None
    }

    /// Explicitly confirm one new write/external dispatch while earlier calls'
    /// side effects are still unknown.
    ///
    /// This is deliberately separate from [`Host::approve_tool_binding`]: an
    /// unknown result is a different fact than "this call is allowed", so it is
    /// never silently treated as approval. The default refuses (fail closed),
    /// so a host that never heard of unknown effects denies the new side effect
    /// rather than approving by omission. A host that implements it must still
    /// run the normal parameter-bound approval afterwards; this confirmation
    /// only releases the unknown-effect gate.
    fn confirm_unknown_side_effect(
        &self,
        _call: &policy::NormalizedCall,
        _unknown: &[unknown_effect::UnknownSideEffect],
    ) -> HostFuture<Result<(), String>> {
        Box::pin(async {
            Err("an earlier tool call has an unknown result; refusing a new side effect until the host explicitly confirms it".to_string())
        })
    }

    /// Observe a dispatched call whose side effect could not be confirmed.
    ///
    /// A durable host records this as `Unknown`, never as `Executed` or
    /// `Failed`. Returning `Err` means the evidence could not be persisted; the
    /// engine then fails closed and refuses later side effects. The default
    /// records nothing, so an existing host keeps compiling and behaving
    /// unchanged.
    fn on_unknown_side_effect(
        &self,
        _effect: &unknown_effect::UnknownSideEffect,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Observe one model or tool dispatch's measured duration.
    ///
    /// The observation carries a monotonic, non-negative whole-millisecond
    /// duration from dispatch to outcome, a `success`/`error` classification,
    /// the run's UUID and the dispatcher's effect id for correlation. It never
    /// carries tool arguments, target paths, model replies, request or response
    /// bodies, headers or credentials.
    ///
    /// The span starts after admission hooks, so it excludes the preceding
    /// scheduling queue wait and any prompt or context preparation before the
    /// dispatch. A tool's span covers the tool body's own execution, which may
    /// include that tool's internal queueing, and concurrent durations must not
    /// be summed as wall time. An effect that never reaches an outcome (a
    /// cancellation or a denied dispatch) emits nothing. The default is a
    /// no-op, so an existing host keeps compiling and behaving unchanged.
    fn on_execution_timing(&self, _timing: &ExecutionTiming) {}

    /// Observe bounded compaction metadata without receiving conversation text.
    fn on_context_compaction(
        &self,
        _observation: &compact::ContextCompactionObservation,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Persist one authoritative task lifecycle record (DES-04).
    ///
    /// The engine's single task tracker calls this at every real transition:
    /// `Queued` when a task is registered (before any scheduling permit),
    /// `Running` only after a real scheduling permit was admitted, and the
    /// terminal phases at settlement (a cancellation or timeout terminal is
    /// deferred until every worker guard is reclaimed). The record carries only
    /// real identity (the canonical task id, the real engine run UUID and the
    /// source provider tool-call id of a subagent-creating call) plus bounded
    /// metadata; it never contains raw arguments, credentials or model output.
    ///
    /// Returning `Err` means the state could not be persisted; the tracker then
    /// latches fail-closed and refuses later transitions instead of pretending
    /// the task was recorded. The default is a no-op `Ok` so an existing host
    /// keeps compiling and the engine performs no I/O of its own.
    fn on_task_lifecycle(&self, _record: &task_lifecycle::TaskRecord) -> Result<(), String> {
        Ok(())
    }
}

/// 把任务生命周期记录投影到宿主的 [`Host::on_task_lifecycle`]。
///
/// 跟踪器不直接持有 I/O；它只把经过合法转移校验的记录交给宿主回调，由宿主把
/// 它投影到既有的 dedup 账本、journal 或子代理快照。回调在跟踪器状态锁之外执行，
/// 因此回调内查询快照不会死锁。
struct HostTaskSink(Arc<dyn Host>);

impl task_lifecycle::TaskLifecycleSink for HostTaskSink {
    fn on_task_lifecycle(&self, record: &task_lifecycle::TaskRecord) -> Result<(), String> {
        self.0.on_task_lifecycle(record)
    }
}

/// 构造一个经宿主回调持久化任务状态的 sink。
///
/// 宿主提供真实的投影（dedup/journal/子代理）；不在宿主内做第二套磁盘权威。
pub fn host_task_sink(host: Arc<dyn Host>) -> Arc<dyn task_lifecycle::TaskLifecycleSink> {
    Arc::new(HostTaskSink(host))
}

/// Agent 执行过程中发出的结构化事件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AgentEvent {
    /// 模型流式文本输出。
    Text(String),
    /// 工具开始执行。
    ToolStarted { operation: String, target: String },
    /// 工具执行完成。
    ToolFinished {
        operation: String,
        target: String,
        result: Result<String, String>,
    },
}

/// 支持的模型供应商类型。
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    #[default]
    Custom,
    /// OpenAI-compatible Responses API on a custom endpoint.
    CustomResponses,
    Codex,
    Deepseek,
    /// Anthropic's native Messages protocol (`/v1/messages`).
    Anthropic,
    /// Google Gemini's native GenerateContent protocol.
    Google,
    /// An Ollama daemon's native chat protocol (`/api/chat`).
    Ollama,
    Antigravity,
    DeepseekWeb,
}

/// 思考预算等级（对齐 Pi `--thinking` 等级）。
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    /// 全部等级，按由弱到强排序。
    pub const ALL: [ThinkingLevel; 7] = [
        ThinkingLevel::Off,
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::Xhigh,
        ThinkingLevel::Max,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    fn reasoning(self) -> mio_core::completion::Reasoning {
        match self {
            ThinkingLevel::Off => mio_core::completion::Reasoning::Off,
            ThinkingLevel::Minimal => {
                mio_core::completion::Reasoning::Effort(mio_core::completion::Effort::Minimal)
            }
            ThinkingLevel::Low => {
                mio_core::completion::Reasoning::Effort(mio_core::completion::Effort::Low)
            }
            ThinkingLevel::Medium => {
                mio_core::completion::Reasoning::Effort(mio_core::completion::Effort::Medium)
            }
            ThinkingLevel::High => {
                mio_core::completion::Reasoning::Effort(mio_core::completion::Effort::High)
            }
            ThinkingLevel::Xhigh => {
                mio_core::completion::Reasoning::Effort(mio_core::completion::Effort::XHigh)
            }
            ThinkingLevel::Max => {
                mio_core::completion::Reasoning::Effort(mio_core::completion::Effort::Max)
            }
        }
    }
}

impl FromStr for ThinkingLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" => Ok(Self::Off),
            "minimal" | "min" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" | "med" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            other => Err(format!(
                "Invalid thinking level '{other}'; expected off, minimal, low, medium, high, xhigh, or max"
            )),
        }
    }
}

/// Validate an explicit sampling temperature before any model dispatch.
/// Unknown catalog entries remain provider-validated; known refusal rules and
/// adapters that do not forward temperature fail locally instead of ignoring it.
pub fn validate_model_temperature(
    config: &Config,
    temperature: Option<f64>,
    thinking: Option<ThinkingLevel>,
    catalog_provider: Option<&str>,
) -> Result<(), String> {
    let Some(temperature) = temperature else {
        return Ok(());
    };
    if !temperature.is_finite() || !(0.0..=2.0).contains(&temperature) {
        return Err("temperature must be finite and between 0 and 2".into());
    }
    if matches!(
        config.provider,
        Provider::Antigravity | Provider::DeepseekWeb
    ) {
        return Err("this adapter does not support an explicit temperature".into());
    }
    let vendor = match config.provider {
        Provider::Custom | Provider::CustomResponses => catalog_provider.unwrap_or("openai"),
        Provider::Codex => "openai",
        Provider::Deepseek => "deepseek",
        Provider::Anthropic => "anthropic",
        Provider::Google => "gcp.gemini",
        Provider::Ollama => "ollama",
        _ => unreachable!(),
    };
    let vendor = if matches!(vendor, "google" | "gemini") {
        "gcp.gemini"
    } else {
        vendor
    };
    let (_, model, inline) = parse_model_spec(&config.model);
    let spec = mio_core::providers::registry::ProviderId::catalog(vendor)
        .and_then(|provider| mio_core::catalog::Catalog::builtin().get(provider, &model));
    match spec.and_then(|model| model.sampling) {
        Some(mio_core::catalog::Sampling::Never) => {
            Err("model catalog refuses temperature for this model".into())
        }
        Some(mio_core::catalog::Sampling::ReasoningOff)
            if thinking.or(inline) != Some(ThinkingLevel::Off) =>
        {
            Err("temperature requires explicit thinking off for this model".into())
        }
        _ => Ok(()),
    }
}

/// 校验指定 provider 是否支持给定的思考预算等级。
pub fn validate_thinking(
    provider: Provider,
    thinking: Option<ThinkingLevel>,
) -> Result<(), String> {
    match provider {
        Provider::Deepseek => match thinking {
            None
            | Some(
                ThinkingLevel::Off | ThinkingLevel::Low | ThinkingLevel::High | ThinkingLevel::Max,
            ) => Ok(()),
            Some(other) => Err(format!(
                "DeepSeek API 仅支持 off、low、high、max 思考等级，不支持 {}",
                other.as_str()
            )),
        },
        Provider::DeepseekWeb => match thinking {
            None | Some(ThinkingLevel::Off) => Ok(()),
            Some(other) => Err(format!(
                "DeepSeek 网页端仅支持 off（关闭思考），不支持 {}",
                other.as_str()
            )),
        },
        _ => Ok(()),
    }
}

/// Validate local provider/model capabilities without account or network access.
pub fn validate_model_thinking(
    config: &Config,
    thinking: Option<ThinkingLevel>,
) -> Result<(), String> {
    let (_, model, inline) = parse_model_spec(&config.model);
    let thinking = thinking.or(inline);
    validate_thinking(config.provider, thinking)?;
    let vendor = match config.provider {
        Provider::Custom | Provider::CustomResponses | Provider::Codex => "openai",
        Provider::Anthropic => "anthropic",
        Provider::Google => "gcp.gemini",
        Provider::Ollama => "ollama",
        _ => "",
    };
    if !vendor.is_empty()
        && let Some(thinking) = thinking
        && let Some(id) = mio_core::providers::registry::ProviderId::catalog(vendor)
        && let Some(spec) = mio_core::catalog::Catalog::builtin().find(id, &model)
    {
        spec.validate(
            &mio_core::completion::GenerationOptions::default().reasoning(thinking.reasoning()),
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// A native provider's API root, stripped of the suffixes a pasted
/// OpenAI-compatible URL carries (Gemini's `/v1beta/openai`, Ollama's `/v1`).
/// Anthropic's own normalization lives in `AnthropicConfig::with_base_url`.
fn native_base_url(provider: Provider, base_url: &str) -> String {
    let mut base = base_url.trim().trim_end_matches('/').to_owned();
    let suffixes: &[&str] = match provider {
        Provider::Google => &["/v1beta/openai", "/v1beta", "/openai"],
        Provider::Ollama => &["/v1"],
        _ => &[],
    };
    let mut changed = true;
    while changed {
        changed = false;
        for suffix in suffixes {
            if let Some(stripped) = base.strip_suffix(suffix) {
                base = stripped.trim_end_matches('/').to_owned();
                changed = true;
                break;
            }
        }
    }
    base
}

/// Parse `[provider/]model[:thinking]` shorthand into `(provider, model, thinking)`.
pub fn parse_model_spec(raw: &str) -> (Option<String>, String, Option<ThinkingLevel>) {
    let trimmed = raw.trim();
    let (before_thinking, thinking) = match trimmed.rsplit_once(':') {
        Some((prefix, suffix)) if !prefix.is_empty() => match ThinkingLevel::from_str(suffix) {
            Ok(level) => (prefix, Some(level)),
            Err(_) => (trimmed, None),
        },
        _ => (trimmed, None),
    };
    if let Some((prov, rest)) = before_thinking.split_once('/') {
        let lower = prov.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "custom"
                | "openai"
                | "anthropic"
                | "google"
                | "gemini"
                | "deepseek"
                | "antigravity"
                | "codex"
                | "openrouter"
                | "groq"
                | "mistral"
                | "xai"
                | "ollama"
        ) && !rest.trim().is_empty()
        {
            return (Some(lower), rest.trim().to_owned(), thinking);
        }
    }
    (None, before_thinking.to_owned(), thinking)
}

/// Agent 运行配置。
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default)]
    pub provider: Provider,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: Provider::Custom,
            base_url: "https://api.openai.com/v1".into(),
            model: String::new(),
            api_key: String::new(),
        }
    }
}

/// 第三方/高级账户服务集合。
#[derive(Default)]
pub struct Services<'a> {
    pub accounts: Option<&'a accounts::Accounts>,
    pub antigravity: Option<&'a antigravity::Antigravity>,
    pub deepseek: Option<&'a deepseek::Deepseek>,
}

/// Optional instructions and turn budget supplied by the host.
/// Loading project files is explicit through `context::ProjectContext`.
#[derive(Clone, Debug)]
pub struct RunOptions {
    /// Optional shared admission and aggregate budgets. CLI runs enable this;
    /// embedding hosts may reuse one Scheduler across independent sessions.
    pub scheduler: Option<Arc<scheduling::Scheduler>>,
    pub instructions: String,
    /// Explicit host-owned constraints and unfinished goals. Included verbatim
    /// in every request's fixed instructions, never inferred from conversation.
    pub pinned_context: context_memory::PinnedContext,
    pub max_turns: usize,
    /// Maximum concurrent tool bodies per run (1–32). Defaults to serial execution.
    pub tool_concurrency: usize,
    pub thinking: Option<ThinkingLevel>,
    /// Session-history compaction before a run. `Some(bytes)` enables it; the
    /// effective trigger is the smaller of `bytes` and 3/4 of this run's input
    /// budget, so a large fixed value can never wait until the model window is
    /// already overflowing. `None` disables session compaction; per-request
    /// budget fitting still applies.
    pub auto_compact_bytes: Option<usize>,
    /// Explicit deployment context capacity; otherwise use the bundled catalog or fallback.
    pub context_window: Option<u32>,
    /// Output reserve and provider request cap (web adapters may not support a cap).
    pub max_output_tokens: Option<u32>,
    /// Fixed sampling temperature sent with every model request.
    /// `None` (the default) leaves the provider's or model's own default in place.
    /// A set value goes into the real provider request; it is never dropped here.
    pub temperature: Option<f64>,
    /// Catalog vendor for a custom compatible endpoint; never changes routing.
    pub catalog_provider: Option<String>,
    /// Optional pre-edit file checkpoints for `write` / `edit`.
    /// `None` (the default) keeps the historical behavior: no snapshots are taken and
    /// `CheckpointStore::rewind` cannot be used for this run. Passing a store attaches it
    /// to the run's workspace, where snapshots still happen only after the tool filter,
    /// read-only check, workspace lease and unified approval have admitted the write.
    pub checkpoints: Option<Arc<checkpoint::CheckpointStore>>,
    /// Optional explicit canonical task binding.
    ///
    /// `Some(handle)` makes that task the authoritative phase owner for this
    /// run: the engine attaches the run's real engine UUID and, at the first
    /// real scheduling permit, records `Queued -> Running`. `None` falls back
    /// to the current task-local scope, and if neither exists the engine
    /// creates a real host-persisted canonical task for the run. legacy
    /// dedup/journal fields are only projections of this record.
    pub task: Option<task_lifecycle::TaskHandle>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            scheduler: None,
            instructions: String::new(),
            pinned_context: context_memory::PinnedContext::default(),
            max_turns: 32,
            tool_concurrency: 1,
            thinking: None,
            auto_compact_bytes: None,
            context_window: None,
            max_output_tokens: None,
            temperature: None,
            catalog_provider: None,
            checkpoints: None,
            task: None,
        }
    }
}

/// 无状态运行一次 Agent 会话循环。
///
/// * `config`: 模型提供商与连接配置
/// * `history`: 先前的对话消息列表（Vec<Message>）
/// * `prompt`: 用户当前输入的任务描述
/// * `cwd`: 工具操作绑定的工作目录（须存在且为目录）
/// * `host`: 宿主交互接口实现
/// * `services`: 账户及协议服务（可选）
///
/// 成功时返回包含新交互的完整对话历史记录。当任务失败时返回错误信息。
/// 若调用方取消（drop 掉返回的 future），内部正在执行的宿主 shell future 也随之 drop。
pub async fn run(
    config: Config,
    history: Vec<Message>,
    prompt: &str,
    cwd: &Path,
    host: Arc<dyn Host>,
    services: Services<'_>,
) -> Result<Vec<Message>, String> {
    run_with_options(
        config,
        history,
        prompt,
        cwd,
        host,
        services,
        RunOptions::default(),
    )
    .await
}

pub async fn run_with_options(
    config: Config,
    history: Vec<Message>,
    prompt: &str,
    cwd: &Path,
    host: Arc<dyn Host>,
    services: Services<'_>,
    options: RunOptions,
) -> Result<Vec<Message>, String> {
    run_with_tools(config, history, prompt, cwd, host, services, options.into()).await
}

/// Like [`run_with_options`], but also returns the authoritative task record
/// this run was bound to, so a host can read the real canonical phase and
/// identity instead of inferring it from history or legacy ids.
pub async fn run_with_options_tracked(
    config: Config,
    history: Vec<Message>,
    prompt: &str,
    cwd: &Path,
    host: Arc<dyn Host>,
    services: Services<'_>,
    options: RunOptions,
) -> Result<(Vec<Message>, task_lifecycle::TaskRecord), String> {
    run_with_tools_tracked(config, history, prompt, cwd, host, services, options.into()).await
}

/// Host-controlled tool access. Children never receive a subagent manager.
#[derive(Default)]
pub struct RunTools {
    pub options: RunOptions,
    pub read_only: bool,
    pub subagents: Option<Arc<subagent::SubagentManager>>,
    pub tool_filter: ToolFilter,
    /// Arbitrary host-supplied dynamic tools. Accepted as-is only under
    /// [`execution_backend::ExecutionPolicy::Unrestricted`]; a run that
    /// requests isolation refuses any non-empty set before registration.
    pub extra_tools: Vec<mio_agent::tool::DynamicTool>,
    /// Engine-loaded plugin and stdio MCP tools bound to the policy they were
    /// loaded under. Construct only through [`mcp::load_policy_bound_tools`].
    pub policy_bound_tools: Option<mcp::PolicyBoundTools>,
}

impl From<RunOptions> for RunTools {
    fn from(options: RunOptions) -> Self {
        Self {
            options,
            ..Self::default()
        }
    }
}

/// Run with optional native delegation. Keep the manager alive across turns to resume children.
///
/// The canonical task is resolved explicitly (see [`RunOptions::task`]), then
/// from the current task-local scope ([`task_lifecycle::scope_task`]), and
/// finally created here as a real host-persisted task. The engine attaches the
/// run's real engine UUID once and the real scheduling hook admits `Queued ->
/// Running` with a live permit. When the run has no scheduler, the wrapper
/// admits with a genuine no-extra-restriction engine lease instead of
/// fabricating one or refusing every default SDK run.
pub async fn run_with_tools(
    config: Config,
    history: Vec<Message>,
    prompt: &str,
    cwd: &Path,
    host: Arc<dyn Host>,
    services: Services<'_>,
    tools: RunTools,
) -> Result<Vec<Message>, String> {
    run_with_tools_tracked(config, history, prompt, cwd, host, services, tools)
        .await
        .map(|(history, _record)| history)
}

/// Like [`run_with_tools`], but also returns the authoritative task record so
/// a host can join the run's outcome with its real canonical state.
pub async fn run_with_tools_tracked(
    config: Config,
    history: Vec<Message>,
    prompt: &str,
    cwd: &Path,
    host: Arc<dyn Host>,
    services: Services<'_>,
    tools: RunTools,
) -> Result<(Vec<Message>, task_lifecycle::TaskRecord), String> {
    run_with_tools_message_tracked(
        config,
        history,
        Message::user(prompt),
        cwd,
        host,
        services,
        tools,
    )
    .await
}

/// Run a typed user message through the same engine as the text entry point.
/// Image blocks are admitted only for locally catalogued image-capable models
/// and bounded base64 PNG/JPEG/GIF/WebP data; other media fail before dispatch.
pub async fn run_with_tools_message(
    config: Config,
    history: Vec<Message>,
    prompt: Message,
    cwd: &Path,
    host: Arc<dyn Host>,
    services: Services<'_>,
    tools: RunTools,
) -> Result<Vec<Message>, String> {
    run_with_tools_message_tracked(config, history, prompt, cwd, host, services, tools)
        .await
        .map(|(history, _record)| history)
}

/// Typed-message counterpart of [`run_with_tools_tracked`].
pub async fn run_with_tools_message_tracked(
    config: Config,
    history: Vec<Message>,
    prompt: Message,
    cwd: &Path,
    host: Arc<dyn Host>,
    services: Services<'_>,
    tools: RunTools,
) -> Result<(Vec<Message>, task_lifecycle::TaskRecord), String> {
    let (handle, ownership) = resolve_run_task(&tools.options, host.clone())?;
    // A task with no scheduler of its own still needs a real admission before
    // `Running`: a genuine owned semaphore permit is a real owner, never a bool
    // marker. A scheduler-backed run admits later, at its first real permit.
    let _engine_lease = if tools.options.scheduler.is_none() {
        admit_with_engine_lease(&handle)?
    } else {
        None
    };
    // The engine settles the task itself when it owns the run. For those tasks a
    // `Deferred` finish (a real worker or reaper still holding a guard) must be
    // awaited before the final record is returned, otherwise a successful tracked
    // run could hand back a still-`Running` snapshot. A caller-owned task-local
    // scope keeps its existing ownership and is never force-settled here.
    let engine_settles = ownership.settle_normal;
    let mut settlement = ownership.settle_normal.then(|| OwnedTaskSettlement {
        handle: Some(handle.clone()),
        settle_on_drop: ownership.settle_on_drop,
    });
    let result = task_lifecycle::scope_task(
        handle.clone(),
        run_with_tools_scoped(config, history, prompt, cwd, host, services, tools),
    )
    .await;
    if let Some(settlement) = &mut settlement {
        settlement.commit(result.is_ok());
    }
    // Await the real settled record for engine-owned tasks. `wait_settled`
    // returns the last committed record when persistence has latched
    // fail-closed, so a broken sink cannot hang the run forever.
    let record = if engine_settles {
        match handle.wait_settled().await {
            Some(record) => record,
            None => handle
                .snapshot()
                .ok_or_else(|| "the canonical task record is unavailable".to_string())?,
        }
    } else {
        handle
            .snapshot()
            .ok_or_else(|| "the canonical task record is unavailable".to_string())?
    };
    result.map(|history| (history, record))
}

/// Who settles the canonical task the engine bound to a run.
#[derive(Clone, Copy)]
struct TaskOwnership {
    /// Commit `Completed`/`Failed` when the run future returns.
    settle_normal: bool,
    /// Mark `Cancelled` if the run future is dropped before settlement.
    settle_on_drop: bool,
}

/// Resolve the canonical task for this run and who owns its settlement.
///
/// An explicit [`RunOptions::task`] is bound and engine-settled on normal
/// return, but the caller keeps ownership of a dropped run. A task-local
/// scope belongs entirely to its caller (the CLI/RPC runner settles it after
/// real worker reclamation). With neither, the engine creates a real
/// host-persisted task and owns it completely.
fn resolve_run_task(
    options: &RunOptions,
    host: Arc<dyn Host>,
) -> Result<(task_lifecycle::TaskHandle, TaskOwnership), String> {
    if let Some(handle) = &options.task {
        return Ok((
            handle.clone(),
            TaskOwnership {
                settle_normal: true,
                settle_on_drop: false,
            },
        ));
    }
    if let Some(handle) = task_lifecycle::current_task_handle() {
        return Ok((
            handle,
            TaskOwnership {
                settle_normal: false,
                settle_on_drop: false,
            },
        ));
    }
    let task_id = task_lifecycle::TaskId::new(Uuid::new_v4().to_string())
        .map_err(|error| error.to_string())?;
    let tracker = task_lifecycle::TaskLifecycleTracker::new(
        task_id.as_str().to_string(),
        host_task_sink(host),
    );
    tracker
        .register(task_lifecycle::TaskIdentity::new(task_id.clone()), None)
        .map_err(|error| error.to_string())?;
    Ok((
        task_lifecycle::TaskHandle::new(tracker, task_id),
        TaskOwnership {
            settle_normal: true,
            settle_on_drop: true,
        },
    ))
}

/// A no-extra-restriction engine lease capacity. A permit from this semaphore
/// is a real owned resource, so admission evidence comes from a held owner
/// rather than a fabricated flag or counter.
const ENGINE_LEASE_PERMITS: usize = usize::MAX >> 3;

/// Admit a still-`Queued` task with a real engine lease when the run has no
/// scheduler. Returns the lease the caller must keep alive for the run; an
/// already-admitted or terminal task is left untouched.
fn admit_with_engine_lease(
    handle: &task_lifecycle::TaskHandle,
) -> Result<Option<Arc<task_lifecycle::ActiveLease>>, String> {
    let Some(record) = handle.snapshot() else {
        return Ok(None);
    };
    if record.phase != task_lifecycle::TaskPhase::Queued {
        return Ok(None);
    }
    let semaphore = Arc::new(tokio::sync::Semaphore::new(ENGINE_LEASE_PERMITS));
    let permit = semaphore
        .try_acquire_owned()
        .map_err(|_| "engine lease capacity exhausted".to_string())?;
    let lease = task_lifecycle::ActiveLease::from_semaphore_permit(permit);
    handle
        .tracker()
        .admit(handle.task_id(), lease.as_ref())
        .map_err(|error| error.to_string())?;
    Ok(Some(lease))
}

/// Commits the terminal phase of an engine-owned task on normal return and,
/// when it owns the dropped case too, marks it `Cancelled` if the run future is
/// dropped first.
struct OwnedTaskSettlement {
    handle: Option<task_lifecycle::TaskHandle>,
    settle_on_drop: bool,
}

impl OwnedTaskSettlement {
    fn commit(&mut self, ok: bool) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        let settlement = if ok {
            task_lifecycle::Settlement::Completed
        } else {
            task_lifecycle::Settlement::Failed
        };
        if let Err(error) = handle.finish(settlement, None) {
            eprintln!("Warning: canonical task settlement failed: {error}");
        }
    }
}

impl Drop for OwnedTaskSettlement {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        if !self.settle_on_drop {
            // The caller still owns a dropped explicit task.
            return;
        }
        let _ = handle.request_cancel(Some("run future dropped before settlement"));
        let _ = handle.finish(task_lifecycle::Settlement::Cancelled, None);
    }
}

async fn run_with_tools_scoped(
    config: Config,
    history: Vec<Message>,
    prompt: Message,
    cwd: &Path,
    host: Arc<dyn Host>,
    services: Services<'_>,
    mut tools: RunTools,
) -> Result<Vec<Message>, String> {
    let options = tools.options;
    if !(1..=32).contains(&options.tool_concurrency) {
        return Err("工具并发数须在 1–32 之间".into());
    }
    if options.instructions.len() > 128 * 1024 || !(1..=256).contains(&options.max_turns) {
        return Err("附加指令须不超过 128 KiB，工具轮次须在 1–256 之间".into());
    }
    let output_schema = host.output_schema();
    if let Some(schema) = &output_schema {
        let bytes =
            serde_json::to_vec(schema).map_err(|error| format!("输出 Schema 编码失败：{error}"))?;
        if bytes.len() > 32 * 1024 {
            return Err("输出 Schema 超过 32 KiB 上限".into());
        }
    }
    image_input::validate_prompt(&config, &options, &prompt)?;
    let (_, clean_model, inline_thinking) = parse_model_spec(&config.model);
    if clean_model.trim().is_empty() {
        return Err("请先在模型设置中填写模型名和服务地址".into());
    }
    let effective_thinking = options.thinking.or(inline_thinking);
    validate_model_thinking(&config, effective_thinking)?;
    validate_model_temperature(
        &config,
        options.temperature,
        effective_thinking,
        options.catalog_provider.as_deref(),
    )?;
    // Isolation is enforced before anything is dispatched or registered. A
    // non-`Unrestricted` run can never register arbitrary host dynamic tools:
    // in-process code cannot attest its own subprocess isolation. Only tools
    // loaded through `mcp::load_policy_bound_tools` are accepted, and their
    // recorded policy fingerprint must still match this run's policy.
    let run_policy = host.execution_policy();
    if !matches!(run_policy, execution_backend::ExecutionPolicy::Unrestricted)
        && !tools.extra_tools.is_empty()
    {
        return Err(format!(
            "Refusing to register {} arbitrary extra tool(s) under execution policy '{}': load plugin and MCP tools through mcp::load_policy_bound_tools instead; a host cannot attest an arbitrary tool's subprocess isolation",
            tools.extra_tools.len(),
            run_policy.name(),
        ));
    }
    if let Some(bound) = tools.policy_bound_tools.take() {
        tools.extra_tools.extend(bound.into_tools_for(&run_policy)?);
    }
    let budget = context_budget::ContextBudget::resolve(&config, &options)?;
    let canonical_cwd = cwd
        .canonicalize()
        .map_err(|e| format!("工作目录不可用：{e}"))?;
    if !canonical_cwd.is_dir() {
        return Err("工作目录不是文件夹".into());
    }
    let run_id = Uuid::new_v4();
    // The engine records its real run UUID on the canonical task (explicit,
    // task-local, or auto-created). Admission happens at the first real
    // scheduling permit; without a scheduler the wrapper already admitted with
    // a real engine lease instead of fabricating one.
    if let Some(handle) = task_lifecycle::current_task_handle() {
        let engine_run = task_lifecycle::EngineRunId::new(run_id.to_string())
            .map_err(|error| error.to_string())?;
        handle
            .attach_engine_run(engine_run)
            .map_err(|error| error.to_string())?;
    }
    let scheduling_hook = options
        .scheduler
        .as_ref()
        .map(|scheduler| scheduling::SchedulingHook::new(scheduler.clone(), &config));

    let model: DynModel<Completion> = match config.provider {
        Provider::Antigravity => services
            .antigravity
            .ok_or("Google 账号服务不可用")?
            .client_with_thinking(&clean_model, effective_thinking.map(|t| t.as_str()))
            .await?
            .completion(clean_model)
            .into(),
        Provider::DeepseekWeb => services
            .deepseek
            .ok_or("DeepSeek 账号服务不可用")?
            .client()
            .await?
            .completion(clean_model)
            .into(),
        Provider::Codex => services
            .accounts
            .ok_or("账号服务不可用")?
            .codex()
            .await?
            .completion(clean_model)
            .into(),
        // Native Messages protocol; a compatible gateway uses `custom`.
        Provider::Anthropic => mio_core::providers::anthropic::AnthropicConfig::new(config.api_key)
            .with_base_url(config.base_url)
            .client()
            .completion(clean_model)
            .into(),
        // Native GenerateContent protocol.
        Provider::Google => mio_core::providers::gemini::GeminiConfig::new(config.api_key)
            .with_base_url(native_base_url(Provider::Google, &config.base_url))
            .client()
            .completion(clean_model)
            .into(),
        // Native `/api/chat` protocol; an OpenAI-compatible daemon uses `custom`.
        Provider::Ollama => mio_core::providers::ollama::OllamaConfig::new()
            .with_base_url(native_base_url(Provider::Ollama, &config.base_url))
            .with_api_key(config.api_key)
            .client()
            .native_completion(clean_model)
            .into(),
        Provider::Custom | Provider::CustomResponses | Provider::Deepseek => {
            let client_config = if config.provider == Provider::Deepseek {
                OpenAIConfig::with_key(&mio_core::providers::openai::wire::DEEPSEEK, config.api_key)
            } else {
                OpenAIConfig::new(config.api_key)
            };
            let route = if config.provider == Provider::CustomResponses {
                Route::Responses
            } else {
                Route::Chat
            };
            let client = client_config
                .with_route(route)
                .with_base_url(config.base_url)
                .client();
            client.completion(clean_model).into()
        }
    };

    if let Some(manager) = &tools.subagents
        && (tools.read_only || manager.root_cwd() != canonical_cwd)
    {
        return Err("Delegation requires a writable parent with the same workspace root".into());
    }
    // read 工具结果会原样进入下一轮请求，因此按已解析的上下文输入预算的 1/4 收紧单次输出，
    // 避免一次 64 KiB read 就撑爆输入上限。这里只约束 read / workspace read；
    // grep、shell 等其它工具的输出仍有各自上限，不承诺永不超预算。
    let read_output_limit_bytes = budget.input_limit() / 4;
    // One admission gate per run. It is registered as the last dispatch hook,
    // so it approves the final, patched arguments the bus actually dispatches.
    let tool_policy = policy::ToolPolicy::new(
        tools.tool_filter.clone(),
        tools.read_only,
        host.clone(),
        policy::PolicyContext::new(canonical_cwd.display().to_string(), run_id.to_string()),
    );
    // A delegated child host hands back its parent's fence, so the whole run
    // tree shares one unknown-effect guard. A top-level run keeps the fence its
    // own scope already resolved.
    let tool_policy = match host.unknown_effect_fence() {
        Some(fence) => tool_policy.with_unknown_fence(fence),
        None => tool_policy,
    };
    // The authoritative tool-call state machine is per run and shared by the
    // policy gate and the final dispatch hook, so admitted, dispatched and
    // terminal phases are tracked once, not in a shadow beside the tool body.
    let tool_lifecycle = tool_policy.lifecycle().clone();
    // Host-provided dynamic tools must respect the same filter as built-ins;
    // plugin and MCP tools were already filtered when they were loaded. Plugin
    // names are matched through their short declared alias as well.
    let extra_filter = tools.tool_filter.clone();
    tools
        .extra_tools
        .retain(|tool| policy::dynamic_tool_allowed(&extra_filter, tool.name().as_str()));
    let mut workspace_tool = if tools.read_only {
        Workspace::new_read_only(&canonical_cwd, host.clone())?
    } else {
        Workspace::new(&canonical_cwd, host.clone())?
    }
    .with_execution_policy(run_policy.clone())
    .with_tool_filter(tools.tool_filter.clone())
    .with_tool_policy(tool_policy.clone())
    .with_read_output_limit_bytes(read_output_limit_bytes);
    if let Some(checkpoints) = options.checkpoints.clone() {
        workspace_tool = workspace_tool.with_checkpoints(checkpoints);
    }
    workspace_tool.subagents = tools.subagents.clone();
    if let Some(manager) = &tools.subagents {
        // A child's permission set is the parent's, minus whatever the parent
        // withholds: nested delegation can only narrow.
        manager.set_permission_ceiling(tools.tool_filter.clone(), tools.read_only);
        manager.set_pinned_context(options.pinned_context.clone())?;
    }
    workspace_tool.shared_limits = options
        .scheduler
        .as_ref()
        .and_then(|scheduler| scheduler.shared.clone());

    let mut preamble = format!(
        "你叫 Mio，是编程助手。使用中文简洁回答。工具在用户选择的工作目录执行。先查看文件再修改，保留用户改动，不声称未执行的测试通过。命令工具不是交互式 shell，避免需要输入的命令。先搜索定位信息并提炼必要事实，再做最小修改。摘要有损，继续调用前简要记录已确认的关键事实；避免无新问题的重复读取。尽早实现并验证，验证通过后直接回答，不为收尾反复调用工具。\n{}",
        options.instructions
    );
    if let Some(pinned) = options.pinned_context.instruction_block() {
        preamble.push_str("\n\n");
        preamble.push_str(&pinned);
    }
    let mut builder = AgentBuilder::new(model)
        .preamble(preamble.clone())
        .max_tokens(u64::from(budget.max_output_tokens))
        .dynamic_tools(Vec::new());
    if config.provider == Provider::CustomResponses {
        builder = builder
            .provider_options(mio_core::completion::ProviderOptions::new().set(
                mio_core::providers::openai::extension::OpenAiOptions::default().store(false),
            ));
    }
    if let Some(temperature) = options.temperature {
        builder = builder.temperature(temperature);
    }
    if let Some(schema) = output_schema {
        builder = builder.output_schema_raw(schema);
    }
    if !matches!(
        config.provider,
        Provider::Antigravity | Provider::DeepseekWeb
    ) && let Some(thinking) = effective_thinking
    {
        builder = builder.reasoning(thinking.reasoning());
    }
    if !tools.tool_filter.is_disabled() {
        // A dynamic tool must never shadow a built-in name, however it was
        // registered, and no name may be registered twice.
        {
            let mut seen: BTreeSet<String> = BTreeSet::new();
            for tool in &tools.extra_tools {
                let name = tool.name().as_str().to_owned();
                if policy::is_reserved_tool_name(&name) {
                    return Err(format!(
                        "Tool name conflict: extra tool '{name}' uses a reserved built-in name"
                    ));
                }
                if !seen.insert(name.clone()) {
                    return Err(format!(
                        "Tool name conflict: dynamic tool '{name}' is registered more than once"
                    ));
                }
            }
        }
        // Every production built-in gets the run's real host observer, so a
        // native supervision unknown (a timed-out shell whose effect may have
        // happened) is recorded before the final outcome.
        fn scoped_with_host<T>(tool: T, run_id: Uuid, host: &Arc<dyn Host>) -> ScopedTool<T> {
            ScopedTool::new(tool, run_id).with_host(host.clone())
        }
        if tools.tool_filter.allows("workspace") {
            builder = builder.tool(scoped_with_host(
                BuiltinTool(workspace_tool.clone()),
                run_id,
                &host,
            ));
        }
        if tools.tool_filter.allows("read") {
            builder = builder.tool(scoped_with_host(
                BuiltinTool(ReadTool(workspace_tool.clone())),
                run_id,
                &host,
            ));
        }
        if tools.tool_filter.allows("grep") {
            builder = builder.tool(scoped_with_host(
                BuiltinTool(GrepTool(workspace_tool.clone())),
                run_id,
                &host,
            ));
        }
        if tools.tool_filter.allows("find") {
            builder = builder.tool(scoped_with_host(
                BuiltinTool(FindTool(workspace_tool.clone())),
                run_id,
                &host,
            ));
        }
        if tools.tool_filter.allows("ls") {
            builder = builder.tool(scoped_with_host(
                BuiltinTool(LsTool(workspace_tool.clone())),
                run_id,
                &host,
            ));
        }
        if !tools.read_only {
            if tools.tool_filter.allows("write") {
                builder = builder.tool(scoped_with_host(
                    BuiltinTool(WriteTool(workspace_tool.clone())),
                    run_id,
                    &host,
                ));
            }
            if tools.tool_filter.allows("edit") {
                builder = builder.tool(scoped_with_host(
                    BuiltinTool(EditTool(workspace_tool.clone())),
                    run_id,
                    &host,
                ));
            }
            if tools.tool_filter.allows("bash") {
                builder = builder.tool(scoped_with_host(
                    BuiltinTool(BashTool(workspace_tool.clone())),
                    run_id,
                    &host,
                ));
            }
        }
        if let Some(manager) = tools.subagents
            && tools.tool_filter.allows("subagent")
        {
            // The subagent control tool is registered through its dedicated
            // typed adapter: a successful `status` / `snapshot` / `kill`
            // snapshot may legitimately carry the child's own top-level
            // `error`, and the generic built-in JSON heuristic would misread
            // that business result as a failure of this control call.
            builder = builder.tool(scoped_with_host(
                subagent::SubagentToolAdapter::new(manager, host.clone())
                    .with_parent_run_id(run_id),
                run_id,
                &host,
            ));
        }
        if !tools.extra_tools.is_empty() {
            // Dynamic (plugin/MCP) tools get the same real identity binding as
            // the built-ins; the wrapper keeps their definition, liveness and
            // result/context metadata intact. Plugin/MCP tools report their own
            // unknown directly and never publish to the native collector, so
            // this host observer never double-reports them.
            let observer: Arc<dyn Host> = host.clone();
            builder = builder.dynamic_tools(
                tools
                    .extra_tools
                    .into_iter()
                    .map(|tool| {
                        tool_identity::scoped_dynamic_tool_with_host(
                            tool,
                            run_id,
                            Some(observer.clone()),
                        )
                    })
                    .collect(),
            );
        }
    }
    let agent = builder.build();
    let definitions = agent
        .tool_definitions(None)
        .await
        .map_err(|_| "Context budget: cannot resolve tool definitions")?;
    let input_limit = budget.input_limit();
    let budget_hook = context_budget::BudgetHook::new(budget, &preamble, &definitions)?
        .with_observer(host.clone(), run_id)
        .with_pinned_context(&options.pinned_context)
        .with_scheduler(options.scheduler.clone())
        .with_output_budget(canonical_cwd.clone(), run_id, tool_lifecycle.clone());

    let auto_compact_threshold = options
        .auto_compact_bytes
        .filter(|threshold| *threshold > 0)
        .map(|threshold| threshold.min(input_limit / 4 * 3).max(1));
    let history = if let Some(threshold) = auto_compact_threshold
        && compact::history_bytes(&history) > threshold
    {
        // Only audit and report the explicit pinned registry when its rendered
        // block can actually fit this run's input budget. An oversized block is
        // refused by the request hook below; reporting it as preserved here
        // would be a false claim.
        let pinned_fits = !options.pinned_context.is_empty()
            && options.pinned_context.instruction_bytes() <= input_limit;
        let (compacted, stats) = compact::compact_history_with_pinned(
            &history,
            compact::DEFAULT_KEEP_RECENT_MESSAGES,
            None,
            pinned_fits.then_some(&options.pinned_context),
        );
        if stats.compacted_bytes >= stats.original_bytes {
            // Automatic compaction must not spend more context than it saves.
            // An explicit summary may still be useful to a caller even when
            // its audit overhead exceeds a tiny input; retain the original
            // history here instead of applying that non-beneficial summary.
            history
        } else {
            if let Some(mut report) = stats.loss_report {
                report.pinned_items_preserved = pinned_fits.then_some(true);
                host.on_context_compaction(&compact::ContextCompactionObservation {
                    run_id,
                    turn: None,
                    agent: None,
                    kind: compact::CompactionKind::Session,
                    report,
                })?;
            }
            compacted
        }
    } else {
        history
    };

    // mio-agent's FinalResponse contains only this run's new messages, not input history.
    let mut complete_history = history.clone();
    // The run's real correlation identity. The early hook is registered first so
    // its lifecycle and pre-admission "prepared" correlation are never
    // preempted by an admission hook (an approval request is raised inside the
    // admission gate and must still be traceable), and the guard attributes a
    // dropped future to this same run UUID.
    let lifecycle = run_observation::RunLifecycle::new(run_id, host.clone());
    let _lifecycle_guard = run_observation::RunLifecycleGuard::new(lifecycle.clone());
    let mut request = agent
        .prompt(prompt)
        .history(history)
        .max_turns(options.max_turns)
        .tool_concurrency(options.tool_concurrency)
        .add_hook(run_observation::RunObservationHook::new(lifecycle.clone()));
    request = request.add_hook(budget_hook);
    if let Some(hook) = &scheduling_hook {
        request = request.add_hook(hook.clone());
    }
    // Last admission hook: approve the final patched arguments. The observers
    // that follow never rewrite or admit a call.
    request = request.add_hook(policy::PolicyHook::new(tool_policy.clone()));
    // Real dispatch observation, registered after every admission hook so a
    // denied call is never reported as dispatched; the engine still runs the
    // outcome boundary for a denial, so the refusal keeps its real ids.
    request = request.add_hook(run_observation::RunDispatchObservationHook::new(
        lifecycle,
        tool_lifecycle,
    ));
    // Measure only after scheduling and policy admission, excluding queue and
    // approval waits that happen before the real dispatch.
    request = request.add_hook(execution_timing::ExecutionTimingHook::new(
        run_id,
        host.clone(),
    ));
    let mut stream = request.stream();

    let mut final_history = None;

    while let Some(item) = stream.next().await {
        let item = match item {
            Ok(item) => item,
            Err(error) => {
                let scheduling_error = match &scheduling_hook {
                    Some(hook) => hook.failed(&error).await.err(),
                    None => None,
                };
                host.on_diagnostic(&classify_run_error(run_id, &error));
                return Err(scheduling_error.unwrap_or_else(|| public_run_error(&error)));
            }
        };
        match item {
            MultiTurnStreamItem::StreamAssistantItem(Item::Event(StreamEvent::Text {
                text,
                ..
            })) => {
                host.on_event(AgentEvent::Text(text));
            }
            MultiTurnStreamItem::StreamAssistantItem(Item::Event(StreamEvent::Reasoning {
                part,
                text,
            })) => {
                host.on_reasoning(run_id, part.index(), &text);
            }
            MultiTurnStreamItem::CompletionCall(call) => {
                host.on_completion_call(&CompletionCallObservation {
                    run_id,
                    call_index: call.call_index,
                    usage: call.usage,
                    response_id: call.response_id,
                    provider_request_id: call.provider_request_id,
                    finish_reason: call.finish_reason,
                    agent: None,
                });
            }
            MultiTurnStreamItem::FinalResponse(response) => {
                complete_history.extend(response.messages);
                final_history = Some(std::mem::take(&mut complete_history));
            }
            _ => {}
        }
    }

    final_history.ok_or_else(|| "模型连接结束，未返回完整结果".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::{
        io::Write,
        sync::{
            Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    struct TestHost {
        events: Mutex<Vec<AgentEvent>>,
        shell_dropped: Option<Arc<AtomicBool>>,
        shell_started: Option<Arc<tokio::sync::Notify>>,
    }

    impl TestHost {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
                shell_dropped: None,
                shell_started: None,
            }
        }

        fn with_drop_flag(dropped: Arc<AtomicBool>) -> Self {
            Self {
                events: Mutex::new(Vec::new()),
                shell_dropped: Some(dropped),
                shell_started: None,
            }
        }

        fn with_drop_and_notify(
            dropped: Arc<AtomicBool>,
            started: Arc<tokio::sync::Notify>,
        ) -> Self {
            Self {
                events: Mutex::new(Vec::new()),
                shell_dropped: Some(dropped),
                shell_started: Some(started),
            }
        }
    }

    struct ShellDropGuard {
        dropped: Option<Arc<AtomicBool>>,
    }

    impl Drop for ShellDropGuard {
        fn drop(&mut self) {
            if let Some(flag) = &self.dropped {
                flag.store(true, Ordering::SeqCst);
            }
        }
    }

    impl Host for TestHost {
        fn shell(&self, command: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
            let cmd = command.to_string();
            let drop_flag = self.shell_dropped.clone();
            let started = self.shell_started.clone();
            Box::pin(async move {
                let _guard = ShellDropGuard { dropped: drop_flag };
                if let Some(n) = started {
                    n.notify_waiters();
                }
                if cmd == "hang" {
                    std::future::pending::<()>().await;
                }
                Ok(format!("executed: {cmd}"))
            })
        }

        fn on_event(&self, event: AgentEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    #[tokio::test]
    async fn rig_streams_tools_and_resumes_history_in_mio() {
        let folder = tempfile::tempdir().unwrap();
        let canonical_dir = folder.path().canonicalize().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            for turn in 0..3 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let (header, request) = crate::test_http::read_json(&mut socket);
                assert!(header.starts_with("POST /v1/chat/completions "));
                tx.send(request).unwrap();
                let delta = if turn == 0 {
                    json!({"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"workspace","arguments":json!({"operation":"write","path":"result.txt","value":"你好 Rig"}).to_string()}}]})
                } else {
                    json!({"role":"assistant","content":"完成：你好"})
                };
                let chunk = |delta: Value, finish: Value| {
                    json!({"id":"test","object":"chat.completion.chunk","created":0,"model":"local-test","choices":[{"index":0,"delta":delta,"finish_reason":finish}]}).to_string()
                };
                let body = format!(
                    "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    chunk(delta, Value::Null),
                    chunk(
                        json!({}),
                        json!(if turn == 0 { "tool_calls" } else { "stop" })
                    )
                );
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });

        let host = Arc::new(TestHost::new());
        let config = Config {
            provider: Provider::Custom,
            base_url: format!("http://{address}/v1"),
            model: "local-test".into(),
            api_key: "test-only".into(),
        };

        // Turn 1 & 2: runs tool call and produces final message history
        let history = run_with_options(
            config.clone(),
            Vec::new(),
            "写入文件",
            &canonical_dir,
            host.clone(),
            Services::default(),
            RunOptions {
                instructions: "Project rule: preserve CRLF".into(),
                ..RunOptions::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(canonical_dir.join("result.txt")).unwrap(),
            "你好 Rig"
        );
        let events = host.events.lock().unwrap().clone();
        assert!(events.iter().any(
            |e| matches!(e, AgentEvent::ToolStarted { operation, .. } if operation == "write")
        ));
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolFinished {
                operation,
                target,
                result: Ok(_)
            } if operation == "write" && target == "result.txt"
        )));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Text(txt) if txt.contains("完成：你好")))
        );

        // Persist and reload the complete tool-call/result history before resuming.
        let mut saved = session::Session::new(&canonical_dir).unwrap();
        saved.history = history;
        let session_path = canonical_dir.join("session.json");
        saved.save(&session_path).unwrap();
        let history = session::Session::load(&session_path).unwrap().history;
        assert_eq!(history, saved.history);
        assert!(
            !std::fs::read_to_string(session_path)
                .unwrap()
                .contains("test-only")
        );

        // Turn 3: resumes from returned history
        let resumed_history = run(
            config,
            history,
            "继续",
            &canonical_dir,
            host.clone(),
            Services::default(),
        )
        .await
        .unwrap();
        assert!(resumed_history.starts_with(&saved.history));
        assert!(resumed_history.len() > saved.history.len());

        server.join().unwrap();
        let requests: Vec<_> = rx.try_iter().collect();
        assert_eq!(requests.len(), 3);
        assert!(
            requests[0]["messages"]
                .to_string()
                .contains("Project rule: preserve CRLF")
        );
        let second = requests[1]["messages"].as_array().unwrap();
        assert!(second.iter().any(|m| m["role"] == "tool"));
        let third = requests[2]["messages"].as_array().unwrap();
        assert!(third.iter().any(|m| m["role"] == "tool"));
        assert!(third.iter().any(|m| m.to_string().contains("写入文件")));
        assert!(!resumed_history.is_empty());
    }

    #[tokio::test]
    async fn file_tools_reject_escape_and_roundtrip_unicode() {
        let folder = tempfile::tempdir().unwrap();
        let canonical_dir = folder.path().canonicalize().unwrap();
        let host = Arc::new(TestHost::new());
        let tools = Workspace::new(canonical_dir.clone(), host).unwrap();

        assert!(
            tools
                .execute(Arguments::write("../escape.txt", "no"))
                .await
                .is_err()
        );
        tools
            .execute(Arguments::write("test.txt", "中文内容"))
            .await
            .unwrap();
        assert_eq!(
            tools.execute(Arguments::read("test.txt")).await.unwrap(),
            "中文内容"
        );

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/", canonical_dir.join("outside")).unwrap();
            assert!(checked_path(&canonical_dir, "outside/etc/passwd", false).is_err());
        }
    }

    #[tokio::test]
    async fn cancellation_drops_host_future() {
        let folder = tempfile::tempdir().unwrap();
        let canonical_dir = folder.path().canonicalize().unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let host = Arc::new(TestHost::with_drop_flag(dropped.clone()));
        let tools = Workspace::new(canonical_dir, host).unwrap();

        let mut exec_fut = Box::pin(tools.execute(Arguments::shell("hang")));

        tokio::select! {
            _ = &mut exec_fut => panic!("should not finish"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }

        assert!(!dropped.load(Ordering::SeqCst));
        drop(exec_fut);
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn run_cancellation_drops_in_flight_shell() {
        let folder = tempfile::tempdir().unwrap();
        let canonical_dir = folder.path().canonicalize().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let (header, _request) = crate::test_http::read_json(&mut socket);
            assert!(header.starts_with("POST /v1/chat/completions "));
            let delta = json!({
                "role": "assistant",
                "tool_calls": [{
                    "index": 0,
                    "id": "call_shell",
                    "type": "function",
                    "function": {
                        "name": "workspace",
                        "arguments": json!({
                            "operation": "shell",
                            "path": "",
                            "value": "hang"
                        }).to_string()
                    }
                }]
            });
            let chunk = |delta: Value, finish: Value| {
                json!({
                    "id": "test",
                    "object": "chat.completion.chunk",
                    "created": 0,
                    "model": "local-test",
                    "choices": [{
                        "index": 0,
                        "delta": delta,
                        "finish_reason": finish
                    }]
                })
                .to_string()
            };
            let body = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                chunk(delta, Value::Null),
                chunk(json!({}), json!("tool_calls"))
            );
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });

        let dropped = Arc::new(AtomicBool::new(false));
        let started = Arc::new(tokio::sync::Notify::new());
        let host = Arc::new(TestHost::with_drop_and_notify(
            dropped.clone(),
            started.clone(),
        ));
        let config = Config {
            provider: Provider::Custom,
            base_url: format!("http://{address}/v1"),
            model: "local-test".into(),
            api_key: "test-only".into(),
        };

        let mut run_fut = Box::pin(run(
            config,
            Vec::new(),
            "执行长命令",
            &canonical_dir,
            host.clone(),
            Services::default(),
        ));

        // 等待 shell 协程确定启动，无需 sleep 忙等
        tokio::select! {
            _ = &mut run_fut => panic!("run should not finish while shell hangs"),
            _ = started.notified() => {}
        }

        assert!(!dropped.load(Ordering::SeqCst));
        drop(run_fut);
        assert!(dropped.load(Ordering::SeqCst));

        server.join().unwrap();
    }
}
