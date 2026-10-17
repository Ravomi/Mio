//! DES-17 项目外 workspace activity 运行锁的针对性测试。
//!
//! 本轮把 activity 锁从项目目录搬到项目之外的运行目录（canonical 工作区原生路径字节 +
//! euid 的 SHA-256），因此这里单独验证运行锁的核心契约：
//!
//! - 纯函数键：稳定、按工作区唯一、长度/字符集正确；
//! - 锁文件位于项目之外、0600 且 owner/非符号链接校验通过，项目里不产生任何 `.mio`；
//! - 同一工作区只有一把锁，不同工作区互不 `Busy`；
//! - shared / exclusive 在同一把工作区锁上互斥；
//! - 子进程在不同 `MIO_CONFIG_DIR` / `TMPDIR` 下解析出同一个锁路径；
//! - 符号链接锁文件被拒绝；
//! - 非 Unix 平台 exclusive 清理明确 `Unsupported`，shared 守卫不伪造互斥。
//!
//! 仅覆盖当前验证过的 Linux 实现；非 Unix 分支只做边界断言。

#![cfg_attr(not(unix), allow(unused_imports))]

use mio_coding_agent::shell::workspace_activity::{
    self, ActivityError, ActivityLease, exclusive_for_workspace, lock_path_for_workspace,
    shared_for_workspace, workspace_activity_key,
};
use std::path::Path;

/// 重试「释放后应当可取得」的 exclusive 租约，跳过 fork 窗口。
///
/// `flock(2)` 锁挂在 open file description 上，`fork(2)` 会把它复制给子进程直到
/// `execve` 关掉 `FD_CLOEXEC` 描述符。同一测试进程的另一线程（本模块的 probe 子进程）
/// 正在 fork 时，一个刚释放 shared 租约的 exclusive 获取可能瞬时观察到 `Busy`；内核在
/// 子进程 exec 后立即丢弃继承的引用，因此这里重试这个保守且正确的拒绝。
#[cfg(unix)]
fn exclusive_for_workspace_when_free(root: &Path) -> Option<ActivityLease> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(lease) = exclusive_for_workspace(root).unwrap() {
            return Some(lease);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// 同 [`exclusive_for_workspace_when_free`]，用于刚释放的一个 exclusive 租约：
/// fork 窗口期间继承的 exclusive 副本会让 shared 获取同样瞬时 `Busy`。
#[cfg(unix)]
fn shared_for_workspace_when_free(root: &Path) -> Option<ActivityLease> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(lease) = shared_for_workspace(root).unwrap() {
            return Some(lease);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// The key derives only from the canonical workspace path bytes plus euid, so it
/// is a pure, stable, per-workspace identifier.
#[test]
fn workspace_key_is_stable_workspace_specific_and_environment_free() {
    let a = Path::new("/tmp/des17-does-not-need-to-exist/workspace-a");
    let b = Path::new("/tmp/des17-does-not-need-to-exist/workspace-b");
    let key = workspace_activity_key(a);
    assert_eq!(key, workspace_activity_key(a), "the key must be stable");
    assert_ne!(key, workspace_activity_key(b), "distinct workspaces differ");
    assert_eq!(key.len(), 64, "SHA-256 is 32 bytes of lowercase hex");
    assert!(
        key.chars().all(|c| c.is_ascii_hexdigit()),
        "the key must be lowercase hex: {key}"
    );
}

/// Holds a shared lease before any output exists, yet creates no project-local
/// state; the lock is private and lives outside the workspace.
#[cfg(unix)]
#[test]
fn runtime_lock_lives_outside_the_project_with_private_permissions() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let lease = shared_for_workspace(&root)
        .unwrap()
        .expect("the first shared lease must succeed");
    let lock = lease.path().to_path_buf();
    assert!(
        lock.starts_with(workspace_activity::runtime_directory().unwrap()),
        "the lock must live in the runtime directory: {}",
        lock.display()
    );
    assert!(
        !lock.starts_with(&root),
        "the lock must not live in the project"
    );
    assert!(lock.exists());
    let metadata = std::fs::symlink_metadata(&lock).unwrap();
    assert!(metadata.is_file(), "the lock must be a regular file");
    assert!(!metadata.file_type().is_symlink());
    assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
    assert_eq!(
        metadata.permissions().mode() & 0o077,
        0,
        "the lock must be 0600"
    );
    assert!(
        !root.join(".mio").exists(),
        "holding the run lease must not create project-local state"
    );
    // The same workspace resolves to the same unique lock.
    assert_eq!(lock_path_for_workspace(&root).unwrap(), lock);
    drop(lease);
    assert!(
        shared_for_workspace(&root).unwrap().is_some(),
        "the lease is released on drop"
    );
}

/// Shared and exclusive leases on the same workspace key conflict; the lock is
/// released when the guard is dropped.
#[cfg(unix)]
#[test]
fn shared_and_exclusive_workspace_leases_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let shared = shared_for_workspace(&root).unwrap().expect("shared lease");
    assert!(
        exclusive_for_workspace(&root).unwrap().is_none(),
        "an exclusive lease must not be granted while a shared lease lives"
    );
    drop(shared);
    let exclusive = exclusive_for_workspace_when_free(&root)
        .expect("exclusive lease after the shared lease is dropped");
    assert!(
        shared_for_workspace(&root).unwrap().is_none(),
        "a shared lease must not be granted while an exclusive lease lives"
    );
    drop(exclusive);
    assert!(shared_for_workspace_when_free(&root).is_some());
}

/// Per-workspace locks do not serialize unrelated workspaces: a held exclusive
/// lease on one workspace never makes another `Busy`.
#[cfg(unix)]
#[test]
fn distinct_workspaces_get_distinct_locks() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let first = first.path().canonicalize().unwrap();
    let second = second.path().canonicalize().unwrap();
    assert_ne!(
        lock_path_for_workspace(&first).unwrap(),
        lock_path_for_workspace(&second).unwrap()
    );

    let shared_first = shared_for_workspace(&first).unwrap().unwrap();
    let exclusive_second = exclusive_for_workspace(&second)
        .unwrap()
        .expect("an unrelated workspace must not be Busy");
    // While `first` is shared-held and `second` is exclusive-held, neither
    // workspace affects the other.
    let shared_first_again = shared_for_workspace(&first).unwrap().unwrap();
    assert!(exclusive_for_workspace(&first).unwrap().is_none());
    assert!(shared_for_workspace(&second).unwrap().is_none());
    drop(shared_first_again);
    drop(shared_first);
    drop(exclusive_second);
}

#[cfg(unix)]
const RUNTIME_KEY_PROBE_ROOT: &str = "MIO_DES17_RUNTIME_KEY_PROBE_ROOT";

/// Child half of the environment-independence test. It only acts when the
/// parent re-invokes this exact test in a fresh process with the probe env.
#[cfg(unix)]
#[test]
fn des17_runtime_key_probe_child() {
    let Ok(root) = std::env::var(RUNTIME_KEY_PROBE_ROOT) else {
        return;
    };
    let path = lock_path_for_workspace(Path::new(&root)).unwrap();
    println!("MIO_DES17_RUNTIME_LOCK={}", path.display());
}

#[cfg(unix)]
fn probe_runtime_lock_path(root: &Path, config_dir: &str, temp_dir: &str) -> String {
    let exe = std::env::current_exe().unwrap();
    let output = std::process::Command::new(exe)
        .args(["--exact", "des17_runtime_key_probe_child", "--nocapture"])
        .env(RUNTIME_KEY_PROBE_ROOT, root.as_os_str())
        .env("MIO_CONFIG_DIR", config_dir)
        .env("TMPDIR", temp_dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "probe child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("MIO_DES17_RUNTIME_LOCK="))
        .expect("the probe child must print its lock path")
        .to_owned()
}

/// `MIO_CONFIG_DIR` and `TMPDIR` must not move the lock: the path is derived
/// from the canonical workspace path and the hard-coded runtime roots.
#[cfg(unix)]
#[test]
fn runtime_lock_path_is_independent_of_profile_and_temp_environment() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let baseline = lock_path_for_workspace(&root)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        probe_runtime_lock_path(&root, "/nonexistent/mio-config-a", "/var/tmp/mio-tmp-a"),
        baseline
    );
    assert_eq!(
        probe_runtime_lock_path(&root, "/nonexistent/mio-config-b", "/var/tmp/mio-tmp-b"),
        baseline
    );
}

/// A symlinked lock file is rejected by the explicit-path helpers.
#[cfg(unix)]
#[test]
fn symlinked_lock_files_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    std::fs::write(&target, b"x").unwrap();
    let link = dir.path().join("activity.lock");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(matches!(
        workspace_activity::try_shared(&link),
        Err(ActivityError::Invalid(_))
    ));
    assert!(matches!(
        workspace_activity::try_exclusive(&link),
        Err(ActivityError::Invalid(_))
    ));
}

/// Non-Unix: exclusive cleanup is explicitly unsupported, the shared helper
/// returns an unlocked guard instead of fabricating mutual exclusion, and the
/// platform flag agrees.
#[cfg(not(unix))]
#[test]
fn exclusive_workspace_cleanup_lease_is_unsupported_on_non_unix() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!mio_coding_agent::shell::output_store::CLEANUP_SUPPORTED);
    assert!(matches!(
        exclusive_for_workspace(dir.path()),
        Err(ActivityError::Unsupported(_))
    ));
    assert!(matches!(
        lock_path_for_workspace(dir.path()),
        Err(ActivityError::Unsupported(_))
    ));
    assert!(
        shared_for_workspace(dir.path()).unwrap().is_some(),
        "the shared guard is a no-op guard, not a fabricated lock"
    );
}
