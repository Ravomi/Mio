//! DES-17 生产输出生命周期的端到端接入测试。
//!
//! 与 `tests/output_cleanup.rs`（store API 单元测试）和 `tests/output_capture.rs`
//! （捕获集成）不同，本文件只从**生产入口**出发，验证这一轮真正接上的链路：
//!
//! - `shell::execute` 的真实 spill → 版本化索引登记 → 引用 → `read` byte 分页完整重建；
//! - 真实子进程仍在捕获时，另一个进程的 exclusive 清理必须 `Busy`（用测试二进制的
//!   child 探针验证，而不是同进程互斥）；
//! - 生产 `Workspace`（run 生命周期）**无论 `.mio/outputs` 是否存在**都持有项目外的
//!   shared activity 租约（包括第一次 capture 发生在 run 构造之后的缺口），期间 dry-run
//!   可以执行、confirm 必须 `Busy`，丢弃后才真正清理；
//! - 小输出零磁盘写入、取消未提交的 spill 不留产物；
//! - 陈旧计划与 compaction 证据 fail-closed（外部来源与跨工作区拒绝由
//!   `tests/output_cleanup.rs` / `tests/output_cleanup_cli.rs` 覆盖）。
//!
//! 仅覆盖当前验证过的 Linux 实现；本文件不声称 Windows/macOS 已实测。

use mio_coding_agent::shell::output_store::{
    DEFAULT_RETENTION_SECS, OutputStore, OutputStoreError,
};
use mio_coding_agent::{AgentEvent, Arguments, Host, HostFuture, Workspace};
use serde_json::Value;
use std::{
    fs,
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct NoShell;
impl Host for NoShell {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { panic!("this test must not execute shell through the Host") })
    }
    fn on_event(&self, _: AgentEvent) {}
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn artifact_count(outputs: &Path) -> usize {
    fs::read_dir(outputs)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().is_file())
                .count()
        })
        .unwrap_or_default()
}

async fn wait_for(mut condition: impl FnMut() -> bool, what: &str) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if condition() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// 重试「释放后应当可取得」的 exclusive 清理租约，直到跳过 fork 窗口。
///
/// `flock(2)` 锁挂在 open file description 上，`fork(2)` 会把它复制给子进程直到
/// `execve` 关掉 `FD_CLOEXEC` 描述符。因此当同一测试进程的另一线程（模块的 probe 子进程
/// 或受监督 shell 子进程）正在 fork 时，一个刚释放 shared 租约的 exclusive 获取可能瞬时
/// 观察到 `Busy`。这个拒绝是保守且正确的答案，内核在子进程 exec 后立即丢弃继承的引用，
/// 所以「释放之后」的断言在唯一障碍是这段有界窗口时重试；断言「活动持有者必须被拒绝」
/// 的地方保持严格立即。
fn without_fork_window<T>(
    mut attempt: impl FnMut() -> Result<T, OutputStoreError>,
) -> Result<T, OutputStoreError> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match attempt() {
            Err(OutputStoreError::Busy(_)) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => return result,
        }
    }
}

/// Age every registered entry so it crosses the 7-day retention threshold. The
/// index is rewritten through its real JSON shape and revalidated by the store.
fn age_registered_outputs(store: &OutputStore, age_secs: u64) {
    let mut index = store.load_index().unwrap();
    let created = now_secs().saturating_sub(age_secs);
    for entry in &mut index.entries {
        entry.created = created;
    }
    fs::write(
        store.index_path(),
        serde_json::to_vec_pretty(&index).unwrap(),
    )
    .unwrap();
    assert_eq!(
        store.load_index().unwrap().entries.len(),
        index.entries.len()
    );
}

/// Real production spill: `shell::execute` crosses 32 KiB, registers the file in
/// the versioned index and emits a `.mio/outputs/<name>` reference.
#[tokio::test]
async fn production_shell_output_flows_through_index_and_byte_paging() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let size = 100_000usize;
    let output = mio_coding_agent::shell::execute(
        &format!("head -c {size} /dev/zero | tr '\\0' 'a'"),
        &root,
    )
    .await
    .unwrap();
    assert!(output.contains("[TRUNCATED: inline output exceeds 8 KiB"));

    let store = OutputStore::open(root.as_path()).unwrap();
    let index = store.load_index().unwrap();
    assert_eq!(index.entries.len(), 1, "one stream, one registered output");
    let entry = index.entries[0].clone();
    assert_eq!(entry.stream, "stdout");
    assert_eq!(entry.bytes, Some(size as u64));
    assert!(
        entry.hash.is_some(),
        "the index records a real content hash"
    );
    assert_eq!(entry.call_id, None, "an unknown call id must stay null");
    assert!(output.contains(entry.path.as_str()));

    // The published reference is byte-for-byte reconstructable through `read`
    // byte paging, with no shell involved.
    let workspace = Workspace::new_read_only(root.as_path(), Arc::new(NoShell)).unwrap();
    let mut offset = 0u64;
    let mut reconstructed = Vec::new();
    loop {
        let mut args = Arguments::read(entry.path.as_str());
        args.byte_offset = Some(offset);
        args.byte_limit = Some(8192);
        let page: Value = serde_json::from_str(&workspace.execute(args).await.unwrap()).unwrap();
        reconstructed.extend_from_slice(page["text"].as_str().unwrap().as_bytes());
        match page["next_byte_offset"].as_u64() {
            Some(next) => {
                assert!(next > offset, "paging must always advance");
                offset = next;
            }
            None => break,
        }
    }
    assert_eq!(reconstructed.len(), size);
    assert!(reconstructed.iter().all(|byte| *byte == b'a'));
}

/// A run (`Workspace`) holds the shared activity lease for its whole lifetime:
/// cleanup is `Busy` while it lives, and succeeds only after it is dropped.
#[tokio::test]
async fn a_production_run_blocks_cleanup_until_it_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    mio_coding_agent::shell::execute("head -c 40000 /dev/zero | tr '\\0' 'a'", &root)
        .await
        .unwrap();
    let store = OutputStore::open(root.as_path()).unwrap();
    age_registered_outputs(&store, DEFAULT_RETENTION_SECS + 60);

    let run = Workspace::new_read_only(root.as_path(), Arc::new(NoShell)).unwrap();
    assert!(
        run.output_activity_lease().is_some(),
        "a run in a workspace with managed outputs holds the shared activity lease"
    );
    assert!(matches!(
        store.begin_cleanup(),
        Err(OutputStoreError::Busy(_))
    ));
    // A dry-run only reads, so it is still allowed while the run is active.
    let plan = store.plan_cleanup(SystemTime::now()).unwrap();
    assert_eq!(plan.candidates.len(), 1);
    assert!(matches!(
        store.confirm_cleanup(&plan, SystemTime::now()),
        Err(OutputStoreError::Busy(_))
    ));
    let path = root.join(&plan.candidates[0].path);
    assert!(path.exists());

    drop(run);
    let report = without_fork_window(|| store.confirm_cleanup(&plan, SystemTime::now())).unwrap();
    assert_eq!(report.removed.len(), 1);
    assert!(!path.exists());
    assert!(store.load_index().unwrap().entries.is_empty());
}

/// A small output never creates `.mio`, and a production run holds its shared
/// activity lease from a project-external runtime lock even before any output
/// exists, without creating any project-local state.
#[tokio::test]
async fn small_outputs_and_empty_workspaces_stay_free_of_state() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    assert!(
        mio_coding_agent::shell::execute("printf small", &root)
            .await
            .unwrap()
            .contains("small")
    );
    assert!(!root.join(".mio").exists());

    let run = Workspace::new_read_only(root.as_path(), Arc::new(NoShell)).unwrap();
    assert!(
        run.output_activity_lease().is_some(),
        "every run holds the workspace activity lease, even before any output exists"
    );
    assert!(
        !root.join(".mio").exists(),
        "the run lease must live outside the project and create no lock files in it"
    );
    drop(run);
    assert!(
        !root.join(".mio").exists(),
        "a run without managed outputs must not create lock files"
    );
    assert!(
        OutputStore::guard_existing(root.as_path())
            .unwrap()
            .is_none()
    );
}

/// Cancelling a spilling production command rolls the uncommitted artifact and
/// its index entry back, leaving no orphan.
#[tokio::test]
async fn cancelling_a_spilling_production_command_leaves_no_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let task_root = root.clone();
    let task = tokio::spawn(async move {
        mio_coding_agent::shell::execute(
            "head -c 40000 /dev/zero | tr '\\0' 'a'; sleep 30",
            &task_root,
        )
        .await
    });
    let outputs = root.join(".mio/outputs");
    wait_for(|| artifact_count(&outputs) == 1, "the spill file").await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    wait_for(
        || artifact_count(&outputs) == 0,
        "the rolled-back spill file",
    )
    .await;
    let store = OutputStore::open(root.as_path()).unwrap();
    assert!(store.load_index().unwrap().entries.is_empty());
}

/// Both a stale plan and an extractively compacted session reject cleanup
/// instead of deleting an output whose references can no longer be enumerated.
#[tokio::test]
async fn stale_plans_and_compacted_history_fail_closed() {
    use mio_coding_agent::{Message, session};

    // Stale plan: a new output registered after the dry-run invalidates it.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    mio_coding_agent::shell::execute("head -c 40000 /dev/zero | tr '\\0' 'a'", &root)
        .await
        .unwrap();
    let store = OutputStore::open(root.as_path()).unwrap();
    age_registered_outputs(&store, DEFAULT_RETENTION_SECS + 60);
    let plan = store.plan_cleanup(SystemTime::now()).unwrap();
    mio_coding_agent::shell::execute("head -c 40000 /dev/zero | tr '\\0' 'b'", &root)
        .await
        .unwrap();
    assert!(matches!(
        without_fork_window(|| store.confirm_cleanup(&plan, SystemTime::now())),
        Err(OutputStoreError::StalePlan(_))
    ));

    // Extractive compaction: a saved history may have dropped a reference.
    let compacted = tempfile::tempdir().unwrap();
    let compacted_root = compacted.path().canonicalize().unwrap();
    mio_coding_agent::shell::execute("head -c 40000 /dev/zero | tr '\\0' 'c'", &compacted_root)
        .await
        .unwrap();
    let compacted_store = OutputStore::open(compacted_root.as_path()).unwrap();
    age_registered_outputs(&compacted_store, DEFAULT_RETENTION_SECS + 60);
    let sessions = compacted_root.join(".mio/sessions");
    fs::create_dir_all(&sessions).unwrap();
    let mut value = session::Session::new(&compacted_root).unwrap();
    value.history = vec![
        Message::user("before ".repeat(500)),
        Message::assistant("old finding"),
        Message::user("recent goal"),
        Message::assistant("recent answer"),
    ];
    value.compact(2, None);
    value.save(&sessions.join("compacted.json")).unwrap();
    assert!(matches!(
        compacted_store.plan_cleanup(SystemTime::now()),
        Err(OutputStoreError::IncompleteEvidence(_))
    ));
}

#[cfg(unix)]
const PROBE_ROOT: &str = "MIO_DES17_LIFECYCLE_PROBE_ROOT";

/// Child half of the cross-process capture test. It acts only when the parent
/// re-invokes this exact test in a fresh process with the probe environment.
#[cfg(unix)]
#[test]
fn des17_lifecycle_probe_child() {
    let Ok(root) = std::env::var(PROBE_ROOT) else {
        return;
    };
    let code = match OutputStore::open(Path::new(&root))
        .and_then(|store| store.begin_cleanup().map(|_| ()))
    {
        Err(OutputStoreError::Busy(_)) => 0,
        Ok(()) => 11,
        Err(_) => 12,
    };
    std::process::exit(code);
}

#[cfg(unix)]
fn probe_begin_cleanup(root: &Path) -> i32 {
    let exe = std::env::current_exe().unwrap();
    std::process::Command::new(exe)
        .args(["--exact", "des17_lifecycle_probe_child", "--nocapture"])
        .env(PROBE_ROOT, root.as_os_str())
        .status()
        .unwrap()
        .code()
        .unwrap_or(-1)
}

/// While a real child process still fills a spilling capture, an exclusive
/// cleanup in another process must be refused as `Busy`.
#[cfg(unix)]
#[tokio::test]
async fn another_process_cannot_cleanup_while_a_capture_is_live() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let task_root = root.clone();
    let task = tokio::spawn(async move {
        mio_coding_agent::shell::execute(
            "head -c 40000 /dev/zero | tr '\\0' 'a'; sleep 30",
            &task_root,
        )
        .await
    });
    let outputs = root.join(".mio/outputs");
    wait_for(|| artifact_count(&outputs) == 1, "the live spill file").await;

    assert_eq!(
        probe_begin_cleanup(&root),
        0,
        "a live capture must make another process's exclusive cleanup Busy"
    );

    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    wait_for(
        || artifact_count(&outputs) == 0,
        "the rolled-back spill file",
    )
    .await;
    // Once the capture is gone the same probe can take the exclusive lease.
    assert_eq!(probe_begin_cleanup(&root), 11);
}
