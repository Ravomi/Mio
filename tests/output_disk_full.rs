//! Real Linux ENOSPC acceptance for externalized tool output.
//!
//! This case does not simulate a full disk with a mock writer. It runs the
//! current test binary as a child inside a real bubblewrap sandbox whose
//! workspace is a small (~128 KiB) `tmpfs`, then has that child call
//! `shell::externalize_output` with ~256 KiB. The kernel itself must fail the
//! spill write with `ENOSPC`, so the acceptance is about the real failure path
//! rather than a permission, read-only-mount or lease error:
//!
//! - the capture must return `Err` and must not report a truncated success;
//! - the failed transaction must leave no new complete output and no temp file;
//! - the output index must not gain a successful entry;
//! - pre-existing content and its index entry must be byte-for-byte unchanged.
//!
//! Boundaries:
//! - Linux only (`cfg(target_os = "linux")`).
//! - A real bubblewrap backend is required to execute the case. When
//!   `/usr/bin/bwrap` is unavailable the test skips locally and prints the
//!   reason; `MIO_REQUIRE_BWRAP=1` turns that skip into a hard failure, so a
//!   job that claims real disk-full evidence cannot pass by skipping.
//! - The child never builds a nested sandbox: it is a private-marker,
//!   `--exact` branch of this same test binary.
//! - Writes never reach the host root. Only empty host directories are created
//!   under `/tmp/mio-output-enospc-qa/` and removed again.
//! - The sandbox reads the host root read-only, but the workspace activity
//!   runtime directory is bound read-write so the real
//!   `workspace_activity::runtime_directory()` lock keeps its production
//!   semantics. Only the normal per-workspace `<sha256>.lock` is created there,
//!   and only when this run created it.
#![cfg(target_os = "linux")]

use mio_coding_agent::execution_backend::{BubblewrapBackend, ExecutionBackend};
use mio_coding_agent::shell::{
    CommandOutput, externalize_output,
    output_store::{OutputEntry, OutputStore},
    run_supervised, workspace_activity,
};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

/// Fixed, isolated host scratch root. The sandboxed workspace lives one level
/// below it so the tmpfs mount point exists in the read-only host root.
const BASE_DIR: &str = "/tmp/mio-output-enospc-qa";
/// Size of the sandboxed workspace tmpfs: large enough for the small seed
/// artifacts, far too small for the 256 KiB payload.
const WORKSPACE_TMPFS_BYTES: usize = 128 * 1024;
/// Deliberately over the tmpfs limit and under the 8 MiB hard output bound, so
/// a successful call would publish a truncated reference instead of failing.
const PAYLOAD_BYTES: usize = 256 * 1024;
const CHILD_MARKER: &str = "MIO_OUTPUT_DISK_FULL_CHILD";
const CHILD_WORKSPACE: &str = "MIO_OUTPUT_DISK_FULL_WORKSPACE";
const CHILD_RUNTIME_DIR: &str = "MIO_OUTPUT_DISK_FULL_RUNTIME_DIR";
const CHILD_TEST: &str = "enospc_child_probe";
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);

/// Real bubblewrap, or `None` when the case must skip here.
fn bubblewrap_for_real_disk_full() -> Option<BubblewrapBackend> {
    let backend = BubblewrapBackend::detect();
    let status = backend.status();
    if status.available {
        return Some(backend);
    }
    let reason = status
        .unavailable_reason
        .unwrap_or_else(|| "unknown reason".into());
    match unavailable_outcome(require_bwrap(), &reason) {
        Ok(()) => {
            eprintln!("skipping real bubblewrap output disk-full test: {reason}");
            None
        }
        Err(message) => panic!("{message}"),
    }
}

/// Decide what an unavailable backend means. `MIO_REQUIRE_BWRAP` must fail,
/// never silently skip the only real disk-full evidence this file provides.
fn unavailable_outcome(required: bool, reason: &str) -> Result<(), String> {
    if required {
        Err(format!(
            "MIO_REQUIRE_BWRAP is set but bubblewrap is unavailable: {reason}"
        ))
    } else {
        Ok(())
    }
}

fn require_bwrap() -> bool {
    std::env::var_os("MIO_REQUIRE_BWRAP").is_some()
}

/// Remove a host scratch tree created by this test, including on panic.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Remove the workspace activity lock file only when this run created it.
struct ActivityLockCleanup {
    path: PathBuf,
    pre_existing: bool,
}

impl ActivityLockCleanup {
    fn for_workspace(workspace: &Path) -> Option<Self> {
        let path = workspace_activity::lock_path_for_workspace(workspace).ok()?;
        let pre_existing = path.exists();
        Some(Self { path, pre_existing })
    }
}

impl Drop for ActivityLockCleanup {
    fn drop(&mut self) {
        if !self.pre_existing {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn argument(arguments: &mut Vec<OsString>, value: impl Into<OsString>) {
    arguments.push(value.into());
}

/// Build the sandbox argv: read-only host root, a bounded tmpfs workspace, and
/// a read-write bind of the real user runtime directory for the activity lock.
fn sandbox_arguments(workspace: &Path, runtime_dir: &Path, test_binary: &Path) -> Vec<OsString> {
    let mut arguments = Vec::new();
    argument(&mut arguments, "--die-with-parent");
    argument(&mut arguments, "--new-session");
    argument(&mut arguments, "--ro-bind");
    argument(&mut arguments, "/");
    argument(&mut arguments, "/");
    argument(&mut arguments, "--size");
    argument(&mut arguments, WORKSPACE_TMPFS_BYTES.to_string());
    argument(&mut arguments, "--tmpfs");
    argument(&mut arguments, workspace.as_os_str());
    argument(&mut arguments, "--bind");
    argument(&mut arguments, runtime_dir.as_os_str());
    argument(&mut arguments, runtime_dir.as_os_str());
    argument(&mut arguments, "--proc");
    argument(&mut arguments, "/proc");
    argument(&mut arguments, "--dev");
    argument(&mut arguments, "/dev");
    argument(&mut arguments, "--unshare-net");
    argument(&mut arguments, "--unshare-pid");
    argument(&mut arguments, "--unshare-ipc");
    argument(&mut arguments, "--unshare-uts");
    argument(&mut arguments, "--clearenv");
    argument(&mut arguments, "--setenv");
    argument(&mut arguments, "PATH");
    argument(&mut arguments, "/usr/local/bin:/usr/bin:/bin");
    argument(&mut arguments, "--setenv");
    argument(&mut arguments, "LANG");
    argument(&mut arguments, "C.UTF-8");
    argument(&mut arguments, "--setenv");
    argument(&mut arguments, "HOME");
    argument(&mut arguments, workspace.as_os_str());
    argument(&mut arguments, "--setenv");
    argument(&mut arguments, CHILD_MARKER);
    argument(&mut arguments, "1");
    argument(&mut arguments, "--setenv");
    argument(&mut arguments, CHILD_WORKSPACE);
    argument(&mut arguments, workspace.as_os_str());
    argument(&mut arguments, "--setenv");
    argument(&mut arguments, CHILD_RUNTIME_DIR);
    argument(&mut arguments, runtime_dir.as_os_str());
    argument(&mut arguments, "--chdir");
    argument(&mut arguments, workspace.as_os_str());
    argument(&mut arguments, "--");
    argument(&mut arguments, test_binary.as_os_str());
    argument(&mut arguments, "--exact");
    argument(&mut arguments, CHILD_TEST);
    argument(&mut arguments, "--nocapture");
    arguments
}

fn unique_run_token() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}", std::process::id())
}

async fn run_sandboxed_child(
    command: &mut tokio::process::Command,
    timeout: Duration,
) -> Result<CommandOutput, String> {
    match tokio::time::timeout(timeout, run_supervised(command)).await {
        Ok(result) => result,
        Err(_) => Err(format!("the sandboxed child timed out after {timeout:?}")),
    }
}

/// Stable ENOSPC evidence in a real filesystem error string.
fn enospc_evidence(error: &str) -> Option<&'static str> {
    let lower = error.to_ascii_lowercase();
    [
        ("no space left on device", "No space left on device"),
        ("os error 28", "os error 28"),
        ("enospc", "ENOSPC"),
    ]
    .into_iter()
    .find_map(|(needle, label)| lower.contains(needle).then_some(label))
}

/// A disk-full assertion must reject a different failure (permissions,
/// read-only mounts, lease contention, or the 8 MiB hard limit).
fn assert_enospc(error: &str) {
    let lower = error.to_ascii_lowercase();
    for disallowed in [
        "no such file",
        "read-only file system",
        "os error 30",
        "permission denied",
        "os error 13",
        "activity lock",
        "activity runtime",
        "busy",
        "exceeds complete output file limit",
    ] {
        assert!(
            !lower.contains(disallowed),
            "the disk-full case failed with '{disallowed}' instead of ENOSPC: {error}"
        );
    }
    assert!(
        enospc_evidence(error).is_some(),
        "the failure is not a real ENOSPC (os error 28 / No space left on device): {error}"
    );
}

fn single_line(value: &str) -> String {
    value.replace(['\n', '\r'], " ")
}

fn output_dir_names(store: &OutputStore) -> Vec<String> {
    let directory = store.output_dir();
    let mut names: Vec<String> = std::fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("child cannot list {}: {error}", directory.display()))
        .map(|entry| {
            entry
                .unwrap_or_else(|error| panic!("child cannot read a directory entry: {error}"))
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

#[test]
fn required_bwrap_fails_instead_of_skipping() {
    let error = unavailable_outcome(true, "/usr/bin/bwrap is missing")
        .expect_err("MIO_REQUIRE_BWRAP must fail, not skip");
    assert!(error.contains("MIO_REQUIRE_BWRAP"), "{error}");
    assert!(error.contains("is missing"), "{error}");
    assert!(unavailable_outcome(false, "/usr/bin/bwrap is missing").is_ok());
}

#[tokio::test]
async fn externalized_output_reports_real_enospc_on_a_full_tmpfs() {
    let Some(backend) = bubblewrap_for_real_disk_full() else {
        return;
    };
    let program = backend.program().to_path_buf();
    let test_binary = std::env::current_exe()
        .unwrap_or_else(|error| panic!("cannot locate the current test binary: {error}"));

    let base = PathBuf::from(BASE_DIR);
    std::fs::create_dir_all(&base)
        .unwrap_or_else(|error| panic!("cannot create {BASE_DIR}: {error}"));
    let run_dir = base.join(unique_run_token());
    std::fs::create_dir(&run_dir)
        .unwrap_or_else(|error| panic!("cannot create {}: {error}", run_dir.display()));
    let _scratch = Scratch(run_dir.clone());
    let workspace = run_dir.join("workspace");
    std::fs::create_dir(&workspace)
        .unwrap_or_else(|error| panic!("cannot create {}: {error}", workspace.display()));
    let workspace = workspace
        .canonicalize()
        .unwrap_or_else(|error| panic!("cannot canonicalize the sandbox workspace: {error}"));

    let runtime_dir = workspace_activity::runtime_directory()
        .unwrap_or_else(|error| panic!("workspace activity runtime directory: {error}"))
        .canonicalize()
        .unwrap_or_else(|error| {
            panic!("cannot canonicalize the workspace activity runtime directory: {error}")
        });
    assert!(
        runtime_dir.is_dir(),
        "the workspace activity runtime directory must be a real directory: {}",
        runtime_dir.display()
    );
    let _lock_cleanup = ActivityLockCleanup::for_workspace(&workspace);

    let mut command = tokio::process::Command::new(program);
    command
        .args(sandbox_arguments(&workspace, &runtime_dir, &test_binary))
        .stdin(Stdio::null());

    let output = run_sandboxed_child(&mut command, CHILD_TIMEOUT)
        .await
        .unwrap_or_else(|error| panic!("the sandboxed child did not complete: {error}"));
    let combined = output.combined();

    assert!(
        output.status.success(),
        "the sandboxed child reported a failure: {combined}"
    );
    assert!(
        combined.contains("MIO_OUTPUT_DISK_FULL_CHILD=ok"),
        "the sandboxed child did not reach its final marker: {combined}"
    );
    assert!(
        combined.contains("MIO_OUTPUT_DISK_FULL_ENOSPC=1"),
        "the sandboxed child did not prove ENOSPC: {combined}"
    );
    let error = combined
        .lines()
        .find_map(|line| line.strip_prefix("MIO_OUTPUT_DISK_FULL_ERROR="))
        .unwrap_or_else(|| panic!("the sandboxed child printed no error line: {combined}"));
    assert_enospc(error);
}

/// Child branch of [`externalized_output_reports_real_enospc_on_a_full_tmpfs`].
///
/// Without the private marker (a normal `cargo test` run) this is a no-op.
#[tokio::test]
async fn enospc_child_probe() {
    if std::env::var_os(CHILD_MARKER).is_none() {
        return;
    }
    let workspace = PathBuf::from(
        std::env::var_os(CHILD_WORKSPACE)
            .unwrap_or_else(|| panic!("{CHILD_WORKSPACE} must be set by the parent sandbox run")),
    );
    child_probe(workspace).await;
}

async fn child_probe(workspace: PathBuf) {
    if let Some(expected) = std::env::var_os(CHILD_RUNTIME_DIR) {
        let expected = PathBuf::from(expected);
        let actual = workspace_activity::runtime_directory()
            .unwrap_or_else(|error| {
                panic!("child cannot resolve the workspace activity runtime directory: {error}")
            })
            .canonicalize()
            .unwrap_or_else(|error| {
                panic!(
                    "child cannot canonicalize the workspace activity runtime directory: {error}"
                )
            });
        assert_eq!(
            actual, expected,
            "the sandbox changed the workspace activity runtime directory"
        );
        let lock =
            workspace_activity::lock_path_for_workspace(&workspace).unwrap_or_else(|error| {
                panic!("child cannot resolve the workspace activity lock path: {error}")
            });
        assert!(
            lock.starts_with(&actual),
            "the workspace activity lock {} is outside {}",
            lock.display(),
            actual.display()
        );
    }

    let store = OutputStore::open(workspace.clone())
        .unwrap_or_else(|error| panic!("child cannot open the output store: {error}"));

    // Seed one already registered output. The failed capture must not disturb
    // its bytes, its index entry, or add another file to `.mio/outputs`.
    let keep_name = "keep.tool";
    let keep_path = store.output_dir().join(keep_name);
    std::fs::write(&keep_path, b"pre-existing reference bytes\n")
        .unwrap_or_else(|error| panic!("child cannot seed the existing output: {error}"));
    {
        let lease = store
            .begin_capture()
            .unwrap_or_else(|error| panic!("child cannot take the capture lease: {error}"));
        store
            .register(
                &lease,
                OutputEntry {
                    path: format!(".mio/outputs/{keep_name}"),
                    bytes: None,
                    hash: None,
                    stream: "tool".to_owned(),
                    created: 0,
                    call_id: None,
                },
            )
            .unwrap_or_else(|error| panic!("child cannot register the existing output: {error}"));
    }
    let index_before = store
        .load_index()
        .unwrap_or_else(|error| panic!("child cannot load the output index: {error}"));
    assert_eq!(
        index_before.entries.len(),
        1,
        "the seed index must hold exactly the pre-existing entry"
    );
    let keep_before = std::fs::read(&keep_path)
        .unwrap_or_else(|error| panic!("child cannot read the existing output: {error}"));

    let payload = "x".repeat(PAYLOAD_BYTES);
    let result = externalize_output(&payload, &workspace).await;
    let error = match result {
        Ok(published) => {
            panic!("a capture on a full tmpfs must fail, but it published: {published}")
        }
        Err(error) => error,
    };
    println!("MIO_OUTPUT_DISK_FULL_ERROR={}", single_line(&error));
    assert_enospc(&error);

    let index_after = store
        .load_index()
        .unwrap_or_else(|error| panic!("child cannot reload the output index: {error}"));
    assert_eq!(
        index_after, index_before,
        "a failed capture must not change the output index"
    );
    assert_eq!(
        std::fs::read(&keep_path)
            .unwrap_or_else(|error| panic!("child cannot reread the existing output: {error}")),
        keep_before,
        "a failed capture must not modify existing content"
    );
    assert_eq!(
        output_dir_names(&store),
        vec![keep_name.to_owned()],
        "a failed capture must not leave a partial or published file behind"
    );

    println!("MIO_OUTPUT_DISK_FULL_ENOSPC=1");
    println!("MIO_OUTPUT_DISK_FULL_PAYLOAD_BYTES={PAYLOAD_BYTES}");
    println!(
        "MIO_OUTPUT_DISK_FULL_INDEX_ENTRIES={}",
        index_after.entries.len()
    );
    println!("MIO_OUTPUT_DISK_FULL_CHILD=ok");
}
