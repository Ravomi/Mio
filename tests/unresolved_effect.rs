//! DES-09 跨重启未知副作用账本的集成测试。
//!
//! 该文件通过 `#[path]` 直接编译 `src/unresolved_effect.rs`，因此在 `lib.rs` 导出该
//! 模块之前即可独立运行：`cargo +1.95.0 test --locked --test unresolved_effect`。
//!
//! 覆盖真实磁盘与真实进程：
//!
//! - 真实临时工作区上的 `flock` + 原子 `rename` 账本（目录 0700、文件 0600）；
//! - 真实子进程（复用同一个测试二进制，`--exact unresolved_effect_child_entrypoint`）
//!   并发派发同一主键、在父进程取得快照后追加新证据；
//! - 真实 SIGKILL：派发进程被杀后恢复必须保守升级为 `Unknown`，而仍然存活的 owner
//!   必须保持 `Outstanding` 且不被误判死亡。
//!
//! 不触网、不调用模型、不读取任何凭据。

#[path = "../src/unresolved_effect.rs"]
mod unresolved_effect;

use serde_json::{Value, json};
#[cfg(target_os = "linux")]
use std::time::Instant;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};
use unresolved_effect::{
    Coverage, CoverageCommit, DispatchAdmission, DispatchClaim, EvidenceResolution, LEDGER_DIR,
    LEDGER_FILE, LEDGER_LOCK_FILE, LedgerMode, MAX_FACTS_PER_RECORD, MAX_LEDGER_ENTRIES,
    MAX_LEDGER_FILE_SIZE, RecoveryOutcome, UnknownEffectInput, UnknownFactReason,
    UnresolvedEffectError, UnresolvedEffectKey, UnresolvedEffectLedger, UnresolvedEffectState,
    UnresolvedSnapshotKey, target_digest,
};
#[cfg(target_os = "linux")]
use unresolved_effect::{Liveness, capture_owner_for_pid};

const ENV_ACTION: &str = "MIO_UE_ACTION";
const ENV_ROOT: &str = "MIO_UE_ROOT";
const ENV_SCOPE: &str = "MIO_UE_SCOPE";
const ENV_INDEX: &str = "MIO_UE_INDEX";
const ENV_OPERATION: &str = "MIO_UE_OPERATION";
const ENV_RESULT: &str = "MIO_UE_RESULT";
const ENV_STARTED: &str = "MIO_UE_STARTED";

// ---------------------------------------------------------------------------
// 子进程入口
// ---------------------------------------------------------------------------

/// 供父进程复用的子进程入口；普通测试运行中因缺少环境变量而直接返回。
#[test]
fn unresolved_effect_child_entrypoint() {
    let Ok(action) = std::env::var(ENV_ACTION) else {
        return;
    };
    if let Err(error) = run_child(&action) {
        eprintln!("unresolved_effect child failed: {error}");
        std::process::exit(2);
    }
    std::process::exit(0);
}

fn run_child(action: &str) -> Result<(), String> {
    let root = std::env::var(ENV_ROOT).map_err(|_| "missing MIO_UE_ROOT".to_owned())?;
    let root = PathBuf::from(root);
    match action {
        // 与其他子进程竞争同一个 (scope, call_index)。
        "dispatch_shared" => {
            let key = env_key()?;
            let claim = UnresolvedEffectLedger::persistent(&root)
                .record_dispatched(&key)
                .map_err(|error| error.to_string())?;
            let label = match claim {
                DispatchClaim::Acquired => "ACQUIRED",
                DispatchClaim::Existing(_) => "EXISTING",
            };
            write_result(label)
        }
        // 在另一个进程已播种之后，以整个工作区重查为准申请派发权。
        "claim_dispatch" => {
            let key = env_key()?;
            let admission = UnresolvedEffectLedger::persistent(&root)
                .claim_dispatch(&key, true)
                .map_err(|error| error.to_string())?;
            let label = match admission {
                DispatchAdmission::Acquired => "ACQUIRED".to_owned(),
                DispatchAdmission::Existing(state) => format!("EXISTING:{}", state.as_str()),
                DispatchAdmission::BlockedByUnknown { pending } => format!("BLOCKED:{pending}"),
                DispatchAdmission::BlockedByCoverage { .. } => "BLOCKED_COVERAGE".to_owned(),
            };
            write_result(&label)
        }
        // 与其他子进程竞争同一次覆盖初始化。
        "commit_coverage" => {
            let commit = UnresolvedEffectLedger::persistent(&root)
                .commit_initial_coverage(&[], true)
                .map_err(|error| error.to_string())?;
            let label = match commit {
                CoverageCommit::Initialized => "INITIALIZED",
                CoverageCommit::AlreadyInitialized => "ALREADY_INITIALIZED",
                CoverageCommit::MarkedIncomplete => "MARKED_INCOMPLETE",
                CoverageCommit::AlreadyIncomplete => "ALREADY_INCOMPLETE",
            };
            write_result(label)
        }
        // 在父进程已取得快照之后追加一条全新的未知证据。
        "record_unknown" => {
            let key = env_key()?;
            let operation = std::env::var(ENV_OPERATION).unwrap_or_else(|_| "write".to_owned());
            let input = UnknownEffectInput::new(operation, UnknownFactReason::TransportLost)
                .with_evidence_id(uuid::Uuid::new_v4());
            UnresolvedEffectLedger::persistent(&root)
                .record_unknown(&key, input)
                .map_err(|error| error.to_string())?;
            write_result("OK")
        }
        // 持久化派发意图后停住，等父进程 SIGKILL。
        "dispatch_hold" => {
            let key = env_key()?;
            UnresolvedEffectLedger::persistent(&root)
                .record_dispatched(&key)
                .map_err(|error| error.to_string())?;
            if let Ok(started) = std::env::var(ENV_STARTED) {
                fs::write(started, b"STARTED").map_err(|error| error.to_string())?;
            }
            std::thread::sleep(Duration::from_secs(120));
            Ok(())
        }
        other => Err(format!("unknown action '{other}'")),
    }
}

fn env_key() -> Result<UnresolvedEffectKey, String> {
    let scope = std::env::var(ENV_SCOPE).map_err(|_| "missing MIO_UE_SCOPE".to_owned())?;
    let index = std::env::var(ENV_INDEX)
        .map_err(|_| "missing MIO_UE_INDEX".to_owned())?
        .parse::<u64>()
        .map_err(|error| format!("bad MIO_UE_INDEX: {error}"))?;
    UnresolvedEffectKey::new(scope, index).map_err(|error| error.to_string())
}

fn write_result(label: &str) -> Result<(), String> {
    if let Ok(result) = std::env::var(ENV_RESULT) {
        fs::write(result, label).map_err(|error| error.to_string())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 测试辅助
// ---------------------------------------------------------------------------

fn record_key(scope: &str, call_index: u64) -> UnresolvedEffectKey {
    UnresolvedEffectKey::new(scope, call_index).expect("valid key")
}

/// Commit a complete empty coverage epoch, the same way a real whole-workspace
/// scan with no journals does before any write is admitted.
fn initialize_coverage(ledger: &UnresolvedEffectLedger) {
    assert_eq!(
        ledger.commit_initial_coverage(&[], true).unwrap(),
        CoverageCommit::Initialized
    );
}

fn ledger_file(root: &Path) -> PathBuf {
    root.join(LEDGER_DIR).join(LEDGER_FILE)
}

fn raw_ledger(root: &Path) -> String {
    fs::read_to_string(ledger_file(root)).expect("ledger file must exist")
}

fn write_raw_ledger(root: &Path, payload: &str) {
    let dir = root.join(LEDGER_DIR);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(LEDGER_FILE), payload).unwrap();
}

fn raw_record(scope: &str, call_index: u64, state: &str, owner: Option<Value>) -> Value {
    let mut record = json!({
        "scope": scope,
        "call_index": call_index,
        "state": state,
        "revision": 1,
        "recorded_at": 0,
        "updated": 0,
    });
    if let Some(owner) = owner {
        record["owner"] = owner;
    }
    record
}

fn raw_ledger_value(entries: Value) -> String {
    json!({"version": 1, "updated": 0, "entries": entries}).to_string()
}

fn unknown_input(operation: &str, target: Option<&str>) -> UnknownEffectInput {
    let input = UnknownEffectInput::new(operation, UnknownFactReason::ResultUnknown);
    match target {
        Some(target) => input.with_target(target),
        None => input,
    }
}

fn child_command(root: &Path, action: &str) -> Command {
    let exe = std::env::current_exe().expect("current test executable");
    let mut command = Command::new(exe);
    command
        .arg("--exact")
        .arg("unresolved_effect_child_entrypoint")
        .arg("--test-threads=1")
        .env(ENV_ACTION, action)
        .env(ENV_ROOT, root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    command
}

#[cfg(target_os = "linux")]
fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

#[cfg(target_os = "linux")]
fn wait_for_file(path: &Path) {
    wait_until(&format!("{} to appear", path.display()), || path.exists());
}

/// 子进程句柄：无论测试如何结束都 kill + wait，不留后台进程。
struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn spawn(command: &mut Command) -> Self {
        Self(Some(command.spawn().expect("spawn child process")))
    }

    fn kill_and_wait(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.kill_and_wait();
    }
}

#[cfg(unix)]
fn permission_mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

// ---------------------------------------------------------------------------
// 模式：disabled 零写、read_only 零创建
// ---------------------------------------------------------------------------

#[test]
fn disabled_ledger_performs_zero_writes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let ledger = UnresolvedEffectLedger::disabled();
    assert_eq!(ledger.mode(), LedgerMode::Disabled);
    assert!(!ledger.mode().is_writable());
    assert!(ledger.path().is_none());

    let key = record_key("run-disabled", 1);
    assert!(matches!(
        ledger.record_dispatched(&key),
        Err(UnresolvedEffectError::Disabled)
    ));
    assert!(matches!(
        ledger.record_unknown(&key, unknown_input("write", None)),
        Err(UnresolvedEffectError::Disabled)
    ));
    assert!(matches!(
        ledger.resolve_confirmed(&key),
        Err(UnresolvedEffectError::Disabled)
    ));
    let snapshot_key = "0".repeat(64).parse::<UnresolvedSnapshotKey>().unwrap();
    assert!(matches!(
        ledger.acknowledge(&key, &snapshot_key, None),
        Err(UnresolvedEffectError::Disabled)
    ));
    assert!(matches!(
        ledger.lookup(&key),
        Err(UnresolvedEffectError::Disabled)
    ));
    assert!(matches!(
        ledger.snapshot(),
        Err(UnresolvedEffectError::Disabled)
    ));
    assert!(matches!(
        ledger.recover_outstanding(),
        Err(UnresolvedEffectError::Disabled)
    ));

    assert_eq!(
        fs::read_dir(root).unwrap().count(),
        0,
        "a disabled ledger must not create anything"
    );
}

#[test]
fn read_only_queries_and_recovery_create_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let ledger = UnresolvedEffectLedger::read_only(root);
    assert_eq!(ledger.mode(), LedgerMode::ReadOnly);
    assert!(!ledger.mode().is_writable());
    assert_eq!(ledger.path(), Some(ledger_file(root)));

    let snapshot = ledger.snapshot().unwrap();
    assert!(!snapshot.present);
    assert!(snapshot.is_empty());
    let report = ledger.recover_outstanding().unwrap();
    assert!(!report.present);
    assert!(!report.persisted);
    assert!(report.entries.is_empty());
    assert!(matches!(
        ledger.record_dispatched(&record_key("run-read-only", 1)),
        Err(UnresolvedEffectError::ReadOnly)
    ));
    assert!(
        !root.join(".mio").exists(),
        "read-only recovery must not create the ledger directory"
    );

    // 已有账本：只读可查，恢复计划给出但绝不落盘。
    UnresolvedEffectLedger::persistent(root)
        .record_dispatched(&record_key("run-read-only", 1))
        .unwrap();
    let before = fs::read(ledger_file(root)).unwrap();
    assert!(ledger.snapshot().unwrap().present);
    assert!(
        ledger
            .lookup(&record_key("run-read-only", 1))
            .unwrap()
            .is_some()
    );
    let report = ledger.recover_outstanding().unwrap();
    assert!(report.present);
    assert!(!report.persisted, "read-only recovery must not persist");
    assert_eq!(report.entries.len(), 1);
    #[cfg(target_os = "linux")]
    assert_eq!(report.entries[0].outcome, RecoveryOutcome::KeptOutstanding);
    #[cfg(not(target_os = "linux"))]
    assert_eq!(
        report.entries[0].outcome,
        RecoveryOutcome::UpgradedToUnknown
    );
    assert_eq!(
        fs::read(ledger_file(root)).unwrap(),
        before,
        "read-only queries must not rewrite the ledger"
    );
}

// ---------------------------------------------------------------------------
// 主键、状态机与精确确认
// ---------------------------------------------------------------------------

#[test]
fn dispatch_is_recorded_once_and_duplicate_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let ledger = UnresolvedEffectLedger::persistent(root);
    let key = record_key("run-1", 7);
    assert_eq!(
        ledger.record_dispatched(&key).unwrap(),
        DispatchClaim::Acquired
    );
    let duplicate = ledger.record_dispatched(&key).unwrap();
    assert!(!duplicate.is_acquired());
    assert_eq!(
        duplicate,
        DispatchClaim::Existing(UnresolvedEffectState::Outstanding)
    );

    let snapshot = ledger.snapshot().unwrap();
    assert!(snapshot.present);
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot.outstanding_count(), 1);
    assert_eq!(snapshot.unknown_count(), 0);
    assert!(!snapshot.is_blocked());
    let entry = &snapshot.entries[0];
    assert_eq!(entry.record.scope, "run-1");
    assert_eq!(entry.record.call_index, 7);
    assert_eq!(entry.record.state, UnresolvedEffectState::Outstanding);
    assert_eq!(entry.record.key(), key);
    assert_eq!(entry.snapshot_key, entry.record.snapshot_key());

    // 非法主键在触碰磁盘之前就被拒绝。
    let blank = UnresolvedEffectKey {
        scope: "   ".into(),
        call_index: 1,
    };
    assert!(matches!(
        ledger.record_dispatched(&blank),
        Err(UnresolvedEffectError::InvalidKey(_))
    ));
    let zero = UnresolvedEffectKey {
        scope: "run-1".into(),
        call_index: 0,
    };
    assert!(matches!(
        ledger.record_dispatched(&zero),
        Err(UnresolvedEffectError::InvalidKey(_))
    ));
}

#[test]
fn unknown_can_only_leave_via_the_exact_acknowledgement() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let ledger = UnresolvedEffectLedger::persistent(root);
    let key = record_key("run-ack", 1);
    ledger.record_dispatched(&key).unwrap();
    let evidence_id = uuid::Uuid::new_v4();
    let observation = ledger
        .record_unknown(
            &key,
            UnknownEffectInput::new("mcp__probe__write", UnknownFactReason::TransportLost)
                .with_target("/tmp/effect.txt")
                .with_evidence_id(evidence_id),
        )
        .unwrap();
    assert_eq!(observation.record.state, UnresolvedEffectState::Unknown);
    assert_eq!(observation.record.facts.len(), 1);
    assert!(observation.record.acknowledgement.is_none());
    assert_eq!(observation.record.facts[0].evidence_id, Some(evidence_id));
    assert_eq!(
        observation.record.facts[0].reason,
        UnknownFactReason::TransportLost
    );

    // Outstanding 不能直接 ack；Unknown 不能由 resolve_confirmed 静默解决。
    let other = record_key("run-ack", 2);
    ledger.record_dispatched(&other).unwrap();
    let other_observation = ledger.lookup(&other).unwrap().unwrap();
    assert!(matches!(
        ledger.acknowledge(&other, &other_observation.snapshot_key, None),
        Err(UnresolvedEffectError::IllegalTransition { .. })
    ));
    assert!(matches!(
        ledger.resolve_confirmed(&key),
        Err(UnresolvedEffectError::IllegalTransition { .. })
    ));

    // 陈旧/伪造的快照键一律拒绝。
    let wrong = "0".repeat(64).parse::<UnresolvedSnapshotKey>().unwrap();
    assert_eq!(
        ledger.acknowledge(&key, &wrong, None),
        Err(UnresolvedEffectError::StaleConfirmation)
    );
    let malformed = "not-a-digest".parse::<UnresolvedSnapshotKey>();
    assert!(matches!(
        malformed,
        Err(UnresolvedEffectError::InvalidSnapshotKey(_))
    ));

    // 精确 ack：原始 unknown 事实保留，确认摘要逐字绑定。
    let acked = ledger
        .acknowledge(
            &key,
            &observation.snapshot_key,
            Some("host verified the site"),
        )
        .unwrap();
    assert_eq!(acked.record.state, UnresolvedEffectState::Acknowledged);
    assert_eq!(acked.record.facts.len(), 1);
    assert_eq!(acked.record.facts[0].evidence_id, Some(evidence_id));
    let ack = acked.record.acknowledgement.as_ref().unwrap();
    assert_eq!(ack.snapshot_key, observation.snapshot_key.as_str());
    assert_eq!(ack.note, "host verified the site");
    assert!(ack.confirmed_at > 0);

    // 重复提交同一快照键是幂等 no-op。
    let again = ledger
        .acknowledge(&key, &observation.snapshot_key, None)
        .unwrap();
    assert_eq!(again.record.state, UnresolvedEffectState::Acknowledged);

    // 重新打开后 ack 摘要仍与证据一致（加载时会重新校验）。
    let reopened = UnresolvedEffectLedger::persistent(root)
        .lookup(&key)
        .unwrap()
        .unwrap();
    assert_eq!(reopened.record.state, UnresolvedEffectState::Acknowledged);
    assert_eq!(
        reopened.record.acknowledgement.as_ref().unwrap().note,
        "host verified the site"
    );
    // 已确认的记录不再阻塞。
    assert!(!ledger.snapshot().unwrap().is_blocked());
}

#[test]
fn facts_are_append_only_bounded_and_never_evicted() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let ledger = UnresolvedEffectLedger::persistent(root);
    let key = record_key("run-facts", 1);

    let mut first_key = None;
    for index in 0..(MAX_FACTS_PER_RECORD + 4) {
        let observation = ledger
            .record_unknown(
                &key,
                UnknownEffectInput::new("write", UnknownFactReason::ResultUnknown)
                    .with_target(format!("target-{index}"))
                    .with_evidence_id(uuid::Uuid::new_v4()),
            )
            .unwrap();
        if index == 0 {
            first_key = Some(observation.snapshot_key.clone());
        }
    }

    let observation = ledger.lookup(&key).unwrap().unwrap();
    assert_eq!(observation.record.facts.len(), MAX_FACTS_PER_RECORD);
    assert_eq!(observation.record.facts_dropped, 4);
    // 最早的事实仍在：有界绝不驱逐。
    assert_eq!(observation.record.facts[0].fact_seq, 1);
    assert_eq!(
        observation.record.facts[0].target_sha256,
        target_digest("target-0")
    );
    // 新证据让旧快照键失效：不能被旧 ack 一起吞掉。
    assert_eq!(
        ledger.acknowledge(&key, first_key.as_ref().unwrap(), None),
        Err(UnresolvedEffectError::StaleConfirmation)
    );
    // 证据不完整（有被省略的事实）时，即使用当前快照键也必须拒绝确认：
    // 宿主不能确认一份它没有完整看到过的现场。Unknown 与全部事实保持原样。
    assert_eq!(
        ledger.acknowledge(&key, &observation.snapshot_key, None),
        Err(UnresolvedEffectError::IncompleteEvidence)
    );
    let still = ledger.lookup(&key).unwrap().unwrap();
    assert_eq!(still.record.state, UnresolvedEffectState::Unknown);
    assert_eq!(still.record.facts.len(), MAX_FACTS_PER_RECORD);
    assert_eq!(still.record.facts_dropped, 4);
    assert!(still.record.acknowledgement.is_none());
}

#[test]
fn durable_admission_rechecks_unknowns_and_ack_lets_a_write_continue() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let ledger = UnresolvedEffectLedger::persistent(root);
    initialize_coverage(&ledger);

    // Another process or session records an Unknown after this run's guard was
    // seeded. A write/external dispatch must re-check the whole workspace in
    // the same flock transaction, not trust the earlier seed.
    let blocker = record_key("run-other", 1);
    let observation = ledger
        .record_unknown(&blocker, unknown_input("mcp__srv__write", None))
        .unwrap();
    assert_eq!(
        ledger
            .claim_dispatch(&record_key("run-this", 1), true)
            .unwrap(),
        DispatchAdmission::BlockedByUnknown { pending: 1 }
    );
    assert!(
        ledger.lookup(&record_key("run-this", 1)).unwrap().is_none(),
        "a refused dispatch must not be claimed"
    );

    // A read-only call is never blocked by a pending unknown.
    assert_eq!(
        ledger
            .claim_dispatch(&record_key("run-read", 1), false)
            .unwrap(),
        DispatchAdmission::Acquired
    );

    // The same-snapshot explicit confirmation lets a new write continue.
    ledger
        .acknowledge(&blocker, &observation.snapshot_key, None)
        .unwrap();
    assert_eq!(
        ledger
            .claim_dispatch(&record_key("run-this", 1), true)
            .unwrap(),
        DispatchAdmission::Acquired
    );
}

#[test]
fn release_undispatched_retracts_only_pure_outstanding_intents() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let ledger = UnresolvedEffectLedger::persistent(root);
    initialize_coverage(&ledger);

    let pure = record_key("run-pure", 1);
    assert_eq!(
        ledger.claim_dispatch(&pure, true).unwrap(),
        DispatchAdmission::Acquired
    );
    assert!(ledger.release_undispatched(&pure).unwrap());
    assert!(
        ledger.lookup(&pure).unwrap().is_none(),
        "a provably undispatched intent is retracted rather than left to become a false unknown"
    );
    // Retracting an already-gone intent is a no-op.
    assert!(!ledger.release_undispatched(&pure).unwrap());

    // A record carrying real unknown evidence can never be retracted.
    let unknown = record_key("run-evidence", 1);
    ledger
        .record_unknown(&unknown, unknown_input("write", None))
        .unwrap();
    assert_eq!(
        ledger.release_undispatched(&unknown),
        Err(UnresolvedEffectError::NotRetractable)
    );
    assert_eq!(
        ledger.lookup(&unknown).unwrap().unwrap().record.state,
        UnresolvedEffectState::Unknown
    );
}

#[test]
fn terminal_states_do_not_regress() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let ledger = UnresolvedEffectLedger::persistent(root);

    let resolved = record_key("run-terminal", 1);
    ledger.record_dispatched(&resolved).unwrap();
    assert!(ledger.resolve_confirmed(&resolved).unwrap());
    assert!(!ledger.resolve_confirmed(&resolved).unwrap());
    assert!(matches!(
        ledger.record_unknown(&resolved, unknown_input("write", None)),
        Err(UnresolvedEffectError::IllegalTransition {
            from: UnresolvedEffectState::Resolved,
            to: UnresolvedEffectState::Unknown
        })
    ));

    let unknown = record_key("run-terminal", 2);
    ledger
        .record_unknown(&unknown, unknown_input("write", None))
        .unwrap();
    assert!(matches!(
        ledger.resolve_confirmed(&unknown),
        Err(UnresolvedEffectError::IllegalTransition {
            from: UnresolvedEffectState::Unknown,
            to: UnresolvedEffectState::Resolved
        })
    ));
    let observation = ledger.lookup(&unknown).unwrap().unwrap();
    ledger
        .acknowledge(&unknown, &observation.snapshot_key, None)
        .unwrap();
    let new_evidence = unknown_input("write", None).with_evidence_id(uuid::Uuid::new_v4());
    assert!(matches!(
        ledger.record_unknown(&unknown, new_evidence),
        Err(UnresolvedEffectError::IllegalTransition {
            from: UnresolvedEffectState::Acknowledged,
            to: UnresolvedEffectState::Unknown
        })
    ));
    assert_eq!(
        ledger.lookup(&unknown).unwrap().unwrap().record.state,
        UnresolvedEffectState::Acknowledged
    );

    // 缺失记录的 resolve_confirmed 是明确的 no-op。
    assert!(
        !ledger
            .resolve_confirmed(&record_key("run-terminal", 99))
            .unwrap()
    );
    assert!(matches!(
        ledger.acknowledge(
            &record_key("run-terminal", 99),
            &"1".repeat(64).parse::<UnresolvedSnapshotKey>().unwrap(),
            None
        ),
        Err(UnresolvedEffectError::NotFound)
    ));
}

#[test]
fn snapshot_is_sorted_and_reports_blocking_counts() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let ledger = UnresolvedEffectLedger::persistent(root);
    ledger.record_dispatched(&record_key("run-z", 2)).unwrap();
    ledger.record_dispatched(&record_key("run-z", 1)).unwrap();
    ledger.record_dispatched(&record_key("run-a", 9)).unwrap();
    ledger
        .record_unknown(&record_key("run-z", 1), unknown_input("write", None))
        .unwrap();

    let snapshot = ledger.snapshot().unwrap();
    let order: Vec<(String, u64)> = snapshot
        .entries
        .iter()
        .map(|entry| (entry.record.scope.clone(), entry.record.call_index))
        .collect();
    assert_eq!(
        order,
        vec![
            ("run-a".to_owned(), 9),
            ("run-z".to_owned(), 1),
            ("run-z".to_owned(), 2),
        ]
    );
    assert_eq!(snapshot.unknown_count(), 1);
    assert_eq!(snapshot.outstanding_count(), 2);
    assert!(snapshot.is_blocked());
    assert_eq!(snapshot.blocking().count(), 1);
}

// ---------------------------------------------------------------------------
// 隐私：不拷贝原始 target / 参数 / 伪造 evidence id
// ---------------------------------------------------------------------------

#[test]
fn ledger_never_stores_raw_targets_or_parameters() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let ledger = UnresolvedEffectLedger::persistent(root);
    let secret = "sk-live-DO-NOT-PERSIST-0123456789";
    let command =
        format!("bash -c 'curl -H \"Authorization: Bearer {secret}\" https://example.test/upload'");
    let key = record_key("run-privacy", 1);
    let observation = ledger
        .record_unknown(
            &key,
            UnknownEffectInput::new("bash", UnknownFactReason::Unclassified)
                .with_target(command.as_str()),
        )
        .unwrap();

    let raw = raw_ledger(root);
    assert!(!raw.contains(secret), "raw secret must never be persisted");
    assert!(!raw.contains("Authorization"));
    assert!(!raw.contains("curl"));
    assert!(
        raw.contains(&target_digest(&command)),
        "the target digest must still allow journal correlation"
    );
    assert_eq!(
        observation.record.facts[0].target_sha256,
        target_digest(&command)
    );
    assert_eq!(
        observation.record.facts[0].evidence_id, None,
        "no evidence id was supplied, so none may be invented"
    );
    assert!(!raw.contains("evidence_id"));

    // 超长 target 被明确拒绝，而不是悄悄截断后伪造摘要。
    let huge = "x".repeat(unresolved_effect::MAX_TARGET_BYTES + 1);
    assert!(matches!(
        ledger.record_unknown(
            &record_key("run-privacy", 2),
            UnknownEffectInput::new("write", UnknownFactReason::Unclassified).with_target(huge)
        ),
        Err(UnresolvedEffectError::InvalidKey(_))
    ));
}

#[test]
fn evidence_id_is_preserved_exactly_or_absent() {
    // 没有真实 evidence id 时字段完全缺席。
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let ledger = UnresolvedEffectLedger::persistent(root);
    ledger
        .record_unknown(&record_key("run-id", 1), unknown_input("write", None))
        .unwrap();
    let raw = raw_ledger(root);
    assert!(!raw.contains("evidence_id"));

    // 有真实 evidence id 时逐字往返。
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let ledger = UnresolvedEffectLedger::persistent(root);
    let evidence_id = uuid::Uuid::from_u128(0x1234_5678_9abc_def0_1122_3344_5566_7788);
    ledger
        .record_unknown(
            &record_key("run-id", 1),
            unknown_input("write", None).with_evidence_id(evidence_id),
        )
        .unwrap();
    let raw = raw_ledger(root);
    assert!(raw.contains(&evidence_id.to_string()));
    let stored = ledger.lookup(&record_key("run-id", 1)).unwrap().unwrap();
    assert_eq!(stored.record.facts[0].evidence_id, Some(evidence_id));
}

#[test]
fn duplicate_evidence_id_is_idempotent_but_unidentified_observations_are_not() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let ledger = UnresolvedEffectLedger::persistent(root);
    let key = record_key("run-dup", 1);
    let evidence_id = uuid::Uuid::new_v4();

    let first = ledger
        .record_unknown(
            &key,
            unknown_input("write", None).with_evidence_id(evidence_id),
        )
        .unwrap();
    let before = fs::read(ledger_file(root)).unwrap();
    let second = ledger
        .record_unknown(
            &key,
            unknown_input("write", None).with_evidence_id(evidence_id),
        )
        .unwrap();
    assert_eq!(second.snapshot_key, first.snapshot_key);
    assert_eq!(second.record.facts.len(), 1);
    assert_eq!(
        fs::read(ledger_file(root)).unwrap(),
        before,
        "a repeated evidence id must not rewrite the ledger"
    );

    // 没有 evidence id 的两次观测不能被当作重复。
    let one = ledger
        .record_unknown(&key, unknown_input("write", None))
        .unwrap();
    let two = ledger
        .record_unknown(&key, unknown_input("write", None))
        .unwrap();
    assert_ne!(one.snapshot_key, two.snapshot_key);
    assert_eq!(two.record.facts.len(), 3);
}

// ---------------------------------------------------------------------------
// fail-closed：损坏、版本、超限、符号链接、坏 owner、满员
// ---------------------------------------------------------------------------

fn assert_all_operations_fail_closed(root: &Path) {
    let ledger = UnresolvedEffectLedger::persistent(root);
    let key = record_key("run-corrupt", 1);
    assert!(ledger.snapshot().is_err());
    assert!(ledger.lookup(&key).is_err());
    assert!(ledger.recover_outstanding().is_err());
    assert!(ledger.record_dispatched(&key).is_err());
    assert!(
        ledger
            .record_unknown(&key, unknown_input("write", None))
            .is_err()
    );
    assert!(ledger.resolve_confirmed(&key).is_err());
    let snapshot_key = "0".repeat(64).parse::<UnresolvedSnapshotKey>().unwrap();
    assert!(ledger.acknowledge(&key, &snapshot_key, None).is_err());
    // 只读账本同样不能把损坏伪装成空账本。
    let read_only = UnresolvedEffectLedger::read_only(root);
    assert!(read_only.snapshot().is_err());
    assert!(read_only.recover_outstanding().is_err());
}

#[test]
fn corrupt_unknown_version_and_oversized_ledgers_fail_closed() {
    let duplicated = raw_ledger_value(json!([
        raw_record("dup", 1, "outstanding", None),
        raw_record("dup", 1, "outstanding", None),
    ]));
    let zero_index = raw_ledger_value(json!([raw_record("x", 0, "outstanding", None)]));
    let missing_ack = raw_ledger_value(json!([raw_record("x", 1, "acknowledged", None)]));
    let sparse_facts = raw_ledger_value(json!([{
        "scope": "x", "call_index": 1, "state": "unknown", "revision": 1,
        "recorded_at": 0, "updated": 0,
        "facts": [{
            "fact_seq": 2, "reason": "unclassified", "operation": "write", "observed": 0,
        }],
    }]));
    let bad_payloads = vec![
        "{ not json".to_owned(),
        "{\"version\":2,\"updated\":0,\"entries\":[]}".to_owned(),
        "{\"updated\":0,\"entries\":[]}".to_owned(),
        "{\"version\":1,\"updated\":0,\"entries\":[{\"scope\":\"a\",\"call_index\":1,\"state\":\"bogus\",\"revision\":1,\"recorded_at\":0,\"updated\":0}]}"
            .to_owned(),
        duplicated,
        zero_index,
        missing_ack,
        sparse_facts,
    ];
    for payload in &bad_payloads {
        let dir = tempfile::tempdir().unwrap();
        write_raw_ledger(dir.path(), payload);
        assert_all_operations_fail_closed(dir.path());
    }

    // 超限文件在解析之前就被拒绝。
    let dir = tempfile::tempdir().unwrap();
    write_raw_ledger(dir.path(), &"x".repeat(MAX_LEDGER_FILE_SIZE + 1));
    assert_all_operations_fail_closed(dir.path());

    // 条目数超限同样 fail-closed，且不静默裁剪。
    let dir = tempfile::tempdir().unwrap();
    let entries: Vec<Value> = (0..(MAX_LEDGER_ENTRIES + 1))
        .map(|index| raw_record(&format!("run-{index}"), 1, "outstanding", None))
        .collect();
    write_raw_ledger(dir.path(), &raw_ledger_value(json!(entries)));
    assert_all_operations_fail_closed(dir.path());
}

#[test]
fn symlinked_ledger_and_malformed_owner_fail_closed() {
    #[cfg(unix)]
    {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let elsewhere = root.join("elsewhere.json");
        fs::write(&elsewhere, raw_ledger_value(json!([]))).unwrap();
        let ledger_dir = root.join(LEDGER_DIR);
        fs::create_dir_all(&ledger_dir).unwrap();
        std::os::unix::fs::symlink(&elsewhere, ledger_dir.join(LEDGER_FILE)).unwrap();
        assert_all_operations_fail_closed(root);
        assert!(elsewhere.exists(), "the symlink target must stay untouched");

        // 叶子目录本身是符号链接也必须拒绝。
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let real = root.join("real-ledger");
        fs::create_dir_all(&real).unwrap();
        fs::create_dir_all(root.join(".mio")).unwrap();
        std::os::unix::fs::symlink(&real, root.join(LEDGER_DIR)).unwrap();
        let ledger = UnresolvedEffectLedger::persistent(root);
        assert!(
            ledger
                .record_dispatched(&record_key("run-symlink", 1))
                .is_err()
        );
        assert_eq!(
            fs::read_dir(&real).unwrap().count(),
            0,
            "the symlinked target must not receive writes"
        );
    }

    // 坏 owner 元数据不能伪装成旧记录（无 owner）。
    let digest = "a".repeat(unresolved_effect::SHA256_HEX_CHARS);
    let owner = json!({
        "pid": 1,
        "start_time_ticks": 1,
        "host_id": digest.clone(),
        "boot_id": digest.clone(),
        "pid_namespace": digest,
        "captured": 0,
    });
    let case = |mutate: fn(&mut Value)| {
        let mut owner = owner.clone();
        mutate(&mut owner);
        owner
    };
    let bad_cases = vec![
        case(|owner| owner["pid"] = json!(0)),
        case(|owner| owner["start_time_ticks"] = json!(0)),
        case(|owner| {
            owner["host_id"] = json!("A".repeat(unresolved_effect::SHA256_HEX_CHARS));
        }),
        case(|owner| {
            owner.as_object_mut().unwrap().remove("boot_id");
        }),
        case(|owner| owner["pid_namespace"] = json!("abc")),
    ];

    for owner in bad_cases {
        let dir = tempfile::tempdir().unwrap();
        write_raw_ledger(
            dir.path(),
            &raw_ledger_value(json!([raw_record(
                "bad-owner",
                1,
                "outstanding",
                Some(owner)
            )])),
        );
        assert_all_operations_fail_closed(dir.path());
    }
}

#[test]
fn full_ledger_refuses_new_records_without_evicting() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let entries: Vec<Value> = (0..MAX_LEDGER_ENTRIES)
        .map(|index| raw_record(&format!("run-{index}"), 1, "outstanding", None))
        .collect();
    write_raw_ledger(root, &raw_ledger_value(json!(entries)));

    let ledger = UnresolvedEffectLedger::persistent(root);
    assert_eq!(ledger.snapshot().unwrap().len(), MAX_LEDGER_ENTRIES);
    assert!(matches!(
        ledger.record_dispatched(&record_key("run-overflow", 1)),
        Err(UnresolvedEffectError::Ledger(_))
    ));
    assert!(matches!(
        ledger.record_unknown(&record_key("run-overflow", 2), unknown_input("write", None)),
        Err(UnresolvedEffectError::Ledger(_))
    ));
    // 既有条目既不被驱逐也仍然可用。
    assert!(ledger.resolve_confirmed(&record_key("run-0", 1)).unwrap());
    assert_eq!(ledger.snapshot().unwrap().len(), MAX_LEDGER_ENTRIES);
    assert_eq!(
        ledger
            .lookup(&record_key("run-0", 1))
            .unwrap()
            .unwrap()
            .record
            .state,
        UnresolvedEffectState::Resolved
    );
}

// ---------------------------------------------------------------------------
// 磁盘格式：私有权限、原子替换、无遗留临时文件
// ---------------------------------------------------------------------------

#[test]
fn ledger_files_are_private_and_published_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let ledger = UnresolvedEffectLedger::persistent(root);
    ledger
        .record_dispatched(&record_key("run-perms", 1))
        .unwrap();
    ledger
        .record_unknown(&record_key("run-perms", 1), unknown_input("write", None))
        .unwrap();

    let ledger_dir = root.join(LEDGER_DIR);
    let data = ledger_dir.join(LEDGER_FILE);
    let lock = ledger_dir.join(LEDGER_LOCK_FILE);
    assert!(data.is_file());
    assert!(lock.is_file());
    #[cfg(unix)]
    {
        assert_eq!(permission_mode(&ledger_dir), 0o700);
        assert_eq!(permission_mode(&data), 0o600);
        assert_eq!(permission_mode(&lock), 0o600);
        let leftovers: Vec<String> = fs::read_dir(&ledger_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != LEDGER_FILE && name != LEDGER_LOCK_FILE)
            .collect();
        assert!(
            leftovers.is_empty(),
            "atomic publication must not leave temporary files behind: {leftovers:?}"
        );
    }

    let parsed: Value = serde_json::from_str(&raw_ledger(root)).unwrap();
    assert_eq!(parsed["version"], 1);
    assert_eq!(parsed["entries"][0]["scope"], "run-perms");
    assert_eq!(parsed["entries"][0]["state"], "unknown");
}

// ---------------------------------------------------------------------------
// 真实进程：并发派发、并发确认、陈旧快照
// ---------------------------------------------------------------------------

#[test]
fn concurrent_processes_cannot_dispatch_the_same_key_twice() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let workers = 6usize;

    let mut children = Vec::new();
    for worker in 0..workers {
        let result = root.join(format!("claim-{worker}.txt"));
        let mut command = child_command(&root, "dispatch_shared");
        command
            .env(ENV_SCOPE, "run-race")
            .env(ENV_INDEX, "42")
            .env(ENV_RESULT, &result);
        children.push(ChildGuard::spawn(&mut command));
    }
    for child in children.iter_mut() {
        let status = child.0.as_mut().unwrap().wait().unwrap();
        assert!(status.success(), "dispatch child failed: {status}");
    }
    children.clear();

    let mut acquired = 0;
    for worker in 0..workers {
        let label = fs::read_to_string(root.join(format!("claim-{worker}.txt"))).unwrap();
        if label == "ACQUIRED" {
            acquired += 1;
        } else {
            assert_eq!(label, "EXISTING");
        }
    }
    assert_eq!(acquired, 1, "exactly one process may acquire the dispatch");
    let snapshot = UnresolvedEffectLedger::persistent(&root)
        .snapshot()
        .unwrap();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(
        snapshot.entries[0].record.state,
        UnresolvedEffectState::Outstanding
    );
    assert_eq!(snapshot.entries[0].record.scope, "run-race");
    assert_eq!(snapshot.entries[0].record.call_index, 42);
}

#[test]
fn stale_snapshot_key_is_rejected_after_a_new_process_adds_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let ledger = UnresolvedEffectLedger::persistent(&root);
    let key = record_key("run-cross", 1);
    ledger
        .record_unknown(&key, unknown_input("write", None))
        .unwrap();
    let stale = ledger.lookup(&key).unwrap().unwrap().snapshot_key;
    let before = raw_ledger(&root);

    // 另一个真实进程在同一主键上追加一条全新证据。
    let result = root.join("cross-result.txt");
    let mut command = child_command(&root, "record_unknown");
    command
        .env(ENV_SCOPE, "run-cross")
        .env(ENV_INDEX, "1")
        .env(ENV_OPERATION, "mcp__probe__write")
        .env(ENV_RESULT, &result);
    let status = command.status().unwrap();
    assert!(status.success(), "record_unknown child failed: {status}");
    assert_eq!(fs::read_to_string(&result).unwrap(), "OK");
    assert_ne!(raw_ledger(&root), before);

    // 父进程手里的旧快照键已经陈旧：绝不能 ack 掉新证据。
    assert_eq!(
        ledger.acknowledge(&key, &stale, None),
        Err(UnresolvedEffectError::StaleConfirmation)
    );
    let fresh = ledger.lookup(&key).unwrap().unwrap();
    assert_ne!(fresh.snapshot_key, stale);
    assert_eq!(fresh.record.facts.len(), 2);
    let acked = ledger
        .acknowledge(&key, &fresh.snapshot_key, Some("reviewed both events"))
        .unwrap();
    assert_eq!(acked.record.state, UnresolvedEffectState::Acknowledged);
    assert_eq!(acked.record.facts.len(), 2);
}

#[test]
fn concurrent_in_process_writers_share_one_ledger_and_one_snapshot_key() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let key = record_key("run-threads", 1);
    UnresolvedEffectLedger::persistent(&root)
        .record_dispatched(&key)
        .unwrap();
    let stale = UnresolvedEffectLedger::persistent(&root)
        .lookup(&key)
        .unwrap()
        .unwrap()
        .snapshot_key;

    let writers = 8usize;
    let mut handles = Vec::new();
    for _ in 0..writers {
        let root = root.clone();
        let key = key.clone();
        handles.push(std::thread::spawn(move || {
            // 每个线程独立的账本实例：跨实例串行只靠 flock。
            UnresolvedEffectLedger::persistent(&root)
                .record_unknown(
                    &key,
                    unknown_input("write", None).with_evidence_id(uuid::Uuid::new_v4()),
                )
                .unwrap();
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }

    let observation = UnresolvedEffectLedger::persistent(&root)
        .lookup(&key)
        .unwrap()
        .unwrap();
    assert_eq!(observation.record.facts.len(), writers);
    assert_eq!(
        UnresolvedEffectLedger::persistent(&root)
            .snapshot()
            .unwrap()
            .len(),
        1,
        "concurrent writers must not duplicate the record"
    );
    assert_eq!(
        UnresolvedEffectLedger::persistent(&root).acknowledge(&key, &stale, None),
        Err(UnresolvedEffectError::StaleConfirmation)
    );
}

// ---------------------------------------------------------------------------
// 真实进程：SIGKILL 恢复
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[test]
fn killed_owner_is_recovered_as_unknown_while_a_live_owner_keeps_outstanding() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();

    // 本测试进程自己持有一条 Outstanding：恢复时它必须被正向判活。
    let live = record_key("run-live", 1);
    UnresolvedEffectLedger::persistent(&root)
        .record_dispatched(&live)
        .unwrap();

    // 真实子进程持久化另一条 Outstanding，然后被 SIGKILL。
    let started = root.join("child-started");
    let mut command = child_command(&root, "dispatch_hold");
    command
        .env(ENV_SCOPE, "run-dead")
        .env(ENV_INDEX, "9")
        .env(ENV_STARTED, &started);
    let mut child = ChildGuard::spawn(&mut command);
    wait_for_file(&started);

    let child_record = UnresolvedEffectLedger::persistent(&root)
        .lookup(&record_key("run-dead", 9))
        .unwrap()
        .unwrap();
    assert_eq!(
        child_record.record.state,
        UnresolvedEffectState::Outstanding
    );
    assert_eq!(
        child_record.liveness,
        Liveness::Live,
        "a running dispatch owner must be observed live"
    );
    assert!(child_record.record.owner.is_some());

    child.kill_and_wait();
    wait_until("the killed owner to stop being live", || {
        UnresolvedEffectLedger::persistent(&root)
            .lookup(&record_key("run-dead", 9))
            .unwrap()
            .unwrap()
            .liveness
            != Liveness::Live
    });

    let report = UnresolvedEffectLedger::persistent(&root)
        .recover_outstanding()
        .unwrap();
    assert!(report.persisted, "recovery upgrades must be durable");

    let dead = report
        .entries
        .iter()
        .find(|entry| entry.key.scope == "run-dead")
        .unwrap();
    assert_eq!(dead.outcome, RecoveryOutcome::UpgradedToUnknown);
    assert_eq!(dead.state, UnresolvedEffectState::Unknown);
    assert_ne!(dead.liveness, Liveness::Live);

    let kept = report
        .entries
        .iter()
        .find(|entry| entry.key.scope == "run-live")
        .unwrap();
    assert_eq!(kept.outcome, RecoveryOutcome::KeptOutstanding);
    assert_eq!(kept.state, UnresolvedEffectState::Outstanding);
    assert_eq!(
        kept.liveness,
        Liveness::Live,
        "a live owner must never be misjudged as dead"
    );

    // 升级落盘、原始 unknown 事实为空（不伪造证据），且必须由宿主 ack 才能离开。
    let reloaded = UnresolvedEffectLedger::persistent(&root)
        .lookup(&record_key("run-dead", 9))
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.record.state, UnresolvedEffectState::Unknown);
    assert!(reloaded.record.recovery.is_some());
    assert!(reloaded.record.facts.is_empty());
    assert!(reloaded.record.acknowledgement.is_none());
    assert!(reloaded.record.is_pending());
    assert_eq!(
        reloaded.record.recovery.as_ref().unwrap().liveness,
        dead.liveness
    );
    assert!(
        UnresolvedEffectLedger::persistent(&root)
            .snapshot()
            .unwrap()
            .is_blocked()
    );

    // 恢复是保守升级，不是自动解决：再跑一次不会改变状态。
    let second = UnresolvedEffectLedger::persistent(&root)
        .recover_outstanding()
        .unwrap();
    assert!(!second.has_upgrades());
    assert_eq!(second.kept_outstanding(), 1);
}

#[cfg(target_os = "linux")]
#[test]
fn owner_liveness_decisions_are_conservative() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let real = capture_owner_for_pid(std::process::id()).expect("self identity");
    let mut reused = real.clone();
    reused.start_time_ticks = reused.start_time_ticks.wrapping_add(1);
    let mut foreign_boot = real.clone();
    foreign_boot.boot_id = "f".repeat(unresolved_effect::SHA256_HEX_CHARS);
    let mut foreign_host = real.clone();
    foreign_host.host_id = "f".repeat(unresolved_effect::SHA256_HEX_CHARS);
    let mut foreign_namespace = real.clone();
    foreign_namespace.pid_namespace = "f".repeat(unresolved_effect::SHA256_HEX_CHARS);

    let entries = json!([
        raw_record(
            "live",
            1,
            "outstanding",
            Some(serde_json::to_value(&real).unwrap())
        ),
        raw_record(
            "pid-reused",
            1,
            "outstanding",
            Some(serde_json::to_value(&reused).unwrap())
        ),
        raw_record(
            "new-boot",
            1,
            "outstanding",
            Some(serde_json::to_value(&foreign_boot).unwrap())
        ),
        raw_record(
            "foreign-host",
            1,
            "outstanding",
            Some(serde_json::to_value(&foreign_host).unwrap())
        ),
        raw_record(
            "foreign-ns",
            1,
            "outstanding",
            Some(serde_json::to_value(&foreign_namespace).unwrap())
        ),
        raw_record("no-owner", 1, "outstanding", None),
    ]);
    write_raw_ledger(root, &raw_ledger_value(entries));

    let report = UnresolvedEffectLedger::persistent(root)
        .recover_outstanding()
        .unwrap();
    let outcome = |scope: &str| {
        report
            .entries
            .iter()
            .find(|entry| entry.key.scope == scope)
            .unwrap()
            .clone()
    };
    assert_eq!(outcome("live").outcome, RecoveryOutcome::KeptOutstanding);
    assert_eq!(outcome("live").liveness, Liveness::Live);
    for scope in ["pid-reused", "new-boot"] {
        assert_eq!(
            outcome(scope).liveness,
            Liveness::Stale,
            "{scope} has positive evidence the owner is gone"
        );
        assert_eq!(outcome(scope).outcome, RecoveryOutcome::UpgradedToUnknown);
    }
    for scope in ["foreign-host", "foreign-ns", "no-owner"] {
        assert_eq!(
            outcome(scope).liveness,
            Liveness::Unknown,
            "{scope} has no positive liveness evidence"
        );
        assert_eq!(
            outcome(scope).outcome,
            RecoveryOutcome::UpgradedToUnknown,
            "uncertain owner evidence must escalate to unknown, never to 'never happened'"
        );
    }
    // 只有正向存活的 owner 保留 Outstanding。
    assert_eq!(report.kept_outstanding(), 1);
    assert_eq!(report.upgraded().count(), 5);
    assert_eq!(
        UnresolvedEffectLedger::persistent(root)
            .snapshot()
            .unwrap()
            .unknown_count(),
        5
    );
}

// ---------------------------------------------------------------------------
// 容量/字节压力：只裁剪 Resolved，绝不驱逐未决
// ---------------------------------------------------------------------------

#[test]
fn capacity_pressure_trims_only_resolved_records_and_counts_them() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let entries: Vec<Value> = (0..MAX_LEDGER_ENTRIES)
        .map(|index| raw_record(&format!("run-{index:05}"), 1, "resolved", None))
        .collect();
    write_raw_ledger(root, &raw_ledger_value(json!(entries)));

    let ledger = UnresolvedEffectLedger::persistent(root);
    let before = ledger.snapshot().unwrap();
    assert_eq!(before.len(), MAX_LEDGER_ENTRIES);
    assert_eq!(before.resolved_trimmed(), 0);

    // One ordinary tool call must not stall the whole workspace: the terminal
    // resolved records are trimmed and counted instead of refusing forever.
    assert_eq!(
        ledger.record_dispatched(&record_key("run-new", 1)).unwrap(),
        DispatchClaim::Acquired
    );
    let after = ledger.snapshot().unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after.entries[0].record.scope, "run-new");
    assert_eq!(
        after.resolved_trimmed(),
        MAX_LEDGER_ENTRIES as u64,
        "every trimmed record must be counted"
    );

    // Reopening preserves the count and the surviving record.
    let reopened = UnresolvedEffectLedger::persistent(root).snapshot().unwrap();
    assert_eq!(reopened.len(), 1);
    assert_eq!(reopened.resolved_trimmed(), MAX_LEDGER_ENTRIES as u64);
}

#[test]
fn capacity_pressure_never_evicts_pending_records() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let entries: Vec<Value> = (0..MAX_LEDGER_ENTRIES)
        .map(|index| {
            let mut record = raw_record(&format!("run-{index:05}"), 1, "unknown", None);
            record["facts"] = json!([{
                "fact_seq": 1,
                "reason": "unclassified",
                "operation": "write",
                "observed": 0
            }]);
            record
        })
        .collect();
    write_raw_ledger(root, &raw_ledger_value(json!(entries)));

    let ledger = UnresolvedEffectLedger::persistent(root);
    assert!(matches!(
        ledger.record_dispatched(&record_key("run-overflow", 1)),
        Err(UnresolvedEffectError::Ledger(_))
    ));
    let snapshot = ledger.snapshot().unwrap();
    assert_eq!(snapshot.len(), MAX_LEDGER_ENTRIES);
    assert_eq!(snapshot.unknown_count(), MAX_LEDGER_ENTRIES);
    assert_eq!(
        snapshot.resolved_trimmed(),
        0,
        "nothing pending may be trimmed to make room"
    );
    // The full message must never instruct removing unresolved evidence.
    let message = ledger
        .record_dispatched(&record_key("run-overflow", 2))
        .unwrap_err()
        .to_string();
    assert!(!message.contains("remove"));
    assert!(!message.contains("to start a new ledger"));
    assert!(message.contains("never delete"), "{message}");
    assert!(message.contains("unresolved"), "{message}");
}

#[cfg(target_os = "linux")]
#[test]
fn byte_pressure_trims_resolved_records_without_evicting_pending() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let owner = capture_owner_for_pid(std::process::id()).expect("self owner");
    let owner_value = serde_json::to_value(&owner).unwrap();
    let long_scope = "s".repeat(unresolved_effect::MAX_SCOPE_CHARS);

    // Build a compact ledger just below the byte cap; pretty-printing plus one
    // more record then exceeds it, so persist must trim the terminal records.
    let mut entries: Vec<Value> = Vec::new();
    loop {
        let mut candidate = entries.clone();
        candidate.push(raw_record(
            &long_scope,
            candidate.len() as u64 + 1,
            "resolved",
            Some(owner_value.clone()),
        ));
        let payload = raw_ledger_value(json!(candidate));
        if payload.len() > MAX_LEDGER_FILE_SIZE - 4096 {
            break;
        }
        entries = candidate;
    }
    let mut pending = raw_record(
        &long_scope,
        entries.len() as u64 + 1,
        "unknown",
        Some(owner_value),
    );
    pending["facts"] = json!([{
        "fact_seq": 1,
        "reason": "unclassified",
        "operation": "write",
        "observed": 0
    }]);
    entries.push(pending);
    write_raw_ledger(root, &raw_ledger_value(json!(entries)));
    assert!(fs::metadata(ledger_file(root)).unwrap().len() <= MAX_LEDGER_FILE_SIZE as u64);

    let ledger = UnresolvedEffectLedger::persistent(root);
    for attempt in 1..64u64 {
        ledger
            .record_dispatched(&record_key(&format!("byte-new-{attempt}"), 1))
            .unwrap();
        if ledger.snapshot().unwrap().resolved_trimmed() > 0 {
            break;
        }
    }
    let snapshot = ledger.snapshot().unwrap();
    assert!(
        snapshot.resolved_trimmed() > 0,
        "byte pressure must trim terminal resolved records"
    );
    assert!(
        snapshot
            .entries
            .iter()
            .any(|entry| entry.record.state == UnresolvedEffectState::Unknown),
        "the pending unknown must survive byte pressure"
    );
    assert!(fs::metadata(ledger_file(root)).unwrap().len() <= MAX_LEDGER_FILE_SIZE as u64);
}

#[test]
fn recovery_report_exposes_the_exact_snapshot_digest_per_record() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let ledger = UnresolvedEffectLedger::persistent(root);
    let key = record_key("run-recovered", 3);
    ledger
        .record_unknown(&key, unknown_input("mcp__probe__write", None))
        .unwrap();
    let live = ledger.lookup(&key).unwrap().unwrap();

    let report = ledger.recover_outstanding().unwrap();
    let entry = report
        .entries
        .iter()
        .find(|entry| entry.key == key)
        .unwrap();
    assert_eq!(entry.snapshot_key, live.snapshot_key);
    assert_eq!(entry.last_operation.as_deref(), Some("mcp__probe__write"));
}

#[cfg(target_os = "linux")]
#[test]
fn a_second_process_blocks_a_write_after_this_process_seeded_empty() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // This process seeds from a workspace ledger that holds no unknown yet.
    let seeded = UnresolvedEffectLedger::read_only(root)
        .recover_outstanding()
        .unwrap();
    assert!(!seeded.present, "the workspace starts with no ledger");

    // A real whole-workspace scan with no journals commits coverage before any
    // write is admitted; a durable unknown added afterwards must still block.
    initialize_coverage(&UnresolvedEffectLedger::persistent(root));

    // A second real process records a fresh durable Unknown for its own scope.
    let result = root.join("foreign-unknown-result");
    let mut command = child_command(root, "record_unknown");
    command
        .env(ENV_SCOPE, "foreign-run")
        .env(ENV_INDEX, "1")
        .env(ENV_OPERATION, "mcp__probe__write")
        .env(ENV_RESULT, &result);
    let mut child = ChildGuard::spawn(&mut command);
    child.0.as_mut().unwrap().wait().unwrap();
    assert_eq!(fs::read_to_string(&result).unwrap(), "OK");

    // The stale in-memory seed must not let a write claim through: the durable
    // re-check inside the same transaction refuses it and claims nothing.
    let admitted = UnresolvedEffectLedger::persistent(root)
        .claim_dispatch(&record_key("this-run", 1), true)
        .unwrap();
    match admitted {
        DispatchAdmission::BlockedByUnknown { pending } => assert!(pending >= 1),
        other => panic!("a durable unknown added by another process must block, got {other:?}"),
    }
    assert!(
        UnresolvedEffectLedger::read_only(root)
            .lookup(&record_key("this-run", 1))
            .unwrap()
            .is_none(),
        "a refused write must not be claimed"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_second_process_makes_this_process_snapshot_stale() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let key = record_key("shared-run", 1);
    let ledger = UnresolvedEffectLedger::persistent(root);
    let first = ledger
        .record_unknown(&key, unknown_input("write", None))
        .unwrap();

    // A second real process appends a new fact (with a new evidence id) to the
    // exact same key, after this process took its snapshot.
    let result = root.join("foreign-append-result");
    let mut command = child_command(root, "record_unknown");
    command
        .env(ENV_SCOPE, "shared-run")
        .env(ENV_INDEX, "1")
        .env(ENV_OPERATION, "write")
        .env(ENV_RESULT, &result);
    let mut child = ChildGuard::spawn(&mut command);
    child.0.as_mut().unwrap().wait().unwrap();
    assert_eq!(fs::read_to_string(&result).unwrap(), "OK");

    // The earlier snapshot digest is now stale and must be refused, never used
    // to acknowledge evidence this process never saw.
    let stale = ledger
        .acknowledge(&key, &first.snapshot_key, None)
        .unwrap_err();
    assert!(
        matches!(stale, UnresolvedEffectError::StaleConfirmation),
        "expected a stale confirmation, got {stale:?}"
    );
    assert_eq!(
        ledger.lookup(&key).unwrap().unwrap().record.state,
        UnresolvedEffectState::Unknown,
        "a refused stale confirmation must leave the unknown pending"
    );
}

// ---------------------------------------------------------------------------
// 覆盖 epoch：初始化必须先证明完整，丢失不再可 ack
// ---------------------------------------------------------------------------

#[test]
fn a_ledger_file_alone_is_not_proof_of_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let ledger = UnresolvedEffectLedger::persistent(root);
    // A record write creates the file without ever proving workspace coverage.
    ledger
        .record_dispatched(&record_key("run-partial", 1))
        .unwrap();
    assert!(ledger_file(root).is_file(), "the ledger file now exists");
    assert_eq!(ledger.coverage().unwrap(), Coverage::default());
    assert!(matches!(
        ledger
            .claim_dispatch(&record_key("run-new", 1), true)
            .unwrap(),
        DispatchAdmission::BlockedByCoverage {
            initialized: false,
            incomplete: false,
        }
    ));
    // A read-only admission stays available even before coverage.
    assert_eq!(
        ledger
            .claim_dispatch(&record_key("run-read", 1), false)
            .unwrap(),
        DispatchAdmission::Acquired
    );
}

#[test]
fn a_complete_initial_coverage_commit_migrates_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let ledger = UnresolvedEffectLedger::persistent(root);
    let legacy = record_key("journal-legacy:old:run", 7);
    let candidates = vec![(legacy.clone(), unknown_input("mcp__legacy__write", None))];

    assert_eq!(
        ledger.commit_initial_coverage(&candidates, true).unwrap(),
        CoverageCommit::Initialized
    );
    assert_eq!(
        ledger.coverage().unwrap(),
        Coverage {
            initialized: true,
            incomplete: false,
        }
    );
    assert_eq!(
        ledger.lookup(&legacy).unwrap().unwrap().record.state,
        UnresolvedEffectState::Unknown
    );
    // A second commit never re-imports or clears anything.
    assert_eq!(
        ledger.commit_initial_coverage(&candidates, true).unwrap(),
        CoverageCommit::AlreadyInitialized
    );
    assert_eq!(
        ledger.lookup(&legacy).unwrap().unwrap().record.facts.len(),
        1
    );
    // After the exact ack a new write can be admitted.
    let shown = ledger.lookup(&legacy).unwrap().unwrap();
    ledger
        .acknowledge(&legacy, &shown.snapshot_key, None)
        .unwrap();
    assert_eq!(
        ledger
            .claim_dispatch(&record_key("run-after", 1), true)
            .unwrap(),
        DispatchAdmission::Acquired
    );
}

#[test]
fn incomplete_coverage_latches_and_no_ack_can_clear_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let ledger = UnresolvedEffectLedger::persistent(root);
    assert_eq!(
        ledger.commit_initial_coverage(&[], false).unwrap(),
        CoverageCommit::MarkedIncomplete
    );
    assert!(ledger.coverage().unwrap().incomplete);
    assert!(matches!(
        ledger
            .claim_dispatch(&record_key("run-blocked", 1), true)
            .unwrap(),
        DispatchAdmission::BlockedByCoverage {
            initialized: false,
            incomplete: true,
        }
    ));
    // The latch is monotonic: a later "complete" commit cannot un-latch it, and
    // `mark_coverage_incomplete` is idempotent.
    assert_eq!(
        ledger.commit_initial_coverage(&[], true).unwrap(),
        CoverageCommit::AlreadyIncomplete
    );
    ledger.mark_coverage_incomplete().unwrap();
    assert!(ledger.coverage().unwrap().incomplete);
    // Acknowledging an unrelated record (the only ack path) leaves it set.
    let key = record_key("run-other", 1);
    let observation = ledger
        .record_unknown(&key, unknown_input("write", None))
        .unwrap();
    ledger
        .acknowledge(&key, &observation.snapshot_key, None)
        .unwrap();
    assert!(ledger.coverage().unwrap().incomplete);
    assert!(matches!(
        ledger
            .claim_dispatch(&record_key("run-still-blocked", 1), true)
            .unwrap(),
        DispatchAdmission::BlockedByCoverage { .. }
    ));
}

#[test]
fn coverage_fields_are_serde_default_compatible_on_an_old_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_raw_ledger(root, &raw_ledger_value(json!([])));
    let ledger = UnresolvedEffectLedger::persistent(root);
    // An old file (no coverage fields) is valid and simply fail-closed.
    assert_eq!(ledger.coverage().unwrap(), Coverage::default());
    assert!(ledger.snapshot().unwrap().is_empty());
    assert_eq!(
        ledger.commit_initial_coverage(&[], true).unwrap(),
        CoverageCommit::Initialized
    );
    // Reload from disk: the marker is durable, not just in memory.
    let reloaded = UnresolvedEffectLedger::persistent(root);
    assert_eq!(
        reloaded.coverage().unwrap(),
        Coverage {
            initialized: true,
            incomplete: false,
        }
    );
    let raw: Value = serde_json::from_str(&raw_ledger(root)).unwrap();
    assert_eq!(raw["coverage_initialized"], true);
}

#[cfg(target_os = "linux")]
#[test]
fn concurrent_processes_commit_the_coverage_epoch_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let workers = 6usize;
    let mut children = Vec::new();
    for worker in 0..workers {
        let result = root.join(format!("coverage-{worker}.txt"));
        let mut command = child_command(&root, "commit_coverage");
        command.env(ENV_RESULT, &result);
        children.push(ChildGuard::spawn(&mut command));
    }
    for child in children.iter_mut() {
        let status = child.0.as_mut().unwrap().wait().unwrap();
        assert!(status.success(), "coverage child failed: {status}");
    }
    children.clear();

    let mut initialized = 0;
    for worker in 0..workers {
        let label = fs::read_to_string(root.join(format!("coverage-{worker}.txt"))).unwrap();
        match label.as_str() {
            "INITIALIZED" => initialized += 1,
            "ALREADY_INITIALIZED" => {}
            other => panic!("unexpected coverage commit result: {other}"),
        }
    }
    assert_eq!(initialized, 1, "exactly one process commits the epoch");
    assert_eq!(
        UnresolvedEffectLedger::persistent(&root)
            .coverage()
            .unwrap(),
        Coverage {
            initialized: true,
            incomplete: false,
        }
    );
}

// ---------------------------------------------------------------------------
// Runtime evidence-id resolution (live MCP/plugin markers -> exact durable key)
// ---------------------------------------------------------------------------

#[test]
fn runtime_evidence_id_resolves_to_exactly_one_durable_record() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let ledger = UnresolvedEffectLedger::persistent(root);
    let evidence = uuid::Uuid::new_v4();
    let key = record_key("run-evidence", 1);
    let observation = ledger
        .record_unknown(
            &key,
            unknown_input("mcp__srv__record", None).with_evidence_id(evidence),
        )
        .unwrap();
    match ledger.resolve_evidence(evidence).unwrap() {
        EvidenceResolution::Unique(record) => {
            assert_eq!(record.key, key);
            assert_eq!(record.snapshot_key, observation.snapshot_key);
            assert_eq!(record.state, UnresolvedEffectState::Unknown);
            assert_eq!(record.facts_dropped, 0);
        }
        other => panic!("expected exactly one record, got {other:?}"),
    }
    assert_eq!(
        ledger.resolve_evidence(uuid::Uuid::new_v4()).unwrap(),
        EvidenceResolution::Missing
    );
    // The same real id persisted on two records is ambiguous, never guessed.
    let second = record_key("run-evidence", 2);
    ledger
        .record_unknown(
            &second,
            unknown_input("mcp__srv__record", None).with_evidence_id(evidence),
        )
        .unwrap();
    assert_eq!(
        ledger.resolve_evidence(evidence).unwrap(),
        EvidenceResolution::Ambiguous(2)
    );
    // A read-only ledger resolves the persisted file without writing.
    let read_only = UnresolvedEffectLedger::read_only(root);
    assert!(matches!(
        read_only.resolve_evidence(evidence).unwrap(),
        EvidenceResolution::Ambiguous(2)
    ));
}

#[test]
fn incomplete_runtime_evidence_reports_dropped_facts() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let ledger = UnresolvedEffectLedger::persistent(root);
    let evidence = uuid::Uuid::new_v4();
    let key = record_key("run-incomplete-evidence", 1);
    ledger
        .record_unknown(
            &key,
            unknown_input("mcp__srv__record", None).with_evidence_id(evidence),
        )
        .unwrap();
    let mut input = unknown_input("mcp__srv__record", None);
    for _ in 0..MAX_FACTS_PER_RECORD {
        input = input.with_evidence_id(uuid::Uuid::new_v4());
        ledger.record_unknown(&key, input.clone()).unwrap();
    }
    match ledger.resolve_evidence(evidence).unwrap() {
        EvidenceResolution::Unique(record) => {
            assert_eq!(record.key, key);
            assert!(record.facts_dropped > 0, "the record must be incomplete");
        }
        other => panic!("expected one incomplete record, got {other:?}"),
    }
}

#[test]
fn disabled_ledger_cannot_resolve_runtime_evidence() {
    let ledger = UnresolvedEffectLedger::disabled();
    assert!(matches!(
        ledger.resolve_evidence(uuid::Uuid::new_v4()),
        Err(UnresolvedEffectError::Disabled)
    ));
}
