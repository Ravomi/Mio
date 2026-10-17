//! Capture appearing after run construction must still prevent offline cleanup.
//! This is independent of journal presence: SDK and ephemeral runs can retain
//! output references in memory without writing a session file.
#![cfg(unix)]

use mio_coding_agent::shell::{
    externalize_output,
    output_store::{OutputStore, OutputStoreError},
};
use mio_coding_agent::{AgentEvent, Host, HostFuture, Workspace};
use std::{
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

struct QuietHost;
impl Host for QuietHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }
    fn on_event(&self, _: AgentEvent) {}
}

/// Simulate elapsed retention time in this test's private, real capture index.
/// A no-op preview cannot prove protection: confirmation must face an actual
/// eligible artifact and be refused while its Workspace still holds the run.
fn age_capture(store: &OutputStore) -> std::path::PathBuf {
    let mut index = store.load_index().unwrap();
    assert_eq!(index.entries.len(), 1);
    index.entries[0].created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - mio_coding_agent::shell::output_store::DEFAULT_RETENTION_SECS
        - 60;
    let artifact = store.root().join(&index.entries[0].path);
    std::fs::write(store.index_path(), serde_json::to_vec(&index).unwrap()).unwrap();
    artifact
}

#[cfg(unix)]
#[tokio::test]
async fn first_capture_during_a_live_workspace_also_excludes_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(root.path(), Arc::new(QuietHost)).unwrap();
    assert!(!root.path().join(".mio").exists());
    externalize_output(&"x".repeat(40 * 1024), root.path())
        .await
        .unwrap();
    let store = OutputStore::open(root.path().to_path_buf()).unwrap();
    let artifact = age_capture(&store);
    let plan = store.plan_cleanup(SystemTime::now()).unwrap();
    assert_eq!(plan.candidates.len(), 1);
    let attempted = store.confirm_cleanup(&plan, SystemTime::now());
    assert!(
        matches!(attempted, Err(OutputStoreError::Busy(_))),
        "a first output must not leave its active workspace unprotected: {attempted:?}"
    );
    assert!(artifact.exists());
    drop(workspace);
    let report = store.confirm_cleanup(&plan, SystemTime::now()).unwrap();
    assert_eq!(report.removed.len(), 1);
    assert!(!artifact.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn an_existing_output_is_protected_by_a_live_workspace() {
    let root = tempfile::tempdir().unwrap();
    externalize_output(&"x".repeat(40 * 1024), root.path())
        .await
        .unwrap();
    let store = OutputStore::open(root.path().to_path_buf()).unwrap();
    let workspace = Workspace::new(root.path(), Arc::new(QuietHost)).unwrap();
    let artifact = age_capture(&store);
    let plan = store.plan_cleanup(SystemTime::now()).unwrap();
    assert_eq!(plan.candidates.len(), 1);
    assert!(matches!(
        store.confirm_cleanup(&plan, SystemTime::now()),
        Err(OutputStoreError::Busy(_))
    ));
    assert!(artifact.exists());
    drop(workspace);
    let report = store.confirm_cleanup(&plan, SystemTime::now()).unwrap();
    assert_eq!(report.removed.len(), 1);
    assert!(!artifact.exists());
}
