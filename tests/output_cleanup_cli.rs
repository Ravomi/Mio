//! DES-17 显式离线输出清理 CLI 入口（`--output-cleanup-plan` /
//! `--output-cleanup-confirm`）的针对性测试。
//!
//! 该文件通过 `#[path]` 直接编译 `src/bin/mio_cli/args.rs`、`json_output.rs` 与
//! `output_cleanup.rs`，因此在 `src/bin/mio.rs` 接入这两个参数之前即可独立运行：
//! `cargo +1.95.0 test --locked --test output_cleanup_cli`。
//!
//! 覆盖：无 Provider 的离线计划、计划写入的 O_EXCL 拒覆盖与 0600 权限、父目录/叶子
//! 符号链接拒绝、不创建缺失父目录、引用保护、7 天保留阈值、真实 CLI confirm 只删除
//! 无引用且过期的文件、陈旧计划与跨工作区计划拒绝、坏 JSON / 符号链接 / 超限计划的
//! 有界读取拒绝、活动 run 与取消（未知副作用）证据拒绝、PartialCleanup 必须非零退出
//! 且保留报告，以及参数互斥与普通 prompt 用法不受影响。
//!
//! 前置条件（与 helper 相同）：`output_cleanup.rs` 通过
//! `mio_coding_agent::shell::output_store` 调用 `plan_cleanup` / `confirm_cleanup`；这两个
//! 方法在整 run 引用保护落地前保持 `pub(crate)`（见 `src/output_store.rs` 模块末尾的
//! “仍未接入”说明），因此在它们提升为公开 API 之前，本测试与 helper 都不会编译。

#![allow(dead_code)]

use mio_coding_agent::shell::output_store::{
    CleanupFailure, CleanupReport, DEFAULT_RETENTION_SECS, OutputEntry, OutputStore,
    OutputStoreError,
};
use mio_coding_agent::{Message, journal, session};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[path = "../src/bin/mio_cli/args.rs"]
mod args;
#[path = "../src/bin/mio_cli/json_output.rs"]
mod json_output;
#[path = "../src/bin/mio_cli/output_cleanup.rs"]
mod output_cleanup;

fn secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn old_enough() -> u64 {
    secs(SystemTime::now()) - DEFAULT_RETENTION_SECS - 60
}

fn entry(path: &str, created: u64) -> OutputEntry {
    OutputEntry {
        path: path.to_owned(),
        bytes: None,
        hash: None,
        stream: "stdout".to_owned(),
        created,
        call_id: None,
    }
}

/// 用真实 store API 发布一个已登记的外置文件。
fn publish(store: &OutputStore, name: &str, contents: &[u8], created: u64) {
    fs::write(store.root().join(".mio/outputs").join(name), contents).unwrap();
    let lease = store.begin_capture().unwrap();
    store
        .register(&lease, entry(&format!(".mio/outputs/{name}"), created))
        .unwrap();
}

fn paths_of(plan: &Value, field: &str) -> Vec<String> {
    plan[field]
        .as_array()
        .unwrap_or_else(|| panic!("{field} must be a JSON array"))
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| item["path"].as_str().unwrap().to_owned())
        })
        .collect()
}

fn plan_args(path: &Path) -> args::Args {
    args::Args {
        output_cleanup_plan: Some(path.to_path_buf()),
        ..Default::default()
    }
}

fn confirm_args(path: &Path) -> args::Args {
    args::Args {
        output_cleanup_confirm: Some(path.to_path_buf()),
        ..Default::default()
    }
}

/// 离线计划的 dry-run 行为：无需 Provider、不删文件、O_EXCL 拒覆盖、0600 权限。
#[test]
fn offline_plan_needs_no_provider_and_refuses_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    let store = OutputStore::open(cwd).unwrap();
    publish(&store, "old.stdout", b"old", old_enough());
    let output_file = cwd.join(".mio/outputs/old.stdout");

    let plan_path = cwd.join("plan.json");
    let parsed = plan_args(&plan_path);
    assert!(
        output_cleanup::selected(&parsed),
        "the plan flag selects the entry"
    );

    // run() 只接收 Args + cwd + output，不解析 Provider/模型/凭据。
    output_cleanup::run(&parsed, cwd, &None).unwrap();
    let bytes = fs::read(&plan_path).unwrap();
    let plan: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(plan["version"], 1);
    assert_eq!(
        paths_of(&plan, "candidates"),
        vec![".mio/outputs/old.stdout".to_string()]
    );
    assert!(
        output_file.exists(),
        "a dry-run plan must never delete an output"
    );

    // O_CREAT|O_EXCL：重复计划被拒绝，且既有计划保持原样。
    let error = output_cleanup::run(&parsed, cwd, &None).unwrap_err();
    assert!(
        error.contains("already exists"),
        "unexpected error: {error}"
    );
    assert_eq!(fs::read(&plan_path).unwrap(), bytes);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&plan_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the plan must be private");
    }
}

/// 父目录与叶子的符号链接都被拒绝；用户路径不会被越界重定向。
#[cfg(unix)]
#[test]
fn plan_write_rejects_symlinked_parent_and_leaf() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    OutputStore::open(cwd).unwrap();

    let target = cwd.join("target.json");
    fs::write(&target, b"do not touch").unwrap();
    let leaf = cwd.join("leaf.json");
    symlink(&target, &leaf).unwrap();
    let error = output_cleanup::run(&plan_args(&leaf), cwd, &None).unwrap_err();
    assert!(error.contains("symlink"), "unexpected error: {error}");
    assert_eq!(fs::read(&target).unwrap(), b"do not touch");

    let real_dir = cwd.join("real-dir");
    fs::create_dir(&real_dir).unwrap();
    let dir_link = cwd.join("dir-link");
    symlink(&real_dir, &dir_link).unwrap();
    let error =
        output_cleanup::run(&plan_args(&dir_link.join("plan.json")), cwd, &None).unwrap_err();
    assert!(error.contains("symlink"), "unexpected error: {error}");
    assert!(!real_dir.join("plan.json").exists());
}

/// 缺失父目录不会被创建（显式用户路径不新建任何目录）。
#[test]
fn plan_write_does_not_create_missing_parents() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    OutputStore::open(cwd).unwrap();
    let plan_path = cwd.join("nested/deeper/plan.json");
    let error = output_cleanup::run(&plan_args(&plan_path), cwd, &None).unwrap_err();
    assert!(!cwd.join("nested").exists(), "unexpected error: {error}");
}

/// 真实 CLI 流程：引用保护 + 7 天阈值，confirm 只删除无引用且过期的文件。
#[test]
fn confirm_deletes_only_unreferenced_expired_outputs() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    let store = OutputStore::open(cwd).unwrap();
    let old = old_enough();
    let recent = secs(SystemTime::now()) - 60;
    publish(&store, "expired.stdout", b"expired", old);
    publish(&store, "referenced.stdout", b"referenced", old);
    publish(&store, "recent.stdout", b"recent", recent);

    // 真实 session 引用其中一个输出。
    let sessions = cwd.join(".mio/sessions");
    fs::create_dir_all(&sessions).unwrap();
    let mut value = session::Session::new(cwd).unwrap();
    value
        .history
        .push(Message::user("preview: .mio/outputs/referenced.stdout"));
    value.save(&sessions.join("ref.json")).unwrap();

    let plan_path = cwd.join("plan.json");
    output_cleanup::run(&plan_args(&plan_path), cwd, &None).unwrap();
    let plan: Value = serde_json::from_slice(&fs::read(&plan_path).unwrap()).unwrap();
    assert_eq!(
        paths_of(&plan, "candidates"),
        vec![".mio/outputs/expired.stdout".to_string()]
    );
    assert!(
        paths_of(&plan, "retained_referenced")
            .contains(&".mio/outputs/referenced.stdout".to_string())
    );
    assert!(paths_of(&plan, "retained_recent").contains(&".mio/outputs/recent.stdout".to_string()));

    output_cleanup::run(&confirm_args(&plan_path), cwd, &None).unwrap();
    assert!(!cwd.join(".mio/outputs/expired.stdout").exists());
    assert!(cwd.join(".mio/outputs/referenced.stdout").exists());
    assert!(cwd.join(".mio/outputs/recent.stdout").exists());

    // 清理后的索引只剩被保留的条目。
    let index = OutputStore::open(cwd).unwrap().load_index().unwrap();
    let mut remaining: Vec<String> = index.entries.into_iter().map(|entry| entry.path).collect();
    remaining.sort();
    assert_eq!(
        remaining,
        vec![
            ".mio/outputs/recent.stdout".to_string(),
            ".mio/outputs/referenced.stdout".to_string()
        ]
    );
}

/// 计划生成后索引发生变化（陈旧计划）时必须拒绝，且不删除任何文件。
#[test]
fn stale_plan_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    let store = OutputStore::open(cwd).unwrap();
    let old = old_enough();
    publish(&store, "one.stdout", b"one", old);

    let plan_path = cwd.join("plan.json");
    output_cleanup::run(&plan_args(&plan_path), cwd, &None).unwrap();
    publish(&store, "two.stdout", b"two", old);

    let error = output_cleanup::run(&confirm_args(&plan_path), cwd, &None).unwrap_err();
    assert!(error.contains("plan"), "unexpected error: {error}");
    assert!(cwd.join(".mio/outputs/one.stdout").exists());
    assert!(cwd.join(".mio/outputs/two.stdout").exists());
}

/// 另一个工作区写出的计划不能被本工作区确认。
#[test]
fn cross_workspace_plan_is_rejected() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let store = OutputStore::open(first.path()).unwrap();
    publish(&store, "first.stdout", b"first", old_enough());

    let plan_path = first.path().join("plan.json");
    output_cleanup::run(&plan_args(&plan_path), first.path(), &None).unwrap();

    // 让第二个工作区也有同样的候选，否则摘要可能因候选为空而难以区分。
    let other = OutputStore::open(second.path()).unwrap();
    publish(&other, "other.stdout", b"other", old_enough());

    let error = output_cleanup::run(&confirm_args(&plan_path), second.path(), &None).unwrap_err();
    assert!(error.contains("workspace"), "unexpected error: {error}");
    assert!(second.path().join(".mio/outputs/other.stdout").exists());
}

/// confirm 读取计划时有界且拒绝符号链接 / 坏 JSON / 超限文件。
#[test]
fn confirm_rejects_bad_symlinked_and_oversized_plans() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    OutputStore::open(cwd).unwrap();

    let bad = cwd.join("bad.json");
    fs::write(&bad, b"{ not json").unwrap();
    let error = output_cleanup::run(&confirm_args(&bad), cwd, &None).unwrap_err();
    assert!(error.contains("JSON"), "unexpected error: {error}");

    let oversized = cwd.join("oversized.json");
    fs::write(
        &oversized,
        vec![b' '; output_cleanup::MAX_PLAN_BYTES as usize + 1],
    )
    .unwrap();
    let error = output_cleanup::run(&confirm_args(&oversized), cwd, &None).unwrap_err();
    assert!(error.contains("limit"), "unexpected error: {error}");

    #[cfg(unix)]
    {
        let real = cwd.join("real-plan.json");
        fs::write(&real, b"{}").unwrap();
        let link = cwd.join("link-plan.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let error = output_cleanup::run(&confirm_args(&link), cwd, &None).unwrap_err();
        assert!(error.contains("symlink"), "unexpected error: {error}");
    }
}

/// 取消的运行留下未知（Unknown）副作用证据时必须整体拒绝清理。
#[test]
fn cancelled_run_with_unknown_side_effect_blocks_plan() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    let store = OutputStore::open(cwd).unwrap();
    publish(&store, "guarded.stdout", b"guarded", old_enough());

    let record = journal::Journal::open(cwd, "cancelled-run");
    record.begin_run().unwrap();
    record
        .record_intent("bash", "cat .mio/outputs/guarded.stdout")
        .unwrap();
    // finish_run 把仍 Pending 的副作用标成 Unknown：证据不完整。
    record.finish_run(journal::RunState::Cancelled).unwrap();

    let plan_path = cwd.join("plan.json");
    let error = output_cleanup::run(&plan_args(&plan_path), cwd, &None).unwrap_err();
    assert!(
        error.contains("unresolved side effect") || error.contains("incomplete"),
        "unexpected error: {error}"
    );
    assert!(!plan_path.exists(), "a refused plan must not be written");
    assert!(cwd.join(".mio/outputs/guarded.stdout").exists());
}

/// 仍在进行（Active）的 run 同样阻止清理。
#[test]
fn active_run_blocks_plan() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    let store = OutputStore::open(cwd).unwrap();
    publish(&store, "active.stdout", b"active", old_enough());

    let record = journal::Journal::open(cwd, "active-run");
    record.begin_run().unwrap();
    record.record_intent("bash", "ls").unwrap();

    let plan_path = cwd.join("plan.json");
    let error = output_cleanup::run(&plan_args(&plan_path), cwd, &None).unwrap_err();
    assert!(error.contains("active"), "unexpected error: {error}");
    assert!(!plan_path.exists());
    assert!(cwd.join(".mio/outputs/active.stdout").exists());
}

/// 真实的 PartialCleanup 必须返回错误并保留完整报告。
///
/// 这里用合成的 `PartialCleanup` 直接验证 CLI 映射：`OutputStore::open` 每次都会把
/// `.mio`/`.mio/outputs`/`.mio/output-index` 重新设成 0700，因此进程内很难用权限制造
/// 一个真实的删除失败；CLI 分支本身才是这里要锁定的契约。
#[test]
fn partial_cleanup_keeps_the_report_and_returns_an_error() {
    let report = CleanupReport {
        removed: Vec::new(),
        freed_bytes: 0,
        retained_referenced: Vec::new(),
        retained_recent: Vec::new(),
        pruned_missing: Vec::new(),
        unregistered: Vec::new(),
        failed: vec![CleanupFailure {
            path: ".mio/outputs/locked.stdout".to_string(),
            message: "permission denied".to_string(),
        }],
        warnings: vec!["partial cleanup".to_string()],
    };
    let outcome = output_cleanup::classify_confirm(Err(OutputStoreError::PartialCleanup(
        Box::new(report),
        "1 registered output(s) could not be deleted".to_string(),
    )))
    .unwrap();
    assert!(
        outcome.error.is_some(),
        "a partial cleanup must make the CLI exit non-zero"
    );
    assert_eq!(outcome.report.failed.len(), 1);
    assert_eq!(outcome.report.failed[0].path, ".mio/outputs/locked.stdout");

    // 成功路径不产生错误，报告仍然带出。
    let ok = output_cleanup::classify_confirm(Ok(CleanupReport {
        removed: Vec::new(),
        freed_bytes: 0,
        retained_referenced: Vec::new(),
        retained_recent: Vec::new(),
        pruned_missing: Vec::new(),
        unregistered: Vec::new(),
        failed: Vec::new(),
        warnings: Vec::new(),
    }))
    .unwrap();
    assert!(ok.error.is_none());
}

/// 参数层：两个动作互斥、不与 prompt/RPC/其它独立动作共用，普通用法不变。
#[test]
fn cleanup_flags_are_standalone_and_mutually_exclusive() {
    let parse = |items: &[&str]| args::parse(items.iter().map(|item| item.to_string()));

    let plan = parse(&["--output-cleanup-plan", "plan.json"]).unwrap();
    assert_eq!(plan.output_cleanup_plan, Some(PathBuf::from("plan.json")));
    assert!(plan.output_cleanup_confirm.is_none());
    assert!(output_cleanup::selected(&plan));

    assert!(parse(&["--output-cleanup-confirm", "plan.json"]).is_ok());
    assert!(
        parse(&[
            "--output-cleanup-plan",
            "plan.json",
            "--output-cleanup-confirm",
            "plan.json"
        ])
        .is_err()
    );
    assert!(parse(&["--output-cleanup-plan", "plan.json", "-p", "hello"]).is_err());
    assert!(parse(&["--output-cleanup-plan", "plan.json", "--mode", "rpc"]).is_err());
    assert!(parse(&["--output-cleanup-plan", "plan.json", "--list-sessions"]).is_err());
    assert!(parse(&["--output-cleanup-confirm", "plan.json", "--sandbox-status"]).is_err());
    for action in ["--output-cleanup-plan", "--output-cleanup-confirm"] {
        assert!(parse(&[action, "plan.json", "--resume"]).is_err());
        assert!(parse(&[action, "plan.json", "--session", "selected.json"]).is_err());
        assert!(parse(&[action, "plan.json", "--resume", "--fork"]).is_err());
    }
    assert!(parse(&["--output-cleanup-plan"]).is_err());
    assert!(parse(&["--output-cleanup-confirm"]).is_err());

    // 普通 prompt / 终端用法完全不受影响。
    let normal = parse(&["-p", "hello", "--json"]).unwrap();
    assert_eq!(normal.prompt.as_deref(), Some("hello"));
    assert!(normal.json);
    assert!(!output_cleanup::selected(&normal));
}
