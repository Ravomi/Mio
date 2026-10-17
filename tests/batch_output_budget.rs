//! DES-12 whole-batch tool output budget, end to end over a real loopback
//! provider and real tool dispatch.
//!
//! These tests drive `run_with_options` against a local SSE endpoint. A turn
//! of concurrent `bash` calls returns a batch of large results; the batch is
//! larger than the request's estimated input limit, so the batch output budget
//! externalizes the oversized bodies before the following provider request is
//! sent. No credential or external network is involved.
//!
//! They pin the parts a unit test cannot:
//! - several concurrent results completing in reverse order keep their call
//!   order and their own identities/references;
//! - the following provider request is materially smaller and carries typed
//!   `output_references`; each reference's file reconstructs the exact body;
//! - the full body is reachable through the real `read` byte-paging tool;
//! - an output-store failure stops the run with zero further provider requests;
//! - a canonical journal reference protects the externalized file from cleanup.

use mio_coding_agent::{
    AgentEvent, Arguments, Config, Host, HostFuture, Provider, ReadObservation, RunOptions,
    Services, Workspace,
    context_budget::ContextBudget,
    journal::{Journal, RunState},
    run_with_options,
    shell::output_store::OutputStore,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    net::TcpListener,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

#[path = "../src/test_http.rs"]
mod test_http;

const TAG_BYTES: usize = 9_000;
const CALLS: usize = 4;

fn tag(index: usize) -> String {
    format!("TAG-{index}-payload-")
}

fn body(index: usize) -> String {
    body_sized(index, TAG_BYTES)
}

fn body_sized(index: usize, size: usize) -> String {
    let mut body = tag(index);
    while body.len() < size {
        body.push('x');
    }
    body.truncate(size);
    body
}

/// Host whose `shell` returns a large, uniquely tagged body after a delay that
/// makes the calls complete in reverse order.
struct ShellHost {
    completed: Arc<Mutex<Vec<usize>>>,
    read_order: Arc<Mutex<Vec<String>>>,
    journal: Option<Arc<Journal>>,
    records: Arc<Mutex<Vec<mio_coding_agent::tool_lifecycle::ToolCallRecord>>>,
    /// When set, `on_tool_call_lifecycle` fails after recording a call whose
    /// output references carry this id, modelling a Host callback that may have
    /// partially persisted.
    supplement_failure: Option<String>,
}

impl ShellHost {
    fn new() -> Self {
        Self {
            completed: Arc::new(Mutex::new(Vec::new())),
            read_order: Arc::new(Mutex::new(Vec::new())),
            journal: None,
            records: Arc::new(Mutex::new(Vec::new())),
            supplement_failure: None,
        }
    }

    fn with_journal(root: &Path) -> Self {
        Self {
            journal: Some(Arc::new(Journal::open(root, "batch-gc"))),
            ..Self::new()
        }
    }

    fn failing_supplement_for(call_id: &str) -> Self {
        Self {
            supplement_failure: Some(call_id.to_owned()),
            ..Self::new()
        }
    }
}

impl Host for ShellHost {
    fn shell(&self, command: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        let command = command.to_owned();
        let completed = self.completed.clone();
        Box::pin(async move {
            let parts: Vec<&str> = command.split_whitespace().collect();
            if parts.len() == 4 && (parts[0] == "emit" || parts[0] == "fail") {
                let delay: u64 = parts[1].parse().map_err(|_| "bad delay")?;
                let index: usize = parts[2].parse().map_err(|_| "bad index")?;
                let size: usize = parts[3].parse().map_err(|_| "bad size")?;
                tokio::time::sleep(Duration::from_millis(delay)).await;
                let body = body_sized(index, size);
                completed
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(index);
                if parts[0] == "fail" {
                    return Err(body);
                }
                return Ok(body);
            }
            Ok(String::new())
        })
    }

    fn on_event(&self, _event: AgentEvent) {}

    fn on_read_observation(&self, observation: &ReadObservation) {
        self.read_order
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(observation.path.clone());
    }

    fn on_tool_call_lifecycle(
        &self,
        call: &mio_coding_agent::tool_lifecycle::ToolCallRecord,
    ) -> Result<(), String> {
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(call.clone());
        if let Some(failing) = &self.supplement_failure
            && call
                .output_references
                .iter()
                .any(|reference| reference.call_id.as_deref() == Some(failing.as_str()))
        {
            // Recorded first: this models a Host that failed after partially
            // persisting, so the caller must conservatively keep the file.
            return Err("host refused to persist output references".into());
        }
        if let Some(journal) = &self.journal {
            journal.record_canonical_phase(call)?;
        }
        Ok(())
    }
}

/// One SSE `data:` event.
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

/// Serve `responses` in order on a loopback endpoint. Extra connections are
/// granted a bounded wait, so a test whose run stops after one request does not
/// hang.
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

fn marker(index: usize) -> String {
    format!("MARK-{index}-start")
}

/// A many-line UTF-8 file whose first line identifies the call that reads it.
fn write_file(path: &Path, first_line: &str, bytes: usize) {
    let mut content = String::with_capacity(bytes + 64);
    content.push_str(first_line);
    content.push('\n');
    let mut index = 0usize;
    while content.len() < bytes {
        content.push_str(&format!("line {index:06} {}\n", "payload-".repeat(8)));
        index += 1;
    }
    std::fs::write(path, content).unwrap();
}

fn read_turn() -> String {
    let mut events = Vec::new();
    events.push(tool_call_chunk(0, "read", json!({"path": "big.txt"})));
    for index in 1..CALLS + 2 {
        events.push(tool_call_chunk(
            index,
            "read",
            json!({"path": format!("small-{index}.txt")}),
        ));
    }
    events.push(chunk(json!({}), json!("tool_calls")));
    sse(events)
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

fn final_turn() -> String {
    sse(vec![
        chunk(json!({"content": "完成"}), Value::Null),
        chunk(json!({}), json!("stop")),
    ])
}

fn tool_messages(request: &Value) -> Vec<Value> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .cloned()
        .collect()
}

#[tokio::test]
async fn concurrent_reverse_order_batch_is_externalized_and_reconstructable() {
    let folder = tempfile::tempdir().unwrap();
    let canonical = folder.path().canonicalize().unwrap();
    // One slow read (a multi-MiB file is read whole before paging) and five
    // fast reads: the tool calls run concurrently, so the slow read completes
    // last even though it was emitted first.
    write_file(&canonical.join("big.txt"), "MARK-0-start", 7 * 1024 * 1024);
    for index in 1..CALLS + 2 {
        write_file(
            &canonical.join(format!("small-{index}.txt")),
            &format!("MARK-{index}-start"),
            10 * 1024,
        );
    }

    let (base_url, server) = spawn_server(vec![read_turn(), final_turn()]);
    let host = Arc::new(ShellHost::new());
    let cfg = config(base_url);
    let run_options = RunOptions {
        tool_concurrency: CALLS + 2,
        ..RunOptions::default()
    };
    let limit = ContextBudget::resolve(&cfg, &run_options)
        .unwrap()
        .input_limit();

    let history = run_with_options(
        cfg,
        Vec::new(),
        "run the batch",
        &canonical,
        host.clone(),
        Services::default(),
        run_options,
    )
    .await
    .expect("the batch is externalized instead of overflowing the request");
    assert!(!history.is_empty());

    let requests = server.join().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "one tool turn must be followed by exactly one more model request"
    );

    // The reads really ran concurrently and completed out of call order: the
    // 7 MiB read (call 0) finishes after the 10 KiB reads.
    let observed = host.read_order.lock().unwrap().clone();
    assert_eq!(observed.len(), CALLS + 2, "{observed:?}");
    assert_eq!(
        observed.last().map(String::as_str),
        Some("big.txt"),
        "the slow first call completes last: {observed:?}"
    );
    assert_ne!(observed.first().map(String::as_str), Some("big.txt"));

    let tools = tool_messages(&requests[1]);
    assert_eq!(
        tools.len(),
        CALLS + 2,
        "every tool result keeps its pairing"
    );
    for (index, message) in tools.iter().enumerate() {
        assert_eq!(
            message["tool_call_id"],
            json!(format!("call_{index}")),
            "tool results stay in call order despite out-of-order completion"
        );
    }

    // The six bounded read pages alone would exceed the estimated input limit;
    // after externalization the whole message array fits.
    let encoded = serde_json::to_vec(&requests[1]["messages"]).unwrap();
    assert!(
        encoded.len() < limit,
        "the batched request {} must fit the {limit}-byte input limit",
        encoded.len()
    );
    let inline: usize = tools
        .iter()
        .filter_map(|message| message["content"].as_str())
        .map(str::len)
        .sum();
    assert!(
        inline < 6 * (limit / 4),
        "the inline batch {inline} must be smaller than the six raw pages"
    );

    let mut externalized = 0usize;
    for (index, message) in tools.iter().enumerate() {
        let content = message["content"]
            .as_str()
            .expect("a read result is a string");
        let value: Value = serde_json::from_str(content).expect("the tool content is JSON");
        if value.get("output_references").is_none() {
            // Under the batch budget a result may stay inline; it must then
            // still be the real (bounded) page.
            assert!(value["output"].as_str().unwrap().contains(&marker(index)));
            continue;
        }
        externalized += 1;
        let reference = &value["output_references"][0];
        assert_eq!(reference["call_id"], json!(format!("call_{index}")));
        let path = reference["path"].as_str().unwrap();
        let name = path.strip_prefix(".mio/outputs/").unwrap();
        let bytes = std::fs::read(canonical.join(".mio/outputs").join(name))
            .expect("externalized file exists");
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        assert_eq!(
            reference["sha256"],
            json!(format!("{:x}", hasher.finalize()))
        );
        let full: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            full["output"].as_str().unwrap().contains(&marker(index)),
            "the externalized body belongs to call {index}"
        );
        assert_eq!(reference["total_bytes"], json!(bytes.len()));

        // The model can rebuild the exact bytes through `read` byte paging.
        let reader = Workspace::new(&canonical, Arc::new(ShellHost::new())).unwrap();
        let mut collected = Vec::new();
        let mut offset = 0u64;
        loop {
            let page = reader
                .execute(Arguments {
                    operation: "read".into(),
                    path: Some(path.to_owned()),
                    byte_offset: Some(offset),
                    byte_limit: Some(1_024),
                    ..Arguments::default()
                })
                .await
                .expect("byte paging the externalized file");
            let page: Value = serde_json::from_str(&page).unwrap();
            collected.extend_from_slice(page["text"].as_str().unwrap().as_bytes());
            match page["next_byte_offset"].as_u64() {
                Some(next) => offset = next,
                None => break,
            }
        }
        assert_eq!(collected, bytes, "byte paging reconstructs the exact body");
    }
    assert!(externalized > 0, "an over-budget batch externalizes bodies");

    // The canonical tracker carries the same typed references and keeps its
    // terminal phase.
    let records = host.records.lock().unwrap().clone();
    assert!(
        records
            .iter()
            .any(|record| !record.output_references.is_empty()),
        "a canonical call carries an externalized reference"
    );
    for record in &records {
        if !record.output_references.is_empty() {
            assert_eq!(
                record.phase,
                mio_coding_agent::tool_lifecycle::ToolCallPhase::Executed
            );
        }
    }
}
#[tokio::test]
async fn irreducible_request_makes_no_provider_request() {
    let folder = tempfile::tempdir().unwrap();
    let canonical = folder.path().canonicalize().unwrap();
    let (base_url, server) = spawn_server(vec![final_turn()]);
    let host = Arc::new(ShellHost::new());
    let huge_prompt = "x".repeat(60_000);

    let error = run_with_options(
        config(base_url),
        Vec::new(),
        &huge_prompt,
        &canonical,
        host,
        Services::default(),
        RunOptions::default(),
    )
    .await
    .expect_err("a prompt that cannot fit must stop the run");
    assert!(
        error.starts_with("Context budget:"),
        "the refusal keeps its budget diagnostic: {error}"
    );
    let requests = server.join().unwrap();
    assert!(
        requests.is_empty(),
        "an irreducible request must not reach the provider; got {}",
        requests.len()
    );
}

#[tokio::test]
async fn output_store_failure_stops_before_the_next_provider_request() {
    let folder = tempfile::tempdir().unwrap();
    let canonical = folder.path().canonicalize().unwrap();
    // `.mio` as a regular file makes the output store fail closed.
    std::fs::write(canonical.join(".mio"), b"not a directory").unwrap();
    let (base_url, server) = spawn_server(vec![tool_turn(), final_turn()]);
    let host = Arc::new(ShellHost::new());

    let error = run_with_options(
        config(base_url),
        Vec::new(),
        "run the batch",
        &canonical,
        host,
        Services::default(),
        options(),
    )
    .await
    .expect_err("a failed externalization must stop the run");
    assert!(
        error.starts_with("Context budget:"),
        "the failure keeps its budget diagnostic: {error}"
    );
    let requests = server.join().unwrap();
    assert_eq!(
        requests.len(),
        1,
        "no request is sent after the externalization failure; got {}",
        requests.len()
    );
}

#[tokio::test]
async fn canonical_reference_protects_the_externalized_file_from_cleanup() {
    let folder = tempfile::tempdir().unwrap();
    let canonical = folder.path().canonicalize().unwrap();
    let (base_url, server) = spawn_server(vec![tool_turn(), final_turn()]);
    let host = Arc::new(ShellHost::with_journal(&canonical));
    let journal = host.journal.clone().unwrap();
    journal.begin_run().unwrap();

    run_with_options(
        config(base_url),
        Vec::new(),
        "run the batch",
        &canonical,
        host.clone(),
        Services::default(),
        options(),
    )
    .await
    .expect("the batch is externalized");
    let _ = server.join().unwrap();
    journal.finish_run(RunState::Completed).unwrap();

    let externalized: Vec<String> = host
        .records
        .lock()
        .unwrap()
        .iter()
        .flat_map(|record| record.output_references.iter())
        .map(|reference| reference.path.clone())
        .collect();
    assert!(!externalized.is_empty(), "the run externalized outputs");

    let store = OutputStore::open(&canonical).unwrap();
    // Far in the future makes every fresh output stale; only a real reference
    // keeps it.
    let plan = store
        .plan_cleanup(SystemTime::now() + Duration::from_secs(30 * 24 * 60 * 60))
        .unwrap();
    for path in &externalized {
        assert!(
            plan.retained_referenced.iter().any(|kept| kept == path),
            "{path} must be retained because the canonical journal references it: {plan:?}"
        );
    }
}

/// A batch of four `bash` calls where call 1 fails: three successes and one real
/// error result, all completing in reverse call order.
fn mixed_turn() -> String {
    let mut events = Vec::new();
    for index in 0..CALLS {
        let delay = (CALLS - index) as u64 * 60;
        let verb = if index == 1 { "fail" } else { "emit" };
        events.push(tool_call_chunk(
            index,
            "bash",
            json!({"command": format!("{verb} {delay} {index} {TAG_BYTES}")}),
        ));
    }
    events.push(chunk(json!({}), json!("tool_calls")));
    sse(events)
}

/// A high-fanout batch of concurrent `read` calls.
fn high_fanout_read_turn(calls: usize) -> String {
    let mut events = Vec::new();
    for index in 0..calls {
        events.push(tool_call_chunk(
            index,
            "read",
            json!({"path": format!("fan-{index}.txt")}),
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

/// 24 concurrent results whose inline bodies cannot share a fixed per-item
/// preview budget: the minimal reference metadata fits, so the batch must be
/// externalized and the remaining room distributed as one shared UTF-8 preview
/// cap rather than stopped.
#[tokio::test]
async fn high_fanout_batch_is_externalized_when_only_the_metadata_fits() {
    const FANOUT: usize = 24;
    const FAN_BYTES: usize = 4_000;
    let folder = tempfile::tempdir().unwrap();
    let canonical = folder.path().canonicalize().unwrap();
    for index in 0..FANOUT {
        write_file(
            &canonical.join(format!("fan-{index}.txt")),
            &format!("FAN-{index}-start"),
            FAN_BYTES,
        );
    }
    let (base_url, server) = spawn_server(vec![high_fanout_read_turn(FANOUT), final_turn()]);
    let host = Arc::new(ShellHost::new());
    let cfg = config(base_url);
    let run_options = RunOptions {
        tool_concurrency: FANOUT,
        ..RunOptions::default()
    };
    let limit = ContextBudget::resolve(&cfg, &run_options)
        .unwrap()
        .input_limit();
    assert!(
        FANOUT * FAN_BYTES > limit,
        "the raw batch {FANOUT}x{FAN_BYTES} must exceed the {limit}-byte input limit"
    );
    let history = run_with_options(
        cfg,
        Vec::new(),
        "run the fanout",
        &canonical,
        host.clone(),
        Services::default(),
        run_options,
    )
    .await
    .expect("a high-fanout batch whose metadata fits must not be stopped");
    assert!(!history.is_empty());
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    let tools = tool_messages(&requests[1]);
    assert_eq!(tools.len(), FANOUT);
    let encoded = serde_json::to_vec(&requests[1]["messages"]).unwrap();
    assert!(
        encoded.len() < limit,
        "the batched request {} must fit the {limit}-byte input limit",
        encoded.len()
    );
    let mut previews = Vec::new();
    for (index, message) in tools.iter().enumerate() {
        assert_eq!(message["tool_call_id"], json!(format!("call_{index}")));
        let content = message["content"]
            .as_str()
            .expect("a read result is a string");
        let value: Value = serde_json::from_str(content).expect("the tool content is JSON");
        let reference = &value["output_references"][0];
        assert_eq!(reference["call_id"], json!(format!("call_{index}")));
        let name = reference["path"]
            .as_str()
            .unwrap()
            .strip_prefix(".mio/outputs/")
            .unwrap();
        let bytes = std::fs::read(canonical.join(".mio/outputs").join(name))
            .expect("externalized file exists");
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        assert_eq!(
            reference["sha256"],
            json!(format!("{:x}", hasher.finalize()))
        );
        assert_eq!(reference["total_bytes"], json!(bytes.len()));
        let full: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            full["output"]
                .as_str()
                .unwrap()
                .contains(&format!("FAN-{index}-start")),
            "the externalized body belongs to call {index}"
        );
        previews.push(value["output"].as_str().unwrap().len());
    }
    assert!(
        previews.iter().all(|&len| len > 0),
        "the remaining room is distributed as real preview: {previews:?}"
    );
    assert!(
        previews.iter().all(|&len| len == previews[0]),
        "one shared UTF-8 preview cap for the whole batch: {previews:?}"
    );
    let records = host.records.lock().unwrap().clone();
    let referenced = records
        .iter()
        .filter(|record| !record.output_references.is_empty())
        .count();
    assert_eq!(
        referenced, FANOUT,
        "every externalized result is protected by a real canonical reference"
    );
}

/// A real loopback batch where one call fails: order, error semantics and full
/// externalized bytes are all preserved through the production hook wiring.
#[tokio::test]
async fn loopback_error_result_is_externalized_with_real_bytes() {
    let folder = tempfile::tempdir().unwrap();
    let canonical = folder.path().canonicalize().unwrap();
    let (base_url, server) = spawn_server(vec![mixed_turn(), final_turn()]);
    let host = Arc::new(ShellHost::new());
    let cfg = config(base_url);
    let limit = ContextBudget::resolve(&cfg, &options())
        .unwrap()
        .input_limit();
    run_with_options(
        cfg,
        Vec::new(),
        "run the mixed batch",
        &canonical,
        host.clone(),
        Services::default(),
        options(),
    )
    .await
    .expect("a failed tool result is reported to the model, not a run failure");
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    let tools = tool_messages(&requests[1]);
    assert_eq!(tools.len(), CALLS, "every tool result keeps its pairing");
    for (index, message) in tools.iter().enumerate() {
        assert_eq!(
            message["tool_call_id"],
            json!(format!("call_{index}")),
            "tool results stay in call order despite out-of-order completion"
        );
    }
    let encoded = serde_json::to_vec(&requests[1]["messages"]).unwrap();
    assert!(
        encoded.len() < limit,
        "the batched request {} must fit the {limit}-byte input limit",
        encoded.len()
    );
    let observed = host.completed.lock().unwrap().clone();
    let mut observed_sorted = observed.clone();
    observed_sorted.sort_unstable();
    assert_eq!(
        observed_sorted,
        (0..CALLS).collect::<Vec<_>>(),
        "every call really completed: {observed:?}"
    );
    let records = host.records.lock().unwrap().clone();
    for index in 0..CALLS {
        let record = records
            .iter()
            .rev()
            .find(|record| record.tool_call_id.as_deref() == Some(&format!("call_{index}")))
            .unwrap_or_else(|| panic!("canonical record for call_{index}"));
        assert_eq!(
            record.output_references.len(),
            1,
            "call_{index} is externalized with its own reference"
        );
        let reference = &record.output_references[0];
        assert_eq!(
            reference.call_id.as_deref(),
            Some(format!("call_{index}").as_str())
        );
        let name = reference.path.strip_prefix(".mio/outputs/").unwrap();
        let bytes = std::fs::read(canonical.join(".mio/outputs").join(name))
            .expect("externalized file exists");
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        assert_eq!(reference.sha256, format!("{:x}", hasher.finalize()));
        assert_eq!(reference.total_bytes, bytes.len() as u64);
        if index == 1 {
            assert!(
                String::from_utf8_lossy(&bytes).contains(&tag(1)),
                "the failed call's full error body is externalized"
            );
        } else {
            assert!(
                String::from_utf8_lossy(&bytes).contains(&body(index)),
                "the successful call's full body is externalized verbatim"
            );
        }
    }
}

/// A Host canonical-supplement failure must not delete files already truly
/// referenced, must conservatively keep the uncertain registered files, and must
/// send no further provider request.
#[tokio::test]
async fn failed_canonical_supplement_keeps_files_and_sends_no_request() {
    let folder = tempfile::tempdir().unwrap();
    let canonical = folder.path().canonicalize().unwrap();
    let (base_url, server) = spawn_server(vec![supplement_failure_turn(), final_turn()]);
    let host = Arc::new(ShellHost::failing_supplement_for("call_1"));
    let error = run_with_options(
        config(base_url),
        Vec::new(),
        "run the batch",
        &canonical,
        host.clone(),
        Services::default(),
        options(),
    )
    .await
    .expect_err("a Host supplement failure stops the run");
    assert!(
        error.starts_with("Context budget:"),
        "the failure keeps its budget diagnostic: {error}"
    );
    assert!(
        error.contains("may have partially persisted"),
        "the kept-file boundary is reported explicitly: {error}"
    );
    let requests = server.join().unwrap();
    assert_eq!(
        requests.len(),
        1,
        "no provider request follows the failed externalization"
    );
    let records = host.records.lock().unwrap().clone();
    let first = records
        .iter()
        .rev()
        .find(|record| record.tool_call_id.as_deref() == Some("call_0"))
        .expect("call_0 was persisted before call_1 failed");
    assert!(
        !first.output_references.is_empty(),
        "call_0 is truly referenced and must survive"
    );
    for reference in &first.output_references {
        let name = reference.path.strip_prefix(".mio/outputs/").unwrap();
        assert!(
            canonical.join(".mio/outputs").join(name).exists(),
            "an already referenced file is never deleted by a later call's failure"
        );
    }
    let store = OutputStore::open(&canonical).unwrap();
    let index = store.load_index().unwrap();
    assert_eq!(
        index.entries.len(),
        2,
        "call_0's referenced file and call_1's uncertain file are both retained"
    );
}
