//! Integration tests for the pluggable OS execution sandbox backends.
//!
//! Real bubblewrap cases run on Linux when `/usr/bin/bwrap` exists and the
//! kernel allows unprivileged namespaces. Set `MIO_REQUIRE_BWRAP=1` to turn a
//! missing backend into a failure instead of a skip. Structural cases use this
//! test binary as a stand-in sandbox helper, so they do not need bubblewrap.
#![cfg(unix)]

use mio_coding_agent::execution_backend::{
    BWRAP_PROGRAM, BackendError, BubblewrapBackend, BubblewrapPolicy, EnvironmentPolicy,
    ExecutionBackend, ExecutionPolicy, ExecutionRequest, FilesystemPolicy, HostPlatform,
    HostReadVisibility, IsolationGuarantees, ResourceLimits, SANDBOX_PATH, SandboxCommand,
    linux_runtime_mounts,
};
use mio_coding_agent::shell::{CommandOutput, run_supervised};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

fn existing(candidates: &[&str]) -> Option<PathBuf> {
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

fn shell_program() -> PathBuf {
    existing(&["/usr/bin/sh", "/bin/sh"]).expect("a POSIX shell is required for these tests")
}

fn sleep_program() -> PathBuf {
    existing(&["/usr/bin/sleep", "/bin/sleep"]).expect("sleep is required for the cleanup tests")
}

fn bash_program() -> Option<PathBuf> {
    existing(&["/usr/bin/bash", "/bin/bash"])
}

/// A backend whose "bwrap" program is this test binary: it exists and is
/// executable, so the backend reports itself as available and produces a plan
/// without needing real bubblewrap.
fn structural_backend(policy: BubblewrapPolicy) -> BubblewrapBackend {
    let program = std::env::current_exe().expect("test binary path");
    BubblewrapBackend::at(program, HostPlatform::Linux).with_policy(policy)
}

/// Real bubblewrap, or `None` when it cannot run here.
fn bubblewrap_backend() -> Option<BubblewrapBackend> {
    let backend = BubblewrapBackend::detect();
    let status = backend.status();
    if status.available {
        return Some(backend);
    }
    let reason = status
        .unavailable_reason
        .unwrap_or_else(|| "unknown reason".into());
    if std::env::var_os("MIO_REQUIRE_BWRAP").is_some() {
        panic!("bubblewrap backend is required but unavailable: {reason}");
    }
    eprintln!("skipping real bubblewrap test: {reason}");
    None
}

async fn run_plan(plan: &SandboxCommand, timeout: Duration) -> Result<CommandOutput, String> {
    let mut command =
        tokio::process::Command::from(plan.to_std_command().map_err(|error| error.to_string())?);
    command.stdin(Stdio::null());
    tokio::time::timeout(timeout, run_supervised(&mut command))
        .await
        .map_err(|_| format!("command timed out after {timeout:?}"))?
}

fn args_of(plan: &SandboxCommand) -> Vec<String> {
    plan.args()
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect()
}

fn unique_token() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    format!("mio-sandbox-{}-{nanos}", std::process::id())
}

#[tokio::test]
async fn unrestricted_backend_is_explicit_and_executes() {
    let workspace = tempfile::tempdir().unwrap();
    let status = ExecutionPolicy::Unrestricted.status();
    assert_eq!(status.name, "unrestricted");
    assert!(status.available);
    assert!(status.unavailable_reason.is_none());
    assert_eq!(status.guarantees, IsolationGuarantees::NONE);
    assert!(!status.guarantees.confines_host_reads());
    assert!(status.program.is_none());
    assert!(!status.caveats.is_empty());

    let request = ExecutionRequest::new(shell_program(), workspace.path())
        .args(["-c", "echo direct-ok && pwd"]);
    let plan = ExecutionPolicy::Unrestricted
        .resolve(&request)
        .expect("unrestricted backend always resolves");
    assert_eq!(plan.backend(), "unrestricted");
    assert!(matches!(plan.environment(), EnvironmentPolicy::Inherit));
    assert!(plan.limits().is_unset());

    let output = run_plan(&plan, COMMAND_TIMEOUT).await.unwrap();
    assert!(output.status.success(), "{:?}", output);
    assert!(output.stdout.contains("direct-ok"));
}

#[test]
fn missing_bubblewrap_binary_is_reported_and_refused() {
    let workspace = tempfile::tempdir().unwrap();
    let backend = BubblewrapBackend::at("/nonexistent/mio-bwrap", HostPlatform::Linux);
    let status = backend.status();
    assert!(!status.available);
    let reason = status
        .unavailable_reason
        .clone()
        .expect("an unavailable backend must explain why");
    assert!(reason.contains("/nonexistent/mio-bwrap"), "{reason}");
    assert_eq!(
        status.program.as_deref(),
        Some(Path::new("/nonexistent/mio-bwrap"))
    );

    let request = ExecutionRequest::new(shell_program(), workspace.path())
        .args(["-c", "echo should-never-run"]);
    let error = backend
        .wrap(&request)
        .expect_err("a missing sandbox must not fall back to an unsandboxed command");
    match error {
        BackendError::Unavailable { backend, reason } => {
            assert_eq!(backend, "bubblewrap");
            assert!(reason.contains("/nonexistent/mio-bwrap"), "{reason}");
        }
        other => panic!("expected an unavailability error, got {other:?}"),
    }
}

#[test]
fn unsupported_platform_is_refused_explicitly() {
    let workspace = tempfile::tempdir().unwrap();
    let backend = BubblewrapBackend::at(BWRAP_PROGRAM, HostPlatform::Unsupported);
    let status = backend.status();
    assert!(!status.available);
    let reason = status.unavailable_reason.clone().unwrap_or_default();
    assert!(reason.contains("Linux"), "{reason}");

    let request = ExecutionRequest::new(shell_program(), workspace.path());
    assert!(matches!(
        backend.wrap(&request),
        Err(BackendError::Unavailable { .. })
    ));
}

#[tokio::test]
async fn real_bubblewrap_status_lists_guarantees_and_caveats() {
    let Some(backend) = bubblewrap_backend() else {
        return;
    };
    let status = backend.status();
    assert_eq!(status.name, "bubblewrap");
    assert!(status.available);
    assert!(status.unavailable_reason.is_none());
    assert_eq!(status.program.as_deref(), Some(Path::new(BWRAP_PROGRAM)));
    let guarantees = status.guarantees;
    assert!(guarantees.workspace_only_writes);
    assert!(guarantees.network_unshared);
    assert!(guarantees.private_tmp);
    assert!(guarantees.private_pid);
    assert!(guarantees.process_exit_cleanup);
    assert!(guarantees.resource_limits);
    assert_eq!(
        guarantees.host_read_visibility,
        HostReadVisibility::HostRootReadOnly
    );
    assert!(
        !guarantees.confines_host_reads(),
        "a read-only host root is not read confinement"
    );
    assert!(
        status.caveats.iter().any(|caveat| caveat.contains("reads")),
        "the read-visibility caveat must be reported: {:?}",
        status.caveats
    );

    // Only an explicit mount list upgrades the read claim.
    let explicit = backend.with_policy(BubblewrapPolicy::default().with_filesystem(
        FilesystemPolicy::ExplicitMounts {
            read_only: linux_runtime_mounts(),
        },
    ));
    let explicit_status = explicit.status();
    assert!(explicit_status.available);
    assert!(explicit_status.guarantees.confines_host_reads());
    assert_eq!(
        explicit_status.guarantees.host_read_visibility,
        HostReadVisibility::ExplicitMountsOnly
    );
}

#[tokio::test]
async fn writes_are_confined_to_the_workspace() {
    let Some(backend) = bubblewrap_backend() else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let token = unique_token();
    let etc_probe = PathBuf::from(format!("/etc/{token}"));
    let var_probe = PathBuf::from(format!("/var/tmp/{token}"));
    std::os::unix::fs::symlink("/etc", workspace.join("escape")).unwrap();

    let script = format!(
        "echo inside-ok > inside.txt && cat inside.txt; \
         echo '--etc'; echo x > {etc} 2>&1; echo rc=$?; \
         echo '--var'; echo x > {var} 2>&1; echo rc=$?; \
         echo '--symlink'; echo x > escape/{token} 2>&1; echo rc=$?; \
         echo '--read'; head -c 24 /etc/passwd",
        etc = etc_probe.display(),
        var = var_probe.display(),
    );
    let plan = backend
        .wrap(&ExecutionRequest::new(shell_program(), &workspace).args(["-c", &script]))
        .unwrap();
    let output = run_plan(&plan, COMMAND_TIMEOUT).await.unwrap();
    let combined = output.combined();

    assert!(
        output.status.success(),
        "sandboxed shell failed: {combined}"
    );
    assert!(combined.contains("inside-ok"), "{combined}");
    assert_eq!(
        std::fs::read_to_string(workspace.join("inside.txt")).unwrap(),
        "inside-ok\n"
    );
    assert!(
        combined.contains("Read-only file system"),
        "outside writes must fail: {combined}"
    );
    assert!(
        combined.matches("Read-only file system").count() >= 2,
        "both the direct and symlinked escape write must fail: {combined}"
    );
    assert!(!etc_probe.exists(), "wrote outside the workspace");
    assert!(!var_probe.exists(), "wrote outside the workspace");
    assert!(
        !Path::new(&format!("/etc/{token}")).exists(),
        "wrote through the workspace symlink"
    );
    assert!(
        combined.contains("root:"),
        "a read-only host root still exposes host files the user can read: {combined}"
    );
}

#[tokio::test]
async fn explicit_mounts_confine_host_reads() {
    let Some(backend) = bubblewrap_backend() else {
        return;
    };
    assert!(
        Path::new("/etc/passwd").is_file(),
        "fixture assumption: /etc/passwd exists on this host"
    );
    let workspace = tempfile::tempdir().unwrap();
    let policy = BubblewrapPolicy::default().with_filesystem(FilesystemPolicy::ExplicitMounts {
        read_only: linux_runtime_mounts(),
    });
    let script =
        "cat /etc/passwd 2>&1 | head -c 40; echo; echo inside-ok > written.txt && echo wrote";
    let plan = backend
        .with_policy(policy)
        .wrap(&ExecutionRequest::new(shell_program(), workspace.path()).args(["-c", script]))
        .unwrap();
    let output = run_plan(&plan, COMMAND_TIMEOUT).await.unwrap();
    let combined = output.combined();

    assert!(output.status.success(), "{combined}");
    assert!(
        combined.contains("No such file"),
        "an unmounted host path must be invisible: {combined}"
    );
    assert!(!combined.contains("root:"), "{combined}");
    assert!(combined.contains("wrote"), "{combined}");
    assert!(workspace.path().join("written.txt").is_file());
}

#[tokio::test]
async fn network_namespace_is_unshared() {
    let Some(backend) = bubblewrap_backend() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();
    let host_net = std::fs::read_link("/proc/self/ns/net")
        .expect("host /proc/self/ns/net")
        .to_string_lossy()
        .into_owned();
    let script = "cat /proc/self/ns/net; echo '--dev'; cat /proc/net/dev";
    let plan = backend
        .wrap(&ExecutionRequest::new(shell_program(), workspace.path()).args(["-c", script]))
        .unwrap();
    let output = run_plan(&plan, COMMAND_TIMEOUT).await.unwrap();
    assert!(output.status.success(), "{:?}", output);
    let sandbox_net = output
        .stdout
        .lines()
        .next()
        .expect("ns/net output")
        .trim()
        .to_string();

    assert_ne!(
        sandbox_net, host_net,
        "the sandbox must run in a different network namespace"
    );
    assert!(output.stdout.contains("lo:"), "{}", output.stdout);
    assert!(
        !output.stdout.contains("eth0") && !output.stdout.contains("enp"),
        "only the loopback interface may exist: {}",
        output.stdout
    );

    // A real connect attempt must fail; /dev/tcp needs bash.
    if let Some(bash) = bash_program() {
        let plan = backend
            .wrap(
                &ExecutionRequest::new(bash, workspace.path())
                    .args(["-c", "exec 3<>/dev/tcp/1.1.1.1/53"]),
            )
            .unwrap();
        let output = run_plan(&plan, Duration::from_secs(20)).await.unwrap();
        assert!(
            !output.status.success(),
            "an outbound connection must not succeed inside the sandbox: {:?}",
            output
        );
    } else {
        eprintln!("bash not found; skipped the live connect probe");
    }
}

#[tokio::test]
async fn private_tmp_and_pid_namespaces() {
    let Some(backend) = bubblewrap_backend() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();
    let token = unique_token();
    let host_tmp_probe = Path::new("/tmp").join(&token);
    let host_pid = std::fs::read_link("/proc/self/ns/pid")
        .expect("host /proc/self/ns/pid")
        .to_string_lossy()
        .into_owned();
    let script = format!(
        "cat /proc/self/ns/pid; echo '--tmp'; echo marker > /tmp/{token} && cat /tmp/{token}"
    );
    let plan = backend
        .wrap(&ExecutionRequest::new(shell_program(), workspace.path()).args(["-c", &script]))
        .unwrap();
    let output = run_plan(&plan, COMMAND_TIMEOUT).await.unwrap();
    assert!(output.status.success(), "{:?}", output);
    let sandbox_pid = output
        .stdout
        .lines()
        .next()
        .expect("ns/pid output")
        .trim()
        .to_string();

    assert_ne!(
        sandbox_pid, host_pid,
        "the sandbox must run in a different PID namespace"
    );
    assert!(output.stdout.contains("marker"), "{}", output.stdout);
    assert!(
        !host_tmp_probe.exists(),
        "the sandbox wrote into the host /tmp"
    );
}

#[tokio::test]
async fn cpu_limit_stops_a_runaway_process() {
    let Some(backend) = bubblewrap_backend() else {
        return;
    };
    use std::os::unix::process::ExitStatusExt;
    let workspace = tempfile::tempdir().unwrap();
    let policy = BubblewrapPolicy::default().with_limits(ResourceLimits {
        cpu_seconds: Some(1),
        ..ResourceLimits::default()
    });
    let plan = backend
        .with_policy(policy)
        .wrap(
            &ExecutionRequest::new(shell_program(), workspace.path())
                .args(["-c", "while :; do :; done"]),
        )
        .unwrap();
    let started = Instant::now();
    let output = run_plan(&plan, Duration::from_secs(30)).await.unwrap();

    assert!(
        !output.status.success(),
        "the runaway loop must be stopped: {:?}",
        output
    );
    let killed = |signal: i32| {
        output.status.signal() == Some(signal) || output.status.code() == Some(128 + signal)
    };
    assert!(
        killed(libc::SIGXCPU) || killed(libc::SIGKILL),
        "expected a CPU-limit signal, got {:?}",
        output.status
    );
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the CPU limit did not apply in time"
    );
}

#[tokio::test]
async fn file_descriptor_limit_is_enforced() {
    let Some(backend) = bubblewrap_backend() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();
    let policy = BubblewrapPolicy::default().with_limits(ResourceLimits {
        open_files: Some(32),
        ..ResourceLimits::default()
    });
    let script = "echo \"nofile=$(ulimit -n)\"; \
         i=0; while [ $i -lt 200 ]; do eval \"exec $((i+10))>/dev/null\" || { echo fd-limit-hit; exit 3; }; i=$((i+1)); done; \
         echo all-opened";
    let plan = backend
        .with_policy(policy)
        .wrap(&ExecutionRequest::new(shell_program(), workspace.path()).args(["-c", script]))
        .unwrap();
    let output = run_plan(&plan, COMMAND_TIMEOUT).await.unwrap();
    let combined = output.combined();

    assert!(combined.contains("nofile=32"), "{combined}");
    assert!(!output.status.success(), "{combined}");
    assert!(combined.contains("fd-limit-hit"), "{combined}");
    assert!(!combined.contains("all-opened"), "{combined}");
}

#[tokio::test]
async fn address_space_limit_is_enforced() {
    let Some(backend) = bubblewrap_backend() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();
    let policy = BubblewrapPolicy::default().with_limits(ResourceLimits {
        address_space_bytes: Some(256 * 1024 * 1024),
        ..ResourceLimits::default()
    });
    let script = "echo \"vlimit=$(ulimit -v)\"; dd if=/dev/zero of=/dev/null bs=536870912 count=1";
    let plan = backend
        .with_policy(policy)
        .wrap(&ExecutionRequest::new(shell_program(), workspace.path()).args(["-c", script]))
        .unwrap();
    let output = run_plan(&plan, COMMAND_TIMEOUT).await.unwrap();
    let combined = output.combined();

    assert!(combined.contains("vlimit=262144"), "{combined}");
    assert!(!output.status.success(), "{combined}");
    assert!(combined.contains("memory exhausted"), "{combined}");
}

#[cfg(target_os = "linux")]
fn find_process(comm: &str, marker: &str) -> Option<i32> {
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|name| name.parse::<i32>().ok()) else {
            continue;
        };
        let process = entry.path();
        let Ok(comm_text) = std::fs::read_to_string(process.join("comm")) else {
            continue;
        };
        if comm_text.trim() != comm {
            continue;
        }
        let Ok(raw) = std::fs::read(process.join("cmdline")) else {
            continue;
        };
        let cmdline = String::from_utf8_lossy(&raw).replace('\0', " ");
        if cmdline.contains(marker) {
            return Some(pid);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn wait_for_process(comm: &str, marker: &str, deadline: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < deadline {
        if find_process(comm, marker).is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[cfg(target_os = "linux")]
fn wait_for_process_gone(comm: &str, marker: &str, deadline: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < deadline {
        if find_process(comm, marker).is_none() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[cfg(target_os = "linux")]
#[test]
fn sandboxed_processes_die_with_the_sandbox() {
    let Some(backend) = bubblewrap_backend() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    let marker = format!("990{}.{:06}", nanos % 1000, nanos % 1_000_000);
    let request = ExecutionRequest::new(sleep_program(), workspace.path()).arg(&marker);

    let spawn = |plan: &SandboxCommand| {
        plan.to_std_command()
            .unwrap()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };

    // Default policy: the sandboxed process dies with its sandbox.
    let plan = backend.wrap(&request).unwrap();
    assert!(args_of(&plan).iter().any(|arg| arg == "--die-with-parent"));
    let mut child = spawn(&plan);
    assert!(
        wait_for_process("sleep", &marker, Duration::from_secs(10)),
        "the sandboxed sleep never started"
    );
    child.kill().unwrap();
    let _ = child.wait();
    assert!(
        wait_for_process_gone("sleep", &marker, Duration::from_secs(10)),
        "the sandboxed process survived its sandbox"
    );

    // Without the flag the sandboxed process is orphaned, which is exactly the
    // behaviour the default policy prevents.
    let relaxed = BubblewrapPolicy {
        die_with_parent: false,
        ..BubblewrapPolicy::default()
    };
    let plan = backend.with_policy(relaxed).wrap(&request).unwrap();
    let mut child = spawn(&plan);
    assert!(
        wait_for_process("sleep", &marker, Duration::from_secs(10)),
        "the relaxed sandboxed sleep never started"
    );
    child.kill().unwrap();
    let _ = child.wait();
    std::thread::sleep(Duration::from_millis(500));
    let survivor = find_process("sleep", &marker);
    assert!(
        survivor.is_some(),
        "expected the sandboxed process to survive without --die-with-parent"
    );
    if let Some(pid) = survivor {
        // SAFETY: signal a process this test created.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    assert!(
        wait_for_process_gone("sleep", &marker, Duration::from_secs(5)),
        "failed to clean up the survivor"
    );
}

#[tokio::test]
async fn sandbox_default_environment_hides_host_variables() {
    let Some(backend) = bubblewrap_backend() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(workspace.path()).unwrap();
    let request = ExecutionRequest::new(shell_program(), workspace.path()).args(["-c", "env"]);
    let plan = backend.wrap(&request).unwrap();
    let output = run_plan(&plan, COMMAND_TIMEOUT).await.unwrap();
    assert!(output.status.success(), "{:?}", output);

    let environment: BTreeMap<&str, &str> = output
        .stdout
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect();
    assert_eq!(environment.get("PATH").copied(), Some(SANDBOX_PATH));
    assert_eq!(
        environment.get("HOME").copied(),
        Some(canonical.to_str().unwrap())
    );
    assert_eq!(environment.get("TMPDIR").copied(), Some("/tmp"));
    for host_only in ["TERM", "USER", "LOGNAME", "SSH_AUTH_SOCK"] {
        assert!(
            !environment.contains_key(host_only),
            "{host_only} leaked into the sandbox"
        );
    }
}

#[tokio::test]
async fn explicit_environment_is_passed_without_inheriting_the_rest() {
    let Some(backend) = bubblewrap_backend() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();
    let policy = BubblewrapPolicy::default().with_environment(EnvironmentPolicy::Explicit(vec![
        ("PATH".into(), SANDBOX_PATH.into()),
        ("MIO_TEST_PROBE".into(), "probe-value".into()),
    ]));
    let plan = backend
        .with_policy(policy)
        .wrap(&ExecutionRequest::new(shell_program(), workspace.path()).args(["-c", "env"]))
        .unwrap();
    let rendered = format!("{plan:?}");
    assert!(rendered.contains("MIO_TEST_PROBE"));
    assert!(!rendered.contains("probe-value"), "{rendered}");

    let output = run_plan(&plan, COMMAND_TIMEOUT).await.unwrap();
    assert!(output.status.success(), "{:?}", output);
    let environment: BTreeMap<&str, &str> = output
        .stdout
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect();
    assert_eq!(
        environment.get("MIO_TEST_PROBE").copied(),
        Some("probe-value")
    );
    assert_eq!(environment.get("PATH").copied(), Some(SANDBOX_PATH));
    for host_only in ["TERM", "USER", "LOGNAME"] {
        assert!(
            !environment.contains_key(host_only),
            "{host_only} leaked into the sandbox"
        );
    }
}

#[test]
fn plan_arguments_express_the_requested_isolation() {
    let workspace = tempfile::tempdir().unwrap();
    let canonical_workspace = std::fs::canonicalize(workspace.path()).unwrap();
    let program = std::fs::canonicalize(shell_program()).unwrap();
    let request = ExecutionRequest::new(&program, workspace.path()).args(["-c", "echo ok"]);

    let plan = structural_backend(BubblewrapPolicy::default())
        .wrap(&request)
        .unwrap();
    let args = args_of(&plan);
    let joined = args.join(" ");
    for expected in [
        "--die-with-parent",
        "--new-session",
        "--ro-bind / /",
        "--tmpfs /tmp",
        "--proc /proc",
        "--dev /dev",
        "--unshare-net",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
    ] {
        assert!(joined.contains(expected), "missing {expected}: {joined}");
    }
    let bind = args.iter().position(|arg| arg == "--bind").unwrap();
    assert_eq!(args[bind + 1], canonical_workspace.to_string_lossy());
    assert_eq!(args[bind + 2], canonical_workspace.to_string_lossy());
    let chdir = args.iter().position(|arg| arg == "--chdir").unwrap();
    assert_eq!(args[chdir + 1], canonical_workspace.to_string_lossy());
    let separator = args.iter().position(|arg| arg == "--").unwrap();
    assert_eq!(args[separator + 1], program.to_string_lossy());
    assert_eq!(args[separator + 2], "-c");
    assert_eq!(args[separator + 3], "echo ok");
    assert_eq!(plan.working_directory(), canonical_workspace.as_path());
    assert_eq!(plan.backend(), "bubblewrap");
    assert!(plan.program().is_file());

    // Turning isolation off is reflected in the plan and its guarantees.
    let relaxed_policy = BubblewrapPolicy {
        disable_network: false,
        private_tmp: false,
        private_pid: false,
        die_with_parent: false,
        new_session: false,
        unshare_ipc: false,
        unshare_uts: false,
        ..BubblewrapPolicy::default()
    };
    let relaxed = structural_backend(relaxed_policy.clone())
        .wrap(&request)
        .unwrap();
    let relaxed_args = args_of(&relaxed);
    for absent in [
        "--unshare-net",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--die-with-parent",
        "--new-session",
        "--tmpfs",
    ] {
        assert!(
            !relaxed_args.iter().any(|arg| arg == absent),
            "{absent} present after disabling it"
        );
    }
    let relaxed_guarantees = relaxed.guarantees();
    assert!(!relaxed_guarantees.network_unshared);
    assert!(!relaxed_guarantees.private_tmp);
    assert!(!relaxed_guarantees.private_pid);
    assert!(!relaxed_guarantees.process_exit_cleanup);
    assert!(relaxed_guarantees.workspace_only_writes);
    assert!(relaxed_guarantees.resource_limits);
}

#[test]
fn apply_overwrites_a_caller_owned_command() {
    let workspace = tempfile::tempdir().unwrap();
    let request = ExecutionRequest::new(shell_program(), workspace.path()).args(["-c", "echo ok"]);
    let plan = structural_backend(BubblewrapPolicy::default())
        .wrap(&request)
        .unwrap();

    let mut command = Command::new("/bin/false");
    command.arg("--stale");
    plan.apply(&mut command).unwrap();
    assert_eq!(command.get_program(), plan.program().as_os_str());
    let arguments: Vec<_> = command.get_args().collect();
    let expected: Vec<_> = plan.args().iter().map(OsString::as_os_str).collect();
    assert_eq!(arguments, expected);
    assert_eq!(command.get_current_dir(), Some(plan.working_directory()));
    assert_eq!(command.get_envs().count(), 4);

    let built = plan.to_std_command().unwrap();
    assert_eq!(built.get_program(), plan.program().as_os_str());
}

#[test]
fn invalid_requests_are_rejected() {
    let workspace = tempfile::tempdir().unwrap();
    let backend = structural_backend(BubblewrapPolicy::default());
    let program = shell_program();

    let error = backend
        .wrap(&ExecutionRequest::new(
            &program,
            "/nonexistent/mio-workspace",
        ))
        .unwrap_err();
    assert!(matches!(error, BackendError::InvalidRequest(_)), "{error}");

    let error = backend
        .wrap(&ExecutionRequest::new("sh", workspace.path()))
        .unwrap_err();
    assert!(matches!(error, BackendError::InvalidRequest(_)), "{error}");

    let limits = ResourceLimits {
        cpu_seconds: Some(0),
        ..ResourceLimits::default()
    };
    let error = structural_backend(BubblewrapPolicy::default().with_limits(limits))
        .wrap(&ExecutionRequest::new(&program, workspace.path()))
        .unwrap_err();
    assert!(matches!(error, BackendError::InvalidRequest(_)), "{error}");

    let explicit = |read_only: Vec<PathBuf>| {
        structural_backend(
            BubblewrapPolicy::default()
                .with_filesystem(FilesystemPolicy::ExplicitMounts { read_only }),
        )
    };
    let request = ExecutionRequest::new(&program, workspace.path());
    assert!(matches!(
        explicit(Vec::new()).wrap(&request),
        Err(BackendError::InvalidRequest(_))
    ));
    assert!(matches!(
        explicit(vec![PathBuf::from("/")]).wrap(&request),
        Err(BackendError::InvalidRequest(_))
    ));
    assert!(matches!(
        explicit(vec![PathBuf::from("/nonexistent/mio-mount")]).wrap(&request),
        Err(BackendError::InvalidRequest(_))
    ));
    assert!(matches!(
        explicit(vec![PathBuf::from("/etc")]).wrap(&request),
        Err(BackendError::InvalidRequest(_))
    ));
    // /tmp is replaced by a private tmpfs, so a read-only mount there would be
    // silently shadowed; reject it instead.
    assert!(matches!(
        explicit(vec![PathBuf::from("/tmp")]).wrap(&request),
        Err(BackendError::InvalidRequest(_))
    ));
}
