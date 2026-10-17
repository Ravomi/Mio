//! 文件检查点的真实多进程与边界测试。
//!
//! 覆盖：并发进程各自创建快照不丢索引条目、同一检查点的并发 rewind 只有一个成功、
//! rewind 拒绝用户后续修改、索引损坏 fail-closed、锁文件权限、以及无关工作区互不影响。
//! 子进程复用当前测试二进制（`--exact checkpoint_child_entrypoint`），不使用真实模型。
//!
//! 这些用例验证的是 `CheckpointStore` 的跨进程索引事务（`flock` + 重新加载 + 原子
//! 落盘）。工作区级租约串行由 `src/workspace.rs` 的单元测试覆盖。

use mio_coding_agent::checkpoint::{
    CHECKPOINT_DIR, CHECKPOINT_LOCK_FILE, CheckpointState, CheckpointStore,
};
use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
};

const ENV_ACTION: &str = "MIO_CP_ACTION";
const ENV_ROOT: &str = "MIO_CP_ROOT";
const ENV_PREFIX: &str = "MIO_CP_PREFIX";
const ENV_COUNT: &str = "MIO_CP_COUNT";
const ENV_ID: &str = "MIO_CP_ID";
const ENV_RESULT: &str = "MIO_CP_RESULT";

// ---------------------------------------------------------------------------
// 子进程入口
// ---------------------------------------------------------------------------

/// 供父进程复用的子进程入口；普通测试运行缺少环境变量时直接返回。
#[test]
fn checkpoint_child_entrypoint() {
    let Ok(action) = std::env::var(ENV_ACTION) else {
        return;
    };
    let Some(root) = env_opt(ENV_ROOT) else {
        return;
    };
    if let Err(error) = run_child(&action, Path::new(&root)) {
        eprintln!("checkpoint child failed: {error}");
        std::process::exit(2);
    }
    std::process::exit(0);
}

fn run_child(action: &str, root: &Path) -> Result<(), String> {
    let store = CheckpointStore::open(root)?;
    match action {
        "record" => {
            let prefix = env_opt(ENV_PREFIX).ok_or("missing MIO_CP_PREFIX")?;
            let count: usize = env_opt(ENV_COUNT)
                .ok_or("missing MIO_CP_COUNT")?
                .parse()
                .map_err(|error| format!("invalid count: {error}"))?;
            for index in 0..count {
                let target = format!("{prefix}-{index}.txt");
                let content = format!("{prefix}-{index}\n");
                let id = store
                    .record_pre_edit("write", &target, None, content.as_bytes())?
                    .ok_or("checkpoint store is disabled")?;
                fs::write(root.join(&target), content.as_bytes()).map_err(|e| e.to_string())?;
                store.confirm(&id)?;
            }
        }
        "rewind" => {
            let selector = env_opt(ENV_ID).ok_or("missing MIO_CP_ID")?;
            let outcome = match store.rewind(&selector) {
                Ok(outcome) => format!("{:?}:{}", outcome.action, outcome.target),
                Err(error) => format!("ERR:{error}"),
            };
            if let Some(result) = env_opt(ENV_RESULT) {
                fs::write(&result, outcome).map_err(|e| e.to_string())?;
            }
        }
        other => return Err(format!("unknown action '{other}'")),
    }
    Ok(())
}

fn env_opt(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn child_command(root: &Path, action: &str) -> Command {
    let exe = std::env::current_exe().expect("current test executable");
    let mut command = Command::new(exe);
    command
        .arg("--exact")
        .arg("checkpoint_child_entrypoint")
        .arg("--test-threads=1")
        .env(ENV_ACTION, action)
        .env(ENV_ROOT, root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    command
}

fn wait_all(children: &mut [Child]) {
    for child in children.iter_mut() {
        let status = child.wait().expect("wait for checkpoint child");
        assert!(status.success(), "checkpoint child failed: {status}");
    }
}

// ---------------------------------------------------------------------------
// 跨进程索引事务
// ---------------------------------------------------------------------------

#[test]
fn concurrent_processes_record_distinct_snapshots_without_loss() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let workers = 4usize;
    let per_worker = 8usize;

    let mut children = Vec::new();
    for worker in 0..workers {
        let mut command = child_command(&root, "record");
        command
            .env(ENV_PREFIX, format!("p{worker}"))
            .env(ENV_COUNT, per_worker.to_string());
        children.push(command.spawn().expect("spawn record child"));
    }
    wait_all(&mut children);

    let store = CheckpointStore::open(&root).unwrap();
    let entries = store.list().unwrap();
    assert_eq!(
        entries.len(),
        workers * per_worker,
        "every child snapshot must survive the shared index"
    );
    for worker in 0..workers {
        for index in 0..per_worker {
            let target = format!("p{worker}-{index}.txt");
            assert_eq!(
                entries
                    .iter()
                    .filter(|entry| entry.target == target)
                    .count(),
                1,
                "target {target} must appear exactly once"
            );
            assert!(
                entries
                    .iter()
                    .all(|entry| entry.state == CheckpointState::Active),
                "all confirmed snapshots must be active"
            );
            assert!(root.join(&target).exists());
        }
    }
    assert!(
        root.join(CHECKPOINT_DIR)
            .join(CHECKPOINT_LOCK_FILE)
            .exists(),
        "the shared index lock must exist"
    );
}

#[test]
fn concurrent_rewinds_of_one_checkpoint_succeed_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fs::write(root.join("a.txt"), "before").unwrap();
    let store = CheckpointStore::open(&root).unwrap();
    let id = store
        .record_pre_edit("write", "a.txt", Some(b"before"), b"after")
        .unwrap()
        .unwrap();
    fs::write(root.join("a.txt"), "after").unwrap();
    store.confirm(&id).unwrap();

    let result_dir = root.join("results");
    fs::create_dir_all(&result_dir).unwrap();
    let workers = 4usize;
    let mut children = Vec::new();
    for worker in 0..workers {
        let mut command = child_command(&root, "rewind");
        command
            .env(ENV_ID, &id)
            .env(ENV_RESULT, result_dir.join(format!("r{worker}.txt")));
        children.push(command.spawn().expect("spawn rewind child"));
    }
    wait_all(&mut children);

    let results: Vec<String> = (0..workers)
        .map(|worker| {
            fs::read_to_string(result_dir.join(format!("r{worker}.txt")))
                .expect("rewind child must write a result")
        })
        .collect();
    let successes = results
        .iter()
        .filter(|result| result.starts_with("Restored:"))
        .count();
    assert_eq!(successes, 1, "results: {results:?}");
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "before");
    let entries = store.list().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].state, CheckpointState::Rewound);
}

#[test]
fn rewind_refuses_a_target_modified_after_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fs::write(root.join("a.txt"), "before").unwrap();
    let store = CheckpointStore::open(&root).unwrap();
    let id = store
        .record_pre_edit("write", "a.txt", Some(b"before"), b"after")
        .unwrap()
        .unwrap();
    fs::write(root.join("a.txt"), "after").unwrap();
    store.confirm(&id).unwrap();

    // 用户后续改动：跨进程 rewind 必须拒绝且不覆盖。
    fs::write(root.join("a.txt"), "user edit").unwrap();
    let result = root.join("result.txt");
    let mut child = {
        let mut command = child_command(&root, "rewind");
        command.env(ENV_ID, &id).env(ENV_RESULT, &result);
        command.spawn().expect("spawn rewind child")
    };
    assert!(child.wait().unwrap().success());
    let outcome = fs::read_to_string(&result).unwrap();
    assert!(outcome.starts_with("ERR:"), "{outcome}");
    assert!(outcome.contains("拒绝覆盖"), "{outcome}");
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "user edit");
    assert_eq!(store.list().unwrap()[0].state, CheckpointState::Active);
}

// ---------------------------------------------------------------------------
// 权限 / 损坏 / 默认关闭 / 无关路径
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn index_lock_and_snapshots_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fs::write(root.join("a.txt"), "before").unwrap();
    let store = CheckpointStore::open(&root).unwrap();
    let id = store
        .record_pre_edit("edit", "a.txt", Some(b"before"), b"after")
        .unwrap()
        .unwrap();
    fs::write(root.join("a.txt"), "after").unwrap();
    store.confirm(&id).unwrap();

    let checkpoint_dir = root.join(CHECKPOINT_DIR);
    assert_eq!(
        fs::metadata(&checkpoint_dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let private = [
        "index.json".to_owned(),
        CHECKPOINT_LOCK_FILE.to_owned(),
        format!("{id}.before"),
    ];
    for name in private {
        assert_eq!(
            fs::metadata(checkpoint_dir.join(&name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "{name} must be private"
        );
    }
}

#[test]
fn corrupt_index_fails_closed_and_is_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let checkpoint_dir = root.join(CHECKPOINT_DIR);
    fs::create_dir_all(&checkpoint_dir).unwrap();
    fs::write(checkpoint_dir.join("index.json"), "{ broken").unwrap();
    let store = CheckpointStore::open(&root).unwrap();

    assert!(store.record_pre_edit("write", "a.txt", None, b"x").is_err());
    assert!(store.list().is_err());
    assert!(store.rewind("deadbeef").is_err());
    assert_eq!(
        fs::read_to_string(checkpoint_dir.join("index.json")).unwrap(),
        "{ broken"
    );
    assert!(!root.join("a.txt").exists());
}

#[test]
fn default_store_is_disabled_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let store = CheckpointStore::disabled();
    assert!(!store.is_enabled());
    assert_eq!(
        store.record_pre_edit("write", "a.txt", None, b"x").unwrap(),
        None
    );
    assert!(store.list().unwrap().is_empty());
    assert!(store.rewind("abcd").is_err());
    assert!(!root.join(CHECKPOINT_DIR).exists());
}

#[test]
fn unrelated_workspaces_keep_independent_indexes() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let first_root = first.path().canonicalize().unwrap();
    let second_root = second.path().canonicalize().unwrap();
    let first_store = CheckpointStore::open(&first_root).unwrap();
    let second_store = CheckpointStore::open(&second_root).unwrap();

    first_store
        .record_pre_edit("write", "only-first.txt", None, b"a")
        .unwrap()
        .unwrap();
    second_store
        .record_pre_edit("write", "only-second.txt", None, b"b")
        .unwrap()
        .unwrap();

    let first_entries = first_store.list().unwrap();
    let second_entries = second_store.list().unwrap();
    assert_eq!(first_entries.len(), 1);
    assert_eq!(second_entries.len(), 1);
    assert_eq!(first_entries[0].target, "only-first.txt");
    assert_eq!(second_entries[0].target, "only-second.txt");
    assert!(
        first_root
            .join(CHECKPOINT_DIR)
            .join(CHECKPOINT_LOCK_FILE)
            .exists()
    );
    assert!(
        second_root
            .join(CHECKPOINT_DIR)
            .join(CHECKPOINT_LOCK_FILE)
            .exists()
    );
}
