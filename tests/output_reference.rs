//! DES-12 structured model-visible output references.
//!
//! These tests drive the real capture path (`shell::execute` ->
//! `CaptureSession` -> versioned output index) through the real tool wrappers
//! (`ScopedTool` and `scoped_dynamic_tool`). No provider, credential or network
//! is involved: the tools run real local shell commands in a private tempdir.
//!
//! What is asserted:
//!
//! - a committed capture appends exactly one trailing
//!   `{"output_references": [...]}` JSON content block whose `path`,
//!   `total_bytes`, `stream`, `call_id` and `sha256` describe the file that is
//!   actually on disk, while every original block (text, image, JSON) is kept
//!   unchanged and in order;
//! - a small output adds no block and writes no `.mio` at all;
//! - a failed or cancelled capture publishes no reference and leaves no
//!   artifact;
//! - an error result that still owns a committed capture keeps its real
//!   reference and its error classification (it never becomes a success);
//! - two same-name calls running concurrently on the same task-local machinery
//!   keep their own references (no cross-talk);
//! - a nested collector scope never inherits the outer collector.

#![cfg(unix)]

use mio_agent::agent::ToolInvocation;
use mio_coding_agent::shell::{execute, output_store::OutputStore};
use mio_coding_agent::tool_identity::{
    self, OutputReference, OutputReferenceCollector, ScopedTool,
};
use mio_core::message::{ImageMediaType, ToolResultContent};
use mio_core::tool::{PortableTool, Tool, ToolContext, ToolErrorKind, ToolOutput};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    convert::Infallible,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use uuid::Uuid;

const PAYLOAD_BYTES: usize = 40_000;

/// A tool-call context carrying the engine's real dispatch identity, exactly
/// the way the bus attaches it for a real tool call.
fn context_with(tool_call_id: &str) -> ToolContext {
    let invocation = ToolInvocation {
        effect_id: 7,
        tool_call_id: tool_call_id.to_owned(),
        turn: 1,
    };
    ToolContext::new().with_scope(Arc::new(invocation))
}

/// A static tool that really runs a shell command and returns a deliberately
/// rich model result: a text block with the legacy marker, an image block and a
/// JSON block wrapping the real command output.
#[derive(Clone)]
struct Capturing {
    cwd: PathBuf,
    command: String,
}

impl PortableTool for Capturing {
    const NAME: &'static str = "capture-probe";
    type Args = Value;
    type Output = ToolOutput;
    type Error = Infallible;

    fn description(&self) -> String {
        "run a command and return rich output".into()
    }

    fn parameters(&self) -> Value {
        json!({"type": "object"})
    }

    async fn call(&self, _arguments: Value) -> Result<ToolOutput, Infallible> {
        let output = execute(&self.command, &self.cwd).await;
        let text = match output {
            Ok(text) | Err(text) => text,
        };
        Ok(ToolOutput::content(vec![
            ToolResultContent::text("legacy TRUNCATED marker"),
            ToolResultContent::image_base64("Zm9vYmFy", Some(ImageMediaType::PNG), None),
            ToolResultContent::json(json!({"output": text, "kind": "shell"})),
        ])
        .expect("the rich output is non-empty"))
    }
}

/// The same, but a failing shell command becomes a typed tool error so the
/// error path (committed capture, error result) can be exercised.
#[derive(Clone)]
struct CapturingStrict {
    cwd: PathBuf,
    command: String,
}

impl PortableTool for CapturingStrict {
    const NAME: &'static str = "capture-strict";
    type Args = Value;
    type Output = ToolOutput;
    type Error = std::io::Error;

    fn description(&self) -> String {
        "run a command, failing on a non-zero exit".into()
    }

    fn parameters(&self) -> Value {
        json!({"type": "object"})
    }

    async fn call(&self, _arguments: Value) -> Result<ToolOutput, std::io::Error> {
        match execute(&self.command, &self.cwd).await {
            Ok(text) => Ok(ToolOutput::text(text)),
            Err(error) => Err(std::io::Error::other(error)),
        }
    }
}

/// The trailing `{"output_references": [...]}` block of a wrapped tool result.
fn references_of(output: &ToolOutput) -> Vec<OutputReference> {
    let block = output
        .as_content()
        .last()
        .expect("a tool output always has at least one block");
    let value = block
        .as_json()
        .expect("the last block of a referenced result must be the JSON reference block");
    value["output_references"]
        .as_array()
        .expect("output_references must be an array")
        .iter()
        .map(|entry| serde_json::from_value(entry.clone()).expect("a typed reference"))
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Every artifact file under `.mio/outputs`.
fn artifact_files(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(root.join(".mio/outputs"))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_file())
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

/// A rich result from a committed capture keeps its original blocks and gains
/// exactly one trailing JSON reference block that matches the file on disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn committed_capture_appends_a_typed_reference_without_flattening_rich_output() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let run_id = Uuid::new_v4();
    let scoped = ScopedTool::new(
        Capturing {
            cwd: cwd.clone(),
            command: format!("head -c {PAYLOAD_BYTES} /dev/zero | tr '\\0' 'r'"),
        },
        run_id,
    );

    let mut context = context_with("call-rich");
    let output = <ScopedTool<Capturing> as Tool>::call(&scoped, &mut context, json!({}))
        .await
        .expect("the command must succeed");

    let blocks = output.as_content();
    assert_eq!(
        blocks.len(),
        4,
        "three original blocks plus one reference block"
    );
    assert_eq!(blocks[0].as_text(), Some("legacy TRUNCATED marker"));
    assert_eq!(
        blocks[1],
        ToolResultContent::image_base64("Zm9vYmFy", Some(ImageMediaType::PNG), None)
    );
    assert_eq!(
        blocks[2].as_json().and_then(|value| value.get("kind")),
        Some(&json!("shell"))
    );

    let references = references_of(&output);
    assert_eq!(references.len(), 1, "{references:?}");
    let reference = &references[0];
    assert_eq!(reference.call_id.as_deref(), Some("call-rich"));
    assert_eq!(reference.stream, "stdout");
    assert_eq!(reference.total_bytes, PAYLOAD_BYTES as u64);

    let artifact = cwd.join(&reference.path);
    let stored = std::fs::read(&artifact).unwrap();
    assert_eq!(stored.len(), PAYLOAD_BYTES);
    assert!(stored.iter().all(|byte| *byte == b'r'));
    assert_eq!(reference.sha256, sha256_hex(&stored));

    // The committed index agrees with the reference, byte for byte.
    let index = OutputStore::open(cwd.as_path())
        .unwrap()
        .load_index()
        .unwrap();
    assert_eq!(index.entries.len(), 1);
    assert_eq!(index.entries[0].path, reference.path);
    assert_eq!(index.entries[0].bytes, Some(reference.total_bytes));
    assert_eq!(
        index.entries[0].hash.as_deref(),
        Some(reference.sha256.as_str())
    );
    assert_eq!(index.entries[0].call_id.as_deref(), Some("call-rich"));
}

/// A small output changes nothing: no reference block, no `.mio` directory.
#[tokio::test]
async fn a_small_output_adds_no_reference_block_and_writes_no_mio() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let scoped = ScopedTool::new(
        Capturing {
            cwd: cwd.clone(),
            command: "echo small".to_string(),
        },
        Uuid::new_v4(),
    );

    let mut context = context_with("call-small");
    let output = <ScopedTool<Capturing> as Tool>::call(&scoped, &mut context, json!({}))
        .await
        .unwrap();

    assert_eq!(
        output.as_content().len(),
        3,
        "the original rich result must be returned unchanged"
    );
    assert!(
        output.as_content().iter().all(|block| block
            .as_json()
            .is_none_or(|value| value.get("output_references").is_none())),
        "no reference block may be appended for a small output"
    );
    assert!(
        !cwd.join(".mio").exists(),
        "a small output must not create .mio"
    );
}

/// A capture that fails before committing publishes no reference and leaves no
/// artifact, so a cancelled or failed call can never be mistaken for evidence.
#[tokio::test]
async fn a_failed_capture_publishes_no_reference() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let collector = OutputReferenceCollector::new();
    let published = collector.clone();
    let task_cwd = cwd.clone();

    let result = tool_identity::scope_output_references(collector, async move {
        // Larger than the 8 MiB complete-output bound: the capture must fail.
        execute("head -c 8388609 /dev/zero", &task_cwd).await
    })
    .await;

    assert!(
        result.is_err(),
        "the oversized capture must fail: {result:?}"
    );
    assert!(
        published.take().is_empty(),
        "a failed capture must not publish a reference"
    );
    assert_eq!(artifact_files(&cwd).len(), 0);
    let index = OutputStore::open(cwd.as_path())
        .unwrap()
        .load_index()
        .unwrap();
    assert!(index.entries.is_empty(), "{index:?}");
}

/// A capture cancelled while spilling rolls back and publishes no reference.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_capture_publishes_no_reference() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let collector = OutputReferenceCollector::new();
    let published = collector.clone();
    let task_cwd = cwd.clone();

    let task = tokio::spawn(async move {
        tool_identity::scope_output_references(collector, async move {
            // Spills past 32 KiB, then blocks so the test can cancel it.
            execute("head -c 200000 /dev/zero; sleep 30", &task_cwd).await
        })
        .await
    });

    let outputs = cwd.join(".mio/outputs");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if std::fs::read_dir(&outputs)
                .map(|mut entries| entries.next().is_some())
                .unwrap_or(false)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the spill artifact was never created");

    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());

    assert!(
        published.take().is_empty(),
        "a cancelled capture must not publish a reference"
    );
    assert_eq!(artifact_files(&cwd).len(), 0);
    let index = OutputStore::open(cwd.as_path())
        .unwrap()
        .load_index()
        .unwrap();
    assert!(index.entries.is_empty(), "{index:?}");
}

/// A tool error that still owns a committed capture keeps the real reference,
/// and stays an error: classification, source and failure status survive.
#[tokio::test]
async fn an_error_result_keeps_its_committed_reference_and_stays_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let scoped = ScopedTool::new(
        CapturingStrict {
            cwd: cwd.clone(),
            command: format!("head -c {PAYLOAD_BYTES} /dev/zero | tr '\\0' 'x'; exit 3"),
        },
        Uuid::new_v4(),
    );

    let mut context = context_with("call-error");
    let error = <ScopedTool<CapturingStrict> as Tool>::call(&scoped, &mut context, json!({}))
        .await
        .expect_err("a non-zero exit must stay an error");

    // The classification is the wrapped tool's real normalized error, and the
    // original source is still downcastable.
    assert_eq!(error.kind(), ToolErrorKind::Other);
    assert!(error.is::<std::io::Error>());

    let model_output = error.model_output();
    assert_eq!(
        model_output.as_content().len(),
        2,
        "the error model output gains exactly the reference block"
    );
    let references = references_of(model_output);
    assert_eq!(references.len(), 1, "{references:?}");
    let reference = &references[0];
    assert_eq!(reference.call_id.as_deref(), Some("call-error"));
    assert_eq!(reference.total_bytes, PAYLOAD_BYTES as u64);

    let artifact = cwd.join(&reference.path);
    let stored = std::fs::read(&artifact).unwrap();
    assert!(stored.iter().all(|byte| *byte == b'x'));
    assert_eq!(reference.sha256, sha256_hex(&stored));
}

/// Two same-name calls running concurrently keep their own references: each
/// result points at its own artifact and its own call id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_same_name_calls_keep_their_own_references() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let run_id = Uuid::new_v4();

    let spawn = |tag: &'static str, call_id: &'static str| {
        let cwd = cwd.clone();
        let scoped = ScopedTool::new(
            Capturing {
                cwd,
                command: format!("head -c {PAYLOAD_BYTES} /dev/zero | tr '\\0' '{tag}'"),
            },
            run_id,
        );
        tokio::spawn(async move {
            let mut context = context_with(call_id);
            let output = <ScopedTool<Capturing> as Tool>::call(&scoped, &mut context, json!({}))
                .await
                .unwrap();
            (call_id, output)
        })
    };

    let first = spawn("A", "call-a");
    let second = spawn("B", "call-b");
    let (_, first_output) = first.await.unwrap();
    let (_, second_output) = second.await.unwrap();

    let first_references = references_of(&first_output);
    let second_references = references_of(&second_output);
    assert_eq!(first_references.len(), 1, "{first_references:?}");
    assert_eq!(second_references.len(), 1, "{second_references:?}");
    assert_eq!(first_references[0].call_id.as_deref(), Some("call-a"));
    assert_eq!(second_references[0].call_id.as_deref(), Some("call-b"));
    assert_ne!(first_references[0].path, second_references[0].path);

    let first_bytes = std::fs::read(cwd.join(&first_references[0].path)).unwrap();
    let second_bytes = std::fs::read(cwd.join(&second_references[0].path)).unwrap();
    assert!(first_bytes.iter().all(|byte| *byte == b'A'));
    assert!(second_bytes.iter().all(|byte| *byte == b'B'));
    assert_eq!(first_references[0].sha256, sha256_hex(&first_bytes));
    assert_eq!(second_references[0].sha256, sha256_hex(&second_bytes));

    let index = OutputStore::open(cwd.as_path())
        .unwrap()
        .load_index()
        .unwrap();
    assert_eq!(
        index.entries.len(),
        2,
        "each concurrent capture is indexed once"
    );
}

/// A nested collector scope masks the outer one instead of inheriting it.
#[tokio::test]
async fn a_nested_scope_does_not_inherit_the_outer_collector() {
    let synthetic = || OutputReference {
        path: ".mio/outputs/outer.stdout".into(),
        total_bytes: 1,
        stream: "stdout".into(),
        call_id: Some("call-outer".into()),
        sha256: "a".repeat(64),
    };

    let outer = OutputReferenceCollector::new();
    let outer_published = outer.clone();
    tool_identity::scope_output_references(outer, async {
        outer_published
            .try_publish(vec![synthetic()])
            .expect("a bounded synthetic reference publishes");
        assert_eq!(outer_published.len(), 1);

        let nested = OutputReferenceCollector::new();
        tool_identity::scope_output_references(nested.clone(), async {
            let current = tool_identity::current_output_reference_collector()
                .expect("a nested scope still binds a collector");
            assert!(
                current.is_empty(),
                "a nested scope must not inherit the outer collector"
            );
            assert_eq!(current.len(), 0);
        })
        .await;

        // The outer collector is untouched by the nested scope.
        assert_eq!(outer_published.len(), 1);
        assert_eq!(outer_published.take(), vec![synthetic()]);
    })
    .await;
}
