//! DES-17 外置输出显式离线清理的针对性测试。
//!
//! 该文件通过 `#[path]` 直接编译 `src/output_store.rs`，因此可以独立运行：
//! `cargo test --locked --test output_cleanup`。生产库现在通过 `src/shell.rs` 的
//! `#[path = "output_store.rs"] pub mod output_store;` 编译同一份源码，两处共享同一
//! 磁盘路径（`.mio/output-index/cleanup.lock`），因此本测试持有的租约与真实
//! `Session::save` / journal 写入看到的是同一把锁。
//!
//! 根部的 `use mio_coding_agent::{journal, session};` 不只是为了方便：`output_store`
//! 用 `crate::session::Session` / `crate::journal::JournalFile` 解析真实 schema，而在
//! 这个测试 crate 里 `crate::` 指向测试根，于是这两个 `use` 在根模块注册了同名模块项，
//! 让同一个 `crate::` 路径解析到被测试的库。
//!
//! 覆盖：真实 schema fixture（用 `Session::save` 与 `Journal` 写入器落盘）上的登记 /
//! 计划 / 确认删除、`history` 与显式 `pinned_context` 引用保护、journal
//! `goal`/`target`/`detail` 引用保护、7 天保留阈值、陈旧计划与跨工作区计划拒绝、损坏
//! 索引与未来 schema（版本不符、缺必需字段、未知枚举值）拒绝、活动捕获租约拒绝、
//! 丢弃或截断的引用证据拒绝、未决副作用拒绝、异常引用拒绝、符号链接与路径穿越拒绝、
//! 内容不匹配拒绝、未登记文件只列出不删除、清理后的可恢复性，以及非 Unix 平台不支持
//! 清理的边界。另外覆盖本轮接入的保护：`guard_existing` 在无 `.mio` 时不创建任何目录、
//! 真实 `Session::save` / journal 写入在 exclusive 清理租约下被拒绝且原文件不变、
//! run 级 shared activity 租约（`Workspace::new` 使用的同一把锁）阻塞清理、阻塞
//! exclusive 获取严格等 shared 释放，以及工作区内/外外部 session 来源的登记与拒绝；
//! 并用测试二进制 child 模式验证跨进程 shared/exclusive 互斥。

#[path = "../src/output_store.rs"]
mod output_store;
#[path = "../src/workspace_activity.rs"]
mod workspace_activity;

use mio_coding_agent::context_memory::{PinnedContextEntry, PinnedContextKind};
use mio_coding_agent::{Message, journal, session};

use output_store::{
    CLEANUP_SUPPORTED, CaptureTransaction, CleanupPlan, DEFAULT_RETENTION_SECS, INDEX_FILE_NAME,
    INDEX_VERSION, MAX_NAME_CHARS, OutputEntry, OutputStore, OutputStoreError,
};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

fn secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn entry(path: &str, stream: &str, created: u64) -> OutputEntry {
    OutputEntry {
        path: path.to_owned(),
        bytes: None,
        hash: None,
        stream: stream.to_owned(),
        created,
        call_id: None,
    }
}

fn write_output(root: &Path, name: &str, contents: &[u8]) {
    fs::write(root.join(".mio/outputs").join(name), contents).unwrap();
}

fn publish(store: &OutputStore, name: &str, contents: &[u8], created: u64) -> OutputEntry {
    write_output(store.root(), name, contents);
    let lease = store.begin_capture().unwrap();
    store
        .register(
            &lease,
            entry(&format!(".mio/outputs/{name}"), "stdout", created),
        )
        .unwrap()
}

/// 重试「释放后应当可取得」的 exclusive 清理租约，直到跳过 fork 窗口。
///
/// `flock(2)` 锁挂在 open file description 上，`fork(2)` 会把它复制给子进程直到
/// `execve` 关掉 `FD_CLOEXEC` 描述符。因此当同一测试进程的另一线程正在 fork 子进程
/// 时，一个刚释放 shared 租约的 exclusive 获取可能瞬时观察到 `Busy`。这个拒绝是保守
/// 且正确的答案（内核在子进程 exec 后立即丢弃继承的引用），所以「释放之后」的断言在
/// 唯一障碍是这段有界窗口时重试；断言「活动持有者必须被拒绝」的地方保持严格立即。
fn without_fork_window<T>(
    mut attempt: impl FnMut() -> Result<T, OutputStoreError>,
) -> Result<T, OutputStoreError> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match attempt() {
            Err(OutputStoreError::Busy(_)) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            result => return result,
        }
    }
}

/// 同 [`without_fork_window`]，但用于生产写入者（`Session::save` / journal）返回的
/// `Result<_, String>`：在 fork 窗口内共享活动租约的获取同样会瞬时失败，写入者会以
/// “refused” 报错。只在错误确实是该拒绝时重试；其它错误原样返回。
fn retry_writer<T>(mut attempt: impl FnMut() -> Result<T, String>) -> Result<T, String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match attempt() {
            Err(error) if error.contains("refused") && std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            result => return result,
        }
    }
}

/// 用真实的 `Session` 写入器落盘一个真实 schema 的 session 文件。
///
/// 刻意不手写镜像 JSON：字段、`pinned_context` 的线格式与消息的 tag 都由
/// `src/session.rs` 的真实序列化路径产生，因此 fixture 不会与生产 schema 漂移。
fn real_session(root: &Path, name: &str, configure: impl FnOnce(&mut session::Session)) -> PathBuf {
    let dir = root.join(".mio/sessions");
    fs::create_dir_all(&dir).unwrap();
    let mut value = session::Session::new(root).unwrap();
    configure(&mut value);
    let path = dir.join(name);
    value.save(&path).unwrap();
    path
}

fn session_with_text(root: &Path, name: &str, text: &str) {
    real_session(root, name, |value| {
        value.history.push(Message::user(text));
    });
}

/// 用真实的 `Journal` 写入器落盘一个真实 schema 的 journal 文件。
fn real_journal(root: &Path, session_id: &str, configure: impl FnOnce(&journal::Journal)) {
    let value = journal::Journal::open(root, session_id);
    configure(&value);
}

fn journal_file(root: &Path, session_id: &str) -> PathBuf {
    journal::journal_path(root, session_id)
}

/// 直接写一份仍然符合真实 schema 的 journal JSON：用于构造写入器不会留下的计数状态。
/// 拒绝原因断言成 `IncompleteEvidence`（而不是 `Corrupt`）本身就证明 fixture 能解析成
/// 真实的 `JournalFile`。
fn write_raw_journal(root: &Path, name: &str, value: serde_json::Value) -> PathBuf {
    let dir = root.join(".mio/journal");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    path
}

/// 用真实类型读回 journal，用于核对 fixture 与 `src/journal.rs` 的真实字段一致。
fn read_journal(path: &Path) -> journal::JournalFile {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn raw_journal(runs: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "version": journal::JOURNAL_VERSION,
        "session_id": "fixture-session",
        "updated": 2,
        "dropped_runs": 0,
        "truncated_operations": 0,
        "runs": runs,
    })
}

fn raw_run(state: &str, operations: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "run_id": "run-1",
        "turn": 1,
        "started": 1,
        "state": state,
        "ended": 2,
        "operations": operations,
        "goal_truncated": false,
    })
}

fn raw_operation(status: &str) -> serde_json::Value {
    serde_json::json!({
        "op_id": 1,
        "operation": "bash",
        "target": "ls",
        "status": status,
        "started": 1,
        "finished": 2,
        "detail": "ok",
    })
}

#[test]
fn aged_unreferenced_outputs_are_planned_and_removed() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    let now = SystemTime::now();
    let created = secs(now) - DEFAULT_RETENTION_SECS - 60;
    let stored = publish(&store, "one.stdout", b"hello world", created);
    assert_eq!(stored.bytes, Some(11));
    assert!(stored.hash.is_some());
    assert_eq!(
        store.index_path().file_name().unwrap(),
        std::ffi::OsStr::new(INDEX_FILE_NAME)
    );
    assert_eq!(store.root(), dir.path());
    assert_eq!(
        store.canonical_root().unwrap(),
        dir.path().canonicalize().unwrap()
    );

    let plan = store.plan_cleanup(now).unwrap();
    assert_eq!(plan.candidates.len(), 1);
    assert_eq!(plan.candidates[0].path, ".mio/outputs/one.stdout");
    assert_eq!(plan.digest.len(), 64);
    assert_eq!(
        plan.workspace,
        dir.path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    );

    let report = without_fork_window(|| store.confirm_cleanup(&plan, now)).unwrap();
    assert_eq!(report.removed.len(), 1);
    assert_eq!(report.freed_bytes, 11);
    assert!(!dir.path().join(".mio/outputs/one.stdout").exists());
    assert!(store.load_index().unwrap().entries.is_empty());
    assert!(store.output_dir().is_dir());
}

#[test]
fn referenced_outputs_are_retained() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    let now = SystemTime::now();
    let created = secs(now) - DEFAULT_RETENTION_SECS - 60;
    publish(&store, "kept.stdout", b"keep me", created);
    session_with_text(
        dir.path(),
        "s1.json",
        "complete output: .mio/outputs/kept.stdout; 7 bytes",
    );

    let plan = store.plan_cleanup(now).unwrap();
    assert!(plan.candidates.is_empty());
    assert_eq!(
        plan.retained_referenced,
        vec![".mio/outputs/kept.stdout".to_string()]
    );
    let report = without_fork_window(|| store.confirm_cleanup(&plan, now)).unwrap();
    assert!(report.removed.is_empty());
    assert!(dir.path().join(".mio/outputs/kept.stdout").exists());
}

#[test]
fn pinned_context_references_also_protect_outputs() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    let now = SystemTime::now();
    let created = secs(now) - DEFAULT_RETENTION_SECS - 60;
    publish(&store, "pinned.stdout", b"pinned", created);
    real_session(dir.path(), "pinned.json", |value| {
        value
            .pinned_context
            .register(
                PinnedContextEntry::new(
                    "des-17",
                    PinnedContextKind::Goal,
                    "keep .mio/outputs/pinned.stdout until DES-17 is reviewed",
                )
                .unwrap(),
            )
            .unwrap();
    });

    let plan = store.plan_cleanup(now).unwrap();
    assert!(plan.candidates.is_empty());
    assert_eq!(
        plan.retained_referenced,
        vec![".mio/outputs/pinned.stdout".to_string()]
    );
    assert!(dir.path().join(".mio/outputs/pinned.stdout").exists());
}

#[test]
fn compacted_history_cannot_prove_an_output_is_unreferenced() {
    for legacy in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let store = OutputStore::open(root.path()).unwrap();
        let now = SystemTime::now();
        publish(
            &store,
            "archived.stdout",
            b"still needed",
            secs(now) - DEFAULT_RETENTION_SECS - 60,
        );
        let path = real_session(root.path(), "compacted.json", |session| {
            session.history = vec![
                Message::user(format!(
                    "{} .mio/outputs/archived.stdout {}",
                    "before ".repeat(500),
                    "after ".repeat(500)
                )),
                Message::assistant("old finding"),
                Message::user("recent goal"),
                Message::assistant("recent finding"),
            ];
            session.compact(2, None);
            assert!(
                !serde_json::to_string(&session.history)
                    .unwrap()
                    .contains(".mio/outputs/archived.stdout")
            );
        });
        if legacy {
            let mut saved: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            saved
                .as_object_mut()
                .unwrap()
                .remove("last_compaction_loss");
            fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
        }
        assert!(matches!(
            store.plan_cleanup(now),
            Err(OutputStoreError::IncompleteEvidence(_))
        ));
        assert!(root.path().join(".mio/outputs/archived.stdout").exists());
    }
}

#[test]
fn unknown_extra_fields_cannot_hide_references() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    let now = SystemTime::now();
    let created = secs(now) - DEFAULT_RETENTION_SECS - 60;
    publish(&store, "hidden.stdout", b"hidden", created);

    // 真实 schema 加上一个未来版本才会出现的额外字段：引用扫描走原始已校验值，
    // 未知字段里的引用一样会被发现，而不是“静默证明无引用”。
    let value = serde_json::json!({
        "version": session::Session::new(dir.path()).unwrap().version,
        "id": "00000000-0000-0000-0000-000000000002",
        "parent_id": null,
        "cwd": dir.path(),
        "created": 1,
        "updated": 2,
        "history": [],
        "future_attachments": [{"note": "kept at .mio/outputs/hidden.stdout"}],
    });
    let sessions = dir.path().join(".mio/sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join("future.json"),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();

    let plan = store.plan_cleanup(now).unwrap();
    assert!(plan.candidates.is_empty());
    assert_eq!(
        plan.retained_referenced,
        vec![".mio/outputs/hidden.stdout".to_string()]
    );
}

#[test]
fn journal_references_also_protect_outputs() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    let now = SystemTime::now();
    let created = secs(now) - DEFAULT_RETENTION_SECS - 60;
    publish(&store, "journal-ref.stdout", b"journal", created);
    real_journal(dir.path(), "journal-ref", |value| {
        value.begin_run().unwrap();
        value
            .record_intent("bash", "cat .mio/outputs/journal-ref.stdout")
            .unwrap();
        value
            .record_result(
                "bash",
                "cat .mio/outputs/journal-ref.stdout",
                &Ok("previewed".to_string()),
            )
            .unwrap();
        value.finish_run(journal::RunState::Completed).unwrap();
    });

    let plan = store.plan_cleanup(now).unwrap();
    assert!(plan.candidates.is_empty());
    assert!(
        plan.retained_referenced
            .iter()
            .any(|path| path == ".mio/outputs/journal-ref.stdout")
    );
    assert!(dir.path().join(".mio/outputs/journal-ref.stdout").exists());
}

#[test]
fn journal_goal_target_and_detail_references_protect_outputs() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    let now = SystemTime::now();
    let created = secs(now) - DEFAULT_RETENTION_SECS - 60;
    publish(&store, "goal.stdout", b"goal", created);
    publish(&store, "detail.stdout", b"detail", created);
    real_journal(dir.path(), "refs", |value| {
        value
            .begin_run_with_goal(Some("finish .mio/outputs/goal.stdout before closing"))
            .unwrap();
        value.record_intent("bash", "ls").unwrap();
        value
            .record_result(
                "bash",
                "ls",
                &Ok("preview of .mio/outputs/detail.stdout".to_string()),
            )
            .unwrap();
        value.finish_run(journal::RunState::Completed).unwrap();
    });

    let plan = store.plan_cleanup(now).unwrap();
    assert!(plan.candidates.is_empty());
    for name in [".mio/outputs/goal.stdout", ".mio/outputs/detail.stdout"] {
        assert!(
            plan.retained_referenced.iter().any(|path| path == name),
            "{name} should be retained by the journal goal/detail"
        );
    }
    assert!(dir.path().join(".mio/outputs/goal.stdout").exists());
    assert!(dir.path().join(".mio/outputs/detail.stdout").exists());
}

#[test]
fn truncated_journal_identity_cannot_prove_an_output_is_unreferenced() {
    for field in ["scope", "tool_call_id", "call_id"] {
        let dir = tempfile::tempdir().unwrap();
        let store = OutputStore::open(dir.path()).unwrap();
        let now = SystemTime::now();
        publish(
            &store,
            "protected.stdout",
            b"keep",
            secs(now) - DEFAULT_RETENTION_SECS - 60,
        );
        let mut operation = raw_operation("executed");
        operation["correlation"] = serde_json::json!({});
        operation["correlation"][field] =
            serde_json::json!(format!("{}...", "x".repeat(journal::MAX_TARGET_CHARS)));
        write_raw_journal(
            dir.path(),
            "truncated-identity.json",
            raw_journal(serde_json::json!([raw_run(
                "completed",
                serde_json::json!([operation]),
            )])),
        );
        assert!(matches!(
            store.plan_cleanup(now),
            Err(OutputStoreError::IncompleteEvidence(_))
        ));
        assert!(store.output_dir().join("protected.stdout").exists());
    }
}

#[test]
fn retention_threshold_is_strict() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    let now = SystemTime::now();
    let now_secs = secs(now);
    // 恰好 7 天：保留。
    publish(
        &store,
        "edge.stdout",
        b"edge",
        now_secs - DEFAULT_RETENTION_SECS,
    );
    // 超过 1 秒：候选。
    publish(
        &store,
        "old.stdout",
        b"old",
        now_secs - DEFAULT_RETENTION_SECS - 1,
    );

    let plan = store.plan_cleanup(now).unwrap();
    let candidates: Vec<String> = plan
        .candidates
        .iter()
        .map(|entry| entry.path.clone())
        .collect();
    assert_eq!(candidates, vec![".mio/outputs/old.stdout".to_string()]);
    assert_eq!(
        plan.retained_recent,
        vec![".mio/outputs/edge.stdout".to_string()]
    );
}

#[test]
fn a_stale_plan_cannot_delete_new_outputs() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    let now = SystemTime::now();
    let created = secs(now) - DEFAULT_RETENTION_SECS - 60;
    publish(&store, "old.stdout", b"old", created);
    let plan = store.plan_cleanup(now).unwrap();
    assert_eq!(plan.candidates.len(), 1);

    // 计划生成后登记一个新文件：摘要不再匹配。
    publish(&store, "new.stdout", b"new", secs(now));
    let error = without_fork_window(|| store.confirm_cleanup(&plan, now)).unwrap_err();
    assert!(
        matches!(error, OutputStoreError::StalePlan(_)),
        "unexpected error: {error}"
    );
    assert!(dir.path().join(".mio/outputs/old.stdout").exists());
    assert!(dir.path().join(".mio/outputs/new.stdout").exists());
}

#[test]
fn plans_are_bound_to_their_workspace() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let store = OutputStore::open(first.path()).unwrap();
    let other = OutputStore::open(second.path()).unwrap();
    let now = SystemTime::now();
    let created = secs(now) - DEFAULT_RETENTION_SECS - 60;
    // 两个工作区的索引内容完全对称，只有工作区路径不同。
    publish(&store, "same.stdout", b"same", created);
    publish(&other, "same.stdout", b"same", created);

    let plan = store.plan_cleanup(now).unwrap();
    let canonical = first
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(plan.workspace, canonical);
    assert_eq!(plan.candidates.len(), 1);

    // 同一个计划不能拿到另一个工作区复用。
    assert!(matches!(
        without_fork_window(|| other.confirm_cleanup(&plan, now)),
        Err(OutputStoreError::StalePlan(_))
    ));
    assert!(second.path().join(".mio/outputs/same.stdout").exists());

    // 计划被篡改（摘要 / 保留期 / 工作区）也必须被拒绝。
    let mut tampered: CleanupPlan = plan.clone();
    tampered.digest = "0".repeat(64);
    assert!(matches!(
        without_fork_window(|| store.confirm_cleanup(&tampered, now)),
        Err(OutputStoreError::StalePlan(_))
    ));

    let mut tampered: CleanupPlan = plan.clone();
    tampered.retention_secs = 1;
    assert!(matches!(
        without_fork_window(|| store.confirm_cleanup(&tampered, now)),
        Err(OutputStoreError::StalePlan(_))
    ));

    let mut tampered: CleanupPlan = plan.clone();
    tampered.workspace = second.path().to_string_lossy().into_owned();
    assert!(matches!(
        without_fork_window(|| store.confirm_cleanup(&tampered, now)),
        Err(OutputStoreError::StalePlan(_))
    ));
    assert!(second.path().join(".mio/outputs/same.stdout").exists());

    // 原计划在本工作区依然有效。
    let report = without_fork_window(|| store.confirm_cleanup(&plan, now)).unwrap();
    assert_eq!(report.removed.len(), 1);
    assert!(second.path().join(".mio/outputs/same.stdout").exists());
}

#[test]
fn corrupt_or_unknown_indexes_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    fs::write(store.index_path(), b"{ not json").unwrap();
    assert!(matches!(
        store.plan_cleanup(SystemTime::now()),
        Err(OutputStoreError::Corrupt(_))
    ));
    assert!(matches!(
        store.load_index(),
        Err(OutputStoreError::Corrupt(_))
    ));

    let unknown = serde_json::json!({"version": INDEX_VERSION + 1, "entries": []});
    fs::write(store.index_path(), serde_json::to_vec(&unknown).unwrap()).unwrap();
    assert!(matches!(
        store.load_index(),
        Err(OutputStoreError::Corrupt(_))
    ));

    let traversal = serde_json::json!({
        "version": INDEX_VERSION,
        "entries": [{
            "path": ".mio/outputs/../escape",
            "bytes": 1,
            "hash": null,
            "stream": "stdout",
            "created": 1,
            "call_id": null,
        }],
    });
    fs::write(store.index_path(), serde_json::to_vec(&traversal).unwrap()).unwrap();
    assert!(matches!(
        store.plan_cleanup(SystemTime::now()),
        Err(OutputStoreError::Invalid(_))
    ));
}

#[test]
fn future_or_incomplete_session_and_journal_schemas_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    let sessions = dir.path().join(".mio/sessions");
    fs::create_dir_all(&sessions).unwrap();
    let version = session::Session::new(dir.path()).unwrap().version;
    let fixture = sessions.join("future.json");

    let future = serde_json::json!({
        "version": version + 1,
        "id": "00000000-0000-0000-0000-000000000001",
        "parent_id": null,
        "cwd": dir.path(),
        "created": 1,
        "updated": 2,
        "history": [],
    });
    fs::write(&fixture, serde_json::to_vec(&future).unwrap()).unwrap();
    assert!(matches!(
        store.plan_cleanup(SystemTime::now()),
        Err(OutputStoreError::Corrupt(_))
    ));

    // 缺少必需字段 `history`：旧镜像会把它读成空列表（等于“没有引用”），真实
    // `Session` schema 直接解析失败。
    let incomplete = serde_json::json!({
        "version": version,
        "id": "00000000-0000-0000-0000-000000000001",
        "parent_id": null,
        "cwd": dir.path(),
        "created": 1,
        "updated": 2,
    });
    fs::write(&fixture, serde_json::to_vec(&incomplete).unwrap()).unwrap();
    assert!(matches!(
        store.plan_cleanup(SystemTime::now()),
        Err(OutputStoreError::Corrupt(_))
    ));
    fs::remove_file(&fixture).unwrap();

    // journal 的未知状态不是“未知记录”，而是真实 `RunState` 枚举解析失败。
    write_raw_journal(
        dir.path(),
        "future.json",
        raw_journal(serde_json::json!([raw_run(
            "teleported",
            serde_json::json!([])
        )])),
    );
    assert!(matches!(
        store.plan_cleanup(SystemTime::now()),
        Err(OutputStoreError::Corrupt(_))
    ));

    // 缺少必需字段 `runs` 的 journal 同理。
    write_raw_journal(
        dir.path(),
        "future.json",
        serde_json::json!({
            "version": journal::JOURNAL_VERSION,
            "session_id": "fixture-session",
            "updated": 2,
        }),
    );
    assert!(matches!(
        store.plan_cleanup(SystemTime::now()),
        Err(OutputStoreError::Corrupt(_))
    ));
}

#[test]
fn an_active_capture_lease_blocks_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    let now = SystemTime::now();
    let created = secs(now) - DEFAULT_RETENTION_SECS - 60;
    publish(&store, "old.stdout", b"old", created);
    let plan = store.plan_cleanup(now).unwrap();

    let capture = store.begin_capture().unwrap();
    assert_eq!(capture.directory(), store.state_dir());
    assert!(matches!(
        store.begin_cleanup(),
        Err(OutputStoreError::Busy(_))
    ));
    assert!(matches!(
        store.confirm_cleanup(&plan, now),
        Err(OutputStoreError::Busy(_))
    ));
    assert!(dir.path().join(".mio/outputs/old.stdout").exists());

    drop(capture);
    assert!(without_fork_window(|| store.begin_cleanup()).is_ok());
    assert!(without_fork_window(|| store.confirm_cleanup(&plan, now)).is_ok());
    assert!(!dir.path().join(".mio/outputs/old.stdout").exists());
}

#[test]
fn content_mismatch_refuses_deletion() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    let now = SystemTime::now();
    let created = secs(now) - DEFAULT_RETENTION_SECS - 60;
    publish(&store, "changed.stdout", b"original", created);
    let plan = store.plan_cleanup(now).unwrap();
    assert_eq!(plan.candidates.len(), 1);

    fs::write(dir.path().join(".mio/outputs/changed.stdout"), b"tampered").unwrap();
    assert!(matches!(
        without_fork_window(|| store.confirm_cleanup(&plan, now)),
        Err(OutputStoreError::ContentMismatch(_))
    ));
    assert!(dir.path().join(".mio/outputs/changed.stdout").exists());
    assert_eq!(store.load_index().unwrap().entries.len(), 1);
}

#[test]
fn traversal_and_illegal_names_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    let lease = store.begin_capture().unwrap();
    for path in [
        ".mio/outputs/../escape",
        ".mio/outputs/sub/file",
        "/etc/passwd",
        "outputs/plain",
        ".mio/outputs/.",
    ] {
        assert!(
            matches!(
                store.register(&lease, entry(path, "stdout", 0)),
                Err(OutputStoreError::Invalid(_))
            ),
            "path {path} should be rejected"
        );
    }
}

#[test]
fn unregistered_files_are_listed_but_never_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    let now = SystemTime::now();
    let created = secs(now) - DEFAULT_RETENTION_SECS - 60;
    publish(&store, "tracked.stdout", b"tracked", created);
    fs::write(dir.path().join(".mio/outputs/manual.log"), b"manual").unwrap();

    let plan = store.plan_cleanup(now).unwrap();
    assert_eq!(
        plan.unregistered,
        vec![".mio/outputs/manual.log".to_string()]
    );
    // 索引与锁文件是保留名，不会伪装成未登记产物。
    assert!(
        !plan
            .unregistered
            .iter()
            .any(|name| name.ends_with(INDEX_FILE_NAME))
    );

    let report = without_fork_window(|| store.confirm_cleanup(&plan, now)).unwrap();
    assert_eq!(report.removed.len(), 1);
    assert!(dir.path().join(".mio/outputs/manual.log").exists());
}

#[test]
fn missing_registered_files_are_pruned_from_the_index() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    let now = SystemTime::now();
    let created = secs(now) - DEFAULT_RETENTION_SECS - 60;
    publish(&store, "gone.stdout", b"gone", created);
    fs::remove_file(dir.path().join(".mio/outputs/gone.stdout")).unwrap();

    let plan = store.plan_cleanup(now).unwrap();
    assert!(plan.candidates.is_empty());
    assert_eq!(
        plan.pruned_missing,
        vec![".mio/outputs/gone.stdout".to_string()]
    );
    let report = without_fork_window(|| store.confirm_cleanup(&plan, now)).unwrap();
    assert_eq!(report.pruned_missing.len(), 1);
    assert!(store.load_index().unwrap().entries.is_empty());
}

#[test]
fn cleanup_recovers_after_files_are_removed_out_of_band() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    let now = SystemTime::now();
    let created = secs(now) - DEFAULT_RETENTION_SECS - 60;
    publish(&store, "one.stdout", b"one", created);
    publish(&store, "two.stdout", b"two", created);
    let plan = store.plan_cleanup(now).unwrap();
    assert_eq!(plan.candidates.len(), 2);

    // 计划之后有人手工删了一个文件：旧计划被拒绝，不会用旧摘要删掉剩下的文件。
    fs::remove_file(dir.path().join(".mio/outputs/one.stdout")).unwrap();
    assert!(matches!(
        without_fork_window(|| store.confirm_cleanup(&plan, now)),
        Err(OutputStoreError::StalePlan(_))
    ));
    assert!(dir.path().join(".mio/outputs/two.stdout").exists());
    assert_eq!(store.load_index().unwrap().entries.len(), 2);

    // 重新计划把消失的文件作为 pruned_missing 回收；删除与索引更新后索引保持一致，
    // 因此“先删文件、后写索引”的中断状态总能被下一次计划回收。
    let replanned = store.plan_cleanup(now).unwrap();
    assert_eq!(
        replanned.pruned_missing,
        vec![".mio/outputs/one.stdout".to_string()]
    );
    assert_eq!(replanned.candidates.len(), 1);
    let report = without_fork_window(|| store.confirm_cleanup(&replanned, now)).unwrap();
    assert_eq!(report.removed.len(), 1);
    assert_eq!(
        report.pruned_missing,
        vec![".mio/outputs/one.stdout".to_string()]
    );
    assert!(report.failed.is_empty());
    assert!(store.load_index().unwrap().entries.is_empty());
    assert!(!dir.path().join(".mio/outputs/two.stdout").exists());
}

#[test]
fn capture_transactions_commit_or_roll_back() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();

    // 未提交：回滚文件与索引。
    {
        let mut transaction = CaptureTransaction::begin(&store).unwrap();
        write_output(dir.path(), "rollback.stdout", b"rollback");
        transaction
            .register(entry(".mio/outputs/rollback.stdout", "stdout", 0))
            .unwrap();
        assert!(dir.path().join(".mio/outputs/rollback.stdout").exists());
    }
    assert!(!dir.path().join(".mio/outputs/rollback.stdout").exists());
    assert!(store.load_index().unwrap().entries.is_empty());

    // 已提交：保留。
    write_output(dir.path(), "commit.stdout", b"commit");
    let mut transaction = CaptureTransaction::begin(&store).unwrap();
    transaction
        .register(entry(".mio/outputs/commit.stdout", "stdout", 0))
        .unwrap();
    transaction.commit();
    assert!(dir.path().join(".mio/outputs/commit.stdout").exists());
    assert_eq!(store.load_index().unwrap().entries.len(), 1);
}

#[test]
fn duplicate_registration_and_foreign_leases_are_rejected() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let store = OutputStore::open(first.path()).unwrap();
    let other = OutputStore::open(second.path()).unwrap();
    let now = SystemTime::now();
    let created = secs(now) - DEFAULT_RETENTION_SECS - 60;

    write_output(first.path(), "dup.stdout", b"dup");
    let lease = store.begin_capture().unwrap();
    store
        .register(&lease, entry(".mio/outputs/dup.stdout", "stdout", created))
        .unwrap();
    assert!(matches!(
        store.register(&lease, entry(".mio/outputs/dup.stdout", "stdout", created)),
        Err(OutputStoreError::Invalid(_))
    ));

    // 另一个工作区的捕获租约不能登记到本 store。
    write_output(second.path(), "foreign.stdout", b"foreign");
    assert!(matches!(
        other.register(
            &lease,
            entry(".mio/outputs/foreign.stdout", "stdout", created)
        ),
        Err(OutputStoreError::Invalid(_))
    ));
}

#[test]
fn active_or_unresolved_journal_runs_reject_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();

    write_raw_journal(
        dir.path(),
        "j.json",
        raw_journal(serde_json::json!([raw_run(
            "active",
            serde_json::json!([])
        )])),
    );
    assert!(matches!(
        store.plan_cleanup(SystemTime::now()),
        Err(OutputStoreError::ActiveRun(_))
    ));

    // 已解决（executed）的 completed / interrupted run 不会被拒绝。
    write_raw_journal(
        dir.path(),
        "j.json",
        raw_journal(serde_json::json!([raw_run(
            "completed",
            serde_json::json!([raw_operation("executed")])
        )])),
    );
    assert!(store.plan_cleanup(SystemTime::now()).is_ok());
    write_raw_journal(
        dir.path(),
        "j.json",
        raw_journal(serde_json::json!([raw_run(
            "interrupted",
            serde_json::json!([raw_operation("failed")])
        )])),
    );
    assert!(store.plan_cleanup(SystemTime::now()).is_ok());
}

#[test]
fn unresolved_side_effects_reject_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    let now = SystemTime::now();
    let created = secs(now) - DEFAULT_RETENTION_SECS - 60;
    publish(&store, "unresolved.stdout", b"unresolved", created);

    write_raw_journal(
        dir.path(),
        "unresolved.json",
        raw_journal(serde_json::json!([raw_run(
            "completed",
            serde_json::json!([raw_operation("unknown")])
        )])),
    );
    assert!(
        matches!(
            store.plan_cleanup(now),
            Err(OutputStoreError::IncompleteEvidence(_))
        ),
        "unknown side effects must not be treated as absence of references"
    );

    write_raw_journal(
        dir.path(),
        "unresolved.json",
        raw_journal(serde_json::json!([raw_run(
            "completed",
            serde_json::json!([raw_operation("pending")])
        )])),
    );
    assert!(matches!(
        store.plan_cleanup(now),
        Err(OutputStoreError::IncompleteEvidence(_))
    ));

    // 同一份 fixture 只把状态换成 executed 就能通过，证明拒绝来自未决副作用本身。
    write_raw_journal(
        dir.path(),
        "unresolved.json",
        raw_journal(serde_json::json!([raw_run(
            "completed",
            serde_json::json!([raw_operation("executed")])
        )])),
    );
    assert!(store.plan_cleanup(now).is_ok());
    assert!(dir.path().join(".mio/outputs/unresolved.stdout").exists());
}

#[test]
fn dropped_journal_records_reject_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    let now = SystemTime::now();
    let created = secs(now) - DEFAULT_RETENTION_SECS - 60;
    publish(&store, "dropped.stdout", b"dropped", created);

    let mut dropped = raw_journal(serde_json::json!([raw_run(
        "completed",
        serde_json::json!([raw_operation("executed")])
    )]));
    dropped["dropped_runs"] = serde_json::json!(1);
    let path = write_raw_journal(dir.path(), "dropped.json", dropped);
    // 拒绝原因是 IncompleteEvidence（而不是 Corrupt），说明这份 fixture 就是真实 schema。
    assert_eq!(read_journal(&path).dropped_runs, 1);
    assert!(matches!(
        store.plan_cleanup(now),
        Err(OutputStoreError::IncompleteEvidence(_))
    ));

    let mut truncated = raw_journal(serde_json::json!([raw_run(
        "completed",
        serde_json::json!([raw_operation("executed")])
    )]));
    truncated["truncated_operations"] = serde_json::json!(2);
    let path = write_raw_journal(dir.path(), "dropped.json", truncated);
    assert_eq!(read_journal(&path).truncated_operations, 2);
    assert!(matches!(
        store.plan_cleanup(now),
        Err(OutputStoreError::IncompleteEvidence(_))
    ));
    assert!(dir.path().join(".mio/outputs/dropped.stdout").exists());
}

#[test]
fn truncated_reference_evidence_rejects_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    let now = SystemTime::now();
    let created = secs(now) - DEFAULT_RETENTION_SECS - 60;
    publish(&store, "cut.stdout", b"cut", created);

    // target 被真实写入器截断：`.mio/outputs/` 只剩下一截前缀，512 字符前缀不是完整
    // 引用证据。
    let target = format!("{} .mio/outputs/cut.stdout", "x".repeat(495));
    real_journal(dir.path(), "truncated", |value| {
        value.begin_run().unwrap();
        value.record_intent("bash", &target).unwrap();
        value
            .record_result("bash", &target, &Ok("done".to_string()))
            .unwrap();
        value.finish_run(journal::RunState::Completed).unwrap();
    });
    let stored = read_journal(&journal_file(dir.path(), "truncated"));
    let stored_target = &stored.runs[0].operations[0].target;
    assert!(stored_target.contains(".mio/outputs/"));
    assert!(!stored_target.contains("cut.stdout"));
    assert!(stored_target.ends_with("..."));
    assert_eq!(
        stored_target.chars().count(),
        journal::MAX_TARGET_CHARS + "...".len()
    );
    assert!(matches!(
        store.plan_cleanup(now),
        Err(OutputStoreError::IncompleteEvidence(_))
    ));
    assert!(dir.path().join(".mio/outputs/cut.stdout").exists());

    // detail 同理：被截断的结果摘要可能整体吃掉了引用。
    let output = format!("{} .mio/outputs/cut.stdout", "y".repeat(600));
    real_journal(dir.path(), "truncated-detail", |value| {
        value.begin_run().unwrap();
        value.record_intent("bash", "ls").unwrap();
        value.record_result("bash", "ls", &Ok(output)).unwrap();
        value.finish_run(journal::RunState::Completed).unwrap();
    });
    let stored = read_journal(&journal_file(dir.path(), "truncated-detail"));
    let stored_detail = stored.runs[0].operations[0].detail.as_deref().unwrap();
    assert!(stored_detail.ends_with("..."));
    assert_eq!(
        stored_detail.chars().count(),
        journal::MAX_DETAIL_CHARS + "...".len()
    );
    assert!(matches!(
        store.plan_cleanup(now),
        Err(OutputStoreError::IncompleteEvidence(_))
    ));
    assert!(dir.path().join(".mio/outputs/cut.stdout").exists());
}

#[test]
fn abnormal_references_reject_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    let now = SystemTime::now();
    let created = secs(now) - DEFAULT_RETENTION_SECS - 60;
    publish(&store, "aged.stdout", b"aged", created);

    for text in [
        // 前缀后面没有完整文件名（被截断的引用）。
        "see .mio/outputs/ for the full log",
        // 路径形态，不是单组件产物名。
        "see .mio/outputs/sub/dir.stdout",
        // 产物名不会是隐藏文件。
        "see .mio/outputs/.hidden",
    ] {
        session_with_text(dir.path(), "s.json", text);
        assert!(
            matches!(store.plan_cleanup(now), Err(OutputStoreError::Invalid(_))),
            "text {text:?} must not be treated as unreferenced"
        );
    }

    let oversized = format!("see .mio/outputs/{}", "a".repeat(MAX_NAME_CHARS + 1));
    session_with_text(dir.path(), "s.json", &oversized);
    assert!(matches!(
        store.plan_cleanup(now),
        Err(OutputStoreError::Invalid(_))
    ));
    assert!(dir.path().join(".mio/outputs/aged.stdout").exists());

    // 对照组：一个合法的完整引用会被正常保留。
    session_with_text(
        dir.path(),
        "s.json",
        "complete output: .mio/outputs/aged.stdout; 4 bytes",
    );
    let plan = store.plan_cleanup(now).unwrap();
    assert_eq!(
        plan.retained_referenced,
        vec![".mio/outputs/aged.stdout".to_string()]
    );
}

#[test]
fn plans_and_reports_record_the_wired_workspace_activity_lease() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    let now = SystemTime::now();
    let created = secs(now) - DEFAULT_RETENTION_SECS - 60;
    publish(&store, "one.stdout", b"one", created);

    let plan = store.plan_cleanup(now).unwrap();
    assert!(
        plan.warnings
            .iter()
            .any(|warning| warning.contains("workspace activity lease")),
        "the plan must state the cooperative activity boundary: {:?}",
        plan.warnings
    );
    let report = without_fork_window(|| store.confirm_cleanup(&plan, now)).unwrap();
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("workspace activity lease"))
    );
    assert!(report.failed.is_empty());
    assert_eq!(report.removed.len(), 1);
}

/// 生产 run（`Workspace::new`）与 `Session::save` / journal 写入使用的是 `guard_existing`
/// 这把 shared activity 租约；它存在时清理必须整体 Busy，丢弃后才可能清理。
#[test]
fn a_shared_activity_lease_blocks_cleanup_until_it_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    let now = SystemTime::now();
    let created = secs(now) - DEFAULT_RETENTION_SECS - 60;
    publish(&store, "guarded.stdout", b"guarded", created);
    let plan = store.plan_cleanup(now).unwrap();

    let run_lease = OutputStore::guard_existing(store.root())
        .unwrap()
        .expect(".mio/outputs exists, so the run lease is present");
    assert!(matches!(
        store.begin_cleanup(),
        Err(OutputStoreError::Busy(_))
    ));
    assert!(matches!(
        store.confirm_cleanup(&plan, now),
        Err(OutputStoreError::Busy(_))
    ));
    assert!(dir.path().join(".mio/outputs/guarded.stdout").exists());

    drop(run_lease);
    let report = without_fork_window(|| store.confirm_cleanup(&plan, now)).unwrap();
    assert_eq!(report.removed.len(), 1);
    assert!(!dir.path().join(".mio/outputs/guarded.stdout").exists());
}

/// 阻塞 exclusive 获取（清理等价路径）必须严格等 shared 释放，而不是靠 sleep 观察。
#[cfg(unix)]
#[test]
fn blocking_exclusive_activity_lease_waits_for_the_shared_lease() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    let lock = store
        .state_dir()
        .join(workspace_activity::ACTIVITY_LOCK_FILE);
    let shared = workspace_activity::shared_blocking(&lock).unwrap();

    let (sender, receiver) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let exclusive = workspace_activity::exclusive_blocking(&lock).unwrap();
        sender.send(()).unwrap();
        drop(exclusive);
    });
    // While the shared lease lives, the exclusive acquisition cannot complete.
    assert!(
        receiver
            .recv_timeout(std::time::Duration::from_millis(200))
            .is_err(),
        "the exclusive lease was granted while a shared lease was held"
    );
    drop(shared);
    receiver
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the exclusive lease was never granted after the shared lease was dropped");
    waiter.join().unwrap();
}

/// 工作区内、`Session::save` 到标准扫描目录之外的 session 会被登记并真正枚举。
#[test]
fn in_workspace_external_sessions_are_registered_and_scanned() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let store = OutputStore::open(root.as_path()).unwrap();
    let now = SystemTime::now();
    let created = secs(now) - DEFAULT_RETENTION_SECS - 60;
    publish(&store, "custom.stdout", b"custom", created);

    let mut value = session::Session::new(&root).unwrap();
    value
        .history
        .push(Message::user("preview: .mio/outputs/custom.stdout"));
    value.save(&root.join("custom.json")).unwrap();
    assert!(
        store.source_index_path().exists(),
        "a session saved outside .mio/sessions must be registered"
    );

    let plan = store.plan_cleanup(now).unwrap();
    assert!(plan.candidates.is_empty());
    assert!(
        plan.retained_referenced
            .contains(&".mio/outputs/custom.stdout".to_string())
    );
    assert!(root.join(".mio/outputs/custom.stdout").exists());
}

/// 工作区外的 session 来源不会被读取（不扫描宿主私密文件），而是整体拒绝清理。
#[test]
fn out_of_workspace_external_sessions_refuse_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let store = OutputStore::open(root.as_path()).unwrap();
    let now = SystemTime::now();
    let created = secs(now) - DEFAULT_RETENTION_SECS - 60;
    publish(&store, "outside.stdout", b"outside", created);

    let outside = tempfile::tempdir().unwrap();
    let path = outside
        .path()
        .canonicalize()
        .unwrap()
        .join("elsewhere.json");
    let mut value = session::Session::new(&root).unwrap();
    value.history.push(Message::user("no reference here"));
    value.save(&path).unwrap();

    assert!(matches!(
        store.plan_cleanup(now),
        Err(OutputStoreError::IncompleteEvidence(_))
    ));
    assert!(root.join(".mio/outputs/outside.stdout").exists());
}

/// 损坏的外部来源登记表同样 fail-closed，而不是当成“没有来源”。
#[test]
fn a_corrupt_reference_source_index_rejects_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    fs::write(store.source_index_path(), b"{ not json").unwrap();
    assert!(matches!(
        store.plan_cleanup(SystemTime::now()),
        Err(OutputStoreError::Corrupt(_))
    ));
}

#[test]
fn a_corrupt_session_rejects_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    let sessions = dir.path().join(".mio/sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(sessions.join("broken.json"), b"{ not json").unwrap();
    assert!(matches!(
        store.plan_cleanup(SystemTime::now()),
        Err(OutputStoreError::Corrupt(_))
    ));
}

#[test]
fn guard_existing_creates_nothing_when_outputs_are_absent() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    assert!(
        OutputStore::guard_existing(root.as_path())
            .unwrap()
            .is_none(),
        "a workspace without .mio/outputs must not need a guard"
    );
    assert!(
        !root.join(".mio").exists(),
        "the guard must not create .mio"
    );

    // A real session save in the same workspace keeps `.mio` absent too: the
    // guard is a no-op until an output has actually been published.
    let mut value = session::Session::new(&root).unwrap();
    value.history.push(Message::user("no outputs yet"));
    let path = root.join("session.json");
    value.save(&path).unwrap();
    assert!(path.exists());
    assert!(
        !root.join(".mio").exists(),
        "a save without published outputs must not create .mio"
    );
}

#[test]
fn session_and_journal_writes_refuse_while_cleanup_holds_the_lease() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let store = OutputStore::open(root.as_path()).unwrap();

    // Real session and journal files, written by the production writers while
    // no cleanup lease is held.
    let session_path = real_session(&root, "guarded.json", |value| {
        value.history.push(Message::user("original"));
    });
    let session_before = fs::read(&session_path).unwrap();

    real_journal(&root, "guarded", |value| {
        value.begin_run_with_goal(Some("original")).unwrap();
        value.finish_run(journal::RunState::Completed).unwrap();
    });
    let journal_path = journal_file(&root, "guarded");
    let journal_before = fs::read(&journal_path).unwrap();

    let lease = without_fork_window(|| store.begin_cleanup()).unwrap();

    let mut blocked = session::Session::new(&root).unwrap();
    blocked.history.push(Message::user("must not land"));
    let error = blocked.save(&session_path).unwrap_err();
    assert!(error.contains("refused"), "unexpected error: {error}");
    assert_eq!(
        fs::read(&session_path).unwrap(),
        session_before,
        "a refused session save must leave the original file unchanged"
    );

    let reopened = journal::Journal::open(&root, "guarded");
    let error = reopened
        .begin_run_with_goal(Some("must not land"))
        .unwrap_err();
    assert!(error.contains("refused"), "unexpected error: {error}");
    assert_eq!(
        fs::read(&journal_path).unwrap(),
        journal_before,
        "a refused journal write must leave the original file unchanged"
    );

    drop(lease);

    // After the exclusive lease is released both writers persist again.
    let mut allowed = session::Session::new(&root).unwrap();
    allowed.history.push(Message::user("lands"));
    retry_writer(|| allowed.save(&session_path)).unwrap();
    assert_ne!(fs::read(&session_path).unwrap(), session_before);
    let reopened = journal::Journal::open(&root, "guarded");
    retry_writer(|| reopened.begin_run_with_goal(Some("lands"))).unwrap();
}

#[cfg(unix)]
const LOCK_PROBE_ROOT: &str = "MIO_DES17_LOCK_PROBE_ROOT";
#[cfg(unix)]
const LOCK_PROBE_MODE: &str = "MIO_DES17_LOCK_PROBE_MODE";

/// Child half of the cross-process lock test. It acts only when the parent
/// re-invokes this exact test in a fresh process with the probe environment.
#[cfg(unix)]
#[test]
fn des17_cross_process_lock_probe_child() {
    let Ok(root) = std::env::var(LOCK_PROBE_ROOT) else {
        return;
    };
    let root = PathBuf::from(root);
    let mode = std::env::var(LOCK_PROBE_MODE).unwrap_or_default();
    let code = match mode.as_str() {
        "cleanup" => match OutputStore::open(root.as_path())
            .and_then(|store| store.begin_cleanup().map(|_| ()))
        {
            Err(OutputStoreError::Busy(_)) => 0,
            Ok(()) => 11,
            Err(_) => 12,
        },
        "guard" => match OutputStore::guard_existing(root.as_path()) {
            Err(OutputStoreError::Busy(_)) => 0,
            Ok(_) => 13,
            Err(_) => 14,
        },
        _ => 15,
    };
    std::process::exit(code);
}

#[cfg(unix)]
#[test]
fn cross_process_shared_and_exclusive_leases_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let store = OutputStore::open(root.as_path()).unwrap();
    let exe = std::env::current_exe().unwrap();
    let probe = |mode: &str| -> i32 {
        std::process::Command::new(&exe)
            .args([
                "--exact",
                "des17_cross_process_lock_probe_child",
                "--nocapture",
            ])
            .env(LOCK_PROBE_ROOT, root.as_os_str())
            .env(LOCK_PROBE_MODE, mode)
            .status()
            .unwrap()
            .code()
            .unwrap_or(-1)
    };

    // A shared capture lease in this process blocks another process's
    // exclusive cleanup lease.
    let capture = store.begin_capture().unwrap();
    assert_eq!(
        probe("cleanup"),
        0,
        "a second process acquired the exclusive cleanup lease while a shared capture lease was held"
    );
    drop(capture);

    // The exclusive cleanup lease blocks another process's non-blocking shared
    // guard instead of letting it block a synchronous host callback.
    let cleanup = without_fork_window(|| store.begin_cleanup()).unwrap();
    assert_eq!(
        probe("guard"),
        0,
        "a second process took the shared guard while the exclusive cleanup lease was held"
    );
    drop(cleanup);
}

#[cfg(unix)]
#[test]
fn symlinks_are_rejected_everywhere() {
    let outside = tempfile::tempdir().unwrap();

    // 外置目录本身是符号链接：open 拒绝。
    let linked_root = tempfile::tempdir().unwrap();
    fs::create_dir(linked_root.path().join(".mio")).unwrap();
    std::os::unix::fs::symlink(outside.path(), linked_root.path().join(".mio/outputs")).unwrap();
    assert!(OutputStore::open(linked_root.path()).is_err());

    // 符号链接的输出文件：登记拒绝。
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    let target = outside.path().join("secret");
    fs::write(&target, b"secret").unwrap();
    std::os::unix::fs::symlink(&target, dir.path().join(".mio/outputs/link.stdout")).unwrap();
    let lease = store.begin_capture().unwrap();
    assert!(matches!(
        store.register(&lease, entry(".mio/outputs/link.stdout", "stdout", 0)),
        Err(OutputStoreError::Invalid(_))
    ));
    drop(lease);

    // 符号链接的 session：扫描拒绝。
    let sessions = dir.path().join(".mio/sessions");
    fs::create_dir_all(&sessions).unwrap();
    std::os::unix::fs::symlink(&target, sessions.join("s.json")).unwrap();
    assert!(matches!(
        store.plan_cleanup(SystemTime::now()),
        Err(OutputStoreError::Invalid(_))
    ));

    // 符号链接的 journal：扫描同样拒绝。
    fs::remove_file(sessions.join("s.json")).unwrap();
    let journal_dir = dir.path().join(".mio/journal");
    fs::create_dir_all(&journal_dir).unwrap();
    std::os::unix::fs::symlink(&target, journal_dir.join("j.json")).unwrap();
    assert!(matches!(
        store.plan_cleanup(SystemTime::now()),
        Err(OutputStoreError::Invalid(_))
    ));
}

#[test]
fn cleanup_support_matches_the_platform_lock_contract() {
    let dir = tempfile::tempdir().unwrap();
    let store = OutputStore::open(dir.path()).unwrap();
    #[cfg(unix)]
    {
        const { assert!(CLEANUP_SUPPORTED) };
        let lease = without_fork_window(|| store.begin_cleanup()).unwrap();
        assert_eq!(lease.directory(), store.state_dir());
    }
    #[cfg(not(unix))]
    {
        const { assert!(!CLEANUP_SUPPORTED) };
        assert!(matches!(
            store.begin_cleanup(),
            Err(OutputStoreError::Unsupported(_))
        ));
    }
}
