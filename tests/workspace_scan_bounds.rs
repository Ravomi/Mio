//! 工作区 `grep` / `find` / `ls` 在低 `RLIMIT_NOFILE` 下的遍历边界回归测试。
//!
//! 评审发现的缺口是：目录遍历先把所有条目收集进 `Vec`（每条都持有固定父目录的 `Arc` fd）
//! 再截断，于是遍历很多目录时会在读取任何文件之前就保留大量目录 fd，可能在 `EMFILE` 处
//! 失败，而且单个大目录先无界收集再截断。本测试用**真实独立子进程**把 soft limit 降到
//! 128，再遍历 256+ 个各自带文件的目录，验证 `grep` / `find` / `ls` 仍然正确、可用，且
//! 操作前后打开 fd 数量不随目录数增长。
//!
//! 父进程与其它测试的 rlimit 一律不变；子进程用有界 `kill` + `wait` 回收，并清空环境，
//! 不带任何 provider 凭据。

#![cfg(unix)]

use mio_coding_agent::{Arguments, Host, HostFuture, Workspace};
use std::{path::Path, sync::Arc, time::Duration};

/// 子进程里运行的测试名；父进程用 `--exact` 只跑它。
const CHILD_TEST: &str = "scan_bounds_child_under_low_nofile";

/// 遍历的目录数量：远多于子进程 128 的 fd 上限。
const DIR_COUNT: usize = 300;

struct QuietHost;

impl Host for QuietHost {
    fn shell(&self, _cmd: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn on_event(&self, _event: mio_coding_agent::AgentEvent) {}
}

/// 在真实子进程里以 128 的 `RLIMIT_NOFILE` 运行遍历边界场景。
#[test]
fn workspace_scan_stays_bounded_under_low_nofile() {
    let exe = std::env::current_exe().expect("integration test binary path");
    let mut command = std::process::Command::new(exe);
    command
        .arg("--exact")
        .arg(CHILD_TEST)
        .arg("--ignored")
        .arg("--nocapture")
        // 清空环境：子进程不需要也不应携带任何 provider key 或用户凭据。
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("RUST_BACKTRACE", "0");
    let mut child = command.spawn().expect("spawn the low-nofile child test");
    let status = wait_bounded(&mut child, Duration::from_secs(180));
    assert!(
        status.success(),
        "child scan-bounds test failed with {status:?}"
    );
}

/// 有界等待子进程：超时先 `kill` 再 `wait`，绝不无限挂起。
fn wait_bounded(child: &mut std::process::Child, timeout: Duration) -> std::process::ExitStatus {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("child scan-bounds test exceeded {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("waiting for the child failed: {error}"),
        }
    }
}

/// 只由父进程通过子进程运行的场景，因此默认标记 `ignored`。
#[ignore = "spawned by workspace_scan_stays_bounded_under_low_nofile under a lowered RLIMIT_NOFILE"]
#[tokio::test]
async fn scan_bounds_child_under_low_nofile() {
    lower_nofile(128);

    let folder = tempfile::tempdir().expect("temp workspace");
    let host = Arc::new(QuietHost);
    let ws = Workspace::new(folder.path(), host).expect("workspace");

    // 300 个目录，每个目录里有一个只读即用的普通文件。
    for index in 0..DIR_COUNT {
        let dir = folder.path().join(format!("dir_{index:03}"));
        std::fs::create_dir(&dir).expect("create scan subdirectory");
        std::fs::write(dir.join(format!("file_{index:03}.txt")), "needle keyword\n")
            .expect("write scan file");
    }

    // 预热：让运行时/阻塞线程等一次性的 fd 先分配出来，再测量泄漏基线。
    let warm = ws
        .execute(Arguments::grep("needle", Some("dir_000".into())))
        .await
        .expect("warm-up grep");
    assert!(warm.contains("needle"));

    let before = open_fd_count();

    let grep = ws
        .execute(Arguments::grep("needle", None))
        .await
        .expect("grep over many directories must not exhaust fds");
    assert_eq!(
        grep.lines().count(),
        DIR_COUNT,
        "grep should read every file exactly once: {grep}"
    );

    let find = ws
        .execute(Arguments::find(".txt", None))
        .await
        .expect("find over many directories must not exhaust fds");
    assert_eq!(
        find.lines().count(),
        DIR_COUNT,
        "find should list every file: {find}"
    );

    let ls = ws
        .execute(Arguments::ls(None))
        .await
        .expect("ls over many directories must not exhaust fds");
    assert_eq!(
        ls.lines().count(),
        DIR_COUNT,
        "ls should list every directory: {ls}"
    );

    // 打开 fd 数量不得随被遍历的目录数量增长：流式遍历一次只保留有界的目录句柄。
    if let (Some(before), Some(after)) = (before, open_fd_count()) {
        assert!(
            after <= before + 16,
            "traversal leaked file descriptors: before={before} after={after}"
        );
    }
}

/// 把当前进程的 `RLIMIT_NOFILE` soft limit 降到 `limit`（不提高 hard limit）。
#[cfg(unix)]
fn lower_nofile(limit: u64) {
    let mut current = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `current` points at a valid `rlimit` and `RLIMIT_NOFILE` is a valid resource.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut current) },
        0
    );
    let target = limit.min(current.rlim_max);
    current.rlim_cur = target;
    // SAFETY: lowering the soft limit to `target` is always permitted; the hard limit is
    // left untouched so the child cannot exhaust host resources.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &current) }, 0);
}

/// 当前进程打开的 fd 数量（Linux 通过 `/proc/self/fd`；无该接口时返回 `None`）。
fn open_fd_count() -> Option<usize> {
    let entries = std::fs::read_dir("/proc/self/fd").ok()?;
    Some(entries.count())
}
