//! Per-request context accounting. UTF-8 serialized bytes are a deliberately
//! conservative token estimate, not a provider tokenizer or a capacity promise.
use crate::{Config, Message, Provider, RunOptions, compact};
use mio_agent::agent::{
    AgentHook, CompletionCallAction, CompletionCallEvent, HookContext, RequestPatch,
};
use mio_core::{catalog::Catalog, providers::registry::ProviderId};
use serde::Serialize;

pub const FALLBACK_CONTEXT_WINDOW: u32 = 32_768;
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;

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
            Provider::Codex => "openai",
            Provider::Antigravity if model.starts_with("claude") => "anthropic",
            Provider::Antigravity => "gcp.gemini",
            Provider::DeepseekWeb => "deepseek-web",
            Provider::Custom => options.catalog_provider.as_deref().unwrap_or("openai"),
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
/// Remote media references cannot be measured by this policy and are rejected.
fn message_cost(message: &Message) -> Result<usize, String> {
    use mio_core::message::{AssistantContent, ToolResultContent, UserContent};
    let unsupported = match message {
        Message::User { content } => content.iter().any(|content| match content {
            UserContent::Text(_) => false,
            UserContent::ToolResult(result) => result
                .content
                .iter()
                .any(|item| matches!(item, ToolResultContent::Image(_))),
            _ => true,
        }),
        Message::Assistant(message) => message
            .content
            .iter()
            .any(|content| matches!(content, AssistantContent::Image(_))),
        Message::System { .. } => false,
    };
    if unsupported {
        return Err("Context budget: media cannot be estimated; use text context".into());
    }
    serde_json::to_vec(message)
        .map(|bytes| bytes.len().saturating_add(16))
        .map_err(|_| "Context budget: cannot encode message".into())
}

pub(crate) struct BudgetHook {
    budget: ContextBudget,
    fixed_cost: usize,
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
        Ok(Self { budget, fixed_cost })
    }

    pub fn fit(
        &self,
        history: &[Message],
        prompt: &Message,
    ) -> Result<Option<Vec<Message>>, String> {
        let fixed = self.fixed_cost.saturating_add(message_cost(prompt)?);
        let limit = self.budget.input_limit();
        let costs = history
            .iter()
            .map(message_cost)
            .collect::<Result<Vec<_>, _>>()?;
        let mut suffix_costs = vec![0usize; history.len() + 1];
        for index in (0..history.len()).rev() {
            suffix_costs[index] = suffix_costs[index + 1].saturating_add(costs[index]);
        }
        if fixed.saturating_add(suffix_costs[0]) <= limit {
            return Ok(None);
        }
        // Preserve the newest complete suffix. If the prompt is a tool result,
        // its pending assistant call prevents cutting away that active exchange.
        for split in compact::safe_boundaries(history) {
            let systems = history[..split]
                .iter()
                .filter(|m| matches!(m, Message::System { .. }))
                .cloned()
                .collect::<Vec<_>>();
            let system_cost = systems
                .iter()
                .map(message_cost)
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .fold(0usize, usize::saturating_add);
            let retained_cost = fixed
                .saturating_add(suffix_costs[split])
                .saturating_add(system_cost);
            if retained_cost >= limit {
                continue;
            }
            let mut summary = compact::build_structured_summary(&history[..split], None);
            let cap = (limit / 4).min(4096);
            if summary.len() > cap {
                let mut end = cap.saturating_sub(32).min(summary.len());
                while !summary.is_char_boundary(end) {
                    end -= 1;
                }
                summary.truncate(end);
                summary.push_str("\n[Summary truncated]");
            }
            let summary = Message::user(format!(
                "[Earlier context summary; details may be lost]\n{summary}"
            ));
            if retained_cost.saturating_add(message_cost(&summary)?) <= limit {
                let mut replacement = systems;
                replacement.push(summary);
                replacement.extend_from_slice(&history[split..]);
                return Ok(Some(replacement));
            }
        }
        Err(format!(
            "Context budget: request cannot fit estimated input limit {limit} (window {}, output reserve {}). Reduce instructions, tools, prompt or tool output, or configure --context-window/--max-output-tokens for this deployment.",
            self.budget.context_window, self.budget.max_output_tokens
        ))
    }
}
impl AgentHook for BudgetHook {
    async fn on_completion_call(
        &self,
        _: &HookContext,
        event: CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        match self.fit(event.history, event.prompt) {
            Ok(None) => CompletionCallAction::Continue,
            Ok(Some(history)) => CompletionCallAction::Patch(RequestPatch::new().history(history)),
            Err(error) => CompletionCallAction::Stop(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mio_core::message::{
        AssistantContent, ToolCall, ToolFunction, ToolResultContent, UserContent,
    };
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
}
