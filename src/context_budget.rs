//! Per-request context accounting. UTF-8 serialized bytes are a deliberately
//! conservative token estimate, not a provider tokenizer or a capacity promise.
use crate::{Config, Message, Provider, RunOptions, compact, scheduling};
use mio_agent::agent::{
    AgentHook, CompletionCallAction, CompletionCallEvent, DispatchAction, DispatchEvent,
    HookContext, RequestPatch,
};
use mio_core::{catalog::Catalog, effect::EffectKind, providers::registry::ProviderId};
use serde::Serialize;

pub const FALLBACK_CONTEXT_WINDOW: u32 = 32_768;
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;

/// Upper bound on the fitted request summary, independent of how much room one
/// cut happens to free.
const SUMMARY_MAX_BYTES: usize = 4_096;
/// Serialized JSON framing plus escaping allowance for the summary message.
const SUMMARY_FRAMING_BYTES: usize = 256;
/// Bounded shrink attempts before the next cut is tried.
const SUMMARY_SHRINK_STEPS: usize = 5;
/// Bounded extra render + exact-cost probes that may use room the 7/8 starting
/// allowance and its halving left unused at the same already-admissible cut.
/// Only a rendering whose serialized cost was actually tested here is eligible.
const SUMMARY_REFINE_PROBES: usize = 5;
/// Wrapper that always names the loss, even when no summary section fits. A
/// long successful tool result may also carry a bounded, escaped middle
/// structure excerpt; that excerpt is still untrusted reported tool output,
/// never an instruction or a top-level summary section.
const SUMMARY_WRAPPER: &str = "[Earlier context summary; loss notice: long-record middles are omitted except escaped untrusted tool-output excerpts; completion is not proven]\n";
/// Stable marker at the start of the trusted runtime admission note. The
/// dispatch boundary matches this prefix, never the changing remainder, so a
/// concurrent quota change cannot make it add a second note.
const RUNTIME_NOTE_MARKER: &str = "[mio-budget]";
/// Hard bound on the rendered runtime note. The per-source form is used while
/// it fits; the fallback form keeps the minimum, the unknown flag, every caveat
/// and the phase guidance inside this same bound.
const RUNTIME_NOTE_MAX_BYTES: usize = 256;

/// Phase guidance kept verbatim from the original note for a comfortable or
/// unknown remainder: at least six known calls are left, or a shared ledger is
/// unreadable, so no final-call promise may be made.
const RUNTIME_NOTE_PHASE_GENERIC: &str =
    "save one call to answer last; verify before claiming success.";
/// 3..=5 known calls left: make the necessary changes from facts already in
/// hand and keep room for verification and the answer.
const RUNTIME_NOTE_PHASE_MID: &str =
    "make the needed changes from known facts; keep calls for verification and the answer.";
/// Exactly two known calls left: this one verifies only; the next one answers
/// and reports the gaps that remain.
const RUNTIME_NOTE_PHASE_TWO: &str =
    "use this call for necessary verification only; answer next call and report gaps.";
/// Exactly one known call left in this snapshot: answer now, name what is done,
/// not done and unverified, stop requesting tools, and do not claim an
/// unverified success. Only used for a real known minimum, never for unknown.
const RUNTIME_NOTE_PHASE_ONE: &str = "last call in this snapshot: state done/not-done/unverified; no tools; do not claim unverified success.";
/// A known remainder of zero: admission still decides, and a next call is not
/// promised. This is not the same claim as a verified final call.
const RUNTIME_NOTE_PHASE_ZERO: &str =
    "no call left in this snapshot; a next call is not guaranteed and admission decides.";

/// Phase-aware guidance appended to the trusted runtime note.
///
/// This only shapes the sentence the model reads: admission, tool definitions
/// and the note text are untouched, and the note itself never forces
/// `tool_choice`. Only the separate R14 dispatch reservation may narrow the real
/// request's protocol-level choice on the snapshot's known final call (see
/// [`BudgetHook::reserves_final_response`]). A final-call promise is only made
/// for a real known finite `minimum`; `None` (unknown) keeps the generic
/// caveat.
fn note_phase_guidance(minimum: Option<u64>) -> &'static str {
    match minimum {
        None => RUNTIME_NOTE_PHASE_GENERIC,
        Some(0) => RUNTIME_NOTE_PHASE_ZERO,
        Some(1) => RUNTIME_NOTE_PHASE_ONE,
        Some(2) => RUNTIME_NOTE_PHASE_TWO,
        Some(3..=5) => RUNTIME_NOTE_PHASE_MID,
        Some(_) => RUNTIME_NOTE_PHASE_GENERIC,
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ContextBudget {
    pub context_window: u32,
    pub max_output_tokens: u32,
    pub source: &'static str,
}
impl ContextBudget {
    pub fn resolve(config: &Config, options: &RunOptions) -> Result<Self, String> {
        if config.provider == Provider::DeepseekWeb && options.max_output_tokens.is_some() {
            return Err(
                "Context budget: DeepSeek web does not support an explicit output token cap".into(),
            );
        }
        let (_, model, _) = crate::parse_model_spec(&config.model);
        let vendor = match config.provider {
            Provider::Deepseek => "deepseek",
            Provider::Anthropic => "anthropic",
            Provider::Google => "gcp.gemini",
            Provider::Ollama => "ollama",
            Provider::Codex => "openai",
            Provider::Antigravity if model.starts_with("claude") => "anthropic",
            Provider::Antigravity => "gcp.gemini",
            Provider::DeepseekWeb => "deepseek-web",
            Provider::Custom | Provider::CustomResponses => {
                options.catalog_provider.as_deref().unwrap_or("openai")
            }
        };
        let vendor = if vendor == "google" || vendor == "gemini" {
            "gcp.gemini"
        } else {
            vendor
        };
        let spec = ProviderId::catalog(vendor).and_then(|id| Catalog::builtin().find(id, &model));
        let known_context = spec.and_then(|spec| spec.context_window);
        let context_window = options
            .context_window
            .or(known_context)
            .unwrap_or(FALLBACK_CONTEXT_WINDOW);
        let default_output = DEFAULT_MAX_OUTPUT_TOKENS.min(context_window / 4).min(
            spec.and_then(|spec| spec.max_output_tokens)
                .unwrap_or(u32::MAX),
        );
        let max_output_tokens = options.max_output_tokens.unwrap_or(default_output);
        if context_window < 1024
            || max_output_tokens == 0
            || max_output_tokens >= context_window.saturating_sub(context_window.div_ceil(20))
        {
            return Err("Context budget: context window must be >= 1024, and output tokens must be positive and leave input room plus a 5% margin".into());
        }
        if let Some(limit) = spec.and_then(|spec| spec.max_output_tokens)
            && max_output_tokens > limit
        {
            return Err(format!(
                "Context budget: output limit {max_output_tokens} exceeds catalog maximum {limit}"
            ));
        }
        Ok(Self {
            context_window,
            max_output_tokens,
            source: if options.context_window.is_some() {
                "override"
            } else if known_context.is_some() {
                "catalog"
            } else {
                "fallback"
            },
        })
    }
    pub fn input_limit(&self) -> usize {
        (self.context_window - self.max_output_tokens - self.context_window.div_ceil(20)) as usize
    }
}

/// JSON bytes plus message framing deliberately overestimate common text tokenizers.
/// Local image data uses a fixed, conservative per-image reserve instead of
/// counting base64 transport bytes as tokens. Remote media stays unsupported.
pub(crate) fn message_cost(message: &Message) -> Result<usize, String> {
    use mio_core::message::{AssistantContent, DocumentSourceKind, ToolResultContent, UserContent};
    let mut measured = message.clone();
    let mut image_reserve = 0usize;
    let unsupported = match &mut measured {
        Message::User { content } => {
            let mut unsupported = false;
            for item in content {
                match item {
                    UserContent::Text(_) => {}
                    UserContent::ToolResult(result) => {
                        unsupported |= result
                            .content
                            .iter()
                            .any(|item| matches!(item, ToolResultContent::Image(_)));
                    }
                    UserContent::Image(image) => {
                        crate::image_input::validate_image(image)?;
                        image_reserve =
                            image_reserve.saturating_add(crate::image_input::IMAGE_CONTEXT_RESERVE);
                        image.data = DocumentSourceKind::Base64(String::new());
                    }
                    _ => unsupported = true,
                }
            }
            unsupported
        }
        Message::Assistant(message) => message
            .content
            .iter()
            .any(|content| matches!(content, AssistantContent::Image(_))),
        Message::System { .. } => false,
    };
    if unsupported {
        return Err("Context budget: media cannot be estimated; use text context".into());
    }
    serde_json::to_vec(&measured)
        .map(|bytes| bytes.len().saturating_add(16).saturating_add(image_reserve))
        .map_err(|_| "Context budget: cannot encode message".into())
}

/// Index of the newest user message whose content is only user text.
///
/// A tool result is never the user's goal, and a message that mixes text with a
/// tool result is not selected either: moving it would separate the result from
/// its call. Text-only means the verbatim re-emit in [`BudgetHook::fit`] can
/// neither duplicate a tool side effect nor orphan a tool-call pair.
fn latest_user_request_index(history: &[Message]) -> Option<usize> {
    use mio_core::message::UserContent;
    history.iter().rposition(|message| match message {
        Message::User { content } => {
            !content.is_empty()
                && content
                    .iter()
                    .all(|item| matches!(item, UserContent::Text(_)))
        }
        _ => false,
    })
}

/// Whether the prepared history shows a prior assistant turn or a trailing
/// tool result, which is the context shape the R14 reservation expects. This is
/// a predicate over the request's message context only: an assistant turn or a
/// tool result in the history does not prove the tool actually executed, nor
/// that it happened in this run, and no such claim is made. A run's very first
/// request has neither shape, so it is never eligible for the R14 reservation.
fn is_tool_exchange(history: &[Message]) -> bool {
    use mio_core::message::UserContent;
    if history
        .iter()
        .any(|message| matches!(message, Message::Assistant(_)))
    {
        return true;
    }
    history
        .iter()
        .rev()
        .find(|message| !matches!(message, Message::System { .. }))
        .is_some_and(|message| {
            matches!(message, Message::User { content }
                if !content.is_empty()
                    && content.iter().all(|item| matches!(item, UserContent::ToolResult(_))))
        })
}

/// One known cross-process model-call quota state for the trusted runtime note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SharedCallQuota {
    /// No cross-process limits are part of this run.
    NotConfigured,
    /// The shared ledger is readable and sets no model-call cap.
    Unbounded,
    /// The shared ledger could not be read; the state is unknown, never zero.
    Unknown,
    /// The shared ledger caps model calls.
    Finite { remaining: u64 },
}

impl SharedCallQuota {
    fn remaining(self) -> Option<u64> {
        match self {
            Self::Finite { remaining } => Some(remaining),
            _ => None,
        }
    }

    fn is_unknown(self) -> bool {
        matches!(self, Self::Unknown)
    }
}

/// Whether a prepared request already carries the trusted runtime note. The
/// stable marker prefix is matched, not the changing remainder. Production code
/// positions the note by filtering the exact same prefix in
/// `note_positioned_request`; this predicate is the test-side view of that rule
/// and is compiled only for the unit-test target.
#[cfg(test)]
fn request_has_runtime_note(request: &mio_core::completion::CompletionRequest) -> bool {
    request.chat_history.iter().any(|message| match message {
        Message::System { content } => content.starts_with(RUNTIME_NOTE_MARKER),
        _ => false,
    })
}

/// The canonical history with the trusted note in front. Nothing else changes,
/// so real user text, system records and tool pairs keep their order and
/// identity; the caller's history is never mutated.
fn note_prepended_history(note: Message, history: &[Message]) -> Vec<Message> {
    std::iter::once(note)
        .chain(history.iter().cloned())
        .collect()
}

/// Per-message costs already reduced by the tool-result bodies the dispatch
/// boundary will really externalize. These are byte estimates only; no
/// provisional reference is ever returned, sent to the model, journaled or
/// persisted from here.
struct PlannedCosts {
    history: Vec<usize>,
    prompt: usize,
}

pub(crate) struct BudgetHook {
    budget: ContextBudget,
    fixed_cost: usize,
    observer: Option<(std::sync::Arc<dyn crate::Host>, uuid::Uuid)>,
    pinned_items_present: bool,
    /// Bounded ID/kind snapshot of the explicit pinned registry; never its text.
    pinned_audit: Option<compact::PinnedAudit>,
    /// DES-12 整批工具输出预算。真实 `run_with_options` 会接线到本 run 的
    /// `ToolCallTracker` 与工作目录；直接构造的单元测试不接线，行为保持“装不下就
    /// Stop”的旧语义。
    output_budget: Option<crate::output_budget::BatchOutputBudget>,
    /// Real admission scheduler for this run, when configured. Only its numeric
    /// model-call quota fields are read for the trusted runtime note; no path,
    /// identity, endpoint or key is ever copied into the note.
    scheduler: Option<std::sync::Arc<scheduling::Scheduler>>,
}
impl BudgetHook {
    pub fn new(
        budget: ContextBudget,
        preamble: &str,
        tools: &[mio_core::completion::ToolDefinition],
    ) -> Result<Self, String> {
        let fixed_cost = serde_json::to_vec(&(preamble, tools))
            .map_err(|_| "Context budget: cannot encode instructions/tools")?
            .len()
            .saturating_add(512);
        Ok(Self {
            budget,
            fixed_cost,
            observer: None,
            pinned_items_present: false,
            pinned_audit: None,
            output_budget: None,
            scheduler: None,
        })
    }

    /// Attach the DES-12 batch output budget writer for this run.
    ///
    /// The writer holds the same [`crate::tool_lifecycle::ToolCallTracker`] the
    /// run's policy gate and dispatch hook share, so an externalized tool result
    /// can be registered on its real canonical call and protected from output
    /// cleanup. It never changes a canonical terminal phase.
    ///
    /// The writer also shares this hook's `fixed_cost` so the request-level and
    /// dispatch-level byte estimates are the same model: `fixed_cost`
    /// (framing + preamble + tool definitions) plus every message plus documents.
    pub(crate) fn with_output_budget(
        mut self,
        cwd: std::path::PathBuf,
        run_id: uuid::Uuid,
        tracker: std::sync::Arc<crate::tool_lifecycle::ToolCallTracker>,
    ) -> Self {
        let input_limit = self.budget.input_limit();
        let fixed_cost = self.fixed_cost;
        self.output_budget = Some(crate::output_budget::BatchOutputBudget::new(
            cwd,
            run_id,
            tracker,
            input_limit,
            fixed_cost,
        ));
        self
    }

    pub(crate) fn with_observer(
        mut self,
        host: std::sync::Arc<dyn crate::Host>,
        run_id: uuid::Uuid,
    ) -> Self {
        self.observer = Some((host, run_id));
        self
    }

    /// Attach the run's real admission scheduler so each completion can see
    /// the model-call quotas that actually gate it. Without one there is no
    /// known finite quota and no runtime note is produced.
    pub(crate) fn with_scheduler(
        mut self,
        scheduler: Option<std::sync::Arc<scheduling::Scheduler>>,
    ) -> Self {
        self.scheduler = scheduler;
        self
    }

    /// Attach the host's explicit pinned registry. Only bounded stable IDs and
    /// their kind are retained for the loss audit; entry text is never copied
    /// here. An empty registry stays `None` (unknown) rather than a false claim
    /// that no constraint exists, while the legacy `pinned_items_preserved`
    /// flag keeps its old meaning.
    pub(crate) fn with_pinned_context(
        mut self,
        pinned: &crate::context_memory::PinnedContext,
    ) -> Self {
        self.pinned_items_present = !pinned.is_empty();
        self.pinned_audit = compact::PinnedAudit::from_context(pinned);
        self
    }

    #[cfg(test)]
    pub fn fit(
        &self,
        history: &[Message],
        prompt: &Message,
    ) -> Result<Option<Vec<Message>>, String> {
        self.fit_with_report(history, prompt, 0)
            .map(|value| value.map(|(history, _)| history))
    }

    /// Fit the request against `input_limit`, reserving `reserved` bytes of
    /// real fixed cost up front. The completion boundary passes the trusted
    /// runtime note's real [`message_cost`] here, so the note is paid for
    /// before any cut is chosen and the fitted history plus the note stays
    /// inside the estimate. A direct `fit` call reserves nothing.
    fn fit_with_report(
        &self,
        history: &[Message],
        prompt: &Message,
        reserved: usize,
    ) -> Result<Option<(Vec<Message>, compact::CompactionLossReport)>, String> {
        self.fit_with_plan(history, prompt, reserved, None)
    }

    /// Per-message planning costs: the serialized cost each message reaches at
    /// the dispatch boundary after the tool-result bodies it will really
    /// externalize. These are numbers only; the provisional references behind
    /// them never enter a returned history, the model, the journal or the
    /// persistent session, and no disk is touched to compute them.
    fn planned_costs(
        &self,
        history: &[Message],
        prompt: &Message,
    ) -> Result<Option<PlannedCosts>, String> {
        let Some(budget) = self.output_budget.as_ref() else {
            return Ok(None);
        };
        let mut costs = Vec::with_capacity(history.len());
        for message in history {
            costs.push(match budget.planned_message_cost(message)? {
                Some(cost) => cost,
                None => message_cost(message)?,
            });
        }
        let prompt = match budget.planned_message_cost(prompt)? {
            Some(cost) => cost,
            None => message_cost(prompt)?,
        };
        Ok(Some(PlannedCosts {
            history: costs,
            prompt,
        }))
    }

    /// Shared request-level fit. With no `planned` costs this is the summary-only
    /// policy. With them, the same cut search runs over the costs the dispatch
    /// boundary will really pay, and every candidate is additionally verified
    /// against the exact in-memory externalization plan of the request that
    /// would be sent. The summary itself is always rendered from the original
    /// source history, never from a provisional reference.
    fn fit_with_plan(
        &self,
        history: &[Message],
        prompt: &Message,
        reserved: usize,
        planned: Option<&PlannedCosts>,
    ) -> Result<Option<(Vec<Message>, compact::CompactionLossReport)>, String> {
        if let Some(planned) = planned {
            debug_assert_eq!(planned.history.len(), history.len());
        }
        let prompt_cost = match planned {
            Some(planned) => planned.prompt,
            None => message_cost(prompt)?,
        };
        let fixed = self
            .fixed_cost
            .saturating_add(reserved)
            .saturating_add(prompt_cost);
        let limit = self.budget.input_limit();
        // Encode every message exactly once. `fit` visits several candidate
        // cuts on a long history, and each cut carries forward the system
        // messages that precede it, so their cost is accumulated as a running
        // prefix here instead of being re-encoded per candidate.
        let mut costs = vec![0usize; history.len()];
        let mut carried_system_costs = vec![0usize; history.len() + 1];
        for (index, message) in history.iter().enumerate() {
            let cost = match planned {
                Some(planned) => planned.history[index],
                None => message_cost(message)?,
            };
            costs[index] = cost;
            carried_system_costs[index + 1] = if matches!(message, Message::System { .. }) {
                carried_system_costs[index].saturating_add(cost)
            } else {
                carried_system_costs[index]
            };
        }
        let mut suffix_costs = vec![0usize; history.len() + 1];
        for index in (0..history.len()).rev() {
            suffix_costs[index] = suffix_costs[index + 1].saturating_add(costs[index]);
        }
        if fixed.saturating_add(suffix_costs[0]) <= limit {
            // With no planning costs this is the raw estimate and the request
            // really fits. With planning costs the sum of per-message reduced
            // costs is only an estimate: each message is planned on its own,
            // while the dispatch boundary plans at most
            // `MAX_BATCH_CANDIDATES` bodies over the whole request. When more
            // bodies exist than that bound, the per-message sum can fit while
            // the real whole-request plan still overflows, so taking this early
            // exit would defer to a dispatch that must refuse. Only the exact
            // whole-request plan (the same pure in-memory cost the dispatch
            // transaction computes) may take the no-summary path; otherwise the
            // safe-boundary cut search below still has to run.
            let whole_request_fits = match planned {
                None => true,
                Some(_) => match self.output_budget.as_ref() {
                    Some(budget) => {
                        let mut verify = Vec::with_capacity(history.len() + 1);
                        verify.extend(history.iter().cloned());
                        verify.push(prompt.clone());
                        matches!(
                            budget.externalized_cost(&verify, reserved),
                            Ok(Some(cost)) if cost <= limit
                        )
                    }
                    // `planned` costs only exist when an output budget is
                    // wired; without one there is no exact plan to trust.
                    None => false,
                },
            };
            if whole_request_fits {
                return Ok(None);
            }
        }
        // Preserve the newest complete suffix. If the prompt is a tool result,
        // its pending assistant call prevents cutting away that active exchange.
        // The plan is extracted once and re-rendered per candidate cut, so the
        // per-candidate work stays proportional to the kept excerpts.
        //
        // For a tool-result-only continuation, preserve the latest text request.
        // A new user prompt is already retained verbatim in `fixed`; it must not
        // pin an obsolete historical request. Older externally compacted text
        // has no recoverable provenance here; its role is never promoted.
        // A cut that would summarize the retained request re-emits the
        // exact message instead, budgeted at its real serialized cost, so a long
        // request cannot silently lose the constraints between its head and its
        // tail. If even that cannot fit, this returns the explicit budget error
        // below rather than truncating the goal or relaxing the limit.
        let continues_tools = matches!(prompt, Message::User { content }
            if !content.is_empty() && content.iter().all(|item|
                matches!(item, mio_core::message::UserContent::ToolResult(_))));
        let protected_request = if continues_tools {
            latest_user_request_index(history)
        } else {
            None
        };
        let protected_cost = match protected_request {
            Some(index) => match planned {
                Some(planned) => planned.history[index],
                None => message_cost(&history[index])?,
            },
            None => 0,
        };
        let mut plan = compact::extract_summary_plan(history);
        if let Some(index) = protected_request {
            plan.exclude_record(index);
        }
        for split in compact::safe_boundaries(history) {
            // When the cut crosses the protected request, it is carried outside
            // the summary and must be paid for on top of the retained suffix.
            let reemits_request = protected_request.is_some_and(|index| split > index);
            let retained_cost = fixed
                .saturating_add(suffix_costs[split])
                .saturating_add(carried_system_costs[split])
                .saturating_add(if reemits_request { protected_cost } else { 0 });
            if retained_cost >= limit {
                continue;
            }
            // Spend the room this cut frees, but trust only the exact
            // serialized check below: `allowance` is a starting guess that is
            // halved while the summary still overflows.
            let room = limit - retained_cost;
            let base = SUMMARY_WRAPPER.len().saturating_add(SUMMARY_FRAMING_BYTES);
            let mut allowance = room
                .saturating_sub(base)
                .saturating_mul(7)
                .saturating_div(8)
                .min(SUMMARY_MAX_BYTES);
            let mut summary = None;
            let mut accepted_allowance = 0usize;
            for _ in 0..SUMMARY_SHRINK_STEPS {
                let rendered = plan.render_audited(None, allowance, split);
                let candidate = Message::user(format!("{SUMMARY_WRAPPER}{}", rendered.text));
                let cost = message_cost(&candidate)?;
                if retained_cost.saturating_add(cost) <= limit {
                    summary = Some((candidate, rendered));
                    accepted_allowance = allowance;
                    break;
                }
                if allowance == 0 {
                    break;
                }
                allowance /= 2;
            }
            let Some((summary, rendered)) = summary else {
                continue;
            };
            // The starting allowance is 7/8 of the room after framing, and a
            // first render that overflows is halved. That leaves room at this
            // same cut for a larger rendering whose exact serialized cost was
            // never tried. Probe a bounded set of larger payloads with the
            // already-extracted plan and keep the original accepted rendering
            // as the fallback. Rendering and escaped cost need not be monotonic
            // in the payload size, so only a candidate actually tested here can
            // win; this is not a search for a global optimum.
            //
            // The refined candidate is tried first and the verified original
            // rendering second, so a failed whole-batch validation below retries
            // the same previously-admissible cut instead of skipping it. The pair
            // is a two-element iterator, not an allocation, because this runs for
            // every compacted request.
            // The evidence for refinement covers the direct summary path.
            // Keep composed externalization on its original rendering policy.
            let refined = if planned.is_none() {
                refine_summary_within_room(
                    &plan,
                    split,
                    &SummaryRoom {
                        retained_cost,
                        limit,
                        payload_room: room,
                        base,
                        accepted_allowance,
                    },
                    &rendered,
                )
            } else {
                None
            };
            // Only an accepted candidate copies its system messages and builds
            // the replacement history.
            for (candidate_summary, candidate_rendered) in refined
                .into_iter()
                .chain(std::iter::once((summary, rendered)))
            {
                let mut replacement = history[..split]
                    .iter()
                    .filter(|message| matches!(message, Message::System { .. }))
                    .cloned()
                    .collect::<Vec<_>>();
                if reemits_request && let Some(index) = protected_request {
                    replacement.push(history[index].clone());
                }
                replacement.push(candidate_summary);
                replacement.extend_from_slice(&history[split..]);
                // With planning costs, the accepted cut is only real when the
                // dispatch boundary can also fit the request it will actually send:
                // this patched history, the reserved note, and the current prompt.
                // The exact in-memory plan is authoritative; provisional metadata
                // stays inside this estimate.
                if planned.is_some()
                    && let Some(budget) = self.output_budget.as_ref()
                {
                    let mut verify = Vec::with_capacity(replacement.len() + 1);
                    verify.extend(replacement.iter().cloned());
                    verify.push(prompt.clone());
                    match budget.externalized_cost(&verify, reserved)? {
                        Some(cost) if cost <= limit => {}
                        _ => continue,
                    }
                }
                let mut report = compact::CompactionLossReport::new(
                    &history[..split],
                    &candidate_rendered.text,
                    candidate_rendered.truncated_sections,
                );
                report.pinned_items_preserved = self.pinned_items_present.then_some(true);
                // The replacement replaces the whole `event.history`, so report the
                // full before/after counts and serialized bytes, not just the
                // summarized prefix.
                report.before_messages = Some(history.len());
                report.after_messages = Some(replacement.len());
                report.before_bytes = Some(compact::history_bytes(history));
                report.after_bytes = Some(compact::history_bytes(&replacement));
                if let Some(audit) = &self.pinned_audit {
                    audit.apply(&mut report);
                }
                return Ok(Some((replacement, report)));
            }
            continue;
        }
        Err(format!(
            "Context budget: request cannot fit estimated input limit {limit} (window {}, output reserve {}). Reduce instructions, tools, prompt or tool output, or configure --context-window/--max-output-tokens for this deployment.",
            self.budget.context_window, self.budget.max_output_tokens
        ))
    }
}
/// The exact room one already-admissible safe cut has for its summary message,
/// plus the payload allowance the original accepted rendering used. Bundled so
/// the bounded refinement helper keeps a readable argument list.
struct SummaryRoom {
    retained_cost: usize,
    limit: usize,
    payload_room: usize,
    base: usize,
    accepted_allowance: usize,
}
/// Bounded R15 probe for a fitting rendering that is larger than the one the
/// 7/8 starting allowance (or its halving) selected at one already-admissible
/// cut.
///
/// `room.payload_room` is the serialized bytes this cut may spend on the summary
/// message and `room.base` is the wrapper plus framing allowance, so
/// `payload_room - base` is the largest payload the old first guess never tried.
/// The probe set is fixed and tiny; rendered and escaped cost are not monotonic
/// in the payload size, so only a candidate whose exact [`message_cost`] fits is
/// ever returned. The caller keeps the original accepted rendering as the
/// fallback and still runs the whole-batch verification on whatever wins here.
///
/// This can never make an already-admissible cut fail: a probe whose exact
/// serialized cost cannot be encoded is skipped, and the caller always still
/// has the original accepted rendering.
fn refine_summary_within_room(
    plan: &compact::SummaryPlan,
    split: usize,
    room: &SummaryRoom,
    fallback: &compact::RenderedSummary,
) -> Option<(Message, compact::RenderedSummary)> {
    let ceiling = room
        .payload_room
        .saturating_sub(room.base)
        .min(SUMMARY_MAX_BYTES);
    if room.accepted_allowance >= ceiling {
        return None;
    }
    let gap = ceiling - room.accepted_allowance;
    let mut best: Option<(Message, compact::RenderedSummary)> = None;
    let mut previous = usize::MAX;
    for step in 1..=SUMMARY_REFINE_PROBES {
        let allowance = if step == 1 {
            ceiling
        } else {
            ceiling.saturating_sub(gap.saturating_mul(step - 1) / SUMMARY_REFINE_PROBES)
        };
        // Skip a duplicate or non-larger probe instead of stopping the ladder: a
        // small gap can repeat an early value and still leave a distinct smaller
        // allowance later. The `1..=SUMMARY_REFINE_PROBES` bound still caps the
        // number of renders and exact-cost probes.
        if allowance <= room.accepted_allowance || allowance == previous {
            continue;
        }
        previous = allowance;
        let rendered = plan.render_audited(None, allowance, split);
        // A larger payload must keep at least the fallback's records and more
        // rendered text; a shorter render is never an improvement.
        if rendered.retained_records < fallback.retained_records
            || rendered.text.len() <= fallback.text.len()
        {
            continue;
        }
        let candidate = Message::user(format!("{SUMMARY_WRAPPER}{}", rendered.text));
        // A refinement that cannot even be encoded is not a fit failure: skip it
        // and keep the original accepted rendering as the cut's fallback.
        let Ok(cost) = message_cost(&candidate) else {
            continue;
        };
        if room.retained_cost.saturating_add(cost) > room.limit {
            continue;
        }
        let better = best
            .as_ref()
            .is_none_or(|(_, current)| rendered.text.len() > current.text.len());
        if better {
            best = Some((candidate, rendered));
        }
    }
    best
}
impl AgentHook for BudgetHook {
    async fn on_completion_call(
        &self,
        _: &HookContext,
        event: CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        self.completion_call_decision(event.history, event.prompt, event.turn)
    }

    /// DES-12: before the prepared provider request is dispatched, bound the
    /// whole batch of tool results (history plus the current prompt) by
    /// externalizing oversized `Text`/`Json` bodies. This is the only boundary
    /// that can rewrite the prompt without changing the no-I/O engine.
    ///
    /// This runs on **every** completion dispatch, including one the request
    /// level `fit` already accepted: the request-level and dispatch-level
    /// estimates share one model (this hook's `fixed_cost` + every message +
    /// documents), so a request that fits the request level also returns
    /// `Unchanged` here without I/O, while a genuinely stronger bound is never
    /// bypassed just to keep a threshold fixture green. Registered before the
    /// scheduling hook, this patch is what real admission then measures.
    ///
    /// The same boundary also owns the trusted runtime budget note: a note the
    /// request-level fit already reserved is relocated to the end of the sent
    /// history, and on the fallback path where the fit could not keep it a
    /// fresh note is appended there. Either way it sits in the real prepared
    /// request before the dispatch estimate runs, so the estimate charges its
    /// actual bytes instead of letting a hook paste text outside the budget.
    async fn on_dispatch(&self, _: &HookContext, event: DispatchEvent<'_>) -> DispatchAction {
        match event.kind {
            EffectKind::Completion { request, stream } => self.dispatch_decision(request, *stream),
            _ => DispatchAction::Proceed,
        }
    }
}

impl BudgetHook {
    /// The same fail-closed reason the request-level `fit` used, so a budget
    /// refusal keeps its stable `Context budget:` diagnostic category whether
    /// it was detected before or after request preparation.
    fn irreducible_message(&self, limit: usize) -> String {
        format!(
            "Context budget: request cannot fit estimated input limit {limit} (window {}, output reserve {}). Reduce instructions, tools, prompt or tool output, or configure --context-window/--max-output-tokens for this deployment.",
            self.budget.context_window, self.budget.max_output_tokens
        )
    }

    /// Local model-call quota remaining, if this run's coordinator caps calls.
    /// The snapshot is taken before this call is admitted, so a started count
    /// that already includes in-flight calls leaves the current call inside the
    /// remainder. `None` means no local finite quota.
    fn local_calls_remaining(scheduler: &scheduling::Scheduler) -> Option<u64> {
        let budget = scheduler.coordinator.snapshot().budget;
        budget
            .max_model_calls
            .map(|max| max.saturating_sub(budget.model_calls_started))
    }

    /// Cross-process model-call quota state. A ledger that cannot be read is
    /// `Unknown`, never a fabricated zero or positive remainder, and no error
    /// text, path or identity is retained.
    fn shared_calls_quota(scheduler: &scheduling::Scheduler) -> SharedCallQuota {
        let Some(shared) = scheduler.shared.as_ref() else {
            return SharedCallQuota::NotConfigured;
        };
        match shared.budget_snapshot() {
            Ok(snapshot) => match snapshot.max_model_calls {
                Some(max) => SharedCallQuota::Finite {
                    remaining: max.saturating_sub(snapshot.calls_started),
                },
                None => SharedCallQuota::Unbounded,
            },
            Err(_) => SharedCallQuota::Unknown,
        }
    }

    /// A small, bounded, trusted runtime note naming the model-call quotas that
    /// actually gate this run, the strictest known remainder, and phase-aware
    /// guidance for it. It is an admission snapshot, not a guarantee or an
    /// authorization: concurrency can lower it between this snapshot and the
    /// real admission. `None` when nothing is finite and the readable shared
    /// state is unbounded: there is no real constraint to report, so no note is
    /// injected.
    ///
    /// The phase sentence is guidance only. It never changes admission, never
    /// forces `tool_choice`, never removes tool definitions and never turns a
    /// model tool call into a final answer. (The separate R14 dispatch
    /// reservation reads the same typed quota snapshot and narrows only the
    /// real request's protocol-level `tool_choice` on the snapshot's known final
    /// call; the note text is never reinterpreted.)
    fn runtime_note(&self) -> Option<Message> {
        let scheduler = self.scheduler.as_ref()?;
        let local = Self::local_calls_remaining(scheduler);
        let shared = Self::shared_calls_quota(scheduler);
        // A known local remainder is not the effective minimum when the
        // shared constraint cannot be read. Keep that aggregate unknown.
        let minimum = if shared.is_unknown() {
            None
        } else {
            [local, shared.remaining()].into_iter().flatten().min()
        };
        if minimum.is_none() && !shared.is_unknown() {
            return None;
        }
        let mut sources = Vec::new();
        if let Some(remaining) = local {
            sources.push(format!("local {remaining}"));
        }
        match shared {
            SharedCallQuota::Finite { remaining } => {
                sources.push(format!("shared {remaining}"));
            }
            SharedCallQuota::Unknown => sources.push("shared unknown".to_owned()),
            SharedCallQuota::NotConfigured | SharedCallQuota::Unbounded => {}
        }
        // The phase follows the real known minimum; `unknown` keeps the generic
        // caveat and never claims this is the last call.
        let phase = note_phase_guidance(minimum);
        let caveat = format!(". concurrency can lower it. {phase}");
        let mut note = format!(
            "{RUNTIME_NOTE_MARKER} pre-admission snapshot incl. this call, not a guarantee or authorization. calls left: {}",
            sources.join(", ")
        );
        if let Some(minimum) = minimum {
            note.push_str(&format!(" (min {minimum})"));
        } else if shared.is_unknown() {
            note.push_str(" (min unknown)");
        }
        note.push_str(&caveat);
        if note.len() > RUNTIME_NOTE_MAX_BYTES {
            // The per-source breakdown is a convenience; the minimum, the
            // unknown flag, every caveat and the phase guidance must survive
            // the hard bound.
            let mut compact = String::from(RUNTIME_NOTE_MARKER);
            compact
                .push_str(" pre-admission snapshot incl. this call, not a guarantee or authorization. calls left: ");
            match minimum {
                Some(minimum) => compact.push_str(&format!("min {minimum}")),
                None => compact.push_str("unknown"),
            }
            compact.push_str(&caveat);
            note = compact;
        }
        debug_assert!(note.len() <= RUNTIME_NOTE_MAX_BYTES);
        Some(Message::system(note))
    }

    /// R13: place the trusted runtime note at the very end of the prepared
    /// request's `chat_history`, after every current tool result and after the
    /// current prompt (the last real message providers send), so the reusable
    /// history prefix stays byte-stable across turns and the newest run state
    /// sits next to the model's decision.
    ///
    /// Only a `System` message whose content starts with [`RUNTIME_NOTE_MARKER`]
    /// is a note: user text or tool data that merely contains the marker is
    /// never moved and never becomes trusted state. An existing note is moved
    /// verbatim - its phase and quota text are not recomputed, so this never
    /// re-reads the quota or changes the admission snapshot - and a fresh note
    /// is only rendered on the fallback path where the request-level fit kept
    /// none. Every other message keeps its relative order, content and identity,
    /// so the note can never land between a tool call and its result. Returns
    /// `None` when the note already is the last message or no note applies, so
    /// an unchanged request is never rewrapped.
    fn note_positioned_request(
        &self,
        request: &mio_core::completion::CompletionRequest,
    ) -> Option<mio_core::completion::CompletionRequest> {
        // Borrowed metadata scan first: the no-op fast paths below must decide
        // without deep-cloning a single message or reading the quota.
        let mut note_count = 0usize;
        let mut note_last = false;
        for (index, message) in request.chat_history.iter().enumerate() {
            if matches!(message, Message::System { content } if content.starts_with(RUNTIME_NOTE_MARKER))
            {
                note_count += 1;
                note_last = index + 1 == request.chat_history.len();
            }
        }
        if note_count == 1 && note_last {
            // The single note already is the last message: no clone, no quota
            // read and no rewrite of an identical request.
            return None;
        }
        // A fresh note applies only when the request-level fit kept none, which
        // happens only on the deferral path that requires the batch output
        // budget. It is rendered once, before the real clone; when no note is
        // available the request is left exactly as it was.
        let fresh_note = if note_count == 0 {
            self.output_budget.as_ref()?;
            Some(self.runtime_note()?)
        } else {
            None
        };
        // Real relocation: clone the request exactly once, then move its owned
        // messages into the partitioned vectors instead of cloning them again.
        // Every non-note message (including a preamble and any number of other
        // reserved notes) is preserved in order.
        let mut patched = request.clone();
        let owned = std::mem::take(&mut patched.chat_history);
        let mut notes: Vec<Message> =
            Vec::with_capacity(note_count + usize::from(fresh_note.is_some()));
        let mut rest: Vec<Message> = Vec::with_capacity(owned.len());
        for message in owned {
            if matches!(&message, Message::System { content } if content.starts_with(RUNTIME_NOTE_MARKER))
            {
                notes.push(message);
            } else {
                rest.push(message);
            }
        }
        if let Some(note) = fresh_note {
            notes.push(note);
        }
        rest.extend(notes);
        patched.chat_history = rest;
        Some(patched)
    }

    /// Insert the trusted note into a fitted replacement history and update the
    /// loss report to the real sent history. Only the after counts change; the
    /// report's source-history range and audit stay exactly as computed from
    /// the original history.
    fn with_note_in_report(
        history: Vec<Message>,
        mut report: compact::CompactionLossReport,
        note: Message,
    ) -> (Vec<Message>, compact::CompactionLossReport) {
        let mut with_note = Vec::with_capacity(history.len() + 1);
        with_note.push(note);
        with_note.extend(history);
        report.after_messages = Some(with_note.len());
        report.after_bytes = Some(compact::history_bytes(&with_note));
        (with_note, report)
    }

    /// Decide the completion-call patch: the fitted history, with the trusted
    /// runtime note inserted when one applies. The note's real message cost is
    /// reserved inside the fit, so it can never push an accepted request over
    /// the estimate.
    fn completion_call_decision(
        &self,
        history: &[Message],
        prompt: &Message,
        turn: usize,
    ) -> CompletionCallAction {
        let note = self.runtime_note();
        let reserved = match note.as_ref().map(message_cost).transpose() {
            Ok(cost) => cost.unwrap_or(0),
            Err(error) => return CompletionCallAction::Stop(error),
        };
        // R8: when the summary-only fit cannot serve the request, plan the
        // summary and the dispatch externalization together instead of leaving
        // the history uncompressed for the dispatch boundary (which can only
        // externalize, so an old non-externalizable prefix plus a large newest
        // result was needlessly irreducible). When no composed cut serves
        // either, the existing deferral stays exactly as it was.
        let fitted = match self.fit_with_report(history, prompt, reserved) {
            Ok(fitted) => Ok(fitted),
            Err(error) => match self.planned_costs(history, prompt) {
                // `Ok(None)` here means the whole request already fits once the
                // dispatch boundary externalizes: no summary is needed, so the
                // existing deferral is kept instead of inventing a patch.
                Ok(Some(planned)) => {
                    match self.fit_with_plan(history, prompt, reserved, Some(&planned)) {
                        Ok(Some(fitted)) => Ok(Some(fitted)),
                        Ok(None) | Err(_) => Err(error),
                    }
                }
                Ok(None) => Err(error),
                Err(combined) => Err(combined),
            },
        };
        match fitted {
            Ok(None) => match note {
                Some(note) => CompletionCallAction::Patch(
                    RequestPatch::new().history(note_prepended_history(note, history)),
                ),
                None => CompletionCallAction::Continue,
            },
            Ok(Some((fitted, report))) => {
                let (fitted, report) = match note {
                    Some(note) => Self::with_note_in_report(fitted, report, note),
                    None => (fitted, report),
                };
                if let Some((host, run_id)) = &self.observer
                    && let Err(error) =
                        host.on_context_compaction(&compact::ContextCompactionObservation {
                            run_id: *run_id,
                            turn: Some(turn),
                            agent: None,
                            kind: compact::CompactionKind::Request,
                            report,
                        })
                {
                    return CompletionCallAction::Stop(format!(
                        "Context compaction audit: {error}"
                    ));
                }
                CompletionCallAction::Patch(RequestPatch::new().history(fitted))
            }
            // A huge newest tool result cannot be compacted away: the prompt is
            // appended by the engine after any patched history. When the run has
            // a batch output budget wired, defer to the completion dispatch,
            // which can externalize the real tool-result bodies (including the
            // prompt) and must then carry the runtime note itself. Without one,
            // keep the old fail-closed Stop.
            Err(_) if self.output_budget.is_some() => CompletionCallAction::Continue,
            Err(error) => CompletionCallAction::Stop(error),
        }
    }

    /// R14: whether the prepared request's pre-admission quota snapshot shows
    /// the strictest known model-call remainder as exactly one during an
    /// ongoing tool exchange, so its protocol-level `tool_choice` may be
    /// narrowed to `none` and the provider asked for the model's own text
    /// answer.
    ///
    /// Eligibility is deliberately narrow and reads the typed quota snapshot,
    /// never the runtime note text. The request must advertise tools, carry no
    /// caller-chosen `tool_choice` (neither the typed field nor a raw
    /// `additional_params.tool_choice`) and show the tool-exchange context, and
    /// the strictest **known** model-call remainder in the snapshot must be
    /// exactly one with at least one local call already started. The snapshot
    /// is taken before this call is admitted, so it cannot promise that the
    /// next request will run or that this call owns the final slot; real
    /// admission still decides. Two-or-more left, zero left, an unreadable
    /// shared ledger, no finite quota at all, the initial one-slot budget and a
    /// request without tools are all left exactly as they were. An unknown
    /// shared remainder is never treated as one.
    ///
    /// This is a protocol-level hint only: tool definitions stay untouched, no
    /// text is fabricated, no tool call is stripped, and real admission still
    /// decides and may refuse; the caller's request is never mutated.
    fn reserves_final_response(&self, request: &mio_core::completion::CompletionRequest) -> bool {
        // Cheap, borrow-only guards first: no shared quota is read and no
        // message is cloned until the request is known to be a narrowable tool
        // exchange.
        if request.tools.is_empty() {
            return false;
        }
        match request.tool_choice {
            None | Some(mio_core::message::ToolChoice::Auto) => {}
            Some(
                mio_core::message::ToolChoice::None
                | mio_core::message::ToolChoice::Required
                | mio_core::message::ToolChoice::Specific { .. },
            ) => return false,
        }
        if request
            .additional_params
            .as_ref()
            .is_some_and(|params| params.get("tool_choice").is_some())
        {
            return false;
        }
        if !is_tool_exchange(&request.chat_history) {
            return false;
        }
        let Some(scheduler) = self.scheduler.as_ref() else {
            return false;
        };
        // Local counter + cap, both typed numbers. The count is a pre-admission
        // snapshot, so a remainder of one describes only that snapshot; it
        // cannot promise the next request or ownership of the final slot.
        let budget = scheduler.coordinator.snapshot().budget;
        let local = budget
            .max_model_calls
            .map(|max| max.saturating_sub(budget.model_calls_started));
        // The initial one-slot budget has no started call to reserve a later
        // slot from, and a spent local budget can never hide remainder one; in
        // both cases no shared IO is needed.
        if budget.model_calls_started == 0 || local == Some(0) {
            return false;
        }
        // Only a readable shared ledger may tighten or confirm the remainder.
        let shared = Self::shared_calls_quota(scheduler);
        if shared.is_unknown() {
            return false;
        }
        // The stricter known source wins; nothing known is never one.
        matches!(
            [local, shared.remaining()].into_iter().flatten().min(),
            Some(1)
        )
    }

    /// Bound the whole prepared request after positioning the trusted runtime
    /// note at the end of its sent history. The note is moved verbatim (or, on
    /// the fallback path, inserted) before the dispatch estimate runs, so its
    /// bytes are charged by the same budget while the sent order keeps the
    /// stable history prefix. Without an output budget there is nothing to
    /// bound, but the note is still positioned so the wire shape is the same.
    ///
    /// R14 additionally narrows the protocol-level `tool_choice` of a prepared
    /// request whose pre-admission quota snapshot shows one known call left to
    /// `none`, so the provider can return the model's own text answer instead of
    /// another tool request. This application is gated on an attached batch
    /// output budget: without one the dispatch passes the original request
    /// through untouched. The eligibility check runs before any clone and only
    /// reads the typed quota snapshot, which cannot promise admission; when the
    /// note relocation already produced an owned request the choice is set in
    /// place, otherwise the request is cloned once for the patch.
    fn dispatch_decision(
        &self,
        request: &mio_core::completion::CompletionRequest,
        stream: bool,
    ) -> DispatchAction {
        let mut positioned = self.note_positioned_request(request);
        // The reserved final choice is an output-budget dispatch behavior: with
        // no attached budget the request is passed through with its original
        // protocol choice, and the quota predicate is not even consulted.
        if self.output_budget.is_some() && self.reserves_final_response(request) {
            match positioned.as_mut() {
                Some(patched) => {
                    patched.tool_choice = Some(mio_core::message::ToolChoice::None);
                }
                None => {
                    let mut patched = request.clone();
                    patched.tool_choice = Some(mio_core::message::ToolChoice::None);
                    positioned = Some(patched);
                }
            }
        }
        let Some(budget) = self.output_budget.as_ref() else {
            return match positioned {
                Some(request) => DispatchAction::Patch(EffectKind::Completion { request, stream }),
                None => DispatchAction::Proceed,
            };
        };
        let effective = positioned.as_ref().unwrap_or(request);
        match budget.fit_request(effective) {
            Ok(crate::output_budget::FitOutcome::Unchanged) => match positioned {
                Some(request) => DispatchAction::Patch(EffectKind::Completion { request, stream }),
                None => DispatchAction::Proceed,
            },
            Ok(crate::output_budget::FitOutcome::Patched(request)) => {
                DispatchAction::Patch(EffectKind::Completion {
                    request: *request,
                    stream,
                })
            }
            Err(crate::output_budget::FitError::Irreducible { limit }) => {
                DispatchAction::stop(self.irreducible_message(limit))
            }
            Err(crate::output_budget::FitError::Message(message)) => DispatchAction::stop(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{coordinator, scheduling, shared_limits};
    use mio_core::completion::CompletionRequest;
    use mio_core::message::{
        AssistantContent, AssistantMessage, CallId, Image, ToolCall, ToolFunction, ToolName,
        ToolResultContent, UserContent,
    };
    use std::sync::Arc;
    fn config(model: &str) -> Config {
        Config {
            provider: Provider::Custom,
            model: model.into(),
            base_url: String::new(),
            api_key: String::new(),
        }
    }
    fn hook(window: u32) -> BudgetHook {
        BudgetHook::new(
            ContextBudget {
                context_window: window,
                max_output_tokens: 128,
                source: "override",
            },
            "rules",
            &[],
        )
        .unwrap()
    }
    fn call(id: &str) -> ToolCall {
        ToolCall::from_wire(
            id,
            ToolFunction::new(
                "read".try_into().unwrap(),
                serde_json::json!({"path":"file"}),
            ),
        )
    }
    fn assistant(calls: &[ToolCall]) -> Message {
        Message::Assistant(mio_core::message::AssistantMessage::new(
            calls
                .iter()
                .cloned()
                .map(AssistantContent::ToolCall)
                .collect(),
        ))
    }
    fn result(call: &ToolCall) -> Message {
        Message::User {
            content: vec![UserContent::ToolResult(
                call.result(vec![ToolResultContent::text("done")]),
            )],
        }
    }
    #[test]
    fn resolves_catalog_overrides_fallback_and_output_reserve() {
        let known = ContextBudget::resolve(&config("gpt-4.1"), &RunOptions::default()).unwrap();
        assert_eq!(known.source, "catalog");
        assert_eq!(known.context_window, 1_047_576);
        let unknown =
            ContextBudget::resolve(&config("local-unknown"), &RunOptions::default()).unwrap();
        assert_eq!(unknown.source, "fallback");
        assert_eq!(unknown.context_window, FALLBACK_CONTEXT_WINDOW);
        let opts = RunOptions {
            context_window: Some(8000),
            max_output_tokens: Some(1000),
            ..Default::default()
        };
        let explicit = ContextBudget::resolve(&config("gpt-4.1"), &opts).unwrap();
        assert_eq!(explicit.input_limit(), 6600);
        assert_eq!(explicit.source, "override");
        for (window, output) in [(0, 1), (1024, 0), (1024, 1024), (1024, 1000), (1024, 972)] {
            assert!(
                ContextBudget::resolve(
                    &config("test"),
                    &RunOptions {
                        context_window: Some(window),
                        max_output_tokens: Some(output),
                        ..Default::default()
                    }
                )
                .is_err()
            );
        }
    }
    #[test]
    fn parallel_tool_pairs_are_never_split_even_without_plain_user_boundaries() {
        let a = call("a");
        let b = call("b");
        let c = call("c");
        let history = vec![
            assistant(&[a.clone(), b.clone()]),
            result(&a),
            result(&b),
            assistant(std::slice::from_ref(&c)),
        ];
        assert_eq!(compact::safe_boundaries(&history), vec![3]);
        let mut large = history.clone();
        if let Message::User { content } = &mut large[1] {
            content[0] = UserContent::ToolResult(
                a.result(vec![ToolResultContent::text("old ".repeat(2000))]),
            );
        }
        let compacted = hook(4096).fit(&large, &result(&c)).unwrap().unwrap();
        assert_eq!(compacted.last(), history.last());
        assert_eq!(compacted.len(), 2);
        let (manual, stats) = compact::compact_history(&large, 2, None);
        assert!(stats.compacted_bytes < stats.original_bytes);
        assert_eq!(manual.last(), history.last());
    }
    #[test]
    fn counts_utf8_instructions_and_tools_and_rejects_irreducible_input() {
        let h = hook(4096);
        assert!(h.fit(&[], &Message::user("小".repeat(1500))).is_err());
        assert!(
            BudgetHook::new(h.budget.clone(), &"rule".repeat(1200), &[])
                .unwrap()
                .fit(&[], &Message::user("hello"))
                .is_err()
        );
        let definition = mio_core::completion::ToolDefinition {
            name: "large".try_into().unwrap(),
            description: "schema".repeat(1000),
            parameters: serde_json::json!({}),
        };
        assert!(
            BudgetHook::new(h.budget.clone(), "", &[definition])
                .unwrap()
                .fit(&[], &Message::user("hello"))
                .is_err()
        );
        let history = vec![
            Message::system("Retain this constraint verbatim"),
            Message::user("旧".repeat(3000)),
            Message::assistant("old answer"),
            Message::user("latest goal"),
            Message::assistant("latest finding"),
        ];
        let compacted = h
            .fit(&history, &Message::user("continue"))
            .unwrap()
            .unwrap();
        assert_eq!(compacted.first(), history.first());
        assert!(
            h.fit(
                &[Message::system("rule".repeat(2000))],
                &Message::user("continue")
            )
            .is_err()
        );
        assert!(
            h.fit(&compacted, &Message::user("continue"))
                .unwrap()
                .is_none()
        );
        assert!(
            serde_json::to_string(&compacted)
                .unwrap()
                .contains("latest goal")
        );
    }
    /// Independently recompute the estimated request cost of a candidate input:
    /// fixed hook cost plus prompt plus every message the request would send.
    fn request_cost(hook: &BudgetHook, history: &[Message], prompt: &Message) -> usize {
        history.iter().fold(
            hook.fixed_cost
                .saturating_add(message_cost(prompt).unwrap()),
            |total, message| total.saturating_add(message_cost(message).unwrap()),
        )
    }
    /// Every assistant tool call is answered by a later tool result and no tool
    /// result appears without its call.
    fn assert_tool_pairs_intact(history: &[Message]) {
        let mut pending = std::collections::BTreeSet::new();
        for message in history {
            match message {
                Message::Assistant(message) => {
                    for content in &message.content {
                        if let AssistantContent::ToolCall(call) = content {
                            assert!(pending.insert(call.id.clone()), "tool call answered twice");
                        }
                    }
                }
                Message::User { content } => {
                    for content in content {
                        if let UserContent::ToolResult(result) = content {
                            assert!(pending.remove(&result.call), "orphan tool result");
                        }
                    }
                }
                Message::System { .. } => {}
            }
        }
        assert!(
            pending.is_empty(),
            "history ends with an unanswered tool call"
        );
    }
    fn has_tool_call(history: &[Message], id: &str) -> bool {
        history.iter().any(|message| match message {
            Message::Assistant(message) => message.content.iter().any(|content| {
                matches!(content, AssistantContent::ToolCall(call) if call.id == CallId::from_wire(id))
            }),
            _ => false,
        })
    }
    #[test]
    fn every_system_message_is_carried_in_order_and_late_systems_are_not_pulled_forward() {
        let history = vec![
            Message::system("Global rules: never touch protected paths"),
            Message::user("explore ".repeat(500)),
            Message::assistant("old analysis"),
            Message::system("Deployment constraint: target Fedora"),
            Message::user("obsolete details ".repeat(1200)),
            Message::assistant("older finding"),
            Message::system("Late constraint added mid-run"),
            Message::user("current goal"),
            Message::assistant("current finding"),
        ];
        let prompt = Message::user("continue");
        let h = hook(4096);
        let compacted = h.fit(&history, &prompt).unwrap().unwrap();
        let systems = compacted
            .iter()
            .filter(|message| matches!(message, Message::System { .. }))
            .collect::<Vec<_>>();
        assert_eq!(
            systems,
            vec![
                &Message::system("Global rules: never touch protected paths"),
                &Message::system("Deployment constraint: target Fedora"),
                &Message::system("Late constraint added mid-run"),
            ]
        );
        // The summary follows the carried systems, and the retained tail is a
        // verbatim suffix of the original history.
        assert!(matches!(compacted[2], Message::User { .. }));
        assert_eq!(&compacted[3..], &history[5..]);
        assert!(compacted.len() < history.len());
        assert!(request_cost(&h, &compacted, &prompt) <= h.budget.input_limit());
    }
    #[test]
    fn selected_boundary_keeps_tool_exchanges_paired() {
        let dropped_a = call("old-a");
        let dropped_b = call("old-b");
        let live = call("live-c");
        let history = vec![
            Message::system("rules"),
            assistant(&[dropped_a.clone(), dropped_b.clone()]),
            result(&dropped_a),
            result(&dropped_b),
            Message::user("mid question"),
            assistant(std::slice::from_ref(&live)),
            Message::User {
                content: vec![UserContent::ToolResult(
                    live.result(vec![ToolResultContent::text("c".repeat(3000))]),
                )],
            },
            Message::user("latest goal"),
        ];
        let prompt = Message::user("continue");
        let h = hook(4096);
        let compacted = h.fit(&history, &prompt).unwrap().unwrap();
        assert_tool_pairs_intact(&compacted);
        // The cut landed after the live exchange, so neither its call nor its
        // result was summarized away on its own.
        assert!(!has_tool_call(&compacted, "live-c"));
        assert_eq!(compacted.last(), Some(&history[7]));
        assert_eq!(compacted.first(), Some(&Message::system("rules")));
        assert!(request_cost(&h, &compacted, &prompt) <= h.budget.input_limit());
    }
    #[test]
    fn most_recent_goal_and_evidence_survive_a_long_summarized_history() {
        let mut history = vec![Message::system("rules")];
        for index in 0..40 {
            history.push(Message::user(format!(
                "exploration {index}: {}",
                "old".repeat(60)
            )));
            history.push(Message::assistant(format!(
                "finding {index}: {}",
                "noise".repeat(60)
            )));
        }
        let goal = Message::user("Deploy the release and verify the migration");
        let evidence = Message::assistant("migration verified on staging");
        history.push(goal.clone());
        history.push(evidence.clone());
        let prompt = Message::user("continue");
        let h = hook(4096);
        let compacted = h.fit(&history, &prompt).unwrap().unwrap();
        assert!(compacted.len() < history.len());
        assert_eq!(&compacted[compacted.len() - 2..], &[goal, evidence]);
        assert_eq!(compacted.first(), Some(&Message::system("rules")));
        assert!(request_cost(&h, &compacted, &prompt) <= h.budget.input_limit());
    }
    #[test]
    fn no_op_path_accepts_exactly_the_limit_and_compacts_one_byte_over() {
        let prompt = Message::user("continue");
        let h = hook(4096);
        let limit = h.budget.input_limit();
        let padded = |pad: usize| {
            vec![
                Message::system("rules"),
                Message::user(format!("goal {}", "x".repeat(pad))),
                Message::assistant("answer"),
            ]
        };
        // ASCII padding adds exactly one serialized byte per character.
        let unit = request_cost(&h, &padded(1), &prompt);
        assert!(unit < limit);
        let exact = limit - unit + 1;
        assert_eq!(request_cost(&h, &padded(exact), &prompt), limit);
        assert_eq!(request_cost(&h, &padded(exact + 1), &prompt), limit + 1);
        // The no-op path is inclusive: exactly at the limit still needs no patch.
        assert!(h.fit(&padded(exact), &prompt).unwrap().is_none());
        assert!(h.fit(&padded(exact - 1), &prompt).unwrap().is_none());
        // One byte over the limit must compact, not silently pass.
        let over = padded(exact + 1);
        let compacted = h.fit(&over, &prompt).unwrap().unwrap();
        assert_eq!(compacted.first(), Some(&Message::system("rules")));
        assert_eq!(compacted.last(), Some(&over[2]));
        assert!(request_cost(&h, &compacted, &prompt) <= limit);
    }
    #[test]
    fn unsupported_media_is_rejected_even_when_its_message_would_be_summarized() {
        let h = hook(4096);
        let media = Message::User {
            content: vec![UserContent::Image(Image::default())],
        };
        assert_eq!(
            h.fit(
                &[
                    Message::system("rules"),
                    media.clone(),
                    Message::user("x".repeat(9000)),
                ],
                &Message::user("continue"),
            )
            .unwrap_err(),
            "Image input requires inline base64 data"
        );
        assert_eq!(
            h.fit(
                &[
                    Message::user("x".repeat(9000)),
                    Message::User {
                        content: vec![UserContent::ToolResult(
                            call("a").result(vec![ToolResultContent::Image(Image::default())]),
                        )],
                    },
                ],
                &Message::user("continue"),
            )
            .unwrap_err(),
            "Context budget: media cannot be estimated; use text context"
        );
        assert_eq!(
            h.fit(&[Message::system("rules")], &media).unwrap_err(),
            "Image input requires inline base64 data"
        );
    }

    /// The summary message the fitted request carries, when the request was
    /// compacted.
    fn summary_text(history: &[Message]) -> Option<String> {
        history.iter().find_map(|message| match message {
            Message::User { content } => content.iter().find_map(|item| match item {
                UserContent::Text(text)
                    if text.text.contains("Context Compaction Summary")
                        || text.text.contains("Compaction Loss Report")
                        || text.text.contains("loss notice") =>
                {
                    Some(text.text.clone())
                }
                _ => None,
            }),
            _ => None,
        })
    }

    fn error_result(call: &ToolCall, body: String) -> Message {
        Message::User {
            content: vec![UserContent::ToolResult(
                call.error_result(vec![ToolResultContent::text(body)]),
            )],
        }
    }

    /// A request that overflows the estimate must be summarized without losing
    /// the early constraint, the tail of a failed tool result, the tool call
    /// identity, or the explicit loss notice - and the fitted request must
    /// still fit the unrelaxed serialized budget.
    #[test]
    fn fitted_summary_keeps_constraints_tail_failures_and_loss_notice() {
        let failed = call("fail-7");
        let mut body = "diagnostic noise ".repeat(400);
        body.push_str("FINAL FAILURE: migration step 7 rejected the payload");
        let mut history = vec![
            Message::system("global rules"),
            Message::user(
                "Constraint: preserve the public API and never write outside the workspace.",
            ),
            Message::assistant("noted"),
        ];
        for index in 0..20 {
            history.push(Message::user(format!(
                "exploration {index} {}",
                "old".repeat(90)
            )));
            history.push(Message::assistant(format!(
                "finding {index} {}",
                "noise".repeat(60)
            )));
        }
        history.push(assistant(std::slice::from_ref(&failed)));
        history.push(error_result(&failed, body));
        history.push(Message::user(
            "Unfinished goal: deploy the release and verify the migration",
        ));
        history.push(Message::assistant("migration verified on staging"));

        let prompt = Message::user("continue");
        let h = hook(4096);
        let limit = h.budget.input_limit();
        assert!(
            request_cost(&h, &history, &prompt) > limit,
            "the fixture must overflow the input estimate"
        );

        let compacted = h.fit(&history, &prompt).unwrap().unwrap();
        let summary = summary_text(&compacted).expect("a fitted request must carry a summary");
        assert!(
            summary.contains("Constraint: preserve the public API"),
            "early constraint lost: {summary}"
        );
        assert!(
            summary.contains("FINAL FAILURE: migration step 7 rejected the payload"),
            "tail of the failed tool result lost: {summary}"
        );
        assert!(
            summary.contains("fail-7"),
            "tool call identity lost: {summary}"
        );
        assert!(
            summary.contains("chars omitted"),
            "bounded head/tail excerpts must report the omitted middle: {summary}"
        );
        assert!(
            summary.contains("Compaction Loss Report"),
            "the loss report must not disappear: {summary}"
        );
        assert!(
            summary.contains("not proof of completion"),
            "the loss report must not claim completion: {summary}"
        );
        assert!(
            summary.contains("exploration 19"),
            "late summarized records must survive: {summary}"
        );
        let serialized = serde_json::to_string(&compacted).unwrap();
        assert!(serialized.contains("Unfinished goal: deploy the release"));
        assert!(serialized.contains("migration verified on staging"));
        assert_tool_pairs_intact(&compacted);
        assert!(
            request_cost(&h, &compacted, &prompt) <= limit,
            "the fitted request must stay inside the estimate"
        );
    }

    /// The complete synthetic evidence sentence for one archived user fact. It
    /// is a whole sentence, not a bare marker, and it stays short enough for the
    /// head window this fixture selects at the ceiling allowance, so a passing
    /// assertion proves that the whole sentence survived.
    fn summary_room_fact(index: usize) -> String {
        format!("Note {index:02}: the probe returned TAIL-FACT-{index:02}.")
    }

    /// The complete synthetic evidence sentence for one successful tool result.
    /// It is a whole sentence and short enough for the head window this fixture
    /// selects at the ceiling allowance.
    fn summary_room_tool_fact(index: usize) -> String {
        format!("Note {index:02}: probe saw TAIL-FACT-{index:02}.")
    }

    /// Synthetic long history for the same-cut room experiment. Every fact is a
    /// whole emitted evidence sentence placed at the *start* of its record (a
    /// marker at the very end would only prove that the tail fragment survived),
    /// and the oversized archived block makes the first already-admissible safe
    /// cut land far inside the budget, so the cut stays fixed and only the
    /// summary payload can change. The filler after each sentence keeps the
    /// per-record size the earlier fixture calibrated, so the same cut is
    /// selected. `escaping_heavy` keeps the same record shapes but makes one
    /// field body escape-dense.
    fn summary_room_history(escaping_heavy: bool) -> Vec<Message> {
        let mut history = vec![Message::system("run rules")];
        history.push(Message::user(
            "Earlier note: the published API surface stays as documented.",
        ));
        for index in 0..24 {
            let filler = if escaping_heavy && index == 18 {
                "\u{1b}".repeat(2_000)
            } else {
                "filler".repeat(45)
            };
            history.push(Message::user(format!(
                "{} {filler}",
                summary_room_fact(index)
            )));
            history.push(Message::assistant(format!(
                "Finding {index:02}: {}",
                "noise".repeat(30)
            )));
        }
        history.push(Message::assistant(format!(
            "Archived output {}",
            "w".repeat(40_000)
        )));
        history.push(Message::user("Current goal: report the current state."));
        history.push(Message::assistant("Current finding: state reported."));
        history
    }

    /// Synthetic history whose whole evidence sentence lives inside a
    /// successful tool result. It uses real tool call IDs and completed
    /// call/result pairs, keeps the system message and the latest original user
    /// goal, and ends with a pending assistant call answered by the current
    /// tool-result prompt, so the exchange is a real continuation.
    fn summary_room_tool_history() -> (Vec<Message>, Message) {
        let mut history = vec![Message::system("run rules")];
        history.push(Message::user(
            "Goal: keep the archived probe evidence for the next step.",
        ));
        for index in 0..24 {
            let probe = call(&format!("arch-{index:02}"));
            history.push(assistant(std::slice::from_ref(&probe)));
            history.push(Message::User {
                content: vec![UserContent::ToolResult(probe.result(vec![
                    ToolResultContent::text(format!(
                        "{} {}",
                        summary_room_tool_fact(index),
                        "payload".repeat(45)
                    )),
                ]))],
            });
        }
        history.push(Message::assistant(format!(
            "Archived output {}",
            "w".repeat(40_000)
        )));
        history.push(Message::user("Current goal: report the current state."));
        history.push(Message::assistant("Current finding: state reported."));
        let pending = call("pending-probe");
        history.push(assistant(std::slice::from_ref(&pending)));
        let prompt = Message::User {
            content: vec![UserContent::ToolResult(
                pending.result(vec![ToolResultContent::text("current page")]),
            )],
        };
        (history, prompt)
    }

    /// The first safe cut whose retained suffix still leaves room, exactly as
    /// the fitter scans for it. Returns the split and the serialized retained
    /// cost that cut already pays.
    fn first_admissible_summary_cut(
        hook: &BudgetHook,
        history: &[Message],
        prompt: &Message,
    ) -> (usize, usize) {
        let limit = hook.budget.input_limit();
        let fixed = hook
            .fixed_cost
            .saturating_add(message_cost(prompt).unwrap());
        let costs: Vec<usize> = history
            .iter()
            .map(|message| message_cost(message).unwrap())
            .collect();
        let mut suffix_costs = vec![0usize; history.len() + 1];
        for index in (0..history.len()).rev() {
            suffix_costs[index] = suffix_costs[index + 1].saturating_add(costs[index]);
        }
        let mut carried_system_costs = vec![0usize; history.len() + 1];
        for (index, message) in history.iter().enumerate() {
            carried_system_costs[index + 1] = if matches!(message, Message::System { .. }) {
                carried_system_costs[index].saturating_add(costs[index])
            } else {
                carried_system_costs[index]
            };
        }
        for split in compact::safe_boundaries(history) {
            let retained = fixed
                .saturating_add(suffix_costs[split])
                .saturating_add(carried_system_costs[split]);
            if retained < limit {
                return (split, retained);
            }
        }
        panic!("the fixture must admit a safe cut");
    }

    /// The admitted replacement must retain the raw original suffix of the
    /// witness cut, byte for byte, and its loss report must name exactly that
    /// cut. This is not a re-derivation of the "first retained cost below the
    /// limit" guess: it checks the messages and report the fitter actually
    /// emitted.
    fn assert_same_cut_suffix(
        history: &[Message],
        split: usize,
        compacted: &[Message],
        report: &compact::CompactionLossReport,
    ) {
        assert_eq!(
            report.source_end, split,
            "the loss report must name the witness cut"
        );
        let retained = history.len() - split;
        assert!(
            compacted.len() >= retained,
            "the replacement must still carry the {retained} retained messages"
        );
        assert_eq!(
            &compacted[compacted.len() - retained..],
            &history[split..],
            "the retained suffix must be the raw original messages"
        );
    }

    /// R15: the payload starts at 7/8 of the room after framing and an
    /// overflowing first render is halved, so room can stay unused while whole
    /// evidence sentences are dropped. This fails on the pre-R15 path: the fitted
    /// summary is missing the complete sentence
    /// `Note 18: the probe returned TAIL-FACT-18.` although a larger rendering of
    /// the same cut, at the same fixed/prompt/retained cost, keeps it whole and
    /// still fits the real serialized estimate.
    #[test]
    fn same_cut_rendering_uses_room_the_seven_eighths_allowance_left_unused() {
        let history = summary_room_history(false);
        let prompt = Message::user("continue");
        let h = hook(4096);
        let limit = h.budget.input_limit();
        assert!(
            request_cost(&h, &history, &prompt) > limit,
            "the fixture must overflow the input estimate"
        );

        // The complete original evidence sentence. A marker-only assertion would
        // still pass while the rest of the sentence was clipped away.
        let evidence = summary_room_fact(18);
        assert_eq!(evidence, "Note 18: the probe returned TAIL-FACT-18.");

        // Independent evidence that a larger rendering at this exact cut is
        // admissible: it is rendered from the precomputed source plan and paid
        // for with the real `message_cost`, then checked against the same
        // retained cost and limit the fitter uses.
        let (split, retained) = first_admissible_summary_cut(&h, &history, &prompt);
        let base = SUMMARY_WRAPPER.len().saturating_add(SUMMARY_FRAMING_BYTES);
        let ceiling = (limit - retained)
            .saturating_sub(base)
            .min(SUMMARY_MAX_BYTES);
        let plan = compact::extract_summary_plan(&history);
        let larger = plan.render_audited(None, ceiling, split);
        let larger_message = Message::user(format!("{SUMMARY_WRAPPER}{}", larger.text));
        assert!(
            larger.text.contains(&evidence),
            "the larger same-cut rendering must carry the whole sentence: {}",
            larger.text
        );
        let larger_cost = message_cost(&larger_message).unwrap();
        assert!(
            retained.saturating_add(larger_cost) <= limit,
            "the larger same-cut rendering must fit the unchanged costs"
        );

        let (compacted, report) = h.fit_with_report(&history, &prompt, 0).unwrap().unwrap();
        let summary = summary_text(&compacted).expect("a fitted request must carry a summary");
        let emitted = request_cost(&h, &compacted, &prompt);
        println!(
            "R15_POSITIVE split={split} retained={retained} room={} base={base} ceiling={ceiling} larger_cost={larger_cost} limit={limit} emitted_cost={emitted} emitted_summary_bytes={} contains_evidence={}",
            limit - retained,
            report.summary_bytes,
            summary.contains(&evidence)
        );
        assert!(
            summary.contains(&evidence),
            "the old candidate dropped the whole sentence {evidence:?}: {summary}"
        );
        assert!(
            summary.contains("Compaction Loss Report"),
            "the loss report must still be present: {summary}"
        );
        assert_same_cut_suffix(&history, split, &compacted, &report);
        assert_tool_pairs_intact(&compacted);
        assert!(emitted <= limit);
    }

    /// R15 tool-result case: the same first already-admissible cut drops the
    /// whole evidence sentence of a successful tool result under the 7/8
    /// allowance, while a larger rendering of that cut keeps it whole. The
    /// exchange is a real continuation: completed `arch-NN` call/result pairs
    /// plus a pending assistant call answered by the current tool-result prompt,
    /// with the system message and the latest original user goal retained.
    #[test]
    fn successful_tool_result_keeps_whole_evidence_sentence_at_the_same_cut() {
        let (history, prompt) = summary_room_tool_history();
        let h = hook(4096);
        let limit = h.budget.input_limit();
        assert!(
            request_cost(&h, &history, &prompt) > limit,
            "the fixture must overflow the input estimate"
        );

        let evidence = summary_room_tool_fact(17);
        assert_eq!(evidence, "Note 17: probe saw TAIL-FACT-17.");

        let (split, retained) = first_admissible_summary_cut(&h, &history, &prompt);
        let base = SUMMARY_WRAPPER.len().saturating_add(SUMMARY_FRAMING_BYTES);
        let ceiling = (limit - retained)
            .saturating_sub(base)
            .min(SUMMARY_MAX_BYTES);
        let plan = compact::extract_summary_plan(&history);
        let larger = plan.render_audited(None, ceiling, split);
        let larger_message = Message::user(format!("{SUMMARY_WRAPPER}{}", larger.text));
        assert!(
            larger.text.contains(&evidence),
            "the larger same-cut rendering must carry the whole tool-result sentence: {}",
            larger.text
        );
        let larger_cost = message_cost(&larger_message).unwrap();
        assert!(
            retained.saturating_add(larger_cost) <= limit,
            "the larger same-cut rendering must fit the unchanged costs"
        );

        let (compacted, report) = h.fit_with_report(&history, &prompt, 0).unwrap().unwrap();
        let summary = summary_text(&compacted).expect("a fitted request must carry a summary");
        let emitted = request_cost(&h, &compacted, &prompt);
        println!(
            "R15_TOOL split={split} retained={retained} room={} base={base} ceiling={ceiling} larger_cost={larger_cost} limit={limit} emitted_cost={emitted} emitted_summary_bytes={} after_messages={:?} contains_evidence={}",
            limit - retained,
            report.summary_bytes,
            report.after_messages,
            summary.contains(&evidence)
        );
        assert!(
            summary.contains(&evidence),
            "the old candidate dropped the whole tool-result sentence {evidence:?}: {summary}"
        );
        // The evidence stays a reported tool result: real call id, untrusted
        // loss notice and the honest loss report all survive.
        assert!(
            summary.contains("[call arch-17]"),
            "the successful tool result must keep its real call id: {summary}"
        );
        assert!(
            summary.contains("loss notice"),
            "the untrusted loss notice must stay: {summary}"
        );
        assert!(
            summary.contains("Compaction Loss Report"),
            "the loss report must stay: {summary}"
        );
        // The system message, the latest original user goal and the current
        // paired exchange are all present.
        assert!(
            compacted
                .iter()
                .any(|message| matches!(message, Message::System { .. }))
        );
        assert!(compacted.contains(&Message::user("Current goal: report the current state.")));
        let mut exchange = compacted.clone();
        exchange.push(prompt.clone());
        assert_tool_pairs_intact(&exchange);
        assert_same_cut_suffix(&history, split, &compacted, &report);
        assert!(emitted <= limit);
    }

    /// R15 negative control: escaping-heavy evidence serializes to more bytes
    /// than it renders because JSON escapes it, so the largest same-cut
    /// rendering does not fit. The refinement must reject exactly that oversized
    /// rendering and keep admitting a fitting one; a smaller intermediate
    /// candidate may legitimately fit sooner, so this does not assert the
    /// absence of an arbitrary marker.
    #[test]
    fn escaping_heavy_larger_same_cut_rendering_is_rejected() {
        let history = summary_room_history(true);
        let prompt = Message::user("continue");
        let h = hook(5_120);
        let limit = h.budget.input_limit();
        assert!(request_cost(&h, &history, &prompt) > limit);

        let (split, retained) = first_admissible_summary_cut(&h, &history, &prompt);
        let base = SUMMARY_WRAPPER.len().saturating_add(SUMMARY_FRAMING_BYTES);
        let ceiling = (limit - retained)
            .saturating_sub(base)
            .min(SUMMARY_MAX_BYTES);
        let plan = compact::extract_summary_plan(&history);
        let larger = plan.render_audited(None, ceiling, split);
        let larger_message = Message::user(format!("{SUMMARY_WRAPPER}{}", larger.text));
        let larger_cost = message_cost(&larger_message).unwrap();
        assert!(
            retained.saturating_add(larger_cost) > limit,
            "the escaping-heavy larger rendering must not fit"
        );

        let (compacted, report) = h.fit_with_report(&history, &prompt, 0).unwrap().unwrap();
        let summary = summary_text(&compacted).expect("a fitted request must carry a summary");
        assert_ne!(
            summary,
            format!("{SUMMARY_WRAPPER}{}", larger.text),
            "the exact unfitting rendering must not be admitted"
        );
        assert!(
            summary.contains("Compaction Loss Report"),
            "a valid admission must still carry the loss report: {summary}"
        );
        assert!(
            summary.contains(&summary_room_fact(19)),
            "the admitted baseline evidence must survive whole: {summary}"
        );
        assert_same_cut_suffix(&history, split, &compacted, &report);
        assert_tool_pairs_intact(&compacted);
        let emitted = request_cost(&h, &compacted, &prompt);
        assert!(emitted <= limit);
        println!(
            "R15_NEGATIVE split={split} retained={retained} ceiling={ceiling} rejected_cost={larger_cost} limit={limit} emitted_cost={emitted}"
        );
    }

    /// R15 boundaries: an already-fitting request still takes the no-op path,
    /// and a payload allowance already capped at `SUMMARY_MAX_BYTES` leaves no
    /// larger room to probe, so the refinement stays inert there.
    #[test]
    fn refinement_leaves_the_noop_and_capped_allowance_paths_unchanged() {
        let h = hook(4096);
        let prompt = Message::user("continue");
        let small = vec![
            Message::system("rules"),
            Message::user("hi"),
            Message::assistant("hello"),
        ];
        assert!(request_cost(&h, &small, &prompt) <= h.budget.input_limit());
        assert!(h.fit(&small, &prompt).unwrap().is_none());

        let history = summary_room_history(false);
        let wide = hook(16_384);
        let (split, retained) = first_admissible_summary_cut(&wide, &history, &prompt);
        let limit = wide.budget.input_limit();
        let base = SUMMARY_WRAPPER.len().saturating_add(SUMMARY_FRAMING_BYTES);
        let room = limit - retained;
        assert!(
            room.saturating_sub(base) > SUMMARY_MAX_BYTES,
            "the fixture must leave more than the cap of post-framing room"
        );
        let plan = compact::extract_summary_plan(&history);
        let capped = plan.render_audited(None, SUMMARY_MAX_BYTES, split);
        assert!(
            refine_summary_within_room(
                &plan,
                split,
                &SummaryRoom {
                    retained_cost: retained,
                    limit,
                    payload_room: room,
                    base,
                    accepted_allowance: SUMMARY_MAX_BYTES,
                },
                &capped,
            )
            .is_none(),
            "a capped allowance leaves no larger room to probe"
        );
    }

    /// The newest all-text user request is retained during tool continuation. When the fit
    /// summarizes the prefix that holds it, the whole message is re-emitted
    /// verbatim instead of a bounded head/tail excerpt, and the summary does
    /// not also present an untrusted excerpt of it as the goal.
    #[test]
    fn fit_reemits_the_newest_user_request_verbatim_instead_of_excerpting_it() {
        let request = Message::user(format!(
            "TASK HEAD {} MIDDLE-CONSTRAINT-42 {} TASK TAIL",
            "H".repeat(1200),
            "T".repeat(1200),
        ));
        let mut history = vec![Message::system("rules"), request.clone()];
        for index in 0..4 {
            let call = call(&format!("read-{index}"));
            history.push(assistant(std::slice::from_ref(&call)));
            history.push(Message::User {
                content: vec![UserContent::ToolResult(
                    call.result(vec![ToolResultContent::text("page content ".repeat(600))]),
                )],
            });
        }
        let current = call("current-read");
        history.push(assistant(std::slice::from_ref(&current)));
        let prompt = Message::User {
            content: vec![UserContent::ToolResult(
                current.result(vec![ToolResultContent::text("current page")]),
            )],
        };
        let h = hook(4096);
        let limit = h.budget.input_limit();
        assert!(request_cost(&h, &history, &prompt) > limit);

        let compacted = h.fit(&history, &prompt).unwrap().unwrap();
        assert!(
            compacted.contains(&request),
            "the newest user request must survive verbatim: {}",
            serde_json::to_string(&compacted).unwrap()
        );
        let summary = summary_text(&compacted).expect("a fitted request must carry a summary");
        assert!(
            !summary.contains("MIDDLE-CONSTRAINT-42") && !summary.contains("TASK HEAD"),
            "the summary must not restate the request it cannot be trusted to keep: {summary}"
        );
        assert!(
            summary.contains("loss notice") || summary.contains("Compaction Loss Report"),
            "the summarized prefix must still carry its loss notice: {summary}"
        );
        let mut exchange = compacted.clone();
        exchange.push(prompt.clone());
        assert_tool_pairs_intact(&exchange);
        assert!(request_cost(&h, &compacted, &prompt) <= limit);
    }

    /// A user request that cannot fit next to the fixed request fails the
    /// budget explicitly; the goal is never truncated and the limit is never
    /// raised.
    #[test]
    fn fit_fails_explicitly_when_the_user_request_cannot_fit() {
        let request = Message::user(format!(
            "TASK {} MIDDLE-CONSTRAINT-42 {}",
            "H".repeat(2000),
            "T".repeat(2000),
        ));
        let current = call("current-read");
        let prompt = Message::User {
            content: vec![UserContent::ToolResult(
                current.result(vec![ToolResultContent::text("current page")]),
            )],
        };
        let h = hook(1024);
        assert_eq!(h.budget.input_limit(), 844);
        let error = h
            .fit(
                &[Message::system("rules"), request, assistant(&[current])],
                &prompt,
            )
            .unwrap_err();
        assert!(
            error.contains("cannot fit estimated input limit 844"),
            "{error}"
        );
    }

    /// A new request is already carried as the prompt; a large obsolete task
    /// must not prevent the user from starting another task under the same cap.
    #[test]
    fn a_new_user_request_does_not_pin_an_oversized_previous_request() {
        let old = Message::user("obsolete task ".repeat(1000));
        let history = vec![old.clone(), Message::assistant("old answer")];
        let prompt = Message::user("New task: explain the current state.");
        let h = hook(4096);
        let fitted = h.fit(&history, &prompt).unwrap().unwrap();
        assert!(!fitted.contains(&old));
        assert!(request_cost(&h, &fitted, &prompt) <= h.budget.input_limit());
    }

    /// A tool result is never the user's goal, and a message that mixes text
    /// with a tool result is not re-emitted either, so the protected message
    /// can never duplicate a tool side effect or orphan a tool pair.
    #[test]
    fn tool_results_and_mixed_messages_are_never_treated_as_the_user_goal() {
        let tool_call = call("mixed");
        let tool_only = Message::User {
            content: vec![UserContent::ToolResult(
                tool_call.result(vec![ToolResultContent::text("payload ".repeat(400))]),
            )],
        };
        let mixed = Message::User {
            content: vec![
                UserContent::Text(mio_core::message::Text::new("looks like a goal")),
                UserContent::ToolResult(
                    tool_call.result(vec![ToolResultContent::text("payload ".repeat(400))]),
                ),
            ],
        };
        assert_eq!(
            latest_user_request_index(std::slice::from_ref(&tool_only)),
            None
        );
        assert_eq!(
            latest_user_request_index(std::slice::from_ref(&mixed)),
            None
        );
        let plain = Message::user("real goal");
        assert_eq!(
            latest_user_request_index(&[plain, mixed, tool_only]),
            Some(0)
        );
    }

    /// Small budgets must never produce invalid UTF-8, must keep the summary
    /// loss notice explicit, and must never exceed the estimate.
    #[test]
    fn small_summary_budgets_stay_utf8_safe_and_never_lose_the_loss_notice() {
        let failed = call("fail-utf8");
        let mut body = "失败诊断信息 ".repeat(300);
        body.push_str("最终失败：迁移第 7 步被拒绝");
        let mut history = vec![
            Message::system("规则：不得修改受保护文件"),
            Message::user("约束：保留现有公共 API 并验证迁移"),
        ];
        for index in 0..10 {
            history.push(Message::user(format!("目标 {index} {}", "内容".repeat(40))));
            history.push(Message::assistant(format!(
                "结论 {index} {}",
                "总结".repeat(30)
            )));
        }
        history.push(assistant(std::slice::from_ref(&failed)));
        history.push(error_result(&failed, body));
        history.push(Message::user("最近目标：部署并验证"));
        history.push(Message::assistant("最近结论：已通过"));

        let prompt = Message::user("继续");
        for window in [1024u32, 2048, 4096] {
            let h = hook(window);
            let limit = h.budget.input_limit();
            match h.fit(&history, &prompt) {
                Ok(Some(compacted)) => {
                    let summary =
                        summary_text(&compacted).expect("a fitted request must carry a summary");
                    assert!(
                        !summary.contains('\u{FFFD}'),
                        "window {window}: summary is not valid UTF-8"
                    );
                    assert!(
                        summary.contains("loss notice")
                            || summary.contains("Compaction Loss Report"),
                        "window {window}: the loss notice disappeared silently: {summary}"
                    );
                    assert_tool_pairs_intact(&compacted);
                    assert!(
                        request_cost(&h, &compacted, &prompt) <= limit,
                        "window {window}: fitted request exceeds the estimate"
                    );
                    let _ = serde_json::to_vec(&compacted).unwrap();
                }
                Ok(None) => panic!("window {window}: an overflowing request must compact"),
                Err(error) => assert!(
                    error.contains("cannot fit"),
                    "window {window}: unexpected budget error: {error}"
                ),
            }
        }
    }

    /// Fitting a request and compacting a session must never rewrite the
    /// caller's history in place.
    #[test]
    fn fit_and_session_compaction_never_mutate_the_source_history() {
        let live = call("live");
        let history = vec![
            Message::system("rules"),
            Message::user("constraint ".repeat(200)),
            Message::assistant("old answer ".repeat(200)),
            Message::user("goal ".repeat(200)),
            assistant(std::slice::from_ref(&live)),
            Message::User {
                content: vec![UserContent::ToolResult(
                    live.result(vec![ToolResultContent::text("tool output ".repeat(400))]),
                )],
            },
            Message::user("latest goal"),
            Message::assistant("latest finding"),
        ];
        let before = history.clone();
        let h = hook(2048);
        let _ = h.fit(&history, &Message::user("continue")).unwrap();
        assert_eq!(history, before, "fit must not mutate the source history");
        let _ = compact::compact_history(&history, 2, None);
        assert_eq!(
            history, before,
            "compact_history must not mutate the source history"
        );
    }

    /// An accepted request replacement reports the whole replaced history, not
    /// just the summarized prefix, and classifies the explicit pinned registry
    /// by stable ID only.
    #[test]
    fn accepted_request_replacement_reports_whole_history_and_pinned_ids() {
        use crate::context_memory::{PinnedContext, PinnedContextKind};
        let mut pinned = PinnedContext::default();
        pinned
            .insert(
                "keep-public-api",
                PinnedContextKind::Constraint,
                "Never rewrite the public API.",
            )
            .unwrap();
        pinned
            .insert(
                "finish-migration",
                PinnedContextKind::Goal,
                "Finish the session store migration.",
            )
            .unwrap();
        let mut history = vec![Message::system("rules")];
        for index in 0..20 {
            history.push(Message::user(format!("turn {index} {}", "u".repeat(300))));
            history.push(Message::assistant(format!(
                "answer {index} {}",
                "a".repeat(300)
            )));
        }
        history.push(Message::user("latest goal"));
        let prompt = Message::user("continue");
        let h = hook(4096).with_pinned_context(&pinned);
        assert!(request_cost(&h, &history, &prompt) > h.budget.input_limit());
        let (replacement, report) = h.fit_with_report(&history, &prompt, 0).unwrap().unwrap();
        assert_eq!(report.before_messages, Some(history.len()));
        assert_eq!(report.after_messages, Some(replacement.len()));
        assert_eq!(report.before_bytes, Some(compact::history_bytes(&history)));
        assert_eq!(
            report.after_bytes,
            Some(compact::history_bytes(&replacement))
        );
        assert_eq!(report.pinned_items_preserved, Some(true));
        assert_eq!(
            report.preserved_constraint_ids,
            Some(vec!["keep-public-api".to_owned()])
        );
        assert_eq!(
            report.preserved_goal_ids,
            Some(vec!["finish-migration".to_owned()])
        );
        assert_eq!(report.dropped_constraint_ids, Some(Vec::new()));
        let encoded = serde_json::to_string(&report).unwrap();
        assert!(!encoded.contains("public API"));
        assert!(!encoded.contains("session store migration"));
        assert!(request_cost(&h, &replacement, &prompt) <= h.budget.input_limit());
    }

    /// An absent or empty pinned registry must stay unknown, never a false
    /// claim that no constraint exists.
    #[test]
    fn empty_or_absent_pinned_registry_leaves_the_loss_audit_unknown() {
        use crate::context_memory::PinnedContext;
        let mut history = vec![Message::system("rules")];
        for index in 0..20 {
            history.push(Message::user(format!("turn {index} {}", "u".repeat(300))));
            history.push(Message::assistant(format!(
                "answer {index} {}",
                "a".repeat(300)
            )));
        }
        history.push(Message::user("latest goal"));
        let prompt = Message::user("continue");
        for h in [
            hook(4096).with_pinned_context(&PinnedContext::default()),
            hook(4096),
        ] {
            let (_, report) = h.fit_with_report(&history, &prompt, 0).unwrap().unwrap();
            assert_eq!(report.pinned_items_preserved, None);
            assert_eq!(report.preserved_constraint_ids, None);
            assert_eq!(report.preserved_goal_ids, None);
            assert_eq!(report.dropped_constraint_ids, None);
            assert!(report.before_messages.is_some());
            assert!(report.after_bytes.is_some());
        }
    }

    // -----------------------------------------------------------------------
    // Trusted runtime budget note (finite model-call quota visibility).
    // -----------------------------------------------------------------------

    fn note_text(hook: &BudgetHook) -> String {
        match hook.runtime_note().expect("a note must apply") {
            Message::System { content } => content,
            other => panic!("the runtime note must be a system message: {other:?}"),
        }
    }

    fn patch_history(action: &CompletionCallAction) -> Vec<Message> {
        match action {
            CompletionCallAction::Patch(patch) => patch
                .history
                .clone()
                .expect("the decision must patch the history"),
            other => panic!("expected a history patch, got {other:?}"),
        }
    }

    fn scheduler_with(
        shared: Option<shared_limits::SharedLimits>,
        max_model_calls: Option<u64>,
    ) -> Arc<scheduling::Scheduler> {
        let coordinator = coordinator::Coordinator::new(coordinator::Limits {
            max_model_calls,
            ..coordinator::Limits::default()
        })
        .unwrap();
        Arc::new(scheduling::Scheduler::from_parts(coordinator, shared))
    }

    fn finite_shared(root: &std::path::Path, max_model_calls: u64) -> shared_limits::SharedLimits {
        shared_limits::SharedLimits::new(
            root,
            shared_limits::SharedLimitsConfig {
                max_model_calls: Some(max_model_calls),
                ..shared_limits::SharedLimitsConfig::default()
            },
        )
        .unwrap()
    }

    /// The local countdown is a pre-admission snapshot: a started (or in-flight)
    /// call is already counted, and settling never refunds the call.
    #[tokio::test]
    async fn runtime_note_tracks_the_local_call_countdown() {
        let scheduler = scheduler_with(None, Some(4));
        let h = hook(4096).with_scheduler(Some(scheduler.clone()));
        let note = note_text(&h);
        assert!(note.starts_with(RUNTIME_NOTE_MARKER), "{note}");
        assert!(note.contains("local 4"), "{note}");
        assert!(note.contains("min 4"), "{note}");
        assert!(note.len() <= RUNTIME_NOTE_MAX_BYTES, "{note}");
        let permit = scheduler
            .coordinator
            .acquire_model("note-test-provider", 64)
            .await
            .unwrap();
        assert!(note_text(&h).contains("local 3"), "{}", note_text(&h));
        drop(permit);
        assert!(
            note_text(&h).contains("local 3"),
            "a settled call is still a started call: {}",
            note_text(&h)
        );
    }

    /// The note reports the strictest known finite remainder. A shared cap that
    /// is tighter than the local one wins, and an in-flight cross-process
    /// reservation is charged at reserve time, not only at settlement.
    #[tokio::test]
    async fn runtime_note_uses_the_stricter_shared_minimum_and_counts_in_flight() {
        let root = tempfile::tempdir().unwrap();
        let shared = finite_shared(root.path(), 3);
        let h = hook(4096).with_scheduler(Some(scheduler_with(Some(shared.clone()), Some(20))));
        let note = note_text(&h);
        assert!(note.contains("local 20"), "{note}");
        assert!(note.contains("shared 3"), "{note}");
        assert!(note.contains("min 3"), "{note}");
        let permit = shared.reserve_budget(64).await.unwrap().unwrap();
        let note = note_text(&h);
        assert!(note.contains("shared 2"), "{note}");
        assert!(note.contains("min 2"), "{note}");
        drop(permit);
        assert!(
            note_text(&h).contains("shared 2"),
            "a settled call is still counted as started"
        );
    }

    /// A real prepared request with `tools` advertised, an optional tool
    /// exchange in its history, and an optional caller-chosen tool choice.
    fn reservation_request(
        choice: Option<mio_core::message::ToolChoice>,
        tools: usize,
        exchange: bool,
        raw_choice: bool,
    ) -> CompletionRequest {
        let call = call("call-reserve");
        let mut history = vec![Message::system("preamble")];
        if exchange {
            history.push(assistant(std::slice::from_ref(&call)));
            history.push(result(&call));
        } else {
            history.push(Message::user("goal"));
        }
        let mut request = CompletionRequest::new(Message::user("goal"));
        request.chat_history = history;
        request.tools = (0..tools)
            .map(|index| mio_core::completion::ToolDefinition {
                name: format!("tool{index}").try_into().unwrap(),
                description: "tool".into(),
                parameters: serde_json::json!({}),
            })
            .collect();
        request.tool_choice = choice;
        if raw_choice {
            request.additional_params = Some(serde_json::json!({"tool_choice": "required"}));
        }
        request
    }

    fn local_started(scheduler: &scheduling::Scheduler) -> u64 {
        scheduler.snapshot()["budget"]["calls"].as_u64().unwrap()
    }

    /// The same scheduler, but with enough model concurrency to hold several
    /// real call permits at once while exercising the call budget.
    fn scheduler_capacity(
        shared: Option<shared_limits::SharedLimits>,
        max_model_calls: Option<u64>,
        concurrency: usize,
    ) -> Arc<scheduling::Scheduler> {
        let coordinator = coordinator::Coordinator::new(coordinator::Limits {
            max_model_calls,
            model: concurrency,
            provider_model: concurrency,
            ..coordinator::Limits::default()
        })
        .unwrap();
        Arc::new(scheduling::Scheduler::from_parts(coordinator, shared))
    }

    /// R14: the reservation applies only to a real tool exchange whose
    /// strictest known remainder is exactly one and that already started at
    /// least one local call. Two-or-more, zero, an unknown shared ledger, no
    /// scheduler, the initial one-slot budget and a spent budget all stay
    /// unchanged, and the caller's request object is never mutated.
    #[tokio::test]
    async fn final_response_reservation_requires_a_known_remainder_of_exactly_one() {
        let request = reservation_request(None, 4, true, false);
        // No scheduler: nothing is known, so nothing changes.
        assert!(!hook(4096).reserves_final_response(&request));

        // Local cap 3: the three-slot start and the two-left state are not
        // eligible; exactly one snapshot-eligible call is; a spent budget never is.
        let scheduler = scheduler_capacity(None, Some(3), 8);
        let h = hook(4096).with_scheduler(Some(scheduler.clone()));
        assert_eq!(local_started(&scheduler), 0);
        assert!(
            !h.reserves_final_response(&request),
            "the initial call still has every slot"
        );
        let first = scheduler
            .coordinator
            .acquire_model("reserve-provider", 1)
            .await
            .unwrap();
        assert_eq!(local_started(&scheduler), 1);
        assert!(
            !h.reserves_final_response(&request),
            "two calls remain, so this is not the final slot"
        );
        let second = scheduler
            .coordinator
            .acquire_model("reserve-provider", 1)
            .await
            .unwrap();
        assert_eq!(local_started(&scheduler), 2);
        assert!(
            h.reserves_final_response(&request),
            "the snapshot shows exactly one call left"
        );
        let third = scheduler
            .coordinator
            .acquire_model("reserve-provider", 1)
            .await
            .unwrap();
        assert!(
            !h.reserves_final_response(&request),
            "a spent budget has no final slot to reserve"
        );
        drop((first, second, third));

        // The legacy one-slot budget keeps its first call unharrowed; nothing
        // is left after it, so no later slot can be reserved.
        let legacy = scheduler_capacity(None, Some(1), 8);
        let h = hook(4096).with_scheduler(Some(legacy.clone()));
        assert!(!h.reserves_final_response(&request));
        let only = legacy
            .coordinator
            .acquire_model("reserve-provider", 1)
            .await
            .unwrap();
        assert!(!h.reserves_final_response(&request));
        drop(only);
    }

    /// The stricter **known** source wins: a shared remainder of one beside a
    /// wider local cap is the final slot, and a wider shared remainder cannot
    /// widen a spent local cap. No shared IO happens before a local call has
    /// started.
    #[tokio::test]
    async fn final_response_reservation_takes_the_stricter_shared_minimum() {
        let request = reservation_request(None, 4, true, false);
        // local 20 vs shared 1: the shared cap is the known final slot.
        let root = tempfile::tempdir().unwrap();
        let shared = finite_shared(root.path(), 1);
        let scheduler = scheduler_capacity(Some(shared.clone()), Some(20), 8);
        let h = hook(4096).with_scheduler(Some(scheduler.clone()));
        assert!(!h.reserves_final_response(&request));
        let local = scheduler
            .coordinator
            .acquire_model("reserve-provider", 1)
            .await
            .unwrap();
        assert!(
            h.reserves_final_response(&request),
            "shared 1 is stricter than local 20"
        );
        drop(local);

        // local 20 vs shared 2 with one shared call already started: the
        // stricter shared remainder is one.
        let root = tempfile::tempdir().unwrap();
        let shared = finite_shared(root.path(), 2);
        let scheduler = scheduler_capacity(Some(shared.clone()), Some(20), 8);
        let h = hook(4096).with_scheduler(Some(scheduler.clone()));
        assert!(
            !h.reserves_final_response(&request),
            "a local call must have started first"
        );
        let local = scheduler
            .coordinator
            .acquire_model("reserve-provider", 1)
            .await
            .unwrap();
        assert!(
            !h.reserves_final_response(&request),
            "the shared remainder is still two"
        );
        let shared_permit = shared.reserve_budget(1).await.unwrap().unwrap();
        assert!(
            h.reserves_final_response(&request),
            "the stricter shared remainder is now one"
        );
        drop((local, shared_permit));

        // A wider shared cap cannot widen a spent local cap.
        let root = tempfile::tempdir().unwrap();
        let shared = finite_shared(root.path(), 20);
        let scheduler = scheduler_capacity(Some(shared), Some(2), 8);
        let h = hook(4096).with_scheduler(Some(scheduler.clone()));
        let first = scheduler
            .coordinator
            .acquire_model("reserve-provider", 1)
            .await
            .unwrap();
        assert!(
            h.reserves_final_response(&request),
            "one local call remains beside twenty shared"
        );
        let second = scheduler
            .coordinator
            .acquire_model("reserve-provider", 1)
            .await
            .unwrap();
        assert!(
            !h.reserves_final_response(&request),
            "the local cap is spent, so no final slot remains"
        );
        drop((first, second));
    }

    /// An unreadable shared ledger is unknown and is never fabricated as a
    /// remainder of one, even when the local remainder alone would be one.
    #[tokio::test]
    async fn final_response_reservation_never_fabricates_one_from_unknown_shared_state() {
        let root = tempfile::tempdir().unwrap();
        let shared = finite_shared(root.path(), 5);
        std::fs::write(root.path().join("budget.json"), b"{not-json").unwrap();
        let scheduler = scheduler_capacity(Some(shared), Some(2), 8);
        let h = hook(4096).with_scheduler(Some(scheduler.clone()));
        let request = reservation_request(None, 4, true, false);
        assert!(!h.reserves_final_response(&request));
        let permit = scheduler
            .coordinator
            .acquire_model("reserve-provider", 1)
            .await
            .unwrap();
        assert!(
            !h.reserves_final_response(&request),
            "an unreadable shared ledger is unknown, never one"
        );
        drop(permit);
    }

    /// Explicit caller intent, a raw `additional_params.tool_choice`, a missing
    /// tool set and a request with no tool exchange all suppress the
    /// reservation even beside a known final slot.
    #[tokio::test]
    async fn final_response_reservation_respects_explicit_choices_tools_and_exchange() {
        use mio_core::message::{ToolChoice, ToolName};
        let scheduler = scheduler_capacity(None, Some(3), 8);
        let h = hook(4096).with_scheduler(Some(scheduler.clone()));
        let _a = scheduler
            .coordinator
            .acquire_model("reserve-provider", 1)
            .await
            .unwrap();
        let _b = scheduler
            .coordinator
            .acquire_model("reserve-provider", 1)
            .await
            .unwrap();
        for choice in [
            Some(ToolChoice::Required),
            Some(ToolChoice::Specific {
                function_names: vec![ToolName::new("read").unwrap()],
            }),
            Some(ToolChoice::None),
        ] {
            assert!(
                !h.reserves_final_response(&reservation_request(choice, 4, true, false)),
                "a caller-chosen tool_choice must be respected"
            );
        }
        assert!(
            h.reserves_final_response(&reservation_request(Some(ToolChoice::Auto), 4, true, false)),
            "an explicit Auto may still be narrowed"
        );
        assert!(
            !h.reserves_final_response(
                &reservation_request(Some(ToolChoice::Auto), 4, true, true,)
            ),
            "a raw additional_params.tool_choice is respected beside Auto"
        );
        assert!(
            !h.reserves_final_response(&reservation_request(None, 0, true, false)),
            "a request without tools cannot reserve a text answer"
        );
        assert!(
            !h.reserves_final_response(&reservation_request(None, 4, false, false)),
            "a first request with no tool exchange is never eligible"
        );
    }

    /// At the real dispatch boundary the eligible request carries the narrowed
    /// choice on the returned patch, the caller's request stays byte-equal, and
    /// an ineligible request is left alone. The output budget is attached
    /// because the R14 narrowing is an output-budget dispatch behavior.
    #[tokio::test]
    async fn dispatch_narrows_only_the_eligible_final_call_choice() {
        let scheduler = scheduler_capacity(None, Some(3), 8);
        let dir = tempfile::tempdir().unwrap();
        let run_id = uuid::Uuid::new_v4();
        let (tracker, _call) = tracked_eval_call(run_id, "call-dispatch-reserve");
        let h = hook(4096)
            .with_scheduler(Some(scheduler.clone()))
            .with_output_budget(dir.path().to_path_buf(), run_id, tracker);
        let _a = scheduler
            .coordinator
            .acquire_model("reserve-provider", 1)
            .await
            .unwrap();
        let _b = scheduler
            .coordinator
            .acquire_model("reserve-provider", 1)
            .await
            .unwrap();
        // A trailing real runtime note keeps the note relocation a no-op, so
        // the history assertion below isolates the R14 choice patch.
        let mut request = reservation_request(None, 4, true, false);
        request.chat_history.push(
            h.runtime_note()
                .expect("the one-call-left snapshot renders a real note"),
        );
        let before = request.clone();
        match h.dispatch_decision(&request, false) {
            DispatchAction::Patch(EffectKind::Completion {
                request: patched,
                stream,
            }) => {
                assert!(!stream);
                assert_eq!(
                    patched.tool_choice,
                    Some(mio_core::message::ToolChoice::None)
                );
                assert_eq!(
                    patched.tools, before.tools,
                    "tool definitions are unchanged"
                );
                assert_eq!(
                    patched.chat_history, before.chat_history,
                    "no message is added, removed or reordered"
                );
            }
            other => panic!("the eligible final call must be narrowed: {other:?}"),
        }
        assert_eq!(
            request.chat_history, before.chat_history,
            "the caller's request history is never mutated"
        );
        assert_eq!(request.tools, before.tools);
        assert_eq!(request.tool_choice, before.tool_choice);

        // Two calls left: the dispatch boundary leaves the request untouched.
        let wider = scheduler_capacity(None, Some(4), 8);
        let h = hook(4096).with_scheduler(Some(wider.clone()));
        let _c = wider
            .coordinator
            .acquire_model("reserve-provider", 1)
            .await
            .unwrap();
        let _d = wider
            .coordinator
            .acquire_model("reserve-provider", 1)
            .await
            .unwrap();
        assert!(matches!(
            h.dispatch_decision(&reservation_request(None, 4, true, false), false),
            DispatchAction::Proceed
        ));
    }

    /// Without an attached batch output budget the dispatch boundary reserves
    /// nothing: the same scheduler snapshot that the predicate above reports as
    /// eligible is left completely untouched, because the R14 reservation may
    /// narrow the real request only on the output-budget dispatch path. This is
    /// the negative control for the presence guard; the helper-level quota
    /// predicate itself stays pure.
    #[tokio::test]
    async fn dispatch_without_an_output_budget_keeps_the_provider_choice() {
        let scheduler = scheduler_capacity(None, Some(3), 8);
        let h = hook(4096).with_scheduler(Some(scheduler.clone()));
        assert!(
            h.output_budget.is_none(),
            "the fixture must have no attached output budget"
        );
        let _a = scheduler
            .coordinator
            .acquire_model("reserve-provider", 1)
            .await
            .unwrap();
        let _b = scheduler
            .coordinator
            .acquire_model("reserve-provider", 1)
            .await
            .unwrap();
        let request = reservation_request(None, 4, true, false);
        let before = request.clone();
        // The pure quota predicate still reports one known call left, so only
        // the missing output budget can keep the dispatch choice unchanged.
        assert!(
            h.reserves_final_response(&request),
            "the fixture snapshot must be predicate-eligible"
        );
        match h.dispatch_decision(&request, false) {
            DispatchAction::Proceed => {}
            other => panic!("no-budget dispatch must keep the original choice: {other:?}"),
        }
        assert_eq!(request.tool_choice, before.tool_choice);
        assert_eq!(request.tools, before.tools);
        assert_eq!(request.chat_history, before.chat_history);
    }

    /// The narrowed protocol field is tiny and changes no message byte: the
    /// reserved `tool_choice` value sits inside the same 512-byte framing
    /// allowance the request budget already carries, so the real fit verdict is
    /// unchanged.
    #[tokio::test]
    async fn the_narrowed_choice_stays_inside_the_reserved_framing_allowance() {
        let scheduler = scheduler_capacity(None, Some(3), 8);
        let dir = tempfile::tempdir().unwrap();
        let run_id = uuid::Uuid::new_v4();
        let (tracker, _call) = tracked_eval_call(run_id, "call-framing-reserve");
        let h = hook(4096)
            .with_scheduler(Some(scheduler.clone()))
            .with_output_budget(dir.path().to_path_buf(), run_id, tracker);
        let _a = scheduler
            .coordinator
            .acquire_model("reserve-provider", 1)
            .await
            .unwrap();
        let _b = scheduler
            .coordinator
            .acquire_model("reserve-provider", 1)
            .await
            .unwrap();
        assert!(
            h.fixed_cost >= 512,
            "the request budget reserves its framing allowance"
        );
        // As above, a trailing real note keeps the dispatch a pure choice patch.
        let mut request = reservation_request(None, 4, true, false);
        request.chat_history.push(
            h.runtime_note()
                .expect("the one-call-left snapshot renders a real note"),
        );
        let before_messages = serde_json::to_vec(&request.chat_history).unwrap().len();
        let DispatchAction::Patch(EffectKind::Completion {
            request: patched, ..
        }) = h.dispatch_decision(&request, false)
        else {
            panic!("the eligible final call must be narrowed")
        };
        assert_eq!(
            serde_json::to_vec(&patched.chat_history).unwrap().len(),
            before_messages,
            "no message byte changes, so the fit cost is identical"
        );
        let before_choice = serde_json::to_vec(&request.tool_choice).unwrap().len();
        let after_choice = serde_json::to_vec(&patched.tool_choice).unwrap().len();
        assert!(after_choice > before_choice);
        assert!(
            after_choice - before_choice < 512,
            "the choice delta stays inside the reserved framing allowance"
        );
    }

    /// An unreadable shared ledger is reported as unknown, never as zero, a
    /// stale positive, or the ledger path.
    #[test]
    fn runtime_note_marks_unreadable_shared_state_unknown_without_leaking_paths() {
        let root = tempfile::tempdir().unwrap();
        let shared = finite_shared(root.path(), 5);
        std::fs::write(root.path().join("budget.json"), b"{not-json").unwrap();
        let h = hook(4096).with_scheduler(Some(scheduler_with(Some(shared), Some(20))));
        let note = note_text(&h);
        assert!(note.contains("local 20"), "{note}");
        assert!(note.contains("shared unknown"), "{note}");
        assert!(
            !note.contains("known min 20") && !note.contains("(min 20)"),
            "a known local remainder must not become a numeric effective minimum: {note}"
        );
        assert!(
            note.contains("min unknown"),
            "the effective minimum must remain unknown when a shared constraint is unreadable: {note}"
        );
        assert!(
            !note.contains("shared 0"),
            "unknown must not be reported as zero: {note}"
        );
        assert!(
            !note.contains("shared 5"),
            "unknown must not be reported as a stale positive: {note}"
        );
        let root_text = root.path().to_string_lossy().into_owned();
        assert!(!note.contains(&root_text), "the note leaked a path: {note}");
    }

    /// The guidance is phase-aware on a real known minimum: 5 and 3 keep the
    /// mid remainder guidance, 2 asks for verification only, and 1 states this
    /// snapshot's last call and forbids claiming an unverified success. Every
    /// step still stays inside the byte bound.
    #[tokio::test]
    async fn runtime_note_switches_phase_guidance_at_known_minimum_steps() {
        let scheduler = Arc::new(scheduling::Scheduler::from_parts(
            coordinator::Coordinator::new(coordinator::Limits {
                max_model_calls: Some(5),
                model: 8,
                provider_model: 8,
                ..coordinator::Limits::default()
            })
            .unwrap(),
            None,
        ));
        let h = hook(4096).with_scheduler(Some(scheduler.clone()));
        let assert_phase = |expected_min: u64, expected_phase: &str| {
            let note = note_text(&h);
            assert!(note.contains(&format!("min {expected_min}")), "{note}");
            assert!(note.contains(expected_phase), "min {expected_min}: {note}");
            assert!(
                note.contains(
                    "pre-admission snapshot incl. this call, not a guarantee or authorization"
                ),
                "min {expected_min}: {note}"
            );
            assert!(
                note.contains("concurrency can lower it."),
                "min {expected_min}: {note}"
            );
            assert!(
                note.len() <= RUNTIME_NOTE_MAX_BYTES,
                "min {expected_min}: {note}"
            );
        };
        assert_phase(5, RUNTIME_NOTE_PHASE_MID);
        let mut permits = Vec::new();
        for _ in 0..2 {
            permits.push(
                scheduler
                    .coordinator
                    .acquire_model("phase-note-provider", 64)
                    .await
                    .unwrap(),
            );
        }
        assert_phase(3, RUNTIME_NOTE_PHASE_MID);
        permits.push(
            scheduler
                .coordinator
                .acquire_model("phase-note-provider", 64)
                .await
                .unwrap(),
        );
        assert_phase(2, RUNTIME_NOTE_PHASE_TWO);
        permits.push(
            scheduler
                .coordinator
                .acquire_model("phase-note-provider", 64)
                .await
                .unwrap(),
        );
        let final_note = note_text(&h);
        assert!(final_note.contains("min 1"), "{final_note}");
        assert!(final_note.contains(RUNTIME_NOTE_PHASE_ONE), "{final_note}");
        assert!(
            final_note.contains("last call in this snapshot"),
            "{final_note}"
        );
        assert!(
            final_note.contains("do not claim unverified success"),
            "{final_note}"
        );
        assert!(
            !final_note.contains(RUNTIME_NOTE_PHASE_GENERIC),
            "{final_note}"
        );
        assert!(final_note.len() <= RUNTIME_NOTE_MAX_BYTES, "{final_note}");
        drop(permits);
    }

    /// A shared cap that is stricter than the local one drives the phase, not
    /// the local remainder: the note takes the shared minimum and switches to
    /// the final-call guidance when that shared minimum reaches one.
    #[tokio::test]
    async fn runtime_note_phase_follows_the_stricter_shared_minimum() {
        let root = tempfile::tempdir().unwrap();
        let shared = finite_shared(root.path(), 2);
        let h = hook(4096).with_scheduler(Some(scheduler_with(Some(shared.clone()), Some(20))));
        let note = note_text(&h);
        assert!(note.contains("local 20"), "{note}");
        assert!(note.contains("shared 2"), "{note}");
        assert!(note.contains("min 2"), "{note}");
        assert!(note.contains(RUNTIME_NOTE_PHASE_TWO), "{note}");
        assert!(!note.contains("last call in this snapshot"), "{note}");
        let permit = shared.reserve_budget(64).await.unwrap().unwrap();
        let note = note_text(&h);
        assert!(note.contains("min 1"), "{note}");
        assert!(note.contains(RUNTIME_NOTE_PHASE_ONE), "{note}");
        assert!(!note.contains(RUNTIME_NOTE_PHASE_GENERIC), "{note}");
        assert!(note.len() <= RUNTIME_NOTE_MAX_BYTES, "{note}");
        drop(permit);
    }

    /// An unreadable shared ledger keeps the aggregate minimum unknown: the
    /// note must not fold in the known local remainder, must not fabricate a
    /// numeric minimum, and must never promise a final call.
    #[test]
    fn runtime_note_unknown_never_promises_a_final_call() {
        let root = tempfile::tempdir().unwrap();
        let shared = finite_shared(root.path(), 5);
        std::fs::write(root.path().join("budget.json"), b"{not-json").unwrap();
        let h = hook(4096).with_scheduler(Some(scheduler_with(Some(shared), Some(u64::MAX))));
        let note = note_text(&h);
        assert!(note.contains("min unknown"), "{note}");
        assert!(note.contains(RUNTIME_NOTE_PHASE_GENERIC), "{note}");
        assert!(!note.contains("last call in this snapshot"), "{note}");
        assert!(
            !note.contains("min 18446744073709551615"),
            "unknown must not be folded into the known local remainder: {note}"
        );
        assert!(
            note.contains(
                "pre-admission snapshot incl. this call, not a guarantee or authorization"
            ),
            "{note}"
        );
        assert!(note.contains("concurrency can lower it."), "{note}");
        assert!(note.len() <= RUNTIME_NOTE_MAX_BYTES, "{note}");
    }

    /// Long scope numbers make the per-source form too long, so the fallback
    /// runs. It must stay inside the byte bound while keeping the real minimum,
    /// every caveat and the phase sentence.
    #[test]
    fn runtime_note_bounded_fallback_keeps_phase_and_caveats() {
        let root = tempfile::tempdir().unwrap();
        let shared = finite_shared(root.path(), u64::MAX);
        let h = hook(4096).with_scheduler(Some(scheduler_with(Some(shared), Some(u64::MAX))));
        let note = note_text(&h);
        assert!(note.len() <= RUNTIME_NOTE_MAX_BYTES, "{note}");
        assert!(note.contains("min 18446744073709551615"), "{note}");
        assert!(
            note.contains(
                "pre-admission snapshot incl. this call, not a guarantee or authorization"
            ),
            "{note}"
        );
        assert!(note.contains("concurrency can lower it."), "{note}");
        assert!(note.contains(RUNTIME_NOTE_PHASE_GENERIC), "{note}");
        assert!(
            !note.contains("local 18446744073709551615"),
            "the fallback drops the per-source breakdown under the hard bound: {note}"
        );

        // The same fallback carries the final-call phase when a real shared
        // minimum of one is stricter than a very wide local cap: the sources
        // are long enough to overflow, so the compact form must keep the real
        // minimum, every caveat and the phase sentence.
        let root = tempfile::tempdir().unwrap();
        let shared = finite_shared(root.path(), 1);
        let h = hook(4096).with_scheduler(Some(scheduler_with(Some(shared), Some(u64::MAX))));
        let note = note_text(&h);
        assert!(note.len() <= RUNTIME_NOTE_MAX_BYTES, "{note}");
        assert!(note.contains("min 1"), "{note}");
        assert!(note.contains(RUNTIME_NOTE_PHASE_ONE), "{note}");
        assert!(note.contains("concurrency can lower it."), "{note}");
        assert!(!note.contains(RUNTIME_NOTE_PHASE_GENERIC), "{note}");
        assert!(
            !note.contains("local 18446744073709551615"),
            "the fallback drops the per-source breakdown under the hard bound: {note}"
        );
    }

    /// No finite quota and no unreadable shared state means there is nothing to
    /// report, so no note is injected on the common unbounded path.
    #[test]
    fn runtime_note_is_absent_when_no_finite_quota_is_known() {
        assert!(
            hook(4096).runtime_note().is_none(),
            "no scheduler has nothing to report"
        );
        assert!(
            hook(4096)
                .with_scheduler(Some(scheduler_with(None, None)))
                .runtime_note()
                .is_none(),
            "an unbounded local quota with no shared limits adds no note"
        );
        let root = tempfile::tempdir().unwrap();
        let shared = shared_limits::SharedLimits::new(
            root.path(),
            shared_limits::SharedLimitsConfig::default(),
        )
        .unwrap();
        assert!(
            hook(4096)
                .with_scheduler(Some(scheduler_with(Some(shared), None)))
                .runtime_note()
                .is_none(),
            "a readable shared ledger with no call cap is unbounded, not unknown"
        );
    }

    /// Each completion gets one fresh note from the unchanged canonical history;
    /// the note is never appended to the persistent history or accumulated.
    #[test]
    fn repeated_completions_carry_one_note_and_never_accumulate_it() {
        let h = hook(4096).with_scheduler(Some(scheduler_with(None, Some(5))));
        let history = vec![
            Message::system("rules"),
            Message::user("goal"),
            Message::assistant("finding"),
        ];
        let original = history.clone();
        let prompt = Message::user("continue");
        let first = patch_history(&h.completion_call_decision(&history, &prompt, 1));
        let second = patch_history(&h.completion_call_decision(&history, &prompt, 2));
        assert_eq!(
            history, original,
            "the decision must not mutate the caller's history"
        );
        for patched in [&first, &second] {
            let notes = patched
                .iter()
                .filter(|message| {
                    matches!(message, Message::System { content } if content.starts_with(RUNTIME_NOTE_MARKER))
                })
                .count();
            assert_eq!(notes, 1, "exactly one fresh note per request: {patched:?}");
            assert_eq!(
                &patched[1..],
                original.as_slice(),
                "the source history is preserved verbatim"
            );
        }
        assert_eq!(
            first.len(),
            second.len(),
            "the note is not accumulated across calls"
        );
    }

    /// A fitted replacement pays for the note up front and reports the real
    /// patched history, while the newest user request survives verbatim.
    #[test]
    fn fitted_replacement_pays_for_the_note_and_reports_the_real_patched_history() {
        let h = hook(4096).with_scheduler(Some(scheduler_with(None, Some(40))));
        let note = h.runtime_note().expect("finite quota note");
        let note_cost = message_cost(&note).unwrap();
        let mut history = vec![Message::system("rules")];
        for index in 0..30 {
            history.push(Message::user(format!("turn {index} {}", "u".repeat(300))));
            history.push(Message::assistant(format!(
                "answer {index} {}",
                "a".repeat(300)
            )));
        }
        history.push(Message::user("LATEST-GOAL-7 must survive"));
        let prompt = Message::user("continue");
        let limit = h.budget.input_limit();
        assert!(
            request_cost(&h, &history, &prompt) > limit,
            "the fixture must overflow the input estimate"
        );
        let (fitted, report) = h
            .fit_with_report(&history, &prompt, note_cost)
            .unwrap()
            .unwrap();
        assert_eq!(report.before_messages, Some(history.len()));
        assert_eq!(report.before_bytes, Some(compact::history_bytes(&history)));
        let (with_note, report) = BudgetHook::with_note_in_report(fitted, report, note);
        assert!(matches!(
            with_note.first(),
            Some(Message::System { content }) if content.starts_with(RUNTIME_NOTE_MARKER)
        ));
        assert_eq!(report.after_messages, Some(with_note.len()));
        assert_eq!(report.after_bytes, Some(compact::history_bytes(&with_note)));
        assert!(
            request_cost(&h, &with_note, &prompt) <= limit,
            "the reserved note cost must keep the final request inside the estimate"
        );
        assert!(
            serde_json::to_string(&with_note)
                .unwrap()
                .contains("LATEST-GOAL-7 must survive"),
            "the newest user request must survive verbatim"
        );
    }

    struct NoteTestHost;

    impl crate::Host for NoteTestHost {
        fn shell(
            &self,
            _command: &str,
            _cwd: &std::path::Path,
        ) -> crate::HostFuture<Result<String, String>> {
            Box::pin(async { Ok(String::new()) })
        }
        fn on_event(&self, _event: crate::AgentEvent) {}
    }

    /// When the request-level fit defers a huge newest tool result to the batch
    /// output budget, the dispatch fallback still carries the note in the real
    /// prepared request so the same budget charges its bytes.
    #[test]
    fn dispatch_fallback_carries_the_runtime_note_into_the_real_request() {
        use crate::tool_lifecycle::{ToolCallPhase, ToolCallTracker};

        let dir = tempfile::tempdir().unwrap();
        let run_id = uuid::Uuid::new_v4();
        let tracker = ToolCallTracker::new(run_id.to_string(), Arc::new(NoteTestHost));
        let index = tracker
            .begin("read", "read", "file", Some("call-note"), Some(1), Some(1))
            .unwrap();
        tracker.admit(index, Some(1), Some(1)).unwrap();
        tracker.mark_dispatched(1, Some("call-note")).unwrap();
        tracker
            .observe_outcome(1, Some("call-note"), ToolCallPhase::Executed, Some("ok"))
            .unwrap();
        let call = ToolCall::from_wire(
            "call-note",
            ToolFunction::new(
                ToolName::new("read").unwrap(),
                serde_json::json!({"path": "file"}),
            ),
        );
        let history = vec![Message::Assistant(AssistantMessage::new(vec![
            AssistantContent::ToolCall(call.clone()),
        ]))];
        let prompt = Message::User {
            content: vec![UserContent::ToolResult(
                call.result(vec![ToolResultContent::text("big ".repeat(4000))]),
            )],
        };
        let mut request = CompletionRequest::new(prompt.clone());
        request.chat_history = std::iter::once(Message::system("preamble"))
            .chain(history.iter().cloned())
            .chain(std::iter::once(prompt.clone()))
            .collect();
        let h = hook(4096)
            .with_scheduler(Some(scheduler_with(None, Some(40))))
            .with_output_budget(dir.path().to_path_buf(), run_id, tracker);
        assert!(
            matches!(
                h.completion_call_decision(&history, &prompt, 1),
                CompletionCallAction::Continue
            ),
            "a huge newest tool result defers to the batch output budget"
        );
        let DispatchAction::Patch(EffectKind::Completion { request, stream }) =
            h.dispatch_decision(&request, true)
        else {
            panic!("the batch budget must patch the prepared request")
        };
        assert!(stream);
        assert!(request_has_runtime_note(&request));
        // R13 replaced the old `vec![1]` assertion (one note right after the
        // preamble). The stronger invariant is that the note is the very last
        // message, after the current tool result, so nothing separates the
        // active call from its result and the whole prior history is a stable
        // prefix. This assertion fails on the pre-R13 placement.
        let note_positions = request
            .chat_history
            .iter()
            .enumerate()
            .filter(|(_, message)| {
                matches!(message, Message::System { content } if content.starts_with(RUNTIME_NOTE_MARKER))
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        assert_eq!(
            note_positions,
            vec![request.chat_history.len() - 1],
            "the runtime note must be the last message, after the current tool result"
        );
        assert!(
            matches!(
                request.chat_history.last(),
                Some(Message::System { content }) if content.starts_with(RUNTIME_NOTE_MARKER)
            ),
            "the note must be the request's last message"
        );
        // The current tool result is immediately before the note, the model's
        // tool call is immediately before its result, and the preamble stays
        // first: no message was inserted between a call and its result.
        assert!(
            matches!(request.chat_history.first(), Some(Message::System { content }) if content == "preamble"),
            "the leading preamble must stay in its original position"
        );
        assert!(
            matches!(
                &request.chat_history[request.chat_history.len() - 2],
                Message::User { content } if content.iter().any(|item| matches!(item, UserContent::ToolResult(result) if result.call == CallId::from_wire("call-note")))
            ),
            "the note must sit after the current tool result, not between the call and its result"
        );
        assert_tool_pairs_intact(&request.chat_history);
        assert!(has_tool_call(&request.chat_history, "call-note"));
        assert!(
            request
                .chat_history
                .iter()
                .filter(|message| matches!(message, Message::Assistant(_)))
                .count()
                == 1
                && request
                    .chat_history
                    .iter()
                    .filter(|message| matches!(message, Message::User { .. }))
                    .count()
                    == 1,
            "the real call/result pair is preserved exactly once"
        );
    }

    /// The dispatch boundary moves an existing reserved note to the end of the
    /// sent history verbatim. It never re-reads the quota, so the note text
    /// (including `local N`) is exactly the one the request-level fit paid for
    /// even after the quota changed. The move changes neither the message count
    /// nor the serialized bytes, and every other message keeps its order.
    #[tokio::test]
    async fn dispatch_relocation_moves_the_note_verbatim_without_rereading_the_quota() {
        use crate::tool_lifecycle::ToolCallTracker;
        let dir = tempfile::tempdir().unwrap();
        let run_id = uuid::Uuid::new_v4();
        let tracker = ToolCallTracker::new(run_id.to_string(), Arc::new(NoteTestHost));
        let scheduler = scheduler_with(None, Some(40));
        let h = hook(4096)
            .with_scheduler(Some(scheduler.clone()))
            .with_output_budget(dir.path().to_path_buf(), run_id, tracker);
        let note = h.runtime_note().expect("finite quota note");
        let note_text = match &note {
            Message::System { content } => content.clone(),
            other => panic!("the note must be a system message: {other:?}"),
        };
        assert!(note_text.contains("local 40"), "{note_text}");
        // The real prepared request shape before the dispatch hook: preamble,
        // the request-level note, then the (here tool-result) current prompt.
        let call = ToolCall::from_wire(
            "call-move",
            ToolFunction::new(
                ToolName::new("read").unwrap(),
                serde_json::json!({"path": "small.txt"}),
            ),
        );
        let result = Message::User {
            content: vec![UserContent::ToolResult(
                call.result(vec![ToolResultContent::text("small result")]),
            )],
        };
        let mut request = CompletionRequest::new(result.clone());
        request.chat_history = vec![
            Message::system("preamble"),
            note.clone(),
            Message::Assistant(AssistantMessage::new(vec![AssistantContent::ToolCall(
                call.clone(),
            )])),
            result.clone(),
        ];
        let before = request.chat_history.clone();
        // Advance the real quota: a re-read would render `local 39`, not 40.
        let permit = scheduler
            .coordinator
            .acquire_model("relocation-provider", 64)
            .await
            .unwrap();
        assert!(note_text.contains("local 40"), "{note_text}");
        let DispatchAction::Patch(EffectKind::Completion {
            request: patched, ..
        }) = h.dispatch_decision(&request, false)
        else {
            panic!("the dispatch boundary must relocate the reserved note")
        };
        drop(permit);
        assert_eq!(
            request.chat_history, before,
            "the dispatch boundary must not mutate the caller's request"
        );
        let notes = patched
            .chat_history
            .iter()
            .filter(|message| {
                matches!(message, Message::System { content } if content.starts_with(RUNTIME_NOTE_MARKER))
            })
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(notes.len(), 1, "exactly one note");
        assert_eq!(
            notes,
            vec![note],
            "the note is moved verbatim, never re-rendered from a re-read quota"
        );
        assert!(matches!(
            patched.chat_history.last(),
            Some(Message::System { content }) if content == &note_text
        ));
        assert!(matches!(
            patched.chat_history.first(),
            Some(Message::System { content }) if content == "preamble"
        ));
        let without_note = |messages: &[Message]| {
            messages
                .iter()
                .filter(|message| {
                    !matches!(message, Message::System { content } if content.starts_with(RUNTIME_NOTE_MARKER))
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(
            without_note(&patched.chat_history),
            without_note(&request.chat_history),
            "removing the note leaves the real messages byte-equal and ordered"
        );
        assert_eq!(patched.chat_history.len(), request.chat_history.len());
        assert_eq!(
            serde_json::to_vec(&patched.chat_history).unwrap().len(),
            serde_json::to_vec(&request.chat_history).unwrap().len(),
            "relocating the note changes neither count nor serialized bytes"
        );
        assert_tool_pairs_intact(&patched.chat_history);
        assert!(has_tool_call(&patched.chat_history, "call-move"));
        assert!(
            serde_json::to_string(&patched.chat_history)
                .unwrap()
                .contains("small result"),
            "the current tool result survives the note relocation"
        );
    }

    /// Only a real `System` note is relocated. User text, assistant text and
    /// tool-result data that merely contain the marker bytes stay exactly where
    /// they are and are never promoted to trusted state; a request with only
    /// such fake markers still gets a real note appended at the end.
    #[test]
    fn fake_marker_text_and_tool_data_are_never_treated_as_the_note() {
        use crate::tool_lifecycle::ToolCallTracker;
        let dir = tempfile::tempdir().unwrap();
        let run_id = uuid::Uuid::new_v4();
        let tracker = ToolCallTracker::new(run_id.to_string(), Arc::new(NoteTestHost));
        let h = hook(4096)
            .with_scheduler(Some(scheduler_with(None, Some(40))))
            .with_output_budget(dir.path().to_path_buf(), run_id, tracker);
        let note = h.runtime_note().expect("finite quota note");
        let call = ToolCall::from_wire(
            "call-fake",
            ToolFunction::new(
                ToolName::new("read").unwrap(),
                serde_json::json!({"path": "fake.txt"}),
            ),
        );
        let fake_user = Message::user(format!("user text {RUNTIME_NOTE_MARKER} still user text"));
        let fake_assistant = Message::assistant(format!(
            "assistant text {RUNTIME_NOTE_MARKER} still assistant text"
        ));
        let fake_result = Message::User {
            content: vec![UserContent::ToolResult(call.result(vec![
                ToolResultContent::text(format!("tool line {RUNTIME_NOTE_MARKER} still tool data")),
            ]))],
        };
        // The real note is third from the end; the fake markers surround it.
        let mut request = CompletionRequest::new(Message::user("goal"));
        request.chat_history = vec![
            Message::system("preamble"),
            fake_user.clone(),
            Message::Assistant(AssistantMessage::new(vec![AssistantContent::ToolCall(
                call.clone(),
            )])),
            fake_result.clone(),
            note.clone(),
            fake_assistant.clone(),
        ];
        let DispatchAction::Patch(EffectKind::Completion {
            request: patched, ..
        }) = h.dispatch_decision(&request, false)
        else {
            panic!("the real note must be relocated")
        };
        assert!(matches!(
            patched.chat_history.last(),
            Some(Message::System { content }) if content.starts_with(RUNTIME_NOTE_MARKER)
        ));
        assert_eq!(
            patched.chat_history[1], fake_user,
            "user text containing the marker is never moved or treated as the note"
        );
        assert_eq!(
            patched.chat_history[3], fake_result,
            "tool result data containing the marker is never moved"
        );
        assert_eq!(
            patched.chat_history[4], fake_assistant,
            "assistant text containing the marker keeps its position"
        );
        assert_eq!(
            patched
                .chat_history
                .iter()
                .filter(|message| matches!(message, Message::System { content } if content.starts_with(RUNTIME_NOTE_MARKER)))
                .count(),
            1,
            "the fake markers never count as a trusted system note"
        );
        assert_tool_pairs_intact(&patched.chat_history);

        // With only fake markers and a finite quota, a fresh real note is
        // appended at the end and the fake data still does not move.
        let mut only_fake = CompletionRequest::new(Message::user("goal"));
        only_fake.chat_history = vec![
            Message::system("preamble"),
            fake_user.clone(),
            Message::Assistant(AssistantMessage::new(vec![AssistantContent::ToolCall(
                call.clone(),
            )])),
            fake_result.clone(),
        ];
        let DispatchAction::Patch(EffectKind::Completion {
            request: appended, ..
        }) = h.dispatch_decision(&only_fake, false)
        else {
            panic!("a finite quota must append a real note on the fallback path")
        };
        assert!(matches!(
            appended.chat_history.last(),
            Some(Message::System { content }) if content.starts_with(RUNTIME_NOTE_MARKER)
        ));
        assert_eq!(appended.chat_history[1], fake_user);
        assert_eq!(appended.chat_history[3], fake_result);
        assert_tool_pairs_intact(&appended.chat_history);
    }

    /// A request whose single note already is the last message is left exactly
    /// as it is: no clone, no rewrite and no quota re-read even after the shared
    /// ledger became unreadable. A re-read would have rendered `shared unknown`
    /// and returned a rewritten request.
    #[test]
    fn already_trailing_note_is_a_no_op_and_never_rereads_the_quota() {
        let root = tempfile::tempdir().unwrap();
        let shared = finite_shared(root.path(), 5);
        let h = hook(4096).with_scheduler(Some(scheduler_with(Some(shared), Some(20))));
        let note = h.runtime_note().expect("finite quota note");
        assert!(
            matches!(&note, Message::System { content } if content.contains("shared 5")),
            "the fixture note must come from the readable ledger"
        );
        let mut request = CompletionRequest::new(Message::user("goal"));
        request.chat_history = vec![
            Message::system("preamble"),
            Message::user("goal"),
            note.clone(),
        ];
        std::fs::write(root.path().join("budget.json"), b"{not-json").unwrap();
        assert!(
            h.note_positioned_request(&request).is_none(),
            "a note already at the end must be a no-op, never a re-read"
        );
        assert!(matches!(
            h.dispatch_decision(&request, false),
            DispatchAction::Proceed
        ));
    }

    // -----------------------------------------------------------------------
    // R8: composed summary + externalization planning.
    // -----------------------------------------------------------------------

    /// The real prepared request shape: the leading preamble system message,
    /// the (possibly patched) history, then the current prompt last.
    fn composed_request(history: &[Message], prompt: &Message) -> CompletionRequest {
        let mut request = CompletionRequest::new(prompt.clone());
        request.chat_history = std::iter::once(Message::system("preamble"))
            .chain(history.iter().cloned())
            .chain(std::iter::once(prompt.clone()))
            .collect();
        request
    }

    /// A real canonical tool call in `run_id`'s tracker, so externalization
    /// planning sees a genuine provider id instead of a guessed one.
    fn tracked_eval_call(
        run_id: uuid::Uuid,
        call_id: &str,
    ) -> (Arc<crate::tool_lifecycle::ToolCallTracker>, ToolCall) {
        use crate::tool_lifecycle::{ToolCallPhase, ToolCallTracker};
        let tracker = ToolCallTracker::new(run_id.to_string(), Arc::new(NoteTestHost));
        tracker
            .begin("bash", "bash", "cmd", Some(call_id), Some(1), Some(1))
            .unwrap();
        tracker.admit(1, Some(1), Some(1)).unwrap();
        tracker.mark_dispatched(1, Some(call_id)).unwrap();
        tracker
            .observe_outcome(1, Some(call_id), ToolCallPhase::Executed, Some("ok"))
            .unwrap();
        let call = ToolCall::from_wire(
            call_id,
            ToolFunction::new(
                ToolName::new("bash").unwrap(),
                serde_json::json!({"command": "emit"}),
            ),
        );
        (tracker, call)
    }

    /// The R8 composition: an old, non-externalizable prefix that only a
    /// summary can shrink, plus a newest tool result so large that it alone
    /// leaves nothing for the summary. Neither mechanism serves the request by
    /// itself; together they must, and the real dispatch transaction must
    /// still publish and verify what the plan assumed.
    fn composed_scenario() -> (
        tempfile::TempDir,
        uuid::Uuid,
        Arc<crate::tool_lifecycle::ToolCallTracker>,
        Vec<Message>,
        Message,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let run_id = uuid::Uuid::new_v4();
        let (tracker, call) = tracked_eval_call(run_id, "call-big-result");
        let history = vec![
            Message::system("rules"),
            Message::user("GOAL-P0"),
            Message::assistant("A".repeat(8_000)),
            Message::user("ack"),
            assistant(std::slice::from_ref(&call)),
        ];
        let prompt = Message::User {
            content: vec![UserContent::ToolResult(
                call.result(vec![ToolResultContent::text("P".repeat(9_000))]),
            )],
        };
        (dir, run_id, tracker, history, prompt)
    }

    #[test]
    fn composed_summary_and_externalization_fit_what_neither_serves_alone() {
        use crate::output_budget::FitError;
        use sha2::Digest;
        let (dir, run_id, tracker, history, prompt) = composed_scenario();
        let h = hook(8192).with_output_budget(dir.path().to_path_buf(), run_id, tracker.clone());
        let limit = h.budget.input_limit();
        let original = history.clone();

        // 1. The summary alone cannot serve it: the newest result is larger than
        //    the whole input estimate, so every cut leaves `fixed` over budget.
        assert!(
            h.fit_with_report(&history, &prompt, 0).is_err(),
            "the raw request-level fit must fail before this regression means anything"
        );

        // 2. Externalization alone cannot serve it either: the retained
        //    non-externalizable history still overflows once the newest result
        //    is replaced by minimal reference metadata. Zero I/O so far.
        let budget = h.output_budget.as_ref().expect("a wired output budget");
        assert!(
            matches!(
                budget.fit_request(&composed_request(&history, &prompt)),
                Err(FitError::Irreducible { .. })
            ),
            "externalization alone must not serve the composed request"
        );
        assert!(
            !dir.path().join(".mio").exists(),
            "an irreducible plan must not create the output store"
        );
        // On the uncompressed history the dispatch boundary is the end of the
        // line: `Continue` leaves nothing that could summarize the old prefix.
        assert!(
            matches!(
                h.dispatch_decision(&composed_request(&history, &prompt), true),
                DispatchAction::Deny(report)
                    if report.message.starts_with("Context budget:")
            ),
            "the uncompressed composed request is needlessly irreducible"
        );
        assert!(
            !dir.path().join(".mio").exists(),
            "a dispatch refusal must stay at zero I/O"
        );

        // 3. The completion decision composes summary + externalization.
        let patched = patch_history(&h.completion_call_decision(&history, &prompt, 1));
        assert_ne!(patched, history, "the composed plan must summarize");
        assert!(
            serde_json::to_string(&patched).unwrap().contains("GOAL-P0"),
            "the user goal must survive summarization"
        );
        assert_eq!(
            history, original,
            "composing a plan must not mutate the source history"
        );

        // 4. The real dispatch boundary still publishes and verifies.
        let dispatched = match h.dispatch_decision(&composed_request(&patched, &prompt), true) {
            DispatchAction::Patch(EffectKind::Completion { request, stream }) => {
                assert!(stream);
                request
            }
            other => panic!("the dispatch boundary must bound the composed request: {other:?}"),
        };
        assert!(
            serde_json::to_vec(&dispatched.chat_history).unwrap().len() < limit,
            "the dispatched request must fit the {limit}-byte estimate"
        );
        assert!(
            !patched.iter().any(|message| serde_json::to_string(message)
                .unwrap()
                .contains("output_references")),
            "no provisional reference may enter the patched history"
        );

        // 5. The reference the plan assumed really exists, carries the exact
        //    body, and is associated with the canonical tool call.
        let records = tracker.calls();
        let record = records
            .iter()
            .find(|record| record.tool_call_id.as_deref() == Some("call-big-result"))
            .expect("the canonical record for the externalized call");
        assert_eq!(record.output_references.len(), 1, "one real reference");
        let reference = &record.output_references[0];
        let name = reference
            .path
            .strip_prefix(".mio/outputs/")
            .expect("a store-relative output path");
        let bytes = std::fs::read(dir.path().join(".mio/outputs").join(name)).unwrap();
        assert_eq!(
            bytes,
            "P".repeat(9_000).into_bytes(),
            "the externalized file holds the exact body"
        );
        let mut hasher = sha2::Sha256::new();
        hasher.update(&bytes);
        assert_eq!(reference.sha256, format!("{:x}", hasher.finalize()));
        assert_eq!(reference.total_bytes, bytes.len() as u64);
    }

    /// The trusted runtime note is still paid exactly once when the composed
    /// path patches: it is reserved inside the fit, carried in the fitted
    /// history, and the dispatch boundary neither drops nor duplicates it.
    #[test]
    fn composed_plan_pays_for_the_runtime_note_exactly_once() {
        let (dir, run_id, tracker, history, prompt) = composed_scenario();
        let h = hook(8192)
            .with_scheduler(Some(scheduler_with(None, Some(40))))
            .with_output_budget(dir.path().to_path_buf(), run_id, tracker);
        let limit = h.budget.input_limit();
        let patched = patch_history(&h.completion_call_decision(&history, &prompt, 1));
        let notes = |messages: &[Message]| {
            messages
                .iter()
                .filter(|message| {
                    matches!(message, Message::System { content }
                        if content.starts_with(RUNTIME_NOTE_MARKER))
                })
                .count()
        };
        assert_eq!(notes(&patched), 1, "one fresh note in the fitted history");
        let dispatched = match h.dispatch_decision(&composed_request(&patched, &prompt), true) {
            DispatchAction::Patch(EffectKind::Completion { request, .. }) => request,
            other => panic!("the dispatch boundary must bound the composed request: {other:?}"),
        };
        assert_eq!(
            notes(&dispatched.chat_history),
            1,
            "the dispatch boundary must not duplicate the reserved note"
        );
        assert!(
            serde_json::to_vec(&dispatched.chat_history).unwrap().len() < limit,
            "the note must be charged inside the same estimate"
        );
    }

    /// A host whose compaction audit fails must stop the run before any real
    /// output publication: an unaudited composition is never sent onward.
    struct FailingAuditHost;

    impl crate::Host for FailingAuditHost {
        fn shell(
            &self,
            _command: &str,
            _cwd: &std::path::Path,
        ) -> crate::HostFuture<Result<String, String>> {
            Box::pin(async { Ok(String::new()) })
        }
        fn on_event(&self, _event: crate::AgentEvent) {}
        fn on_context_compaction(
            &self,
            _observation: &compact::ContextCompactionObservation,
        ) -> Result<(), String> {
            Err("audit store unavailable".into())
        }
    }

    #[test]
    fn a_failed_compaction_audit_stops_before_any_output_publication() {
        let (dir, run_id, tracker, history, prompt) = composed_scenario();
        let h = hook(8192)
            .with_observer(Arc::new(FailingAuditHost), run_id)
            .with_output_budget(dir.path().to_path_buf(), run_id, tracker);
        match h.completion_call_decision(&history, &prompt, 1) {
            CompletionCallAction::Stop(reason) => assert!(
                reason.starts_with("Context compaction audit:"),
                "the audit failure keeps its diagnostic: {reason}"
            ),
            other => panic!("an unaudited composition must not proceed: {other:?}"),
        }
        assert!(
            !dir.path().join(".mio").exists(),
            "a failed audit must publish nothing and write no output store"
        );
    }

    /// Identity that is not in the canonical tracker is never externalized, and
    /// a request that has nothing externalizable keeps the old fail-closed
    /// deferral: no summary is fabricated and no output store is created.
    #[test]
    fn composed_plan_never_fabricates_references_for_unknown_or_irreducible_input() {
        // (a) a huge newest tool result whose call id the tracker never saw.
        let dir = tempfile::tempdir().unwrap();
        let run_id = uuid::Uuid::new_v4();
        let (tracker, _) = tracked_eval_call(run_id, "call-known");
        let unknown = ToolCall::from_wire(
            "call-unknown",
            ToolFunction::new(
                ToolName::new("bash").unwrap(),
                serde_json::json!({"command": "emit"}),
            ),
        );
        let history = vec![
            Message::system("rules"),
            Message::user("GOAL-P0"),
            Message::assistant("A".repeat(8_000)),
            Message::user("ack"),
            assistant(std::slice::from_ref(&unknown)),
        ];
        let prompt = Message::User {
            content: vec![UserContent::ToolResult(
                unknown.result(vec![ToolResultContent::text("P".repeat(9_000))]),
            )],
        };
        let h = hook(8192).with_output_budget(dir.path().to_path_buf(), run_id, tracker.clone());
        assert!(
            matches!(
                h.completion_call_decision(&history, &prompt, 1),
                CompletionCallAction::Continue
            ),
            "an unknown identity keeps the existing deferral, never a fabricated patch"
        );
        assert!(
            matches!(
                h.dispatch_decision(&composed_request(&history, &prompt), true),
                DispatchAction::Deny(report) if report.message.starts_with("Context budget:")
            ),
            "an unknown identity is never externalized"
        );
        // (b) a huge plain user prompt has nothing externalizable at all.
        let plain = Message::user("X".repeat(9_000));
        assert!(
            matches!(
                h.completion_call_decision(&history, &plain, 1),
                CompletionCallAction::Continue
            ),
            "nothing externalizable must not fabricate a summary path"
        );
        assert!(
            matches!(
                h.dispatch_decision(&composed_request(&history, &plain), true),
                DispatchAction::Deny(report) if report.message.starts_with("Context budget:")
            ),
            "an irreducible plain prompt is still refused"
        );
        assert!(
            !dir.path().join(".mio").exists(),
            "no candidate must never create the output store"
        );
        assert!(
            tracker
                .calls()
                .iter()
                .all(|record| record.output_references.is_empty()),
            "no reference may be invented for an unknown call"
        );
    }

    /// R8 residual edge from the root review: `planned_costs` plans every
    /// message in isolation, while the dispatch transaction plans at most
    /// `MAX_BATCH_CANDIDATES` bodies over the whole request. With one body more
    /// than that bound the per-message sum can already fit, so the early
    /// "no summary needed" exit would defer to a dispatch that must refuse.
    /// Only the exact whole-request plan may take that exit; when it does not
    /// fit, the normal safe-boundary cut search still has to run and produce a
    /// request the real dispatch plan can bound.
    #[test]
    fn planned_costs_do_not_trust_per_message_costs_past_the_batch_candidate_bound() {
        use crate::output_budget::MAX_BATCH_CANDIDATES;
        use crate::tool_lifecycle::{ToolCallPhase, ToolCallTracker};

        // One body past the whole-request candidate bound, so the exact batch
        // plan must leave bodies inline that each single-message plan
        // externalizes on its own.
        const BODY_BYTES: usize = 2_000;
        // The newest prompt is a real tool result larger than the whole input
        // estimate. The raw summary-only fit can never serve it, while the
        // batch planner externalizes it, which is exactly the composed case
        // this edge lives in.
        const PROMPT_BYTES: usize = 250_000;
        // A large window keeps the many minimal references inside the estimate
        // so the remaining slack can be tuned with one non-externalizable note.
        const WINDOW: u32 = 262_144;

        let dir = tempfile::tempdir().unwrap();
        let run_id = uuid::Uuid::new_v4();
        let tracker = ToolCallTracker::new(run_id.to_string(), Arc::new(NoteTestHost));
        let body = |index: usize| {
            let mut text = format!("BODY-{index:04}-");
            text.push_str(&"b".repeat(BODY_BYTES));
            text.truncate(BODY_BYTES);
            text
        };
        let tracked = |call_id: &str, effect: u64| {
            tracker
                .begin("bash", "bash", "cmd", Some(call_id), Some(1), Some(effect))
                .unwrap();
            tracker.admit(effect, Some(1), Some(effect)).unwrap();
            tracker.mark_dispatched(effect, Some(call_id)).unwrap();
            tracker
                .observe_outcome(effect, Some(call_id), ToolCallPhase::Executed, Some("ok"))
                .unwrap();
            ToolCall::from_wire(
                call_id,
                ToolFunction::new(
                    ToolName::new("bash").unwrap(),
                    serde_json::json!({"command": "emit"}),
                ),
            )
        };
        let mut history = vec![Message::system("rules"), Message::user("GOAL-P0")];
        for index in 0..MAX_BATCH_CANDIDATES + 1 {
            let call = tracked(&format!("call-{index:04}"), index as u64 + 1);
            history.push(Message::User {
                content: vec![UserContent::ToolResult(
                    call.result(vec![ToolResultContent::text(body(index))]),
                )],
            });
        }
        let prompt_call = tracked("call-prompt", (MAX_BATCH_CANDIDATES + 2) as u64);
        let prompt = Message::User {
            content: vec![UserContent::ToolResult(
                prompt_call.result(vec![ToolResultContent::text("P".repeat(PROMPT_BYTES))]),
            )],
        };
        let h = hook(WINDOW).with_output_budget(dir.path().to_path_buf(), run_id, tracker.clone());
        let limit = h.budget.input_limit();

        // Precondition 1: summary-only cannot serve this request, because the
        // newest result alone exceeds the whole input estimate.
        assert!(
            h.fit_with_report(&history, &prompt, 0).is_err(),
            "the raw summary-only fit must fail"
        );

        let budget = h.output_budget.as_ref().expect("a wired output budget");
        let estimated = |history: &[Message]| {
            let planned = h
                .planned_costs(history, &prompt)
                .unwrap()
                .expect("planning costs with a wired budget");
            h.fixed_cost
                .saturating_add(planned.prompt)
                .saturating_add(planned.history.iter().sum::<usize>())
        };
        let exact = |history: &[Message]| {
            let mut whole = history.to_vec();
            whole.push(prompt.clone());
            budget
                .externalized_cost(&whole, 0)
                .unwrap()
                .expect("the batch has externalizable bodies")
        };
        // Precondition 2: the per-message estimate and the exact whole-batch
        // plan disagree. The detail is that the exact plan pays for every body
        // the global candidate bound left inline.
        let gap = exact(&history).saturating_sub(estimated(&history));
        assert!(
            gap > 0,
            "the whole-batch candidate bound must leave externalizable bodies inline"
        );

        // Tune the slack so the per-message estimate lands just under the limit
        // while the exact whole-batch plan lands just over it: one large,
        // non-externalizable note consumes the rest of the room.
        let estimated_before = estimated(&history);
        assert!(estimated_before < limit);
        let slack = (gap / 3).max(1);
        let empty = message_cost(&Message::assistant(String::new())).unwrap();
        let filler_bytes = (limit - slack - estimated_before)
            .checked_sub(empty)
            .expect("the estimate must leave room for a non-externalizable note");
        history.insert(2, Message::assistant("A".repeat(filler_bytes)));
        // The note is not externalizable, so it shifts both costs by its own
        // serialized size and keeps the gap between them unchanged.
        assert_eq!(exact(&history) - estimated(&history), gap);
        let estimated_after = estimated(&history);
        assert!(
            estimated_after <= limit,
            "the per-message estimate must claim the request fits: {estimated_after} <= {limit}"
        );
        let exact_after = exact(&history);
        assert!(
            exact_after > limit,
            "the exact whole-batch plan must overflow: {exact_after} > {limit}"
        );

        // The decision must not take the estimate's word for it: the cut search
        // has to summarize and leave a request the dispatch plan can bound.
        let original = history.clone();
        let patched = patch_history(&h.completion_call_decision(&history, &prompt, 1));
        assert_ne!(patched, history, "the exact recheck must still summarize");
        assert_eq!(history, original, "the source history is never mutated");
        assert!(
            serde_json::to_string(&patched).unwrap().contains("GOAL-P0"),
            "the user goal survives the summary"
        );
        assert!(
            !patched.iter().any(|message| serde_json::to_string(message)
                .unwrap()
                .contains("output_references")),
            "no provisional reference may enter the patched history"
        );
        let dispatched = match h.dispatch_decision(&composed_request(&patched, &prompt), true) {
            DispatchAction::Patch(EffectKind::Completion { request, .. }) => request,
            other => panic!("the dispatch boundary must bound the composed request: {other:?}"),
        };
        assert!(
            serde_json::to_vec(&dispatched.chat_history).unwrap().len() < limit,
            "the dispatched request must fit the {limit}-byte estimate"
        );
    }
}

/// Manual, non-CI benchmark and equivalence harness for `BudgetHook::fit`.
///
/// Run with:
/// `cargo +1.95.0 test --release --locked -- --ignored --nocapture fit_scan_profile`
///
/// It prints a structural fingerprint of `fit` for a fixed set of cases plus
/// wall-clock timings for one large history, so the pre-optimization and
/// post-optimization versions of this file can be compared on identical inputs
/// without adding a flaky performance assertion to the suite.
#[cfg(test)]
mod fit_scan_profile {
    use super::*;
    use mio_core::message::{
        AssistantContent, AssistantMessage, Image, ToolCall, ToolFunction, ToolResultContent,
        UserContent,
    };
    use std::time::{Duration, Instant};

    fn hook(window: u32) -> BudgetHook {
        BudgetHook::new(
            ContextBudget {
                context_window: window,
                max_output_tokens: (window / 32).min(4096),
                source: "override",
            },
            "operating rules for this run",
            &[],
        )
        .unwrap()
    }

    fn call(id: &str) -> ToolCall {
        ToolCall::from_wire(
            id,
            ToolFunction::new(
                "read".try_into().unwrap(),
                serde_json::json!({ "path": "file" }),
            ),
        )
    }

    fn assistant(calls: &[ToolCall]) -> Message {
        Message::Assistant(AssistantMessage::new(
            calls
                .iter()
                .cloned()
                .map(AssistantContent::ToolCall)
                .collect(),
        ))
    }

    fn result(call: &ToolCall, text: String) -> Message {
        Message::User {
            content: vec![UserContent::ToolResult(
                call.result(vec![ToolResultContent::text(text)]),
            )],
        }
    }

    fn request_cost(hook: &BudgetHook, history: &[Message], prompt: &Message) -> usize {
        history.iter().fold(
            hook.fixed_cost
                .saturating_add(message_cost(prompt).unwrap()),
            |total, message| total.saturating_add(message_cost(message).unwrap()),
        )
    }

    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    fn fingerprint(result: &Result<Option<Vec<Message>>, String>) -> String {
        match result {
            Ok(None) => "no-patch".to_owned(),
            Ok(Some(history)) => {
                let json = serde_json::to_string(history).unwrap();
                format!(
                    "patched messages={} bytes={} hash={:016x}",
                    history.len(),
                    json.len(),
                    fnv1a(json.as_bytes())
                )
            }
            Err(error) => format!("error: {error}"),
        }
    }

    fn boundary_history(pad: usize) -> Vec<Message> {
        vec![
            Message::system("rules"),
            Message::user(format!("goal {}", "p".repeat(pad))),
            Message::assistant("answer"),
            Message::user("next"),
        ]
    }

    fn fixtures() -> Vec<(String, Vec<Message>, Message, u32)> {
        let mut cases = Vec::new();
        cases.push((
            "no_op".to_owned(),
            vec![Message::system("rules"), Message::user("hello")],
            Message::user("continue"),
            4096,
        ));

        let mut multi = vec![Message::system("first rule")];
        for index in 0..30 {
            multi.push(Message::user(format!(
                "user turn {index} {}",
                "u".repeat(180)
            )));
            multi.push(Message::assistant(format!(
                "assistant turn {index} {}",
                "a".repeat(180)
            )));
            if index == 9 || index == 19 {
                multi.push(Message::system(format!(
                    "constraint {index} {}",
                    "c".repeat(1500)
                )));
            }
        }
        cases.push((
            "multi_system".to_owned(),
            multi.clone(),
            Message::user("continue"),
            16_384,
        ));
        let mut late = multi;
        late.push(Message::system("late constraint"));
        late.push(Message::user("latest goal"));
        cases.push((
            "late_system".to_owned(),
            late,
            Message::user("continue"),
            16_384,
        ));

        let dropped_a = call("a");
        let dropped_b = call("b");
        let live = call("c");
        let pairs = vec![
            Message::system("rules"),
            assistant(&[dropped_a.clone(), dropped_b.clone()]),
            result(&dropped_a, "first".to_owned()),
            result(&dropped_b, "second".to_owned()),
            Message::user("middle"),
            assistant(std::slice::from_ref(&live)),
            result(&live, "c".repeat(3000)),
            Message::user("final goal"),
        ];
        cases.push((
            "tool_pairs".to_owned(),
            pairs,
            result(&call("pending"), "pending output".to_owned()),
            4096,
        ));

        let mut utf8 = vec![Message::system("规则：不要修改受保护文件")];
        for index in 0..40 {
            utf8.push(Message::user(format!(
                "目标 {index} {}",
                "内容".repeat(300)
            )));
            utf8.push(Message::assistant(format!(
                "结论 {index} {}",
                "总结".repeat(120)
            )));
        }
        utf8.push(Message::user("最近目标：部署并验证"));
        cases.push(("utf8_history".to_owned(), utf8, Message::user("继续"), 4096));

        let boundary_hook = hook(4096);
        let boundary_prompt = Message::user("continue");
        let unit = request_cost(&boundary_hook, &boundary_history(1), &boundary_prompt);
        let exact = boundary_hook.budget.input_limit() - unit + 1;
        cases.push((
            "budget_exact".to_owned(),
            boundary_history(exact),
            boundary_prompt.clone(),
            4096,
        ));
        cases.push((
            "budget_over_one".to_owned(),
            boundary_history(exact + 1),
            boundary_prompt.clone(),
            4096,
        ));

        let media = Message::User {
            content: vec![UserContent::Image(Image::default())],
        };
        cases.push((
            "media_in_history".to_owned(),
            vec![
                Message::system("rules"),
                Message::user("x".repeat(9000)),
                media.clone(),
            ],
            Message::user("continue"),
            4096,
        ));
        cases.push((
            "media_in_prompt".to_owned(),
            vec![Message::system("rules"), Message::user("x".repeat(9000))],
            media,
            4096,
        ));
        cases.push((
            "irreducible_system".to_owned(),
            vec![Message::system("rule".repeat(4000))],
            Message::user("continue"),
            4096,
        ));
        cases
    }

    /// Long history where every boundary before the archived block is rejected:
    /// the block stays inside the retained suffix, so `fit` walks all of those
    /// candidates on each call.
    fn walled_history() -> Vec<Message> {
        let mut history = vec![Message::system("run rules")];
        for index in 0..600 {
            if index > 0 && index % 20 == 0 {
                history.push(Message::system(format!(
                    "bulletin {index}: {}",
                    "b".repeat(8 * 1024)
                )));
            }
            if index % 2 == 0 {
                history.push(Message::user(format!(
                    "request {index} {}",
                    "u".repeat(160)
                )));
            } else {
                history.push(Message::assistant(format!(
                    "finding {index} {}",
                    "a".repeat(160)
                )));
            }
        }
        history.push(Message::assistant(format!(
            "archived output {}",
            "w".repeat(1_050_000)
        )));
        history.push(Message::user("final goal"));
        history.push(Message::assistant("final finding"));
        history
    }

    #[test]
    #[ignore = "manual pre/post optimization profile; run with --ignored --nocapture"]
    fn fit_scan_profile() {
        for (name, history, prompt, window) in fixtures() {
            let hook = hook(window);
            println!(
                "case={name} input_messages={} {}",
                history.len(),
                fingerprint(&hook.fit(&history, &prompt))
            );
        }

        let hook = hook(1_048_576);
        let prompt = Message::user("continue");
        let history = walled_history();
        println!(
            "case=large_walled input_messages={} input_bytes={}",
            history.len(),
            request_cost(&hook, &history, &prompt)
        );
        let mut best = Duration::from_secs(u64::MAX);
        let mut sample = None;
        for _ in 0..3 {
            let started = Instant::now();
            let fitted = hook.fit(&history, &prompt);
            let elapsed = started.elapsed();
            best = best.min(elapsed);
            sample = Some(fitted);
        }
        println!(
            "case=large_walled best_ms={:.2} {}",
            best.as_secs_f64() * 1_000.0,
            fingerprint(&sample.unwrap())
        );
    }
}
