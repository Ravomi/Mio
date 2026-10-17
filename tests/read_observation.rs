//! Real `Workspace`/`Host` tests for the successful `read` observation.
//!
//! They exercise both entry points (`workspace` with `operation: "read"` and
//! the standalone `read` tool) against a real on-disk workspace and a real
//! `Host`, and assert that the emitted observation reflects the range the tool
//! actually returned, carries no file contents and no absolute path, and is
//! only emitted on success.

use mio_coding_agent::{
    AgentEvent, Arguments, Host, HostFuture, ReadObservation, ReadRangeKind, ReadTool, Workspace,
    read_observation::MAX_READ_PATH_CHARS,
    tool_identity::{ToolIdentity, scope},
    workspace::{MIN_READ_OUTPUT_BYTES, ReadArgs},
};
use mio_core::tool::PortableTool;
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

/// Real host that records every read observation the engine emits.
#[derive(Default)]
struct RecordingHost {
    reads: Mutex<Vec<ReadObservation>>,
}

impl RecordingHost {
    fn take(&self) -> Vec<ReadObservation> {
        std::mem::take(&mut *self.reads.lock().unwrap())
    }
}

impl Host for RecordingHost {
    fn shell(&self, _command: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn on_event(&self, _event: AgentEvent) {}

    fn on_read_observation(&self, observation: &ReadObservation) {
        self.reads.lock().unwrap().push(observation.clone());
    }
}

/// One read observation from a recording host, asserting exactly one was emitted.
fn only_read(host: &Arc<RecordingHost>) -> ReadObservation {
    let reads = host.take();
    assert_eq!(
        reads.len(),
        1,
        "expected exactly one read observation: {reads:?}"
    );
    reads.into_iter().next().unwrap()
}

/// Assert the serialized observation leaks neither contents nor host paths.
fn assert_content_free(observation: &ReadObservation, folder: &Path, forbidden: &[&str]) {
    let serialized = serde_json::to_string(observation).unwrap();
    let absolute = folder.to_string_lossy().to_string();
    assert!(
        !serialized.contains(&absolute),
        "observation leaked the absolute workspace path: {serialized}"
    );
    for needle in forbidden {
        assert!(
            !serialized.contains(needle),
            "observation leaked content {needle:?}: {serialized}"
        );
    }
    assert!(
        !observation.path.starts_with('/'),
        "observation path must be workspace-relative: {}",
        observation.path
    );
}

fn five_lines() -> String {
    (1..=5)
        .map(|index| format!("line-{index}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn truncated_line_read_reports_the_actual_returned_range() {
    let folder = tempfile::tempdir().unwrap();
    let marker = "SECRET-CONTENT-MARKER";
    let content = (1..=200)
        .map(|index| format!("{marker} line-{index:04} padded"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(folder.path().join("spec.txt"), &content).unwrap();

    let host = Arc::new(RecordingHost::default());
    let workspace = Workspace::new(folder.path(), host.clone())
        .unwrap()
        .with_read_output_limit_bytes(MIN_READ_OUTPUT_BYTES);

    let output = workspace
        .execute(Arguments::read("spec.txt"))
        .await
        .unwrap();
    assert!(
        output.starts_with("[TRUNCATED:"),
        "read was not truncated: {output:?}"
    );
    let body = output.split_once("]\n").expect("truncation header").1;
    let actually_returned = body.lines().count() as u64;
    assert!((1..200).contains(&actually_returned));

    let observation = only_read(&host);
    assert_eq!(observation.kind, ReadRangeKind::Line);
    assert_eq!(observation.path, "spec.txt");
    assert_eq!(observation.offset, Some(1));
    assert_eq!(observation.limit, Some(actually_returned));
    assert_eq!(observation.total, Some(200));
    assert!(observation.truncated);
    assert!(observation.byte_offset.is_none());
    assert!(observation.byte_limit.is_none());
    assert_content_free(&observation, folder.path(), &[marker]);
}

#[tokio::test]
async fn explicit_limit_reports_the_requested_window_and_its_actual_end() {
    let folder = tempfile::tempdir().unwrap();
    std::fs::write(folder.path().join("five.txt"), five_lines()).unwrap();

    let host = Arc::new(RecordingHost::default());
    let workspace = Workspace::new(folder.path(), host.clone()).unwrap();

    // A window that stops before EOF: two lines, marked truncated.
    workspace
        .execute(Arguments::read_paged("five.txt", 2, 2))
        .await
        .unwrap();
    let truncated = only_read(&host);
    assert_eq!(truncated.kind, ReadRangeKind::Line);
    assert_eq!(truncated.offset, Some(2));
    assert_eq!(truncated.limit, Some(2));
    assert_eq!(truncated.total, Some(5));
    assert!(truncated.truncated);

    // A window that consumes the rest of the file: exact, not truncated.
    let output = workspace
        .execute(Arguments::read_paged("five.txt", 4, 5))
        .await
        .unwrap();
    assert_eq!(output, "line-4\nline-5");
    let exact = only_read(&host);
    assert_eq!(exact.offset, Some(4));
    assert_eq!(exact.limit, Some(2));
    assert_eq!(exact.total, Some(5));
    assert!(!exact.truncated);
}

#[tokio::test]
async fn byte_paging_reports_actual_utf8_bytes_without_content() {
    let folder = tempfile::tempdir().unwrap();
    let content = "界面保持简洁🌊";
    std::fs::write(folder.path().join("out.txt"), content).unwrap();

    let host = Arc::new(RecordingHost::default());
    let workspace = Workspace::new(folder.path(), host.clone()).unwrap();

    let mut args = Arguments::read("out.txt");
    args.byte_offset = Some(0);
    args.byte_limit = Some(4);
    workspace.execute(args).await.unwrap();

    let observation = only_read(&host);
    assert_eq!(observation.kind, ReadRangeKind::Byte);
    assert_eq!(observation.path, "out.txt");
    assert_eq!(observation.byte_offset, Some(0));
    // 4 requested bytes would split the second character: only 3 whole bytes returned.
    assert_eq!(observation.byte_limit, Some(3));
    assert_eq!(observation.total, Some(content.len() as u64));
    assert!(observation.truncated);
    assert!(observation.offset.is_none());
    assert!(observation.limit.is_none());
    assert_content_free(&observation, folder.path(), &["界", "保持", "🌊"]);
}

#[tokio::test]
async fn empty_file_reports_an_explicit_zero_length_unknown_range() {
    let folder = tempfile::tempdir().unwrap();
    std::fs::write(folder.path().join("empty.txt"), "").unwrap();

    let host = Arc::new(RecordingHost::default());
    let workspace = Workspace::new(folder.path(), host.clone()).unwrap();

    // Entry 1: the workspace tool.
    let output = workspace
        .execute(Arguments::read("empty.txt"))
        .await
        .unwrap();
    assert_eq!(output, "");
    let observation = only_read(&host);
    assert_eq!(observation.kind, ReadRangeKind::Unknown);
    assert_eq!(observation.path, "empty.txt");
    assert_eq!(observation.total, Some(0));
    assert!(!observation.truncated);
    assert!(observation.offset.is_none());
    assert!(observation.limit.is_none());
    assert!(observation.byte_offset.is_none());
    assert!(observation.byte_limit.is_none());
    assert_content_free(&observation, folder.path(), &[]);

    // A line offset beyond an empty file is still the same empty success.
    let output = workspace
        .execute(Arguments::read_paged("empty.txt", 7, 3))
        .await
        .unwrap();
    assert_eq!(output, "");
    let observation = only_read(&host);
    assert_eq!(observation.kind, ReadRangeKind::Unknown);
    assert_eq!(observation.total, Some(0));
}

#[tokio::test]
async fn rejected_reads_emit_no_observation() {
    let folder = tempfile::tempdir().unwrap();
    std::fs::write(folder.path().join("ok.txt"), "ok").unwrap();

    let host = Arc::new(RecordingHost::default());
    let workspace = Workspace::new(folder.path(), host.clone()).unwrap();

    assert!(
        workspace
            .execute(Arguments::read("../outside.txt"))
            .await
            .is_err()
    );
    assert!(
        workspace
            .execute(Arguments::read("/etc/passwd"))
            .await
            .is_err()
    );
    assert!(
        workspace
            .execute(Arguments::read_paged("ok.txt", 0, 1))
            .await
            .is_err()
    );
    assert!(
        workspace
            .execute(Arguments::read_paged("ok.txt", 9, 1))
            .await
            .is_err()
    );
    assert!(
        host.reads.lock().unwrap().is_empty(),
        "a failed or rejected read must not report success"
    );
}

#[tokio::test]
async fn unrepresentable_relative_paths_emit_nothing_but_the_read_succeeds() {
    let folder = tempfile::tempdir().unwrap();
    let deep = "d".repeat(100);
    let directory = folder.path().join(&deep).join(&deep).join(&deep);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("f.txt"), "payload").unwrap();

    let relative = format!("{deep}/{deep}/{deep}/f.txt");
    assert!(relative.chars().count() > MAX_READ_PATH_CHARS);

    let host = Arc::new(RecordingHost::default());
    let workspace = Workspace::new(folder.path(), host.clone()).unwrap();
    let output = workspace.execute(Arguments::read(relative)).await.unwrap();
    assert_eq!(output, "payload");
    assert!(
        host.reads.lock().unwrap().is_empty(),
        "an over-long path must not be emitted (and never as an absolute path)"
    );
}

#[tokio::test]
async fn both_entry_points_emit_a_workspace_relative_observation() {
    let folder = tempfile::tempdir().unwrap();
    std::fs::write(folder.path().join("f.txt"), five_lines()).unwrap();

    let host = Arc::new(RecordingHost::default());
    let workspace = Workspace::new(folder.path(), host.clone()).unwrap();

    // Entry 1: the workspace tool's read operation.
    workspace.execute(Arguments::read("f.txt")).await.unwrap();
    let from_workspace = only_read(&host);
    assert_eq!(from_workspace.path, "f.txt");
    assert_eq!(from_workspace.kind, ReadRangeKind::Line);

    // Entry 2: the standalone read tool.
    let tool = ReadTool(workspace.clone());
    let value = tool
        .call(ReadArgs {
            path: "f.txt".into(),
            offset: Some(2),
            limit: Some(2),
            byte_offset: None,
            byte_limit: None,
            include_sha256: None,
        })
        .await
        .unwrap();
    assert!(value.get("error").is_none(), "read tool failed: {value}");
    let from_read_tool = only_read(&host);
    assert_eq!(from_read_tool.path, "f.txt");
    assert_eq!(from_read_tool.offset, Some(2));
    assert_eq!(from_read_tool.limit, Some(2));
    assert_content_free(&from_read_tool, folder.path(), &["line-2", "line-3"]);
}

#[tokio::test]
async fn real_dispatch_identity_is_copied_before_the_blocking_read() {
    let folder = tempfile::tempdir().unwrap();
    std::fs::write(folder.path().join("f.txt"), "one\ntwo\n").unwrap();

    let host = Arc::new(RecordingHost::default());
    let workspace = Workspace::new(folder.path(), host.clone()).unwrap();

    let run_id = Uuid::new_v4();
    let identity = ToolIdentity::new(run_id, "call-42", 3, 77);
    scope(Some(identity), async {
        workspace.execute(Arguments::read("f.txt")).await.unwrap();
    })
    .await;

    let observation = only_read(&host);
    assert_eq!(observation.run_id, Some(run_id));
    assert_eq!(observation.tool_call_id.as_deref(), Some("call-42"));
    assert_eq!(observation.turn_id, Some(3));
    assert_eq!(observation.effect_id, Some(77));
}

#[tokio::test]
async fn a_missing_identity_stays_absent_instead_of_being_fabricated() {
    let folder = tempfile::tempdir().unwrap();
    std::fs::write(folder.path().join("f.txt"), "one\ntwo\n").unwrap();

    let host = Arc::new(RecordingHost::default());
    let workspace = Workspace::new(folder.path(), host.clone()).unwrap();
    workspace.execute(Arguments::read("f.txt")).await.unwrap();

    let observation = only_read(&host);
    assert!(observation.run_id.is_none());
    assert!(observation.tool_call_id.is_none());
    assert!(observation.turn_id.is_none());
    assert!(observation.effect_id.is_none());
    assert!(observation.subagent_id.is_none());
    assert!(observation.parent_run_id.is_none());
}
