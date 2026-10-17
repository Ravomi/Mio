//! CLI / RPC 去重命中响应的保守 liveness 验收。
//!
//! 全部为真实跨进程用例：测试进程写入工作区账本或启动 owner 子进程，再启动真实
//! `mio` CLI / RPC 子进程去查询。不触网、不调用真实 Provider：去重命中不发起模型
//! 请求，不可达的 `--base-url` 正好用来证明没有重放。每个子进程都在结束时
//! kill + wait，不留后台进程。
//!
//! 覆盖：重复 key 的 CLI `task` 事件、空闲态 RPC `prompt`/`follow_up` 命中、活动
//! 回合里的重复 `prompt`/`follow_up` 命中，且 `stale` 不会被自动重放。

use mio_coding_agent::dedup::{LEDGER_DIR, LEDGER_FILE};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;

#[cfg(target_os = "linux")]
use mio_coding_agent::dedup::capture_owner_for_pid;
#[cfg(target_os = "linux")]
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener},
    process::{Child, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

/// 不可达的本地端点：任何真实模型请求都会失败，用来证明去重命中没有重放任务。
const UNREACHABLE: &str = "http://127.0.0.1:1/v1";

fn cli(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mio"));
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.starts_with("MIO_")
            || name.contains("API_KEY")
            || name.to_lowercase().contains("proxy")
        {
            command.env_remove(key);
        }
    }
    command
        .arg("--cwd")
        .arg(root)
        .env("MIO_CONFIG_DIR", root.join("config"))
        .env("MIO_PROVIDER", "custom")
        .env("MIO_API_KEY", "local-test-credential")
        .args(["--model", "test", "--provider", "custom"]);
    command
}

fn record(task_id: &str, state: &str, key: &str, owner: Option<Value>) -> Value {
    let mut record = json!({"task_id": task_id, "state": state, "key": key, "updated": 0});
    if let Some(owner) = owner {
        record["owner"] = owner;
    }
    record
}

fn write_ledger(root: &Path, entries: &Value) {
    let dir = root.join(LEDGER_DIR);
    std::fs::create_dir_all(&dir).unwrap();
    let payload = json!({"version": 1, "updated": 0, "entries": entries});
    std::fs::write(dir.join(LEDGER_FILE), payload.to_string()).unwrap();
}

fn ledger_bytes(root: &Path) -> Vec<u8> {
    std::fs::read(root.join(LEDGER_DIR).join(LEDGER_FILE)).unwrap()
}

fn parse_json_lines(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("非 JSON 输出行 {line:?}: {error}"))
        })
        .collect()
}

fn run_cli_task(root: &Path, key: &str) -> Value {
    let output = cli(root)
        .args([
            "--base-url",
            UNREACHABLE,
            "--json",
            "--dedup-key",
            key,
            "-p",
            "dedup probe",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    parse_json_lines(&output.stdout)
        .into_iter()
        .find(|event| event["type"] == "task")
        .expect("task event")
}

#[test]
fn cli_reused_task_event_reports_unknown_for_terminal_record() {
    let dir = tempfile::tempdir().unwrap();
    let task_id = uuid::Uuid::new_v4().to_string();
    write_ledger(
        dir.path(),
        &json!([record(&task_id, "completed", "done-job", None)]),
    );
    let before = ledger_bytes(dir.path());

    // 终态记录不参与 owner 探测：只能如实报 unknown，不猜 live/stale。
    let task = run_cli_task(dir.path(), "done-job");
    assert_eq!(task["reused"], true);
    assert_eq!(task["task_id"], task_id);
    assert_eq!(task["state"], "completed");
    assert_eq!(task["liveness"], "unknown");

    // 查询只读：命中不会改写记录（既不改 state，也不删除 key）。
    assert_eq!(
        ledger_bytes(dir.path()),
        before,
        "去重命中查询不得改写账本记录"
    );
}

#[cfg(target_os = "linux")]
struct ChildGuard(Child);

#[cfg(target_os = "linux")]
impl ChildGuard {
    fn spawn_sleep() -> Self {
        Self(
            Command::new("sleep")
                .arg("120")
                .spawn()
                .expect("spawn sleep"),
        )
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
#[test]
fn cli_reused_task_event_reports_live_and_stale_owner() {
    let dir = tempfile::tempdir().unwrap();
    // owner 是一个真实存活的子进程：同 host、同 boot、同 PID namespace。
    let child = ChildGuard::spawn_sleep();
    let owner = capture_owner_for_pid(child.pid()).expect("live child identity");

    let live_id = uuid::Uuid::new_v4().to_string();
    write_ledger(
        dir.path(),
        &json!([record(
            &live_id,
            "active",
            "live-job",
            Some(serde_json::to_value(&owner).unwrap())
        )]),
    );

    let live = run_cli_task(dir.path(), "live-job");
    assert_eq!(live["reused"], true);
    assert_eq!(live["task_id"], live_id);
    assert_eq!(live["state"], "active");
    assert_eq!(live["liveness"], "live");

    // stale 不等于可重放：查询后记录状态与 owner 都保持原样。
    let stored: Value = serde_json::from_slice(&ledger_bytes(dir.path())).unwrap();
    assert_eq!(stored["entries"][0]["state"], "active");
    assert_eq!(stored["entries"][0]["task_id"], live_id);

    // 同一个 PID 仍然存在，但启动时刻不同：原 owner 的身份已被替换。
    let mut reused_owner = owner.clone();
    reused_owner.start_time_ticks = reused_owner.start_time_ticks.wrapping_add(1);
    let stale_id = uuid::Uuid::new_v4().to_string();
    write_ledger(
        dir.path(),
        &json!([record(
            &stale_id,
            "queued",
            "stale-job",
            Some(serde_json::to_value(&reused_owner).unwrap())
        )]),
    );
    let before = ledger_bytes(dir.path());

    let stale = run_cli_task(dir.path(), "stale-job");
    assert_eq!(stale["reused"], true);
    assert_eq!(stale["task_id"], stale_id);
    assert_eq!(stale["state"], "queued");
    assert_eq!(stale["liveness"], "stale");
    assert_eq!(
        ledger_bytes(dir.path()),
        before,
        "stale 命中仍不得改写或删除记录"
    );
}

#[cfg(target_os = "linux")]
struct RpcChild {
    child: Child,
    events: mpsc::Receiver<Value>,
}

#[cfg(target_os = "linux")]
impl RpcChild {
    fn spawn(root: &Path, base_url: &str) -> Self {
        let mut command = cli(root);
        command.args([
            "--base-url",
            base_url,
            "--mode",
            "rpc",
            "--no-context",
            "--no-mcp",
        ]);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (events_tx, events) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(event) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if events_tx.send(event).is_err() {
                    break;
                }
            }
        });
        Self { child, events }
    }

    fn send(&mut self, value: Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{value}").unwrap();
        stdin.flush().unwrap();
    }

    fn until(&self, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let event = self
                .events
                .recv_timeout(remaining)
                .expect("RPC event deadline");
            if predicate(&event) {
                return event;
            }
        }
    }

    fn response(&self, id: &str) -> Value {
        self.until(|event| event["id"] == id)
    }
}

#[cfg(target_os = "linux")]
impl Drop for RpcChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(target_os = "linux")]
#[test]
fn rpc_dedup_hit_reports_liveness() {
    let dir = tempfile::tempdir().unwrap();
    let child = ChildGuard::spawn_sleep();
    let owner = capture_owner_for_pid(child.pid()).expect("live child identity");
    let mut stale_owner = owner.clone();
    stale_owner.start_time_ticks = stale_owner.start_time_ticks.wrapping_add(1);

    let live_id = uuid::Uuid::new_v4().to_string();
    let stale_id = uuid::Uuid::new_v4().to_string();
    write_ledger(
        dir.path(),
        &json!([
            record(
                &live_id,
                "active",
                "rpc-live",
                Some(serde_json::to_value(&owner).unwrap())
            ),
            record(
                &stale_id,
                "queued",
                "rpc-stale",
                Some(serde_json::to_value(&stale_owner).unwrap())
            ),
        ]),
    );

    let mut client = RpcChild::spawn(dir.path(), UNREACHABLE);

    client.send(json!({"type":"prompt","id":"live","message":"probe","dedup_key":"rpc-live"}));
    let live = client.response("live");
    assert_eq!(live["success"], true);
    assert_eq!(live["data"]["status"], "deduplicated");
    assert_eq!(live["data"]["task_id"], live_id);
    assert_eq!(live["data"]["state"], "active");
    assert_eq!(live["data"]["liveness"], "live");

    // stale 命中只回放 task_id/state，不重放、不改写，并如实标注 stale。
    client.send(json!({"type":"prompt","id":"stale","message":"probe","dedup_key":"rpc-stale"}));
    let stale = client.response("stale");
    assert_eq!(stale["success"], true);
    assert_eq!(stale["data"]["status"], "deduplicated");
    assert_eq!(stale["data"]["task_id"], stale_id);
    assert_eq!(stale["data"]["state"], "queued");
    assert_eq!(stale["data"]["liveness"], "stale");
}

/// 本地模型端点：接受连接后读一次请求就保持挂起，使 RPC 回合停在活动态。
#[cfg(target_os = "linux")]
struct HangingMock {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    received: mpsc::Receiver<()>,
}

#[cfg(target_os = "linux")]
impl HangingMock {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (received_tx, received) = mpsc::channel();
        let thread_stop = stop.clone();
        std::thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        let _ = socket.set_read_timeout(Some(Duration::from_millis(500)));
                        let mut buffer = [0u8; 8192];
                        let _ = socket.read(&mut buffer);
                        let _ = received_tx.send(());
                        while !thread_stop.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            address,
            stop,
            received,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    fn wait_for_request(&self) {
        self.received
            .recv_timeout(Duration::from_secs(10))
            .expect("model request deadline");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn rpc_active_prompt_and_follow_up_report_liveness() {
    let dir = tempfile::tempdir().unwrap();
    let mock = HangingMock::start();
    let mut client = RpcChild::spawn(dir.path(), &mock.base_url());

    client.send(json!({"type":"prompt","id":"p1","message":"hold","dedup_key":"active-job"}));
    let started = client.response("p1");
    assert_eq!(started["success"], true);
    assert_eq!(started["data"]["status"], "started");
    // 等到真实模型请求到达本地 mock，确认活动回合已经在读控制输入。
    mock.wait_for_request();

    // 活动回合里的重复 prompt：命中并附带 owner 存活性。
    client.send(json!({"type":"prompt","id":"dup","message":"duplicate","dedup_key":"active-job"}));
    let duplicate = client.response("dup");
    assert_eq!(duplicate["success"], true);
    assert_eq!(duplicate["data"]["status"], "deduplicated");
    assert_eq!(duplicate["data"]["task_id"], started["data"]["task_id"]);
    assert_eq!(duplicate["data"]["state"], "active");
    assert_eq!(duplicate["data"]["liveness"], "live");

    // 活动回合里的重复 follow_up 走另一条早退分支，同样附带 liveness。
    client.send(json!({"type":"follow_up","id":"dup-follow","message":"duplicate follow up","dedup_key":"active-job"}));
    let follow = client.response("dup-follow");
    assert_eq!(follow["success"], true);
    assert_eq!(follow["data"]["status"], "deduplicated");
    assert_eq!(follow["data"]["task_id"], started["data"]["task_id"]);
    assert_eq!(follow["data"]["liveness"], "live");

    client.send(json!({"type":"abort","id":"a1"}));
    assert_eq!(client.response("a1")["data"]["aborted"], true);
    assert_eq!(
        client.until(|event| event["type"] == "agent_settled")["status"],
        "cancelled"
    );

    mock.stop.store(true, Ordering::SeqCst);
}
