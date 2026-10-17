//! Lifecycle evidence binds approved parameters without copying their contents.
use mio_coding_agent::{
    AgentEvent, Host, HostFuture, ToolFilter,
    policy::{ApprovalBinding, PolicyContext, ToolPolicy},
    tool_lifecycle::ToolCallRecord,
};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct EvidenceHost {
    records: Mutex<Vec<Value>>,
    approved_digest: Mutex<Option<String>>,
    approved_target: Mutex<Option<String>>,
}

impl Host for EvidenceHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { panic!("this admission test must not execute shell") })
    }
    fn on_event(&self, _: AgentEvent) {}
    fn on_tool_call_lifecycle(&self, call: &ToolCallRecord) -> Result<(), String> {
        self.records
            .lock()
            .unwrap()
            .push(serde_json::to_value(call).unwrap());
        Ok(())
    }
    fn approve_tool_binding(
        &self,
        _: &str,
        target: &str,
        binding: &ApprovalBinding,
    ) -> HostFuture<Result<(), String>> {
        *self.approved_digest.lock().unwrap() = Some(binding.params_sha256.clone());
        *self.approved_target.lock().unwrap() = Some(target.to_owned());
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn canonical_evidence_keeps_the_binding_digest_and_omits_raw_arguments() {
    let host = Arc::new(EvidenceHost::default());
    let policy = ToolPolicy::new(
        ToolFilter::all(),
        false,
        host.clone(),
        PolicyContext::new("/workspace", uuid::Uuid::new_v4().to_string()),
    );
    // These are synthetic canaries, never credentials from the environment.
    let args = json!({
        "api_key":"sk-SYNTHETIC-PRIVATE-CANARY",
        "document":"PRIVATE-DOCUMENT-BODY-CANARY",
    })
    .to_string();
    policy
        .admit_with_call_id("mcp__probe__send", &args, Some("call-one"))
        .await
        .unwrap();
    let records = host.records.lock().unwrap();
    assert!(!records.is_empty());
    let serialized = serde_json::to_string(&*records).unwrap();
    assert!(!serialized.contains("sk-SYNTHETIC-PRIVATE-CANARY"));
    assert!(!serialized.contains("PRIVATE-DOCUMENT-BODY-CANARY"));
    let expected = host.approved_digest.lock().unwrap().clone().unwrap();
    assert!(
        records.iter().any(|record| {
            record["phase"] == "normalized"
                && record["params_sha256"].as_str() == Some(expected.as_str())
        }),
        "normalization must preserve the exact digest handed to approval"
    );
}

#[tokio::test]
async fn canonical_shell_evidence_omits_the_command_but_approval_keeps_it() {
    let host = Arc::new(EvidenceHost::default());
    let policy = ToolPolicy::new(
        ToolFilter::all(),
        false,
        host.clone(),
        PolicyContext::new("/workspace", uuid::Uuid::new_v4().to_string()),
    );
    // A synthetic canary, never a credential from the environment.
    let command = "printf MIO-SYNTHETIC-SHELL-CANARY-2f1c > out.txt";
    let args = json!({"operation": "shell", "value": command}).to_string();
    policy
        .admit_with_call_id("workspace", &args, Some("call-shell"))
        .await
        .unwrap();
    let records = host.records.lock().unwrap();
    let serialized = serde_json::to_string(&*records).unwrap();
    assert!(
        !serialized.contains("MIO-SYNTHETIC-SHELL-CANARY-2f1c"),
        "a shell command must never be copied into canonical evidence: {serialized}"
    );
    let record = records
        .iter()
        .find(|record| record["phase"] == "normalized" && record["operation"] == "shell")
        .expect("a normalized shell record");
    assert_eq!(record["target_redacted"], serde_json::json!(true));
    assert!(
        !record["target"]
            .as_str()
            .unwrap_or_default()
            .contains("MIO-SYNTHETIC"),
        "the canonical target must stay a redacted summary: {record}"
    );
    // The exact command digest is preserved unchanged, byte for byte.
    let expected = host.approved_digest.lock().unwrap().clone().unwrap();
    assert_eq!(record["params_sha256"].as_str(), Some(expected.as_str()));
    // The real command still reaches the approval UI, so operator visibility is
    // unchanged; only durable evidence is redacted.
    assert_eq!(
        host.approved_target.lock().unwrap().as_deref(),
        Some(command)
    );
}
