//! Regression tests for the read-output/context-budget interaction.
//!
//! A real workflow read a ~61 KiB document and the next request immediately hit
//! `ContextBudget` because `MAX_READ_OUTPUT_BYTES` (64 KiB) is far larger than
//! the estimated input limit of a fallback (non-catalog) model. The fix binds a
//! per-Workspace read output budget, defaulting to the old 64 KiB so direct
//! callers are unchanged, while `run_with_options` derives it from the resolved
//! `ContextBudget::input_limit()`.
use mio_coding_agent::{
    AgentEvent, Arguments, Config, Host, HostFuture, Message, Provider, RunOptions, Services,
    Workspace,
    context_budget::ContextBudget,
    run_with_options,
    workspace::{MAX_READ_OUTPUT_BYTES, MIN_READ_OUTPUT_BYTES, clamp_read_output_bytes},
};
use serde_json::{Value, json};
use std::{io::Write, net::TcpListener, path::Path, sync::Arc, time::Duration};

#[path = "../src/test_http.rs"]
mod test_http;

struct DummyHost;
impl Host for DummyHost {
    fn shell(&self, _cmd: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }
    fn on_event(&self, _event: AgentEvent) {}
}

/// Records the real `ReadObservation`s a `read` emits, so pagination tests can
/// check the reported range against the page they actually got.
#[derive(Default)]
struct RecordingHost {
    observations: std::sync::Mutex<Vec<mio_coding_agent::ReadObservation>>,
}

impl Host for RecordingHost {
    fn shell(&self, _cmd: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }
    fn on_event(&self, _event: AgentEvent) {}
    fn on_read_observation(&self, observation: &mio_coding_agent::ReadObservation) {
        self.observations.lock().unwrap().push(observation.clone());
    }
}

/// Number of lines a returned page body really carries.
fn returned_line_count(body: &str) -> u64 {
    if body.is_empty() {
        0
    } else if body.ends_with('\n') {
        body.split('\n').count() as u64 - 1
    } else {
        body.split('\n').count() as u64
    }
}

/// A multi-line document that mixes CJK and ASCII, roughly `lines * 48` bytes.
fn long_document(prefix: &str, lines: usize) -> String {
    let mut doc = String::new();
    for index in 1..=lines {
        doc.push_str(&format!(
            "{prefix} 第 {index} 行，包含中文与 ASCII 混排内容 padding-{:04}\n",
            index % 10_000
        ));
    }
    doc
}

/// Split a truncated page into `(body, next_offset)`.
fn split_truncated(page: &str) -> Option<(&str, u64)> {
    let rest = page.strip_prefix("[TRUNCATED: ")?;
    let end = rest.find("]\n")?;
    let next = rest[..end]
        .split("Next offset: ")
        .nth(1)?
        .trim()
        .parse()
        .ok()?;
    Some((&rest[end + 2..], next))
}

#[tokio::test]
async fn default_workspace_read_of_long_file_is_unchanged() {
    let folder = tempfile::tempdir().unwrap();
    let host = Arc::new(DummyHost);
    // ~62 KiB: longer than a small dynamic budget but under the 64 KiB default.
    let doc = long_document("default", 900);
    assert!(
        doc.len() > 60 * 1024 && doc.len() < MAX_READ_OUTPUT_BYTES,
        "len={}",
        doc.len()
    );
    std::fs::write(folder.path().join("spec.txt"), &doc).unwrap();

    let ws = Workspace::new(folder.path(), host).unwrap();
    assert_eq!(ws.read_output_limit_bytes(), MAX_READ_OUTPUT_BYTES);

    let output = ws.execute(Arguments::read("spec.txt")).await.unwrap();
    assert_eq!(
        output, doc,
        "default Workspace must still read the whole file"
    );
    assert!(!output.contains("[TRUNCATED:"));
}

#[tokio::test]
async fn instance_limit_clamps_and_stays_below_cap() {
    assert_eq!(clamp_read_output_bytes(0), MIN_READ_OUTPUT_BYTES);
    assert_eq!(clamp_read_output_bytes(usize::MAX), MAX_READ_OUTPUT_BYTES);

    let folder = tempfile::tempdir().unwrap();
    let host = Arc::new(DummyHost);
    let doc = long_document("bounded", 1_300);
    std::fs::write(folder.path().join("spec.txt"), &doc).unwrap();

    let limit = 512;
    let ws = Workspace::new(folder.path(), host)
        .unwrap()
        .with_read_output_limit_bytes(limit);
    assert_eq!(ws.read_output_limit_bytes(), limit);

    let output = ws.execute(Arguments::read("spec.txt")).await.unwrap();
    assert!(
        output.len() <= limit,
        "output {} must respect the instance limit {limit}",
        output.len()
    );
    assert!(output.len() < MAX_READ_OUTPUT_BYTES);
    assert!(output.contains("[TRUNCATED:"));
    assert!(output.contains("Next offset:"));
}

#[tokio::test]
async fn small_limit_paginates_utf8_without_losing_lines() {
    let folder = tempfile::tempdir().unwrap();
    let host = Arc::new(DummyHost);
    let doc = long_document("paged", 120);
    std::fs::write(folder.path().join("doc.txt"), &doc).unwrap();

    let ws = Workspace::new(folder.path(), host)
        .unwrap()
        .with_read_output_limit_bytes(320);

    let mut offset = 1u64;
    let mut collected = String::new();
    let mut pages = 0usize;
    loop {
        let mut args = Arguments::read("doc.txt");
        args.offset = Some(offset);
        let page = ws.execute(args).await.unwrap();
        pages += 1;
        assert!(pages < 1_000, "pagination must terminate");
        assert!(!page.contains('\u{FFFD}'), "UTF-8 must not be cut mid-char");
        assert!(page.len() <= 320, "page must respect the instance limit");
        match split_truncated(&page) {
            Some((body, next)) => {
                assert!(next > offset, "Next offset must move forward");
                collected.push_str(body);
                offset = next;
            }
            None => {
                collected.push_str(&page);
                break;
            }
        }
    }

    assert!(pages > 1, "a 320-byte budget must need several pages");
    assert_eq!(
        collected, doc,
        "pagination must not drop or duplicate lines"
    );
}

#[tokio::test]
async fn header_reports_exact_next_offset_across_pages() {
    let folder = tempfile::tempdir().unwrap();
    let host = Arc::new(DummyHost);
    let doc = long_document("header", 40);
    std::fs::write(folder.path().join("doc.txt"), doc).unwrap();

    let ws = Workspace::new(folder.path(), host)
        .unwrap()
        .with_read_output_limit_bytes(400);

    let first = ws.execute(Arguments::read("doc.txt")).await.unwrap();
    let (first_body, next) = split_truncated(&first).expect("first page must truncate");
    let first_lines = first_body.lines().count() as u64;
    assert_eq!(
        next,
        first_lines + 1,
        "Next offset is the first unshown line"
    );

    let second = ws
        .execute(Arguments::read_paged("doc.txt", next, 5))
        .await
        .unwrap();
    let (second_body, _) = split_truncated(&second).expect("limit 5 must truncate");
    let expected = long_document("header", 40)
        .split('\n')
        .skip((next - 1) as usize)
        .take(5)
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(second_body.trim_end_matches('\n'), expected);
}

#[tokio::test]
async fn long_single_line_still_returns_recoverable_error() {
    let folder = tempfile::tempdir().unwrap();
    let host = Arc::new(DummyHost);
    let huge_line = "A".repeat(4_000);
    std::fs::write(folder.path().join("huge.txt"), &huge_line).unwrap();

    let ws = Workspace::new(folder.path(), host)
        .unwrap()
        .with_read_output_limit_bytes(512);
    let err = ws.execute(Arguments::read("huge.txt")).await.unwrap_err();
    assert!(err.contains("超出单次输出上限预算"), "got: {err}");
    assert!(err.contains("请使用 shell 分段读取"), "got: {err}");
}

#[tokio::test]
async fn run_with_options_truncates_read_before_second_request() {
    let folder = tempfile::tempdir().unwrap();
    let canonical = folder.path().canonicalize().unwrap();
    // > 60 KiB, the real workflow payload that used to overflow the next request.
    let doc = long_document("wire-spec", 1_400);
    assert!(doc.len() > 60 * 1024);
    std::fs::write(canonical.join("spec.txt"), &doc).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        for turn in 0..2 {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let (header, request) = test_http::read_json(&mut socket);
            assert!(header.starts_with("POST /v1/chat/completions "));
            tx.send(request).unwrap();
            let delta = if turn == 0 {
                json!({
                    "role": "assistant",
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_read",
                        "type": "function",
                        "function": {
                            "name": "read",
                            "arguments": json!({"path": "spec.txt"}).to_string()
                        }
                    }]
                })
            } else {
                json!({"role": "assistant", "content": "读完了"})
            };
            let chunk = |delta: Value, finish: Value| {
                json!({
                    "id": "test",
                    "object": "chat.completion.chunk",
                    "created": 0,
                    "model": "local-test",
                    "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
                })
                .to_string()
            };
            let body = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                chunk(delta, Value::Null),
                chunk(
                    json!({}),
                    json!(if turn == 0 { "tool_calls" } else { "stop" })
                )
            );
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        }
    });

    let config = Config {
        provider: Provider::Custom,
        base_url: format!("http://{address}/v1"),
        model: "local-test".into(),
        api_key: "test-only".into(),
    };
    let options = RunOptions::default();
    // The same resolution the run performs; the fallback window must tighten read.
    let budget = ContextBudget::resolve(&config, &options).unwrap();
    assert_eq!(budget.source, "fallback");
    let effective_limit = clamp_read_output_bytes(budget.input_limit() / 4);
    assert!(
        effective_limit < MAX_READ_OUTPUT_BYTES,
        "fallback window must tighten the read output budget"
    );

    let history: Vec<Message> = run_with_options(
        config,
        Vec::new(),
        "读规范",
        &canonical,
        Arc::new(DummyHost),
        Services::default(),
        options,
    )
    .await
    .expect("the request following the read must not hit ContextBudget");

    server.join().unwrap();
    assert!(!history.is_empty());

    let requests: Vec<Value> = rx.try_iter().collect();
    assert_eq!(
        requests.len(),
        2,
        "one read turn must be followed by exactly one more model request"
    );
    assert!(
        !requests[0]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "tool")
    );

    let second = requests[1]["messages"].as_array().unwrap();
    let tool = second
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("second request must carry the read tool result");
    let content = tool["content"]
        .as_str()
        .expect("tool content must be plain text");
    // The `read` tool returns `{"output": "..."}`; the budget applies to the
    // file text, and the JSON framing/escaping overhead sits on top of it.
    let parsed: Value = serde_json::from_str(content).expect("tool result is JSON");
    let output = parsed["output"].as_str().expect("read output must be text");
    assert!(
        output.contains("[TRUNCATED:"),
        "read result must be truncated to the instance budget"
    );
    assert!(
        output.contains("Next offset:"),
        "truncation must advertise the next page"
    );
    assert!(
        output.len() <= effective_limit,
        "read output {} must fit the derived budget {effective_limit}",
        output.len()
    );
    assert!(output.len() < doc.len());
    assert!(content.len() < budget.input_limit());
}

/// A byte-cap truncation must add the generic grep guidance inside the same
/// metadata header, keep `Next offset` last, stay within the original cap, keep
/// UTF-8 intact, and rebuild the file exactly from the real emitted pages.
/// The `ReadObservation` of every page must match the lines actually returned.
#[tokio::test]
async fn byte_capped_truncation_carries_grep_guidance_within_the_original_cap() {
    let folder = tempfile::tempdir().unwrap();
    let host = Arc::new(RecordingHost::default());
    let doc = long_document("guided", 220);
    let total_lines = doc.split('\n').count() as u64;
    std::fs::write(folder.path().join("doc.txt"), &doc).unwrap();

    let cap = 512usize;
    let ws = Workspace::new(folder.path(), host.clone())
        .unwrap()
        .with_read_output_limit_bytes(cap);

    let mut offset = 1u64;
    let mut collected = String::new();
    let mut pages = 0usize;
    let mut guided_pages = 0usize;
    loop {
        let mut args = Arguments::read("doc.txt");
        args.offset = Some(offset);
        let before = host.observations.lock().unwrap().len();
        let page = ws.execute(args).await.unwrap();
        pages += 1;
        assert!(pages < 2_000, "pagination must terminate");
        assert!(!page.contains('\u{FFFD}'), "UTF-8 must not be cut mid-char");
        assert!(
            page.len() <= cap,
            "page {} must respect the original cap {cap}",
            page.len()
        );

        let observation = {
            let observations = host.observations.lock().unwrap();
            assert_eq!(
                observations.len(),
                before + 1,
                "one read emits one observation"
            );
            observations.last().unwrap().clone()
        };
        assert_eq!(observation.total, Some(total_lines));

        match split_truncated(&page) {
            Some((body, next)) => {
                let header = &page[..page.find("]\n").expect("header must close")];
                assert!(
                    header.starts_with("[TRUNCATED: Showing lines "),
                    "{header:?}"
                );
                assert!(
                    header.contains(&format!(" of {total_lines}.")),
                    "{header:?}"
                );
                assert_eq!(
                    header[header.find("Next offset: ").expect("Next offset stays")
                        + "Next offset: ".len()..]
                        .parse::<u64>()
                        .ok(),
                    Some(next),
                );

                let lines = returned_line_count(body);
                if header.contains("Use grep to locate relevant sections before paging further.") {
                    guided_pages += 1;
                    let guidance_at = header.find("Use grep").unwrap();
                    let next_at = header.find("Next offset: ").unwrap();
                    assert!(guidance_at < next_at, "Next offset must remain last");
                }
                assert!(lines > 0, "a byte-capped page keeps at least one line");
                assert_eq!(observation.offset, Some(offset));
                assert_eq!(observation.limit, Some(lines));
                assert!(observation.truncated);
                assert_eq!(
                    next,
                    offset + lines,
                    "Next offset advances by returned lines"
                );
                collected.push_str(body);
                offset = next;
            }
            None => {
                // Reaching EOF without a header is the only way to get here, so
                // the page carries every remaining line up to the file total.
                let lines = total_lines - offset + 1;
                assert_eq!(observation.offset, Some(offset));
                assert_eq!(observation.limit, Some(lines));
                assert!(!observation.truncated);
                collected.push_str(&page);
                break;
            }
        }
    }

    assert!(pages > 1, "a {cap}-byte cap must need several pages");
    assert!(
        guided_pages > 0,
        "byte-capped pages must carry the guidance"
    );
    assert_eq!(
        collected, doc,
        "pagination must not drop or duplicate lines"
    );
}

/// An explicit `limit` that paginates without touching the byte cap must keep
/// the old header (no new guidance) and the exact requested page size.
#[tokio::test]
async fn explicit_limit_pagination_keeps_the_old_header_without_guidance() {
    let folder = tempfile::tempdir().unwrap();
    let host = Arc::new(DummyHost);
    let doc = long_document("limited", 12);
    std::fs::write(folder.path().join("doc.txt"), &doc).unwrap();

    // The default 64 KiB cap is far above three lines: truncation is limit-only.
    let ws = Workspace::new(folder.path(), host).unwrap();
    let page = ws
        .execute(Arguments::read_paged("doc.txt", 1, 3))
        .await
        .unwrap();

    assert!(page.contains("[TRUNCATED:"), "limit 3 must truncate");
    assert!(
        !page.contains("Use grep"),
        "limit-only pagination must not add byte-cap guidance: {page:?}"
    );
    let (body, next) = split_truncated(&page).expect("limit 3 must truncate");
    assert_eq!(next, 4, "Next offset is the first unshown line");
    assert_eq!(returned_line_count(body), 3, "exact requested page size");
    let expected = long_document("limited", 12)
        .split('\n')
        .take(3)
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(body.trim_end_matches('\n'), expected);
}

/// A small file that fits must stay a plain read: no header, no guidance.
#[tokio::test]
async fn small_file_read_has_no_truncation_guidance() {
    let folder = tempfile::tempdir().unwrap();
    let host = Arc::new(DummyHost);
    let doc = "alpha\nbeta\ngamma\n";
    std::fs::write(folder.path().join("small.txt"), doc).unwrap();

    let ws = Workspace::new(folder.path(), host).unwrap();
    let output = ws.execute(Arguments::read("small.txt")).await.unwrap();
    assert_eq!(output, doc);
    assert!(!output.contains("[TRUNCATED:"));
    assert!(!output.contains("Use grep"));
}

/// Tool filters apply to retrieval advice too: a read-only whitelist must not
/// be told to dispatch a grep operation it cannot use.
#[tokio::test]
async fn truncation_does_not_advise_a_filtered_out_grep_tool() {
    let folder = tempfile::tempdir().unwrap();
    let doc = long_document("filtered", 40);
    std::fs::write(folder.path().join("doc.txt"), &doc).unwrap();
    let filter = mio_coding_agent::pattern::ToolFilter::from_whitelist(&["read".into()]).unwrap();
    assert!(!filter.allows_workspace_operation("grep"));
    let cap = 512;
    let ws = Workspace::new(folder.path(), Arc::new(DummyHost))
        .unwrap()
        .with_tool_filter(filter)
        .with_read_output_limit_bytes(cap);
    let page = ws.execute(Arguments::read("doc.txt")).await.unwrap();
    assert!(page.len() <= cap);
    assert!(!page.contains("Use grep"));
    let (body, next) = split_truncated(&page).expect("the cap still truncates");
    assert!(next > 1);
    assert!(doc.starts_with(body));
    assert_eq!(next, returned_line_count(body) + 1);
}
