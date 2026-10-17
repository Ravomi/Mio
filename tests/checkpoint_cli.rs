//! Offline checkpoint maintenance must not require model credentials.
use mio_coding_agent::{
    AgentEvent, Arguments, Host, HostFuture, Workspace, checkpoint::CheckpointStore,
};
use serde_json::Value;
use std::{path::Path, process::Command, sync::Arc};

struct FileHost;
impl Host for FileHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Err("shell is not used".into()) })
    }
    fn on_event(&self, _: AgentEvent) {}
}

fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mio"));
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.starts_with("MIO_") || name.contains("API_KEY") {
            command.env_remove(key);
        }
    }
    command
        .env("HOME", root)
        .env("MIO_CONFIG_DIR", root.join("config"))
        .env("MIO_PROVIDER", "no-provider-may-be-resolved")
        .args([
            "--no-context",
            "--no-mcp",
            "--no-shared-limits",
            "--json",
            "--cwd",
        ])
        .arg(root);
    command
}

#[tokio::test]
async fn maintenance_lists_rewinds_and_protects_later_user_edits() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    std::fs::write(root.join("file.txt"), "original").unwrap();
    let store = Arc::new(CheckpointStore::open(root).unwrap());
    let workspace = Workspace::new(root, Arc::new(FileHost))
        .unwrap()
        .with_checkpoints(store.clone());
    workspace
        .execute(Arguments::write("file.txt", "changed"))
        .await
        .unwrap();
    let id = store.list().unwrap()[0].id.clone();
    let listing = command(root).arg("--list-checkpoints").output().unwrap();
    assert!(
        listing.status.success(),
        "{}",
        String::from_utf8_lossy(&listing.stdout)
    );
    let event: Value = serde_json::from_slice(&listing.stdout).unwrap();
    assert_eq!(event["type"], "checkpoints");
    assert_eq!(event["data"][0]["id"], id);

    std::fs::write(root.join("file.txt"), "later user edit").unwrap();
    let refused = command(root).args(["--rewind", &id]).output().unwrap();
    assert!(!refused.status.success());
    assert_eq!(
        std::fs::read_to_string(root.join("file.txt")).unwrap(),
        "later user edit"
    );

    std::fs::write(root.join("file.txt"), "changed").unwrap();
    let restored = command(root).args(["--rewind", &id]).output().unwrap();
    assert!(
        restored.status.success(),
        "{}",
        String::from_utf8_lossy(&restored.stdout)
    );
    assert_eq!(
        std::fs::read_to_string(root.join("file.txt")).unwrap(),
        "original"
    );
    let event: Value = serde_json::from_slice(&restored.stdout).unwrap();
    assert_eq!(event["type"], "rewound");
    assert!(!root.join(".git").exists());
}

#[test]
fn checkpoints_cannot_be_enabled_without_persistence() {
    let directory = tempfile::tempdir().unwrap();
    let output = command(directory.path())
        .args(["--checkpoints", "--no-session", "-p", "unused"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("require persistence"));
}
