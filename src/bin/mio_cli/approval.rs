//! 审批代理与 OSC 7501 程序状态上报。
//!
//! RPC 模式通过 `RpcApprovals` 完成 `approval_request` / `approve_response` 往返；
//! `StatusReporter` 仅在终端文本模式且 stderr 为 TTY 时输出状态序列。

use mio_coding_agent::{HostFuture, ProgramState, format_osc7501};
use serde_json::json;

use crate::json_output::{CorrelationFields, JsonOutput, emit_or_print_correlated};
use std::{
    collections::BTreeMap,
    io::{self, Write},
    sync::{Arc, Mutex, atomic::AtomicBool},
};
use tokio::sync::oneshot;
use uuid::Uuid;

/// RPC 回合相关队列的统一容量上限：审批 pending、steering 与 follow-up 共用。
///
/// 定义在审批模块（依赖图的叶子），避免 approval → rpc 的反向依赖。
pub(super) const RPC_QUEUE_LIMIT: usize = 64;

pub(super) struct StatusReporter {
    enabled: bool,
    pub(super) cancelled: AtomicBool,
    last_seq: Mutex<Option<String>>,
}

impl StatusReporter {
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            cancelled: AtomicBool::new(false),
            last_seq: Mutex::new(None),
        }
    }

    pub(super) fn report(&self, state: ProgramState, msg: Option<&str>) {
        if !self.enabled {
            return;
        }
        let seq = format_osc7501(&state, msg);
        let mut guard = self.last_seq.lock().unwrap();
        if guard.as_ref() == Some(&seq) {
            return;
        }
        *guard = Some(seq.clone());
        let mut stderr = io::stderr().lock();
        let _ = stderr.write_all(seq.as_bytes());
        let _ = stderr.flush();
    }
}

struct PendingGuard {
    broker: Arc<RpcApprovals>,
    request_id: String,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        let mut pending = self.broker.pending.lock().unwrap();
        pending.remove(&self.request_id);
    }
}

struct PendingApproval {
    sender: oneshot::Sender<bool>,
    expires_at: tokio::time::Instant,
}

pub(super) struct RpcApprovals {
    pending: Mutex<BTreeMap<String, PendingApproval>>,
    timeout: std::time::Duration,
    /// 审批请求是关键事件：经有界传输发送，投递失败时 fail-closed 而不是静默等待。
    output: Option<JsonOutput>,
}

impl RpcApprovals {
    #[cfg(test)]
    pub(super) fn new(timeout: std::time::Duration) -> Self {
        Self {
            pending: Mutex::new(BTreeMap::new()),
            timeout,
            output: None,
        }
    }

    pub(super) fn with_output(timeout: std::time::Duration, output: Option<JsonOutput>) -> Self {
        Self {
            pending: Mutex::new(BTreeMap::new()),
            timeout,
            output,
        }
    }

    pub(super) fn pending_count(&self) -> usize {
        self.pending.lock().unwrap().len()
    }

    /// Test-only: the real pending request ids, in order, so an in-process
    /// loopback client can answer the exact request the broker emitted.
    #[cfg(test)]
    pub(super) fn pending_request_ids(&self) -> Vec<String> {
        self.pending.lock().unwrap().keys().cloned().collect()
    }

    pub(super) fn resolve(&self, request_id: &str, approved: bool) -> Result<(), String> {
        let entry = {
            let mut pending = self.pending.lock().unwrap();
            pending.remove(request_id)
        };
        match entry {
            Some(pending) => {
                if tokio::time::Instant::now() >= pending.expires_at {
                    return Err(format!(
                        "Unknown, expired, or already resolved approval request: '{request_id}'"
                    ));
                }
                pending
                    .sender
                    .send(approved)
                    .map_err(|_| "Approval receiver dropped".to_string())
            }
            None => Err(format!(
                "Unknown, expired, or already resolved approval request: '{request_id}'"
            )),
        }
    }

    pub(super) fn request(
        self: &Arc<Self>,
        operation: &str,
        target: &str,
    ) -> HostFuture<Result<(), String>> {
        self.request_bound(operation, target, None, None)
    }

    /// 发起一次绑定审批，并带上真实运行关联字段。
    ///
    /// `correlation` 的 `runId` 来自审批绑定自己的 `scope`，`toolCallId` 来自
    /// 绑定携带的模型工具调用 id；审批尝试的 `callId` 已包含在 `binding` 里，
    /// 不会被冒充为模型 toolCallId。
    pub(super) fn request_bound(
        self: &Arc<Self>,
        operation: &str,
        target: &str,
        binding: Option<&mio_coding_agent::policy::ApprovalBinding>,
        correlation: Option<&CorrelationFields>,
    ) -> HostFuture<Result<(), String>> {
        let (tx, rx) = oneshot::channel();
        let request_id = Uuid::new_v4().to_string();
        let timeout_ms = self.timeout.as_millis().min(u64::MAX as u128) as u64;
        let expires_at = tokio::time::Instant::now() + self.timeout;

        {
            let mut pending = self.pending.lock().unwrap();
            if pending.len() >= RPC_QUEUE_LIMIT {
                return Box::pin(std::future::ready(Err(
                    "Too many pending approval requests".to_string(),
                )));
            }
            pending.insert(
                request_id.clone(),
                PendingApproval {
                    sender: tx,
                    expires_at,
                },
            );
        }

        // 审批请求不可静默丢失：投递失败时移除 pending，让调用方看到显式错误。
        let correlation = correlation.cloned().unwrap_or_default();
        if let Err(error) = emit_or_print_correlated(
            &self.output,
            json!({
                "type": "approval_request",
                "request_id": request_id,
                "operation": operation,
                "target": target,
                "timeout_ms": timeout_ms,
                "binding": binding,
            }),
            true,
            &correlation,
        ) {
            self.pending.lock().unwrap().remove(&request_id);
            return Box::pin(std::future::ready(Err(format!(
                "Approval request could not be delivered; refusing to wait on an undeliverable request: {error}"
            ))));
        }

        let broker = self.clone();
        let op = operation.to_string();
        let guard = PendingGuard { broker, request_id };

        Box::pin(async move {
            let _guard = guard;

            let res = tokio::time::timeout_at(expires_at, rx).await;
            match res {
                Ok(Ok(true)) => Ok(()),
                Ok(Ok(false)) => Err(format!("User rejected tool operation '{op}'")),
                Ok(Err(_)) => Err("Approval request was canceled or receiver dropped".to_string()),
                Err(_) => Err(format!("Approval request timed out for tool '{op}'")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rpc_approvals_guard_and_resolution() {
        let approvals = Arc::new(RpcApprovals::new(std::time::Duration::from_secs(60)));

        // 1. drop unpolled future -> pending_count 清零
        let fut = approvals.request("write", "a.txt");
        assert_eq!(approvals.pending_count(), 1);
        drop(fut);
        assert_eq!(approvals.pending_count(), 0);

        // 2. drop polled future -> pending_count 清零，resolve 旧 id 失败
        let mut fut = approvals.request("write", "b.txt");
        let id = {
            let pending = approvals.pending.lock().unwrap();
            pending.keys().next().cloned().unwrap()
        };
        assert_eq!(approvals.pending_count(), 1);
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        assert!(std::future::Future::poll(fut.as_mut(), &mut cx).is_pending());
        assert_eq!(approvals.pending_count(), 1);
        drop(fut);
        assert_eq!(approvals.pending_count(), 0);
        assert!(approvals.resolve(&id, true).is_err());

        // 3. timeout: new(Duration::ZERO) 运行 await，pending 归零且旧 id 失败
        let zero_approvals = Arc::new(RpcApprovals::new(std::time::Duration::ZERO));
        let zero_fut = zero_approvals.request("write", "c.txt");
        let zero_id = {
            let pending = zero_approvals.pending.lock().unwrap();
            pending.keys().next().cloned().unwrap()
        };
        let res = zero_fut.await;
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("timed out"));
        assert_eq!(zero_approvals.pending_count(), 0);
        assert!(zero_approvals.resolve(&zero_id, true).is_err());

        // 3b. Duration::ZERO 但不 poll request 直接 resolve 必须 Err 且 pending 归零
        let zero_approvals2 = Arc::new(RpcApprovals::new(std::time::Duration::ZERO));
        let zero_fut2 = zero_approvals2.request("write", "unpolled.txt");
        let zero_id2 = {
            let pending = zero_approvals2.pending.lock().unwrap();
            pending.keys().next().cloned().unwrap()
        };
        assert_eq!(zero_approvals2.pending_count(), 1);
        // 不 poll zero_fut2，直接 resolve 已经到期的请求
        let resolve_res = zero_approvals2.resolve(&zero_id2, true);
        assert!(resolve_res.is_err());
        assert_eq!(zero_approvals2.pending_count(), 0);
        drop(zero_fut2);
        assert_eq!(zero_approvals2.pending_count(), 0);

        // 4. pending 容量 64 及 drop 后可再次申请（不用等 120 秒）
        let limit_approvals = Arc::new(RpcApprovals::new(std::time::Duration::from_secs(120)));
        let mut held_futs = Vec::new();
        for i in 0..RPC_QUEUE_LIMIT {
            held_futs.push(limit_approvals.request("write", &format!("file_{i}.txt")));
        }
        assert_eq!(limit_approvals.pending_count(), RPC_QUEUE_LIMIT);
        // 第 65 个请求应直接失败
        let overflow_fut = limit_approvals.request("write", "overflow.txt");
        let overflow_res = overflow_fut.await;
        assert!(overflow_res.is_err());
        assert!(overflow_res.unwrap_err().contains("Too many pending"));
        assert_eq!(limit_approvals.pending_count(), RPC_QUEUE_LIMIT);

        // drop 其中一个后无需等待 120 秒，即可再次成功申请
        held_futs.pop();
        assert_eq!(limit_approvals.pending_count(), RPC_QUEUE_LIMIT - 1);
        let new_fut = limit_approvals.request("write", "replacement.txt");
        assert_eq!(limit_approvals.pending_count(), RPC_QUEUE_LIMIT);
        drop(new_fut);
        assert_eq!(limit_approvals.pending_count(), RPC_QUEUE_LIMIT - 1);
        drop(held_futs);
        assert_eq!(limit_approvals.pending_count(), 0);

        // 4. 两个 pending 反序 resolve 各自结果正确
        let keys_before: std::collections::BTreeSet<String> =
            approvals.pending.lock().unwrap().keys().cloned().collect();
        let fut1 = approvals.request("write", "first.txt");
        let id1 = approvals
            .pending
            .lock()
            .unwrap()
            .keys()
            .find(|k| !keys_before.contains(*k))
            .cloned()
            .unwrap();

        let keys_before2: std::collections::BTreeSet<String> =
            approvals.pending.lock().unwrap().keys().cloned().collect();
        let fut2 = approvals.request("write", "second.txt");
        let id2 = approvals
            .pending
            .lock()
            .unwrap()
            .keys()
            .find(|k| !keys_before2.contains(*k))
            .cloned()
            .unwrap();
        assert_eq!(approvals.pending_count(), 2);

        let handle1 = tokio::spawn(fut1);
        let handle2 = tokio::spawn(fut2);

        // 反序 resolve：先 resolve id2 为 false，后 resolve id1 为 true
        assert!(approvals.resolve(&id2, false).is_ok());
        assert_eq!(approvals.pending_count(), 1);
        assert!(approvals.resolve(&id1, true).is_ok());
        assert_eq!(approvals.pending_count(), 0);

        let res2 = handle2.await.unwrap();
        let res1 = handle1.await.unwrap();
        assert!(res2.is_err());
        assert!(res2.unwrap_err().contains("rejected"));
        assert!(res1.is_ok());
    }
}
