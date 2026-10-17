//! DES-17 production capture integration.
//!
//! These tests drive the real shell capture path (`shell::externalize_output`,
//! `shell::execute`, `shell::run_supervised`) instead of the store API directly,
//! so they prove the wiring, not just the store:
//!
//! - a complete output is registered in the versioned index and the original
//!   bytes can be read back verbatim;
//! - a small output never creates `.mio` (lazy lease + lazy output directory);
//! - when one stream fails, the files and index entries created for the sibling
//!   stream are rolled back and pre-existing outputs are untouched;
//! - two large streams each keep their own index entry (the synchronous index
//!   read-modify-write does not drop the other stream's entry).

use mio_coding_agent::shell::externalize_output;
use mio_coding_agent::shell::output_store::OutputStore;
#[cfg(unix)]
use mio_coding_agent::shell::{execute, run_supervised};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Real artifact files under `.mio/outputs`. The index and lock files live in
/// `.mio/output-index`, so this only ever returns products.
fn artifact_files(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(root.join(".mio/outputs"))
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

#[tokio::test]
async fn a_large_externalized_output_is_registered_with_its_full_content() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let original = "完整输出🌊".repeat(20_000);
    let preview = externalize_output(&original, &root).await.unwrap();
    assert!(preview.contains("[TRUNCATED: inline output exceeds 8 KiB"));
    assert!(preview.contains("complete output: .mio/outputs/"));
    assert!(
        preview.len() < 3 * 1024,
        "external preview must leave room for the next model request"
    );

    let files = artifact_files(&root);
    assert_eq!(files.len(), 1, "one complete output artifact");
    let bytes = fs::read(&files[0]).unwrap();
    assert_eq!(
        bytes,
        original.as_bytes(),
        "the stored bytes are the original"
    );
    let name = files[0].file_name().unwrap().to_string_lossy().to_string();
    assert!(preview.contains(&name));
    assert!(preview.contains(&format!("{} bytes]", bytes.len())));
    assert_eq!(files[0].extension().and_then(|e| e.to_str()), Some("tool"));

    let index = OutputStore::open(root.as_path())
        .unwrap()
        .load_index()
        .unwrap();
    assert_eq!(index.entries.len(), 1);
    let entry = &index.entries[0];
    assert_eq!(entry.path, format!(".mio/outputs/{name}"));
    assert_eq!(entry.bytes, Some(bytes.len() as u64));
    assert!(
        entry.hash.is_some(),
        "the index records a real content hash"
    );
    assert_eq!(entry.stream, "tool");
    assert_eq!(entry.call_id, None, "an unknown call id stays null");
    assert!(entry.created > 0);
}

#[tokio::test]
async fn a_small_externalized_output_creates_no_mio_directory() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    assert_eq!(externalize_output("small", &root).await.unwrap(), "small");
    assert!(
        !root.join(".mio").exists(),
        "an inline output must not create the product directory"
    );
}

#[tokio::test]
async fn inline_boundary_preserves_complete_output_on_both_sides() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let inline = "x".repeat(8 * 1024);
    assert_eq!(externalize_output(&inline, &root).await.unwrap(), inline);
    assert!(!root.join(".mio").exists());

    let full = "y".repeat(8 * 1024 + 1);
    let preview = externalize_output(&full, &root).await.unwrap();
    assert!(preview.contains("[TRUNCATED: inline output exceeds 8 KiB"));
    let files = artifact_files(&root);
    assert_eq!(files.len(), 1);
    assert_eq!(fs::read(&files[0]).unwrap(), full.as_bytes());
}

#[cfg(unix)]
#[tokio::test]
async fn a_failed_stream_rolls_back_new_files_and_keeps_existing_outputs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let kept = "kept".repeat(20_000);
    let existing = externalize_output(&kept, &root).await.unwrap();
    assert!(existing.contains(".mio/outputs/"));
    let before_files = artifact_files(&root);
    assert_eq!(
        before_files.len(),
        1,
        "one committed artifact before the failure"
    );
    let before_index = OutputStore::open(root.as_path())
        .unwrap()
        .load_index()
        .unwrap();
    assert_eq!(before_index.entries.len(), 1);

    // stderr fills a complete output and registers it; stdout then exceeds the
    // 8 MiB hard limit, so the whole supervised command fails.
    let error = execute(
        "head -c 100000 /dev/zero >&2; exec 2>&-; sleep 0.1; head -c 8388609 /dev/zero",
        &root,
    )
    .await
    .unwrap_err();
    assert!(error.contains("limit"), "unexpected error: {error}");

    assert_eq!(
        artifact_files(&root),
        before_files,
        "the failed stream must not leave a new artifact"
    );
    let after_index = OutputStore::open(root.as_path())
        .unwrap()
        .load_index()
        .unwrap();
    assert_eq!(
        after_index, before_index,
        "the failed stream must roll back its index entry"
    );
    assert!(fs::read(&before_files[0]).unwrap().len() > 32 * 1024);
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_after_one_stream_is_published_rolls_back_only_new_outputs() {
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    externalize_output(&"original".repeat(6000), &root)
        .await
        .unwrap();
    let store = OutputStore::open(root.as_path()).unwrap();
    let before_index = store.load_index().unwrap();
    let before_files = artifact_files(&root);
    let original = fs::read(&before_files[0]).unwrap();
    let task_root = root.clone();
    let task = tokio::spawn(async move {
        execute(
            "head -c 40000 /dev/zero >&2; exec 2>&-; printf ready > capture-ready; while :; do sleep 1; done",
            &task_root,
        ).await
    });
    let published = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let index = store.load_index().unwrap();
            if index.entries.len() == before_index.entries.len() + 1
                && index.entries.iter().any(|entry| entry.stream == "stderr")
                && fs::read(root.join("capture-ready"))
                    .is_ok_and(|bytes| bytes.as_slice() == b"ready")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    // Always cancel before asserting, including when publication timed out.
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    published.expect("stderr was not published while stdout remained open");
    assert_eq!(store.load_index().unwrap(), before_index);
    assert_eq!(artifact_files(&root), before_files);
    assert_eq!(fs::read(&before_files[0]).unwrap(), original);
    // Cancellation does not undo the command's completed file side effects.
    assert_eq!(fs::read(root.join("capture-ready")).unwrap(), b"ready");
}

#[cfg(unix)]
#[tokio::test]
async fn both_streams_register_independent_index_entries() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut command = tokio::process::Command::new("sh");
    command
        .current_dir(&root)
        .stdin(std::process::Stdio::null())
        .args([
            "-c",
            "head -c 40000 /dev/zero | tr '\\0' 'a'; head -c 40000 /dev/zero | tr '\\0' 'b' >&2",
        ]);
    let output = run_supervised(&mut command).await.unwrap();
    assert!(output.status.success());
    assert!(output.stdout.contains("TRUNCATED"));
    assert!(output.stderr.contains("TRUNCATED"));

    let index = OutputStore::open(root.as_path())
        .unwrap()
        .load_index()
        .unwrap();
    assert_eq!(index.entries.len(), 2, "each stream keeps its own entry");
    let mut streams: Vec<&str> = index
        .entries
        .iter()
        .map(|entry| entry.stream.as_str())
        .collect();
    streams.sort();
    assert_eq!(streams, vec!["stderr", "stdout"]);
    for entry in &index.entries {
        let bytes = fs::read(root.join(&entry.path)).unwrap();
        assert_eq!(Some(bytes.len() as u64), entry.bytes);
        let expected = if entry.stream == "stdout" { b'a' } else { b'b' };
        assert!(bytes.iter().all(|byte| *byte == expected));
    }
}
