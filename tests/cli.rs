use serde_json::{Value, json};
use std::{
    io::Write,
    net::TcpListener,
    process::Command,
    time::{Duration, Instant},
};
#[path = "../src/test_http.rs"]
mod test_http;

fn cli() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mio"));
    cmd.env_remove("MIO_MODEL")
        .env("MIO_PROVIDER", "custom")
        .env_remove("MIO_BASE_URL")
        .env_remove("OPENAI_API_KEY")
        .env("MIO_API_KEY", "local-test-credential");
    cmd
}

#[test]
fn help_validation_and_listing_need_no_model() {
    assert!(cli().arg("--help").output().unwrap().status.success());
    assert!(
        !cli()
            .args(["--max-turns", "0"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(!cli().args(["--fork"]).output().unwrap().status.success());
    let dir = tempfile::tempdir().unwrap();
    let output = cli()
        .arg("--cwd")
        .arg(dir.path())
        .arg("--list-sessions")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!dir.path().join(".mio").exists());
}

#[test]
fn cli_edits_with_project_context_then_resumes_forks_and_disables_shell() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("AGENTS.md"),
        "Project marker: preserve user work",
    )
    .unwrap();
    std::fs::write(dir.path().join("code.txt"), "before").unwrap();
    std::fs::create_dir_all(dir.path().join(".agents/skills/review")).unwrap();
    std::fs::create_dir_all(dir.path().join(".mio/prompts")).unwrap();
    std::fs::write(
        dir.path().join(".agents/skills/review/SKILL.md"),
        "---\nname: review\ndescription: Review code\n---\nSelected skill body marker",
    )
    .unwrap();
    std::fs::write(
        dir.path().join(".mio/prompts/review.md"),
        "Review task: $ARGUMENTS",
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for turn in 0..4 {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "CLI did not send request {turn}");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let (_, request) = test_http::read_json(&mut socket);
            requests.push(request);
            let tool = match turn {
                0 => Some(
                    json!({"operation":"edit","path":"code.txt","old_text":"before","new_text":"after"}),
                ),
                2 => Some(json!({"operation":"shell","value":"touch should-not-exist"})),
                _ => None,
            };
            let delta = if let Some(args) = &tool {
                json!({"role":"assistant","tool_calls":[{"index":0,"id":format!("call_{turn}"),"type":"function","function":{"name":"workspace","arguments":args.to_string()}}]})
            } else {
                json!({"role":"assistant","content":"完成"})
            };
            let chunk = |delta: Value, finish: Value| {
                json!({"id":"test","object":"chat.completion.chunk","created":0,"model":"test","choices":[{"index":0,"delta":delta,"finish_reason":finish}]}).to_string()
            };
            let body = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                chunk(delta, Value::Null),
                chunk(
                    json!({}),
                    json!(if tool.is_some() { "tool_calls" } else { "stop" })
                )
            );
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        }
        requests
    });
    let run = |extra: &[&str]| {
        let output = cli()
            .arg("--cwd")
            .arg(dir.path())
            .args([
                "--model",
                "test",
                "--base-url",
                &format!("http://{address}/v1"),
                "--json",
                "--no-shell",
            ])
            .args(extra)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>()
    };
    let events = run(&[
        "-p",
        "fix code",
        "--skill",
        "review",
        "--template",
        "review",
        "--session",
        ".mio/sessions/original.json",
    ]);
    assert_eq!(events.last().unwrap()["type"], "done");
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "tool_finished" && e["data"]["operation"] == "edit")
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("code.txt")).unwrap(),
        "after"
    );
    let original_path = dir.path().join(".mio/sessions/original.json");
    let original = std::fs::read(&original_path).unwrap();
    assert!(!String::from_utf8_lossy(&original).contains("local-test-credential"));
    let original_session = mio_coding_agent::session::Session::load(&original_path).unwrap();
    let events = run(&["--resume", "--fork", "-p", "continue task"]);
    assert!(events.iter().any(|e| {
        e["type"] == "tool_finished"
            && e["data"]["result"]["Err"]
                .as_str()
                .is_some_and(|s| s.contains("disabled"))
    }));
    assert!(!dir.path().join("should-not-exist").exists());
    assert_eq!(std::fs::read(&original_path).unwrap(), original);
    let fork_path = events.last().unwrap()["session"].as_str().unwrap();
    let fork = mio_coding_agent::session::Session::load(std::path::Path::new(fork_path)).unwrap();
    assert_eq!(fork.parent_id, Some(original_session.id));
    assert!(fork.history.len() > original_session.history.len());
    let requests = server.join().unwrap();
    let first = requests[0]["messages"].to_string();
    assert!(
        first.contains("Project marker")
            && first.contains("Selected skill body marker")
            && first.contains("Review task: fix code")
    );
    let resumed = requests[2]["messages"].to_string();
    assert!(resumed.contains("Review task: fix code") && resumed.contains("call_0"));
    let list = cli()
        .arg("--cwd")
        .arg(dir.path())
        .args(["--list-sessions", "--json"])
        .output()
        .unwrap();
    assert!(list.status.success());
    assert_eq!(String::from_utf8(list.stdout).unwrap().lines().count(), 2);
}
