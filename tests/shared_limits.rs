//! `shared_limits` 的针对性测试。
//!
//! 覆盖：配置校验、权限 / symlink 拒绝、不可变策略文件（版本化、原子首建、一致性
//! fail-closed）、in-process 串行与取消、cooldown 合并与 fail-closed，以及通过当前
//! 测试程序子进程验证的跨进程共享、进程退出释放、cooldown 同步与并发首建竞争。
//!
//! 子进程模式复用同一个测试二进制：父进程以 `--exact shared_limits_child_entrypoint`
//! 重新执行自身，并通过环境变量下发动作；不使用任何真实模型服务。

#[path = "../src/shared_limits.rs"]
mod shared_limits;

use shared_limits::{SharedLimits, SharedLimitsConfig, SharedLimitsError};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
};

const ENV_ACTION: &str = "MIO_SL_ACTION";
const ENV_ROOT: &str = "MIO_SL_ROOT";
const ENV_GLOBAL: &str = "MIO_SL_GLOBAL";
const ENV_PROVIDER: &str = "MIO_SL_PROVIDER";
const ENV_HEAVY: &str = "MIO_SL_HEAVY";
const ENV_PROVIDER_KEY: &str = "MIO_SL_PROVIDER_KEY";
const ENV_HOLD_MS: &str = "MIO_SL_HOLD_MS";
const ENV_TIMEOUT_MS: &str = "MIO_SL_TIMEOUT_MS";
const ENV_DEFER_MS: &str = "MIO_SL_DEFER_MS";
const ENV_SIGNAL: &str = "MIO_SL_SIGNAL";
const ENV_RESULT: &str = "MIO_SL_RESULT";
const ENV_STARTED: &str = "MIO_SL_STARTED";
const ENV_WAIT: &str = "MIO_SL_WAIT";
const ENV_WORKSPACE: &str = "MIO_SL_WORKSPACE";
const ENV_WRITE: &str = "MIO_SL_WRITE";

// ---------------------------------------------------------------------------
// 子进程入口
// ---------------------------------------------------------------------------

/// 供父进程复用的子进程入口；在普通测试运行中因缺少环境变量而直接返回。
#[test]
fn shared_limits_child_entrypoint() {
    let Ok(action) = std::env::var(ENV_ACTION) else {
        return;
    };
    if env_string(ENV_ROOT).is_none() {
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build child runtime");
    if let Err(err) = runtime.block_on(run_child(&action)) {
        eprintln!("shared_limits child failed: {err}");
        std::process::exit(2);
    }
    std::process::exit(0);
}

async fn run_child(action: &str) -> Result<(), Box<dyn std::error::Error>> {
    let root = env_string(ENV_ROOT).ok_or("missing MIO_SL_ROOT")?;
    let config = SharedLimitsConfig {
        global_model_concurrency: env_usize(ENV_GLOBAL, 2),
        provider_model_concurrency: env_usize(ENV_PROVIDER, 2),
        heavy_concurrency: env_usize(ENV_HEAVY, 2),
        ..SharedLimitsConfig::default()
    };
    // 可选的同步闸门：先报告 STARTED，再等待 gate 文件出现，用于制造真实首建竞争。
    if let Some(gate) = env_string(ENV_WAIT) {
        if let Some(started) = env_string(ENV_STARTED) {
            write_atomic(Path::new(&started), b"STARTED")?;
        }
        while !Path::new(&gate).exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    match action {
        // 只做策略初始化（含原子首建竞争），不获取槽位。
        "init_policy" => {
            let outcome = match SharedLimits::new(&root, config) {
                Ok(_) => "OK",
                Err(SharedLimitsError::PolicyMismatch(_)) => "MISMATCH",
                Err(SharedLimitsError::CorruptPolicy(_)) => "CORRUPT",
                Err(_) => "ERROR",
            };
            signal(ENV_RESULT, outcome)?;
        }
        _ => {
            let limits = SharedLimits::new(&root, config)?;
            match action {
                "hold_heavy" => {
                    let permit = limits.acquire_heavy().await?;
                    signal(ENV_SIGNAL, "READY")?;
                    tokio::time::sleep(Duration::from_millis(env_u64(ENV_HOLD_MS, 1_000))).await;
                    drop(permit);
                }
                "try_heavy" => {
                    let outcome = match tokio::time::timeout(
                        Duration::from_millis(env_u64(ENV_TIMEOUT_MS, 1_000)),
                        limits.acquire_heavy(),
                    )
                    .await
                    {
                        Ok(Ok(_permit)) => "ACQUIRED",
                        Ok(Err(_)) => "ERROR",
                        Err(_) => "TIMEOUT",
                    };
                    signal(ENV_RESULT, outcome)?;
                }
                "hold_model" => {
                    let key = env_string(ENV_PROVIDER_KEY).ok_or("missing MIO_SL_PROVIDER_KEY")?;
                    let permit = limits.acquire_model(&key).await?;
                    signal(ENV_SIGNAL, "READY")?;
                    tokio::time::sleep(Duration::from_millis(env_u64(ENV_HOLD_MS, 1_000))).await;
                    drop(permit);
                }
                "try_model" => {
                    let key = env_string(ENV_PROVIDER_KEY).ok_or("missing MIO_SL_PROVIDER_KEY")?;
                    let outcome = match tokio::time::timeout(
                        Duration::from_millis(env_u64(ENV_TIMEOUT_MS, 1_000)),
                        limits.acquire_model(&key),
                    )
                    .await
                    {
                        Ok(Ok(_permit)) => "ACQUIRED",
                        Ok(Err(_)) => "ERROR",
                        Err(_) => "TIMEOUT",
                    };
                    signal(ENV_RESULT, outcome)?;
                }
                "defer" => {
                    let key = env_string(ENV_PROVIDER_KEY).ok_or("missing MIO_SL_PROVIDER_KEY")?;
                    limits
                        .defer_provider(&key, Duration::from_millis(env_u64(ENV_DEFER_MS, 0)))
                        .await?;
                    signal(ENV_RESULT, "DEFERRED")?;
                }
                "hold_workspace" => {
                    let path = env_string(ENV_WORKSPACE).ok_or("missing MIO_SL_WORKSPACE")?;
                    let write = env_string(ENV_WRITE).as_deref() == Some("1");
                    let permit = limits.acquire_workspace(Path::new(&path), write).await?;
                    signal(ENV_SIGNAL, "READY")?;
                    tokio::time::sleep(Duration::from_millis(env_u64(ENV_HOLD_MS, 1_000))).await;
                    drop(permit);
                }
                "try_workspace" => {
                    let path = env_string(ENV_WORKSPACE).ok_or("missing MIO_SL_WORKSPACE")?;
                    let write = env_string(ENV_WRITE).as_deref() == Some("1");
                    let outcome = match tokio::time::timeout(
                        Duration::from_millis(env_u64(ENV_TIMEOUT_MS, 1_000)),
                        limits.acquire_workspace(Path::new(&path), write),
                    )
                    .await
                    {
                        Ok(Ok(_permit)) => "ACQUIRED",
                        Ok(Err(_)) => "ERROR",
                        Err(_) => "TIMEOUT",
                    };
                    signal(ENV_RESULT, outcome)?;
                }
                other => return Err(format!("unknown child action: {other}").into()),
            }
        }
    }
    Ok(())
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

// ---------------------------------------------------------------------------
// 父进程辅助
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ChildSpec {
    action: &'static str,
    provider_key: Option<String>,
    hold_ms: u64,
    timeout_ms: u64,
    defer_ms: u64,
    signal: Option<PathBuf>,
    result: Option<PathBuf>,
    started: Option<PathBuf>,
    wait: Option<PathBuf>,
    workspace: Option<String>,
    write: bool,
}

fn spawn_child(
    root: &Path,
    spec: &ChildSpec,
    global: usize,
    provider: usize,
    heavy: usize,
) -> Child {
    let exe = std::env::current_exe().expect("current test executable");
    let mut command = Command::new(exe);
    command
        .arg("--exact")
        .arg("shared_limits_child_entrypoint")
        .arg("--test-threads=1")
        .env(ENV_ACTION, spec.action)
        .env(ENV_ROOT, root)
        .env(ENV_GLOBAL, global.to_string())
        .env(ENV_PROVIDER, provider.to_string())
        .env(ENV_HEAVY, heavy.to_string())
        .env(
            ENV_PROVIDER_KEY,
            spec.provider_key.clone().unwrap_or_default(),
        )
        .env(ENV_HOLD_MS, spec.hold_ms.to_string())
        .env(ENV_TIMEOUT_MS, spec.timeout_ms.to_string())
        .env(ENV_DEFER_MS, spec.defer_ms.to_string())
        .env(ENV_SIGNAL, path_to_env(spec.signal.as_deref()))
        .env(ENV_RESULT, path_to_env(spec.result.as_deref()))
        .env(ENV_STARTED, path_to_env(spec.started.as_deref()))
        .env(ENV_WAIT, path_to_env(spec.wait.as_deref()))
        .env(ENV_WORKSPACE, spec.workspace.clone().unwrap_or_default())
        .env(ENV_WRITE, if spec.write { "1" } else { "0" })
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.spawn().expect("spawn shared_limits child")
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

async fn child_result(child: &mut Child, result: &Path) -> String {
    if !wait_for_file(result, Duration::from_secs(15)).await {
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
    let content = std::fs::read_to_string(result).unwrap_or_default();
    let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
    content.trim().to_string()
}

fn limits_with(root: &Path, global: usize, provider: usize, heavy: usize) -> SharedLimits {
    SharedLimits::new(
        root,
        SharedLimitsConfig {
            global_model_concurrency: global,
            provider_model_concurrency: provider,
            heavy_concurrency: heavy,
            ..SharedLimitsConfig::default()
        },
    )
    .expect("create shared limits")
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_string(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Some(value),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 配置与基础行为
// ---------------------------------------------------------------------------

#[test]
fn defaults_match_documented_values() {
    let config = SharedLimitsConfig::default();
    assert_eq!(config.global_model_concurrency, 2);
    assert_eq!(config.provider_model_concurrency, 2);
    assert_eq!(config.heavy_concurrency, 2);
    assert!(config.validate().is_ok());
}

#[test]
fn invalid_config_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let invalid = [
        SharedLimitsConfig {
            global_model_concurrency: 0,
            ..SharedLimitsConfig::default()
        },
        SharedLimitsConfig {
            provider_model_concurrency: 0,
            ..SharedLimitsConfig::default()
        },
        SharedLimitsConfig {
            heavy_concurrency: 0,
            ..SharedLimitsConfig::default()
        },
        SharedLimitsConfig {
            global_model_concurrency: 100_000,
            ..SharedLimitsConfig::default()
        },
        SharedLimitsConfig {
            max_cooldown_entries: 0,
            ..SharedLimitsConfig::default()
        },
        SharedLimitsConfig {
            max_cooldown_file_bytes: 8,
            ..SharedLimitsConfig::default()
        },
    ];
    for config in invalid {
        let result = SharedLimits::new(temp.path(), config);
        assert!(
            matches!(result, Err(SharedLimitsError::InvalidConfig(_))),
            "config should be rejected: {config:?}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn directories_and_files_are_private() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = SharedLimits::new(&root, SharedLimitsConfig::default()).unwrap();

    let root_mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
    assert_eq!(root_mode, 0o700, "root dir must be 0700");
    let locks_mode = std::fs::metadata(root.join("locks"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(locks_mode, 0o700, "locks dir must be 0700");

    let policy_mode = std::fs::metadata(root.join("policy.json"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(policy_mode, 0o600, "policy file must be 0600");

    let _permit = limits.acquire_heavy().await.unwrap();
    let slot_mode = std::fs::metadata(root.join("locks").join("heavy-0.lock"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(slot_mode, 0o600, "lock file must be 0600");

    limits
        .defer_provider("p", Duration::from_millis(100))
        .await
        .unwrap();
    let cooldown_mode = std::fs::metadata(root.join("cooldowns.json"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(cooldown_mode, 0o600, "cooldown file must be 0600");
}

#[cfg(unix)]
#[test]
fn symlinked_root_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = temp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(matches!(
        SharedLimits::new(&link, SharedLimitsConfig::default()),
        Err(SharedLimitsError::UnsafePath(_))
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_slot_file_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = limits_with(&root, 2, 2, 1);

    let target = temp.path().join("target");
    std::fs::write(&target, b"x").unwrap();
    let slot = root.join("locks").join("heavy-0.lock");
    std::os::unix::fs::symlink(&target, &slot).unwrap();

    let result = limits.acquire_heavy().await;
    assert!(
        matches!(
            result,
            Err(SharedLimitsError::Io(_)) | Err(SharedLimitsError::UnsafePath(_))
        ),
        "symlinked slot must be refused"
    );
}

// ---------------------------------------------------------------------------
// 不可变策略文件
// ---------------------------------------------------------------------------

#[test]
fn versioned_policy_file_is_durable_and_private() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    SharedLimits::new(&root, SharedLimitsConfig::default()).unwrap();

    let raw = std::fs::read_to_string(root.join("policy.json")).unwrap();
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(value["version"], 1);
    assert_eq!(value["global_model_concurrency"], 2);
    assert_eq!(value["provider_model_concurrency"], 2);
    assert_eq!(value["heavy_concurrency"], 2);
}

#[test]
fn same_root_different_config_is_rejected_in_process() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    SharedLimits::new(&root, SharedLimitsConfig::default()).unwrap();

    let different_global = SharedLimitsConfig {
        global_model_concurrency: 3,
        ..SharedLimitsConfig::default()
    };
    assert!(matches!(
        SharedLimits::new(&root, different_global),
        Err(SharedLimitsError::PolicyMismatch(_))
    ));
    let different_provider = SharedLimitsConfig {
        provider_model_concurrency: 3,
        ..SharedLimitsConfig::default()
    };
    assert!(matches!(
        SharedLimits::new(&root, different_provider),
        Err(SharedLimitsError::PolicyMismatch(_))
    ));
    let different_heavy = SharedLimitsConfig {
        heavy_concurrency: 3,
        ..SharedLimitsConfig::default()
    };
    assert!(matches!(
        SharedLimits::new(&root, different_heavy),
        Err(SharedLimitsError::PolicyMismatch(_))
    ));

    // 一致配置重复打开必须成功。
    SharedLimits::new(&root, SharedLimitsConfig::default()).unwrap();
}

#[test]
fn corrupt_policy_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    std::fs::create_dir_all(&root).unwrap();
    let policy = root.join("policy.json");

    std::fs::write(&policy, b"not-json").unwrap();
    assert!(matches!(
        SharedLimits::new(&root, SharedLimitsConfig::default()),
        Err(SharedLimitsError::CorruptPolicy(_))
    ));

    std::fs::write(
        &policy,
        br#"{"version":2,"global_model_concurrency":2,"provider_model_concurrency":2,"heavy_concurrency":2}"#,
    )
    .unwrap();
    assert!(matches!(
        SharedLimits::new(&root, SharedLimitsConfig::default()),
        Err(SharedLimitsError::CorruptPolicy(_))
    ));

    std::fs::write(
        &policy,
        br#"{"version":1,"global_model_concurrency":0,"provider_model_concurrency":2,"heavy_concurrency":2}"#,
    )
    .unwrap();
    assert!(matches!(
        SharedLimits::new(&root, SharedLimitsConfig::default()),
        Err(SharedLimitsError::CorruptPolicy(_))
    ));

    std::fs::write(&policy, vec![b'a'; 8192]).unwrap();
    assert!(matches!(
        SharedLimits::new(&root, SharedLimitsConfig::default()),
        Err(SharedLimitsError::CorruptPolicy(_))
    ));
}

#[cfg(unix)]
#[test]
fn symlinked_policy_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    std::fs::create_dir_all(&root).unwrap();
    let target = temp.path().join("target.json");
    std::fs::write(&target, b"{}").unwrap();
    std::os::unix::fs::symlink(&target, root.join("policy.json")).unwrap();
    assert!(matches!(
        SharedLimits::new(&root, SharedLimitsConfig::default()),
        Err(SharedLimitsError::UnsafePath(_))
    ));
}

#[tokio::test]
async fn concurrent_first_creation_with_different_configs_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let gate = temp.path().join("gate");
    let started_a = temp.path().join("a.started");
    let started_b = temp.path().join("b.started");
    let result_a = temp.path().join("a.txt");
    let result_b = temp.path().join("b.txt");

    let mut a = spawn_child(
        &root,
        &ChildSpec {
            action: "init_policy",
            result: Some(result_a.clone()),
            started: Some(started_a.clone()),
            wait: Some(gate.clone()),
            ..ChildSpec::default()
        },
        1,
        2,
        2,
    );
    let mut b = spawn_child(
        &root,
        &ChildSpec {
            action: "init_policy",
            result: Some(result_b.clone()),
            started: Some(started_b.clone()),
            wait: Some(gate.clone()),
            ..ChildSpec::default()
        },
        3,
        2,
        2,
    );

    // 两个子进程都就绪后同时放行，确保真的并发首建。
    assert!(wait_for_file(&started_a, Duration::from_secs(15)).await);
    assert!(wait_for_file(&started_b, Duration::from_secs(15)).await);
    std::fs::write(&gate, b"go").unwrap();

    let mut outcomes = vec![
        child_result(&mut a, &result_a).await,
        child_result(&mut b, &result_b).await,
    ];
    outcomes.sort();
    assert_eq!(
        outcomes,
        vec!["MISMATCH".to_string(), "OK".to_string()],
        "exactly one initializer may win; the other must fail closed"
    );

    // 胜出者的策略是唯一被持久化的，败者没有覆盖它。
    let raw = std::fs::read_to_string(root.join("policy.json")).unwrap();
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let persisted = value["global_model_concurrency"].as_u64().unwrap();
    assert!(
        persisted == 1 || persisted == 3,
        "policy must match one of the racers, got {persisted}"
    );
    assert_eq!(value["version"], 1);
}

#[tokio::test]
async fn concurrent_first_creation_with_same_config_both_succeed() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let gate = temp.path().join("gate");
    let started_a = temp.path().join("a.started");
    let started_b = temp.path().join("b.started");
    let result_a = temp.path().join("a.txt");
    let result_b = temp.path().join("b.txt");

    let mut a = spawn_child(
        &root,
        &ChildSpec {
            action: "init_policy",
            result: Some(result_a.clone()),
            started: Some(started_a.clone()),
            wait: Some(gate.clone()),
            ..ChildSpec::default()
        },
        2,
        2,
        2,
    );
    let mut b = spawn_child(
        &root,
        &ChildSpec {
            action: "init_policy",
            result: Some(result_b.clone()),
            started: Some(started_b.clone()),
            wait: Some(gate.clone()),
            ..ChildSpec::default()
        },
        2,
        2,
        2,
    );

    assert!(wait_for_file(&started_a, Duration::from_secs(15)).await);
    assert!(wait_for_file(&started_b, Duration::from_secs(15)).await);
    std::fs::write(&gate, b"go").unwrap();

    assert_eq!(child_result(&mut a, &result_a).await, "OK");
    assert_eq!(child_result(&mut b, &result_b).await, "OK");

    let raw = std::fs::read_to_string(root.join("policy.json")).unwrap();
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(value["version"], 1);
    assert_eq!(value["global_model_concurrency"], 2);
    assert_eq!(value["provider_model_concurrency"], 2);
    assert_eq!(value["heavy_concurrency"], 2);
}

// ---------------------------------------------------------------------------
// In-process 语义
// ---------------------------------------------------------------------------

#[tokio::test]
async fn model_global_slot_is_shared_in_process() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = limits_with(&root, 1, 2, 2);

    let permit = limits.acquire_model("alpha").await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(250), limits.acquire_model("beta"))
            .await
            .is_err(),
        "second request must wait for the global slot"
    );
    drop(permit);
    assert!(
        tokio::time::timeout(Duration::from_secs(2), limits.acquire_model("beta"))
            .await
            .is_ok(),
        "global slot must be reusable after drop"
    );
}

#[tokio::test]
async fn provider_slot_is_per_provider() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = limits_with(&root, 4, 1, 2);

    let first = limits.acquire_model("alpha").await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(250), limits.acquire_model("alpha"))
            .await
            .is_err(),
        "same provider must be serialized"
    );
    let other = tokio::time::timeout(Duration::from_secs(2), limits.acquire_model("beta"))
        .await
        .expect("other provider should not wait")
        .unwrap();
    drop(first);
    drop(other);
}

#[tokio::test]
async fn heavy_slots_are_serial_in_process() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = limits_with(&root, 2, 2, 1);

    let first = limits.acquire_heavy().await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(250), limits.acquire_heavy())
            .await
            .is_err(),
        "heavy must be serial"
    );
    drop(first);
    assert!(
        tokio::time::timeout(Duration::from_secs(2), limits.acquire_heavy())
            .await
            .is_ok(),
        "heavy slot must be reusable after drop"
    );
}

#[tokio::test]
async fn cancelled_wait_does_not_leak_slots() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = limits_with(&root, 1, 1, 1);

    let heavy = limits.acquire_heavy().await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(120), limits.acquire_heavy())
            .await
            .is_err()
    );
    drop(heavy);
    assert!(
        tokio::time::timeout(Duration::from_secs(2), limits.acquire_heavy())
            .await
            .is_ok(),
        "cancelled waiter must not hold the heavy slot"
    );

    let model = limits.acquire_model("alpha").await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(120), limits.acquire_model("alpha"))
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(120), limits.acquire_model("beta"))
            .await
            .is_err()
    );
    drop(model);
    assert!(
        tokio::time::timeout(Duration::from_secs(2), limits.acquire_model("beta"))
            .await
            .is_ok(),
        "cancelled model waiters must not leak global/provider slots"
    );
}

// ---------------------------------------------------------------------------
// cooldown
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cooldown_keeps_max_deadline_and_is_per_provider() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = SharedLimits::new(&root, SharedLimitsConfig::default()).unwrap();

    limits
        .defer_provider("p", Duration::from_millis(500))
        .await
        .unwrap();
    limits
        .defer_provider("p", Duration::from_millis(50))
        .await
        .unwrap();

    let hash = SharedLimits::provider_key_hash("p");
    let snapshot = limits.snapshot().unwrap();
    let entry = snapshot
        .cooldowns
        .iter()
        .find(|entry| entry.provider_hash == hash)
        .expect("p cooldown present");
    assert!(
        entry.remaining >= Duration::from_millis(300),
        "max deadline must win, got {:?}",
        entry.remaining
    );

    limits
        .defer_provider("q", Duration::from_millis(500))
        .await
        .unwrap();
    let snapshot = limits.snapshot().unwrap();
    let q_hash = SharedLimits::provider_key_hash("q");
    assert!(
        snapshot
            .cooldowns
            .iter()
            .any(|entry| entry.provider_hash == q_hash)
    );
    assert!(
        snapshot
            .cooldowns
            .iter()
            .any(|entry| entry.provider_hash == hash)
    );
}

#[tokio::test]
async fn model_acquire_honors_provider_cooldown() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = SharedLimits::new(&root, SharedLimitsConfig::default()).unwrap();
    limits
        .defer_provider("p", Duration::from_millis(500))
        .await
        .unwrap();

    let start = std::time::Instant::now();
    let permit = limits.acquire_model("p").await.unwrap();
    assert!(
        start.elapsed() >= Duration::from_millis(300),
        "acquire must respect cooldown, elapsed {:?}",
        start.elapsed()
    );
    drop(permit);
}

#[tokio::test]
async fn corrupt_cooldown_file_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = SharedLimits::new(&root, SharedLimitsConfig::default()).unwrap();
    let cooldown = root.join("cooldowns.json");

    std::fs::write(&cooldown, b"").unwrap();
    assert!(matches!(
        limits.snapshot(),
        Err(SharedLimitsError::CorruptCooldown(_))
    ));
    std::fs::write(&cooldown, b"not-json").unwrap();
    assert!(matches!(
        limits.snapshot(),
        Err(SharedLimitsError::CorruptCooldown(_))
    ));
    assert!(matches!(
        limits.defer_provider("p", Duration::from_millis(10)).await,
        Err(SharedLimitsError::CorruptCooldown(_))
    ));

    std::fs::write(&cooldown, br#"{"shortkey":123}"#).unwrap();
    assert!(matches!(
        limits.snapshot(),
        Err(SharedLimitsError::CorruptCooldown(_))
    ));

    let oversized =
        vec![b'a'; (SharedLimitsConfig::default().max_cooldown_file_bytes + 1) as usize];
    std::fs::write(&cooldown, oversized).unwrap();
    assert!(matches!(
        limits.snapshot(),
        Err(SharedLimitsError::CorruptCooldown(_))
    ));
}

#[tokio::test]
async fn snapshot_returns_config_and_cooldowns_only() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = SharedLimits::new(&root, SharedLimitsConfig::default()).unwrap();
    let _permit = limits.acquire_heavy().await.unwrap();

    let snapshot = limits.snapshot().unwrap();
    assert_eq!(snapshot.config, SharedLimitsConfig::default());
    assert!(snapshot.cooldowns.is_empty());
    assert_eq!(limits.config(), SharedLimitsConfig::default());

    // 状态输出可序列化为 JSON（无新增依赖）。
    let json = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(json["config"]["heavy_concurrency"], 2);
    assert!(json["cooldowns"].is_array());
}

#[tokio::test]
async fn raw_provider_key_never_touches_disk() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = limits_with(&root, 2, 2, 1);
    let secret = "sk-SUPER-SECRET-endpoint.example.com/tenant-42";

    limits
        .defer_provider(secret, Duration::from_millis(500))
        .await
        .unwrap();
    let _permit = limits.acquire_model(secret).await.unwrap();

    let hash = SharedLimits::provider_key_hash(secret);
    assert_eq!(hash.len(), 64);
    assert!(hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert!(
        root.join("locks")
            .join(format!("model-provider-{hash}-0.lock"))
            .exists(),
        "provider slot file must use the SHA-256 hash"
    );

    let mut files = Vec::new();
    collect_files(&root, &mut files);
    assert!(!files.is_empty());
    for file in &files {
        let bytes = std::fs::read(file).unwrap();
        assert!(
            !contains_subslice(&bytes, secret.as_bytes()),
            "raw provider key leaked into {}",
            file.display()
        );
        let name = file.file_name().unwrap().to_string_lossy();
        assert!(
            !name.contains("SECRET"),
            "raw provider key leaked into filename {name}"
        );
    }
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_files(&path, out);
            } else {
                out.push(path);
            }
        }
    }
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

// ---------------------------------------------------------------------------
// 跨进程
// ---------------------------------------------------------------------------

#[tokio::test]
async fn heavy_slot_is_shared_across_processes_and_released_on_exit() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = limits_with(&root, 2, 2, 1);

    let ready = temp.path().join("holder.ready");
    let mut holder = spawn_child(
        &root,
        &ChildSpec {
            action: "hold_heavy",
            hold_ms: 30_000,
            signal: Some(ready.clone()),
            ..ChildSpec::default()
        },
        2,
        2,
        1,
    );
    assert!(
        wait_for_file(&ready, Duration::from_secs(15)).await,
        "holder child never acquired the heavy slot"
    );

    assert!(
        tokio::time::timeout(Duration::from_millis(400), limits.acquire_heavy())
            .await
            .is_err(),
        "parent must not acquire a slot held by another process"
    );

    holder.kill().await.ok();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), limits.acquire_heavy())
            .await
            .is_ok(),
        "kernel must release the flock when the holder process dies"
    );
}

#[tokio::test]
async fn model_provider_slot_is_shared_across_processes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");

    let ready = temp.path().join("alpha.ready");
    let mut alpha = spawn_child(
        &root,
        &ChildSpec {
            action: "hold_model",
            provider_key: Some("alpha".into()),
            hold_ms: 30_000,
            signal: Some(ready.clone()),
            ..ChildSpec::default()
        },
        4,
        1,
        2,
    );
    assert!(
        wait_for_file(&ready, Duration::from_secs(15)).await,
        "alpha holder never acquired its slots"
    );

    let blocked_result = temp.path().join("alpha_blocked.txt");
    let mut blocked = spawn_child(
        &root,
        &ChildSpec {
            action: "try_model",
            provider_key: Some("alpha".into()),
            timeout_ms: 500,
            result: Some(blocked_result.clone()),
            ..ChildSpec::default()
        },
        4,
        1,
        2,
    );
    assert_eq!(child_result(&mut blocked, &blocked_result).await, "TIMEOUT");

    let other_result = temp.path().join("beta.txt");
    let mut other = spawn_child(
        &root,
        &ChildSpec {
            action: "try_model",
            provider_key: Some("beta".into()),
            timeout_ms: 5_000,
            result: Some(other_result.clone()),
            ..ChildSpec::default()
        },
        4,
        1,
        2,
    );
    assert_eq!(child_result(&mut other, &other_result).await, "ACQUIRED");

    alpha.kill().await.ok();
    let after_result = temp.path().join("alpha_after_kill.txt");
    let mut after = spawn_child(
        &root,
        &ChildSpec {
            action: "try_model",
            provider_key: Some("alpha".into()),
            timeout_ms: 5_000,
            result: Some(after_result.clone()),
            ..ChildSpec::default()
        },
        4,
        1,
        2,
    );
    assert_eq!(child_result(&mut after, &after_result).await, "ACQUIRED");
}

#[tokio::test]
async fn cooldown_updates_sync_across_processes_and_take_max() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = SharedLimits::new(&root, SharedLimitsConfig::default()).unwrap();

    let long_result = temp.path().join("long.txt");
    let short_result = temp.path().join("short.txt");
    let mut long = spawn_child(
        &root,
        &ChildSpec {
            action: "defer",
            provider_key: Some("shared".into()),
            defer_ms: 900,
            result: Some(long_result.clone()),
            ..ChildSpec::default()
        },
        2,
        2,
        2,
    );
    let mut short = spawn_child(
        &root,
        &ChildSpec {
            action: "defer",
            provider_key: Some("shared".into()),
            defer_ms: 150,
            result: Some(short_result.clone()),
            ..ChildSpec::default()
        },
        2,
        2,
        2,
    );
    assert_eq!(child_result(&mut long, &long_result).await, "DEFERRED");
    assert_eq!(child_result(&mut short, &short_result).await, "DEFERRED");

    let shared_hash = SharedLimits::provider_key_hash("shared");
    let snapshot = limits.snapshot().unwrap();
    let entry = snapshot
        .cooldowns
        .iter()
        .find(|entry| entry.provider_hash == shared_hash)
        .expect("shared cooldown present");
    assert!(
        entry.remaining >= Duration::from_millis(500),
        "concurrent cross-process updates must take the max deadline, got {:?}",
        entry.remaining
    );

    let other_hash = SharedLimits::provider_key_hash("other");
    assert!(
        snapshot
            .cooldowns
            .iter()
            .all(|entry| entry.provider_hash != other_hash),
        "different providers must stay independent"
    );
}

// ---------------------------------------------------------------------------
// workspace 作用域租约
// ---------------------------------------------------------------------------

fn make_dir(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
}

/// 返回 `(parent, parent/child, sibling)` 三棵已创建的目录。
fn workspace_tree(temp: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let parent = temp.join("ws-parent");
    let child = parent.join("ws-child");
    let sibling = temp.join("ws-sibling");
    make_dir(&child);
    make_dir(&sibling);
    (parent, child, sibling)
}

#[tokio::test]
async fn workspace_read_read_is_concurrent() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = SharedLimits::new(&root, SharedLimitsConfig::default()).unwrap();
    let dir = temp.path().join("dir");
    make_dir(&dir);

    let first = limits.acquire_workspace(&dir, false).await.unwrap();
    let second = tokio::time::timeout(
        Duration::from_secs(2),
        limits.acquire_workspace(&dir, false),
    )
    .await
    .expect("read-read must not block")
    .unwrap();
    drop(first);
    drop(second);
}

#[tokio::test]
async fn workspace_disjoint_writes_are_concurrent() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = SharedLimits::new(&root, SharedLimitsConfig::default()).unwrap();
    let dir_a = temp.path().join("dir-a");
    let dir_b = temp.path().join("dir-b");
    make_dir(&dir_a);
    make_dir(&dir_b);

    let first = limits.acquire_workspace(&dir_a, true).await.unwrap();
    let second = tokio::time::timeout(
        Duration::from_secs(2),
        limits.acquire_workspace(&dir_b, true),
    )
    .await
    .expect("disjoint writes must not block")
    .unwrap();
    drop(first);
    drop(second);
}

#[tokio::test]
async fn workspace_parent_writer_blocks_child_access() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = SharedLimits::new(&root, SharedLimitsConfig::default()).unwrap();
    let (parent, child, _sibling) = workspace_tree(temp.path());

    let writer = limits.acquire_workspace(&parent, true).await.unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(300),
            limits.acquire_workspace(&child, false)
        )
        .await
        .is_err(),
        "parent writer must block child reader"
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(300),
            limits.acquire_workspace(&child, true)
        )
        .await
        .is_err(),
        "parent writer must block child writer"
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(300),
            limits.acquire_workspace(&parent, false)
        )
        .await
        .is_err(),
        "writer must block a reader on the same path"
    );

    drop(writer);
    assert!(
        tokio::time::timeout(
            Duration::from_secs(2),
            limits.acquire_workspace(&child, false)
        )
        .await
        .is_ok(),
        "child reader must proceed once the parent writer is gone"
    );
}

#[tokio::test]
async fn workspace_cancelled_wait_does_not_leak() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = SharedLimits::new(&root, SharedLimitsConfig::default()).unwrap();
    let (parent, child, _sibling) = workspace_tree(temp.path());

    let writer = limits.acquire_workspace(&parent, true).await.unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(120),
            limits.acquire_workspace(&child, true)
        )
        .await
        .is_err()
    );
    drop(writer);
    assert!(
        tokio::time::timeout(
            Duration::from_secs(2),
            limits.acquire_workspace(&child, true)
        )
        .await
        .is_ok(),
        "a cancelled workspace waiter must not leave a claim behind"
    );
}

#[tokio::test]
async fn workspace_cross_process_parent_writer_blocks_child() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = SharedLimits::new(&root, SharedLimitsConfig::default()).unwrap();
    let (parent, child, sibling) = workspace_tree(temp.path());

    let ready = temp.path().join("ws.ready");
    let mut holder = spawn_child(
        &root,
        &ChildSpec {
            action: "hold_workspace",
            workspace: Some(parent.to_string_lossy().into_owned()),
            write: true,
            hold_ms: 30_000,
            signal: Some(ready.clone()),
            ..ChildSpec::default()
        },
        2,
        2,
        2,
    );
    assert!(
        wait_for_file(&ready, Duration::from_secs(15)).await,
        "workspace holder never acquired its lease"
    );

    assert!(
        tokio::time::timeout(
            Duration::from_millis(400),
            limits.acquire_workspace(&child, false)
        )
        .await
        .is_err(),
        "cross-process parent writer must block child reader"
    );
    assert!(
        tokio::time::timeout(
            Duration::from_secs(2),
            limits.acquire_workspace(&sibling, true)
        )
        .await
        .is_ok(),
        "disjoint directory must stay available across processes"
    );

    holder.kill().await.ok();
    assert!(
        tokio::time::timeout(
            Duration::from_secs(5),
            limits.acquire_workspace(&child, false)
        )
        .await
        .is_ok(),
        "child reader must proceed after the holder process dies"
    );
}

#[tokio::test]
async fn workspace_stale_claim_reclaimed_after_holder_kill() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = SharedLimits::new(&root, SharedLimitsConfig::default()).unwrap();
    let dir = temp.path().join("dir");
    make_dir(&dir);

    let ready = temp.path().join("stale.ready");
    let mut holder = spawn_child(
        &root,
        &ChildSpec {
            action: "hold_workspace",
            workspace: Some(dir.to_string_lossy().into_owned()),
            write: true,
            hold_ms: 30_000,
            signal: Some(ready.clone()),
            ..ChildSpec::default()
        },
        2,
        2,
        2,
    );
    assert!(wait_for_file(&ready, Duration::from_secs(15)).await);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(300),
            limits.acquire_workspace(&dir, true)
        )
        .await
        .is_err(),
        "live holder must block a conflicting writer"
    );

    holder.kill().await.ok();
    let reclaimed =
        tokio::time::timeout(Duration::from_secs(5), limits.acquire_workspace(&dir, true))
            .await
            .expect("stale claim must be reclaimed after SIGKILL")
            .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(300),
            limits.acquire_workspace(&dir, true)
        )
        .await
        .is_err(),
        "the newly acquired live claim must itself block conflicts"
    );
    drop(reclaimed);
}

#[tokio::test]
async fn workspace_corrupt_or_oversized_registry_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = SharedLimits::new(&root, SharedLimitsConfig::default()).unwrap();
    let registry = root.join("workspace").join("registry.json");
    let dir = temp.path().join("dir");
    make_dir(&dir);

    std::fs::write(&registry, b"").unwrap();
    assert!(matches!(
        limits.acquire_workspace(&dir, false).await,
        Err(SharedLimitsError::CorruptWorkspace(_))
    ));
    std::fs::write(&registry, b"not-json").unwrap();
    assert!(matches!(
        limits.acquire_workspace(&dir, false).await,
        Err(SharedLimitsError::CorruptWorkspace(_))
    ));

    std::fs::write(&registry, br#"{"version":99,"claims":[]}"#).unwrap();
    assert!(matches!(
        limits.acquire_workspace(&dir, false).await,
        Err(SharedLimitsError::CorruptWorkspace(_))
    ));

    let claims: Vec<serde_json::Value> = (0..5000)
        .map(|index| {
            serde_json::json!({
                "id": format!("claim{index}"),
                "components": ["0000000000000000000000000000dead"],
                "write": false,
            })
        })
        .collect();
    let too_many = serde_json::json!({ "version": 1, "claims": claims });
    std::fs::write(&registry, serde_json::to_vec(&too_many).unwrap()).unwrap();
    assert!(matches!(
        limits.acquire_workspace(&dir, false).await,
        Err(SharedLimitsError::CorruptWorkspace(_))
    ));

    std::fs::write(&registry, vec![b'a'; 1024 * 1024 + 1]).unwrap();
    assert!(matches!(
        limits.acquire_workspace(&dir, false).await,
        Err(SharedLimitsError::CorruptWorkspace(_))
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn workspace_symlinked_registry_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = SharedLimits::new(&root, SharedLimitsConfig::default()).unwrap();
    let dir = temp.path().join("dir");
    make_dir(&dir);

    let target = temp.path().join("target.json");
    std::fs::write(&target, b"{}").unwrap();
    std::os::unix::fs::symlink(&target, root.join("workspace").join("registry.json")).unwrap();
    assert!(matches!(
        limits.acquire_workspace(&dir, false).await,
        Err(SharedLimitsError::UnsafePath(_))
    ));
}

#[tokio::test]
async fn workspace_registry_does_not_leak_raw_path() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("limits");
    let limits = SharedLimits::new(&root, SharedLimitsConfig::default()).unwrap();
    let secret = temp.path().join("TOP-SECRET-WORKSPACE-xyz");
    make_dir(&secret);

    let _permit = limits.acquire_workspace(&secret, false).await.unwrap();
    let registry = std::fs::read_to_string(root.join("workspace").join("registry.json")).unwrap();
    assert!(
        !registry.contains("TOP-SECRET-WORKSPACE-xyz"),
        "registry must not contain the raw path"
    );
    assert!(
        !registry.contains("SECRET"),
        "registry leaked raw path text"
    );
}
