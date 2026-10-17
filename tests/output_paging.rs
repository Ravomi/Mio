//! Long tool output remains recoverable with read-only file tools and no shell.
use mio_coding_agent::{AgentEvent, Arguments, Host, HostFuture, Workspace};
use serde_json::Value;
use std::{path::Path, sync::Arc};

struct NoShell;
impl Host for NoShell {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { panic!("paging must not require shell execution") })
    }
    fn on_event(&self, _: AgentEvent) {}
}

#[tokio::test]
async fn read_only_tools_reconstruct_a_large_single_line_artifact() {
    let root = tempfile::tempdir().unwrap();
    let original = "完整输出海浪🌊".repeat(20_000);
    let preview = mio_coding_agent::shell::externalize_output(&original, root.path())
        .await
        .unwrap();
    assert!(preview.contains("complete output"));
    let artifact = std::fs::read_dir(root.path().join(".mio/outputs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let relative = artifact
        .strip_prefix(root.path())
        .unwrap()
        .to_str()
        .unwrap();
    let workspace = Workspace::new_read_only(root.path(), Arc::new(NoShell)).unwrap();
    let mut offset = 0;
    let mut reconstructed = String::new();
    loop {
        let mut args = Arguments::read(relative);
        args.byte_offset = Some(offset);
        args.byte_limit = Some(8192);
        let output = workspace.execute(args).await.unwrap();
        let page: Value = serde_json::from_str(&output).unwrap();
        reconstructed.push_str(page["text"].as_str().unwrap());
        match page["next_byte_offset"].as_u64() {
            Some(next) => {
                assert!(next > offset);
                offset = next;
            }
            None => break,
        }
    }
    assert_eq!(reconstructed, original);
    assert_eq!(std::fs::read(&artifact).unwrap(), original.as_bytes());
}

#[tokio::test]
async fn paging_modes_cannot_be_combined_or_escape_the_workspace() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("text"), "你好世界").unwrap();
    let workspace = Workspace::new_read_only(root.path(), Arc::new(NoShell)).unwrap();
    let mut args = Arguments::read_paged("text", 1, 1);
    args.byte_offset = Some(0);
    assert!(
        workspace
            .execute(args)
            .await
            .unwrap_err()
            .contains("cannot be combined")
    );
    let mut args = Arguments::read("../outside");
    args.byte_offset = Some(0);
    assert!(workspace.execute(args).await.is_err());
}
