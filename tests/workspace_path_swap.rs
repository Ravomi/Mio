//! Deterministic adversarial regression for a real `Workspace` read when the
//! authorised parent directory is swapped for an out-of-workspace sentinel.
//!
//! ## What this proves
//!
//! The production read path resolves a path string (`checked_path`) and then
//! re-resolves it again at the final `open(2)`. A non-cooperating writer can
//! rename the authorised parent directory and replace the original name with a
//! symlink to an attacker-controlled directory in that window. The read must
//! still observe only the originally authorised inode.
//!
//! The harness does not rely on thread scheduling or probabilistic pressure.
//! An `LD_PRELOAD` shim (`tests/fixtures/workspace_path_swap.c`) parks the child
//! at the exact read-only `open`/`openat` of the target leaf, ACKs over a
//! private pipe, and waits for a release byte. The parent then performs the
//! rename + symlink swap and releases the child before the real syscall runs.
//!
//! ## Expected behaviour
//!
//! The child asserts one invariant that must not change:
//!
//! > a workspace read either returns the originally authorised inside content
//! > or rejects the operation explicitly; it never returns the outside
//! > sentinel.
//!
//! - Today's string-path implementation re-resolves `.../sub/leaf.txt` after
//!   the swap and follows the new `sub` symlink, so it reads the sentinel and
//!   the child fails. This is the raw failure evidence for wiring the pinned
//!   directory handles from `src/workspace_fs.rs` into `Workspace`.
//! - Once the read holds the parent directory handle and issues
//!   `openat(parent_fd, "leaf", ...)`, the same fault point and the same
//!   assertion resolve to the original inode and pass. The shim matches a bare
//!   relative leaf precisely so a secure implementation is not misdiagnosed as
//!   a hook timeout.
//!
//! ## Scope and safety
//!
//! - Linux GNU only; the shim is compiled at test time into the test's own
//!   `TempDir` with `cc -shared -fPIC ... -ldl`.
//! - The shim is injected only into the exact child test process via
//!   `Command::env("LD_PRELOAD", ...)`. The ambient test process and the rest of
//!   the environment are never touched; there is no `set_var`.
//! - Without its marker environment the child test body is a no-op and never
//!   spawns another process, so an ordinary `cargo test` run stays flat.
//! - Every pipe and child has a bounded poll/timeout, and a panic-safe guard
//!   kills and reaps the child and closes all descriptors.

#![cfg(all(target_os = "linux", target_env = "gnu"))]

use mio_coding_agent::{AgentEvent, Arguments, Host, HostFuture, Workspace};
use std::{
    fs,
    io::Read,
    os::{
        fd::RawFd,
        unix::{fs::symlink, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

/// Embedded so the test is independent of the source tree at run time; the
/// canonical copy lives in `tests/fixtures/workspace_path_swap.c`.
const SHIM_SOURCE: &str = include_str!("fixtures/workspace_path_swap.c");

const INSIDE_MARKER: &str = "MIO-INSIDE-AUTHORIZED-7f3a";
const OUTSIDE_MARKER: &str = "MIO-OUTSIDE-SENTINEL-7f3a";

const CHILD_LINE_TEST: &str =
    "child_workspace_read_line_after_parent_swap_only_returns_authorized_content";
const CHILD_BYTE_TEST: &str =
    "child_workspace_read_byte_after_parent_swap_only_returns_authorized_content";

/// Liveness bound for the whole parent/child conversation. It is not a mask for
/// a failing assertion: the child's own invariant decides pass or fail.
const CHILD_LIVENESS_TIMEOUT: Duration = Duration::from_secs(30);
const OUTPUT_CAP_BYTES: usize = 16 * 1024;

/// The read must never leak content from outside the workspace, so the only
/// acceptable outcomes are the authorised inside content or an explicit error.
fn verdict_text(result: Result<String, String>) -> (bool, String) {
    match result {
        Ok(text) if text.contains(OUTSIDE_MARKER) => (
            false,
            format!(
                "FAIL: the workspace read followed the swapped parent and returned the \
                 outside sentinel: {text:?}"
            ),
        ),
        Ok(text) if text.contains(INSIDE_MARKER) => (
            true,
            format!("OK: the workspace read returned only the authorised inside content: {text:?}"),
        ),
        Ok(text) => (
            false,
            format!(
                "FAIL: the workspace read returned neither the authorised inside content nor an \
                 explicit rejection: {text:?}"
            ),
        ),
        Err(error) => (
            true,
            format!("OK: the workspace read rejected the swapped parent explicitly: {error}"),
        ),
    }
}

struct ChildRequest {
    cwd: PathBuf,
    relative: String,
}

impl ChildRequest {
    /// `None` means this is an ordinary in-process run, so the child body is a
    /// no-op and never recurses into another spawn.
    fn from_env(expected_mode: &str) -> Option<Self> {
        if std::env::var("MIO_SWAP_MODE").ok().as_deref() != Some(expected_mode) {
            return None;
        }
        let cwd = PathBuf::from(std::env::var_os("MIO_SWAP_CWD")?);
        let relative = std::env::var("MIO_SWAP_REL").ok()?;
        Some(Self { cwd, relative })
    }
}

/// Minimal host: the read path under test never shells out or reports events.
struct NoopHost;

impl Host for NoopHost {
    fn shell(&self, _command: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Err("shell is not used by this test".to_string()) })
    }

    fn on_event(&self, _event: AgentEvent) {}
}

/// Child body: run the real `Workspace` read and record the raw verdict for the
/// parent before asserting the invariant in-process.
async fn child_check(request: ChildRequest, args: Arguments) {
    if let Some(marker) = std::env::var_os("MIO_SWAP_MARKER") {
        let _ = fs::write(marker, b"ran\n");
    }
    let result_file = std::env::var_os("MIO_SWAP_RESULT").map(PathBuf::from);

    let verdict = match Workspace::new(&request.cwd, Arc::new(NoopHost)) {
        Ok(workspace) => verdict_text(workspace.execute(args).await),
        Err(error) => (
            false,
            format!("FAIL: the child could not construct the Workspace: {error}"),
        ),
    };
    let (ok, detail) = verdict;
    if let Some(result_file) = &result_file {
        let _ = fs::write(result_file, format!("{detail}\n"));
    }
    assert!(ok, "{detail}");
}

#[tokio::test]
async fn child_workspace_read_line_after_parent_swap_only_returns_authorized_content() {
    let Some(request) = ChildRequest::from_env("line") else {
        return;
    };
    let args = Arguments::read(request.relative.clone());
    child_check(request, args).await;
}

#[tokio::test]
async fn child_workspace_read_byte_after_parent_swap_only_returns_authorized_content() {
    let Some(request) = ChildRequest::from_env("byte") else {
        return;
    };
    let args = Arguments {
        operation: "read".into(),
        path: Some(request.relative.clone()),
        byte_offset: Some(0),
        byte_limit: Some(4096),
        ..Arguments::default()
    };
    child_check(request, args).await;
}

#[tokio::test]
async fn workspace_line_read_resists_parent_directory_swap() {
    run_swap_case("line", CHILD_LINE_TEST);
}

#[tokio::test]
async fn workspace_byte_read_resists_parent_directory_swap() {
    run_swap_case("byte", CHILD_BYTE_TEST);
}

/// Build the layout, spawn the exact child test under the shim, swap the
/// authorised parent at the fault point, and judge by the child's verdict.
fn run_swap_case(mode: &'static str, child_test: &str) {
    let base = tempfile::tempdir().unwrap();
    let base_path = fs::canonicalize(base.path()).unwrap();
    let cwd = base_path.join("cwd");
    let external = base_path.join("external");
    let sub = cwd.join("sub");
    for dir in [&cwd, &external, &sub] {
        fs::create_dir(dir).unwrap();
    }

    let leaf = format!("mio-swap-{}-{mode}.txt", std::process::id());
    let relative = format!("sub/{leaf}");
    let absolute = sub.join(&leaf);
    fs::write(&absolute, INSIDE_MARKER).unwrap();
    fs::write(external.join(&leaf), OUTSIDE_MARKER).unwrap();

    let shim = compile_shim(&base_path);

    let (ack_read, ack_write) = pipe();
    let (release_read, release_write) = pipe();
    let mut harness = Harness {
        child: None,
        ack_read,
        ack_write,
        release_read,
        release_write,
    };

    let result_file = base_path.join("child-result.txt");
    let marker_file = base_path.join("child-ran.txt");

    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .arg("--exact")
        .arg(child_test)
        .arg("--test-threads=1")
        .arg("--nocapture")
        .env("LD_PRELOAD", &shim)
        .env("MIO_SWAP_MODE", mode)
        .env("MIO_SWAP_CWD", &cwd)
        .env("MIO_SWAP_REL", &relative)
        .env("MIO_SWAP_LEAF", &leaf)
        .env("MIO_SWAP_ABS", &absolute)
        .env("MIO_SWAP_ACK_FD", ack_write.to_string())
        .env("MIO_SWAP_RELEASE_FD", release_read.to_string())
        .env("MIO_SWAP_RESULT", &result_file)
        .env("MIO_SWAP_MARKER", &marker_file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: only async-signal-safe fcntl calls run between fork and exec.
    // Change flags in the child so concurrently spawned tests cannot inherit
    // this test's private protocol descriptors from the parent process.
    unsafe {
        command.pre_exec(move || {
            clear_cloexec(ack_write)?;
            clear_cloexec(release_read)
        });
    }
    harness.child = Some(command.spawn().unwrap());
    let child = harness.child.as_mut().unwrap();
    let stdout = drain_bounded(child.stdout.take().unwrap());
    let stderr = drain_bounded(child.stderr.take().unwrap());
    // The parent keeps only ack_read and release_write.
    harness.close_child_ends();

    let deadline = Instant::now() + CHILD_LIVENESS_TIMEOUT;
    let mut injected = false;
    let status = loop {
        if !injected {
            let mut waiter = libc::pollfd {
                fd: harness.ack_read,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: `waiter` is one valid pollfd for a live pipe read end.
            let ready = unsafe { libc::poll(&mut waiter, 1, 50) };
            if ready > 0 && (waiter.revents & libc::POLLIN) != 0 {
                let mut byte = [0u8; 1];
                // SAFETY: `ack_read` is the live read end of the private ACK pipe.
                let count = unsafe { libc::read(harness.ack_read, byte.as_mut_ptr().cast(), 1) };
                assert_eq!(count, 1, "the fault point must send a real ACK");
                // The child is parked inside the read-only open, before the real
                // syscall. Swap the authorised parent for the outside sentinel.
                swap_parent_for_sentinel(&sub, &external).unwrap_or_else(|error| {
                    panic!("failed to build the adversarial symlink: {error}")
                });
                let count = unsafe { libc::write(harness.release_write, b"A".as_ptr().cast(), 1) };
                assert_eq!(count, 1, "the parked child must be released");
                harness.close_release_write();
                injected = true;
            }
        }

        if let Some(child) = harness.child.as_mut()
            && let Some(status) = child.try_wait().unwrap()
        {
            break status;
        }
        if Instant::now() >= deadline {
            panic!(
                "child test `{child_test}` did not finish within {CHILD_LIVENESS_TIMEOUT:?} \
                 (fault point injected: {injected}). A secure implementation that reaches the \
                 leaf with a non-interposable raw syscall never triggers the shim, but it must \
                 still exit on its own, so this indicates a stuck child rather than a verdict."
            );
        }
        thread::sleep(Duration::from_millis(10));
    };

    // `try_wait` already reaped the child; do not let the guard kill it again.
    let _ = harness.child.take();
    let stdout = join_drain(stdout);
    let stderr = join_drain(stderr);
    let verdict = fs::read_to_string(&result_file).unwrap_or_default();
    let ran = marker_file.exists();

    let report = format!(
        "mode={mode} child=`{child_test}`\n\
         fault_point_injected={injected}\n\
         child_ran={ran}\n\
         exit_status={status:?}\n\
         verdict: {verdict}\n\
         --- child stdout ---\n{stdout}\n--- child stderr ---\n{stderr}"
    );
    assert!(
        injected,
        "no filesystem swap was injected; this run is not containment evidence\n{report}"
    );
    assert!(
        ran,
        "the child test `{child_test}` never executed its body; the `--exact` filter may be \
         stale, so the parent must not treat a zero-test run as success\n{report}"
    );
    assert!(
        status.success(),
        "the real Workspace read violated the containment invariant at the swapped parent \
         directory. The same fault point and assertion must pass once the read holds the pinned \
         parent directory handle and opens the leaf with `openat(parent_fd, leaf)`.\n{report}"
    );
    assert!(
        verdict.starts_with("OK"),
        "the child test reported a non-OK verdict despite a zero exit status\n{report}"
    );
}

/// Rename the authorised parent away and leave an outside sentinel symlink in
/// its place. This is the real filesystem attack the fault point enables.
fn swap_parent_for_sentinel(sub: &Path, external: &Path) -> std::io::Result<()> {
    let moved = sub.with_file_name("sub.saved");
    fs::rename(sub, &moved)?;
    symlink(external, sub)?;
    Ok(())
}

fn compile_shim(base: &Path) -> PathBuf {
    let source = base.join("workspace_path_swap.c");
    let library = base.join("libworkspace_path_swap.so");
    fs::write(&source, SHIM_SOURCE).unwrap();
    let output = Command::new("cc")
        .arg("-shared")
        .arg("-fPIC")
        .arg("-O2")
        .arg("-Wall")
        .arg("-o")
        .arg(&library)
        .arg(&source)
        .arg("-ldl")
        .output()
        .unwrap_or_else(|error| {
            panic!("`cc` is required to build the LD_PRELOAD shim for this test: {error}")
        });
    assert!(
        output.status.success(),
        "`cc` failed to build the LD_PRELOAD shim:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    library
}

/// Owns the child and every protocol descriptor. Drop closes all of them and,
/// unless the child was already reaped, kills and waits for it, so a panic can
/// never leak a parked process or a descriptor.
struct Harness {
    child: Option<Child>,
    ack_read: RawFd,
    ack_write: RawFd,
    release_read: RawFd,
    release_write: RawFd,
}

impl Harness {
    fn close(fd: &mut RawFd) {
        if *fd >= 0 {
            // SAFETY: each descriptor is owned by this guard and closed once.
            unsafe { libc::close(*fd) };
            *fd = -1;
        }
    }

    fn close_child_ends(&mut self) {
        Self::close(&mut self.ack_write);
        Self::close(&mut self.release_read);
    }

    fn close_release_write(&mut self) {
        Self::close(&mut self.release_write);
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        Self::close(&mut self.ack_read);
        Self::close(&mut self.ack_write);
        Self::close(&mut self.release_read);
        Self::close(&mut self.release_write);
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn pipe() -> (RawFd, RawFd) {
    let mut fds: [RawFd; 2] = [0; 2];
    // SAFETY: `fds` is a valid two-element array for `pipe2` to fill.
    let result = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    assert_eq!(
        result,
        0,
        "pipe2 failed: {}",
        std::io::Error::last_os_error()
    );
    (fds[0], fds[1])
}

fn clear_cloexec(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: `fd` is a live descriptor owned by this test.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` is live; F_SETFD only changes its descriptor flag.
    let result = unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Drain a child stream concurrently so the child can never block on a full
/// pipe, storing at most `OUTPUT_CAP_BYTES` while still consuming the rest.
fn drain_bounded<R: Read + Send + 'static>(mut reader: R) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut stored = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if stored.len() < OUTPUT_CAP_BYTES {
                        let remaining = OUTPUT_CAP_BYTES - stored.len();
                        stored.extend_from_slice(&chunk[..read.min(remaining)]);
                    }
                }
            }
        }
        stored
    })
}

fn join_drain(handle: thread::JoinHandle<Vec<u8>>) -> String {
    let bytes = handle.join().unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}
