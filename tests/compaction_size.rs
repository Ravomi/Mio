//! Serialized-size regressions for the session/manual compaction path.
//!
//! These drive `compact::compact_history` directly: the request-budget path is
//! covered by `context_budget`, and the end-to-end auto path by
//! `compaction_observation`. Every assertion compares real serialized bytes,
//! never an estimate.

use mio_coding_agent::{
    Message,
    compact::{compact_history, history_bytes},
};

/// The exact shape of the auto-compaction observation fixture: one carried
/// system record and eight short-record turns, compacted keeping six recent
/// messages.
fn short_record_history() -> Vec<Message> {
    let mut history = vec![Message::system("run rules")];
    for index in 0..8 {
        history.push(Message::user(format!(
            "old turn {index} {}",
            "x".repeat(200)
        )));
        history.push(Message::assistant(format!("old answer {index}")));
    }
    history
}

/// The measured regression: rendering every short record with `usize::MAX`
/// budget copied the records and the loss-report boilerplate, so the
/// replacement was larger than the history it replaced. A real render must end
/// below that history.
#[test]
fn short_records_compact_to_fewer_real_bytes() {
    let history = short_record_history();
    let before_bytes = history_bytes(&history);
    let before_messages = history.len();
    let (compacted, stats) = compact_history(&history, 6, None);

    assert!(
        stats.compacted_bytes < stats.original_bytes,
        "compaction must shrink the real serialized history: {} -> {}",
        stats.original_bytes,
        stats.compacted_bytes
    );
    assert!(compacted.len() < before_messages);
    assert_eq!(stats.original_bytes, before_bytes);
    assert_eq!(stats.original_messages, before_messages);
    assert_eq!(stats.compacted_bytes, history_bytes(&compacted));
    assert_eq!(stats.compacted_messages, compacted.len());
}

#[test]
fn carried_system_and_recent_tail_survive_verbatim() {
    let history = short_record_history();
    let (compacted, _stats) = compact_history(&history, 6, None);

    assert!(
        compacted
            .iter()
            .any(|message| matches!(message, Message::System { .. })),
        "the carried system record must survive"
    );
    assert_eq!(
        serde_json::to_value(&compacted[compacted.len() - 6..]).unwrap(),
        serde_json::to_value(&history[history.len() - 6..]).unwrap(),
        "the retained recent tail must stay a verbatim suffix"
    );
}

/// With no extractable record there is no honest summary to build: a
/// replacement would only add the notice and loss-report boilerplate, so the
/// history must be left unchanged and no compaction audited.
#[test]
fn tiny_history_without_extractable_records_is_left_unchanged() {
    let history = vec![
        Message::system("rules"),
        Message::system("more rules"),
        Message::system("still more rules"),
    ];
    let before_bytes = history_bytes(&history);
    let (compacted, stats) = compact_history(&history, 2, None);

    assert_eq!(
        compacted, history,
        "a non-beneficial compaction must not rewrite the history"
    );
    assert_eq!(stats.compacted_bytes, before_bytes);
    assert_eq!(stats.original_bytes, before_bytes);
    assert!(
        stats.loss_report.is_none(),
        "no honest size-reducing summary exists, so no compaction is audited"
    );
}

/// The audit's whole-history before/after metrics are the real serialized
/// histories, not estimates, and a squeezed summary still names its omitted
/// sections.
#[test]
fn report_before_and_after_are_the_real_serialized_histories() {
    let history = short_record_history();
    let (compacted, stats) = compact_history(&history, 6, None);
    let report = stats
        .loss_report
        .expect("a size-reducing compaction has an audit");

    assert_eq!(report.before_messages, Some(history.len()));
    assert_eq!(report.after_messages, Some(compacted.len()));
    assert_eq!(report.before_bytes, Some(history_bytes(&history)));
    assert_eq!(report.after_bytes, Some(history_bytes(&compacted)));
    assert_eq!(stats.original_bytes, report.before_bytes.unwrap());
    assert_eq!(stats.compacted_bytes, report.after_bytes.unwrap());
    assert!(report.after_bytes.unwrap() < report.before_bytes.unwrap());
    assert!(
        !report.truncated_sections.is_empty(),
        "a squeezed summary must report its omitted sections: {:?}",
        report.truncated_sections
    );
}
