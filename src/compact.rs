//! Context compaction engine (`/compact` and automatic pre-turn compaction).
//!
//! Compresses older conversation history into a structured summary while preserving
//! the most recent turns verbatim and ensuring tool-call/tool-result pairs are never split.

use crate::Message;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Default byte threshold for automatic compaction when enabled (512 KiB).
pub const DEFAULT_AUTO_COMPACT_BYTES: usize = 512 * 1024;
/// Default number of recent messages preserved verbatim during compaction.
pub const DEFAULT_KEEP_RECENT_MESSAGES: usize = 6;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactionStats {
    pub original_messages: usize,
    pub compacted_messages: usize,
    pub original_bytes: usize,
    pub compacted_bytes: usize,
    pub summary: String,
}

/// Returns the approximate serialized JSON byte size of `history`.
pub fn history_bytes(history: &[Message]) -> usize {
    serde_json::to_vec(history).map(|v| v.len()).unwrap_or(0)
}

/// Compact `history`, preserving up to `keep_recent` recent messages verbatim
/// (aligned to a non-tool-result user message boundary) and summarizing older messages.
pub fn compact_history(
    history: &[Message],
    keep_recent: usize,
    custom_focus: Option<&str>,
) -> (Vec<Message>, CompactionStats) {
    let original_bytes = history_bytes(history);
    let original_messages = history.len();
    let keep = keep_recent.max(2);

    if history.len() <= keep {
        return (
            history.to_vec(),
            CompactionStats {
                original_messages,
                compacted_messages: original_messages,
                original_bytes,
                compacted_bytes: original_bytes,
                summary: "History is already within the recent message budget.".into(),
            },
        );
    }

    let raw_split = history.len().saturating_sub(keep);
    let split_idx = find_safe_split_index(history, raw_split);
    if split_idx == 0 {
        return (
            history.to_vec(),
            CompactionStats {
                original_messages,
                compacted_messages: original_messages,
                original_bytes,
                compacted_bytes: original_bytes,
                summary: "No safe split boundary found for compaction.".into(),
            },
        );
    }

    let older = &history[..split_idx];
    let recent = &history[split_idx..];
    let summary = build_structured_summary(older, custom_focus);

    let mut compacted = Vec::with_capacity(2 + recent.len());
    compacted.push(Message::user(
        "[System Context Compaction: Earlier conversation turns have been summarized below to conserve context window.]",
    ));
    compacted.push(Message::assistant(summary.clone()));
    compacted.extend_from_slice(recent);

    let compacted_bytes = history_bytes(&compacted);
    let compacted_messages = compacted.len();

    (
        compacted,
        CompactionStats {
            original_messages,
            compacted_messages,
            original_bytes,
            compacted_bytes,
            summary,
        },
    )
}

fn find_safe_split_index(history: &[Message], target: usize) -> usize {
    // Walk backward or forward from `target` to find a `user` message that does NOT contain tool results.
    for idx in (1..=target).rev() {
        if is_plain_user_message(&history[idx]) {
            return idx;
        }
    }
    for (idx, msg) in history.iter().enumerate().skip(target + 1) {
        if is_plain_user_message(msg) {
            return idx;
        }
    }
    0
}

fn is_plain_user_message(msg: &Message) -> bool {
    let Ok(val) = serde_json::to_value(msg) else {
        return false;
    };
    if val["role"].as_str() != Some("user") {
        return false;
    }
    if let Some(arr) = val["content"].as_array() {
        !arr.iter().any(|item| {
            item["type"].as_str() == Some("tool_result")
                || item.get("tool_result").is_some()
                || item.get("call_id").is_some()
        })
    } else {
        true
    }
}

fn build_structured_summary(messages: &[Message], custom_focus: Option<&str>) -> String {
    let mut user_goals = Vec::new();
    let mut tool_actions = Vec::new();
    let mut assistant_notes = Vec::new();

    for msg in messages {
        let Ok(val) = serde_json::to_value(msg) else {
            continue;
        };
        let role = val["role"].as_str().unwrap_or("");
        match role {
            "user" => {
                extract_text_snippets(&val["content"], &mut user_goals, 240);
            }
            "assistant" => {
                extract_text_snippets(&val["content"], &mut assistant_notes, 300);
                extract_tool_calls(&val["content"], &mut tool_actions);
            }
            _ => {}
        }
    }

    let mut out = String::from("## Context Compaction Summary\n");
    if let Some(focus) = custom_focus.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str(&format!("Focus: {focus}\n\n"));
    }
    out.push_str(&format!(
        "Summarized {} earlier messages:\n",
        messages.len()
    ));

    if !user_goals.is_empty() {
        out.push_str("\n### User Requests\n");
        for goal in user_goals.iter().take(12) {
            out.push_str("- ");
            out.push_str(goal);
            out.push('\n');
        }
    }

    if !tool_actions.is_empty() {
        out.push_str("\n### Tool Operations Performed\n");
        for action in tool_actions.iter().take(20) {
            out.push_str("- ");
            out.push_str(action);
            out.push('\n');
        }
    }

    if !assistant_notes.is_empty() {
        out.push_str("\n### Key Assistant Findings & Progress\n");
        let start = assistant_notes.len().saturating_sub(8);
        for note in &assistant_notes[start..] {
            out.push_str("- ");
            out.push_str(note);
            out.push('\n');
        }
    }

    out.trim_end().to_owned()
}

fn extract_text_snippets(content: &Value, dest: &mut Vec<String>, max_chars: usize) {
    if let Some(s) = content.as_str() {
        let clean = collapse_whitespace(s, max_chars);
        if !clean.is_empty() {
            dest.push(clean);
        }
    } else if let Some(arr) = content.as_array() {
        for item in arr {
            if let Some(text) = item["text"].as_str().or_else(|| item["content"].as_str())
                && item["type"].as_str() != Some("tool_result")
            {
                let clean = collapse_whitespace(text, max_chars);
                if !clean.is_empty() {
                    dest.push(clean);
                }
            }
        }
    }
}

fn extract_tool_calls(content: &Value, dest: &mut Vec<String>) {
    let Some(arr) = content.as_array() else {
        return;
    };
    for item in arr {
        let func = if item["type"].as_str() == Some("tool_call") || item.get("function").is_some() {
            &item["function"]
        } else {
            continue;
        };
        let Some(name) = func["name"].as_str() else {
            continue;
        };
        let args = &func["arguments"];
        let detail = if let Some(op) = args["operation"].as_str() {
            let target = args["path"]
                .as_str()
                .or_else(|| args["pattern"].as_str())
                .or_else(|| args["value"].as_str())
                .unwrap_or("");
            format!("{name}:{op} {}", collapse_whitespace(target, 80))
        } else if let Some(path) = args["path"].as_str() {
            format!("{name} {}", collapse_whitespace(path, 80))
        } else if let Some(cmd) = args["command"].as_str() {
            format!("{name} `{}`", collapse_whitespace(cmd, 80))
        } else {
            name.to_owned()
        };
        let trimmed = detail.trim().to_owned();
        if !dest.contains(&trimmed) {
            dest.push(trimmed);
        }
    }
}

fn collapse_whitespace(text: &str, max_chars: usize) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max_chars {
        collapsed
    } else {
        let mut truncated: String = collapsed.chars().take(max_chars).collect();
        truncated.push_str("...");
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compact_history_reduces_messages_and_preserves_recent() {
        let mut history = Vec::new();
        for i in 1..=10 {
            history.push(Message::user(format!("User request number {i}")));
            history.push(Message::assistant(format!(
                "Assistant completed step {i} with detailed explanation."
            )));
        }
        let (compacted, stats) = compact_history(&history, 4, Some("Focus on step 1-8"));
        assert_eq!(stats.original_messages, 20);
        assert_eq!(compacted.len(), 6); // 2 summary + 4 recent
        assert!(stats.summary.contains("Focus: Focus on step 1-8"));
        assert!(stats.summary.contains("User request number 1"));
        let last_json = serde_json::to_string(compacted.last().unwrap()).unwrap();
        assert!(last_json.contains("Assistant completed step 10"));
    }
}
