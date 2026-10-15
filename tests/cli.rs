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
        .env(
            "MIO_CONFIG_DIR",
            std::env::temp_dir().join(format!("mio-cli-config-{}", uuid::Uuid::new_v4())),
        )
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

#[test]
fn cli_list_models_session_naming_export_and_rpc_mode() {
    let dir = tempfile::tempdir().unwrap();

    // 1. Test --list-models
    let models_out = cli()
        .args(["--list-models", "deepseek", "--json"])
        .output()
        .unwrap();
    assert!(models_out.status.success());
    let models_text = String::from_utf8(models_out.stdout).unwrap();
    assert!(models_text.contains("deepseek-reasoner"));

    // 2. Create a named session and export it to HTML
    let sessions_dir = dir.path().join(".mio/sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    let mut named_session = mio_coding_agent::session::Session::new(dir.path()).unwrap();
    named_session.name = Some("alpha-feature".into());
    named_session
        .history
        .push(mio_coding_agent::Message::user("Build alpha feature"));
    named_session
        .history
        .push(mio_coding_agent::Message::assistant(
            "Alpha feature complete.",
        ));
    let session_path = sessions_dir.join(format!("{}.json", named_session.id));
    named_session.save(&session_path).unwrap();

    let html_out = dir.path().join("alpha.html");
    let export_res = cli()
        .arg("--cwd")
        .arg(dir.path())
        .args([
            "-r",
            "alpha-feature",
            "--export",
            html_out.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        export_res.status.success(),
        "export stderr={}",
        String::from_utf8_lossy(&export_res.stderr)
    );
    let html_content = std::fs::read_to_string(&html_out).unwrap();
    assert!(html_content.contains("alpha-feature"));
    assert!(html_content.contains("Build alpha feature"));
    assert!(html_content.contains("Alpha feature complete."));

    // 3. Test --mode rpc
    let mut child = cli()
        .arg("--cwd")
        .arg(dir.path())
        .args([
            "--model",
            "test-model",
            "-r",
            "alpha-feature",
            "--mode",
            "rpc",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(stdin, r#"{{"id":"1","type":"get_state"}}"#).unwrap();
        writeln!(
            stdin,
            r#"{{"id":"2","type":"set_session_name","name":"beta-feature"}}"#
        )
        .unwrap();
        writeln!(stdin, r#"{{"id":"3","type":"compact"}}"#).unwrap();
        writeln!(stdin, r#"{{"id":"4","type":"new_session"}}"#).unwrap();
    }
    let rpc_out = child.wait_with_output().unwrap();
    assert!(rpc_out.status.success());
    let lines: Vec<Value> = String::from_utf8(rpc_out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 4);
    assert_eq!(lines[0]["id"], "1");
    assert_eq!(lines[0]["success"], true);
    assert_eq!(lines[0]["data"]["session_name"], "alpha-feature");
    assert_eq!(lines[1]["id"], "2");
    assert_eq!(lines[1]["data"]["session_name"], "beta-feature");
    assert_eq!(lines[2]["id"], "3");
    assert_eq!(lines[2]["command"], "compact");
    assert_eq!(lines[3]["id"], "4");
    assert_eq!(lines[3]["command"], "new_session");
}

#[test]
fn cli_plugin_lifecycle_init_validate_install_list_search_remove() {
    let dir = tempfile::tempdir().unwrap();
    let src_plugin = dir.path().join("demo-plugin");

    // 1. --plugin-init
    let init_out = cli()
        .arg("--cwd")
        .arg(dir.path())
        .args(["--plugin-init", src_plugin.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        init_out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&init_out.stderr)
    );
    assert!(src_plugin.join("mio-plugin.json").is_file());

    // 2. --plugin-validate
    let val_out = cli()
        .arg("--cwd")
        .arg(dir.path())
        .args(["--plugin-validate", src_plugin.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(val_out.status.success());

    // 3. --plugin-install (local path)
    let inst_out = cli()
        .arg("--cwd")
        .arg(dir.path())
        .args(["--plugin-install", src_plugin.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(inst_out.status.success());
    assert!(
        dir.path()
            .join(".mio/plugins/demo-plugin/mio-plugin.json")
            .is_file()
    );

    // 4. --plugin-list --json
    let list_out = cli()
        .arg("--cwd")
        .arg(dir.path())
        .args(["--plugin-list", "--json"])
        .output()
        .unwrap();
    assert!(list_out.status.success());
    let list_str = String::from_utf8(list_out.stdout).unwrap();
    assert!(list_str.contains("demo-plugin"));

    // 5. --plugin-search
    let search_out = cli()
        .arg("--cwd")
        .arg(dir.path())
        .args(["--plugin-search", "rust"])
        .output()
        .unwrap();
    assert!(search_out.status.success());
    assert!(
        String::from_utf8(search_out.stdout)
            .unwrap()
            .contains("rust-expert")
    );

    // 6. --plugin-remove
    let rm_out = cli()
        .arg("--cwd")
        .arg(dir.path())
        .args(["--plugin-remove", "demo-plugin"])
        .output()
        .unwrap();
    assert!(rm_out.status.success());
    assert!(!dir.path().join(".mio/plugins/demo-plugin").exists());
}
