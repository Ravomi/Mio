//! Offline backend discovery and fail-closed CLI configuration.
use serde_json::Value;
use std::process::Command;

fn command(root: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mio"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("MIO_") {
            command.env_remove(key);
        }
    }
    command
        .env("MIO_CONFIG_DIR", root.join("config"))
        .env("MIO_PROVIDER", "invalid-provider-must-not-be-loaded")
        .arg("--cwd")
        .arg(root)
        .arg("--json");
    command
}

#[test]
fn backend_status_is_offline_and_default_is_explicitly_unrestricted() {
    let directory = tempfile::tempdir().unwrap();
    let output = command(directory.path())
        .arg("--sandbox-status")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let event: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(event["type"], "backend_status");
    assert_eq!(event["data"]["backend"], "unrestricted");
    assert_eq!(event["data"]["hostReads"], "full_host_access");
    assert_eq!(event["data"]["workspaceOnlyWrites"], false);
    assert_eq!(event["data"]["networkUnshared"], false);
    assert!(!directory.path().join(".mio").exists());
}

#[test]
fn invalid_backend_or_unapplied_resource_limit_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    for flags in [
        vec!["--sandbox", "unknown", "--sandbox-status"],
        vec!["--sandbox-cpu-seconds", "1", "--sandbox-status"],
        vec![
            "--sandbox",
            "bubblewrap",
            "--sandbox-memory-mib",
            "0",
            "--sandbox-status",
        ],
        vec![
            "--sandbox",
            "bubblewrap",
            "--sandbox-open-files",
            "2",
            "--sandbox-status",
        ],
    ] {
        let output = command(directory.path()).args(&flags).output().unwrap();
        assert!(!output.status.success(), "accepted {flags:?}");
    }
}

#[cfg(target_os = "linux")]
#[path = "../src/test_http.rs"]
mod test_http;

/// Shell probe that reports whether it could read `secret` and write `escaped`.
///
/// The markers are printed, never echoed back as command text, so a tool result
/// can be classified without matching the request that carried the command.
#[cfg(target_os = "linux")]
fn shell_boundary_probe(secret: &std::path::Path, escaped: &std::path::Path) -> String {
    format!(
        "if cat '{}' >/dev/null 2>&1; then printf READ_LEAK; else printf READ_BLOCKED; fi; if printf leak > '{}'; then printf WRITE_LEAK; else printf WRITE_BLOCKED; fi; printf allowed > inside.txt",
        secret.display(),
        escaped.display()
    )
}

/// Drive the real CLI tool loop, rather than inferring routing from status JSON.
///
/// Returns the tool message the CLI sent back on the continuation request, which
/// is what a model would actually see.
#[cfg(target_os = "linux")]
fn tool_result_through_cli(
    workspace: &std::path::Path,
    backend: &str,
    shell: &str,
    read_only: Option<&std::path::Path>,
) -> String {
    use serde_json::json;
    use std::{
        io::Write,
        net::TcpListener,
        time::{Duration, Instant},
    };

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let shell = shell.to_owned();
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for turn in 0..2 {
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "missing CLI request {turn}");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            requests.push(test_http::read_json(&mut socket).1);
            let delta = if turn == 0 {
                json!({"role":"assistant","tool_calls":[{"index":0,"id":"sandbox-call","type":"function","function":{"name":"workspace","arguments":json!({"operation":"shell","value":shell}).to_string()}}]})
            } else {
                json!({"role":"assistant","content":"verified"})
            };
            let chunk = |delta: Value, finish: Value| {
                json!({"id":"test","object":"chat.completion.chunk","created":0,"model":"test","choices":[{"index":0,"delta":delta,"finish_reason":finish}]}).to_string()
            };
            let body = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                chunk(delta, Value::Null),
                chunk(
                    json!({}),
                    json!(if turn == 0 { "tool_calls" } else { "stop" })
                )
            );
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        }
        requests
    });
    let mut cli = command(workspace);
    cli.env("MIO_PROVIDER", "custom")
        .env("MIO_API_KEY", "local-test-credential")
        .env_remove("OPENAI_API_KEY")
        .args([
            "--model",
            "test",
            "--base-url",
            &format!("http://{address}/v1"),
            "--sandbox",
            backend,
            "--no-session",
            "-p",
            "Run the shell probe",
        ]);
    if let Some(path) = read_only {
        cli.arg("--sandbox-read-only").arg(path);
    }
    let output = cli.output().unwrap();
    assert!(
        output.status.success(),
        "sandbox={backend} stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = server.join().unwrap();
    let messages = requests[1]["messages"].as_array().unwrap();
    let tool_result = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("actual shell result in continuation");
    tool_result["content"].to_string()
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cli_bubblewrap_policy_reaches_actual_shell_dispatch() {
    use mio_coding_agent::execution_backend::{
        BackendProbe, BubblewrapPolicy, ExecutionPolicy, FilesystemPolicy, linux_runtime_mounts,
    };

    let workspace = tempfile::tempdir().unwrap();
    // /tmp is deliberately replaced by private tmpfs. Use another host
    // location to test an explicit read grant without colliding with it.
    let outside = tempfile::Builder::new()
        .prefix("mio-sandbox-read-grant-")
        .tempdir_in("/var/tmp")
        .unwrap();
    let secret = outside.path().join("secret.txt");
    let escaped = outside.path().join("escaped.txt");
    std::fs::write(&secret, "outside-secret-must-not-be-readable").unwrap();
    let policy = ExecutionPolicy::Bubblewrap(BubblewrapPolicy::default().with_filesystem(
        FilesystemPolicy::ExplicitMounts {
            read_only: linux_runtime_mounts(),
        },
    ));
    let status = policy.probe(workspace.path()).await;
    if status.probe != BackendProbe::Verified {
        assert!(
            std::env::var_os("MIO_REQUIRE_BWRAP").is_none(),
            "required sandbox unavailable: {status:?}"
        );
        eprintln!("sandbox unavailable; real CLI routing not verified: {status:?}");
        return;
    }
    let result = tool_result_through_cli(
        workspace.path(),
        "bubblewrap",
        &shell_boundary_probe(&secret, &escaped),
        None,
    );
    assert!(result.contains("READ_BLOCKED"), "{result}");
    assert!(result.contains("WRITE_BLOCKED"), "{result}");
    assert!(!result.contains("READ_LEAK"), "{result}");
    assert!(!result.contains("WRITE_LEAK"), "{result}");
    assert!(!escaped.exists());
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("inside.txt")).unwrap(),
        "allowed"
    );
    let mounted = tool_result_through_cli(
        workspace.path(),
        "bubblewrap",
        &shell_boundary_probe(&secret, &escaped),
        Some(outside.path()),
    );
    assert!(
        mounted.contains("READ_LEAK"),
        "explicit read grant: {mounted}"
    );
    assert!(
        mounted.contains("WRITE_BLOCKED"),
        "read-only grant: {mounted}"
    );
    assert!(!mounted.contains("WRITE_LEAK"), "{mounted}");
    assert!(!escaped.exists());
}

/// Positive control: the identical probe must leak once the default backend is
/// in effect. Without it, the isolation assertions above could pass because the
/// probe never ran, not because the sandbox blocked it.
#[cfg(target_os = "linux")]
#[test]
fn unrestricted_cli_shell_dispatch_reaches_the_host_filesystem() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.txt");
    let escaped = outside.path().join("escaped.txt");
    std::fs::write(&secret, "outside-secret-must-not-be-readable").unwrap();
    let result = tool_result_through_cli(
        workspace.path(),
        "unrestricted",
        &shell_boundary_probe(&secret, &escaped),
        None,
    );
    assert!(result.contains("READ_LEAK"), "{result}");
    assert!(result.contains("WRITE_LEAK"), "{result}");
    assert!(!result.contains("READ_BLOCKED"), "{result}");
    assert!(!result.contains("WRITE_BLOCKED"), "{result}");
    assert_eq!(std::fs::read_to_string(&escaped).unwrap(), "leak");
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("inside.txt")).unwrap(),
        "allowed"
    );
}

#[test]
fn read_mounts_require_isolation_absolute_paths_and_exclude_the_host_root() {
    let workspace = tempfile::tempdir().unwrap();
    for flags in [
        vec!["--sandbox-read-only", "/", "--sandbox-status"],
        vec![
            "--sandbox",
            "bubblewrap",
            "--sandbox-read-only",
            "/",
            "--sandbox-status",
        ],
        vec![
            "--sandbox",
            "bubblewrap",
            "--sandbox-read-only",
            "relative",
            "--sandbox-status",
        ],
    ] {
        let output = command(workspace.path()).args(&flags).output().unwrap();
        assert!(!output.status.success(), "accepted {flags:?}");
    }
    #[cfg(unix)]
    {
        let alias = workspace.path().join("root-alias");
        std::os::unix::fs::symlink("/", &alias).unwrap();
        let output = command(workspace.path())
            .args(["--sandbox", "bubblewrap", "--sandbox-read-only"])
            .arg(alias)
            .arg("--sandbox-status")
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "accepted an alias for the host root"
        );
    }
}
