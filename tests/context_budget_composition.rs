//! Composed context planning, end to end over a real loopback provider.
//!
//! The R8 gap: the request-level `fit` only summarizes, and the dispatch
//! boundary only externalizes. A run whose retained, non-externalizable history
//! is already near the input limit and whose newest tool result is larger than
//! the remaining room used to be irreducible: no single mechanism could serve
//! it. This test drives the real loopback loop and pins that
//!
//! - the run completes instead of stopping on a needless budget refusal;
//! - the final provider request fits the frozen input estimate and carries a
//!   typed `output_references` block for the newest call;
//! - the old, non-externalizable assistant prefix was summarized away;
//! - the externalized body reconstructs byte-for-byte through the real `read`
//!   byte-paging tool;
//! - the canonical tracker associates the reference with its own call.
//!
//! No credential or external network is involved; the mock provider serves the
//! three canned turns.

use mio_coding_agent::{
    AgentEvent, Arguments, Config, Host, HostFuture, Provider, RunOptions, Services, Workspace,
    context_budget::ContextBudget, run_with_options,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    net::TcpListener,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

#[path = "../src/test_http.rs"]
mod test_http;

/// Bytes of the summarizable, non-externalizable assistant prefix. It must be
/// large enough that externalizing the newest result alone still overflows, and
/// small enough that the previous turn's request fits without compaction; the
/// window is a few hundred bytes wide, so the assertion below pins it.
///
/// The current default tool schemas and runtime note leave a narrow synthetic
/// window. 17_450 bytes keeps the preceding request admissible after adding
/// the preview-bound edit hash fields to both tool schemas, while still
/// forcing the composed summary-plus-externalization plan:
/// the previous turn's request fits without compaction, externalizing the
/// newest result alone still overflows, and the original prefix only leaves
/// after the composed plan.
const PREFIX_BYTES: usize = 17_450;
/// Bytes of the newest tool result. Larger than the whole input estimate, so no
/// summary can shrink the prompt and externalization alone is required for it.
const NEWEST_BYTES: usize = 30_000;

fn body_sized(index: usize, size: usize) -> String {
    let mut body = format!("TAG-{index}-payload-");
    while body.len() < size {
        body.push('x');
    }
    body.truncate(size);
    body
}

/// Host that serves the two `emit` commands and records the canonical lifecycle
/// callbacks and the read order.
struct ShellHost {
    records: Arc<Mutex<Vec<mio_coding_agent::tool_lifecycle::ToolCallRecord>>>,
    read_order: Arc<Mutex<Vec<String>>>,
}

impl ShellHost {
    fn new() -> Self {
        Self {
            records: Arc::new(Mutex::new(Vec::new())),
            read_order: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl Host for ShellHost {
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

    fn on_read_observation(&self, observation: &mio_coding_agent::ReadObservation) {
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
        Ok(())
    }
}

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

fn tool_call_chunk(index: usize, id: &str, name: &str, arguments: Value) -> String {
    chunk(
        json!({"tool_calls": [{
            "index": index,
            "id": id,
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

fn spawn_server(responses: Vec<String>) -> (String, std::thread::JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let mut requests = Vec::new();
        let mut served = 0usize;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
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

/// Turn 1: a large non-externalizable assistant text plus one small `bash`.
fn prefix_turn() -> String {
    sse(vec![
        chunk(json!({"content": "A".repeat(PREFIX_BYTES)}), Value::Null),
        tool_call_chunk(0, "call_t1", "bash", json!({"command": "emit 0 0 8"})),
        chunk(json!({}), json!("tool_calls")),
    ])
}

/// Turn 2: one `bash` whose result alone exceeds the input estimate.
fn newest_turn() -> String {
    sse(vec![
        tool_call_chunk(
            0,
            "call_t2",
            "bash",
            json!({"command": format!("emit 0 1 {NEWEST_BYTES}")}),
        ),
        chunk(json!({}), json!("tool_calls")),
    ])
}

fn final_turn() -> String {
    sse(vec![
        chunk(json!({"content": "done"}), Value::Null),
        chunk(json!({}), json!("stop")),
    ])
}

fn message_bytes(request: &Value) -> usize {
    serde_json::to_vec(&request["messages"]).unwrap().len()
}

/// No assistant tool call is missing its result and no result is orphaned.
fn assert_tool_pairs_intact(request: &Value) {
    let mut pending: Vec<String> = Vec::new();
    for message in request["messages"].as_array().unwrap() {
        if let Some(calls) = message["tool_calls"].as_array() {
            for call in calls {
                pending.push(call["id"].as_str().unwrap().to_owned());
            }
        }
        if let Some(id) = message["tool_call_id"].as_str() {
            let position = pending
                .iter()
                .position(|call| call == id)
                .unwrap_or_else(|| panic!("orphan tool result {id}"));
            pending.remove(position);
        }
    }
    assert!(pending.is_empty(), "unanswered tool calls: {pending:?}");
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
async fn composed_summary_and_externalization_serve_a_real_run() {
    let folder = tempfile::tempdir().unwrap();
    let canonical = folder.path().canonicalize().unwrap();
    let (base_url, server) = spawn_server(vec![prefix_turn(), newest_turn(), final_turn()]);
    let host = Arc::new(ShellHost::new());
    let cfg = config(base_url);
    let options = RunOptions {
        tool_concurrency: 4,
        ..RunOptions::default()
    };
    let limit = ContextBudget::resolve(&cfg, &options)
        .unwrap()
        .input_limit();
    assert_eq!(
        limit, 27_033,
        "the frozen fallback input estimate must not move"
    );
    assert!(
        NEWEST_BYTES > limit,
        "the newest result must exceed the whole input estimate"
    );

    let history = run_with_options(
        cfg,
        Vec::new(),
        "go",
        &canonical,
        host.clone(),
        Services::default(),
        options,
    )
    .await
    .expect("the composed plan must serve the request instead of stopping");
    assert!(!history.is_empty());

    let requests = server.join().unwrap();
    assert_eq!(
        requests.len(),
        3,
        "both tool turns must be followed by exactly one more model request"
    );

    // The final request must fit the same estimate the engine enforced, and it
    // must carry the typed reference for the newest call.
    let final_request = &requests[2];
    assert!(
        message_bytes(final_request) < limit,
        "the composed request {} must fit the {limit}-byte estimate",
        message_bytes(final_request)
    );
    // Every exchange in the sent request keeps its pairing: the older call was
    // compacted together with its result, never orphaned.
    assert_tool_pairs_intact(final_request);
    let tools = tool_messages(final_request);
    assert_eq!(
        tools.len(),
        1,
        "the compacted older exchange leaves only the newest result: {tools:?}"
    );
    assert_eq!(tools[0]["tool_call_id"], json!("call_t2"));

    // The old, non-externalizable prefix was summarized: the final request must
    // not carry the full run of `A`s.
    let encoded = serde_json::to_vec(&final_request["messages"]).unwrap();
    let raw_prefix = "A".repeat(PREFIX_BYTES);
    assert!(
        !encoded
            .windows(raw_prefix.len())
            .any(|window| window == raw_prefix.as_bytes()),
        "the old assistant prefix must be summarized away, not carried whole"
    );

    // The newest result was externalized for real: the file reconstructs the
    // exact body and the canonical tracker associates it with its own call.
    let content = tools[0]["content"]
        .as_str()
        .expect("the bash result is a string");
    let value: Value = serde_json::from_str(content).expect("the tool content is JSON");
    let reference = &value["output_references"][0];
    assert_eq!(reference["call_id"], json!("call_t2"));
    let path = reference["path"].as_str().unwrap();
    let name = path.strip_prefix(".mio/outputs/").unwrap();
    let bytes = std::fs::read(canonical.join(".mio/outputs").join(name))
        .expect("the externalized file exists");
    // The shell tool's real body is its native output envelope; the
    // externalized file holds that body verbatim.
    let full: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        full["output"].as_str().unwrap(),
        body_sized(1, NEWEST_BYTES),
        "the externalized body is the newest call's own output"
    );
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    assert_eq!(
        reference["sha256"],
        json!(format!("{:x}", hasher.finalize()))
    );
    assert_eq!(reference["total_bytes"], json!(bytes.len()));

    let records = host.records.lock().unwrap().clone();
    let newest = records
        .iter()
        .rev()
        .find(|record| record.tool_call_id.as_deref() == Some("call_t2"))
        .expect("the canonical record for the newest call");
    assert_eq!(newest.output_references.len(), 1);
    assert_eq!(
        newest.output_references[0].path, path,
        "the canonical record names the same file"
    );
    assert_eq!(
        newest.phase,
        mio_coding_agent::tool_lifecycle::ToolCallPhase::Executed
    );

    // The model can rebuild the exact bytes through the real `read` byte paging
    // tool, which also records the read observation.
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
