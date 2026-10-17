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
fn rpc_starts_with_an_unselected_large_skill() {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join(".agents/skills/large");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let mut skill = "---\nname: large\ndescription: Large optional skill\n---\n".to_owned();
    skill.push_str(&"x".repeat(32 * 1024));
    std::fs::write(skill_dir.join("SKILL.md"), skill).unwrap();

    let mut child = cli()
        .arg("--cwd")
        .arg(dir.path())
        .args(["--model", "test-model", "--mode", "rpc"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(
        child.stdin.as_mut().unwrap(),
        r#"{{"id":"1","type":"get_state"}}"#
    )
    .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "RPC startup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(output.stdout.trim_ascii()).unwrap();
    assert_eq!(response["id"], "1");
    assert_eq!(response["success"], true);
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
    // The shared policy refuses the aggregate operation before entering the
    // workspace body, so no shell tool-finished event may be fabricated.
    assert!(
        !events
            .iter()
            .any(|e| { e["type"] == "tool_finished" && e["data"]["operation"] == "shell" })
    );
    assert!(!dir.path().join("should-not-exist").exists());
    assert_eq!(std::fs::read(&original_path).unwrap(), original);
    let fork_path = events.last().unwrap()["session"].as_str().unwrap();
    let fork = mio_coding_agent::session::Session::load(std::path::Path::new(fork_path)).unwrap();
    assert_eq!(fork.parent_id, Some(original_session.id));
    assert!(fork.history.len() > original_session.history.len());
    let requests = server.join().unwrap();
    let refused = requests[3]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "call_2")
        .expect("the refused call must resolve with its original provider id");
    let denial = refused["content"].to_string();
    assert!(denial.contains("Tool policy denied"), "{denial}");
    assert!(denial.contains("does not allow 'shell'"), "{denial}");
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

fn parse_json_lines(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8(stdout.to_vec())
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn write_completion(socket: &mut std::net::TcpStream) {
    let chunk = |delta: Value, finish: Value| {
        json!({"id":"test","object":"chat.completion.chunk","created":0,"model":"test","choices":[{"index":0,"delta":delta,"finish_reason":finish}]}).to_string()
    };
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(json!({"role":"assistant","content":"ok"}), Value::Null),
        chunk(json!({}), json!("stop"))
    );
    write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
}

#[test]
fn dedup_key_is_persisted_and_prevents_replay_across_runs() {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (_, request) = test_http::read_json(&mut socket);
        write_completion(&mut socket);
        request
    });
    let run = |base: &str| {
        cli()
            .arg("--cwd")
            .arg(dir.path())
            .args([
                "--model",
                "test",
                "--base-url",
                base,
                "--json",
                "--dedup-key",
                "job-1",
                "-p",
                "do it",
            ])
            .output()
            .unwrap()
    };
    let first = run(&format!("http://{address}/v1"));
    assert!(
        first.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&first.stderr)
    );
    let events = parse_json_lines(&first.stdout);
    let task = events
        .iter()
        .find(|event| event["type"] == "task")
        .expect("task event");
    assert_eq!(task["reused"], false);
    assert_eq!(task["dedup_key"], "job-1");
    let task_id = task["task_id"].as_str().unwrap().to_owned();
    let request = server.join().unwrap();
    assert!(request["messages"].to_string().contains("do it"));

    // 第二次运行指向不可达端口：命中 dedup 时不应发出任何模型请求。
    let second = run("http://127.0.0.1:1/v1");
    assert!(
        second.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&second.stderr)
    );
    let events = parse_json_lines(&second.stdout);
    let task = events
        .iter()
        .find(|event| event["type"] == "task")
        .expect("task event");
    assert_eq!(task["reused"], true);
    assert_eq!(task["task_id"].as_str().unwrap(), task_id);
    assert_eq!(task["state"], "completed");
    assert!(dir.path().join(".mio/dedup/task-keys.json").is_file());
}

#[test]
fn dedup_key_validation_rejects_invalid_combinations() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["--dedup-key", "", "-p", "x"],
        vec!["--dedup-key", "   ", "-p", "x"],
        vec!["--dedup-key", "k", "--no-session", "-p", "x"],
        vec!["--dedup-key", "k", "--mode", "rpc"],
    ] {
        let output = cli()
            .arg("--cwd")
            .arg(dir.path())
            .args(&args)
            .output()
            .unwrap();
        assert!(!output.status.success(), "expected rejection for {args:?}");
    }
}

#[test]
fn active_dedup_key_is_not_replayed_after_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let ledger_dir = dir.path().join(".mio/dedup");
    std::fs::create_dir_all(&ledger_dir).unwrap();
    // 账本校验要求 task_id 是 UUID；这里模拟一个被强杀后停留在 active 的记录。
    let crashed_task_id = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";
    std::fs::write(
        ledger_dir.join("task-keys.json"),
        json!({
            "version": 1,
            "updated": 0,
            "entries": [
                {"task_id":crashed_task_id,"state":"active","key":"job-crash","updated":0}
            ]
        })
        .to_string(),
    )
    .unwrap();
    // 不可达端点证明命中 dedup 后没有触发模型调用。
    let output = cli()
        .arg("--cwd")
        .arg(dir.path())
        .args([
            "--model",
            "test",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--json",
            "--dedup-key",
            "job-crash",
            "-p",
            "retry",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let events = parse_json_lines(&output.stdout);
    let task = events
        .iter()
        .find(|event| event["type"] == "task")
        .expect("task event");
    assert_eq!(task["reused"], true);
    assert_eq!(task["task_id"], crashed_task_id);
    assert_eq!(task["state"], "active");
}

#[test]
fn corrupt_dedup_ledger_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let ledger_dir = dir.path().join(".mio/dedup");
    std::fs::create_dir_all(&ledger_dir).unwrap();
    std::fs::write(ledger_dir.join("task-keys.json"), "{not json").unwrap();
    let output = cli()
        .arg("--cwd")
        .arg(dir.path())
        .args([
            "--model",
            "test",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--dedup-key",
            "k",
            "-p",
            "x",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("task key ledger"),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// 不同进程并发抢同一 dedup key：只有一个进程真正发起模型请求，其余进程只回放
/// 已有的 task_id；所有进程报告的 task_id 必须一致，且账本里也只剩这一条记录。
#[test]
fn concurrent_processes_execute_one_dedup_key_once_with_a_shared_task_id() {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let start = Instant::now();
        let mut served = 0usize;
        let mut first_served: Option<Instant> = None;
        let mut last_request = None;
        loop {
            match listener.accept() {
                Ok((mut socket, _)) => {
                    socket
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let (_, request) = test_http::read_json(&mut socket);
                    served += 1;
                    last_request = Some(request);
                    first_served.get_or_insert_with(Instant::now);
                    write_completion(&mut socket);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    match first_served {
                        // 首个请求之后再留 2s 宽限期，确认没有第二个进程也去执行。
                        Some(at) if at.elapsed() > Duration::from_secs(2) => break,
                        Some(_) => {}
                        // 没等到任何请求时最多等 20s（正常情况应远早于此）。
                        None if start.elapsed() > Duration::from_secs(20) => break,
                        None => {}
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("{error}"),
            }
        }
        (served, last_request)
    });

    let base = format!("http://{address}/v1");
    let contenders = 4;
    let mut children = Vec::new();
    for _ in 0..contenders {
        children.push(
            cli()
                .arg("--cwd")
                .arg(dir.path())
                .args([
                    "--model",
                    "test",
                    "--base-url",
                    &base,
                    "--json",
                    "--dedup-key",
                    "shared-job",
                    "-p",
                    "do the shared job",
                ])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }
    let outputs: Vec<std::process::Output> = children
        .into_iter()
        .map(|child| child.wait_with_output().unwrap())
        .collect();
    for output in &outputs {
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let tasks: Vec<Value> = outputs
        .iter()
        .map(|output| {
            parse_json_lines(&output.stdout)
                .into_iter()
                .find(|event| event["type"] == "task")
                .expect("each process must report a task event")
        })
        .collect();
    let ids: std::collections::BTreeSet<String> = tasks
        .iter()
        .map(|task| task["task_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        ids.len(),
        1,
        "all processes must share one task id: {tasks:?}"
    );
    assert_eq!(
        tasks.iter().filter(|task| task["reused"] == false).count(),
        1,
        "exactly one process may acquire the key: {tasks:?}"
    );
    let (served, request) = server.join().unwrap();
    assert_eq!(served, 1, "only one real model request is allowed");
    assert!(
        request.unwrap()["messages"]
            .to_string()
            .contains("do the shared job")
    );
    let ledger: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join(".mio/dedup/task-keys.json")).unwrap(),
    )
    .unwrap();
    let entries = ledger["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "the ledger keeps exactly one record");
    assert_eq!(entries[0]["key"], "shared-job");
    assert_eq!(
        entries[0]["task_id"].as_str().unwrap(),
        ids.iter().next().unwrap()
    );
}
