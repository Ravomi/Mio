//! Bounded optional `grep` neighbor context (`context_lines`).
//!
//! These are offline regressions for the native `grep` tool and the aggregate
//! `workspace` tool: an optional non-negative `context_lines` (0-20, default 0)
//! returns neighboring lines so a caller does not need a follow-up `read` just
//! to see the surrounding statement. The old output must stay byte-identical
//! when the option is omitted or explicitly 0, real matches keep priority over
//! any amount of context, request metadata stays honest, and every existing
//! hard cap (64 KiB output, 64 KiB line, 2 MiB file, traversal depth, 500
//! matches, workspace/symlink boundaries) is unchanged.
//!
//! Arguments are always built from JSON so a missing/unknown field is observed
//! as behavior, not as a compile error.

use mio_coding_agent::{
    Arguments, Host, HostFuture, Message, Workspace,
    compact::compact_history,
    workspace::{GrepArgs, MAX_GREP_TOTAL_BYTES},
};
use mio_core::message::{
    AssistantContent, AssistantMessage, ToolCall, ToolFunction, ToolResultContent, UserContent,
};
use mio_core::tool::PortableTool;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};

struct DummyHost;

impl Host for DummyHost {
    fn shell(&self, _cmd: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }
    fn on_event(&self, _event: mio_coding_agent::AgentEvent) {}
}

/// One grep request as the model would send it, with `context_lines` only when
/// explicitly requested (so omission is distinguishable from an explicit 0).
fn request(pattern: &str, path: Option<&str>, context_lines: Option<i64>) -> Value {
    let mut value = json!({"operation": "grep", "pattern": pattern});
    if let Some(path) = path {
        value["path"] = json!(path);
    }
    if let Some(lines) = context_lines {
        value["context_lines"] = json!(lines);
    }
    value
}

fn aggregate_args(pattern: &str, path: Option<&str>, context_lines: Option<i64>) -> Arguments {
    serde_json::from_value(request(pattern, path, context_lines)).expect("aggregate grep args")
}

async fn aggregate(
    ws: &Workspace,
    pattern: &str,
    path: Option<&str>,
    context_lines: Option<i64>,
) -> Result<String, String> {
    ws.execute(aggregate_args(pattern, path, context_lines))
        .await
}

/// Drive the native `grep` portable tool through its real typed args.
async fn native(
    ws: &Workspace,
    pattern: &str,
    path: Option<&str>,
    context_lines: Option<i64>,
) -> String {
    let args: GrepArgs =
        serde_json::from_value(request(pattern, path, context_lines)).expect("native grep args");
    let value = PortableTool::call(&mio_coding_agent::GrepTool(ws.clone()), args)
        .await
        .expect("native grep is infallible at the type level");
    value["output"]
        .as_str()
        .or_else(|| value["error"].as_str())
        .unwrap_or_default()
        .to_owned()
}

fn workspace(folder: &tempfile::TempDir) -> Workspace {
    Workspace::new(folder.path(), Arc::new(DummyHost)).expect("workspace")
}

#[tokio::test]
async fn omitted_and_explicit_zero_context_stay_byte_identical_to_the_legacy_output() {
    let folder = tempfile::tempdir().unwrap();
    let ws = workspace(&folder);
    std::fs::create_dir(folder.path().join("sub")).unwrap();
    std::fs::write(
        folder.path().join("b_file.txt"),
        "hello keyword world\nsecond line\n",
    )
    .unwrap();
    std::fs::write(
        folder.path().join("sub/a_file.txt"),
        "another keyword match\n",
    )
    .unwrap();

    let legacy = ws.execute(Arguments::grep("keyword", None)).await.unwrap();
    let omitted = aggregate(&ws, "keyword", None, None).await.unwrap();
    let zero = aggregate(&ws, "keyword", None, Some(0)).await.unwrap();

    let expected = vec![
        "b_file.txt:1: hello keyword world",
        "sub/a_file.txt:1: another keyword match",
    ];
    assert_eq!(legacy.lines().collect::<Vec<_>>(), expected);
    assert_eq!(omitted, legacy);
    assert_eq!(zero, legacy);
    // The dash-separated context shape must not leak into the default output.
    assert!(!legacy.contains("b_file.txt-"));
}

#[tokio::test]
async fn context_lines_returns_neighboring_lines_in_the_standard_dash_format() {
    let folder = tempfile::tempdir().unwrap();
    let ws = workspace(&folder);
    std::fs::write(
        folder.path().join("ctx.txt"),
        "alpha\nbefore\nneedle\nafter\nomega\n",
    )
    .unwrap();

    let output = aggregate(&ws, "needle", Some("ctx.txt"), Some(1))
        .await
        .unwrap();
    assert_eq!(
        output.lines().collect::<Vec<_>>(),
        vec!["ctx.txt-2- before", "ctx.txt:3: needle", "ctx.txt-4- after",]
    );
}

#[tokio::test]
async fn native_and_aggregate_grep_share_the_same_context_output() {
    let folder = tempfile::tempdir().unwrap();
    let ws = workspace(&folder);
    std::fs::write(
        folder.path().join("shared.txt"),
        "one\ntwo target\nthree\nfour\n",
    )
    .unwrap();

    let aggregate_output = aggregate(&ws, "target", Some("shared.txt"), Some(2))
        .await
        .unwrap();
    let native_output = native(&ws, "target", Some("shared.txt"), Some(2)).await;
    assert_eq!(native_output, aggregate_output);
    assert!(
        native_output.contains("shared.txt:2: two target"),
        "{native_output}"
    );
    assert!(
        native_output.contains("shared.txt-1- one"),
        "{native_output}"
    );
}

#[tokio::test]
async fn intersecting_context_windows_merge_and_never_duplicate_a_line() {
    let folder = tempfile::tempdir().unwrap();
    let ws = workspace(&folder);
    std::fs::write(
        folder.path().join("merge.txt"),
        "l1\nl2\nl3 need\nl4\nl5\nl6 need2\nl7\nl8\nl9\nl10\n",
    )
    .unwrap();

    let output = aggregate(&ws, "need", Some("merge.txt"), Some(2))
        .await
        .unwrap();
    assert_eq!(
        output.lines().collect::<Vec<_>>(),
        vec![
            "merge.txt-1- l1",
            "merge.txt-2- l2",
            "merge.txt:3: l3 need",
            "merge.txt-4- l4",
            "merge.txt-5- l5",
            "merge.txt:6: l6 need2",
            "merge.txt-7- l7",
            "merge.txt-8- l8",
        ]
    );
    for line in output.lines() {
        assert_eq!(output.lines().filter(|other| *other == line).count(), 1);
    }
}

#[tokio::test]
async fn context_lines_stop_at_real_file_boundaries() {
    let folder = tempfile::tempdir().unwrap();
    let ws = workspace(&folder);
    std::fs::write(
        folder.path().join("edge.txt"),
        "first needle\nsecond\nthird\nfourth needle\n",
    )
    .unwrap();

    let output = aggregate(&ws, "needle", Some("edge.txt"), Some(2))
        .await
        .unwrap();
    assert_eq!(
        output.lines().collect::<Vec<_>>(),
        vec![
            "edge.txt:1: first needle",
            "edge.txt-2- second",
            "edge.txt-3- third",
            "edge.txt:4: fourth needle",
        ]
    );
    // No line 0 and nothing past the last real line.
    assert!(!output.contains("edge.txt-0-"));
    assert!(!output.contains("edge.txt-5-"));
}

#[tokio::test]
async fn context_lines_sort_across_multiple_files() {
    let folder = tempfile::tempdir().unwrap();
    let ws = workspace(&folder);
    std::fs::create_dir(folder.path().join("dir")).unwrap();
    std::fs::write(folder.path().join("b.txt"), "b1\nb2 needle\nb3\n").unwrap();
    std::fs::write(folder.path().join("dir/a.txt"), "a1 needle\na2\n").unwrap();

    let output = aggregate(&ws, "needle", None, Some(1)).await.unwrap();
    assert_eq!(
        output.lines().collect::<Vec<_>>(),
        vec![
            "b.txt-1- b1",
            "b.txt:2: b2 needle",
            "b.txt-3- b3",
            "dir/a.txt:1: a1 needle",
            "dir/a.txt-2- a2",
        ]
    );
}

#[tokio::test]
async fn context_lines_follow_the_same_regex_and_case_rules_as_the_match() {
    let folder = tempfile::tempdir().unwrap();
    let ws = workspace(&folder);
    std::fs::write(
        folder.path().join("case.txt"),
        "Alpha target\nbeta\nGamma\nalpha target\n",
    )
    .unwrap();

    let mut args = aggregate_args("^alpha", Some("case.txt"), Some(1));
    args.regex = Some(true);
    args.case_insensitive = Some(true);
    let output = ws.execute(args).await.unwrap();
    assert_eq!(
        output.lines().collect::<Vec<_>>(),
        vec![
            "case.txt:1: Alpha target",
            "case.txt-2- beta",
            "case.txt-3- Gamma",
            "case.txt:4: alpha target",
        ]
    );

    // Case-sensitive literal search over the same file keeps its own matches.
    let sensitive = aggregate(&ws, "Alpha", Some("case.txt"), Some(1))
        .await
        .unwrap();
    assert_eq!(
        sensitive.lines().collect::<Vec<_>>(),
        vec!["case.txt:1: Alpha target", "case.txt-2- beta"]
    );
}

#[tokio::test]
async fn no_match_returns_no_orphan_context() {
    let folder = tempfile::tempdir().unwrap();
    let ws = workspace(&folder);
    std::fs::write(folder.path().join("plain.txt"), "alpha\nbeta\ngamma\n").unwrap();

    let output = aggregate(&ws, "zzz", Some("plain.txt"), Some(5))
        .await
        .unwrap();
    assert_eq!(output, "未找到匹配内容");
    assert!(!output.contains("plain.txt-"));
}

#[tokio::test]
async fn context_lines_up_to_twenty_are_accepted_and_above_are_rejected() {
    let folder = tempfile::tempdir().unwrap();
    let ws = workspace(&folder);
    let mut body = String::new();
    for index in 0..40 {
        body.push_str(&format!("line {index} target\n"));
    }
    std::fs::write(folder.path().join("range.txt"), body).unwrap();

    for lines in 0..=20i64 {
        assert!(
            aggregate(&ws, "target", Some("range.txt"), Some(lines))
                .await
                .is_ok(),
            "context_lines={lines} must be accepted"
        );
    }
    for lines in [21i64, 50, 10_000] {
        let error = aggregate(&ws, "target", Some("range.txt"), Some(lines))
            .await
            .unwrap_err();
        assert!(error.contains("context_lines"), "{error}");
    }
    // The native tool rejects the same values through the shared implementation.
    let native_error = native(&ws, "target", Some("range.txt"), Some(21)).await;
    assert!(native_error.contains("context_lines"), "{native_error}");
}

#[test]
fn negative_and_wrong_type_context_values_are_rejected() {
    for bad in [json!(-1), json!("3"), json!(1.5), json!(true)] {
        let mut value = request("needle", None, None);
        value["context_lines"] = bad.clone();
        assert!(
            serde_json::from_value::<Arguments>(value.clone()).is_err(),
            "aggregate args must reject context_lines={bad}"
        );
        assert!(
            serde_json::from_value::<GrepArgs>(value).is_err(),
            "native args must reject context_lines={bad}"
        );
    }
}

#[test]
fn schemas_and_descriptions_declare_a_bounded_optional_context_lines() {
    let folder = tempfile::tempdir().unwrap();
    let ws = workspace(&folder);

    for schema in [
        PortableTool::parameters(&mio_coding_agent::GrepTool(ws.clone())),
        PortableTool::parameters(&ws),
    ] {
        let property = &schema["properties"]["context_lines"];
        assert_eq!(property["type"], "integer", "{schema}");
        assert_eq!(property["minimum"], 0, "{schema}");
        assert_eq!(property["maximum"], 20, "{schema}");
    }
    assert!(
        PortableTool::description(&mio_coding_agent::GrepTool(ws.clone()))
            .contains("context_lines")
    );
    assert!(PortableTool::description(&ws).contains("context_lines"));
}

#[tokio::test]
async fn a_preceding_huge_context_cannot_squeeze_out_a_later_real_match() {
    let folder = tempfile::tempdir().unwrap();
    let ws = workspace(&folder);

    let mut first = String::from("needle_a\n");
    for index in 0..40 {
        first.push_str(&format!("filler {index} {}\n", "x".repeat(8 * 1024)));
    }
    std::fs::write(folder.path().join("a.txt"), first).unwrap();
    std::fs::write(folder.path().join("b.txt"), "needle_b\n").unwrap();

    let output = aggregate(&ws, "needle", None, Some(20)).await.unwrap();
    assert!(output.contains("a.txt-2- "), "{output}");
    assert!(
        output.contains("b.txt:1: needle_b"),
        "a later real match must never be squeezed out by preceding context: {output}"
    );
    assert!(output.contains("[TRUNCATED"), "{output}");
    assert!(output.contains("达到最大输出字节数限制"), "{output}");
    assert!(output.len() <= MAX_GREP_TOTAL_BYTES);
}

#[tokio::test]
async fn a_huge_utf8_context_line_truncates_without_losing_the_match() {
    let folder = tempfile::tempdir().unwrap();
    let ws = workspace(&folder);
    let long_line = "中".repeat(40_000);
    std::fs::write(
        folder.path().join("utf8.txt"),
        format!("needle_here\n{long_line}\ntail\n"),
    )
    .unwrap();

    let output = aggregate(&ws, "needle_here", Some("utf8.txt"), Some(1))
        .await
        .unwrap();
    assert!(
        output.contains("utf8.txt:1: needle_here"),
        "the real match must survive a huge neighboring line"
    );
    assert!(output.contains("utf8.txt-2- "), "{output}");
    assert!(output.contains("[TRUNCATED LINE]"), "{output}");
    assert!(output.len() <= MAX_GREP_TOTAL_BYTES);
}

#[tokio::test]
async fn the_match_cap_counts_real_matches_not_context_lines() {
    let folder = tempfile::tempdir().unwrap();
    let ws = workspace(&folder);
    let mut body = String::new();
    for index in 1..=600 {
        body.push_str(&format!("line {index} special_tag\n"));
    }
    std::fs::write(folder.path().join("many_lines.txt"), body).unwrap();

    let output = aggregate(&ws, "special_tag", Some("many_lines.txt"), Some(5))
        .await
        .unwrap();
    assert!(output.contains("达到最大匹配数限制 500"), "{output}");

    let match_lines = output
        .lines()
        .filter(|line| line.starts_with("many_lines.txt:"))
        .count();
    assert_eq!(match_lines, 500);
    let context_lines = output
        .lines()
        .filter(|line| line.starts_with("many_lines.txt-"))
        .count();
    assert!(
        context_lines > 0,
        "the file that reached the match cap still gets bounded context"
    );
}

#[tokio::test]
async fn symlink_and_workspace_boundaries_are_unchanged_with_context() {
    let folder = tempfile::tempdir().unwrap();
    let ws = workspace(&folder);
    std::fs::write(folder.path().join("real.txt"), "needle\naround\n").unwrap();
    std::fs::write(folder.path().join("outside.txt"), "needle outside\n").unwrap();

    // A path outside the workspace root stays rejected.
    let escape = aggregate(&ws, "needle", Some("../outside.txt"), Some(1)).await;
    assert!(escape.is_err(), "{escape:?}");

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            folder.path().join("real.txt"),
            folder.path().join("link.txt"),
        )
        .unwrap();
        let legacy = ws
            .execute(Arguments::grep("needle", Some("link.txt".into())))
            .await;
        let with_context = aggregate(&ws, "needle", Some("link.txt"), Some(1)).await;
        assert!(legacy.is_err() && with_context.is_err());
        assert_eq!(legacy.unwrap_err(), with_context.unwrap_err());
    }

    // A read-only workspace keeps the same read-only search capability.
    let read_only = Workspace::new_read_only(folder.path(), Arc::new(DummyHost)).unwrap();
    assert!(read_only.is_read_only());
    let output = aggregate(&read_only, "needle", Some("real.txt"), Some(1))
        .await
        .unwrap();
    assert_eq!(
        output.lines().collect::<Vec<_>>(),
        vec!["real.txt:1: needle", "real.txt-2- around"]
    );
}

// ---------------------------------------------------------------------------
// Requested (not returned) context metadata in the compaction summary.
// ---------------------------------------------------------------------------

fn requested_grep(call_args: Value) -> ToolCall {
    ToolCall::from_wire(
        "grep-context",
        ToolFunction::new("grep".try_into().unwrap(), call_args),
    )
}

/// Compact a history whose older half is exactly one requested grep call, so
/// the summary must carry the requested arguments the model itself sent.
fn compacted_summary(call_args: Value) -> String {
    let call = requested_grep(call_args);
    let assistant = Message::Assistant(AssistantMessage::new(vec![AssistantContent::ToolCall(
        call.clone(),
    )]));
    let result = Message::User {
        content: vec![UserContent::ToolResult(
            call.result(vec![ToolResultContent::text("old grep output")]),
        )],
    };
    let mut history = vec![Message::system("run rules"), assistant, result];
    // Enough older padding that a real extractive shrink happens.
    for index in 0..30 {
        history.push(Message::user(format!(
            "exploration {index} {}",
            "old ".repeat(80)
        )));
        history.push(Message::assistant(format!(
            "finding {index} {}",
            "noise ".repeat(60)
        )));
    }
    let before = history.len();
    let (_, stats) = compact_history(&history, 4, None);
    assert_eq!(stats.original_messages, before);
    stats.summary
}

#[test]
fn requested_context_lines_is_preserved_verbatim_in_the_summary() {
    let five = compacted_summary(json!({"pattern": "needle", "context_lines": 5}));
    assert!(
        five.contains("context_lines=5"),
        "the requested context_lines must survive into the summary: {five}"
    );
    let zero = compacted_summary(json!({"pattern": "needle", "context_lines": 0}));
    assert!(
        zero.contains("context_lines=0"),
        "an explicit requested 0 is still a requested value: {zero}"
    );
}

#[test]
fn unknown_context_line_shapes_are_never_guessed_in_the_summary() {
    for bad in [json!("5"), json!(-1), json!(1.5), json!(true), json!(null)] {
        let summary = compacted_summary(json!({"pattern": "needle", "context_lines": bad}));
        assert!(
            !summary.contains("context_lines"),
            "an unsupported shape must not become a requested range: {summary}"
        );
    }
}
