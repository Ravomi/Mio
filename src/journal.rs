//! 持久化执行证据 journal（AUD-13）。
//!
//! 目标：在失败、取消或进程重启后，仍能区分「已执行」「执行失败」「结果未知」的
//! 副作用操作。设计约束：
//!
//! - **独立、版本化**：与 session 分离，使用独立的 `version` 字段与文件。
//! - **有大小限制**：限制运行数、每轮操作数与文件字节数，超出时丢弃最旧记录并计数。
//! - **Unix 0600**：journal 目录 0700、文件 0600，原子替换写入。
//! - **副作用之前持久化 intent**：`record_intent` 在操作真正执行前落盘；写入失败时
//!   返回 `Err`，由调用方阻止副作用。
//! - **不自动重放**：本模块只记录证据，绝不据此重新执行历史工具，也不声称回滚文件。
//! - **无会话模式零落盘**：`Journal::disabled()` 的所有写入都是空操作。
//!
//! 通过 `#[path]` 被 `bin/mio.rs` 引用，避免与并行修改 `lib.rs` 的变更冲突。

use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use tempfile::NamedTempFile;

/// 当前 journal 数据格式版本。读取到不同版本时视为不可信证据。
pub const JOURNAL_VERSION: u32 = 1;
/// 相对于工作区根目录的 journal 目录。
pub const JOURNAL_DIR: &str = ".mio/journal";
/// 单个 journal 文件的字节上限。
pub const MAX_JOURNAL_FILE_SIZE: usize = 512 * 1024;
/// 保留的最大运行记录数，超出丢弃最旧记录。
pub const MAX_JOURNAL_RUNS: usize = 64;
/// 单次运行保留的最大操作数，超出丢弃最旧操作。
pub const MAX_RUN_OPERATIONS: usize = 512;
/// 单个操作目标字段的最大字符数。
pub const MAX_TARGET_CHARS: usize = 512;
/// 单条结果摘要的最大字符数。
pub const MAX_DETAIL_CHARS: usize = 512;

/// 一次 Agent 运行（通常对应一个用户回合）的终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// 进程仍在运行或意外中断。
    Active,
    /// 回合正常完成。
    Completed,
    /// 回合以错误结束（例如模型请求失败）。
    Failed,
    /// 回合被用户取消。
    Cancelled,
    /// 进程在回合结束前退出，重启时标记。
    Interrupted,
}

/// 副作用操作的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpStatus {
    /// 已持久化 intent，但尚未记录结果。
    Pending,
    /// 已执行并记录结果。
    Executed,
    /// 已执行但返回错误。
    Failed,
    /// 无法确认结果（进程中断、取消或结果未落盘）。
    Unknown,
}

/// 单个副作用操作的证据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationRecord {
    pub op_id: u64,
    pub operation: String,
    pub target: String,
    pub status: OpStatus,
    pub started: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// 单次运行的证据记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    pub turn: u64,
    pub started: u64,
    pub state: RunState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<u64>,
    pub operations: Vec<OperationRecord>,
}

/// journal 文件的持久化结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalFile {
    pub version: u32,
    pub session_id: String,
    pub updated: u64,
    #[serde(default)]
    pub dropped_runs: u64,
    #[serde(default)]
    pub truncated_operations: u64,
    pub runs: Vec<RunRecord>,
}

/// 恢复报告中单条操作。
#[derive(Debug, Clone, Serialize)]
pub struct RecoveredOperation {
    pub run_id: String,
    pub turn: u64,
    pub operation: String,
    pub target: String,
    pub status: OpStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub recorded_at: u64,
}

/// 查询/启动时返回的恢复证据摘要。
#[derive(Debug, Clone, Default, Serialize)]
pub struct RecoveryReport {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal_error: Option<String>,
    pub interrupted_runs: usize,
    pub executed_operations: usize,
    pub executed: Vec<RecoveredOperation>,
    pub failed_operations: Vec<RecoveredOperation>,
    pub unknown_operations: Vec<RecoveredOperation>,
    pub dropped_runs: u64,
    pub truncated_operations: u64,
}

impl RecoveryReport {
    /// 是否存在需要用户注意的未完成运行或结果未知操作。
    pub fn has_unfinished(&self) -> bool {
        self.interrupted_runs > 0 || !self.unknown_operations.is_empty()
    }

    /// 单行人类可读摘要（明确声明不重放、不回滚）。
    pub fn summary_line(&self) -> String {
        format!(
            "{} interrupted run(s) and {} operation(s) with unknown results. \
             Mio will not replay these operations; file changes are not rolled back. \
             Inspect evidence with --recovery.",
            self.interrupted_runs,
            self.unknown_operations.len()
        )
    }
}

struct JournalData {
    path: PathBuf,
    file: JournalFile,
    active: Option<String>,
    load_error: Option<String>,
}

/// 执行证据 journal。`inner == None` 表示禁用（无会话模式），所有写入为空操作。
pub struct Journal {
    inner: Mutex<Option<JournalData>>,
}

impl Journal {
    /// 无会话模式：不创建、不写入任何文件。
    pub fn disabled() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    /// 打开指定会话的 journal。损坏或版本不符的证据保留原样，禁止后续覆盖。
    pub fn open(root: &Path, session_id: &str) -> Self {
        let path = journal_path(root, session_id);
        let (file, load_error) = load_file(&path, session_id);
        Self {
            inner: Mutex::new(Some(JournalData {
                path,
                file,
                active: None,
                load_error,
            })),
        }
    }

    /// 是否启用持久化。
    pub fn is_enabled(&self) -> bool {
        self.inner.lock().unwrap().is_some()
    }

    /// journal 文件路径（禁用时为 `None`）。
    pub fn path(&self) -> Option<PathBuf> {
        self.inner
            .lock()
            .unwrap()
            .as_ref()
            .map(|data| data.path.clone())
    }

    /// 切换到另一个会话的 journal，并返回其恢复报告。
    pub fn rebind(&self, root: &Path, session_id: &str) -> Option<RecoveryReport> {
        {
            let mut guard = self.inner.lock().unwrap();
            let data = guard.as_mut()?;
            let path = journal_path(root, session_id);
            let (file, load_error) = load_file(&path, session_id);
            data.path = path;
            data.file = file;
            data.active = None;
            data.load_error = load_error;
        }
        self.mark_interrupted()
    }

    /// 开始一次运行。写入失败时返回 `Err`，调用方应提示并阻止副作用操作。
    pub fn begin_run(&self) -> Result<(), String> {
        let mut guard = self.inner.lock().unwrap();
        let Some(data) = guard.as_mut() else {
            return Ok(());
        };
        if let Some(active) = data.active.clone() {
            let mut candidate = data.file.clone();
            if let Some(run) = candidate.runs.iter_mut().find(|r| r.run_id == active) {
                interrupt_run(run);
            }
            let _ = persist(data, &candidate);
        }
        let turn = data.file.runs.iter().map(|r| r.turn).max().unwrap_or(0) + 1;
        let run_id = uuid::Uuid::new_v4().to_string();
        let mut candidate = data.file.clone();
        candidate.runs.push(RunRecord {
            run_id: run_id.clone(),
            turn,
            started: now_secs(),
            state: RunState::Active,
            ended: None,
            operations: Vec::new(),
        });
        let enforced = persist(data, &candidate)?;
        data.file = enforced;
        data.active = Some(run_id);
        Ok(())
    }

    /// 在副作用发生之前持久化 intent。失败时返回 `Err`，调用方不得继续执行。
    pub fn record_intent(&self, operation: &str, target: &str) -> Result<(), String> {
        let mut guard = self.inner.lock().unwrap();
        let Some(data) = guard.as_mut() else {
            return Ok(());
        };
        let Some(run_id) = current_run(data) else {
            return Err(
                "Execution journal has no active run; refusing side-effecting operation".into(),
            );
        };
        let operation = truncate(operation, MAX_TARGET_CHARS);
        let target = truncate(target, MAX_TARGET_CHARS);
        let mut candidate = data.file.clone();
        let Some(run) = candidate.runs.iter_mut().find(|r| r.run_id == run_id) else {
            return Err("Execution journal lost the active run; refusing operation".into());
        };
        if run.operations.len() >= MAX_RUN_OPERATIONS {
            run.operations.remove(0);
            candidate.truncated_operations += 1;
        }
        let op_id = run.operations.last().map(|op| op.op_id).unwrap_or(0) + 1;
        run.operations.push(OperationRecord {
            op_id,
            operation,
            target,
            status: OpStatus::Pending,
            started: now_secs(),
            finished: None,
            detail: None,
        });
        let enforced = persist(data, &candidate)?;
        data.file = enforced;
        Ok(())
    }

    /// 记录副作用操作的结果。匹配不到 intent 时静默忽略（纯读取操作不产生证据）。
    pub fn record_result(
        &self,
        operation: &str,
        target: &str,
        result: &Result<String, String>,
    ) -> Result<(), String> {
        let mut guard = self.inner.lock().unwrap();
        let Some(data) = guard.as_mut() else {
            return Ok(());
        };
        let Some(run_id) = current_run(data) else {
            return Ok(());
        };
        let operation = truncate(operation, MAX_TARGET_CHARS);
        let target = truncate(target, MAX_TARGET_CHARS);
        let mut candidate = data.file.clone();
        let Some(run) = candidate.runs.iter_mut().find(|r| r.run_id == run_id) else {
            return Ok(());
        };
        let Some(index) = run.operations.iter().rposition(|op| {
            op.status == OpStatus::Pending && op.operation == operation && op.target == target
        }) else {
            return Ok(());
        };
        let (status, detail) = match result {
            Ok(output) => (OpStatus::Executed, truncate(output, MAX_DETAIL_CHARS)),
            Err(error) => (OpStatus::Failed, truncate(error, MAX_DETAIL_CHARS)),
        };
        let op = &mut run.operations[index];
        op.status = status;
        op.finished = Some(now_secs());
        op.detail = if detail.is_empty() {
            None
        } else {
            Some(detail)
        };
        let enforced = persist(data, &candidate)?;
        data.file = enforced;
        Ok(())
    }

    /// 结束当前运行。仍处于 `Pending` 的操作一律标记为 `Unknown`。
    pub fn finish_run(&self, state: RunState) -> Result<(), String> {
        let mut guard = self.inner.lock().unwrap();
        let Some(data) = guard.as_mut() else {
            return Ok(());
        };
        let Some(run_id) = current_run(data) else {
            return Ok(());
        };
        let mut candidate = data.file.clone();
        let Some(run) = candidate.runs.iter_mut().find(|r| r.run_id == run_id) else {
            return Ok(());
        };
        let now = now_secs();
        for op in run.operations.iter_mut() {
            if op.status == OpStatus::Pending {
                op.status = OpStatus::Unknown;
                op.finished = Some(now);
                op.detail = Some("result not recorded before the run ended".into());
            }
        }
        run.state = state;
        run.ended = Some(now);
        let enforced = persist(data, &candidate)?;
        data.file = enforced;
        data.active = None;
        Ok(())
    }

    /// 将上次进程遗留的 `Active` 运行标记为 `Interrupted`，并返回恢复报告。
    pub fn mark_interrupted(&self) -> Option<RecoveryReport> {
        let mut guard = self.inner.lock().unwrap();
        let data = guard.as_mut()?;
        let mut candidate = data.file.clone();
        let mut changed = false;
        for run in candidate.runs.iter_mut() {
            if run.state == RunState::Active {
                interrupt_run(run);
                changed = true;
            }
        }
        if changed && let Ok(enforced) = persist(data, &candidate) {
            data.file = enforced;
        }
        data.active = None;
        Some(build_report(&data.file, data.load_error.clone()))
    }

    /// 只读查询当前恢复证据，不修改文件。
    pub fn recovery_report(&self) -> Option<RecoveryReport> {
        let guard = self.inner.lock().unwrap();
        let data = guard.as_ref()?;
        Some(build_report(&data.file, data.load_error.clone()))
    }
}

/// 依据根目录与会话 ID 计算 journal 路径。
pub fn journal_path(root: &Path, session_id: &str) -> PathBuf {
    root.join(JOURNAL_DIR)
        .join(format!("{}.json", sanitize(session_id)))
}

fn current_run(data: &JournalData) -> Option<String> {
    // An on-disk Active record may belong to a crashed or another process.
    // Only a successfully persisted begin_run in this instance grants access.
    data.active.clone()
}

fn interrupt_run(run: &mut RunRecord) {
    let now = now_secs();
    run.state = RunState::Interrupted;
    run.ended = Some(now);
    for op in run.operations.iter_mut() {
        if op.status == OpStatus::Pending {
            op.status = OpStatus::Unknown;
            op.finished = Some(now);
            op.detail = Some("process ended before the operation result was recorded".into());
        }
    }
}

fn build_report(file: &JournalFile, load_error: Option<String>) -> RecoveryReport {
    let mut report = RecoveryReport {
        session_id: Some(file.session_id.clone()),
        journal_error: load_error,
        dropped_runs: file.dropped_runs,
        truncated_operations: file.truncated_operations,
        ..RecoveryReport::default()
    };
    for run in &file.runs {
        if matches!(run.state, RunState::Active | RunState::Interrupted) {
            report.interrupted_runs += 1;
        }
        for op in &run.operations {
            match op.status {
                OpStatus::Pending | OpStatus::Unknown => {
                    report.unknown_operations.push(recovered(run, op));
                }
                OpStatus::Failed => report.failed_operations.push(recovered(run, op)),
                OpStatus::Executed => {
                    report.executed_operations += 1;
                    report.executed.push(recovered(run, op));
                }
            }
        }
    }
    report
}

fn recovered(run: &RunRecord, op: &OperationRecord) -> RecoveredOperation {
    RecoveredOperation {
        run_id: run.run_id.clone(),
        turn: run.turn,
        operation: op.operation.clone(),
        target: op.target.clone(),
        status: op.status,
        detail: op.detail.clone(),
        recorded_at: op.finished.unwrap_or(op.started),
    }
}

fn load_file(path: &Path, session_id: &str) -> (JournalFile, Option<String>) {
    let fresh = || JournalFile {
        version: JOURNAL_VERSION,
        session_id: session_id.to_owned(),
        updated: now_secs(),
        dropped_runs: 0,
        truncated_operations: 0,
        runs: Vec::new(),
    };
    let bytes = (|| -> std::io::Result<Vec<u8>> {
        validate_locations(path, false).map_err(std::io::Error::other)?;
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file() || metadata.len() > MAX_JOURNAL_FILE_SIZE as u64 {
            return Err(std::io::Error::other(
                "journal must be a regular file within its size limit",
            ));
        }
        let mut bytes = Vec::new();
        fs::File::open(path)?
            .take((MAX_JOURNAL_FILE_SIZE + 1) as u64)
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    })();
    match bytes {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (fresh(), None),
        Err(error) => (
            fresh(),
            Some(format!(
                "cannot read execution journal {}: {error}",
                path.display()
            )),
        ),
        Ok(bytes) if bytes.len() > MAX_JOURNAL_FILE_SIZE => (
            fresh(),
            Some(format!(
                "execution journal {} exceeds {MAX_JOURNAL_FILE_SIZE} bytes",
                path.display()
            )),
        ),
        Ok(bytes) => match serde_json::from_slice::<JournalFile>(&bytes) {
            Ok(file) if file.version == JOURNAL_VERSION && file.session_id == session_id => {
                (file, None)
            }
            Ok(file) if file.session_id != session_id => (
                fresh(),
                Some("execution journal session identity does not match".into()),
            ),
            Ok(file) => (
                fresh(),
                Some(format!(
                    "unsupported execution journal version {} (expected {JOURNAL_VERSION})",
                    file.version
                )),
            ),
            Err(error) => (
                fresh(),
                Some(format!(
                    "corrupt execution journal {}: {error}",
                    path.display()
                )),
            ),
        },
    }
}

fn persist(data: &JournalData, candidate: &JournalFile) -> Result<JournalFile, String> {
    if let Some(error) = &data.load_error {
        return Err(format!(
            "Execution journal requires inspection; existing evidence was preserved: {error}"
        ));
    }
    let Some(dir) = data.path.parent() else {
        return Err("Execution journal path has no parent directory".into());
    };
    validate_locations(&data.path, true)?;
    set_private_dir(dir)?;

    let mut enforced = candidate.clone();
    enforce_limits(&mut enforced);
    let mut payload = serde_json::to_vec_pretty(&enforced)
        .map_err(|error| format!("cannot serialize execution journal: {error}"))?;
    while payload.len() > MAX_JOURNAL_FILE_SIZE && enforced.runs.len() > 1 {
        enforced.runs.remove(0);
        enforced.dropped_runs += 1;
        payload = serde_json::to_vec_pretty(&enforced)
            .map_err(|error| format!("cannot serialize execution journal: {error}"))?;
    }
    if payload.len() > MAX_JOURNAL_FILE_SIZE {
        return Err(format!(
            "execution journal exceeds {MAX_JOURNAL_FILE_SIZE} bytes even after trimming"
        ));
    }

    let mut temp = NamedTempFile::new_in(dir).map_err(|error| {
        format!(
            "cannot create journal tempfile in {}: {error}",
            dir.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("cannot set journal mode 0600: {error}"))?;
    }
    temp.write_all(&payload)
        .map_err(|error| format!("cannot write execution journal: {error}"))?;
    temp.as_file()
        .sync_all()
        .map_err(|error| format!("cannot sync execution journal: {error}"))?;
    temp.persist(&data.path).map_err(|error| {
        format!(
            "cannot persist execution journal to {}: {error}",
            data.path.display()
        )
    })?;
    Ok(enforced)
}

fn enforce_limits(file: &mut JournalFile) {
    while file.runs.len() > MAX_JOURNAL_RUNS {
        file.runs.remove(0);
        file.dropped_runs += 1;
    }
    let mut truncated = 0u64;
    for run in &mut file.runs {
        while run.operations.len() > MAX_RUN_OPERATIONS {
            run.operations.remove(0);
            truncated += 1;
        }
        for op in &mut run.operations {
            op.operation = truncate(&op.operation, MAX_TARGET_CHARS);
            op.target = truncate(&op.target, MAX_TARGET_CHARS);
            if let Some(detail) = &op.detail {
                let trimmed = truncate(detail, MAX_DETAIL_CHARS);
                op.detail = Some(trimmed);
            }
        }
    }
    file.truncated_operations += truncated;
}

// Reject static symlinks in the two product-managed directories and the final
// file. As with workspace paths, this is not an OS sandbox against rename races.
fn validate_locations(path: &Path, create: bool) -> Result<(), String> {
    let dir = path.parent().ok_or("invalid journal directory")?;
    let product = dir.parent().ok_or("invalid journal root")?;
    for directory in [product, dir] {
        if create {
            match fs::create_dir(directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(format!(
                        "cannot create journal directory {}: {error}",
                        directory.display()
                    ));
                }
            }
        }
        match fs::symlink_metadata(directory) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err("journal directories must be real directories, not symlinks".into());
            }
            Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err("journal must be a regular file, not a symlink".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(unix)]
fn set_private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|error| error.to_string())
}

#[cfg(not(unix))]
fn set_private_dir(_dir: &Path) -> Result<(), String> {
    Ok(())
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let mut out: String = value.chars().take(max_chars).collect();
    out.push_str("...");
    out
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_file(root: &Path, session_id: &str) -> JournalFile {
        let path = journal_path(root, session_id);
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap()
    }

    #[test]
    fn records_intent_result_and_verifies_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path(), "sess-1");
        journal.begin_run().unwrap();
        journal.record_intent("write", "a.txt").unwrap();
        journal
            .record_result("write", "a.txt", &Ok("wrote 3 bytes".to_string()))
            .unwrap();
        journal.finish_run(RunState::Completed).unwrap();

        let file = read_file(dir.path(), "sess-1");
        assert_eq!(file.version, JOURNAL_VERSION);
        assert_eq!(file.runs.len(), 1);
        assert_eq!(file.runs[0].state, RunState::Completed);
        assert_eq!(file.runs[0].operations.len(), 1);
        assert_eq!(file.runs[0].operations[0].status, OpStatus::Executed);

        let report = Journal::open(dir.path(), "sess-1")
            .mark_interrupted()
            .unwrap();
        assert_eq!(report.executed_operations, 1);
        assert!(!report.has_unfinished());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_path = journal_path(dir.path(), "sess-1");
            assert_eq!(
                fs::metadata(&file_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(file_path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn failed_turn_keeps_executed_evidence_and_marks_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path(), "sess-2");
        journal.begin_run().unwrap();
        journal.record_intent("write", "good.txt").unwrap();
        journal
            .record_result("write", "good.txt", &Ok("ok".to_string()))
            .unwrap();
        journal.record_intent("bash", "sleep 99").unwrap();
        journal.finish_run(RunState::Failed).unwrap();

        let report = journal.recovery_report().unwrap();
        assert_eq!(report.executed_operations, 1);
        assert_eq!(report.unknown_operations.len(), 1);
        assert_eq!(report.unknown_operations[0].operation, "bash");
        assert!(report.has_unfinished());
        // 已执行操作保留为 Executed，不会被改成 Unknown。
        let file = read_file(dir.path(), "sess-2");
        assert_eq!(file.runs[0].operations[0].status, OpStatus::Executed);
        assert_eq!(file.runs[0].operations[1].status, OpStatus::Unknown);
        assert_eq!(file.runs[0].state, RunState::Failed);
    }

    #[test]
    fn cancelled_turn_marks_pending_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path(), "sess-3");
        journal.begin_run().unwrap();
        journal.record_intent("bash", "long-running").unwrap();
        journal.finish_run(RunState::Cancelled).unwrap();
        let report = journal.recovery_report().unwrap();
        assert_eq!(report.unknown_operations.len(), 1);
        assert_eq!(report.unknown_operations[0].status, OpStatus::Unknown);
        assert!(report.has_unfinished());
    }

    #[test]
    fn restart_marks_active_run_interrupted_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        {
            let journal = Journal::open(dir.path(), "sess-4");
            journal.begin_run().unwrap();
            journal.record_intent("write", "partial.txt").unwrap();
            // 模拟进程崩溃：不调用 finish_run。
        }
        let reopened = Journal::open(dir.path(), "sess-4");
        assert!(reopened.recovery_report().unwrap().has_unfinished());
        let report = reopened.mark_interrupted().unwrap();
        assert_eq!(report.interrupted_runs, 1);
        assert_eq!(report.unknown_operations.len(), 1);
        let file = read_file(dir.path(), "sess-4");
        assert_eq!(file.runs[0].state, RunState::Interrupted);
        assert_eq!(file.runs[0].operations[0].status, OpStatus::Unknown);
    }

    #[test]
    fn write_failure_blocks_intent() {
        let dir = tempfile::tempdir().unwrap();
        // `.mio/journal` 是普通文件，无法作为目录写入。
        fs::create_dir_all(dir.path().join(".mio")).unwrap();
        fs::write(dir.path().join(".mio/journal"), "not a directory").unwrap();
        let journal = Journal::open(dir.path(), "sess-5");
        let _ = journal.begin_run();
        let err = journal.record_intent("write", "a.txt").unwrap_err();
        assert!(!err.is_empty());
        // 失败被记录为加载/写入诊断，而不是静默成功。
        let report = journal.recovery_report().unwrap();
        assert!(report.journal_error.is_some());
    }

    #[test]
    fn disabled_journal_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::disabled();
        assert!(!journal.is_enabled());
        assert!(journal.path().is_none());
        assert!(journal.begin_run().is_ok());
        assert!(journal.record_intent("write", "a.txt").is_ok());
        assert!(journal.finish_run(RunState::Completed).is_ok());
        assert!(journal.recovery_report().is_none());
        assert!(!dir.path().join(".mio").exists());
    }

    #[test]
    fn size_limit_drops_oldest_runs() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path(), "sess-6");
        for _ in 0..(MAX_JOURNAL_RUNS + 5) {
            journal.begin_run().unwrap();
            journal.finish_run(RunState::Completed).unwrap();
        }
        let file = read_file(dir.path(), "sess-6");
        assert!(file.runs.len() <= MAX_JOURNAL_RUNS);
        assert!(file.dropped_runs >= 5);
    }

    #[test]
    fn long_targets_and_details_are_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path(), "sess-7");
        journal.begin_run().unwrap();
        let huge = "x".repeat(MAX_TARGET_CHARS * 4);
        journal.record_intent("write", &huge).unwrap();
        journal
            .record_result("write", &huge, &Err(huge.clone()))
            .unwrap();
        journal.finish_run(RunState::Completed).unwrap();
        let file = read_file(dir.path(), "sess-7");
        let op = &file.runs[0].operations[0];
        assert!(op.target.chars().count() <= MAX_TARGET_CHARS + 3);
        assert_eq!(op.status, OpStatus::Failed);
        assert!(
            op.detail
                .as_ref()
                .is_some_and(|d| d.chars().count() <= MAX_DETAIL_CHARS + 3)
        );
    }

    #[test]
    fn corrupt_journal_is_preserved_and_blocks_new_side_effects() {
        let dir = tempfile::tempdir().unwrap();
        let path = journal_path(dir.path(), "sess-8");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{ not json").unwrap();
        let journal = Journal::open(dir.path(), "sess-8");
        assert!(journal.recovery_report().unwrap().journal_error.is_some());
        assert!(journal.begin_run().is_err());
        assert!(journal.record_intent("write", "a.txt").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn oversized_or_mismatched_journals_are_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = journal_path(dir.path(), "bounded");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, vec![b'x'; MAX_JOURNAL_FILE_SIZE + 1]).unwrap();
        let journal = Journal::open(dir.path(), "bounded");
        assert!(journal.begin_run().is_err());
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            (MAX_JOURNAL_FILE_SIZE + 1) as u64
        );
        let good = Journal::open(dir.path(), "source");
        good.begin_run().unwrap();
        good.finish_run(RunState::Completed).unwrap();
        let original = fs::read(journal_path(dir.path(), "source")).unwrap();
        fs::write(&path, &original).unwrap();
        let mismatched = Journal::open(dir.path(), "bounded");
        assert!(mismatched.begin_run().is_err());
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_journal_directory_never_changes_external_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".mio")).unwrap();
        std::os::unix::fs::symlink(external.path(), dir.path().join(".mio/journal")).unwrap();
        let journal = Journal::open(dir.path(), "linked");
        assert!(journal.begin_run().is_err());
        assert_eq!(fs::read_dir(external.path()).unwrap().count(), 0);
    }
}
