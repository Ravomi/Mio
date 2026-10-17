//! RPC 模式：协议循环、回合调度与显式任务去重。
//!
//! 所有出站 JSON 经 `crate::json_output::JsonOutput` 有界非阻塞传输，因此
//! `Host::on_event` 不会因慢消费者卡住 RPC `abort`。

use mio_coding_agent::{
    Config, Host, Message, ProgramState, RunOptions,
    compact::DEFAULT_KEEP_RECENT_MESSAGES,
    dedup::{TaskClaim, TaskKeyLedger, TaskKeyState, TaskLiveness},
    rpc::{
        LineReadError, RPC_ENVELOPE_VERSIONS, RPC_SCHEMA_VERSION, RpcCommand, RpcImage, RpcInput,
        rpc_response,
    },
    session::Session,
    task_lifecycle::{Settlement, TaskHandle},
};
use mio_core::message::{Image, UserContent};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering},
};

use crate::{
    journal::RunState,
    json_output::{Envelope, JsonOutput, ProtocolState, emit_or_print},
};

use super::{
    approval::RPC_QUEUE_LIMIT,
    runner::{CliHost, Runner, cancel_children, create_task_handle},
};

/// 服务端已知的可协商能力（名称 -> 是否已实现并可用）。
const SERVER_CAPABILITIES: &[(&str, bool)] = &[
    ("steering", true),
    ("followUp", true),
    ("asyncApproval", true),
    ("dedupKey", true),
    ("recoveryView", true),
    ("pinnedContext", true),
    ("contextCompactionAudit", true),
    ("eventReplay", false),
    ("automaticSideEffectReplay", false),
    ("toolLifecycle", true),
    ("imageInput", true),
    ("imageSteering", false),
    ("reasoningEvents", true),
];

fn capability_map() -> Value {
    let map: serde_json::Map<String, Value> = SERVER_CAPABILITIES
        .iter()
        .map(|(name, enabled)| ((*name).to_string(), json!(enabled)))
        .collect();
    Value::Object(map)
}

/// 客户端请求能力的三分类：已授予、已知但未实现、未知名称。不伪造授予。
fn granted_capabilities(requested: &[String]) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut granted = Vec::new();
    let mut unsupported = Vec::new();
    let mut unknown = Vec::new();
    if requested.is_empty() {
        for (name, enabled) in SERVER_CAPABILITIES {
            if *enabled {
                granted.push((*name).to_string());
            }
        }
        return (granted, unsupported, unknown);
    }
    for name in requested {
        match SERVER_CAPABILITIES
            .iter()
            .find(|(capability, _)| *capability == name.as_str())
        {
            Some((_, true)) => {
                if !granted.contains(name) {
                    granted.push(name.clone());
                }
            }
            Some((_, false)) => {
                if !unsupported.contains(name) {
                    unsupported.push(name.clone());
                }
            }
            None => {
                if !unknown.contains(name) {
                    unknown.push(name.clone());
                }
            }
        }
    }
    (granted, unsupported, unknown)
}

/// 处理一次 `initialize`：取双方版本交集中的最高版本；交集为空则显式拒绝。
///
/// 拒绝时把协议状态标记为 `rejected`，不静默降级，也不让新任务在未知版本下启动。
fn handle_initialize(
    output: &Option<JsonOutput>,
    id: Option<&str>,
    schema_versions: &[u64],
    capabilities: &[String],
) -> Value {
    let server: Vec<u64> = RPC_ENVELOPE_VERSIONS.to_vec();
    let reject = |message: String| {
        if let Some(output) = output {
            output.set_protocol(ProtocolState {
                initialized: false,
                rejected: true,
                envelope: Envelope::Legacy,
                schema_version: RPC_SCHEMA_VERSION,
            });
        }
        rpc_response(
            "initialize",
            id,
            false,
            json!({
                "status": "rejected",
                "negotiated": false,
                "envelope": "legacy",
                "schemaVersion": Value::Null,
                "clientSchemaVersions": schema_versions,
                "serverSchemaVersions": server,
                "capabilities": capability_map(),
                "error": message,
            }),
        )
    };
    if schema_versions.is_empty() {
        return reject("initialize requires at least one schema version".to_string());
    }
    let chosen = server
        .iter()
        .copied()
        .filter(|version| schema_versions.contains(version))
        .max();
    let Some(version) = chosen else {
        return reject(format!(
            "No common RPC schema version; client offered {schema_versions:?}, server supports {server:?}"
        ));
    };
    if let Some(output) = output {
        output.set_protocol(ProtocolState {
            initialized: true,
            rejected: false,
            envelope: Envelope::V1,
            schema_version: version,
        });
    }
    let (granted, unsupported, unknown) = granted_capabilities(capabilities);
    rpc_response(
        "initialize",
        id,
        true,
        json!({
            "status": "negotiated",
            "negotiated": true,
            "envelope": "v1",
            "schemaVersion": version,
            "clientSchemaVersions": schema_versions,
            "serverSchemaVersions": server,
            "capabilities": capability_map(),
            "grantedCapabilities": granted,
            "unsupportedCapabilities": unsupported,
            "unknownCapabilities": unknown,
        }),
    )
}

/// 协商被拒后阻止新任务启动；已运行的任务不会被撤回，也不会被谎称启动成功。
fn negotiation_blocked(output: &Option<JsonOutput>) -> Option<String> {
    let state = output.as_ref()?.protocol_state();
    state.rejected.then(|| {
        let versions: Vec<String> = RPC_ENVELOPE_VERSIONS.iter().map(u64::to_string).collect();
        format!(
            "Protocol negotiation was rejected; re-send initialize with a supported schema version ({})",
            versions.join(", ")
        )
    })
}

/// 等待传输 fail-closed；没有传输时永不完成（纯文本模式不会走到这里）。
async fn wait_output_failure(output: &Option<JsonOutput>) -> String {
    match output {
        Some(output) => {
            output.wait_failure().await;
            output
                .failure()
                .unwrap_or_else(|| "JSON output transport failed".to_string())
        }
        None => std::future::pending().await,
    }
}

fn model_capabilities(runner: &Runner) -> serde_json::Value {
    json!({
        "imageInput": mio_coding_agent::image_input::model_supports_image(&runner.config, &runner.options),
        "source": "localCatalog",
    })
}

fn rpc_state(runner: &Runner, running: bool, queued: usize) -> serde_json::Value {
    let output = runner.host.output.as_ref();
    let mut protocol = mio_coding_agent::rpc::protocol_info();
    if let Some(output) = output {
        let state = output.protocol_state();
        protocol["eventEnvelope"] = json!(state.envelope.as_str());
        protocol["negotiation"] = json!({
            "initialized": state.initialized,
            "rejected": state.rejected,
            "schemaVersion": state.schema_version,
        });
    }
    json!({
        "protocol": protocol,
        "sessionId": runner.session.id,
        "sessionName": runner.session.name,
        "session_name": runner.session.name,
        "sessionFile": if runner.no_session { serde_json::Value::Null } else { json!(runner.path) },
        "model": runner.config.model,
        "provider": runner.provider_name,
        "modelCapabilities": model_capabilities(runner),
        "thinkingLevel": runner.options.thinking,
        "toolConcurrency": runner.options.tool_concurrency,
        "executionBackend": super::runner::execution_status(&runner.host.execution_policy),
        "contextBudget": mio_coding_agent::context_budget::ContextBudget::resolve(&runner.config, &runner.options).ok(),
        "pinnedContext": runner.session.pinned_context,
        "scheduler": runner.options.scheduler.as_ref().map(|scheduler| scheduler.snapshot()),
        "childTasks": runner.subagents.as_ref().map(|manager| manager.activity()),
        "messageCount": runner.session.history.len(),
        "isStreaming": running,
        "followUpCount": queued,
        "pendingApprovalCount": runner.host.rpc_approvals.as_ref().map_or(0, |a| a.pending_count()),
        "journalEnabled": runner.journal.is_enabled(),
        "journalPath": runner.journal.path(),
        "unresolvedEffects": runner.host.ledger_status(),
        "recovery": runner.journal.recovery_report(),
        "recoveryView": super::runner::recovery_view(
            &runner.session.id.to_string(),
            &runner.session.cwd,
            if runner.no_session { None } else { Some(&runner.path) },
            "recovery",
        ),
    })
}

fn rpc_message_valid(message: &str) -> bool {
    !message.trim().is_empty() && message.len() <= 65536
}

fn prepare_images(
    config: &Config,
    options: &RunOptions,
    message: &str,
    images: &[RpcImage],
) -> Result<Vec<Image>, String> {
    let images = images
        .iter()
        .map(RpcImage::to_image)
        .collect::<Result<Vec<_>, _>>()?;
    let mut content = vec![UserContent::text(message)];
    content.extend(images.iter().cloned().map(UserContent::Image));
    mio_coding_agent::image_input::validate_prompt(config, options, &Message::User { content })?;
    Ok(images)
}

fn rpc_parse(line: &str, output: &Option<JsonOutput>) -> Option<RpcCommand> {
    if line.trim().is_empty() {
        return None;
    }
    let value: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => {
            let _ = emit_or_print(
                output,
                rpc_response("parse", None, false, json!({"error": error.to_string()})),
                true,
            );
            return None;
        }
    };
    // Message commands must fail closed on unknown fields. Otherwise serde
    // silently drops an image attachment while dispatching the text alone.
    if let Some(object) = value.as_object()
        && let Some(command @ ("prompt" | "steer" | "follow_up")) =
            object.get("type").and_then(Value::as_str)
    {
        let allowed: &[&str] = if command == "steer" {
            &["type", "id", "message", "images"]
        } else {
            &["type", "id", "message", "images", "dedup_key"]
        };
        if let Some(field) = object
            .keys()
            .find(|field| !allowed.contains(&field.as_str()))
        {
            let field = if field.len() <= 64 {
                field.as_str()
            } else {
                "<field name over 64 bytes>"
            };
            let _ = emit_or_print(
                output,
                rpc_response(
                    command,
                    object.get("id").and_then(Value::as_str),
                    false,
                    json!({"error": format!("Unsupported {command} field '{field}'; no task was started. Use the documented images array for image input.")}),
                ),
                true,
            );
            return None;
        }
    }
    let rejected_command = value
        .get("type")
        .and_then(Value::as_str)
        .filter(|kind| matches!(*kind, "prompt" | "steer" | "follow_up"))
        .map(str::to_owned);
    let rejected_id = value.get("id").and_then(Value::as_str).map(str::to_owned);
    match serde_json::from_value(value) {
        Ok(command) => Some(command),
        Err(error) => {
            let _ = emit_or_print(
                output,
                rpc_response(
                    rejected_command.as_deref().unwrap_or("parse"),
                    rejected_id.as_deref(),
                    false,
                    json!({"error": error.to_string()}),
                ),
                true,
            );
            None
        }
    }
}

fn with_liveness(mut data: Value, liveness: Option<TaskLiveness>) -> Value {
    if let Some(liveness) = liveness {
        data["liveness"] = json!(liveness.as_str());
    }
    data
}

/// 读取去重命中记录的 owner 存活性观测。
///
/// 只读：不重放、不改写 state，也不删除 key。账本不可读、损坏或超限时返回
/// `Err`，调用方必须 fail-closed，不得把探测失败降级为 `unknown`。
fn reused_liveness(ledger: &TaskKeyLedger, key: &str) -> Result<Option<TaskLiveness>, String> {
    Ok(ledger.observe(key)?.map(|observation| observation.liveness))
}

/// 输出任务标识（JSON Lines 事件或人类可读行）。
///
/// 只有 `reused`（重复 key 命中）才携带 liveness；新准入的任务没有可观测的
/// owner 历史，字段如实省略，不用 `updated` 或 `kill 0` 之类信号猜测存活。
fn report_task(
    output: &Option<JsonOutput>,
    reused: bool,
    key: &str,
    task_id: &str,
    state: TaskKeyState,
    liveness: Option<TaskLiveness>,
) {
    if output.is_some() {
        let event = with_liveness(
            json!({"type":"task","task_id":task_id,"dedup_key":key,"state":state,"reused":reused}),
            liveness,
        );
        let _ = emit_or_print(output, event, true);
    } else if reused {
        match liveness {
            Some(liveness) => println!(
                "Task {task_id} already recorded for dedup key {key:?} ({}); owner liveness {}; not re-executed.",
                state.as_str(),
                liveness.as_str()
            ),
            None => println!(
                "Task {task_id} already recorded for dedup key {key:?} ({}); not re-executed.",
                state.as_str()
            ),
        }
    } else {
        eprintln!("Task: {task_id}");
    }
}

/// CLI 单次任务：带 key 时先去重，命中则只回放已有任务标识。
pub(super) async fn run_one_shot(
    runner: &mut Runner,
    prompt: &str,
    dedup_key: Option<&str>,
) -> Result<(), String> {
    let Some(key) = dedup_key else {
        return runner.turn(prompt).await;
    };
    let ledger = runner.dedup.clone();
    let output = runner.host.output.clone();
    match ledger.claim(key)? {
        TaskClaim::Existing { task_id, state } => {
            // 只读观测 owner 存活性；探测失败即 fail-closed，不猜 live/stale，
            // 也不据此重放任务。
            report_task(
                &output,
                true,
                key,
                &task_id,
                state,
                reused_liveness(&ledger, key)?,
            );
            Ok(())
        }
        TaskClaim::Acquired { task_id } => {
            // The dedup ledger becomes Active only when the canonical tracker
            // observes a real scheduling admission; reporting `queued` here is
            // honest because nothing was dispatched yet. The old eager
            // `ledger.begin` claimed a model lease that had not been obtained.
            report_task(&output, false, key, &task_id, TaskKeyState::Queued, None);
            // A task whose canonical registration cannot persist must not run:
            // the dedup key would be stuck `queued` with no reclamation path.
            let handle = runner.new_task_handle(Some(task_id.clone()))?;
            let (_run_state, result) = runner.run_turn_with_task(prompt, Some(handle)).await;
            result
        }
    }
}

#[derive(Debug)]
enum RpcTurnEnd {
    Finished(RunState, Result<(), String>),
    Abort(Option<String>),
    Closed,
    InputError(String),
    /// 出站传输 fail-closed：取消活动工作，不冒充成功。
    OutputFailed(String),
}

/// The real terminal a dropped RPC turn must request.
///
/// A normal cancel (abort or peer EOF) records a real cancellation request and
/// commits `Cancelled`. A transport/input failure is not a cancel: recording a
/// cancel request would forbid the real `Failed` terminal, so it requests
/// `Failed` directly. The boolean says whether a real cancellation request must
/// be recorded first.
fn turn_end_settlement(end: &RpcTurnEnd) -> (Settlement<'static>, RunState, bool) {
    match end {
        RpcTurnEnd::Abort(_) | RpcTurnEnd::Closed => {
            (Settlement::Cancelled, RunState::Cancelled, true)
        }
        RpcTurnEnd::OutputFailed(_) | RpcTurnEnd::InputError(_) => {
            (Settlement::Failed, RunState::Failed, false)
        }
        RpcTurnEnd::Finished(state, _) => (
            match state {
                RunState::Completed => Settlement::Completed,
                RunState::Cancelled => Settlement::Cancelled,
                _ => Settlement::Failed,
            },
            *state,
            matches!(state, RunState::Cancelled),
        ),
    }
}

/// 在 RPC 回合内发送关键事件；传输失败立刻结束本回合并 fail-closed。
macro_rules! rpc_emit {
    ($output:expr, $value:expr) => {
        if let Err(error) = emit_or_print($output, $value, true) {
            break RpcTurnEnd::OutputFailed(error);
        }
    };
}

/// 排队中的 RPC 回合及其唯一真实 canonical 句柄。
///
/// 句柄在受理入队那一刻就唯一确定：keyed 沿用去重账本的真实 task id，无 key
/// 一次铸出自己的真实 id。此后 pop 执行与取消都复用同一个句柄，绝不重新注册
/// 或重铸 ID，排队期始终存在权威 `Queued` 状态。
struct QueuedTurn {
    message: String,
    images: Vec<Image>,
    handle: TaskHandle,
    /// 仅当该回合来自显式 dedup key 时，才是 legacy 账本里的真实 task id；
    /// 无 key 回合在此保持 `None`，绝不把 canonical id 误当 keyed 去 lookup。
    dedup_task_id: Option<String>,
}

/// 入队期句柄工厂。
///
/// 活动回合的 future 独占 `&mut Runner`，因此受理排队 `follow_up` 时不能借用
/// `runner`。本工厂只持有 clone 出来的真实宿主与去重账本，并委托给 runner 里
/// 全进程唯一的 [`create_task_handle`]：同一实现、同一 tracker / projection，
/// 不是第二套任务构造，也不新建任何持久账本。
struct QueuedHandleFactory {
    host: Arc<CliHost>,
    dedup: Arc<TaskKeyLedger>,
}

impl QueuedHandleFactory {
    fn new(host: Arc<CliHost>, dedup: Arc<TaskKeyLedger>) -> Self {
        Self { host, dedup }
    }

    /// 登记一个真实 canonical 句柄（`Queued`）。
    ///
    /// `dedup_task` 是 keyed 回合在账本里的真实 task id；无 key 时铸出独立新 id。
    /// 登记失败即返回错误，调用方必须明确拒绝并处理已 claim 的 key。
    fn mint(&self, dedup_task: Option<String>) -> Result<TaskHandle, String> {
        create_task_handle(&self.host, &self.dedup, dedup_task)
    }
}

/// 显式 key 的准入结果。
enum DedupAdmission {
    None,
    Acquired {
        task_id: String,
    },
    Existing {
        task_id: String,
        state: TaskKeyState,
        /// 去重命中的保守 owner 存活性观测；观测不到时为 `None`（如实省略）。
        liveness: Option<TaskLiveness>,
    },
}

fn admit_dedup(ledger: &TaskKeyLedger, key: Option<&str>) -> Result<DedupAdmission, String> {
    let Some(key) = key else {
        return Ok(DedupAdmission::None);
    };
    Ok(match ledger.claim(key)? {
        TaskClaim::Acquired { task_id } => DedupAdmission::Acquired { task_id },
        TaskClaim::Existing { task_id, state } => DedupAdmission::Existing {
            task_id,
            state,
            // claim 已确认 key 存在；随后只读观测 owner 存活性，失败即 fail-closed。
            liveness: reused_liveness(ledger, key)?,
        },
    })
}

fn finish_task(ledger: &TaskKeyLedger, task_id: &Option<String>, state: TaskKeyState) {
    if let Some(id) = task_id
        && let Err(error) = ledger.finish(id, state)
    {
        eprintln!("Warning: failed to finalize task key evidence for {id}: {error}");
    }
}

/// 丢弃排队回合时经各自受理时持有的真实句柄请求取消，保守地不允许重放。
///
/// 句柄就是排队期唯一权威：直接复用它，不重新注册、不重铸 ID。跟踪器立即提交
/// `Queued -> Cancelled`（零 admit），journal 与 legacy 账本由同一个 sink 投影；
/// 无 key 回合没有 legacy key 记录，`dedup_task_id` 为 `None` 时跳过，绝不用
/// canonical id 去 lookup 一个不存在的 keyed 记录。
fn cancel_queued_turns(ledger: &TaskKeyLedger, follow_ups: &mut VecDeque<QueuedTurn>) {
    for turn in follow_ups.drain(..) {
        if let Err(error) = turn
            .handle
            .request_cancel(Some("the queued RPC turn was cancelled"))
        {
            eprintln!("Warning: failed to cancel a queued canonical task: {error}");
        }
        finish_task(ledger, &turn.dedup_task_id, TaskKeyState::Cancelled);
    }
}

// The turn future borrows Runner exclusively; control input uses an immutable
// state snapshot and the shared host. Dropping the future cancels model I/O and
// the shell process guard before cancellation is acknowledged.
async fn run_rpc_tasks(
    runner: &mut Runner,
    lines: &mut RpcInput,
    initial: String,
    initial_images: Vec<Image>,
    initial_handle: Option<TaskHandle>,
) -> Result<bool, String> {
    let dedup = runner.dedup.clone();
    let handles = QueuedHandleFactory::new(runner.host.clone(), dedup.clone());
    let mut current_handle = initial_handle;
    let mut follow_ups: VecDeque<QueuedTurn> = VecDeque::new();
    let mut message = initial;
    let mut images = initial_images;
    let image_config = runner.config.clone();
    let image_options = runner.options.clone();
    loop {
        let host = runner.host.clone();
        let output = host.output.clone();
        let scheduler = runner.options.scheduler.clone();
        let child_manager = runner.subagents.clone();
        let mut state = rpc_state(runner, true, follow_ups.len());
        let end = {
            let turn = runner.run_turn_with_task_images(
                &message,
                std::mem::take(&mut images),
                current_handle.clone(),
            );
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    line = lines.next_line() => {
                        let line = match line {
                            Ok(Some(line)) => line,
                            Ok(None) => break RpcTurnEnd::Closed,
                            Err(LineReadError::TooLong { max_bytes }) => {
                                rpc_emit!(&output, json!({"type":"error","error":format!("RPC input line exceeds {max_bytes} bytes; line discarded")}));
                                continue;
                            }
                            Err(error) => break RpcTurnEnd::InputError(error.to_string()),
                        };
                        let Some(command) = rpc_parse(&line, &output) else { continue };
                        match command {
                            RpcCommand::Abort { id } => break RpcTurnEnd::Abort(id),
                            RpcCommand::Initialize { id, schema_versions, capabilities } => {
                                let response = handle_initialize(&output, id.as_deref(), &schema_versions, &capabilities);
                                rpc_emit!(&output, response);
                            }
                            RpcCommand::Steer { id, message, images } => {
                                if let Some(error) = negotiation_blocked(&output) {
                                    rpc_emit!(&output, rpc_response("steer", id.as_deref(), false, json!({"error":error})));
                                    continue;
                                }
                                if !images.is_empty() {
                                    rpc_emit!(&output, rpc_response("steer", id.as_deref(), false, json!({"error":"Image steering during an active turn is unsupported; use follow_up to send the image after this turn"})));
                                    continue;
                                }
                                let mut queue = host.steering_queue.lock().unwrap();
                                if !rpc_message_valid(&message) || queue.len() >= RPC_QUEUE_LIMIT {
                                    drop(queue);
                                    rpc_emit!(&output, rpc_response("steer", id.as_deref(), false, json!({"error":"Message must be 1–65536 bytes and steering queue must have space"})));
                                } else {
                                    queue.push_back(message);
                                    drop(queue);
                                    rpc_emit!(&output, rpc_response("steer", id.as_deref(), true, json!({"queued":true})));
                                }
                            }
                            RpcCommand::Prompt { id, message, images, dedup_key } => {
                                if let Some(error) = negotiation_blocked(&output) {
                                    rpc_emit!(&output, rpc_response("prompt", id.as_deref(), false, json!({"error":error})));
                                    continue;
                                }
                                let existing = if !rpc_message_valid(&message) {
                                    Err("Message must be 1–65536 bytes".to_owned())
                                } else if let Err(error) = prepare_images(&image_config, &image_options, &message, &images) {
                                    Err(error)
                                } else if let Some(key) = dedup_key.as_deref() {
                                    // 活动回合里的重复 prompt 也必须接入保守观测；
                                    // 观测失败 fail-closed，不伪装成命中或未知。
                                    mio_coding_agent::dedup::validate_key(key).and_then(|()| dedup.observe(key))
                                } else {
                                    Ok(None)
                                };
                                let value = match existing {
                                    Ok(Some(observation)) => rpc_response("prompt", id.as_deref(), true, with_liveness(json!({"status":"deduplicated","task_id":observation.record.task_id,"state":observation.record.state,"dedup_key":dedup_key}), Some(observation.liveness))),
                                    Ok(None) => rpc_response("prompt", id.as_deref(), false, json!({"error":"Agent is running; use steer, follow_up, abort, approve_response or get_state"})),
                                    Err(error) => rpc_response("prompt", id.as_deref(), false, json!({"error":error})),
                                };
                                rpc_emit!(&output, value);
                            }
                            RpcCommand::FollowUp { id, message, images, dedup_key } => {
                                if let Some(error) = negotiation_blocked(&output) {
                                    rpc_emit!(&output, rpc_response("follow_up", id.as_deref(), false, json!({"error":error})));
                                    continue;
                                }
                                let prepared_images = match prepare_images(&image_config, &image_options, &message, &images) {
                                    Ok(images) => images,
                                    Err(error) => {
                                        rpc_emit!(&output, rpc_response("follow_up", id.as_deref(), false, json!({"error":error})));
                                        continue;
                                    }
                                };
                                if rpc_message_valid(&message)
                                    && let Some(key) = dedup_key.as_deref()
                                {
                                    match mio_coding_agent::dedup::validate_key(key).and_then(|()| dedup.observe(key)) {
                                        Ok(Some(observation)) => {
                                            rpc_emit!(&output, rpc_response("follow_up", id.as_deref(), true, with_liveness(json!({"status":"deduplicated","task_id":observation.record.task_id,"state":observation.record.state,"dedup_key":dedup_key}), Some(observation.liveness))));
                                            continue;
                                        }
                                        Err(error) => {
                                            rpc_emit!(&output, rpc_response("follow_up", id.as_deref(), false, json!({"error":error})));
                                            continue;
                                        }
                                        Ok(None) => {}
                                    }
                                }
                                // 消息校验与队列容量必须先于 key 准入：被拒绝的
                                // follow_up 不得消耗 key。
                                if !rpc_message_valid(&message) || follow_ups.len() >= RPC_QUEUE_LIMIT {
                                    rpc_emit!(&output, rpc_response("follow_up", id.as_deref(), false, json!({"error":"Message must be 1–65536 bytes and follow-up queue must have space"})));
                                } else {
                                    match admit_dedup(&dedup, dedup_key.as_deref()) {
                                        Ok(DedupAdmission::Existing { task_id, state, liveness }) => {
                                            rpc_emit!(&output, rpc_response("follow_up", id.as_deref(), true, with_liveness(json!({"status":"deduplicated","task_id":task_id,"state":state,"dedup_key":dedup_key}), liveness)));
                                        }
                                        Ok(DedupAdmission::Acquired { task_id }) => {
                                            // 受理入队就登记真实 canonical 句柄：keyed 用账本的真实 task id，
                                            // 排队期立即拥有权威 `Queued`，而不是等 pop 才换身份。
                                            match handles.mint(Some(task_id.clone())) {
                                                Ok(handle) => {
                                                    follow_ups.push_back(QueuedTurn {
                                                        message,
                                                        images: prepared_images,
                                                        handle,
                                                        dedup_task_id: Some(task_id.clone()),
                                                    });
                                                    rpc_emit!(&output, rpc_response("follow_up", id.as_deref(), true, json!({"queued":follow_ups.len(),"task_id":task_id,"dedup_key":dedup_key})));
                                                }
                                                Err(error) => {
                                                    // key 已被 claim 成 queued，但 canonical 登记失败：明确拒绝，
                                                    // 并把该 key 保守记为准备失败终态，绝不留下无法 reclaim 的
                                                    // queued，也绝不自动重放。
                                                    finish_task(&dedup, &Some(task_id), TaskKeyState::Failed);
                                                    rpc_emit!(&output, rpc_response("follow_up", id.as_deref(), false, json!({"error":error})));
                                                }
                                            }
                                        }
                                        Ok(DedupAdmission::None) => {
                                            // 无 key 回合同样在受理时一次铸出真实 task id：应答回传它作为
                                            // canonical id，但绝不附带 dedup_key，也不假装去重。
                                            match handles.mint(None) {
                                                Ok(handle) => {
                                                    let task_id = handle.task_id().as_str().to_owned();
                                                    follow_ups.push_back(QueuedTurn {
                                                        message,
                                                        images: prepared_images,
                                                        handle,
                                                        dedup_task_id: None,
                                                    });
                                                    rpc_emit!(&output, rpc_response("follow_up", id.as_deref(), true, json!({"queued":follow_ups.len(),"task_id":task_id})));
                                                }
                                                Err(error) => {
                                                    rpc_emit!(&output, rpc_response("follow_up", id.as_deref(), false, json!({"error":error})));
                                                }
                                            }
                                        }
                                        Err(error) => {
                                            rpc_emit!(&output, rpc_response("follow_up", id.as_deref(), false, json!({"error":error})));
                                        }
                                    }
                                }
                            }
                            RpcCommand::GetState { id } => {
                                // 协商可能在本次回合内发生，重新读取实时协议状态。
                                if let Some(output) = &output {
                                    let protocol_state = output.protocol_state();
                                    state["protocol"]["eventEnvelope"] =
                                        json!(protocol_state.envelope.as_str());
                                    state["protocol"]["negotiation"] = json!({
                                        "initialized": protocol_state.initialized,
                                        "rejected": protocol_state.rejected,
                                        "schemaVersion": protocol_state.schema_version,
                                    });
                                }
                                state["followUpCount"] = json!(follow_ups.len());
                                state["steeringCount"] = json!(host.steering_queue.lock().unwrap().len());
                                state["pendingApprovalCount"] = json!(host.rpc_approvals.as_ref().map_or(0, |a| a.pending_count()));
                                state["scheduler"] = scheduler.as_ref().map_or(Value::Null, |scheduler| scheduler.snapshot());
                                state["childTasks"] = child_manager.as_ref().map_or(Value::Null, |manager| json!(manager.activity()));
                                rpc_emit!(&output, rpc_response("get_state", id.as_deref(), true, state.clone()));
                            }
                            RpcCommand::ApproveResponse { request_id, approved } => {
                                let res = match &host.rpc_approvals {
                                    Some(broker) => broker.resolve(&request_id, approved),
                                    None => Err("Approval broker is not enabled".to_string()),
                                };
                                let value = match res {
                                    Ok(()) => rpc_response("approve_response", Some(&request_id), true, json!({"approved": approved})),
                                    Err(err) => rpc_response("approve_response", Some(&request_id), false, json!({"error": err})),
                                };
                                rpc_emit!(&output, value);
                            }
                            other => {
                                let value = serde_json::to_value(other).expect("RPC serialization");
                                rpc_emit!(&output, rpc_response(value["type"].as_str().unwrap_or("unknown"), value["id"].as_str().or(value["request_id"].as_str()), false, json!({"error":"Agent is running; use steer, follow_up, abort, approve_response or get_state"})));
                            }
                        }
                    }
                    result = &mut turn => break RpcTurnEnd::Finished(result.0, result.1),
                    failure = wait_output_failure(&output) => break RpcTurnEnd::OutputFailed(failure),
                }
            }
        };
        match end {
            RpcTurnEnd::Finished(_run_state, Ok(())) => {
                // `run_turn_with_task` already settled the canonical task from
                // this result, and its sink projected the terminal into both
                // the dedup ledger and the journal.
                current_handle = None;
                // Tool-free responses (and failed tools) may leave steering
                // unconsumed. Continue with it before normal follow-up work.
                if let Some(next) = host.poll_steering() {
                    message = next;
                    images.clear();
                    continue;
                }
                if let Some(next) = follow_ups.pop_front() {
                    // 受理入队时已经登记了唯一真实句柄；这里直接沿用，绝不重新
                    // 注册或重铸 ID。引擎只在真实调度许可处准入 `Queued -> Running`。
                    current_handle = Some(next.handle);
                    message = next.message;
                    images = next.images;
                    continue;
                }
                emit_or_print(
                    &output,
                    json!({"type":"agent_settled", "status":"completed"}),
                    true,
                )?;
                return Ok(true);
            }
            other => {
                host.steering_queue.lock().unwrap().clear();
                // Acknowledge an abort request as received before any
                // reclamation work, so the client learns receipt without
                // waiting for workers. The legacy `aborted` field is retained
                // for existing clients; `received` is the receipt-only signal.
                if let RpcTurnEnd::Abort(id) = &other {
                    host.reporter.cancelled.store(true, Ordering::SeqCst);
                    emit_or_print(
                        &output,
                        rpc_response(
                            "abort",
                            id.as_deref(),
                            true,
                            json!({"received":true,"aborted":true}),
                        ),
                        true,
                    )?;
                }
                cancel_queued_turns(&dedup, &mut follow_ups);
                cancel_children(runner.subagents.as_ref()).await?;
                // A normal cancel (abort or peer EOF) records a real cancel
                // request and commits `Cancelled`. A transport/input failure is
                // not a cancel request: requesting cancellation would forbid the
                // real `Failed` terminal, so it commits `Failed` directly. Every
                // finish/request result is reported, and a task that never
                // settles is reported as a failure instead of a false
                // `agent_settled`.
                let (settlement, terminal_state, request_cancel) = turn_end_settlement(&other);
                let mut settled = true;
                if let Some(handle) = current_handle.take() {
                    if request_cancel
                        && let Err(error) =
                            handle.request_cancel(Some("rpc turn ended before settlement"))
                    {
                        eprintln!("Warning: could not record the cancellation request: {error}");
                    }
                    if let Err(error) = handle.finish(settlement, None) {
                        eprintln!("Warning: canonical task settlement failed: {error}");
                    }
                    // Wait for the real settled record, but never forever: a
                    // latched persistence failure returns the last non-terminal
                    // record, which is reported as a failure below rather than
                    // as a false settlement.
                    match handle.wait_settled().await {
                        Some(record) if record.phase.is_terminal() => {}
                        other => {
                            settled = false;
                            let phase = other
                                .map(|record| record.phase.as_str().to_owned())
                                .unwrap_or_else(|| "untracked".to_owned());
                            eprintln!(
                                "Warning: canonical task did not settle (phase {phase}); reporting failure instead of settled"
                            );
                        }
                    }
                }
                if let Err(error) = runner.journal.finish_run(terminal_state) {
                    eprintln!("Warning: failed to finalize execution evidence: {error}");
                }
                if !settled {
                    // A terminal `agent_settled` must never be emitted for a
                    // task that never settled; fail closed instead.
                    return Err("the RPC turn ended but its canonical task did not settle".into());
                }
                match other {
                    RpcTurnEnd::Abort(_) => {
                        runner.host.reporter.report(ProgramState::Idle, None);
                        // The settled event follows only after the real workers
                        // were reclaimed above.
                        emit_or_print(
                            &output,
                            json!({"type":"agent_settled", "status":"cancelled"}),
                            true,
                        )?;
                        return Ok(true);
                    }
                    RpcTurnEnd::Closed => return Ok(false),
                    RpcTurnEnd::InputError(error) => return Err(error),
                    RpcTurnEnd::OutputFailed(error) => {
                        // 传输已 fail-closed：只允许短 stderr 诊断，不污染 stdout。
                        eprintln!("Mio: {error}");
                        return Err(error);
                    }
                    RpcTurnEnd::Finished(_, Err(error)) => {
                        emit_or_print(&output, json!({"type":"error", "error":error}), true)?;
                        emit_or_print(
                            &output,
                            json!({"type":"agent_settled", "status":"error"}),
                            true,
                        )?;
                        return Ok(true);
                    }
                    RpcTurnEnd::Finished(_, Ok(())) => unreachable!(),
                }
            }
        }
    }
}

/// 开始一个顶层 RPC 回合（空闲态收到的 `prompt` / `steer` / `follow_up`）。
///
/// 带显式 key 时先去重：命中则只回放已有任务标识并立即 settle，不调用模型。
async fn start_rpc_turn(
    runner: &mut Runner,
    lines: &mut RpcInput,
    name: &str,
    id: Option<&str>,
    message: String,
    images: Vec<RpcImage>,
    dedup_key: Option<&str>,
) -> Result<bool, String> {
    let output = runner.host.output.clone();
    if let Some(error) = negotiation_blocked(&output) {
        emit_or_print(
            &output,
            rpc_response(name, id, false, json!({"error": error})),
            true,
        )?;
        return Ok(true);
    }
    if !rpc_message_valid(&message) {
        emit_or_print(
            &output,
            rpc_response(
                name,
                id,
                false,
                json!({"error":"Message must be 1–65536 bytes"}),
            ),
            true,
        )?;
        return Ok(true);
    }
    let images = match prepare_images(&runner.config, &runner.options, &message, &images) {
        Ok(images) => images,
        Err(error) => {
            emit_or_print(
                &output,
                rpc_response(name, id, false, json!({"error": error})),
                true,
            )?;
            return Ok(true);
        }
    };
    let handles = QueuedHandleFactory::new(runner.host.clone(), runner.dedup.clone());
    let task = match admit_dedup(&runner.dedup, dedup_key) {
        Ok(DedupAdmission::None) => {
            // An accepted RPC task is always tracked by a real canonical handle,
            // whether or not it carried a dedup key. Tracking is never decided
            // by dedup, so an unkeyed running turn still has a handle the abort
            // path can wait on.
            match handles.mint(None) {
                Ok(handle) => {
                    emit_or_print(
                        &output,
                        rpc_response(name, id, true, json!({"status":"started"})),
                        true,
                    )?;
                    Some(handle)
                }
                Err(error) => {
                    emit_or_print(
                        &output,
                        rpc_response(name, id, false, json!({"error":error})),
                        true,
                    )?;
                    return Ok(true);
                }
            }
        }
        Ok(DedupAdmission::Acquired { task_id }) => {
            // The ledger stays `queued` until the canonical tracker records a
            // real admission; no model lease has been obtained yet.
            let handle = match handles.mint(Some(task_id.clone())) {
                Ok(handle) => handle,
                Err(error) => {
                    // 该 key 已被 claim 成 queued，但 canonical 登记失败：明确
                    // 拒绝，并把 key 保守记为准备失败终态，绝不留下无法 reclaim
                    // 的 queued，也绝不自动重放。
                    finish_task(&runner.dedup, &Some(task_id), TaskKeyState::Failed);
                    emit_or_print(
                        &output,
                        rpc_response(name, id, false, json!({"error":error})),
                        true,
                    )?;
                    return Ok(true);
                }
            };
            emit_or_print(
                &output,
                rpc_response(
                    name,
                    id,
                    true,
                    json!({"status":"started","task_id":task_id,"dedup_key":dedup_key}),
                ),
                true,
            )?;
            Some(handle)
        }
        Ok(DedupAdmission::Existing {
            task_id,
            state,
            liveness,
        }) => {
            emit_or_print(
                &output,
                rpc_response(
                    name,
                    id,
                    true,
                    with_liveness(
                        json!({"status":"deduplicated","task_id":task_id,"state":state,"dedup_key":dedup_key}),
                        liveness,
                    ),
                ),
                true,
            )?;
            emit_or_print(
                &output,
                json!({"type":"agent_settled", "status":"deduplicated"}),
                true,
            )?;
            return Ok(true);
        }
        Err(error) => {
            emit_or_print(
                &output,
                rpc_response(name, id, false, json!({"error":error})),
                true,
            )?;
            return Ok(true);
        }
    };
    run_rpc_tasks(runner, lines, message, images, task).await
}

pub(super) async fn run_rpc_loop(
    runner: &mut Runner,
    directory: &Path,
    cwd: &Path,
) -> Result<(), String> {
    let mut lines = RpcInput::new();
    let output = runner.host.output.clone();
    loop {
        let line = tokio::select! {
            line = lines.next_line() => line,
            failure = wait_output_failure(&output) => {
                eprintln!("Mio: {failure}");
                return Err(failure);
            }
        };
        let line = match line {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(LineReadError::TooLong { max_bytes }) => {
                emit_or_print(
                    &output,
                    json!({"type":"error","error":format!("RPC input line exceeds {max_bytes} bytes; line discarded")}),
                    true,
                )?;
                continue;
            }
            Err(error) => return Err(error.to_string()),
        };
        let Some(cmd) = rpc_parse(&line, &output) else {
            continue;
        };
        match cmd {
            RpcCommand::Initialize {
                id,
                schema_versions,
                capabilities,
            } => {
                let response =
                    handle_initialize(&output, id.as_deref(), &schema_versions, &capabilities);
                emit_or_print(&output, response, true)?;
            }
            RpcCommand::Prompt {
                id,
                message,
                images,
                dedup_key,
            } => {
                if !start_rpc_turn(
                    runner,
                    &mut lines,
                    "prompt",
                    id.as_deref(),
                    message,
                    images,
                    dedup_key.as_deref(),
                )
                .await?
                {
                    return Ok(());
                }
            }
            RpcCommand::FollowUp {
                id,
                message,
                images,
                dedup_key,
            } => {
                if !start_rpc_turn(
                    runner,
                    &mut lines,
                    "follow_up",
                    id.as_deref(),
                    message,
                    images,
                    dedup_key.as_deref(),
                )
                .await?
                {
                    return Ok(());
                }
            }
            RpcCommand::Steer {
                id,
                message,
                images,
            } => {
                if !start_rpc_turn(
                    runner,
                    &mut lines,
                    "steer",
                    id.as_deref(),
                    message,
                    images,
                    None,
                )
                .await?
                {
                    return Ok(());
                }
            }
            RpcCommand::Abort { id } => {
                runner.host.steering_queue.lock().unwrap().clear();
                emit_or_print(
                    &output,
                    rpc_response("abort", id.as_deref(), true, json!({"aborted":false})),
                    true,
                )?;
            }
            RpcCommand::Compact { id, instructions } => {
                let mut candidate = runner.session.clone();
                let stats =
                    candidate.compact(DEFAULT_KEEP_RECENT_MESSAGES, instructions.as_deref());
                match runner.commit_session(candidate, runner.path.clone()) {
                    Ok(()) => emit_or_print(
                        &output,
                        rpc_response("compact", id.as_deref(), true, json!(stats)),
                        true,
                    )?,
                    Err(error) => emit_or_print(
                        &output,
                        rpc_response("compact", id.as_deref(), false, json!({"error": error})),
                        true,
                    )?,
                }
            }
            RpcCommand::SetPinnedContext { id, context } => {
                let mut candidate = runner.session.clone();
                candidate.pinned_context = context;
                match runner.commit_session(candidate, runner.path.clone()) {
                    Ok(()) => emit_or_print(
                        &output,
                        rpc_response(
                            "set_pinned_context",
                            id.as_deref(),
                            true,
                            json!({"pinnedContext": runner.session.pinned_context}),
                        ),
                        true,
                    )?,
                    Err(error) => emit_or_print(
                        &output,
                        rpc_response(
                            "set_pinned_context",
                            id.as_deref(),
                            false,
                            json!({"error": error}),
                        ),
                        true,
                    )?,
                }
            }
            RpcCommand::GetState { id } => {
                emit_or_print(
                    &output,
                    rpc_response(
                        "get_state",
                        id.as_deref(),
                        true,
                        rpc_state(runner, false, 0),
                    ),
                    true,
                )?;
            }
            RpcCommand::SetModel {
                id,
                model,
                thinking,
            } => match runner.set_model(&model, thinking) {
                Ok(()) => emit_or_print(
                    &output,
                    rpc_response(
                        "set_model",
                        id.as_deref(),
                        true,
                        json!({"model":runner.config.model,"provider":runner.provider_name,"thinkingLevel":runner.options.thinking,"modelCapabilities":model_capabilities(runner)}),
                    ),
                    true,
                )?,
                Err(error) => emit_or_print(
                    &output,
                    rpc_response("set_model", id.as_deref(), false, json!({"error":error})),
                    true,
                )?,
            },

            RpcCommand::NewSession { id, name } => match Session::new(cwd) {
                Ok(mut s) => {
                    s.set_name(name);
                    let path = directory.join(format!("{}.json", s.id));
                    match runner.commit_session(s, path) {
                        Ok(()) => emit_or_print(
                            &output,
                            rpc_response(
                                "new_session",
                                id.as_deref(),
                                true,
                                json!({"sessionId": runner.session.id, "name": runner.session.name}),
                            ),
                            true,
                        )?,
                        Err(error) => emit_or_print(
                            &output,
                            rpc_response(
                                "new_session",
                                id.as_deref(),
                                false,
                                json!({"error": error}),
                            ),
                            true,
                        )?,
                    }
                }
                Err(error) => emit_or_print(
                    &output,
                    rpc_response("new_session", id.as_deref(), false, json!({"error": error})),
                    true,
                )?,
            },
            RpcCommand::ForkSession { id } => {
                let candidate = runner.session.fork();
                let path = directory.join(format!("{}.json", candidate.id));
                match runner.commit_session(candidate, path) {
                    Ok(()) => emit_or_print(
                        &output,
                        rpc_response(
                            "fork_session",
                            id.as_deref(),
                            true,
                            json!({"sessionId": runner.session.id}),
                        ),
                        true,
                    )?,
                    Err(error) => emit_or_print(
                        &output,
                        rpc_response(
                            "fork_session",
                            id.as_deref(),
                            false,
                            json!({"error": error}),
                        ),
                        true,
                    )?,
                }
            }
            RpcCommand::SetSessionName { id, name } => {
                let mut candidate = runner.session.clone();
                candidate.set_name(Some(name));
                match runner.commit_session(candidate, runner.path.clone()) {
                    Ok(()) => emit_or_print(
                        &output,
                        rpc_response(
                            "set_session_name",
                            id.as_deref(),
                            true,
                            json!({
                                "sessionId": runner.session.id,
                                "sessionName": runner.session.name,
                                "session_name": runner.session.name,
                            }),
                        ),
                        true,
                    )?,
                    Err(error) => emit_or_print(
                        &output,
                        rpc_response(
                            "set_session_name",
                            id.as_deref(),
                            false,
                            json!({"error": error}),
                        ),
                        true,
                    )?,
                }
            }
            RpcCommand::ExportHtml { id, output_path } => {
                let out_path = match output_path {
                    Some(ref p) => {
                        let pb = PathBuf::from(p);
                        if pb.is_absolute() { pb } else { cwd.join(pb) }
                    }
                    None => cwd.join(format!("mio-session-{}.html", runner.session.id)),
                };
                match runner.session.export_html(&out_path) {
                    Ok(()) => {
                        emit_or_print(
                            &output,
                            rpc_response(
                                "export_html",
                                id.as_deref(),
                                true,
                                json!({"path": out_path}),
                            ),
                            true,
                        )?;
                    }
                    Err(err) => {
                        emit_or_print(
                            &output,
                            rpc_response(
                                "export_html",
                                id.as_deref(),
                                false,
                                json!({"error": err}),
                            ),
                            true,
                        )?;
                    }
                }
            }
            RpcCommand::ApproveResponse {
                request_id,
                approved,
            } => {
                let res = match &runner.host.rpc_approvals {
                    Some(broker) => broker.resolve(&request_id, approved),
                    None => Err("Approval broker is not enabled".to_string()),
                };
                let value = match res {
                    Ok(()) => rpc_response(
                        "approve_response",
                        Some(&request_id),
                        true,
                        json!({"approved": approved}),
                    ),
                    Err(err) => rpc_response(
                        "approve_response",
                        Some(&request_id),
                        false,
                        json!({"error": err}),
                    ),
                };
                emit_or_print(&output, value, true)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transport_failure_requests_failed_without_a_cancel_request() {
        for end in [
            RpcTurnEnd::OutputFailed("transport died".into()),
            RpcTurnEnd::InputError("input died".into()),
        ] {
            let (settlement, state, cancel) = turn_end_settlement(&end);
            assert!(matches!(settlement, Settlement::Failed), "{end:?}");
            assert_eq!(state, RunState::Failed, "{end:?}");
            assert!(
                !cancel,
                "a transport/input failure must not record a cancel request: {end:?}"
            );
        }
        for end in [RpcTurnEnd::Abort(None), RpcTurnEnd::Closed] {
            let (settlement, state, cancel) = turn_end_settlement(&end);
            assert!(matches!(settlement, Settlement::Cancelled), "{end:?}");
            assert_eq!(state, RunState::Cancelled, "{end:?}");
            assert!(cancel, "a normal cancel records its real request: {end:?}");
        }
        let (settlement, state, cancel) =
            turn_end_settlement(&RpcTurnEnd::Finished(RunState::Completed, Ok(())));
        assert!(matches!(settlement, Settlement::Completed));
        assert_eq!(state, RunState::Completed);
        assert!(!cancel);
    }
}
