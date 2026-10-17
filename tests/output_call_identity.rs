//! DES-12 / DES-15: the tool call id an externalized output is filed under.
//!
//! These tests drive the real engine loop (`run_with_options`) against a local
//! loopback mock provider. No external provider, credential or network is
//! touched: the requests carry only the fixed `local-test` token this file sets
//! on `Config`.
//!
//! The first case is the real wiring, end to end:
//!
//! - the mock model returns a real `bash` tool call with a known provider
//!   `toolCallId`;
//! - `Host::shell` really calls `shell::execute`, so the capture path
//!   (`CaptureSession` -> `OutputStore::register`) runs while the engine's own
//!   `mio_agent::agent::ToolInvocation` is bound to the executing task by
//!   `ScopedTool`;
//! - the command emits more than 32 KiB (and less than the 8 MiB hard bound) of
//!   generic text, so it is externalized under `.mio/outputs/`;
//! - the model's second request really carries the workspace-relative file
//!   reference, not the whole payload, both as the legacy text marker and as a
//!   structured `{"output_references": [...]}` JSON content block appended
//!   after the real tool result.
//!
//! It then asserts, from real persisted artifacts rather than a hand-built
//! `ToolIdentity`:
//!
//! - the versioned index entry's `call_id` equals the provider's own
//!   `toolCallId` -- not the dispatcher's effect id and not the approval
//!   attempt's `callId`;
//! - a real read-only `Workspace` reconstructs the artifact through byte
//!   paging, and the rebuilt bytes equal the original output exactly;
//! - the indexed byte count and content hash match the file on disk.
//!
//! The second case is the honest contrast: the direct, engine-less
//! `externalize_output` API binds no scope, so its index entry stays
//! `call_id == None` instead of fabricating one.

#![cfg(unix)]

#[path = "../src/test_http.rs"]
mod test_http;

use mio_coding_agent::{
    AgentEvent, Arguments, Config, Host, HostFuture, Provider, RunObservation, RunObservationKind,
    RunOptions, Services, Workspace, policy::ApprovalBinding, run_with_options,
    shell::output_store::OutputStore,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// The fixed test credential. The mock peer is local, so this is the only
/// token in play, and nothing in the workspace may ever contain it.
const API_KEY: &str = "local-test";

/// The model's own tool-call id, chosen by the mock provider. The index entry
/// must be filed under exactly this string.
const PROVIDER_CALL_ID: &str = "call-mio-external-output-1";

// ---------------------------------------------------------------------------
// Local loopback mock provider
// ---------------------------------------------------------------------------

struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    fn sse(body: String) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream",
            body: body.into_bytes(),
        }
    }
}

/// A scripted, sequential loopback provider. It records every JSON request body
/// so the test can assert what the model actually saw, and it stops and joins
/// its serving thread on drop so no background thread outlives the test.
struct MockServer {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockServer {
    fn start(responses: Vec<Response>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let captured = Arc::clone(&requests);
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            let mut index = 0;
            while !thread_stop.load(Ordering::SeqCst)
                && Instant::now() < deadline
                && index < responses.len()
            {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        socket
                            .set_read_timeout(Some(Duration::from_secs(10)))
                            .unwrap();
                        socket
                            .set_write_timeout(Some(Duration::from_secs(10)))
                            .unwrap();
                        let (_, body) = test_http::read_json(&mut socket);
                        captured.lock().unwrap().push(body);
                        write_response(&mut socket, &responses[index]);
                        index += 1;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("mock server accept failed: {error}"),
                }
            }
        });
        Self {
            address,
            requests,
            stop,
            handle: Some(handle),
        }
    }

    /// Every JSON request body the mock received, in arrival order.
    fn bodies(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn write_response(socket: &mut TcpStream, response: &Response) {
    let head = format!(
        "HTTP/1.1 {} OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        response.content_type,
        response.body.len()
    );
    socket.write_all(head.as_bytes()).unwrap();
    socket.write_all(&response.body).unwrap();
    socket.flush().unwrap();
}

fn openai_stream(id: &str, text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "test",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": "stop"}]
        })
    )
}

fn openai_tool_stream(id: &str, tool_name: &str, calls: &[(&str, Value)]) -> String {
    let tool_calls = calls
        .iter()
        .enumerate()
        .map(|(index, (call_id, arguments))| {
            json!({
                "index": index,
                "id": call_id,
                "type": "function",
                "function": {"name": tool_name, "arguments": arguments.to_string()}
            })
        })
        .collect::<Vec<_>>();
    let frames = [
        json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "test",
            "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": tool_calls}, "finish_reason": null}]
        })
        .to_string(),
        json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "test",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
        })
        .to_string(),
    ];
    frames
        .iter()
        .map(|frame| format!("data: {frame}"))
        .collect::<Vec<_>>()
        .join("\n\n")
        + "\n\ndata: [DONE]\n\n"
}

fn config(address: SocketAddr) -> Config {
    Config {
        provider: Provider::Custom,
        base_url: format!("http://{address}/v1"),
        model: "test".into(),
        api_key: API_KEY.into(),
    }
}

// ---------------------------------------------------------------------------
// Fixture and hosts
// ---------------------------------------------------------------------------

/// A model-agnostic text payload: deterministic, mixed ASCII and multi-byte
/// UTF-8 so byte paging has to respect character boundaries. It is larger than
/// the 8 KiB inline limit and far below the 8 MiB complete-output bound.
fn generic_text_payload() -> String {
    let mut payload = String::new();
    for index in 0..1200 {
        payload.push_str(&format!(
            "mio-output-line-{index:04}-通用文本-abcdefghijklmnopqrstuvwxyz0123456789\n"
        ));
    }
    payload
}

/// A host that runs the real capture path. `shell` calls `shell::execute`
/// directly, so the index is written while the engine's tool identity scope is
/// active on the executing task.
#[derive(Default)]
struct CaptureHost {
    approvals: Mutex<Vec<ApprovalBinding>>,
    observations: Mutex<Vec<RunObservation>>,
    shell_commands: Mutex<Vec<String>>,
}

impl CaptureHost {
    /// The real dispatch observation for one provider tool-call id.
    fn dispatched(&self, call_id: &str) -> Option<RunObservation> {
        self.observations
            .lock()
            .unwrap()
            .iter()
            .find(|observation| {
                observation.kind == RunObservationKind::ToolDispatched
                    && observation.tool_call_id.as_deref() == Some(call_id)
            })
            .cloned()
    }
}

impl Host for CaptureHost {
    fn shell(&self, command: &str, cwd: &Path) -> HostFuture<Result<String, String>> {
        self.shell_commands.lock().unwrap().push(command.to_owned());
        let command = command.to_owned();
        let cwd = cwd.to_path_buf();
        Box::pin(async move { mio_coding_agent::shell::execute(&command, &cwd).await })
    }

    fn on_event(&self, _event: AgentEvent) {}

    fn on_run_observation(&self, observation: &RunObservation) {
        self.observations.lock().unwrap().push(observation.clone());
    }

    fn approve_tool_binding(
        &self,
        _operation: &str,
        _target: &str,
        binding: &ApprovalBinding,
    ) -> HostFuture<Result<(), String>> {
        self.approvals.lock().unwrap().push(binding.clone());
        Box::pin(async { Ok(()) })
    }
}

/// A host that must never be asked to run a command during the byte-paging
/// reconstruction.
struct NoShell;

impl Host for NoShell {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { panic!("byte paging must not execute shell") })
    }

    fn on_event(&self, _event: AgentEvent) {}
}

/// Every regular file below `root`, following no symlinks.
fn workspace_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file() {
                files.push(path);
            }
        }
    }
    files
}

/// The trailing `{"output_references": [...]}` content block the real capture
/// appended to the tool result, read from the exact JSON the mock provider got.
///
/// The OpenAI-compatible wire encodes each content block as a text part and
/// either joins them with `\n` into one string or keeps an array, so both forms
/// are accepted; the structured block is the one after the real
/// `{"output": ...}` JSON.
fn output_references_from_request(request: &Value) -> Value {
    let messages = request["messages"]
        .as_array()
        .expect("the request must carry messages");
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        let Some(content) = message.get("content") else {
            continue;
        };
        let mut texts: Vec<&str> = Vec::new();
        match content {
            Value::String(text) => texts.push(text),
            Value::Array(parts) => {
                for part in parts {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        texts.push(text);
                    }
                }
            }
            _ => {}
        }
        for text in texts {
            if let Some((_, rest)) = text.split_once("{\"output_references\"") {
                let candidate = format!("{{\"output_references\"{rest}");
                if let Ok(value) = serde_json::from_str::<Value>(&candidate) {
                    return value;
                }
            }
        }
    }
    panic!("the second request must carry a real output_references block: {request}");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A real engine `bash` call externalizes a >32 KiB output under the provider's
/// own `toolCallId`, and a real `Workspace` rebuilds the exact bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_bash_output_is_indexed_under_the_provider_call_id() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let payload = generic_text_payload();
    assert!(
        payload.len() > 32 * 1024 && payload.len() < 8 * 1024 * 1024,
        "the fixture must be externalized but stay within the hard bound: {} bytes",
        payload.len()
    );
    // The command emits exactly the fixture bytes, so the original output is
    // known independently of the engine and can be checked byte for byte.
    std::fs::write(cwd.join("payload.txt"), payload.as_bytes()).unwrap();

    let server = MockServer::start(vec![
        Response::sse(openai_tool_stream(
            "turn-1",
            "bash",
            &[(PROVIDER_CALL_ID, json!({"command": "cat payload.txt"}))],
        )),
        Response::sse(openai_stream("turn-2", "done")),
    ]);

    let host = Arc::new(CaptureHost::default());
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        run_with_options(
            config(server.address),
            Vec::new(),
            "外部化一次大输出",
            &cwd,
            host.clone(),
            Services::default(),
            RunOptions::default(),
        ),
    )
    .await
    .expect("the run must settle within the bounded test timeout");
    assert!(result.is_ok(), "run failed: {result:?}");

    // The host really executed the command through `shell::execute`.
    let shell_commands = host.shell_commands.lock().unwrap().clone();
    assert_eq!(
        shell_commands,
        vec!["cat payload.txt".to_owned()],
        "Host::shell must run the real capture path exactly once"
    );

    // The model's second request carries the externalized reference, not the
    // full payload.
    let bodies = server.bodies();
    assert_eq!(
        bodies.len(),
        2,
        "one tool turn plus one final turn: {bodies:?}"
    );
    let second_request = serde_json::to_string(&bodies[1]).unwrap();
    assert!(
        second_request.contains("TRUNCATED"),
        "the second request must show the truncation notice: {second_request}"
    );
    assert!(
        second_request.contains(".mio/outputs/"),
        "the second request must carry the workspace-relative reference: {second_request}"
    );
    assert!(
        second_request.len() < payload.len(),
        "the model must not receive the whole payload ({} vs {})",
        second_request.len(),
        payload.len()
    );

    // The persisted index is the product of the real capture, not a hand-made
    // identity: its `call_id` is the provider's own tool-call id.
    let index = OutputStore::open(cwd.as_path())
        .unwrap()
        .load_index()
        .unwrap();
    assert_eq!(index.entries.len(), 1, "{index:?}");
    let entry = &index.entries[0];
    assert_eq!(entry.stream, "stdout");
    assert_eq!(
        entry.call_id.as_deref(),
        Some(PROVIDER_CALL_ID),
        "the indexed call id must be the provider toolCallId, not a derived id"
    );
    assert!(
        second_request.contains(&entry.path),
        "the second request must name the exact artifact path `{}`",
        entry.path
    );

    // It is neither the dispatcher's effect id nor the approval attempt id.
    let dispatch = host
        .dispatched(PROVIDER_CALL_ID)
        .expect("the real dispatch must be observed");
    let effect_id = dispatch
        .effect_id
        .expect("a real dispatch has an effect id");
    assert_eq!(
        dispatch.tool_call_id.as_deref(),
        Some(PROVIDER_CALL_ID),
        "sanity: the dispatch carries the provider id"
    );
    let effect_id_text = effect_id.to_string();
    assert_ne!(
        entry.call_id.as_deref(),
        Some(effect_id_text.as_str()),
        "the index must never be filed under the effect id"
    );

    let approvals = host.approvals.lock().unwrap().clone();
    let approval = approvals
        .iter()
        .find(|binding| binding.tool_call_id.as_deref() == Some(PROVIDER_CALL_ID))
        .expect("the bash call must have a bound approval");
    assert_ne!(
        approval.call_id, PROVIDER_CALL_ID,
        "the approval attempt id must be distinct from the provider id"
    );
    assert_ne!(
        entry.call_id.as_deref(),
        Some(approval.call_id.as_str()),
        "the index must never be filed under the approval attempt id"
    );

    // The indexed bytes and hash describe the real file on disk.
    let artifact = cwd.join(&entry.path);
    let stored = std::fs::read(&artifact).unwrap();
    assert_eq!(
        entry.bytes,
        Some(stored.len() as u64),
        "the indexed byte count must match the file"
    );
    let stored_hash = format!("{:x}", Sha256::digest(&stored));
    assert_eq!(
        entry.hash.as_deref(),
        Some(stored_hash.as_str()),
        "the indexed hash must match the file"
    );
    assert_eq!(
        stored,
        payload.as_bytes(),
        "the artifact is the original output"
    );

    // The model's second request carries the real typed reference as a
    // structured JSON content block, not just the legacy text marker, and the
    // reference fields describe the file that is actually on disk.
    let references = output_references_from_request(&bodies[1]);
    let entries = references["output_references"]
        .as_array()
        .expect("output_references must be an array");
    assert_eq!(entries.len(), 1, "{references}");
    let reference = &entries[0];
    assert_eq!(
        reference["path"].as_str(),
        Some(entry.path.as_str()),
        "the reference path must be the real artifact: {references}"
    );
    assert_eq!(
        reference["total_bytes"].as_u64(),
        Some(stored.len() as u64),
        "the reference byte count must match the file on disk: {references}"
    );
    assert_eq!(
        reference["stream"].as_str(),
        Some("stdout"),
        "the reference must name the real stream: {references}"
    );
    assert_eq!(
        reference["call_id"].as_str(),
        Some(PROVIDER_CALL_ID),
        "the reference must carry the provider's own tool-call id: {references}"
    );
    assert_eq!(
        reference["sha256"].as_str(),
        Some(stored_hash.as_str()),
        "the reference hash must match the file on disk: {references}"
    );

    // A real read-only Workspace rebuilds the file through byte paging.
    let workspace = Workspace::new_read_only(&cwd, Arc::new(NoShell)).unwrap();
    let mut offset = 0u64;
    let mut reconstructed = String::new();
    loop {
        let mut args = Arguments::read(entry.path.clone());
        args.byte_offset = Some(offset);
        args.byte_limit = Some(8192);
        let output = workspace.execute(args).await.unwrap();
        let page: Value = serde_json::from_str(&output).unwrap();
        reconstructed.push_str(page["text"].as_str().unwrap());
        match page["next_byte_offset"].as_u64() {
            Some(next) => {
                assert!(next > offset, "byte paging must make progress");
                offset = next;
            }
            None => break,
        }
    }
    assert_eq!(
        reconstructed.as_bytes(),
        payload.as_bytes(),
        "byte paging must rebuild the exact original bytes"
    );

    // The fixed test credential is never persisted anywhere in the workspace.
    let needle = API_KEY.as_bytes();
    for file in workspace_files(&cwd) {
        let bytes = std::fs::read(&file).unwrap();
        assert!(
            !bytes.windows(needle.len()).any(|window| window == needle),
            "the test credential must not leak into {}",
            file.display()
        );
    }
}

/// Without an engine scope the direct API stays honestly unknown: no run, no
/// dispatch and no approval exist, so the entry's `call_id` is `None`.
#[tokio::test]
async fn direct_externalize_without_engine_scope_stays_null() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let payload = generic_text_payload();
    assert!(payload.len() > 32 * 1024);

    let preview = mio_coding_agent::shell::externalize_output(&payload, &root)
        .await
        .unwrap();
    assert!(preview.contains(".mio/outputs/"));

    assert!(
        mio_coding_agent::tool_identity::current().is_none(),
        "a direct API call must not bind an engine identity"
    );

    let index = OutputStore::open(root.as_path())
        .unwrap()
        .load_index()
        .unwrap();
    assert_eq!(index.entries.len(), 1, "{index:?}");
    assert_eq!(
        index.entries[0].call_id, None,
        "an unknown call id must stay null, never fabricated"
    );
}
