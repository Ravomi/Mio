//! DES-04 去重账本 owner / liveness 观测的集成测试。
//!
//! 只使用本地进程表与临时工作区：不触网、不调用真实 Provider，也不使用任何凭据。
//! 真实子进程用例都带超时，并在结束时 kill + wait（`ChildGuard`），不留后台进程。

use mio_coding_agent::dedup::{
    LEDGER_DIR, LEDGER_FILE, MAX_DEDUP_KEY_BYTES, MAX_LEDGER_FILE_SIZE, MAX_OWNER_IDENTITY_CHARS,
    TaskClaim, TaskKeyLedger, TaskKeyState, TaskLiveness, capture_owner_for_pid,
};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::{Child, Command};
#[cfg(target_os = "linux")]
use std::thread::sleep;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

fn ledger_path(root: &Path) -> PathBuf {
    root.join(LEDGER_DIR).join(LEDGER_FILE)
}

fn task_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn record(task_id: &str, state: &str, key: &str, owner: Option<Value>) -> Value {
    let mut record = json!({
        "task_id": task_id,
        "state": state,
        "key": key,
        "updated": 0,
    });
    if let Some(owner) = owner {
        record["owner"] = owner;
    }
    record
}

fn write_ledger(root: &Path, entries: &Value) {
    let dir = root.join(LEDGER_DIR);
    fs::create_dir_all(&dir).unwrap();
    let payload = json!({"version": 1, "updated": 0, "entries": entries});
    fs::write(dir.join(LEDGER_FILE), payload.to_string()).unwrap();
}

fn observe(root: &Path, key: &str) -> TaskLiveness {
    TaskKeyLedger::persistent(root)
        .observe(key)
        .unwrap()
        .expect("record must exist")
        .liveness
}

fn digest(fill: char) -> String {
    fill.to_string().repeat(MAX_OWNER_IDENTITY_CHARS)
}

fn is_hex_digest(value: &str) -> bool {
    value.len() == MAX_OWNER_IDENTITY_CHARS
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn synthetic_owner() -> Value {
    json!({
        "pid": 1,
        "start_time_ticks": 1,
        "host_id": digest('a'),
        "boot_id": digest('b'),
        "pid_namespace": digest('c'),
        "captured": 0,
    })
}

#[test]
fn observe_missing_key_returns_none() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = TaskKeyLedger::persistent(dir.path());
    assert!(ledger.observe("absent").unwrap().is_none());
}

#[test]
fn capture_owner_requires_process_evidence() {
    let captured = capture_owner_for_pid(std::process::id());
    #[cfg(target_os = "linux")]
    {
        let owner = captured.expect("linux must capture the current process identity");
        assert_eq!(owner.pid, std::process::id());
        assert!(owner.start_time_ticks > 0);
        assert!(is_hex_digest(&owner.host_id));
        assert!(is_hex_digest(&owner.boot_id));
        assert!(is_hex_digest(&owner.pid_namespace));
    }
    #[cfg(not(target_os = "linux"))]
    assert!(captured.is_none());
    assert!(capture_owner_for_pid(0).is_none());
}

#[test]
fn claim_captures_bounded_owner_without_raw_identifiers() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = TaskKeyLedger::persistent(dir.path());
    let claim = ledger.claim("job-1").unwrap();
    let id = claim.task_id().to_owned();

    let observation = ledger.observe("job-1").unwrap().unwrap();
    assert_eq!(observation.task_id(), id.as_str());
    assert_eq!(observation.record.state, TaskKeyState::Queued);
    #[cfg(target_os = "linux")]
    {
        assert_eq!(observation.liveness, TaskLiveness::Live);
        let owner = observation.record.owner.as_ref().expect("owner captured");
        assert_eq!(owner.pid, std::process::id());
        assert!(is_hex_digest(&owner.host_id));
        assert!(is_hex_digest(&owner.boot_id));
        assert!(is_hex_digest(&owner.pid_namespace));
    }
    #[cfg(not(target_os = "linux"))]
    assert_eq!(observation.liveness, TaskLiveness::Unknown);

    let raw = fs::read_to_string(ledger_path(dir.path())).unwrap();
    let persisted: Value = serde_json::from_str(&raw).unwrap();
    #[cfg(target_os = "linux")]
    {
        assert!(persisted["entries"][0]["owner"].is_object());
        // 只保存摘要，绝不落机器原始标识。
        let machine_id = fs::read_to_string("/etc/machine-id").unwrap_or_default();
        let machine_id = machine_id.trim().to_owned();
        if !machine_id.is_empty() {
            assert!(
                !raw.contains(&machine_id),
                "raw machine id must never be persisted: {raw}"
            );
        }
    }
    #[cfg(not(target_os = "linux"))]
    assert!(persisted["entries"][0]["owner"].is_null());
}

#[test]
fn other_host_owner_is_unknown_not_dead() {
    // host 不同：共享工作区可能属于另一台机器，不能宣称死亡。
    let dir = tempfile::tempdir().unwrap();
    write_ledger(
        dir.path(),
        &json!([record(&task_id(), "active", "job", Some(synthetic_owner()))]),
    );
    assert_eq!(observe(dir.path(), "job"), TaskLiveness::Unknown);
}

#[test]
fn legacy_record_without_owner_is_unknown_and_blocks_replay() {
    let dir = tempfile::tempdir().unwrap();
    let id = task_id();
    write_ledger(dir.path(), &json!([record(&id, "active", "legacy", None)]));
    let ledger = TaskKeyLedger::persistent(dir.path());

    let observation = ledger.observe("legacy").unwrap().unwrap();
    assert_eq!(observation.liveness, TaskLiveness::Unknown);
    assert_eq!(observation.record.state, TaskKeyState::Active);

    // 旧记录仍然阻止重放，并且不会因为缺少 owner 或时间被自动改成 failed。
    assert_eq!(
        ledger.claim("legacy").unwrap(),
        TaskClaim::Existing {
            task_id: id.clone(),
            state: TaskKeyState::Active,
        }
    );
    assert_eq!(
        ledger.lookup("legacy").unwrap().unwrap().state,
        TaskKeyState::Active
    );
}

#[test]
fn read_only_queries_do_not_rewrite_the_persisted_record() {
    let dir = tempfile::tempdir().unwrap();
    let id = task_id();
    write_ledger(
        dir.path(),
        &json!([record(&id, "active", "job", Some(synthetic_owner()))]),
    );
    let ledger = TaskKeyLedger::persistent(dir.path());

    let before = fs::read(ledger_path(dir.path())).unwrap();
    assert!(ledger.lookup("job").unwrap().is_some());
    assert!(ledger.observe("job").unwrap().is_some());
    let after = fs::read(ledger_path(dir.path())).unwrap();
    assert_eq!(
        before, after,
        "read-only queries must not rewrite the record"
    );
}

#[test]
fn terminal_record_is_unknown_and_never_regresses() {
    let dir = tempfile::tempdir().unwrap();
    let id = task_id();
    write_ledger(
        dir.path(),
        &json!([record(&id, "completed", "job", Some(synthetic_owner()))]),
    );
    let ledger = TaskKeyLedger::persistent(dir.path());

    // 终态不参与 owner 探测：即使 owner 指向一个不存在的进程也不宣称死亡。
    let observation = ledger.observe("job").unwrap().unwrap();
    assert_eq!(observation.liveness, TaskLiveness::Unknown);
    assert_eq!(observation.record.state, TaskKeyState::Completed);

    assert_eq!(
        ledger.claim("job").unwrap(),
        TaskClaim::Existing {
            task_id: id.clone(),
            state: TaskKeyState::Completed,
        }
    );
    // 重复 finalize 是幂等 no-op，不会覆盖第一次的终态。
    ledger.finish(&id, TaskKeyState::Failed).unwrap();
    assert_eq!(
        ledger.lookup("job").unwrap().unwrap().state,
        TaskKeyState::Completed
    );
}

#[test]
fn owner_metadata_roundtrips_through_the_ledger_file() {
    let dir = tempfile::tempdir().unwrap();
    let id = task_id();
    let owner = json!({
        "pid": 1234,
        "start_time_ticks": 5678,
        "host_id": digest('a'),
        "boot_id": digest('b'),
        "pid_namespace": digest('c'),
        "captured": 99,
    });
    write_ledger(
        dir.path(),
        &json!([record(&id, "active", "job", Some(owner.clone()))]),
    );
    let ledger = TaskKeyLedger::persistent(dir.path());
    let stored = ledger.lookup("job").unwrap().unwrap();
    let parsed = serde_json::to_value(stored.owner.expect("owner present")).unwrap();
    assert_eq!(parsed, owner);
}

fn assert_owner_rejected(root: &Path, owner: Value) {
    write_ledger(
        root,
        &json!([record(&task_id(), "active", "bad", Some(owner))]),
    );
    let ledger = TaskKeyLedger::persistent(root);
    assert!(
        ledger.observe("bad").is_err(),
        "malformed owner must fail closed"
    );
    assert!(
        ledger.lookup("bad").is_err(),
        "malformed owner must fail closed"
    );
    assert!(
        ledger.claim("bad").is_err(),
        "malformed owner must fail closed"
    );
}

#[test]
fn malformed_owner_metadata_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let mut owner = synthetic_owner();
    owner["host_id"] = json!("a".repeat(MAX_OWNER_IDENTITY_CHARS - 1));
    assert_owner_rejected(root, owner);

    let mut owner = synthetic_owner();
    owner["boot_id"] = json!(digest('A'));
    assert_owner_rejected(root, owner);

    let mut owner = synthetic_owner();
    owner["pid_namespace"] = json!(digest('z'));
    assert_owner_rejected(root, owner);

    let mut owner = synthetic_owner();
    owner["pid"] = json!(0);
    assert_owner_rejected(root, owner);

    let mut owner = synthetic_owner();
    owner["start_time_ticks"] = json!(0);
    assert_owner_rejected(root, owner);

    let mut owner = synthetic_owner();
    owner.as_object_mut().unwrap().remove("boot_id");
    assert_owner_rejected(root, owner);
}

#[test]
fn oversized_ledger_file_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let ledger_dir = dir.path().join(LEDGER_DIR);
    fs::create_dir_all(&ledger_dir).unwrap();

    // 每 key 用满 512 字节，1000 条足以超过文件字节上限。
    let mut entries = Vec::new();
    for index in 0..1000 {
        let mut key = format!("{index:08}");
        key.push_str(&"k".repeat(MAX_DEDUP_KEY_BYTES - key.len()));
        entries.push(record(&task_id(), "queued", &key, None));
    }
    let payload = json!({"version": 1, "updated": 0, "entries": entries}).to_string();
    assert!(payload.len() > MAX_LEDGER_FILE_SIZE);
    fs::write(ledger_dir.join(LEDGER_FILE), payload).unwrap();

    let ledger = TaskKeyLedger::persistent(dir.path());
    assert!(ledger.lookup("anything").is_err());
    assert!(ledger.observe("anything").is_err());
    assert!(ledger.claim("anything").is_err());
}

#[cfg(target_os = "linux")]
struct ChildGuard(Child);

#[cfg(target_os = "linux")]
impl ChildGuard {
    fn spawn(command: &mut Command) -> Self {
        Self(command.spawn().expect("spawn child process"))
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }

    fn kill_and_wait(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(target_os = "linux")]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.kill_and_wait();
    }
}

#[cfg(target_os = "linux")]
fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if check() {
            return;
        }
        sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

#[cfg(target_os = "linux")]
fn proc_state(pid: u32) -> Option<u8> {
    let stat = fs::read(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.iter().rposition(|byte| *byte == b')')?;
    let rest = stat.get(close + 1..)?;
    rest.split(|byte| *byte == b' ')
        .find(|field| !field.is_empty())
        .and_then(|field| field.first().copied())
}

#[cfg(target_os = "linux")]
#[test]
fn real_child_is_live_then_stale_after_exit() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = ChildGuard::spawn(Command::new("sleep").arg("120"));
    let owner = capture_owner_for_pid(child.pid()).expect("live child identity");
    write_ledger(
        dir.path(),
        &json!([record(
            &task_id(),
            "active",
            "job",
            Some(serde_json::to_value(owner).unwrap())
        )]),
    );
    assert_eq!(observe(dir.path(), "job"), TaskLiveness::Live);

    child.kill_and_wait();
    wait_for("exited child to be observed stale", || {
        observe(dir.path(), "job") == TaskLiveness::Stale
    });
}

#[cfg(target_os = "linux")]
#[test]
fn changed_start_time_means_pid_was_reused() {
    let dir = tempfile::tempdir().unwrap();
    let child = ChildGuard::spawn(Command::new("sleep").arg("120"));
    let mut owner = capture_owner_for_pid(child.pid()).expect("live child identity");
    // 同一个 PID 仍然存在，但启动时刻不同：原 owner 的身份已被替换。
    owner.start_time_ticks = owner.start_time_ticks.wrapping_add(1);
    write_ledger(
        dir.path(),
        &json!([record(
            &task_id(),
            "active",
            "job",
            Some(serde_json::to_value(owner).unwrap())
        )]),
    );
    assert_eq!(observe(dir.path(), "job"), TaskLiveness::Stale);
}

#[cfg(target_os = "linux")]
#[test]
fn zombie_process_is_not_live() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = ChildGuard::spawn(Command::new("sh").arg("-c").arg("exit 0"));
    let pid = child.pid();
    let owner = capture_owner_for_pid(pid).expect("child identity");
    write_ledger(
        dir.path(),
        &json!([record(
            &task_id(),
            "active",
            "job",
            Some(serde_json::to_value(owner).unwrap())
        )]),
    );

    // 故意不 wait：子进程退出后保持 zombie，证明“已结束但未回收”也不算存活。
    wait_for("child to become a zombie", || proc_state(pid) == Some(b'Z'));
    assert_eq!(observe(dir.path(), "job"), TaskLiveness::Stale);

    let _ = child.0.wait();
}

#[cfg(target_os = "linux")]
#[test]
fn different_pid_namespace_is_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let mut owner = capture_owner_for_pid(std::process::id()).expect("self identity");
    owner.pid_namespace = digest('9');
    write_ledger(
        dir.path(),
        &json!([record(
            &task_id(),
            "active",
            "job",
            Some(serde_json::to_value(owner).unwrap())
        )]),
    );
    // PID 只在同一个 namespace 内可比：namespace 不同只能是未知。
    assert_eq!(observe(dir.path(), "job"), TaskLiveness::Unknown);
}

#[cfg(target_os = "linux")]
#[test]
fn same_host_new_boot_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    let real = capture_owner_for_pid(std::process::id()).expect("self identity");
    let mut owner = real.clone();
    owner.boot_id = digest('9');
    assert_ne!(owner.boot_id, real.boot_id);
    write_ledger(
        dir.path(),
        &json!([record(
            &task_id(),
            "active",
            "job",
            Some(serde_json::to_value(owner).unwrap())
        )]),
    );
    // 同 host 已经重启：上一个 boot 的进程必然不存在。
    assert_eq!(observe(dir.path(), "job"), TaskLiveness::Stale);
}

#[cfg(target_os = "linux")]
#[test]
fn stale_record_still_returns_existing_and_is_not_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    let child = ChildGuard::spawn(Command::new("sleep").arg("120"));
    let mut owner = capture_owner_for_pid(child.pid()).expect("live child identity");
    owner.start_time_ticks = owner.start_time_ticks.wrapping_add(1);
    let id = task_id();
    write_ledger(
        dir.path(),
        &json!([record(
            &id,
            "queued",
            "job",
            Some(serde_json::to_value(owner).unwrap())
        )]),
    );

    let ledger = TaskKeyLedger::persistent(dir.path());
    assert_eq!(
        ledger.observe("job").unwrap().unwrap().liveness,
        TaskLiveness::Stale
    );
    let before = fs::read(ledger_path(dir.path())).unwrap();

    // stale 不等于可重放：仍然返回原 task_id 与状态，且不自动转 failed。
    assert_eq!(
        ledger.claim("job").unwrap(),
        TaskClaim::Existing {
            task_id: id.clone(),
            state: TaskKeyState::Queued,
        }
    );
    assert_eq!(
        ledger.lookup("job").unwrap().unwrap().state,
        TaskKeyState::Queued
    );
    assert_eq!(
        ledger.observe("job").unwrap().unwrap().liveness,
        TaskLiveness::Stale
    );

    let after = fs::read(ledger_path(dir.path())).unwrap();
    assert_eq!(
        before, after,
        "queries and a dedup hit must not rewrite the record"
    );
}
