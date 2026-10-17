//! The real identity of the tool call a task is executing (DES-15).
//!
//! The engine knows the exact identity of every tool dispatch: the dispatcher
//! mints the effect id, the model supplies the tool-call id and the runtime
//! knows the turn. The low-level runtime hands that triple to the tool as
//! [`mio_agent::agent::ToolInvocation`] attached to the call's runtime scope
//! (read back with `context.scope::<ToolInvocation>()`), but the host callbacks
//! that must report on the call -- [`Host::on_event`](crate::Host::on_event) and
//! the shell output capture -- never receive a [`ToolContext`] of their own.
//!
//! This module closes that gap without guessing. [`ScopedTool`] and
//! [`scoped_dynamic_tool`] read the engine's own [`ToolInvocation`] off the
//! call's context and bind it, together with the real engine run UUID, to the
//! executing task with a `tokio::task_local!` scope for exactly the length of
//! the tool body. [`current`] then answers from that binding.
//!
//! The binding is a fact, not an inference:
//!
//! - it is derived from the engine's real dispatch identity, never from tool
//!   names, argument hashes or dispatch/event ordering;
//! - when the engine supplied no [`ToolInvocation`], nothing is bound and
//!   [`current`] answers `None` -- callers must render that as `null` rather
//!   than fabricate an id;
//! - it lives on the current task only, so concurrent calls, nested scopes,
//!   `await` boundaries and dropped/cancelled futures cannot cross ids;
//! - it is not serializable and is never mixed into arguments, schemas or
//!   model-visible output.
//!
//! The same wrappers also build the DES-12 structured output references. Each
//! execution binds its own [`OutputReferenceCollector`] to the task; the shell
//! capture publishes a reference there only when its whole capture transaction
//! commits, and the wrapper appends the committed references as a trailing
//! `{"output_references": [...]}` JSON content block after the real result.
//! Unwrapped or engine-less calls bind no collector and keep their original
//! text/JSON shape, so a direct `shell::execute` still returns `String` plus
//! the versioned index entry with an honest `call_id: null`.

pub use crate::shell::output_store::OutputReference;
use crate::shell::output_store::OutputStoreError;
use crate::unknown_effect::UnknownEffectCollector;
use mio_agent::agent::ToolInvocation;
use mio_core::message::ToolResultContent;
use mio_core::tool::{
    ContextValue, DynamicTool, IntoToolOutput, Tool, ToolContext, ToolExecutionError, ToolOutput,
};
use mio_core::wasm_compat::{WasmBoxedFuture, WasmCompatSend};
use serde::{Deserialize, Serialize};
use std::{
    future::Future,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

tokio::task_local! {
    /// The identity bound to the task while a tool body runs.
    static SCOPED_TOOL: Option<ToolIdentity>;
}

/// The real identity of one tool call: the engine run, the model's tool-call
/// id, the engine turn and the dispatcher's effect id.
///
/// Values are copied out of the engine's [`ToolInvocation`] at call time; this
/// type is deliberately not `Serialize`, so it can never leak into a provider
/// request or a model-visible result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolIdentity {
    run_id: Uuid,
    tool_call_id: String,
    turn: usize,
    effect_id: u64,
}

impl ToolIdentity {
    /// Build an identity from the engine's real values.
    pub fn new(run_id: Uuid, tool_call_id: impl Into<String>, turn: usize, effect_id: u64) -> Self {
        Self {
            run_id,
            tool_call_id: tool_call_id.into(),
            turn,
            effect_id,
        }
    }

    /// The engine run UUID this call belongs to.
    pub fn run_id(&self) -> Uuid {
        self.run_id
    }

    /// The model's own tool-call id, never the tool name.
    pub fn tool_call_id(&self) -> &str {
        &self.tool_call_id
    }

    /// The engine turn this call belongs to.
    pub fn turn(&self) -> usize {
        self.turn
    }

    /// The dispatcher's minted effect id for this dispatch.
    pub fn effect_id(&self) -> u64 {
        self.effect_id
    }

    /// The turn as the observation wire type, or `None` when it does not fit.
    ///
    /// `usize` and `u64` agree on every platform this crate builds for; the
    /// checked conversion keeps an out-of-range value honest instead of
    /// truncating it.
    pub fn turn_id(&self) -> Option<u64> {
        u64::try_from(self.turn).ok()
    }

    /// Read the engine's real dispatch identity off a tool call's context.
    ///
    /// The engine attaches [`ToolInvocation`] to the call's runtime scope. When
    /// a call has no such scope (a direct, engine-less invocation), this is
    /// `None`: absence is reported, never filled in with a guess.
    pub fn from_context(run_id: Uuid, context: &ToolContext) -> Option<Self> {
        let invocation = context.scope::<ToolInvocation>()?;
        Some(Self {
            run_id,
            tool_call_id: invocation.tool_call_id.clone(),
            turn: invocation.turn,
            effect_id: invocation.effect_id,
        })
    }
}

/// Run `future` with `identity` bound to the current task. An absent identity
/// masks any outer call's identity for this future's lifetime.
///
/// The binding is scoped to the future: it is visible to every `await` inside
/// it (including the host callbacks a tool invokes) and gone once it completes
/// or is dropped. A nested call restores the identity that was bound before it.
pub async fn scope<F: Future>(identity: Option<ToolIdentity>, future: F) -> F::Output {
    SCOPED_TOOL.scope(identity, future).await
}

/// The real tool identity bound to the current task, if the engine supplied one.
///
/// Returns `None` outside a tool body and whenever the dispatch carried no
/// [`ToolInvocation`]; callers must treat that as unknown.
pub fn current() -> Option<ToolIdentity> {
    SCOPED_TOOL
        .try_with(|identity| identity.clone())
        .ok()
        .flatten()
}

/// The model tool-call id bound to the current task, if known.
pub fn current_tool_call_id() -> Option<String> {
    current().map(|identity| identity.tool_call_id)
}

/// 单次工具执行内可以发布的真实外置输出引用上限。
///
/// 每个 stdout / stderr / MCP 结果各自只产生一条引用，因此正常调用远低于该上限；
/// 触及时是 fail-closed（整次调用失败），而不是静默丢弃引用。
pub const MAX_OUTPUT_REFERENCES_PER_CALL: usize = 32;

/// Host-only typed slot carrying the committed DES-12 output references of the
/// tool call that just ran.
///
/// The tool wrapper publishes it into the real [`ToolContext`] result, so the
/// run's canonical terminal outcome can attach the same typed references
/// without parsing model-visible text. Values in this slot are host-only result
/// metadata: they never enter the model conversation, a provider request or a
/// tool argument.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommittedOutputReferences(pub Vec<OutputReference>);

impl ContextValue for CommittedOutputReferences {
    const KEY: &'static str = "mio.committed_output_references";
}

/// 引用收集失败的原因。两个变体都是保守拒绝，不包含“部分成功”。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputReferenceError {
    /// 超过单次调用的引用数量上限。
    Bounds(String),
    /// 引用的元数据非法或越界。
    Invalid(String),
}

impl std::fmt::Display for OutputReferenceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bounds(message) | Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for OutputReferenceError {}

impl From<OutputStoreError> for OutputReferenceError {
    fn from(error: OutputStoreError) -> Self {
        match error {
            OutputStoreError::Bounds(message) => Self::Bounds(message),
            other => Self::Invalid(other.to_string()),
        }
    }
}

/// 一次工具执行内、由**真实成功提交**的 capture 填写的引用收集器。
///
/// 收集器通过 [`scope_output_references`] 绑定到执行任务；`src/shell.rs` 的
/// `CaptureSession` 在事务 `commit` 时调用 [`Self::try_publish`]，因此取消或失败回滚
/// 不会留下任何已发布引用。包装器在工具体返回后用 [`Self::take`] 把引用附到模型可见
/// 结果上。
#[derive(Clone, Default)]
pub struct OutputReferenceCollector {
    references: Arc<Mutex<Vec<OutputReference>>>,
}

impl OutputReferenceCollector {
    /// 新建一个空收集器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 原子地发布一批已提交引用：字段校验或容量校验失败时一条也不发布。
    pub fn try_publish(
        &self,
        references: Vec<OutputReference>,
    ) -> Result<(), OutputReferenceError> {
        for reference in &references {
            reference.validate()?;
        }
        let mut collected = self.lock();
        let total = collected.len().saturating_add(references.len());
        if total > MAX_OUTPUT_REFERENCES_PER_CALL {
            return Err(OutputReferenceError::Bounds(format!(
                "a single tool call would collect {total} output references, above the {MAX_OUTPUT_REFERENCES_PER_CALL} limit; refusing to drop any"
            )));
        }
        collected.extend(references);
        Ok(())
    }

    /// 取走当前已提交的引用；调用方负责把它们附到模型可见结果上。
    pub fn take(&self) -> Vec<OutputReference> {
        std::mem::take(&mut *self.lock())
    }

    /// 当前已提交引用的克隆快照；不取走内容，因此包装器仍会把它们附到模型可见结果上。
    ///
    /// 唯一用途是让宿主的 `on_tool_result` 在结果落地时把真实 typed 引用写进 canonical
    /// 调用记录。它不改变发布语义，也不含原始参数或密钥。
    pub fn snapshot(&self) -> Vec<OutputReference> {
        self.lock().clone()
    }

    /// 当前已提交引用条数。
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// 当前是否没有已提交引用。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<OutputReference>> {
        self.references
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

tokio::task_local! {
    /// 绑定到执行任务的引用收集器；每个工具调用绑定自己的一个。
    static OUTPUT_REFERENCE_COLLECTOR: OutputReferenceCollector;
}

/// 在 `future` 存续期间把一个独立的引用收集器绑定到当前任务。
///
/// 每次工具执行都绑定一个**新的**收集器，所以在同一个任务上并发或嵌套调用的工具
/// 不会共享、也不会继承外层的收集器。
pub async fn scope_output_references<F: Future>(
    collector: OutputReferenceCollector,
    future: F,
) -> F::Output {
    OUTPUT_REFERENCE_COLLECTOR.scope(collector, future).await
}

/// 当前任务上由工具执行包装器绑定的引用收集器；不在任何工具执行内时为 `None`。
pub fn current_output_reference_collector() -> Option<OutputReferenceCollector> {
    OUTPUT_REFERENCE_COLLECTOR.try_with(Clone::clone).ok()
}

/// A [`Tool`] wrapper that binds the engine's real tool identity to the
/// executing task for the length of the call.
///
/// It forwards the tool's name, arguments, description and parameters unchanged,
/// and always runs the wrapped tool's own error mapping. Its [`Tool`] impl
/// presents the canonical [`ToolOutput`] so committed DES-12 output references
/// can be appended as a trailing JSON content block, and a native supervision
/// unknown recorded during the body is written into the real dispatch context
/// as a typed [`crate::unknown_effect::UnknownSideEffect`]. Direct SDK calls and erased engine dispatch
/// use the same implementation. Wrapping cannot alter the model-facing schema,
/// the admission gate, budgets, or the tool's own result and context metadata.
pub struct ScopedTool<T> {
    tool: T,
    run_id: Uuid,
    /// The production host observer for a native supervision unknown that no
    /// tool body could report (the native shell path has no `Host` of its own).
    /// A plugin/MCP tool reports its own unknown directly through its own host
    /// callback and never publishes to the collector, so this observer never
    /// reports the same fact twice.
    observer: Option<Arc<dyn crate::Host>>,
}

impl<T> ScopedTool<T> {
    /// Wrap `tool`, stamping its calls with the run's real engine UUID.
    pub fn new(tool: T, run_id: Uuid) -> Self {
        Self {
            tool,
            run_id,
            observer: None,
        }
    }

    /// Attach the production host that durably records a native supervision
    /// unknown before the final outcome. Additive: hosts that do not set it
    /// keep the typed marker in the real dispatch context and the run fence.
    pub fn with_host(mut self, host: Arc<dyn crate::Host>) -> Self {
        self.observer = Some(host);
        self
    }

    /// The wrapped tool.
    pub fn inner(&self) -> &T {
        &self.tool
    }
}

impl<T: Tool> Tool for ScopedTool<T> {
    const NAME: &'static str = T::NAME;
    type Args = T::Args;
    /// The canonical model presentation, so committed output references can be
    /// appended as a trailing JSON content block after the real result.
    type Output = ToolOutput;
    /// Already normalized: the wrapped tool's own `map_error` runs inside
    /// [`Tool::call`], so this re-publishes exactly the same error.
    type Error = ToolExecutionError;

    fn description(&self) -> String {
        self.tool.description()
    }

    fn parameters(&self) -> serde_json::Value {
        self.tool.parameters()
    }

    fn map_error(&self, error: ToolExecutionError) -> ToolExecutionError {
        error
    }

    fn call(
        &self,
        context: &mut ToolContext,
        args: Self::Args,
    ) -> impl Future<Output = Result<ToolOutput, ToolExecutionError>> + WasmCompatSend {
        let identity = ToolIdentity::from_context(self.run_id, context);
        let tool = &self.tool;
        let collector = OutputReferenceCollector::new();
        let published = collector.clone();
        let unknown = UnknownEffectCollector::new();
        let unknown_published = unknown.clone();
        let observer = self.observer.clone();
        async move {
            // The real chain (identity scope -> reference scope -> unknown scope ->
            // tool body) is polled inline. A debug build materializes each level's
            // child state on the stack, so the cost multiplies with depth and can
            // exceed a default worker/test stack even though the logical future is
            // small. Boxing the innermost body keeps every enclosing scope holding
            // a pointer instead of a copy; the wrapper still awaits exactly the
            // same typed body, so no behavior changes.
            let body = Box::pin(async move {
                let outcome = tool.call(context, args).await;
                // Read both collectors while their scopes are still
                // active, so a committed reference or a native unknown
                // can never bleed into a later call.
                let references = published.take();
                // Publish the same typed references on the real dispatch
                // context so the canonical terminal outcome can carry them
                // even when they were minted after the tool-finished event.
                if !references.is_empty() {
                    let _ = context.insert_result(CommittedOutputReferences(references.clone()));
                }
                for effect in unknown_published.take() {
                    // Trip the run fence and write the typed marker into
                    // the real dispatch context. The native path already
                    // marked the fence immediately; this is idempotent and
                    // additionally makes the canonical phase `Unknown`.
                    let fence = crate::unknown_effect::mark_unknown(context, effect.clone());
                    if let Some(host) = &observer
                        && host.on_unknown_side_effect(&effect).is_err()
                        && let Some(fence) = &fence
                    {
                        fence.mark_persist_failed();
                    }
                }
                match outcome {
                    Ok(output) => match output.into_tool_output() {
                        Ok(output) => Ok(append_output_references(output, references)),
                        Err(error) => Err(append_references_to_error(error, references)),
                    },
                    Err(error) => Err(append_references_to_error(
                        tool.map_error(error),
                        references,
                    )),
                }
            });
            scope(
                identity,
                scope_output_references(
                    collector,
                    crate::unknown_effect::scope_unknown_effects(unknown, body),
                ),
            )
            .await
        }
    }
}

/// Append the real committed output references as a trailing JSON content
/// block, keeping every original block (text, JSON, image) and their order.
fn append_output_references(output: ToolOutput, references: Vec<OutputReference>) -> ToolOutput {
    if references.is_empty() {
        return output;
    }
    let value = output_references_value(&references);
    let mut content = output.into_content();
    content.push(ToolResultContent::json(value.clone()));
    // A `ToolOutput` is never empty, so appending one block keeps it non-empty;
    // the fallback still publishes the real references rather than dropping
    // them if that invariant were ever violated.
    ToolOutput::content(content).unwrap_or_else(|_| ToolOutput::json(value))
}

/// The same append, applied to an error's model-visible output.
///
/// The error keeps its normalized kind, retryability, code, HTTP status,
/// refusal disposition and concrete source; only its model-visible blocks gain
/// the reference block. A failure is never turned into a success.
fn append_references_to_error(
    error: ToolExecutionError,
    references: Vec<OutputReference>,
) -> ToolExecutionError {
    if references.is_empty() {
        return error;
    }
    let output = append_output_references(error.model_output().clone(), references);
    error.with_model_output(output)
}

fn output_references_value(references: &[OutputReference]) -> serde_json::Value {
    serde_json::json!({ "output_references": references })
}

/// Run a dynamic tool's original handler inside the real identity scope.
///
/// A named function (rather than an inline closure) gives the callback its
/// exact higher-ranked signature: the returned boxed future borrows the call
/// context for the call's own lifetime.
fn run_scoped_dynamic<'a>(
    inner: Arc<DynamicTool>,
    run_id: Uuid,
    observer: Option<Arc<dyn crate::Host>>,
    context: &'a mut ToolContext,
    arguments: serde_json::Value,
) -> WasmBoxedFuture<'a, Result<ToolOutput, ToolExecutionError>> {
    let identity = ToolIdentity::from_context(run_id, context);
    let collector = OutputReferenceCollector::new();
    let published = collector.clone();
    let unknown = UnknownEffectCollector::new();
    let unknown_published = unknown.clone();
    Box::pin(scope(
        identity,
        scope_output_references(
            collector,
            crate::unknown_effect::scope_unknown_effects(
                unknown,
                Box::pin(async move {
                    let outcome = inner.execute_with(context, arguments).await;
                    let references = published.take();
                    if !references.is_empty() {
                        let _ =
                            context.insert_result(CommittedOutputReferences(references.clone()));
                    }
                    for effect in unknown_published.take() {
                        let fence = crate::unknown_effect::mark_unknown(context, effect.clone());
                        if let Some(host) = &observer
                            && host.on_unknown_side_effect(&effect).is_err()
                            && let Some(fence) = &fence
                        {
                            fence.mark_persist_failed();
                        }
                    }
                    match outcome {
                        Ok(output) => Ok(append_output_references(output, references)),
                        Err(error) => Err(append_references_to_error(error, references)),
                    }
                }),
            ),
        ),
    ))
}

/// Wrap a runtime-defined [`DynamicTool`] so its body runs under the same real
/// identity scope as [`ScopedTool`].
///
/// The returned tool keeps the original definition (name, description,
/// parameters), delegates liveness to the wrapped tool's own probe, and runs
/// the original handler in the caller's context, so published result metadata
/// and the canonical [`ToolOutput`] blocks are preserved exactly; only committed
/// DES-12 output references and native supervision unknowns are added.
pub fn scoped_dynamic_tool(tool: DynamicTool, run_id: Uuid) -> DynamicTool {
    scoped_dynamic_tool_with_host(tool, run_id, None)
}

/// [`scoped_dynamic_tool`] with the production host observer for native
/// supervision unknowns. Additive: passing `None` keeps the legacy shape.
pub fn scoped_dynamic_tool_with_host(
    tool: DynamicTool,
    run_id: Uuid,
    observer: Option<Arc<dyn crate::Host>>,
) -> DynamicTool {
    let definition = tool.definition();
    let inner = Arc::new(tool);
    let liveness = Arc::clone(&inner);
    DynamicTool::new_with_context(
        definition.name,
        definition.description,
        definition.parameters,
        move |context: &mut ToolContext, arguments: serde_json::Value| {
            run_scoped_dynamic(
                Arc::clone(&inner),
                run_id,
                observer.clone(),
                context,
                arguments,
            )
        },
    )
    .with_liveness(move || liveness.is_live())
}
