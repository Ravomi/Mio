//! Offline fidelity regressions for `compact::compact_history`.
//!
//! These exercise the extractive summary without a model, network, or key. The
//! request-level budget behavior of `BudgetHook::fit` is covered by the unit
//! tests in `src/context_budget.rs`, because `BudgetHook` is deliberately
//! crate-private; this file only uses the public compaction API.

use mio_coding_agent::{
    Message,
    compact::{compact_history, history_bytes},
};
use mio_core::message::{
    AssistantContent, AssistantMessage, ToolCall, ToolFunction, ToolResultContent, UserContent,
};

fn call(id: &str, tool: &str) -> ToolCall {
    ToolCall::from_wire(
        id,
        ToolFunction::new(
            tool.try_into().unwrap(),
            serde_json::json!({ "path": "wire-spec.txt" }),
        ),
    )
}

fn assistant_calling(calls: &[ToolCall]) -> Message {
    Message::Assistant(AssistantMessage::new(
        calls
            .iter()
            .cloned()
            .map(AssistantContent::ToolCall)
            .collect(),
    ))
}

fn tool_result(call: &ToolCall, text: String, is_error: bool) -> Message {
    let result = if is_error {
        call.error_result(vec![ToolResultContent::text(text)])
    } else {
        call.result(vec![ToolResultContent::text(text)])
    };
    Message::User {
        content: vec![UserContent::ToolResult(result)],
    }
}

fn exploration(index: usize) -> (Message, Message) {
    (
        Message::user(format!("exploration {index} {}", "old ".repeat(80))),
        Message::assistant(format!("finding {index} {}", "noise ".repeat(60))),
    )
}

#[test]
fn content_without_a_text_excerpt_is_explicit_in_the_loss_audit() {
    let history = vec![
        Message::User {
            content: vec![UserContent::Image(Default::default())],
        },
        Message::assistant("earlier finding"),
        Message::user("recent request"),
        Message::assistant("recent answer"),
    ];
    let report = compact_history(&history, 2, None).1.loss_report.unwrap();
    assert_eq!(report.unrepresented_content_messages, 1);
    assert!(
        report
            .truncated_sections
            .contains(&"Non-extracted content".to_owned())
    );
}

#[test]
fn session_pins_and_manual_loss_report_survive_save_reload_and_fork() {
    use mio_coding_agent::{context_memory::PinnedContextKind, session::Session};
    let root = tempfile::tempdir().unwrap();
    let mut session = Session::new(root.path()).unwrap();
    let text = format!(
        "head {} middle constraint {} tail",
        "界".repeat(100),
        "文".repeat(100)
    );
    session
        .pinned_context
        .insert("rule", PinnedContextKind::Constraint, &text)
        .unwrap();
    session.history = vec![
        Message::user("earlier task ".repeat(400)),
        Message::assistant("earlier finding"),
        Message::user("recent task"),
        Message::assistant("recent answer"),
    ];
    let stats = session.compact(2, None);
    assert_eq!(
        stats.loss_report.as_ref().unwrap().pinned_items_preserved,
        Some(true)
    );
    assert_eq!(session.pinned_context.get("rule").unwrap().text, text);
    let path = root.path().join("session.json");
    session.save(&path).unwrap();
    let restored = Session::load(&path).unwrap();
    assert_eq!(restored.last_compaction_loss, stats.loss_report);
    assert_eq!(restored.pinned_context, session.pinned_context);
    let fork = restored.fork();
    assert_eq!(fork.pinned_context, restored.pinned_context);
    assert_eq!(fork.last_compaction_loss, restored.last_compaction_loss);
    let mut legacy = serde_json::to_value(&restored).unwrap();
    legacy.as_object_mut().unwrap().remove("pinned_context");
    legacy
        .as_object_mut()
        .unwrap()
        .remove("last_compaction_loss");
    let old: Session = serde_json::from_value(legacy).unwrap();
    assert!(old.pinned_context.is_empty());
    assert!(old.last_compaction_loss.is_none());
}

#[test]
fn structured_loss_report_locates_replaced_records_without_copying_content() {
    let failed = call("audit-call", "read");
    let history = vec![
        Message::system("system rules"),
        Message::user("private-original-message"),
        assistant_calling(std::slice::from_ref(&failed)),
        tool_result(
            &failed,
            format!("{}private-result-tail", "界".repeat(3000)),
            true,
        ),
        Message::user("current request"),
        Message::assistant("current finding"),
    ];
    let (_, stats) = compact_history(&history, 2, None);
    let report = stats
        .loss_report
        .expect("every actual compaction has an audit");
    assert_eq!((report.source_start, report.source_end), (0, 4));
    assert_eq!(report.source_bytes, history_bytes(&history[..4]));
    assert_eq!(report.preserved_system_messages, 1);
    assert_eq!(report.summary_bytes, stats.summary.len());
    assert_eq!(report.affected_tool_call_ids, ["audit-call"]);
    assert!(!report.tool_call_ids_truncated);
    assert!(
        report
            .truncated_sections
            .contains(&"Failed Tool Results".to_owned())
    );
    assert_eq!(report.pinned_items_preserved, None);
    let encoded = serde_json::to_string(&report).unwrap();
    assert!(!encoded.contains("private-original-message"));
    assert!(!encoded.contains("private-result-tail"));
    assert!(
        compact_history(&history[..2], 2, None)
            .1
            .loss_report
            .is_none()
    );
}

#[test]
fn loss_report_bounds_call_identifiers_and_marks_incomplete_audits() {
    let mut history = Vec::new();
    for index in 0..140 {
        let call = call(&format!("call-{index:03}"), "read");
        history.push(assistant_calling(std::slice::from_ref(&call)));
        history.push(tool_result(&call, "ok".to_owned(), false));
    }
    history.push(Message::user("current request"));
    history.push(Message::assistant("current finding"));
    let report = compact_history(&history, 2, None).1.loss_report.unwrap();
    assert_eq!(report.affected_tool_call_ids.len(), 128);
    assert!(report.tool_call_ids_truncated);
    assert!(
        report
            .affected_tool_call_ids
            .windows(2)
            .all(|pair| pair[0] < pair[1])
    );
}

/// Early constraints, the tail of a failed tool result, the tool call identity,
/// and the explicit loss report must all survive session compaction; the
/// middle that is dropped must be reported, never presented as complete.
#[test]
fn session_summary_keeps_constraints_tail_failures_and_reports_losses() {
    let failed = call("fail-7", "bash");
    let mut body = "diagnostic noise ".repeat(400);
    body.push_str("FINAL FAILURE: migration step 7 rejected the payload");
    let mut history = vec![
        Message::system("run rules"),
        Message::user("Constraint: preserve the public API and never write outside the workspace."),
        Message::assistant("noted"),
        assistant_calling(std::slice::from_ref(&failed)),
        tool_result(&failed, body, true),
    ];
    for index in 0..30 {
        let (user, assistant) = exploration(index);
        history.push(user);
        history.push(assistant);
    }
    history.push(Message::user(
        "Unfinished goal: deploy the release and verify the migration",
    ));
    history.push(Message::assistant("migration verified on staging"));

    let before = history.clone();
    let (_compacted, stats) = compact_history(&history, 4, None);

    assert!(
        stats
            .summary
            .contains("Constraint: preserve the public API"),
        "early constraint lost: {}",
        stats.summary
    );
    assert!(
        stats
            .summary
            .contains("FINAL FAILURE: migration step 7 rejected the payload"),
        "tail of the failed tool result lost: {}",
        stats.summary
    );
    assert!(
        stats.summary.contains("fail-7"),
        "tool call identity lost: {}",
        stats.summary
    );
    assert!(
        stats.summary.contains("chars omitted"),
        "bounded head/tail excerpts must report the omitted middle: {}",
        stats.summary
    );
    assert!(
        stats.summary.contains("Compaction Loss Report"),
        "the loss report must not disappear: {}",
        stats.summary
    );
    assert!(
        stats.summary.contains("not proof of completion"),
        "the loss report must not claim completion: {}",
        stats.summary
    );
    assert!(
        !stats.summary.contains(&"diagnostic noise ".repeat(20)),
        "an omitted middle must not be presented as a complete record"
    );
    assert_eq!(
        history, before,
        "compaction must not mutate the source history"
    );
    assert!(stats.compacted_bytes < stats.original_bytes);
}

/// Multi-byte records must keep a usable head and tail without replacement
/// characters, and the retained tail must stay a verbatim suffix.
#[test]
fn multibyte_summary_keeps_head_and_tail_without_replacement_characters() {
    let read = call("read-cjk", "read");
    let mut body = "中文诊断信息 ".repeat(400);
    body.push_str("最终结论：迁移验证通过");
    let mut history = vec![
        Message::system("规则：不得修改受保护文件"),
        assistant_calling(std::slice::from_ref(&read)),
        tool_result(&read, body, false),
    ];
    for index in 0..30 {
        history.push(Message::user(format!(
            "探索 {index} {}",
            "内容".repeat(200)
        )));
        history.push(Message::assistant(format!(
            "结论 {index} {}",
            "总结".repeat(150)
        )));
    }
    history.push(Message::user("最近目标：部署并验证"));
    history.push(Message::assistant("最近结论：已通过"));

    let before = history.clone();
    let (compacted, stats) = compact_history(&history, 4, None);

    assert!(
        !stats.summary.contains('\u{FFFD}'),
        "summary must not contain replacement characters: {}",
        stats.summary
    );
    assert!(
        stats.summary.contains("最终结论：迁移验证通过"),
        "multi-byte tail lost: {}",
        stats.summary
    );
    assert!(
        stats.summary.contains("chars omitted"),
        "multi-byte head/tail must report the omitted middle: {}",
        stats.summary
    );
    assert_eq!(
        history, before,
        "compaction must not mutate the source history"
    );
    assert_eq!(
        serde_json::to_value(&compacted[compacted.len() - 4..]).unwrap(),
        serde_json::to_value(&history[history.len() - 4..]).unwrap(),
        "the retained tail must stay a verbatim suffix"
    );
}

/// Compaction must cut only on complete tool exchanges: no orphan result and
/// no unanswered call, and the retained tail must be verbatim.
#[test]
fn compaction_never_splits_tool_call_result_pairs() {
    let old_a = call("old-a", "read");
    let old_b = call("old-b", "read");
    let live = call("live-c", "read");
    let mut history = vec![
        Message::system("rules"),
        assistant_calling(&[old_a.clone(), old_b.clone()]),
        tool_result(&old_a, "first".into(), false),
        tool_result(&old_b, "second".into(), false),
    ];
    for index in 0..25 {
        let (user, assistant) = exploration(index);
        history.push(user);
        history.push(assistant);
    }
    history.push(Message::user("latest question"));
    history.push(assistant_calling(std::slice::from_ref(&live)));
    history.push(tool_result(&live, "live output ".repeat(200), false));

    let (compacted, _) = compact_history(&history, 2, None);

    let mut pending = std::collections::BTreeSet::new();
    for message in &compacted {
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
    assert_eq!(
        serde_json::to_value(&compacted[compacted.len() - 2..]).unwrap(),
        serde_json::to_value(&history[history.len() - 2..]).unwrap(),
        "the active tool exchange must be retained verbatim"
    );
    assert!(history_bytes(&compacted) < history_bytes(&history));
}

/// A manual session compaction reports the whole replaced history and
/// classifies the explicit pinned registry by stable ID only. The report must
/// remain content-free and use the existing snake_case wire form.
#[test]
fn manual_compaction_reports_whole_history_and_pinned_ids_without_text() {
    use mio_coding_agent::context_memory::PinnedContextKind;
    use mio_coding_agent::session::Session;

    let root = tempfile::tempdir().unwrap();
    let mut session = Session::new(root.path()).unwrap();
    session
        .pinned_context
        .insert(
            "preserve-api",
            PinnedContextKind::Constraint,
            "Never rewrite the public API.",
        )
        .unwrap();
    session
        .pinned_context
        .insert(
            "finish-migration",
            PinnedContextKind::Goal,
            "Finish the session store migration.",
        )
        .unwrap();
    let mut history = vec![Message::system("rules")];
    for index in 0..12 {
        history.push(Message::user(format!("old {index} {}", "界".repeat(200))));
        history.push(Message::assistant(format!("finding {index}")));
    }
    history.push(Message::user("recent request"));
    history.push(Message::assistant("recent answer"));
    session.history = history.clone();

    let stats = session.compact(2, None);
    let report = stats.loss_report.expect("a real compaction has an audit");
    assert_eq!(report.before_messages, Some(history.len()));
    assert_eq!(report.after_messages, Some(session.history.len()));
    assert_eq!(report.before_bytes, Some(history_bytes(&history)));
    assert_eq!(report.after_bytes, Some(history_bytes(&session.history)));
    assert_eq!(report.pinned_items_preserved, Some(true));
    assert_eq!(
        report.preserved_constraint_ids,
        Some(vec!["preserve-api".to_owned()])
    );
    assert_eq!(
        report.preserved_goal_ids,
        Some(vec!["finish-migration".to_owned()])
    );
    assert_eq!(report.dropped_constraint_ids, Some(Vec::new()));
    assert_eq!(session.last_compaction_loss.as_ref(), Some(&report));

    let encoded = serde_json::to_string(&report).unwrap();
    assert!(!encoded.contains("public API"), "{encoded}");
    assert!(!encoded.contains("session store migration"), "{encoded}");
    assert!(encoded.contains("\"before_messages\""), "{encoded}");
    assert!(encoded.contains("\"after_bytes\""), "{encoded}");
    assert!(
        encoded.contains("\"preserved_constraint_ids\""),
        "{encoded}"
    );
    assert!(encoded.contains("\"preserved_goal_ids\""), "{encoded}");
    assert!(encoded.contains("\"dropped_constraint_ids\""), "{encoded}");

    // The whole report survives the session round trip unchanged.
    let path = root.path().join("session.json");
    session.save(&path).unwrap();
    let restored = Session::load(&path).unwrap();
    assert_eq!(restored.last_compaction_loss.as_ref(), Some(&report));
}

/// An empty pinned registry is unknown, never a false claim that no pinned
/// constraint exists.
#[test]
fn empty_pinned_registry_reports_unknown_ids() {
    use mio_coding_agent::session::Session;

    let root = tempfile::tempdir().unwrap();
    let mut session = Session::new(root.path()).unwrap();
    let mut history = vec![Message::user("old ".repeat(300))];
    for index in 0..8 {
        history.push(Message::user(format!("turn {index} {}", "x".repeat(120))));
        history.push(Message::assistant(format!("answer {index}")));
    }
    session.history = history;

    let stats = session.compact(2, None);
    let report = stats.loss_report.expect("a real compaction has an audit");
    assert_eq!(report.pinned_items_preserved, None);
    assert_eq!(report.preserved_constraint_ids, None);
    assert_eq!(report.preserved_goal_ids, None);
    assert_eq!(report.dropped_constraint_ids, None);
}

/// A report written before the new metrics existed still deserializes; the new
/// fields stay `None` instead of forcing a schema upgrade.
#[test]
fn legacy_loss_report_without_new_metrics_still_deserializes() {
    use mio_coding_agent::compact::CompactionLossReport;

    let legacy = serde_json::json!({
        "schema_version": 1,
        "source_start": 0,
        "source_end": 3,
        "source_bytes": 128,
        "preserved_system_messages": 1,
        "summary_bytes": 16,
        "affected_tool_call_ids": ["call-1"],
        "tool_call_ids_truncated": false,
        "truncated_sections": [],
        "unrepresented_content_messages": 0,
        "pinned_items_preserved": true,
        "extractive": true
    });
    let report: CompactionLossReport = serde_json::from_value(legacy).unwrap();
    assert_eq!(report.source_bytes, 128);
    assert_eq!(report.affected_tool_call_ids, ["call-1"]);
    assert_eq!(report.pinned_items_preserved, Some(true));
    assert_eq!(report.before_messages, None);
    assert_eq!(report.after_messages, None);
    assert_eq!(report.before_bytes, None);
    assert_eq!(report.after_bytes, None);
    assert_eq!(report.preserved_constraint_ids, None);
    assert_eq!(report.preserved_goal_ids, None);
    assert_eq!(report.dropped_constraint_ids, None);
}
