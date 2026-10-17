use mio_coding_agent::{AgentEvent, Arguments, Host, HostFuture, Workspace};
use mio_core::tool::ToolOutput;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Arc};

struct NoShell;
impl Host for NoShell {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { panic!("byte paging must not invoke shell") })
    }
    fn on_event(&self, _: AgentEvent) {}
}

async fn first_page(text: &str, budget: usize) -> (String, Value) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("data.txt"), text).unwrap();
    let workspace = Workspace::new(root.path(), Arc::new(NoShell))
        .unwrap()
        .with_read_output_limit_bytes(budget);
    let mut args = Arguments::read("data.txt");
    args.byte_offset = Some(0);
    args.byte_limit = Some(16384);
    let result = workspace.execute(args).await.unwrap();
    let page = serde_json::from_str(&result).unwrap();
    (result, page)
}

#[tokio::test]
async fn plain_text_uses_available_byte_budget_without_worst_case_escape_penalty() {
    let budget = 2048;
    let (result, page) = first_page(&"a".repeat(10000), budget).await;
    let text = page["text"].as_str().unwrap();
    assert!(
        text.len() >= 1024,
        "ordinary text must use most of the available budget, got {} bytes",
        text.len()
    );
    let model_output = ToolOutput::json(json!({"output": result}));
    assert!(serde_json::to_vec(&model_output).unwrap().len() <= budget);
    assert_eq!(page["next_byte_offset"], text.len());
}

#[tokio::test]
async fn escaped_text_and_utf8_fit_the_actual_model_output_budget() {
    for text in ["\0\n\t\\\"".repeat(3000), "界🌊".repeat(3000)] {
        for budget in [256, 2048] {
            let (result, page) = first_page(&text, budget).await;
            let returned = page["text"].as_str().unwrap();
            assert!(!returned.is_empty());
            assert!(text.starts_with(returned));
            assert_eq!(page["next_byte_offset"], returned.len());
            assert!(
                serde_json::to_vec(&ToolOutput::json(json!({"output": result})))
                    .unwrap()
                    .len()
                    <= budget
            );
        }
    }
}

#[tokio::test]
async fn byte_page_hash_covers_the_whole_file_within_the_same_output_budget() {
    let root = tempfile::tempdir().unwrap();
    let body = "界🌊".repeat(500);
    std::fs::write(root.path().join("data.txt"), &body).unwrap();
    let workspace = Workspace::new(root.path(), Arc::new(NoShell))
        .unwrap()
        .with_read_output_limit_bytes(256);
    let mut args = Arguments::read("data.txt");
    args.byte_offset = Some(0);
    args.byte_limit = Some(1024);
    args.include_sha256 = Some(true);
    let output = workspace.execute(args).await.unwrap();
    let page: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(
        page["sha256"],
        format!("{:x}", Sha256::digest(body.as_bytes()))
    );
    assert!(!page["text"].as_str().unwrap().is_empty());
    assert!(page["next_byte_offset"].as_u64().unwrap() < body.len() as u64);
    assert!(
        serde_json::to_vec(&ToolOutput::json(json!({"output": output})))
            .unwrap()
            .len()
            <= 256
    );
}
