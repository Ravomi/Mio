//! `--output-schema PATH` 的 CLI/RPC 验收。
//!
//! 三件事各自用真实本地回环证明：
//!
//! 1. CLI 一次性运行与 `--mode rpc` 的启动选项都会把 schema 放进真实模型请求
//!    （`response_format` 为原生 `json_schema`）；
//! 2. 不加这个开关时请求与以前完全一致（没有 `response_format`）；
//! 3. 缺文件、非法 JSON、不是 JSON Schema、超限以及引擎 32 KiB 上限都在任何模型
//!    请求之前明确失败，而不是静默退化成“没有 schema”。
//!
//! 这些测试只证明输出**格式**约束到达了线上请求，不证明模型结论或审查内容正确。

#[path = "../src/test_http.rs"]
mod test_http;

use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    process::{Command, Output, Stdio},
    sync::mpsc,
    time::Duration,
};

const EVENT_TIMEOUT: Duration = Duration::from_secs(30);

fn cli() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mio"));
    cmd.env_remove("MIO_MODEL")
        .env(
            "MIO_CONFIG_DIR",
            std::env::temp_dir().join(format!("mio-schema-cli-config-{}", uuid::Uuid::new_v4())),
        )
        .env("MIO_PROVIDER", "custom")
        .env_remove("MIO_BASE_URL")
        .env_remove("OPENAI_API_KEY")
        .env("MIO_API_KEY", "local-test-credential");
    cmd
}

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {"summary": {"type": "string"}},
        "required": ["summary"],
        "additionalProperties": false
    })
}

fn write_schema_file(dir: &Path, name: &str, contents: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

fn write_text_reply(socket: &mut TcpStream, content: &str) {
    let delta = json!({
        "id": "schema-cli-test",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "test",
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": content}, "finish_reason": null}]
    });
    let finish = json!({
        "id": "schema-cli-test",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "test",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    });
    let body = format!("data: {delta}\n\ndata: {finish}\n\ndata: [DONE]\n\n");
    write!(
        socket,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    socket.flush().unwrap();
}

/// Serve exactly one model request and return the parsed request body.
fn serve_one(listener: TcpListener, content: &'static str) -> std::thread::JoinHandle<Value> {
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("expected one model request");
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let (_, request) = test_http::read_json(&mut socket);
        write_text_reply(&mut socket, content);
        request
    })
}

fn run_cli(dir: &Path, args: &[&str], address: Option<SocketAddr>) -> Output {
    let mut command = cli();
    command
        .current_dir(dir)
        .arg("--cwd")
        .arg(dir)
        .args(["--model", "test"])
        .args(["--json", "--no-context", "--no-mcp", "--no-tools"]);
    if let Some(address) = address {
        command.args(["--base-url", &format!("http://{address}/v1")]);
    }
    command.args(args).output().unwrap()
}

/// A listener that is never served: any accepted connection means the CLI
/// contacted the model before failing.
fn idle_listener() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    (listener, address)
}

fn assert_no_model_request(listener: &TcpListener) {
    match listener.accept() {
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        Ok(_) => panic!("the CLI contacted the model despite a schema error"),
        Err(error) => panic!("unexpected accept error: {error}"),
    }
}

/// Every user-visible failure line: stderr plus JSON `error` events on stdout.
///
/// `--json` and `--mode rpc` report failures as JSON lines, so a test that only
/// read stderr would miss the real diagnostic.
fn failure_text(output: &Output) -> String {
    let mut text = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        match serde_json::from_str::<Value>(line) {
            Ok(event) => {
                if let Some(error) = event.get("error").and_then(Value::as_str) {
                    text.push('\n');
                    text.push_str(error);
                }
            }
            Err(_) => {
                text.push('\n');
                text.push_str(line);
            }
        }
    }
    text
}

#[test]
fn cli_output_schema_reaches_the_model_request() {
    let dir = tempfile::tempdir().unwrap();
    let schema_path = write_schema_file(
        dir.path(),
        "answer.schema.json",
        &serde_json::to_string_pretty(&schema()).unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = serve_one(listener, "{\"summary\":\"checked\"}");
    let output = run_cli(
        dir.path(),
        &[
            "-p",
            "Return a structured summary.",
            "--output-schema",
            schema_path.to_str().unwrap(),
        ],
        Some(address),
    );
    let request = server.join().unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(request["response_format"]["type"], "json_schema");
    assert_eq!(
        request["response_format"]["json_schema"]["schema"]["required"],
        json!(["summary"])
    );
}

#[test]
fn without_output_schema_the_request_carries_no_response_format() {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = serve_one(listener, "plain answer");
    let output = run_cli(dir.path(), &["-p", "Say something."], Some(address));
    let request = server.join().unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        request["response_format"].is_null(),
        "ordinary CLI behaviour changed: {}",
        request["response_format"]
    );
}

#[test]
fn rpc_output_schema_reaches_the_model_request() {
    let dir = tempfile::tempdir().unwrap();
    let schema_path = write_schema_file(
        dir.path(),
        "answer.schema.json",
        &serde_json::to_string(&schema()).unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (request_tx, requests) = mpsc::channel();
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + EVENT_TIMEOUT;
        loop {
            match listener.accept() {
                Ok((mut socket, _)) => {
                    socket
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    let (_, request) = test_http::read_json(&mut socket);
                    write_text_reply(&mut socket, "{\"summary\":\"checked\"}");
                    let _ = request_tx.send(request);
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "RPC never sent a model request"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("{error}"),
            }
        }
    });

    let mut child = cli()
        .current_dir(dir.path())
        .env("MIO_CONFIG_DIR", dir.path())
        .args(["--cwd"])
        .arg(dir.path())
        .args([
            "--mode",
            "rpc",
            "--no-context",
            "--no-mcp",
            "--no-tools",
            "--no-session",
            "--model",
            "test",
            "--base-url",
            &format!("http://{address}/v1"),
            "--output-schema",
            schema_path.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(
        child.stdin.as_mut().unwrap(),
        r#"{{"type":"prompt","id":"schema","message":"Return a structured summary."}}"#
    )
    .unwrap();
    child.stdin.as_mut().unwrap().flush().unwrap();

    let stdout = child.stdout.take().unwrap();
    let (event_tx, events) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            let event: Value = serde_json::from_str(&line)
                .unwrap_or_else(|_| panic!("non-JSON RPC output: {line}"));
            if event_tx.send(event).is_err() {
                break;
            }
        }
    });
    let deadline = std::time::Instant::now() + EVENT_TIMEOUT;
    let mut done = false;
    while std::time::Instant::now() < deadline {
        let Ok(event) = events.recv_timeout(Duration::from_millis(200)) else {
            continue;
        };
        if event["type"] == "error" {
            let stderr = child
                .wait_with_output()
                .map(|out| String::from_utf8_lossy(&out.stderr).into_owned());
            panic!("RPC error event: {event} stderr={stderr:?}");
        }
        if event["type"] == "done" {
            done = true;
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(done, "RPC run never reported done");
    let request = requests
        .recv_timeout(EVENT_TIMEOUT)
        .expect("RPC model request");
    assert_eq!(request["response_format"]["type"], "json_schema");
    assert_eq!(
        request["response_format"]["json_schema"]["schema"]["required"],
        json!(["summary"])
    );
}

#[test]
fn broken_schema_files_are_refused_before_any_model_request() {
    let dir = tempfile::tempdir().unwrap();
    let invalid_json = write_schema_file(dir.path(), "invalid.json", "{\"type\": ");
    let not_a_schema = write_schema_file(dir.path(), "array.json", "[1, 2, 3]");
    let directory = dir.path().join("dir.json");
    std::fs::create_dir_all(&directory).unwrap();
    let cases: Vec<(&str, std::path::PathBuf, &str)> = vec![
        (
            "missing",
            dir.path().join("missing.schema.json"),
            "cannot access",
        ),
        ("invalid json", invalid_json, "is not valid JSON"),
        ("not a schema", not_a_schema, "is not a valid JSON Schema"),
        ("directory", directory, "must be a regular file"),
    ];
    for (label, path, expected) in cases {
        let (listener, address) = idle_listener();
        let output = run_cli(
            dir.path(),
            &[
                "-p",
                "This must never reach the model.",
                "--output-schema",
                path.to_str().unwrap(),
            ],
            Some(address),
        );
        assert!(
            !output.status.success(),
            "{label}: the CLI accepted a broken schema file"
        );
        let failure = failure_text(&output);
        assert!(
            failure.contains(expected),
            "{label}: unexpected failure output {failure}"
        );
        assert_no_model_request(&listener);
    }
}

#[test]
fn oversized_schema_file_is_refused_before_any_model_request() {
    let dir = tempfile::tempdir().unwrap();
    let oversized = format!(
        "{{\"type\":\"object\",\"description\":\"{}\"}}",
        "x".repeat(300 * 1024)
    );
    let path = write_schema_file(dir.path(), "huge.schema.json", &oversized);
    let (listener, address) = idle_listener();
    let output = run_cli(
        dir.path(),
        &[
            "-p",
            "Never reached.",
            "--output-schema",
            path.to_str().unwrap(),
        ],
        Some(address),
    );
    assert!(!output.status.success());
    let failure = failure_text(&output);
    assert!(
        failure.contains("read limit"),
        "unexpected failure output: {failure}"
    );
    assert_no_model_request(&listener);
}

#[test]
fn schema_over_the_engine_32_kib_limit_is_refused_before_any_model_request() {
    let dir = tempfile::tempdir().unwrap();
    let big = format!(
        "{{\"type\":\"object\",\"description\":\"{}\"}}",
        "x".repeat(33 * 1024)
    );
    let path = write_schema_file(dir.path(), "big.schema.json", &big);
    let (listener, address) = idle_listener();
    let output = run_cli(
        dir.path(),
        &[
            "-p",
            "Never reached.",
            "--output-schema",
            path.to_str().unwrap(),
        ],
        Some(address),
    );
    assert!(!output.status.success());
    let failure = failure_text(&output);
    assert!(
        failure.contains("32 KiB"),
        "unexpected failure output: {failure}"
    );
    assert_no_model_request(&listener);
}

#[test]
fn rpc_startup_refuses_a_broken_schema_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_schema_file(dir.path(), "invalid.json", "not json at all");
    let output = cli()
        .current_dir(dir.path())
        .env("MIO_CONFIG_DIR", dir.path())
        .args(["--cwd"])
        .arg(dir.path())
        .args([
            "--mode",
            "rpc",
            "--no-context",
            "--no-mcp",
            "--no-session",
            "--model",
            "test",
            "--base-url",
            "http://127.0.0.1:9/v1",
            "--output-schema",
            path.to_str().unwrap(),
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let failure = failure_text(&output);
    assert!(
        failure.contains("is not valid JSON"),
        "unexpected failure output: {failure}"
    );
}

#[test]
fn output_schema_is_refused_with_standalone_commands() {
    let dir = tempfile::tempdir().unwrap();
    let schema_path = write_schema_file(
        dir.path(),
        "answer.schema.json",
        &serde_json::to_string(&schema()).unwrap(),
    );
    let output = run_cli(
        dir.path(),
        &[
            "--list-sessions",
            "--output-schema",
            schema_path.to_str().unwrap(),
        ],
        None,
    );
    assert!(!output.status.success());
    let failure = failure_text(&output);
    assert!(
        failure.contains("--list-sessions"),
        "unexpected failure output: {failure}"
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_schema_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let real = write_schema_file(
        dir.path(),
        "real.schema.json",
        &serde_json::to_string(&schema()).unwrap(),
    );
    let link = dir.path().join("link.schema.json");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let (listener, address) = idle_listener();
    let output = run_cli(
        dir.path(),
        &[
            "-p",
            "Never reached.",
            "--output-schema",
            link.to_str().unwrap(),
        ],
        Some(address),
    );
    assert!(!output.status.success());
    let failure = failure_text(&output);
    assert!(
        failure.contains("must be a regular file"),
        "unexpected failure output: {failure}"
    );
    assert_no_model_request(&listener);
}
