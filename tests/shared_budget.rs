//! `shared_limits` 跨进程预算账本的针对性测试。
//!
//! 覆盖：版本化账本的持久化与私有权限、原子预留与真实 usage 结算、未知 usage 保守
//! 计费、over-reservation 与 overage 的 fail-closed、调用次数硬上限、pre-dispatch
//! 归还、只能收窄不能放宽、未设置预算保持兼容、账本损坏或丢失 fail-closed，以及通过
//! 当前测试程序子进程验证的真实多进程竞争、重启继承与 `SIGKILL` 后不退款。
//!
//! 子进程模式复用同一个测试二进制：父进程以 `--exact shared_budget_child_entrypoint`
//! 重新执行自身，并通过环境变量下发动作；不使用任何真实模型服务，也不写入任何凭据。

use mio_coding_agent::shared_limits::{SharedLimits, SharedLimitsConfig, SharedLimitsError};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
};

const ENV_ACTION: &str = "MIO_SB_ACTION";
const ENV_ROOT: &str = "MIO_SB_ROOT";
const ENV_MAX_TOKENS: &str = "MIO_SB_MAX_TOKENS";
const ENV_MAX_CALLS: &str = "MIO_SB_MAX_CALLS";
const ENV_TOKENS: &str = "MIO_SB_TOKENS";
const ENV_USAGE: &str = "MIO_SB_USAGE";
const ENV_HOLD_MS: &str = "MIO_SB_HOLD_MS";
const ENV_RESULT: &str = "MIO_SB_RESULT";
const ENV_SIGNAL: &str = "MIO_SB_SIGNAL";
const ENV_STARTED: &str = "MIO_SB_STARTED";
const ENV_WAIT: &str = "MIO_SB_WAIT";
const ENV_RELEASE: &str = "MIO_SB_RELEASE";

// ---------------------------------------------------------------------------
// 子进程入口
// ---------------------------------------------------------------------------

/// 供父进程复用的子进程入口；普通测试运行中因缺少环境变量而直接返回。
#[test]
fn shared_budget_child_entrypoint() {
    let Ok(action) = std::env::var(ENV_ACTION) else {
        return;
    };
    let Some(root) = env_string(ENV_ROOT) else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build child runtime");
    if let Err(err) = runtime.block_on(run_child(&action, &root)) {
        eprintln!("shared_budget child failed: {err}");
        std::process::exit(2);
    }
    std::process::exit(0);
}

async fn run_child(action: &str, root: &str) -> Result<(), Box<dyn std::error::Error>> {
    // 可选的同步闸门：先报告 STARTED，再等待 gate 文件出现，用于制造真实竞争。
    if let Some(gate) = env_string(ENV_WAIT) {
        if let Some(started) = env_string(ENV_STARTED) {
            write_atomic(Path::new(&started), b"STARTED")?;
        }
        while !Path::new(&gate).exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    let limits = match SharedLimits::new(root, child_config()) {
        Ok(limits) => limits,
        Err(SharedLimitsError::BudgetPolicyMismatch(_)) => {
            signal(ENV_RESULT, "MISMATCH")?;
            return Ok(());
        }
        Err(err) => return Err(format!("open shared limits failed: {err}").into()),
    };
    match action {
        "try" => {
            let tokens = env_u64(ENV_TOKENS, 1);
            match limits.reserve_budget(tokens).await {
                Ok(Some(permit)) => {
                    signal(ENV_RESULT, "RESERVED")?;
                    // 可选的释放闸门：赢家一直持有预留，直到父进程放行，从而让并发
                    // 竞争的结果确定（不会被「赢家已经结算」掩盖）。
                    if let Some(release) = env_string(ENV_RELEASE) {
                        while !Path::new(&release).exists() {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                    }
                    let hold = env_u64(ENV_HOLD_MS, 0);
                    if hold > 0 {
                        tokio::time::sleep(Duration::from_millis(hold)).await;
                    }
                    permit.settle(usage_from_env()).await?;
                }
                Ok(None) => signal(ENV_RESULT, "NO_BUDGET")?,
                Err(_) => signal(ENV_RESULT, "DENIED")?,
            }
        }
        "settle" => {
            let tokens = env_u64(ENV_TOKENS, 1);
            let permit = limits
                .reserve_budget(tokens)
                .await?
                .ok_or("budget ledger is not enabled")?;
            permit.settle(usage_from_env()).await?;
            signal(ENV_RESULT, "SETTLED")?;
        }
        "reserve_hold" => {
            // 预留后一直持有，不做结算；父进程会用 SIGKILL 终止本进程。
            let tokens = env_u64(ENV_TOKENS, 1);
            match limits.reserve_budget(tokens).await {
                Ok(Some(_permit)) => {
                    // `_permit` 绑定到函数结束；被 SIGKILL 时不会执行任何 Drop 结算。
                    signal(ENV_SIGNAL, "READY")?;
                    tokio::time::sleep(Duration::from_millis(env_u64(ENV_HOLD_MS, 60_000))).await;
                }
                Ok(None) => signal(ENV_RESULT, "NO_BUDGET")?,
                Err(_) => signal(ENV_RESULT, "DENIED")?,
            }
        }
        other => return Err(format!("unknown child action: {other}").into()),
    }
    Ok(())
}

fn child_config() -> SharedLimitsConfig {
    SharedLimitsConfig {
        max_total_tokens: env_u64_opt(ENV_MAX_TOKENS),
        max_model_calls: env_u64_opt(ENV_MAX_CALLS),
        ..SharedLimitsConfig::default()
    }
}

fn usage_from_env() -> Option<u64> {
    env_string(ENV_USAGE).and_then(|value| value.parse().ok())
}

fn signal(var: &str, content: &str) -> std::io::Result<()> {
    let path = std::env::var(var).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("missing {var}"))
    })?;
    write_atomic(Path::new(&path), content.as_bytes())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_u64_opt(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
}

fn env_string(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Some(value),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 父进程辅助
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct ChildSpec {
    action: &'static str,
    max_tokens: Option<u64>,
    max_calls: Option<u64>,
    tokens: u64,
    usage: Option<u64>,
    hold_ms: u64,
    signal: Option<PathBuf>,
    result: Option<PathBuf>,
    started: Option<PathBuf>,
    wait: Option<PathBuf>,
    release: Option<PathBuf>,
}

fn spawn_child(root: &Path, spec: &ChildSpec) -> Child {
    let exe = std::env::current_exe().expect("current test executable");
    let mut command = Command::new(exe);
    command
        .arg("--exact")
        .arg("shared_budget_child_entrypoint")
        .arg("--test-threads=1")
        .env(ENV_ACTION, spec.action)
        .env(ENV_ROOT, root)
        .env(
            ENV_MAX_TOKENS,
            spec.max_tokens.map(|v| v.to_string()).unwrap_or_default(),
        )
        .env(
            ENV_MAX_CALLS,
            spec.max_calls.map(|v| v.to_string()).unwrap_or_default(),
        )
        .env(ENV_TOKENS, spec.tokens.to_string())
        .env(
            ENV_USAGE,
            spec.usage.map(|v| v.to_string()).unwrap_or_default(),
        )
        .env(ENV_HOLD_MS, spec.hold_ms.to_string())
        .env(ENV_SIGNAL, path_to_env(spec.signal.as_deref()))
        .env(ENV_RESULT, path_to_env(spec.result.as_deref()))
        .env(ENV_STARTED, path_to_env(spec.started.as_deref()))
        .env(ENV_WAIT, path_to_env(spec.wait.as_deref()))
        .env(ENV_RELEASE, path_to_env(spec.release.as_deref()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.spawn().expect("spawn shared_budget child")
}

fn path_to_env(path: Option<&Path>) -> String {
    path.map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default()
}

async fn wait_for_file(path: &Path, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if path.exists() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// 等待子进程写出结果文件并读回内容；**不**等待子进程退出。
async fn wait_result(child: &mut Child, result: &Path) -> String {
    if !wait_for_file(result, Duration::from_secs(20)).await {
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), pipe.read_to_string(&mut stderr))
                .await;
        }
        child.kill().await.ok();
        panic!(
            "child produced no result file {} (stderr: {stderr})",
            result.display()
        );
    }
    std::fs::read_to_string(result)
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// 等待结果文件并等待子进程正常退出。
async fn child_result(child: &mut Child, result: &Path) -> String {
    let content = wait_result(child, result).await;
    let _ = tokio::time::timeout(Duration::from_secs(15), child.wait()).await;
    content
}

fn config_with_tokens(limit: u64) -> SharedLimitsConfig {
    SharedLimitsConfig {
        max_total_tokens: Some(limit),
        ..SharedLimitsConfig::default()
    }
}

fn config_with_calls(limit: u64) -> SharedLimitsConfig {
    SharedLimitsConfig {
        max_model_calls: Some(limit),
        ..SharedLimitsConfig::default()
    }
}

// ---------------------------------------------------------------------------
// 账本格式与基础语义
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn budget_ledger_is_versioned_and_private() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    SharedLimits::new(&root, config_with_tokens(10_000)).unwrap();

    let ledger_path = root.join("budget.json");
    let mode = std::fs::metadata(&ledger_path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "budget ledger must be 0600");
    let lock_mode = std::fs::metadata(root.join("budget.lock"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(lock_mode, 0o600, "budget lock must be 0600");

    let raw = std::fs::read_to_string(&ledger_path).unwrap();
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(value["version"], 1);
    assert_eq!(value["max_total_tokens"], 10_000);
    assert_eq!(value["max_model_calls"], serde_json::Value::Null);
    assert_eq!(value["calls_started"], 0);
    assert_eq!(value["tokens_reserved_active"], 0);
    assert_eq!(value["tokens_used"], 0);
    assert_eq!(value["blocked"].as_bool(), Some(false));
}

#[tokio::test]
async fn unset_budget_keeps_a_root_compatible() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = SharedLimits::new(&root, SharedLimitsConfig::default()).unwrap();

    assert!(
        !root.join("budget.json").exists(),
        "no ledger must be created"
    );
    assert!(limits.reserve_budget(1_000).await.unwrap().is_none());
    let snapshot = limits.snapshot().unwrap();
    assert_eq!(snapshot.budget.max_total_tokens, None);
    assert_eq!(snapshot.budget.tokens_used, 0);
    assert_eq!(limits.config(), SharedLimitsConfig::default());
}

#[tokio::test]
async fn reservation_settles_real_usage_and_leaves_room() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = SharedLimits::new(&root, config_with_tokens(1_000)).unwrap();

    let permit = limits.reserve_budget(400).await.unwrap().unwrap();
    let snapshot = limits.budget_snapshot().unwrap();
    assert_eq!(snapshot.calls_started, 1);
    assert_eq!(snapshot.tokens_reserved_active, 400);
    assert_eq!(snapshot.tokens_used, 0);

    permit.settle(Some(150)).await.unwrap();
    let snapshot = limits.budget_snapshot().unwrap();
    assert_eq!(snapshot.tokens_used, 150);
    assert_eq!(snapshot.tokens_reserved_active, 0);
    assert_eq!(
        snapshot.tokens_over_reservation, 0,
        "usage below reservation is not over-reservation"
    );
    assert_eq!(
        snapshot.calls_started, 1,
        "settling never refunds the call count"
    );

    // 150 + 500 <= 1000 仍可预留。
    let permit = limits.reserve_budget(500).await.unwrap().unwrap();
    permit.settle(Some(500)).await.unwrap();
    assert_eq!(limits.budget_snapshot().unwrap().tokens_used, 650);

    // 650 + 400 > 1000 → 下一次预留 fail-closed，且不记录 overage。
    assert!(matches!(
        limits.reserve_budget(400).await,
        Err(SharedLimitsError::BudgetExhausted { .. })
    ));
    assert_eq!(limits.budget_snapshot().unwrap().tokens_used, 650);
    assert!(!limits.budget_snapshot().unwrap().blocked);
}

#[tokio::test]
async fn unknown_usage_is_charged_the_full_reservation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = SharedLimits::new(&root, config_with_tokens(1_000)).unwrap();

    // 未显式结算就被 drop（取消 / 崩溃前的最后一次 drop）：按未知 usage 保守计费。
    let permit = limits.reserve_budget(300).await.unwrap().unwrap();
    drop(permit);
    let snapshot = limits.budget_snapshot().unwrap();
    assert_eq!(
        snapshot.tokens_used, 300,
        "unknown usage must never be charged zero"
    );
    assert_eq!(snapshot.tokens_reserved_active, 0);
    assert_eq!(snapshot.calls_started, 1);

    // 显式 None 与 drop 的语义一致。
    let permit = limits.reserve_budget(200).await.unwrap().unwrap();
    permit.settle(None).await.unwrap();
    assert_eq!(limits.budget_snapshot().unwrap().tokens_used, 500);
}

#[tokio::test]
async fn pre_dispatch_abandon_refunds_tokens_and_calls() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = SharedLimits::new(&root, config_with_tokens(200)).unwrap();

    let permit = limits.reserve_budget(200).await.unwrap().unwrap();
    permit.abandon().await.unwrap();
    let snapshot = limits.budget_snapshot().unwrap();
    assert_eq!(snapshot.tokens_used, 0);
    assert_eq!(snapshot.tokens_reserved_active, 0);
    assert_eq!(
        snapshot.calls_started, 0,
        "a call that never reached the provider is not counted"
    );

    // 归还后同一份预算仍然可用。
    let permit = limits.reserve_budget(200).await.unwrap().unwrap();
    permit.settle(Some(200)).await.unwrap();
    assert_eq!(limits.budget_snapshot().unwrap().tokens_used, 200);
}

#[tokio::test]
async fn over_reservation_records_overage_and_blocks_follow_up() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = SharedLimits::new(&root, config_with_tokens(1_000)).unwrap();

    let permit = limits.reserve_budget(300).await.unwrap().unwrap();
    permit.settle(Some(5_000)).await.unwrap();
    let snapshot = limits.budget_snapshot().unwrap();
    assert_eq!(snapshot.tokens_used, 5_000);
    assert_eq!(snapshot.tokens_over_reservation, 4_700);
    assert_eq!(snapshot.tokens_overage, 4_000);
    assert!(snapshot.blocked, "overage must block follow-up requests");

    // 超额后即使预留很小也一律拒绝（fail-closed）。
    assert!(matches!(
        limits.reserve_budget(1).await,
        Err(SharedLimitsError::BudgetExhausted { .. })
    ));
    assert!(limits.reserve_budget(50_000).await.is_err());
}

#[tokio::test]
async fn call_budget_is_hard_and_counts_only_dispatched_reservations() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = SharedLimits::new(&root, config_with_calls(2)).unwrap();

    for _ in 0..2 {
        let permit = limits.reserve_budget(10).await.unwrap().unwrap();
        permit.settle(Some(1)).await.unwrap();
    }
    assert!(matches!(
        limits.reserve_budget(10).await,
        Err(SharedLimitsError::ModelCallBudgetExceeded { used: 2, limit: 2 })
    ));

    // 未发出的调用会归还次数：新 root 上先放弃两次，仍可正常发出两次。
    let other = temp.path().join("other");
    let limits = SharedLimits::new(&other, config_with_calls(2)).unwrap();
    for _ in 0..2 {
        let permit = limits.reserve_budget(10).await.unwrap().unwrap();
        permit.abandon().await.unwrap();
    }
    assert_eq!(limits.budget_snapshot().unwrap().calls_started, 0);
    for _ in 0..2 {
        let permit = limits.reserve_budget(10).await.unwrap().unwrap();
        permit.settle(None).await.unwrap();
    }
    assert!(limits.reserve_budget(10).await.is_err());
}

#[tokio::test]
async fn a_single_reservation_larger_than_the_budget_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = SharedLimits::new(&root, config_with_tokens(100)).unwrap();
    assert!(matches!(
        limits.reserve_budget(101).await,
        Err(SharedLimitsError::BudgetReservationTooLarge {
            reservation: 101,
            limit: 100
        })
    ));
    assert_eq!(limits.budget_snapshot().unwrap().calls_started, 0);
}

// ---------------------------------------------------------------------------
// 只能收窄 / 继承 / 损坏 fail-closed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn widening_is_refused_and_narrowing_is_persisted() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    SharedLimits::new(&root, config_with_tokens(1_000)).unwrap();

    // 更窄：允许，并写回账本成为新的共享策略。
    let narrowed = SharedLimits::new(&root, config_with_tokens(400)).unwrap();
    assert_eq!(narrowed.config().max_total_tokens, Some(400));
    assert_eq!(
        narrowed.budget_snapshot().unwrap().max_total_tokens,
        Some(400)
    );

    // 更宽：fail-closed，绝不悄悄放大已有共享策略。
    assert!(matches!(
        SharedLimits::new(&root, config_with_tokens(1_000)),
        Err(SharedLimitsError::BudgetPolicyMismatch(_))
    ));
    // 完全未设置：继承账本已记录的策略。
    let inherited = SharedLimits::new(&root, SharedLimitsConfig::default()).unwrap();
    assert_eq!(inherited.config().max_total_tokens, Some(400));
    // 与已记录值相同仍然允许。
    SharedLimits::new(&root, config_with_tokens(400)).unwrap();
}

#[tokio::test]
async fn call_limit_can_only_be_narrowed_too() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    SharedLimits::new(&root, config_with_calls(10)).unwrap();
    assert!(matches!(
        SharedLimits::new(&root, config_with_calls(20)),
        Err(SharedLimitsError::BudgetPolicyMismatch(_))
    ));
    // Adding a previously unlimited token dimension narrows the policy.
    let token_limited = SharedLimits::new(
        &root,
        SharedLimitsConfig {
            max_model_calls: Some(10),
            max_total_tokens: Some(5_000),
            ..SharedLimitsConfig::default()
        },
    )
    .unwrap();
    assert_eq!(
        token_limited.budget_snapshot().unwrap().max_total_tokens,
        Some(5_000)
    );
    let narrowed = SharedLimits::new(
        &root,
        SharedLimitsConfig {
            max_model_calls: Some(3),
            ..SharedLimitsConfig::default()
        },
    )
    .unwrap();
    assert_eq!(narrowed.config().max_model_calls, Some(3));
}

#[tokio::test]
async fn corrupt_or_missing_budget_ledger_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    std::fs::create_dir_all(&root).unwrap();
    let ledger = root.join("budget.json");

    std::fs::write(&ledger, b"not-json").unwrap();
    assert!(matches!(
        SharedLimits::new(&root, config_with_tokens(100)),
        Err(SharedLimitsError::CorruptBudget(_))
    ));

    std::fs::write(
        &ledger,
        br#"{"version":2,"max_model_calls":null,"max_total_tokens":100,"calls_started":0,"tokens_reserved_active":0,"tokens_used":0,"tokens_over_reservation":0,"tokens_overage":0,"blocked":false}"#,
    )
    .unwrap();
    assert!(matches!(
        SharedLimits::new(&root, config_with_tokens(100)),
        Err(SharedLimitsError::CorruptBudget(_))
    ));

    std::fs::write(
        &ledger,
        br#"{"version":1,"max_model_calls":null,"max_total_tokens":0,"calls_started":0,"tokens_reserved_active":0,"tokens_used":0,"tokens_over_reservation":0,"tokens_overage":0,"blocked":false}"#,
    )
    .unwrap();
    assert!(matches!(
        SharedLimits::new(&root, config_with_tokens(100)),
        Err(SharedLimitsError::CorruptBudget(_))
    ));

    std::fs::write(&ledger, vec![b'a'; 16 * 1024]).unwrap();
    assert!(matches!(
        SharedLimits::new(&root, config_with_tokens(100)),
        Err(SharedLimitsError::CorruptBudget(_))
    ));

    // 账本被删除后运行时必须 fail-closed，而不是静默降级为「没有预算」。
    let manager = temp.path().join("manager");
    let limits = SharedLimits::new(&manager, config_with_tokens(1_000)).unwrap();
    std::fs::remove_file(manager.join("budget.json")).unwrap();
    assert!(matches!(
        limits.reserve_budget(10).await,
        Err(SharedLimitsError::CorruptBudget(_))
    ));
    assert!(matches!(
        limits.budget_snapshot(),
        Err(SharedLimitsError::CorruptBudget(_))
    ));
}

#[cfg(unix)]
#[test]
fn symlinked_budget_ledger_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    std::fs::create_dir_all(&root).unwrap();
    let target = temp.path().join("target.json");
    std::fs::write(&target, b"{}").unwrap();
    std::os::unix::fs::symlink(&target, root.join("budget.json")).unwrap();
    assert!(matches!(
        SharedLimits::new(&root, config_with_tokens(100)),
        Err(SharedLimitsError::UnsafePath(_))
    ));
}

// ---------------------------------------------------------------------------
// 真实多进程竞争、重启与崩溃
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_processes_never_over_reserve_the_shared_budget() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    SharedLimits::new(&root, config_with_tokens(300)).unwrap();

    let release = temp.path().join("release");
    let started: Vec<PathBuf> = (0..4).map(|i| temp.path().join(format!("s{i}"))).collect();
    let results: Vec<PathBuf> = (0..4).map(|i| temp.path().join(format!("r{i}"))).collect();
    let gate = temp.path().join("gate");
    let mut children: Vec<Child> = (0..4)
        .map(|index| {
            spawn_child(
                &root,
                &ChildSpec {
                    action: "try",
                    max_tokens: Some(300),
                    tokens: 200,
                    usage: Some(200),
                    result: Some(results[index].clone()),
                    started: Some(started[index].clone()),
                    wait: Some(gate.clone()),
                    release: Some(release.clone()),
                    ..Default::default()
                },
            )
        })
        .collect();
    for path in &started {
        assert!(
            wait_for_file(path, Duration::from_secs(20)).await,
            "child must reach the gate"
        );
    }
    std::fs::write(&gate, b"go").unwrap();

    let mut reserved = 0;
    let mut denied = 0;
    for (child, result) in children.iter_mut().zip(&results) {
        match wait_result(child, result).await.as_str() {
            "RESERVED" => reserved += 1,
            "DENIED" => denied += 1,
            other => panic!("unexpected child outcome: {other}"),
        }
    }
    assert_eq!(
        reserved, 1,
        "exactly one process may hold the remaining budget"
    );
    assert_eq!(denied, 3);

    // 只有赢家结算后才释放预算；在此之前其它进程确实被拒绝。
    let snapshot = SharedLimits::new(&root, config_with_tokens(300))
        .unwrap()
        .budget_snapshot()
        .unwrap();
    assert_eq!(snapshot.tokens_reserved_active, 200);
    assert_eq!(snapshot.tokens_used, 0);
    assert_eq!(snapshot.calls_started, 1);

    std::fs::write(&release, b"go").unwrap();
    for mut child in children {
        let _ = tokio::time::timeout(Duration::from_secs(20), child.wait()).await;
    }
    let snapshot = SharedLimits::new(&root, SharedLimitsConfig::default())
        .unwrap()
        .budget_snapshot()
        .unwrap();
    assert_eq!(snapshot.tokens_used, 200);
    assert_eq!(snapshot.tokens_reserved_active, 0);
    assert_eq!(snapshot.calls_started, 1);
}

#[tokio::test]
async fn concurrent_processes_share_one_call_budget() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    SharedLimits::new(&root, config_with_calls(1)).unwrap();

    let release = temp.path().join("release");
    let started: Vec<PathBuf> = (0..3).map(|i| temp.path().join(format!("cs{i}"))).collect();
    let results: Vec<PathBuf> = (0..3).map(|i| temp.path().join(format!("cr{i}"))).collect();
    let gate = temp.path().join("cgate");
    let mut children: Vec<Child> = (0..3)
        .map(|index| {
            spawn_child(
                &root,
                &ChildSpec {
                    action: "try",
                    max_calls: Some(1),
                    tokens: 10,
                    usage: Some(0),
                    result: Some(results[index].clone()),
                    started: Some(started[index].clone()),
                    wait: Some(gate.clone()),
                    release: Some(release.clone()),
                    ..Default::default()
                },
            )
        })
        .collect();
    for path in &started {
        assert!(wait_for_file(path, Duration::from_secs(20)).await);
    }
    std::fs::write(&gate, b"go").unwrap();

    let mut reserved = 0;
    for (child, result) in children.iter_mut().zip(&results) {
        if wait_result(child, result).await == "RESERVED" {
            reserved += 1;
        }
    }
    assert_eq!(reserved, 1, "a call budget of 1 admits exactly one process");

    std::fs::write(&release, b"go").unwrap();
    for mut child in children {
        let _ = tokio::time::timeout(Duration::from_secs(20), child.wait()).await;
    }
    let snapshot = SharedLimits::new(&root, config_with_calls(1))
        .unwrap()
        .budget_snapshot()
        .unwrap();
    assert_eq!(snapshot.calls_started, 1);
    assert_eq!(
        snapshot.tokens_used, 0,
        "a reported zero usage is charged zero"
    );
}

#[tokio::test]
async fn settled_usage_persists_across_process_restart() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let result = temp.path().join("settle");
    let mut child = spawn_child(
        &root,
        &ChildSpec {
            action: "settle",
            max_tokens: Some(1_000),
            tokens: 600,
            usage: Some(600),
            result: Some(result.clone()),
            ..Default::default()
        },
    );
    assert_eq!(child_result(&mut child, &result).await, "SETTLED");

    // 重启后（新进程重新打开同一 root）策略与用量都还在。
    let limits = SharedLimits::new(&root, config_with_tokens(1_000)).unwrap();
    let snapshot = limits.budget_snapshot().unwrap();
    assert_eq!(snapshot.max_total_tokens, Some(1_000));
    assert_eq!(snapshot.tokens_used, 600);
    assert_eq!(snapshot.calls_started, 1);
    assert!(matches!(
        limits.reserve_budget(500).await,
        Err(SharedLimitsError::BudgetExhausted { .. })
    ));

    // 完全未带预算参数的进程继承同一账本，不能重置用量或放宽策略。
    let inherited = SharedLimits::new(&root, SharedLimitsConfig::default()).unwrap();
    assert_eq!(inherited.config().max_total_tokens, Some(1_000));
    assert_eq!(inherited.budget_snapshot().unwrap().tokens_used, 600);
    assert!(matches!(
        SharedLimits::new(&root, config_with_tokens(2_000)),
        Err(SharedLimitsError::BudgetPolicyMismatch(_))
    ));
}

#[tokio::test]
async fn crashed_in_flight_reservation_is_not_refunded() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let ready = temp.path().join("ready");
    let mut child = spawn_child(
        &root,
        &ChildSpec {
            action: "reserve_hold",
            max_tokens: Some(1_000),
            tokens: 800,
            hold_ms: 60_000,
            signal: Some(ready.clone()),
            ..Default::default()
        },
    );
    assert!(
        wait_for_file(&ready, Duration::from_secs(20)).await,
        "child must reserve before the kill"
    );
    child.kill().await.unwrap();
    let _ = child.wait().await;

    // SIGKILL 不执行任何 Drop：预留必须保留在账本里，不能退款绕过共享预算。
    let limits = SharedLimits::new(&root, config_with_tokens(1_000)).unwrap();
    let snapshot = limits.budget_snapshot().unwrap();
    assert_eq!(
        snapshot.tokens_reserved_active, 800,
        "a crashed in-flight reservation must stay charged"
    );
    assert_eq!(snapshot.tokens_used, 0);
    assert_eq!(snapshot.calls_started, 1);
    assert!(matches!(
        limits.reserve_budget(300).await,
        Err(SharedLimitsError::BudgetExhausted { .. })
    ));
    assert!(
        limits.reserve_budget(200).await.unwrap().is_some(),
        "only the exact remaining budget is still usable"
    );
}

#[tokio::test]
async fn concurrent_first_creation_never_widens_the_recorded_budget() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let gate = temp.path().join("gate");
    let a_started = temp.path().join("a.started");
    let b_started = temp.path().join("b.started");
    let a_result = temp.path().join("a");
    let b_result = temp.path().join("b");

    let mut a = spawn_child(
        &root,
        &ChildSpec {
            action: "try",
            max_tokens: Some(1_000),
            tokens: 100,
            usage: Some(100),
            result: Some(a_result.clone()),
            started: Some(a_started.clone()),
            wait: Some(gate.clone()),
            ..Default::default()
        },
    );
    let mut b = spawn_child(
        &root,
        &ChildSpec {
            action: "try",
            max_tokens: Some(500),
            tokens: 100,
            usage: Some(100),
            result: Some(b_result.clone()),
            started: Some(b_started.clone()),
            wait: Some(gate.clone()),
            ..Default::default()
        },
    );
    assert!(wait_for_file(&a_started, Duration::from_secs(20)).await);
    assert!(wait_for_file(&b_started, Duration::from_secs(20)).await);
    std::fs::write(&gate, b"go").unwrap();

    let a_outcome = child_result(&mut a, &a_result).await;
    let b_outcome = child_result(&mut b, &b_result).await;
    // 更窄的一方必须成功；更宽的一方要么先创建（随后被收窄），要么 fail-closed。
    assert_eq!(b_outcome, "RESERVED");
    assert!(
        a_outcome == "RESERVED" || a_outcome == "MISMATCH",
        "unexpected wider-request outcome: {a_outcome}"
    );
    let snapshot = SharedLimits::new(&root, SharedLimitsConfig::default())
        .unwrap()
        .budget_snapshot()
        .unwrap();
    assert_eq!(
        snapshot.max_total_tokens,
        Some(500),
        "the narrower recorded budget must win"
    );
    assert!(
        snapshot.tokens_used <= 200,
        "only the two admitted reservations may be charged, got {}",
        snapshot.tokens_used
    );
}
