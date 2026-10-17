//! DES-17 reference publication intent ledger, end to end over real loopback
//! providers, a real disk and a real SIGKILL.
//!
//! The gap this file pins: after `batch_output_budget` writes an externalized
//! file and registers it, it calls the Host to persist typed references. If that
//! callback returns `Err` there is no way to tell whether the references were
//! partially persisted, so the files must be kept without pretending they were
//! committed. This file proves the new durable, versioned, bounded publication
//! intent ledger closes that hole:
//!
//! - a real Host failure keeps a `PendingUncertain` intent with the real
//!   attempt/snapshot, stops before the next provider request, and keeps the
//!   possibly persisted file;
//! - a Host that really writes the journal and then fails leaves the reference
//!   visible to the normal scan and the intent pending;
//! - an earlier call whose Host callback succeeded hands its intent off, while a
//!   later failure stays pending;
//! - an intent write failure rolls back the fresh, never-published files;
//! - a real child process killed by SIGKILL before/during the Host callback
//!   still shows a recoverable `Pending` intent through a read-only open;
//! - an old snapshot or unknown identity can never resolve a newer intent and
//!   never deletes a real referenced file;
//! - a pending intent protects its file past the 7-day retention threshold;
//! - a successful pipeline leaves no unresolved intents and does not pin files
//!   forever.

use mio_coding_agent::{
    AgentEvent, Config, Host, HostFuture, Provider, RunOptions, Services,
    journal::{Journal, RunState},
    run_with_options,
    shell::output_store::{
        OutputEntry, OutputReference, OutputStore, OutputStoreError, PublicationIntent,
        PublicationIntentState, PublicationResolution,
    },
    tool_lifecycle::{ToolCallPhase, ToolCallRecord, ToolCallTracker},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    net::TcpListener,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

#[path = "../src/test_http.rs"]
mod test_http;

const TAG_BYTES: usize = 9_000;
const CALLS: usize = 4;

fn body_sized(index: usize, size: usize) -> String {
    let mut body = format!("TAG-{index}-payload-");
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

fn tool_call_chunk(index: usize, name: &str, arguments: Value) -> String {
    chunk(
        json!({"tool_calls": [{
            "index": index,
            "id": format!("call_{index}"),
            "type": "function",
            "function": {"name": name, "arguments": arguments.to_string()}
        }]}),
        Value::Null,
    )
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
    RunOptions {
        tool_concurrency: CALLS,
        ..RunOptions::default()
    }
}

fn tool_turn() -> String {
    let mut events = Vec::new();
    for index in 0..CALLS {
        let delay = (CALLS - index) as u64 * 120;
        events.push(tool_call_chunk(
            index,
            "bash",
            json!({"command": format!("emit {delay} {index} {TAG_BYTES}")}),
        ));
    }
    events.push(chunk(json!({}), json!("tool_calls")));
    sse(events)
}

/// A batch whose largest body is call_0, so call_0 is published and truly
/// referenced before call_1 fails its canonical supplement.
fn supplement_failure_turn() -> String {
    let mut events = Vec::new();
    for index in 0..CALLS {
        let delay = (CALLS - index) as u64 * 60;
        let size = if index == 0 { TAG_BYTES * 3 } else { TAG_BYTES };
        events.push(tool_call_chunk(
            index,
            "bash",
            json!({"command": format!("emit {delay} {index} {size}")}),
        ));
    }
    events.push(chunk(json!({}), json!("tool_calls")));
    sse(events)
}

fn final_turn() -> String {
    sse(vec![
        chunk(json!({"content": "完成"}), Value::Null),
        chunk(json!({}), json!("stop")),
    ])
}

/// A one-call batch whose single body is large enough on its own to exceed the
/// input budget, so exactly one publication attempt is made.
fn single_tool_turn() -> String {
    let mut events = Vec::new();
    events.push(tool_call_chunk(
        0,
        "bash",
        json!({"command": format!("emit 10 0 {}", TAG_BYTES * 4)}),
    ));
    events.push(chunk(json!({}), json!("tool_calls")));
    sse(events)
}

#[derive(Clone)]
enum HostMode {
    /// Persist every lifecycle record; the supplement succeeds.
    Persist,
    /// Persist every lifecycle record, then fail the supplement *after* writing.
    PersistThenFail,
    /// Fail only the supplement (a record carrying output references).
    Fail,
    /// Fail only the supplement whose reference belongs to this provider call.
    FailFor(String),
    /// Persist the supplement to the journal, then corrupt the durable
    /// publication ledger and return `Ok`: the engine's own handoff read fails
    /// closed instead of pretending the intent was retired.
    PersistThenCorruptLedger,
    /// Persist the supplement to the journal, then precisely remove this
    /// attempt from the durable ledger and return `Ok`: the handoff finds
    /// `Ok(None)` instead of silently continuing as though it were finalized.
    PersistThenDropIntent,
}

struct PublicationHost {
    journal: Option<Arc<Journal>>,
    root: Option<PathBuf>,
    mode: HostMode,
    records: Arc<Mutex<Vec<ToolCallRecord>>>,
}

impl PublicationHost {
    fn new(mode: HostMode) -> Self {
        Self {
            journal: None,
            root: None,
            mode,
            records: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn with_journal(root: &Path, mode: HostMode) -> Self {
        Self {
            journal: Some(Arc::new(Journal::open(root, "publication-qa"))),
            root: Some(root.to_path_buf()),
            mode,
            records: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn persist(&self, call: &ToolCallRecord) -> Result<(), String> {
        if let Some(journal) = &self.journal {
            journal.record_canonical_phase(call)?;
        }
        Ok(())
    }

    fn publication_ledger_path(&self) -> PathBuf {
        self.root
            .as_ref()
            .expect("a ledger-mutating host mode needs the workspace root")
            .join(".mio/output-index/publication-intents.json")
    }
}

impl Host for PublicationHost {
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
        let is_supplement = !call.output_references.is_empty();
        match &self.mode {
            HostMode::Persist => self.persist(call),
            HostMode::PersistThenFail => {
                self.persist(call)?;
                if is_supplement {
                    Err("publication host failed after writing the journal".into())
                } else {
                    Ok(())
                }
            }
            HostMode::Fail => {
                if is_supplement {
                    Err("publication host refused to persist".into())
                } else {
                    Ok(())
                }
            }
            HostMode::FailFor(target) => {
                let matches = call
                    .output_references
                    .iter()
                    .any(|reference| reference.call_id.as_deref() == Some(target.as_str()));
                if matches {
                    Err("publication host refused to persist".into())
                } else {
                    self.persist(call)
                }
            }
            HostMode::PersistThenCorruptLedger => {
                self.persist(call)?;
                if is_supplement {
                    // The Host really wrote the journal reference, then the
                    // ledger became unreadable before the engine's handoff.
                    std::fs::write(self.publication_ledger_path(), b"{corrupt")
                        .map_err(|error| error.to_string())?;
                }
                Ok(())
            }
            HostMode::PersistThenDropIntent => {
                self.persist(call)?;
                if is_supplement {
                    // The only unresolved attempt is this one, so rewriting the
                    // versioned ledger with an empty intent list removes exactly
                    // this attempt while leaving the journal reference durable.
                    std::fs::write(
                        self.publication_ledger_path(),
                        br#"{"version":1,"intents":[]}"#,
                    )
                    .map_err(|error| error.to_string())?;
                }
                Ok(())
            }
        }
    }
}

fn future() -> SystemTime {
    SystemTime::now() + Duration::from_secs(60 * 24 * 60 * 60)
}

/// The on-disk bytes behind a reference match its recorded SHA-256 and length.
fn assert_reference_bytes(root: &Path, reference: &OutputReference) {
    let bytes = std::fs::read(root.join(&reference.path)).unwrap();
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    assert_eq!(reference.sha256, format!("{:x}", hasher.finalize()));
    assert_eq!(reference.total_bytes, bytes.len() as u64);
}

// --- store-level ledger contracts ----------------------------------------

fn temp_store() -> (tempfile::TempDir, OutputStore) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let store = OutputStore::open(&root).unwrap();
    (dir, store)
}

fn publish_pending(
    store: &OutputStore,
    name: &str,
    body: &[u8],
    call_id: &str,
) -> (OutputEntry, PublicationIntent) {
    let lease = store.begin_capture().unwrap();
    std::fs::write(store.output_dir().join(name), body).unwrap();
    let entry = store
        .register(
            &lease,
            OutputEntry {
                path: format!(".mio/outputs/{name}"),
                bytes: None,
                hash: None,
                stream: "tool".into(),
                created: 0,
                call_id: Some(call_id.into()),
            },
        )
        .unwrap();
    let reference = OutputReference::from_entry(&entry).unwrap();
    let intent = store
        .begin_publication_intent(&lease, "run-scope", Some(call_id), &[reference])
        .unwrap();
    (entry, intent)
}

/// These ledger tests require a quiescent workspace. Other tests in this same
/// process spawn child probes in parallel, and a just-forked child can briefly
/// inherit the shared activity descriptor until exec closes it. Production
/// reconciliation remains nonblocking; only the fixture waits for that narrow
/// handoff window, and a persistent Busy still fails the test.
fn reconcile_when_quiescent(
    store: &OutputStore,
    attempt_id: &str,
    snapshot: &str,
    resolution: PublicationResolution,
) -> Result<Option<PublicationIntent>, OutputStoreError> {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match store.reconcile_publication_intent(attempt_id, snapshot, resolution) {
            Err(OutputStoreError::Busy(_)) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(2));
            }
            result => return result,
        }
    }
}

/// A `Pending` intent is protected by cleanup past the 7-day retention window;
/// only an explicit Host reconciliation lifts the protection, and that
/// reconciliation never deletes the physical file.
#[test]
fn pending_intent_protects_its_file_beyond_retention() {
    let (dir, store) = temp_store();
    let (_entry, intent) = publish_pending(&store, "pending.tool", b"pending body", "call-a");
    assert_eq!(intent.state, PublicationIntentState::Pending);
    let file = dir.path().join(&intent.references[0].path);
    assert!(file.exists());

    let plan = store.plan_cleanup(future()).unwrap();
    assert!(
        plan.retained_referenced
            .iter()
            .any(|path| path == &intent.references[0].path),
        "a pending publication intent keeps its reference retained: {plan:?}"
    );
    assert!(
        !plan
            .candidates
            .iter()
            .any(|entry| entry.path == intent.references[0].path),
        "retention alone must never GC a pending reference"
    );
    assert!(
        plan.warnings
            .iter()
            .any(|warning| warning.contains("unresolved output publication intent")),
        "the plan reports the pending evidence: {plan:?}"
    );

    // Explicit host reconciliation to Unpublished retires the intent.
    let resolved = reconcile_when_quiescent(
        &store,
        &intent.attempt_id,
        &intent.snapshot,
        PublicationResolution::Unpublished,
    )
    .unwrap()
    .expect("the exact snapshot resolves the intent");
    assert_eq!(resolved.attempt_id, intent.attempt_id);
    assert!(store.publication_intents().unwrap().is_empty());
    assert!(
        file.exists(),
        "reconciliation never deletes a possibly shared content-addressed file"
    );

    // Now only normal retention applies, and the file is beyond it.
    let plan = store.plan_cleanup(future()).unwrap();
    assert!(
        plan.candidates
            .iter()
            .any(|entry| entry.path == intent.references[0].path),
        "after reconciliation the unreferenced file follows normal retention: {plan:?}"
    );
}

/// An old snapshot cannot resolve a newer intent, a missing identity is `None`
/// (never fabricated), and neither path deletes a real referenced file.
#[test]
fn stale_snapshot_is_rejected_and_unknown_identity_is_not_fabricated() {
    let (dir, store) = temp_store();
    let (_entry, intent) = publish_pending(&store, "stale.tool", b"stale body", "call-b");
    let file = dir.path().join(&intent.references[0].path);
    assert!(file.exists());

    let old = "0".repeat(64);
    let error = reconcile_when_quiescent(
        &store,
        &intent.attempt_id,
        &old,
        PublicationResolution::Published,
    )
    .unwrap_err();
    assert!(
        matches!(error, OutputStoreError::StalePlan(_)),
        "an old snapshot must be rejected: {error:?}"
    );
    assert!(
        file.exists(),
        "a rejected snapshot must not delete a referenced file"
    );

    // Unknown attempt id: no intent, no fabrication.
    let missing = reconcile_when_quiescent(
        &store,
        "00000000-0000-0000-0000-000000000000",
        &intent.snapshot,
        PublicationResolution::Published,
    )
    .unwrap();
    assert!(
        missing.is_none(),
        "an unknown identity must not be fabricated"
    );
    assert!(file.exists());

    // A second intent for the same file cannot be resolved with the first
    // intent's snapshot: the snapshot binds the attempt identity.
    let (_second_entry, second) =
        publish_pending(&store, "stale-two.tool", b"stale body two", "call-c");
    let error = reconcile_when_quiescent(
        &store,
        &second.attempt_id,
        &intent.snapshot,
        PublicationResolution::Published,
    )
    .unwrap_err();
    assert!(matches!(error, OutputStoreError::StalePlan(_)));

    // The exact snapshot resolves and the file survives.
    assert!(
        reconcile_when_quiescent(
            &store,
            &intent.attempt_id,
            &intent.snapshot,
            PublicationResolution::Published,
        )
        .unwrap()
        .is_some()
    );
    assert!(file.exists());
    assert_eq!(store.publication_intents().unwrap().len(), 1);
}

/// Read-only recovery enumerates pending evidence without creating any file or
/// directory, and does not mutate the ledger.
#[test]
fn read_only_recovery_enumerates_pending_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let read_only = OutputStore::open_readonly(&root).unwrap();
    assert!(read_only.publication_intents().unwrap().is_empty());
    assert!(
        !root.join(".mio").exists(),
        "read-only recovery must not create .mio"
    );

    let store = OutputStore::open(&root).unwrap();
    let (_entry, intent) = publish_pending(&store, "recover.tool", b"recover body", "call-d");
    drop(store);

    let recovered = OutputStore::open_readonly(&root).unwrap();
    let intents = recovered.publication_intents().unwrap();
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].attempt_id, intent.attempt_id);
    assert_eq!(intents[0].state, PublicationIntentState::Pending);
    assert_eq!(intents[0].call_id.as_deref(), Some("call-d"));
    assert_eq!(intents[0].references[0].path, intent.references[0].path);
    let again = OutputStore::open_readonly(&root)
        .unwrap()
        .publication_intents()
        .unwrap();
    assert_eq!(
        again, intents,
        "the read-only pass must not mutate the ledger"
    );
}

/// Corrupt, wrong-version or oversized ledgers fail closed and refuse cleanup.
#[test]
fn corrupt_versioned_or_oversized_ledger_fails_closed() {
    let (_dir, store) = temp_store();
    let path = store.publication_intent_path();

    std::fs::write(&path, b"{not json").unwrap();
    assert!(matches!(
        store.load_publication_intents(),
        Err(OutputStoreError::Corrupt(_))
    ));

    std::fs::write(&path, br#"{"version":999,"intents":[]}"#).unwrap();
    assert!(matches!(
        store.load_publication_intents(),
        Err(OutputStoreError::Corrupt(_))
    ));

    let oversized = vec![b' '; mio_coding_agent::shell::output_store::MAX_PUBLICATION_BYTES + 1];
    std::fs::write(&path, oversized).unwrap();
    assert!(matches!(
        store.load_publication_intents(),
        Err(OutputStoreError::Corrupt(_))
    ));

    // A corrupt ledger refuses cleanup instead of silently ignoring the evidence.
    std::fs::write(&path, b"{not json").unwrap();
    assert!(matches!(
        store.plan_cleanup(SystemTime::now()),
        Err(OutputStoreError::Corrupt(_))
    ));
}

// --- loopback end-to-end cases -------------------------------------------

/// A Host that never persists references keeps exactly one uncertain intent,
/// stops before the next provider request, and reports the real attempt.
#[tokio::test]
async fn host_failure_records_pending_uncertain_and_stops_before_the_next_request() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (base_url, server) = spawn_server(vec![tool_turn(), final_turn()]);
    let host = Arc::new(PublicationHost::new(HostMode::Fail));

    let error = run_with_options(
        config(base_url),
        Vec::new(),
        "run the batch",
        &root,
        host,
        Services::default(),
        options(),
    )
    .await
    .expect_err("a Host failure stops the run");
    assert!(error.starts_with("Context budget:"), "{error}");
    assert!(error.contains("may have partially persisted"), "{error}");
    assert!(error.contains("PendingUncertain"), "{error}");

    let requests = server.join().unwrap();
    assert_eq!(
        requests.len(),
        1,
        "no provider request follows the failed publication; got {}",
        requests.len()
    );

    let store = OutputStore::open(&root).unwrap();
    let intents = store.publication_intents().unwrap();
    assert_eq!(intents.len(), 1, "{intents:?}");
    let intent = &intents[0];
    assert_eq!(intent.state, PublicationIntentState::PendingUncertain);
    assert_eq!(intent.call_id.as_deref(), Some("call_0"));
    assert_eq!(intent.references.len(), 1);
    assert!(
        error.contains(&intent.attempt_id),
        "the error reports the real attempt id: {error}"
    );
    assert!(
        error.contains(&intent.snapshot),
        "the error reports the real snapshot: {error}"
    );
    let file = root.join(&intent.references[0].path);
    assert!(file.exists(), "a possibly persisted file is never deleted");
    assert_eq!(store.load_index().unwrap().entries.len(), 1);

    let plan = store.plan_cleanup(future()).unwrap();
    assert!(
        plan.retained_referenced
            .iter()
            .any(|path| path == &intent.references[0].path),
        "the uncertain reference is protected past retention: {plan:?}"
    );
}

/// A Host that really writes the canonical record to the journal and *then*
/// fails leaves both the real reference and a `PendingUncertain` intent.
#[tokio::test]
async fn partial_journal_failure_keeps_a_pending_intent_and_the_real_reference() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let host = Arc::new(PublicationHost::with_journal(
        &root,
        HostMode::PersistThenFail,
    ));
    let journal = host.journal.clone().unwrap();
    journal.begin_run().unwrap();
    let (base_url, server) = spawn_server(vec![tool_turn(), final_turn()]);

    let error = run_with_options(
        config(base_url),
        Vec::new(),
        "run the batch",
        &root,
        host.clone(),
        Services::default(),
        options(),
    )
    .await
    .expect_err("a Host failure stops the run");
    assert!(error.contains("may have partially persisted"), "{error}");
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 1);
    journal.finish_run(RunState::Completed).unwrap();

    let store = OutputStore::open(&root).unwrap();
    let intents = store.publication_intents().unwrap();
    assert_eq!(intents.len(), 1, "{intents:?}");
    assert_eq!(intents[0].state, PublicationIntentState::PendingUncertain);

    let records = host.records.lock().unwrap().clone();
    let persisted = records
        .iter()
        .rev()
        .find(|record| !record.output_references.is_empty())
        .expect("the Host wrote the canonical reference before failing");
    let reference = persisted.output_references[0].clone();
    assert!(
        root.join(&reference.path).exists(),
        "the partially persisted reference file survives"
    );

    // The journal scan alone protects the file once the intent is handed off.
    store
        .reconcile_publication_intent(
            &intents[0].attempt_id,
            &intents[0].snapshot,
            PublicationResolution::Published,
        )
        .unwrap();
    let plan = store.plan_cleanup(future()).unwrap();
    assert!(
        plan.retained_referenced
            .iter()
            .any(|path| path == &reference.path),
        "the real journal reference keeps the file: {plan:?}"
    );
}

/// call_0 published and truly referenced first; call_1 then fails. Only the
/// uncertain call stays pending, and both files survive.
#[tokio::test]
async fn earlier_published_call_is_handed_off_while_the_later_failure_stays_pending() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let host = Arc::new(PublicationHost::with_journal(
        &root,
        HostMode::FailFor("call_1".into()),
    ));
    let journal = host.journal.clone().unwrap();
    journal.begin_run().unwrap();
    let (base_url, server) = spawn_server(vec![supplement_failure_turn(), final_turn()]);

    let error = run_with_options(
        config(base_url),
        Vec::new(),
        "run the batch",
        &root,
        host.clone(),
        Services::default(),
        options(),
    )
    .await
    .expect_err("the later Host supplement failure stops the run");
    assert!(error.contains("may have partially persisted"), "{error}");
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 1);
    journal.finish_run(RunState::Completed).unwrap();

    let store = OutputStore::open(&root).unwrap();
    let intents = store.publication_intents().unwrap();
    assert_eq!(
        intents.len(),
        1,
        "only the uncertain call stays pending: {intents:?}"
    );
    assert_eq!(intents[0].call_id.as_deref(), Some("call_1"));
    assert_eq!(intents[0].state, PublicationIntentState::PendingUncertain);

    let index = store.load_index().unwrap();
    assert_eq!(
        index.entries.len(),
        2,
        "call_0's referenced file and call_1's uncertain file are both retained"
    );
    let plan = store.plan_cleanup(future()).unwrap();
    for entry in &index.entries {
        assert!(
            plan.retained_referenced
                .iter()
                .any(|path| path == &entry.path),
            "{} must be retained: {plan:?}",
            entry.path
        );
    }
}

/// A successful pipeline hands every intent off to the real journal scan: the
/// ledger stays empty, files are not pinned forever, and the bytes match.
#[tokio::test]
async fn successful_pipeline_hands_off_and_leaves_no_unresolved_intents() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let host = Arc::new(PublicationHost::with_journal(&root, HostMode::Persist));
    let journal = host.journal.clone().unwrap();
    journal.begin_run().unwrap();
    let (base_url, server) = spawn_server(vec![tool_turn(), final_turn()]);

    run_with_options(
        config(base_url),
        Vec::new(),
        "run the batch",
        &root,
        host.clone(),
        Services::default(),
        options(),
    )
    .await
    .expect("the batch is externalized");
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    journal.finish_run(RunState::Completed).unwrap();

    let store = OutputStore::open(&root).unwrap();
    assert!(
        store.publication_intents().unwrap().is_empty(),
        "a successful Host hands every intent off"
    );
    let ledger = store.load_publication_intents().unwrap();
    assert_eq!(
        ledger.version,
        mio_coding_agent::shell::output_store::PUBLICATION_INTENT_VERSION
    );

    let index = store.load_index().unwrap();
    assert!(!index.entries.is_empty());
    let records = host.records.lock().unwrap().clone();
    for record in records.iter().filter(|r| !r.output_references.is_empty()) {
        for reference in &record.output_references {
            let relative = reference.path.strip_prefix(".mio/outputs/").unwrap();
            let bytes = std::fs::read(root.join(".mio/outputs").join(relative)).unwrap();
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            assert_eq!(reference.sha256, format!("{:x}", hasher.finalize()));
            assert_eq!(reference.total_bytes, bytes.len() as u64);
        }
    }

    // The real journal scan protects the files; the ledger does not pin them.
    let plan = store.plan_cleanup(future()).unwrap();
    for entry in &index.entries {
        assert!(
            plan.retained_referenced
                .iter()
                .any(|path| path == &entry.path),
            "{} is protected by the real journal reference: {plan:?}",
            entry.path
        );
    }
}

/// An intent write failure happens before any Host callback, so the fresh files
/// and their index entries must be rolled back with no unregistered leftovers.
#[tokio::test]
async fn intent_registration_failure_rolls_back_fresh_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    // A directory at the ledger path makes the durable Pending write fail
    // closed before any Host callback.
    std::fs::create_dir_all(root.join(".mio/output-index/publication-intents.json")).unwrap();
    let (base_url, server) = spawn_server(vec![tool_turn(), final_turn()]);
    let host = Arc::new(PublicationHost::new(HostMode::Persist));

    let error = run_with_options(
        config(base_url),
        Vec::new(),
        "run the batch",
        &root,
        host,
        Services::default(),
        options(),
    )
    .await
    .expect_err("a failed intent registration stops the run");
    assert!(error.starts_with("Context budget:"), "{error}");
    assert!(error.contains("publication intent"), "{error}");
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 1);

    let store = OutputStore::open(&root).unwrap();
    let leftovers: Vec<PathBuf> = std::fs::read_dir(root.join(".mio/outputs"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    assert!(
        leftovers.is_empty(),
        "a failed intent write must not leak a fresh file: {leftovers:?}"
    );
    assert!(store.load_index().unwrap().entries.is_empty());
}

// --- handoff failures after a genuinely successful Host callback ----------

/// A Host that really writes the canonical journal reference and then makes the
/// durable publication ledger unreadable must not be reported as a successful
/// handoff. Before the fix the engine ignored the handoff error, kept going, and
/// the run looked like an ordinary success; now it stops with a real
/// `ContextBudget` error, keeps the file the Host may already reference, and
/// never asks the model again.
#[tokio::test]
async fn corrupt_ledger_after_journal_write_stops_before_the_next_request() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let host = Arc::new(PublicationHost::with_journal(
        &root,
        HostMode::PersistThenCorruptLedger,
    ));
    let journal = host.journal.clone().unwrap();
    journal.begin_run().unwrap();
    let (base_url, server) = spawn_server(vec![single_tool_turn(), final_turn()]);

    let error = run_with_options(
        config(base_url),
        Vec::new(),
        "run the batch",
        &root,
        host.clone(),
        Services::default(),
        options(),
    )
    .await
    .expect_err("a failed publication handoff stops the run");
    assert!(error.starts_with("Context budget:"), "{error}");
    assert!(
        error.contains("could not finalize output publication attempt"),
        "{error}"
    );
    assert!(error.contains("files are kept"), "{error}");

    let requests = server.join().unwrap();
    assert_eq!(
        requests.len(),
        1,
        "no provider request follows the failed handoff; got {}",
        requests.len()
    );

    journal.finish_run(RunState::Completed).unwrap();
    let store = OutputStore::open(&root).unwrap();

    // The journal the Host really wrote is durable, and the file it references
    // is kept because the Host may already hold it.
    let records = host.records.lock().unwrap().clone();
    let persisted = records
        .iter()
        .rev()
        .find(|record| !record.output_references.is_empty())
        .expect("the Host persisted the reference before the ledger broke");
    let reference = persisted.output_references[0].clone();
    let file = root.join(&reference.path);
    assert!(
        file.exists(),
        "an accepted reference file is never rolled back"
    );
    assert_reference_bytes(&root, &reference);

    // The unreadable ledger fails closed, so cleanup cannot delete the file...
    assert!(matches!(
        store.plan_cleanup(future()),
        Err(OutputStoreError::Corrupt(_))
    ));
    // ...and the durable journal reference keeps it even once the unreadable
    // evidence is removed.
    std::fs::remove_file(store.publication_intent_path()).unwrap();
    let plan = store.plan_cleanup(future()).unwrap();
    assert!(
        plan.retained_referenced
            .iter()
            .any(|path| path == &reference.path),
        "the real journal reference protects the file: {plan:?}"
    );
    assert!(file.exists());
}

/// A Host that really writes the canonical journal reference and then precisely
/// removes this attempt from the durable ledger must likewise stop explicitly:
/// the handoff returns `Ok(None)`, which is not a success and must not be
/// silently ignored. The file the Host references survives and stays protected
/// by the real journal scan.
#[tokio::test]
async fn dropped_intent_after_journal_write_stops_before_the_next_request() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let host = Arc::new(PublicationHost::with_journal(
        &root,
        HostMode::PersistThenDropIntent,
    ));
    let journal = host.journal.clone().unwrap();
    journal.begin_run().unwrap();
    let (base_url, server) = spawn_server(vec![single_tool_turn(), final_turn()]);

    let error = run_with_options(
        config(base_url),
        Vec::new(),
        "run the batch",
        &root,
        host.clone(),
        Services::default(),
        options(),
    )
    .await
    .expect_err("a vanished publication intent stops the run");
    assert!(error.starts_with("Context budget:"), "{error}");
    assert!(error.contains("disappeared before handoff"), "{error}");
    assert!(error.contains("reconciliation is required"), "{error}");

    let requests = server.join().unwrap();
    assert_eq!(
        requests.len(),
        1,
        "no provider request follows the failed handoff; got {}",
        requests.len()
    );

    journal.finish_run(RunState::Completed).unwrap();
    let store = OutputStore::open(&root).unwrap();
    assert!(
        store.publication_intents().unwrap().is_empty(),
        "the Host removed the attempt from the durable ledger"
    );

    let records = host.records.lock().unwrap().clone();
    let persisted = records
        .iter()
        .rev()
        .find(|record| !record.output_references.is_empty())
        .expect("the Host persisted the reference before dropping the intent");
    let reference = persisted.output_references[0].clone();
    assert!(root.join(&reference.path).exists());
    assert_reference_bytes(&root, &reference);

    let plan = store.plan_cleanup(future()).unwrap();
    assert!(
        plan.retained_referenced
            .iter()
            .any(|path| path == &reference.path),
        "the real journal reference protects the file: {plan:?}"
    );
}

// --- real SIGKILL across a process boundary ------------------------------

#[cfg(unix)]
const PROBE_ROOT: &str = "MIO_DES17_PUBLICATION_PROBE_ROOT";
#[cfg(unix)]
const PROBE_MODE: &str = "MIO_DES17_PUBLICATION_PROBE_MODE";

#[cfg(unix)]
struct BlockingHost;

#[cfg(unix)]
impl Host for BlockingHost {
    fn shell(&self, _command: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn on_event(&self, _event: AgentEvent) {}

    fn on_tool_call_lifecycle(&self, _call: &ToolCallRecord) -> Result<(), String> {
        // A Host callback that never returns: the intent must remain Pending.
        loop {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Child half of the SIGKILL test. It only acts when the parent re-invokes this
/// exact test in a fresh process with the probe environment.
#[cfg(unix)]
#[test]
fn des17_publication_probe_child() {
    let Ok(root) = std::env::var(PROBE_ROOT) else {
        return;
    };
    let mode = std::env::var(PROBE_MODE).unwrap_or_default();
    let root = PathBuf::from(root);
    let store = OutputStore::open(&root).unwrap();
    let lease = store.begin_capture().unwrap();
    let name = "probe.tool";
    std::fs::write(store.output_dir().join(name), b"probe body").unwrap();
    let entry = store
        .register(
            &lease,
            OutputEntry {
                path: format!(".mio/outputs/{name}"),
                bytes: None,
                hash: None,
                stream: "tool".into(),
                created: 0,
                call_id: Some("probe-call".into()),
            },
        )
        .unwrap();
    let reference = OutputReference::from_entry(&entry).unwrap();
    store
        .begin_publication_intent(
            &lease,
            "probe-run",
            Some("probe-call"),
            std::slice::from_ref(&reference),
        )
        .unwrap();
    if mode == "during_host" {
        let tracker = ToolCallTracker::new("probe-run".to_string(), Arc::new(BlockingHost));
        let call_index = tracker
            .begin("read", "read", "file", Some("probe-call"), Some(1), Some(1))
            .unwrap();
        tracker.admit(call_index, Some(1), Some(1)).unwrap();
        tracker.mark_dispatched(1, Some("probe-call")).unwrap();
        tracker
            .observe_outcome(1, Some("probe-call"), ToolCallPhase::Executed, Some("ok"))
            .unwrap();
        // Blocks inside the Host callback; the parent kills us here.
        let _ =
            tracker.supplement_output_references("probe-run", "probe-call", &[reference], false);
    }
    loop {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// RAII guard around a probe child process.
///
/// A panicking assertion (or an early return) must never leak a probe. The old
/// raw `Child` left orphans behind whenever an assertion fired before the
/// explicit kill; `Drop` now reclaims the child.
///
/// Invariants:
/// - only this exact pid is signalled, never the process group (the probes
///   share cargo's process group, so a group kill would take the runner down);
/// - a child that was already reaped is never signalled again (its pid may have
///   been reused);
/// - reaping is bounded by a 5 s deadline instead of an unbounded `wait`, so a
///   stuck child cannot hang the test run.
#[cfg(unix)]
struct ProbeChild {
    /// `None` once the child has been reaped (or given up on), so this guard
    /// never signals a stale pid and never waits twice.
    child: Option<std::process::Child>,
}

#[cfg(unix)]
impl ProbeChild {
    fn spawn(command: &mut std::process::Command) -> Self {
        Self {
            child: Some(command.spawn().unwrap()),
        }
    }

    /// SIGKILL only this child's pid and return the raw `libc::kill` result.
    fn signal_sigkill(&self) -> i32 {
        match self.child.as_ref() {
            // SAFETY: this is the pid of a process we spawned and have not
            // reaped; a positive pid never targets the process group.
            Some(child) => unsafe { libc::kill(child.id() as i32, libc::SIGKILL) },
            // Already reaped: never signal a possibly-reused pid.
            None => 0,
        }
    }

    /// Reap with a hard 5 s deadline; never blocks without a bound. Stops
    /// tracking the child whatever the outcome, so a later `Drop` can never add
    /// a second wait.
    fn reap_bounded(&mut self) -> Option<std::process::ExitStatus> {
        let child = self.child.as_mut()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) | Err(_) => break None,
            }
        };
        self.child = None;
        status
    }
}

#[cfg(unix)]
impl Drop for ProbeChild {
    fn drop(&mut self) {
        if self.child.is_some() {
            let _ = self.signal_sigkill();
            let _ = self.reap_bounded();
        }
    }
}

#[cfg(unix)]
fn wait_for_pending_ledger(root: &Path) {
    let path = root.join(".mio/output-index/publication-intents.json");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if path.exists()
            && let Ok(store) = OutputStore::open_readonly(root)
            && store
                .publication_intents()
                .map(|intents| !intents.is_empty())
                .unwrap_or(false)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the probe never recorded a durable pending intent");
}

#[cfg(unix)]
fn kill_probe(root: &Path, mode: &str) {
    let exe = std::env::current_exe().unwrap();
    let mut command = std::process::Command::new(exe);
    command
        .args(["--exact", "des17_publication_probe_child", "--nocapture"])
        .env(PROBE_ROOT, root.as_os_str())
        .env(PROBE_MODE, mode)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = ProbeChild::spawn(&mut command);
    // A panic/early return from here on is reclaimed by the guard instead of
    // orphaning the probe.
    wait_for_pending_ledger(root);
    assert_eq!(child.signal_sigkill(), 0, "SIGKILL the probe");
    let status = child
        .reap_bounded()
        .expect("the probe must have been killed and reaped");
    assert!(!status.success(), "the probe must have been killed");
}

#[cfg(unix)]
fn assert_pending_recoverable(root: &Path) {
    let store = OutputStore::open_readonly(root).unwrap();
    let intents = store.publication_intents().unwrap();
    assert_eq!(
        intents.len(),
        1,
        "the SIGKILL left exactly one intent: {intents:?}"
    );
    assert_eq!(intents[0].state, PublicationIntentState::Pending);
    assert_eq!(intents[0].call_id.as_deref(), Some("probe-call"));
    assert!(root.join(&intents[0].references[0].path).exists());
}

/// SIGKILL before the Host callback: the durable `Pending` intent survives and
/// a read-only open recovers it.
#[cfg(unix)]
#[test]
fn sigkill_before_host_publication_leaves_a_recoverable_pending_intent() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    kill_probe(&root, "before_host");
    assert_pending_recoverable(&root);
}

/// SIGKILL while the Host callback is still running: the result is unknown, so
/// the intent must stay `Pending` and be recoverable.
#[cfg(unix)]
#[test]
fn sigkill_during_host_publication_leaves_a_recoverable_pending_intent() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    kill_probe(&root, "during_host");
    assert_pending_recoverable(&root);
}

// --- explicit reconciliation vs. a live publisher ------------------------

#[cfg(unix)]
const RECONCILE_PROBE_ROOT: &str = "MIO_DES17_RECONCILE_PROBE_ROOT";
#[cfg(unix)]
const RECONCILE_PROBE_MARKER: &str = "MIO_DES17_RECONCILE_PROBE_MARKER";

/// A Host whose supplement callback writes a real barrier file and then blocks
/// forever, so the parent can prove the publisher is inside its Host callback
/// while still holding the capture lease.
#[cfg(unix)]
struct BarrierHost {
    marker: PathBuf,
}

#[cfg(unix)]
impl Host for BarrierHost {
    fn shell(&self, _command: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn on_event(&self, _event: AgentEvent) {}

    fn on_tool_call_lifecycle(&self, call: &ToolCallRecord) -> Result<(), String> {
        // Let the earlier canonical phases complete; block only once the
        // supplement carrying real output references arrives.
        if call.output_references.is_empty() {
            return Ok(());
        }
        let _ = std::fs::write(&self.marker, b"host-callback-entered");
        loop {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Child half of the reconcile-race test: register a real file, write a durable
/// `Pending` intent, then block inside the Host callback while the capture lease
/// (and therefore the workspace shared activity lease) is still held.
#[cfg(unix)]
#[test]
fn des17_reconcile_probe_child() {
    let Ok(root) = std::env::var(RECONCILE_PROBE_ROOT) else {
        return;
    };
    let marker = PathBuf::from(
        std::env::var(RECONCILE_PROBE_MARKER).expect("the parent passes a barrier marker path"),
    );
    let root = PathBuf::from(root);
    let store = OutputStore::open(&root).unwrap();
    let lease = store.begin_capture().unwrap();
    let name = "reconcile-probe.tool";
    std::fs::write(store.output_dir().join(name), b"reconcile probe body").unwrap();
    let entry = store
        .register(
            &lease,
            OutputEntry {
                path: format!(".mio/outputs/{name}"),
                bytes: None,
                hash: None,
                stream: "tool".into(),
                created: 0,
                call_id: Some("probe-call".into()),
            },
        )
        .unwrap();
    let reference = OutputReference::from_entry(&entry).unwrap();
    store
        .begin_publication_intent(
            &lease,
            "probe-run",
            Some("probe-call"),
            std::slice::from_ref(&reference),
        )
        .unwrap();
    let tracker = ToolCallTracker::new("probe-run".to_string(), Arc::new(BarrierHost { marker }));
    let call_index = tracker
        .begin("read", "read", "file", Some("probe-call"), Some(1), Some(1))
        .unwrap();
    tracker.admit(call_index, Some(1), Some(1)).unwrap();
    tracker.mark_dispatched(1, Some("probe-call")).unwrap();
    tracker
        .observe_outcome(1, Some("probe-call"), ToolCallPhase::Executed, Some("ok"))
        .unwrap();
    // Blocks inside the Host callback; the parent kills us here after its
    // reconciliation attempt while the capture lease is still held.
    let _ = tracker.supplement_output_references("probe-run", "probe-call", &[reference], false);
    loop {
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
fn spawn_reconcile_probe(root: &Path, marker: &Path) -> ProbeChild {
    let exe = std::env::current_exe().unwrap();
    let mut command = std::process::Command::new(exe);
    command
        .args(["--exact", "des17_reconcile_probe_child", "--nocapture"])
        .env(RECONCILE_PROBE_ROOT, root.as_os_str())
        .env(RECONCILE_PROBE_MARKER, marker.as_os_str())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    ProbeChild::spawn(&mut command)
}

#[cfg(unix)]
fn wait_for_marker(marker: &Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if marker.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the reconcile probe never entered its Host callback");
}

/// Another process reads a real `Pending` snapshot while the publisher's Host
/// callback is still blocked. An explicit `Unpublished` reconciliation must not
/// lift the reference protection yet: the current implementation checks real
/// quiescence through the workspace activity lease instead of trusting time or a
/// PID. Only after the publisher is provably dead does the exact snapshot
/// resolve, and even then the physical file is never deleted.
#[cfg(unix)]
#[test]
fn reconcile_refuses_a_live_publisher_and_succeeds_once_it_is_dead() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let marker = root.join("reconcile-host-callback.marker");
    let mut child = spawn_reconcile_probe(&root, &marker);
    wait_for_marker(&marker);
    wait_for_pending_ledger(&root);

    let store = OutputStore::open(&root).unwrap();
    let intents = store.publication_intents().unwrap();
    assert_eq!(intents.len(), 1, "{intents:?}");
    let intent = intents[0].clone();
    assert_eq!(intent.state, PublicationIntentState::Pending);
    let file = root.join(&intent.references[0].path);
    assert!(file.exists());

    // The snapshot is real and readable, but the publisher is still live: an
    // explicit reconciliation must fail closed with `Busy`.
    let error = store
        .reconcile_publication_intent(
            &intent.attempt_id,
            &intent.snapshot,
            PublicationResolution::Unpublished,
        )
        .unwrap_err();
    assert!(
        matches!(error, OutputStoreError::Busy(_)),
        "a live publisher must not be reconciled away: {error:?}"
    );
    assert_eq!(
        store.publication_intents().unwrap().len(),
        1,
        "the pending intent is untouched while the publisher is live"
    );
    assert!(
        file.exists(),
        "a refused reconciliation must not delete a referenced file"
    );

    // Kill the publisher; the OS releases the flock, so quiescence is now real.
    assert_eq!(child.signal_sigkill(), 0, "SIGKILL the publisher");
    assert!(!child.reap_bounded().unwrap().success());

    let resolved = store
        .reconcile_publication_intent(
            &intent.attempt_id,
            &intent.snapshot,
            PublicationResolution::Unpublished,
        )
        .unwrap()
        .expect("the exact snapshot now resolves");
    assert_eq!(resolved.attempt_id, intent.attempt_id);
    assert!(store.publication_intents().unwrap().is_empty());
    assert!(
        file.exists(),
        "reconciliation never deletes the shared content-addressed file"
    );
}
