#[path = "../src/test_http.rs"]
mod test_http;
use serde_json::{Value, json};
use std::{io::Write, time::Duration};

#[test]
fn cli_model_delegates_polls_and_collects_a_real_native_child() {
    run_scenario(true, false);
}

#[test]
fn writable_child_inherits_cli_shell_restriction() {
    run_scenario(false, true);
}

#[test]
fn writable_child_can_edit_and_parent_receives_result() {
    run_scenario(false, false);
}

fn run_scenario(read_only: bool, shell: bool) {
    let dir = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let (mut parent_turn, mut child_turn) = (0, 0);
        for _ in 0..5 {
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "Missing model request"
                        );
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let (_, request) = test_http::read_json(&mut socket);
            let messages = request["messages"].as_array().unwrap();
            let child = messages
                .iter()
                .any(|m| m["role"] == "user" && m["content"].to_string().contains("child task"));
            let tools = request["tools"].as_array().unwrap();
            assert_eq!(
                tools.iter().any(|t| t["function"]["name"] == "subagent"),
                !child
            );
            let last_tool = || {
                let value =
                    &messages.iter().rev().find(|m| m["role"] == "tool").unwrap()["content"];
                serde_json::from_str::<Value>(value.as_str().unwrap()).unwrap()
            };
            let (tool, args, answer) = if child {
                child_turn += 1;
                if child_turn == 1 {
                    (
                        "workspace",
                        if shell {
                            json!({"operation":"shell","value":"touch forbidden.txt"})
                        } else {
                            json!({"operation":"write","path":"forbidden.txt","value":"no"})
                        },
                        "",
                    )
                } else {
                    if read_only {
                        assert!(last_tool()["error"].as_str().unwrap().contains("只读"));
                    } else if shell {
                        assert!(
                            last_tool()["error"]
                                .as_str()
                                .unwrap()
                                .contains("Shell disabled")
                        );
                    } else {
                        assert!(last_tool()["output"].is_string());
                    }
                    ("", Value::Null, "child complete")
                }
            } else {
                parent_turn += 1;
                match parent_turn {
                    1 => (
                        "subagent",
                        json!({"operation":"delegate","task":{"session":"review","goal":"Review","prompt":"child task","read_only":read_only}}),
                        "",
                    ),
                    2 => (
                        "subagent",
                        json!({"operation":"status","run_id":last_tool()["run_id"],"wait_ms":10000}),
                        "",
                    ),
                    _ => {
                        let result = last_tool();
                        assert_eq!(result["status"], "completed");
                        assert_eq!(result["output"], "child complete");
                        ("", Value::Null, "parent verified")
                    }
                }
            };
            let delta = if tool.is_empty() {
                json!({"role":"assistant","content":answer})
            } else {
                json!({"role":"assistant","tool_calls":[{"index":0,"id":format!("call_{parent_turn}_{child_turn}"),"type":"function","function":{"name":tool,"arguments":args.to_string()}}]})
            };
            let chunk = |delta: Value, finish: Value| {
                json!({"id":"mock","object":"chat.completion.chunk","created":0,"model":"mock","choices":[{"index":0,"delta":delta,"finish_reason":finish}]}).to_string()
            };
            let body = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                chunk(delta, Value::Null),
                chunk(
                    json!({}),
                    json!(if tool.is_empty() {
                        "stop"
                    } else {
                        "tool_calls"
                    })
                )
            );
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
        assert_eq!((parent_turn, child_turn), (3, 2));
    });
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_mio"))
        .args([
            "--subagents",
            "--json",
            "--no-context",
            "--no-shell",
            "--model",
            "mock",
            "--base-url",
            &format!("http://{address}/v1"),
            "-p",
            "parent task",
        ])
        .arg("--cwd")
        .arg(dir.path())
        .env("MIO_API_KEY", "test-only")
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert!(events.iter().any(|e| e["type"] == "done"));
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "text" && e["data"] == "parent verified")
    );
    assert!(
        !events
            .iter()
            .any(|e| e["type"] == "text" && e["data"] == "child complete")
    );
    assert_eq!(
        dir.path().join("forbidden.txt").exists(),
        !read_only && !shell
    );
}
