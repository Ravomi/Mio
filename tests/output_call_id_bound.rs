//! Regression for the provider tool-call id bound of the externalized output
//! path.
//!
//! `ToolPolicy` accepts a provider tool-call id of at most 512 UTF-8 bytes and
//! a real 512-ASCII-byte id then flows through the batch output budget into the
//! durable output index, the typed `OutputReference`, the canonical journal
//! record and the publication intent ledger. `OutputStore` used to cap the same
//! field at 256 characters, so a legal 512-byte id was late-rejected (or, worse,
//! could have been aliased by a shorter cap). These tests pin the fixed bound:
//!
//! - a real loopback engine run with a legal 512-byte provider id externalizes
//!   its oversized tool result and the full id survives every persisted
//!   artifact byte for byte, with matching SHA-256 and length;
//! - two ids that share 511 bytes and differ only in the last byte stay
//!   distinct in the index and in the ledger (no truncation/aliasing), and both
//!   files survive an explicit reconciliation;
//! - the policy bound itself is unchanged: 512 bytes is admitted, 513 bytes is
//!   still refused by `ToolPolicy` (the regression is not greened by widening
//!   the policy limit).
#![cfg(unix)]

#[path = "../src/test_http.rs"]
mod test_http;

use mio_coding_agent::{
    AgentEvent, Config, Host, HostFuture, Provider, RunOptions, Services, ToolFilter,
    journal::{Journal, RunState, journal_path},
    policy::{PolicyContext, ToolPolicy},
    run_with_options,
    shell::output_store::{
        OutputEntry, OutputReference, OutputStore, PublicationIntentState, PublicationResolution,
    },
    tool_lifecycle::ToolCallRecord,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    io::Write,
    net::TcpListener,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

/// A provider id that is exactly 512 ASCII bytes, the largest one policy admits.
fn legal_provider_id() -> String {
    let mut id = String::from("mio512-");
    while id.len() < 512 {
        id.push('a');
    }
    id
}

fn body_sized(index: usize, size: usize) -> String {
    let mut body = format!("CALLID-{index}-payload-");
    while body.len() < size {
        body.push('x');
    }
    body.truncate(size);
    body
}

// --- loopback provider helpers -------------------------------------------

fn chunk(delta: Value, finish: Value) -> String {
    json!({
        "id": "test",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "local-test",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
    })
    .to_string()
}

fn sse(events: Vec<String>) -> String {
    let mut body = String::new();
    for event in events {
        body.push_str("data: ");
        body.push_str(&event);
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    body
}

/// One `bash` tool call carrying the provider's own, exact `id`.
fn tool_turn(id: &str, size: usize) -> String {
    let call = json!({
        "tool_calls": [{
            "index": 0,
            "id": id,
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": json!({"command": format!("emit 5 0 {size}")}).to_string()
            }
        }]
    });
    sse(vec![
        chunk(call, Value::Null),
        chunk(json!({}), json!("tool_calls")),
    ])
}

fn final_turn() -> String {
    sse(vec![
        chunk(json!({"content": "done"}), Value::Null),
        chunk(json!({}), json!("stop")),
    ])
}

/// Serve `responses` in order on a loopback endpoint; extra connections get a
/// bounded wait so a run that stops after one request does not hang.
fn spawn_server(responses: Vec<String>) -> (String, std::thread::JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let mut requests = Vec::new();
        let mut served = 0usize;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while served < responses.len() && std::time::Instant::now() < deadline {
            match listener.accept() {
                Ok((mut socket, _)) => {
                    socket
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    let (header, request) = test_http::read_json(&mut socket);
                    assert!(header.starts_with("POST /v1/chat/completions "));
                    requests.push(request);
                    let body = responses[served].clone();
                    write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                    served += 1;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept failed: {error}"),
            }
        }
        requests
    });
    (format!("http://{address}/v1"), handle)
}

fn config(base_url: String) -> Config {
    Config {
        provider: Provider::Custom,
        base_url,
        model: "local-test".into(),
        api_key: "test-only".into(),
    }
}

fn options() -> RunOptions {
    RunOptions::default()
}

/// Persist every canonical record to a real journal, then fail the supplement
/// after writing it: the durable journal reference and the `PendingUncertain`
/// publication intent are both observable after the run stops.
struct PersistThenFailHost {
    journal: Arc<Journal>,
    records: Arc<Mutex<Vec<ToolCallRecord>>>,
}

impl Host for PersistThenFailHost {
    fn shell(&self, command: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        let command = command.to_owned();
        Box::pin(async move {
            let parts: Vec<&str> = command.split_whitespace().collect();
            if parts.len() == 4 && parts[0] == "emit" {
                let delay: u64 = parts[1].parse().map_err(|_| "bad delay")?;
                let index: usize = parts[2].parse().map_err(|_| "bad index")?;
                let size: usize = parts[3].parse().map_err(|_| "bad size")?;
                tokio::time::sleep(Duration::from_millis(delay)).await;
                return Ok(body_sized(index, size));
            }
            Ok(String::new())
        })
    }

    fn on_event(&self, _event: AgentEvent) {}

    fn on_tool_call_lifecycle(&self, call: &ToolCallRecord) -> Result<(), String> {
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(call.clone());
        self.journal.record_canonical_phase(call)?;
        if call.output_references.is_empty() {
            Ok(())
        } else {
            Err("call-id-bound host failed after writing the journal".into())
        }
    }
}

fn assert_reference_bytes(root: &Path, reference: &OutputReference) {
    let bytes = std::fs::read(root.join(&reference.path)).unwrap();
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    assert_eq!(reference.sha256, format!("{:x}", hasher.finalize()));
    assert_eq!(reference.total_bytes, bytes.len() as u64);
}

/// A real loopback run with the largest legal provider id: the oversized tool
/// result is externalized, and the exact 512-byte id reaches the index, the
/// typed reference, the canonical journal on disk and the publication ledger
/// without truncation.
#[tokio::test]
async fn legal_512_byte_provider_id_survives_the_published_output_path() {
    let id = legal_provider_id();
    assert_eq!(id.len(), 512);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let host = Arc::new(PersistThenFailHost {
        journal: Arc::new(Journal::open(&root, "call-id-bound")),
        records: Arc::new(Mutex::new(Vec::new())),
    });
    host.journal.begin_run().unwrap();
    let (base_url, server) = spawn_server(vec![tool_turn(&id, 60_000), final_turn()]);

    let error = run_with_options(
        config(base_url),
        Vec::new(),
        "externalize the batch",
        &root,
        host.clone(),
        Services::default(),
        options(),
    )
    .await
    .expect_err("the failing Host stops the run after externalizing");
    assert!(error.contains("may have partially persisted"), "{error}");
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 1, "no provider request after the failure");
    host.journal.finish_run(RunState::Completed).unwrap();

    let store = OutputStore::open(&root).unwrap();
    let index = store.load_index().unwrap();
    assert_eq!(index.entries.len(), 1, "{:?}", index.entries);
    assert_eq!(
        index.entries[0].call_id.as_deref(),
        Some(id.as_str()),
        "the durable index stores the full provider id"
    );
    assert_eq!(index.entries[0].call_id.as_deref().unwrap().len(), 512);

    let intents = store.publication_intents().unwrap();
    assert_eq!(intents.len(), 1, "{intents:?}");
    assert_eq!(intents[0].state, PublicationIntentState::PendingUncertain);
    assert_eq!(intents[0].call_id.as_deref(), Some(id.as_str()));
    let intent_reference = intents[0].references[0].clone();
    assert_eq!(intent_reference.call_id.as_deref(), Some(id.as_str()));
    assert_eq!(intent_reference.call_id.as_deref().unwrap().len(), 512);
    assert_reference_bytes(&root, &intent_reference);

    let records = host.records.lock().unwrap().clone();
    let persisted = records
        .iter()
        .rev()
        .find(|record| !record.output_references.is_empty())
        .expect("the Host persisted a canonical reference");
    assert_eq!(
        persisted.tool_call_id.as_deref(),
        Some(id.as_str()),
        "the canonical call keeps the provider id, not an approval attempt id"
    );
    let reference = persisted.output_references[0].clone();
    assert_eq!(reference.call_id.as_deref(), Some(id.as_str()));
    assert_eq!(reference.call_id.as_deref().unwrap().len(), 512);
    assert_reference_bytes(&root, &reference);
    assert_eq!(reference.path, intent_reference.path);

    // The durable journal really contains the full 512-byte id on disk.
    let journal_file = journal_path(&root, "call-id-bound");
    let journal_text = std::fs::read_to_string(&journal_file).unwrap();
    assert!(
        journal_text.contains(&id),
        "the on-disk journal keeps the full id"
    );
}

/// Two provider ids that share 511 bytes and differ only in the last byte must
/// not collide in the index or the ledger, and neither physical file may be
/// deleted by reconciliation.
#[test]
fn near_identical_512_byte_ids_do_not_alias() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let store = OutputStore::open(&root).unwrap();
    let prefix = "z".repeat(511);
    let id_a = format!("{prefix}a");
    let id_b = format!("{prefix}b");
    assert_eq!(id_a.len(), 512);
    assert_eq!(id_b.len(), 512);
    assert_ne!(id_a, id_b);

    let lease = store.begin_capture().unwrap();
    let mut intents = Vec::new();
    for (name, id) in [("alias-a.tool", &id_a), ("alias-b.tool", &id_b)] {
        std::fs::write(store.output_dir().join(name), name.as_bytes()).unwrap();
        let entry = store
            .register(
                &lease,
                OutputEntry {
                    path: format!(".mio/outputs/{name}"),
                    bytes: None,
                    hash: None,
                    stream: "tool".into(),
                    created: 0,
                    call_id: Some(id.clone()),
                },
            )
            .unwrap();
        assert_eq!(entry.call_id.as_deref(), Some(id.as_str()));
        let reference = OutputReference::from_entry(&entry).unwrap();
        assert_eq!(reference.call_id.as_deref(), Some(id.as_str()));
        intents.push(
            store
                .begin_publication_intent(&lease, "run-scope", Some(id), &[reference])
                .unwrap(),
        );
    }
    drop(lease);

    let expected: BTreeSet<String> = [id_a.clone(), id_b.clone()].into_iter().collect();
    let index = store.load_index().unwrap();
    let index_ids: BTreeSet<String> = index
        .entries
        .iter()
        .filter_map(|entry| entry.call_id.clone())
        .collect();
    assert_eq!(
        index_ids, expected,
        "no index entry is aliased or truncated"
    );

    let persisted = store.publication_intents().unwrap();
    assert_eq!(persisted.len(), 2, "{persisted:?}");
    let intent_ids: BTreeSet<String> = persisted
        .iter()
        .filter_map(|intent| intent.call_id.clone())
        .collect();
    assert_eq!(
        intent_ids, expected,
        "the ledger binds each attempt's real id"
    );
    for intent in &persisted {
        assert_eq!(
            intent.references[0].call_id.as_deref(),
            intent.call_id.as_deref()
        );
        assert!(root.join(&intent.references[0].path).exists());
        assert!(
            store
                .reconcile_publication_intent(
                    &intent.attempt_id,
                    &intent.snapshot,
                    PublicationResolution::Unpublished,
                )
                .unwrap()
                .is_some()
        );
    }
    for name in ["alias-a.tool", "alias-b.tool"] {
        assert!(
            root.join(".mio/outputs").join(name).exists(),
            "reconciliation never deletes a shared file"
        );
    }
}

/// A host with no shell and no side effects, only for the policy boundary.
struct NoopHost;

impl Host for NoopHost {
    fn shell(&self, _command: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn on_event(&self, _event: AgentEvent) {}
}

/// The policy byte bound is unchanged: a legal 512-byte id is admitted, a
/// 513-byte one is still refused. The store bound is not greened by widening
/// `ToolPolicy`'s limit.
#[tokio::test]
async fn policy_still_refuses_wire_ids_over_512_bytes() {
    let policy = ToolPolicy::new(
        ToolFilter::all(),
        false,
        Arc::new(NoopHost),
        PolicyContext::new("/tmp", "call-id-bound"),
    );
    let legal = legal_provider_id();
    assert_eq!(legal.len(), 512);
    assert!(
        policy
            .admit_with_call_id("read", r#"{"path":"a"}"#, Some(&legal))
            .await
            .is_ok(),
        "a legal 512-byte provider id is admitted"
    );

    let too_long = format!("{legal}x");
    assert_eq!(too_long.len(), 513);
    let denied = policy
        .admit_with_call_id("read", r#"{"path":"a"}"#, Some(&too_long))
        .await
        .unwrap_err();
    assert!(
        denied.reason().contains("exceeds 512 bytes"),
        "{}",
        denied.reason()
    );
    assert_eq!(
        policy.claim_count(),
        1,
        "the refused over-long id claims no ledger slot"
    );
}
