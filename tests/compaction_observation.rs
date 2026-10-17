//! End-to-end compaction observation regressions.
//!
//! Every test drives a real `run_with_tools` turn against a loopback HTTP
//! server that carries only the fixed `local-test` token. No external provider
//! is contacted and no real credential is read. The tests assert the honest
//! boundaries of the audit: real whole-history before/after counts and bytes,
//! explicit pinned IDs classified by kind with no entry text, and an explicit
//! refusal instead of a false preserved claim when the pinned block cannot fit.

#[path = "../src/test_http.rs"]
mod test_http;

use mio_coding_agent::compact::{CompactionKind, ContextCompactionObservation, history_bytes};
use mio_coding_agent::context_memory::{PinnedContext, PinnedContextKind};
use mio_coding_agent::{
    AgentEvent, Config, Host, HostFuture, Message, Provider, RunOptions, RunTools, Services,
    ToolFilter, run_with_tools,
};
use serde_json::{Value, json};
use std::{
    io::Write,
    net::{SocketAddr, TcpListener},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Observes the bounded, content-free compaction audits a run emits.
#[derive(Default)]
struct AuditHost {
    observations: Mutex<Vec<ContextCompactionObservation>>,
}

impl Host for AuditHost {
    fn shell(&self, _command: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Err("unexpected shell".into()) })
    }

    fn on_event(&self, _event: AgentEvent) {}

    fn on_context_compaction(
        &self,
        observation: &ContextCompactionObservation,
    ) -> Result<(), String> {
        self.observations.lock().unwrap().push(observation.clone());
        Ok(())
    }
}

/// One-shot loopback provider that records the first real request and answers
/// with a final assistant message.
fn capture_request() -> (SocketAddr, std::thread::JoinHandle<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "no provider request");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
        };
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (header, request) = test_http::read_json(&mut socket);
        assert!(header.starts_with("POST /v1/chat/completions "));
        let chunk = json!({"id":"t","object":"chat.completion.chunk","created":0,"model":"test","choices":[{"index":0,"delta":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]});
        let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
        write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        request
    });
    (address, server)
}

fn config(address: SocketAddr) -> Config {
    Config {
        provider: Provider::Custom,
        base_url: format!("http://{address}/v1"),
        model: "local-test".into(),
        api_key: "local-test".into(),
    }
}

async fn run_turn(
    config: Config,
    history: Vec<Message>,
    host: Arc<AuditHost>,
    cwd: &Path,
    options: RunOptions,
) -> Result<Vec<Message>, String> {
    run_with_tools(
        config,
        history,
        "go",
        cwd,
        host,
        Services::default(),
        RunTools {
            options,
            tool_filter: ToolFilter::none(),
            ..RunTools::default()
        },
    )
    .await
}

/// A request-level compaction keeps the explicit pinned instructions verbatim
/// in the real provider request, reports the whole before/after history and
/// classifies the registry by stable ID without copying any entry text.
#[tokio::test]
async fn request_compaction_keeps_pinned_ids_and_reports_whole_history() {
    let (address, server) = capture_request();
    let pinned_text = format!(
        "head {} MIDDLE-CONSTRAINT {} tail-constraint",
        "界".repeat(60),
        "文".repeat(60)
    );
    let mut pinned = PinnedContext::default();
    pinned
        .insert("mid-tail-rule", PinnedContextKind::Constraint, &pinned_text)
        .unwrap();
    pinned
        .insert(
            "finish-migration",
            PinnedContextKind::Goal,
            "Finish the migration before publishing.",
        )
        .unwrap();
    let mut history = vec![Message::system("run rules")];
    for index in 0..20 {
        history.push(Message::user(format!(
            "history {index} {}",
            "旧".repeat(120)
        )));
        history.push(Message::assistant(format!(
            "finding {index} {}",
            "noise ".repeat(40)
        )));
    }
    history.push(Message::user("recent goal"));
    history.push(Message::assistant("recent finding"));
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(AuditHost::default());
    let result = run_turn(
        config(address),
        history.clone(),
        host.clone(),
        dir.path(),
        RunOptions {
            pinned_context: pinned,
            context_window: Some(4096),
            max_output_tokens: Some(128),
            ..RunOptions::default()
        },
    )
    .await;
    let request = server.join().unwrap();
    result.unwrap();

    // The pinned instructions are fixed instructions, so a middle and a tail
    // marker must both reach the real provider request unchanged.
    let request = serde_json::to_string(&request).unwrap();
    assert!(
        request.contains(&pinned_text),
        "the full registered constraint must survive verbatim: {request}"
    );
    assert!(request.contains("MIDDLE-CONSTRAINT"), "{request}");
    assert!(request.contains("tail-constraint"), "{request}");
    assert!(request.contains("Earlier context summary"), "{request}");

    let observations = host.observations.lock().unwrap().clone();
    assert_eq!(observations.len(), 1, "one request compaction expected");
    let observation = &observations[0];
    assert_eq!(observation.kind, CompactionKind::Request);
    assert_eq!(observation.turn, Some(1));
    let report = &observation.report;
    assert_eq!(report.before_messages, Some(history.len()));
    assert!(report.after_messages.unwrap() < report.before_messages.unwrap());
    assert_eq!(report.before_bytes, Some(history_bytes(&history)));
    assert!(report.before_bytes.unwrap() > report.after_bytes.unwrap());
    assert_eq!(report.pinned_items_preserved, Some(true));
    assert_eq!(
        report.preserved_constraint_ids,
        Some(vec!["mid-tail-rule".to_owned()])
    );
    assert_eq!(
        report.preserved_goal_ids,
        Some(vec!["finish-migration".to_owned()])
    );
    assert_eq!(report.dropped_constraint_ids, Some(Vec::new()));

    // The audit is content-free: neither the pinned text nor the summarized
    // source text may leak into it.
    let encoded = serde_json::to_string(&observations).unwrap();
    assert!(!encoded.contains("MIDDLE-CONSTRAINT"), "{encoded}");
    assert!(!encoded.contains("tail-constraint"), "{encoded}");
    assert!(!encoded.contains("Finish the migration"), "{encoded}");
    assert!(encoded.contains("\"before_messages\""), "{encoded}");
    assert!(
        encoded.contains("\"preserved_constraint_ids\""),
        "{encoded}"
    );
}

/// The automatic pre-run compaction path emits its own session-kind audit with
/// the same real whole-history metrics.
#[tokio::test]
async fn run_auto_compaction_reports_session_before_and_after() {
    let (address, server) = capture_request();
    let mut history = vec![Message::system("run rules")];
    for index in 0..8 {
        history.push(Message::user(format!(
            "old turn {index} {}",
            "x".repeat(200)
        )));
        history.push(Message::assistant(format!("old answer {index}")));
    }
    let expected_messages = history.len();
    let expected_bytes = history_bytes(&history);
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(AuditHost::default());
    let result = run_turn(
        config(address),
        history,
        host.clone(),
        dir.path(),
        RunOptions {
            auto_compact_bytes: Some(256),
            context_window: Some(8192),
            max_output_tokens: Some(256),
            ..RunOptions::default()
        },
    )
    .await;
    let _request = server.join().unwrap();
    result.unwrap();

    let observations = host.observations.lock().unwrap().clone();
    assert_eq!(observations.len(), 1, "exactly one auto compaction audit");
    let observation = &observations[0];
    assert_eq!(observation.kind, CompactionKind::Session);
    assert_eq!(observation.turn, None);
    assert_eq!(observation.report.before_messages, Some(expected_messages));
    assert_eq!(observation.report.before_bytes, Some(expected_bytes));
    assert!(
        observation.report.after_messages.unwrap() < expected_messages,
        "the auto compaction must shorten the history"
    );
    assert!(observation.report.after_bytes.unwrap() < expected_bytes);
}

/// The default 512 KiB setting must still compact once history passes 3/4 of
/// a small model's input budget, instead of waiting for request-level fitting
/// on every turn.
#[tokio::test]
async fn auto_compaction_threshold_is_capped_by_the_input_budget() {
    let (address, server) = capture_request();
    let mut history = vec![Message::system("run rules")];
    for index in 0..60 {
        history.push(Message::user(format!("turn {index} {}", "y".repeat(200))));
        history.push(Message::assistant(format!("answer {index}")));
    }
    let original_bytes = history_bytes(&history);
    assert!(original_bytes < mio_coding_agent::compact::DEFAULT_AUTO_COMPACT_BYTES);
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(AuditHost::default());
    let result = run_turn(
        config(address),
        history,
        host.clone(),
        dir.path(),
        RunOptions {
            auto_compact_bytes: Some(mio_coding_agent::compact::DEFAULT_AUTO_COMPACT_BYTES),
            context_window: Some(16384),
            max_output_tokens: Some(256),
            ..RunOptions::default()
        },
    )
    .await;
    let _request = server.join().unwrap();
    result.unwrap();

    let observations = host.observations.lock().unwrap().clone();
    let session: Vec<_> = observations
        .iter()
        .filter(|observation| observation.kind == CompactionKind::Session)
        .collect();
    assert_eq!(session.len(), 1, "{observations:?}");
    assert_eq!(session[0].report.before_bytes, Some(original_bytes));
    assert!(session[0].report.after_bytes.unwrap() < original_bytes);
}

#[tokio::test]
async fn automatic_compaction_keeps_tiny_history_when_summary_would_expand_it() {
    let (address, server) = capture_request();
    let mut history = Vec::new();
    for index in 0..4 {
        history.push(Message::user(format!("keep-user-{index}")));
        history.push(Message::assistant(format!("keep-answer-{index}")));
    }
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(AuditHost::default());
    let result = run_turn(
        config(address),
        history,
        host.clone(),
        dir.path(),
        RunOptions {
            auto_compact_bytes: Some(1),
            context_window: Some(8192),
            max_output_tokens: Some(256),
            ..RunOptions::default()
        },
    )
    .await;
    let request = server.join().unwrap();
    result.unwrap();
    let encoded = serde_json::to_string(&request).unwrap();
    for index in 0..4 {
        assert!(encoded.contains(&format!("keep-user-{index}")));
        assert!(encoded.contains(&format!("keep-answer-{index}")));
    }
    assert!(host.observations.lock().unwrap().is_empty());
    assert!(!encoded.contains("Earlier conversation turns have been summarized below"));
}

/// A pinned block that cannot fit refuses the request before any dispatch:
/// no provider connection, and no audit claiming the pins were preserved.
#[tokio::test]
async fn oversized_pinned_constraints_refuse_without_contacting_the_provider() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let mut pinned = PinnedContext::default();
    pinned
        .insert("huge-rule", PinnedContextKind::Constraint, "x".repeat(4000))
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(AuditHost::default());
    let error = run_turn(
        config(address),
        vec![Message::system("rules")],
        host.clone(),
        dir.path(),
        RunOptions {
            pinned_context: pinned,
            context_window: Some(1024),
            max_output_tokens: Some(128),
            ..RunOptions::default()
        },
    )
    .await
    .unwrap_err();
    assert!(error.contains("cannot fit"), "{error}");
    assert!(
        host.observations.lock().unwrap().is_empty(),
        "a refused request must not be reported as preserved"
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "a refused pinned block must never reach the provider"
    );
}
