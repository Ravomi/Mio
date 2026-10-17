//! DES-17 外置输出显式离线清理的 CLI 入口（`--output-cleanup-plan` /
//! `--output-cleanup-confirm`）。
//!
//! 两个动作都是独立、互斥、无需 Provider 的离线命令：
//!
//! - `--output-cleanup-plan PATH`：只做一次 dry-run，把全新的清理计划写到 `PATH`。
//!   `PATH` 必须是尚不存在的叶子，用 `O_CREAT|O_EXCL` 拒绝覆盖；父目录必须已存在且不是
//!   符号链接，叶子符号链接被显式拒绝；写入上限 [`MAX_PLAN_BYTES`]，创建权限 `0600`。
//!   显式用户路径可以位于工作区之外，本模块只沿用它，**不会创建父目录或任何全局目录**。
//! - `--output-cleanup-confirm PATH`：读取一份由上面动作写出的计划并显式确认清理。
//!   计划必须是普通文件、不是符号链接、且不超过 [`MAX_PLAN_BYTES`]（用
//!   `take(limit + 1)` 有界读取，绝不无界 `read`）。确认阶段由
//!   [`mio_coding_agent::shell::output_store::OutputStore::confirm_cleanup`] 重新绑定当前
//!   规范工作区、重算摘要并只删除无引用且超过 7 天的已完成输出。
//! - **默认不自动清理**：plan 不删除任何东西；只有显式 confirm 才会删除。
//! - **失败保留报告**：真正的部分清理（部分删除或索引未更新）会先发布
//!   [`CleanupReport`]，再返回错误使退出码非零；证据不完整、索引损坏、存在活动 run
//!   或引用关系不明时整体拒绝，而不是猜着删。
//!
//! 相对路径按 `--cwd` 解析（与 `--export` / `--shared-limits-dir` 的既有约定一致）。
//!
//! **接入状态**：本文件目前尚未出现在 `src/bin/mio.rs` 的 `mod` 列表中，因此暂不参与
//! 编译；计划/确认所需的公开 API 也仍待全 run 保护落地（见文件末尾）。

use super::args::Args;
use super::json_output::{JsonOutput, emit_or_print};
use mio_coding_agent::shell::output_store::{
    CleanupPlan, CleanupReport, OutputStore, OutputStoreError,
};
use serde_json::json;
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

/// 计划 JSON 的字节上限；plan 与 confirm 两侧都按它做有界读写。
pub(super) const MAX_PLAN_BYTES: u64 = 4 * 1024 * 1024;

/// 本次调用是否是离线输出清理动作。
pub(super) fn selected(args: &Args) -> bool {
    args.output_cleanup_plan.is_some() || args.output_cleanup_confirm.is_some()
}

/// 执行 `--output-cleanup-plan` 或 `--output-cleanup-confirm`。
///
/// `emit_or_print` 在 `--json` 下仍使用统一的 `schemaVersion` 封套；文本模式只打印必要
/// 计数与告警，不引入任何界面。
pub(super) fn run(args: &Args, cwd: &Path, output: &Option<JsonOutput>) -> Result<(), String> {
    match (&args.output_cleanup_plan, &args.output_cleanup_confirm) {
        (Some(path), None) => run_plan(&resolve_plan_path(cwd, path), cwd, args.json, output),
        (None, Some(path)) => run_confirm(&resolve_plan_path(cwd, path), cwd, args.json, output),
        (Some(_), Some(_)) => {
            Err("--output-cleanup-plan conflicts with --output-cleanup-confirm".into())
        }
        (None, None) => {
            Err("output cleanup requires --output-cleanup-plan or --output-cleanup-confirm".into())
        }
    }
}

/// 一次确认清理的 CLI 结论：`report` 必须在返回错误之前发布，因此单独带出。
pub(super) struct ConfirmOutcome {
    pub(super) report: CleanupReport,
    /// `Some` 表示这次清理存在真实的失败（[`OutputStoreError::PartialCleanup`]），
    /// 调用方必须发布 `report` 后让退出码非零。
    pub(super) error: Option<String>,
}

/// 把 `confirm_cleanup` 的结果映射成 CLI 结论。
///
/// 只有 [`OutputStoreError::PartialCleanup`] 成功离开删除阶段：它带着如实记录的部分
/// 报告，本模块必须先发布该报告再以非零退出。其余错误（陈旧计划、证据不完整、索引
/// 损坏、活动 run 等）没有删除任何东西，直接作为错误返回。
pub(super) fn classify_confirm(
    result: Result<CleanupReport, OutputStoreError>,
) -> Result<ConfirmOutcome, String> {
    match result {
        Ok(report) => Ok(ConfirmOutcome {
            report,
            error: None,
        }),
        Err(OutputStoreError::PartialCleanup(report, message)) => Ok(ConfirmOutcome {
            report: *report,
            error: Some(format!("output cleanup failed: {message}")),
        }),
        Err(error) => Err(format!("output cleanup failed: {error}")),
    }
}

fn run_plan(
    plan_path: &Path,
    cwd: &Path,
    json: bool,
    output: &Option<JsonOutput>,
) -> Result<(), String> {
    let store = OutputStore::open(cwd).map_err(|error| error.to_string())?;
    let plan = store
        .plan_cleanup(SystemTime::now())
        .map_err(|error| error.to_string())?;
    let payload = serde_json::to_vec_pretty(&plan)
        .map_err(|error| format!("cannot serialize the cleanup plan: {error}"))?;
    if payload.len() as u64 > MAX_PLAN_BYTES {
        return Err(format!(
            "cleanup plan would be {} bytes, above the {MAX_PLAN_BYTES} byte limit",
            payload.len()
        ));
    }
    write_new_plan(plan_path, &payload)?;

    if json {
        emit_or_print(
            output,
            json!({"type": "output_cleanup_plan", "path": plan_path, "data": plan}),
            false,
        )?;
    } else {
        println!("Output cleanup plan written to {}", plan_path.display());
        println!("{}", plan_counts(&plan));
        print_warnings(&plan.warnings);
    }
    Ok(())
}

fn run_confirm(
    plan_path: &Path,
    cwd: &Path,
    json: bool,
    output: &Option<JsonOutput>,
) -> Result<(), String> {
    let store = OutputStore::open(cwd).map_err(|error| error.to_string())?;
    let plan = read_plan(plan_path)?;
    let outcome = classify_confirm(store.confirm_cleanup(&plan, SystemTime::now()))?;
    emit_report(
        plan_path,
        &outcome.report,
        json,
        output,
        outcome.error.is_some(),
    )?;
    match outcome.error {
        Some(message) => Err(message),
        None => Ok(()),
    }
}

fn emit_report(
    plan_path: &Path,
    report: &CleanupReport,
    json: bool,
    output: &Option<JsonOutput>,
    critical: bool,
) -> Result<(), String> {
    if json {
        return emit_or_print(
            output,
            json!({"type": "output_cleanup", "path": plan_path, "data": report}),
            critical,
        );
    }
    println!("Output cleanup report for {}", plan_path.display());
    println!(
        "removed: {} file(s), freed: {} bytes",
        report.removed.len(),
        report.freed_bytes
    );
    println!(
        "referenced: {}, recent: {}, missing pruned: {}, unregistered: {}, failed: {}",
        report.retained_referenced.len(),
        report.retained_recent.len(),
        report.pruned_missing.len(),
        report.unregistered.len(),
        report.failed.len()
    );
    for failure in &report.failed {
        println!("failed: {} ({})", failure.path, failure.message);
    }
    print_warnings(&report.warnings);
    Ok(())
}

fn plan_counts(plan: &CleanupPlan) -> String {
    format!(
        "candidates: {}, referenced: {}, recent: {}, missing: {}, unregistered: {}",
        plan.candidates.len(),
        plan.retained_referenced.len(),
        plan.retained_recent.len(),
        plan.pruned_missing.len(),
        plan.unregistered.len()
    )
}

fn print_warnings(warnings: &[String]) {
    for warning in warnings {
        println!("warning: {warning}");
    }
}

/// 显式用户路径可在工作区之外；相对路径沿用 `--cwd` 的既有约定。
fn resolve_plan_path(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// 写出全新的计划：拒绝覆盖、拒绝符号链接父目录/叶子，权限 `0600`。
fn write_new_plan(path: &Path, payload: &[u8]) -> Result<(), String> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let metadata = fs::symlink_metadata(parent)
        .map_err(|error| format!("cannot access plan directory {}: {error}", parent.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "plan directory {} must be a real directory, not a symlink",
            parent.display()
        ));
    }
    match fs::symlink_metadata(path) {
        Ok(existing) if existing.file_type().is_symlink() => {
            return Err(format!(
                "plan path {} must not be a symlink",
                path.display()
            ));
        }
        Ok(_) => {
            return Err(format!(
                "plan path {} already exists; refusing to overwrite the existing plan",
                path.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "cannot inspect plan path {}: {error}",
                path.display()
            ));
        }
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("cannot create plan {}: {error}", path.display()))?;
    file.write_all(payload)
        .map_err(|error| format!("cannot write plan {}: {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| format!("cannot flush plan {}: {error}", path.display()))?;
    Ok(())
}

/// 有界读取并解析计划；叶子符号链接、非普通文件与超限都拒绝。
fn read_plan(path: &Path) -> Result<CleanupPlan, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot access plan {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "plan {} must be a regular file, not a symlink",
            path.display()
        ));
    }
    if metadata.len() > MAX_PLAN_BYTES {
        return Err(format!(
            "plan {} is {} bytes, above the {MAX_PLAN_BYTES} byte limit",
            path.display(),
            metadata.len()
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|error| format!("cannot open plan {}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    (&file)
        .take(MAX_PLAN_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read plan {}: {error}", path.display()))?;
    if bytes.len() as u64 > MAX_PLAN_BYTES {
        return Err(format!(
            "plan {} is larger than the {MAX_PLAN_BYTES} byte limit",
            path.display()
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("cleanup plan {} is not valid JSON: {error}", path.display()))
}

// ---------------------------------------------------------------------------
// main 接入点与所需公开 API（尚未执行，不要在这里改 `src/bin/mio.rs`）
// ---------------------------------------------------------------------------
//
// 1. `src/bin/mio.rs` 的模块列表（`#[path = "mio_cli/....rs"] mod ...;`）新增：
//
//        #[path = "mio_cli/output_cleanup.rs"]
//        mod output_cleanup;
//
// 2. 调用位置：`execute()` 里解析出 `cwd`（`let cwd = args.cwd...canonicalize()` 且
//    `cwd.is_dir()` 通过）之后、任何 Provider/`Config`/`Session` 初始化之前，加入：
//
//        if output_cleanup::selected(&args) {
//            return output_cleanup::run(&args, &cwd, &output);
//        }
//
//    这样两个动作与 `--sandbox-status` / checkpoint 维护等离线命令一样在
//    `resolve_provider_endpoint`、`Config`、`Session::new/load`、scheduler 之前返回，
//    不需要 `--model` / `MIO_API_KEY`，也不会触碰网络。
//
// 3. 所需公开 API：`src/output_store.rs` 的 `OutputStore::plan_cleanup` 与
//    `confirm_cleanup` 目前是 `pub(crate)`。接入本入口（以及整 run 引用保护）需要把
//    它们提升为 crate root 的公开方法（连同 `CleanupPlan` / `CleanupReport` /
//    `CleanupFailure` / `OutputStoreError` 已公开的类型），否则本文件与
//    `tests/output_cleanup_cli.rs` 都无法在 `mio_coding_agent` 之外编译。
//    这也正是本文件暂不加入 `mod` 列表的原因：在全 run 租约就绪前不对外暴露删除入口。
