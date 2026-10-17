//! DES-09 native subprocess supervision: typed pre-spawn vs after-spawn failure.
//!
//! These tests drive the **real** supervision path (`shell::execute_with_timeout`
//! and the [`ScopedTool`] wrapper) against real local subprocesses. No provider,
//! credential or network is involved.
//!
//! What is asserted:
//!
//! - a private short timeout shares production's exact supervision path, and a
//!   real sentinel command that already wrote a file before the timeout is
//!   reported as an after-spawn loss (the effect may have happened);
//! - that after-spawn loss becomes a typed `UnknownSideEffect` in the real
//!   dispatch context and the run-shared fence, with the real tool-call id and
//!   no raw command copied into the bounded target;
//! - a pre-spawn (spawn/backend) failure is a known failure with no unknown
//!   fence, and the legacy `String` APIs are unchanged;
//! - a confirmed non-zero exit is a known failure, never an unknown;
//! - the 8 MiB complete-output bound is a typed capture loss and still rolls
//!   back the incomplete artifact;
//! - a new write is denied while the fence holds that unknown, and a fresh
//!   tool-call id is admitted only after a separate host confirmation of the
//!   exact evidence, without any process restart;
//! - a direct, engine-less call has no identity, no collector and no fence.

#![cfg(unix)]

use mio_agent::agent::ToolInvocation;
use mio_coding_agent::policy::{NormalizedCall, PolicyContext, ToolPolicy};
use mio_coding_agent::shell::{
    SupervisionLoss, execute_with_timeout, run_supervised, run_supervised_with_timeout,
};
use mio_coding_agent::tool_identity::{self, ScopedTool};
use mio_coding_agent::unknown_effect::{self, UnknownReason, UnknownSideEffect, fence_for_scope};
use mio_coding_agent::{AgentEvent, Host, HostFuture, ToolFilter};
use mio_core::tool::{PortableTool, Tool, ToolContext, ToolOutput};
use serde_json::{Value, json};
use std::{
    convert::Infallible,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use uuid::Uuid;

/// A real tool-call context carrying the engine's dispatch identity, exactly
/// the way the bus attaches it for a real tool call.
fn context_with(tool_call_id: &str) -> ToolContext {
    let invocation = ToolInvocation {
        effect_id: 11,
        tool_call_id: tool_call_id.to_owned(),
        turn: 1,
    };
    ToolContext::new().with_scope(Arc::new(invocation))
}

/// A static tool that really runs a supervised shell command and returns its
/// text as a normal output, so the wrapper's own typed-unknown path is the only
/// thing under test.
#[derive(Clone)]
struct ShellProbe {
    cwd: PathBuf,
    command: String,
    timeout: Duration,
}

impl PortableTool for ShellProbe {
    const NAME: &'static str = "native-shell-probe";
    type Args = Value;
    type Output = ToolOutput;
    type Error = Infallible;

    fn description(&self) -> String {
        "run a supervised shell command".into()
    }

    fn parameters(&self) -> Value {
        json!({"type": "object"})
    }

    async fn call(&self, _arguments: Value) -> Result<ToolOutput, Infallible> {
        let text = match execute_with_timeout(&self.command, &self.cwd, self.timeout).await {
            Ok(text) | Err(text) => text,
        };
        Ok(ToolOutput::text(text))
    }
}

#[derive(Default)]
struct RecordingHost {
    recorded: Mutex<Vec<UnknownSideEffect>>,
}

impl Host for RecordingHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { panic!("these tests run the native shell path directly") })
    }
    fn on_event(&self, _: AgentEvent) {}
    fn on_unknown_side_effect(&self, effect: &UnknownSideEffect) -> Result<(), String> {
        self.recorded.lock().unwrap().push(effect.clone());
        Ok(())
    }
}

#[derive(Default)]
struct ConfirmingHost {
    confirmations: Mutex<Vec<Vec<UnknownSideEffect>>>,
}
impl Host for ConfirmingHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { panic!("admission tests must not execute a shell") })
    }
    fn on_event(&self, _: AgentEvent) {}
    fn on_unknown_side_effect(&self, effect: &UnknownSideEffect) -> Result<(), String> {
        self.confirmations
            .lock()
            .unwrap()
            .push(vec![effect.clone()]);
        Ok(())
    }
    fn confirm_unknown_side_effect(
        &self,
        _: &NormalizedCall,
        unknown: &[UnknownSideEffect],
    ) -> HostFuture<Result<(), String>> {
        self.confirmations.lock().unwrap().push(unknown.to_vec());
        Box::pin(async { Ok(()) })
    }
}

/// A host whose durable unknown record cannot be written.
struct FailingHost;

impl Host for FailingHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { panic!("persistence tests must not execute a shell") })
    }
    fn on_event(&self, _: AgentEvent) {}
    fn on_unknown_side_effect(&self, _: &UnknownSideEffect) -> Result<(), String> {
        Err("the durable unknown record could not be written".into())
    }
}

fn output_artifact_count(root: &Path) -> usize {
    std::fs::read_dir(root.join(".mio/outputs"))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().is_file())
                .count()
        })
        .unwrap_or_default()
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A direct, engine-less call has no dispatch identity. A real sentinel already
/// ran before the timeout, but nothing is invented: the String error is
/// unchanged and neither a collector nor a fence is created.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_direct_engine_less_timeout_stays_a_string_without_invented_identity() {
    let dir = tempfile::tempdir().unwrap();
    let sentinel = dir.path().join("ran.txt");
    let error = execute_with_timeout(
        "printf ran > ran.txt; sleep 30",
        dir.path(),
        Duration::from_millis(300),
    )
    .await
    .unwrap_err();
    assert!(error.contains("timed out"), "{error}");
    wait_for(&sentinel);
    assert!(tool_identity::current().is_none());
    assert!(unknown_effect::current_unknown_effect_collector().is_none());
}

/// A pre-spawn failure is a fact the supervisor observed: nothing ran, so it is
/// never an unknown, and the legacy `String` API keeps the same message.
#[tokio::test]
async fn a_spawn_failure_is_pre_spawn_and_never_an_unknown() {
    let mut command = tokio::process::Command::new("/mio-no-such-binary-native-supervision");
    let error = run_supervised_with_timeout(&mut command, Duration::from_secs(5))
        .await
        .unwrap_err();
    assert_eq!(error.loss(), None);
    assert!(!error.is_post_spawn());

    let mut command = tokio::process::Command::new("/mio-no-such-binary-native-supervision");
    let legacy: String = run_supervised(&mut command).await.unwrap_err();
    assert!(legacy.contains("Cannot start process"), "{legacy}");
}

/// A confirmed non-zero exit is a known failure: the wrapper records no typed
/// unknown and never trips the run fence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_known_nonzero_exit_is_not_an_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let run_id = Uuid::new_v4();
    let fence = fence_for_scope(&run_id.to_string());
    let host = Arc::new(RecordingHost::default());
    let probe = ShellProbe {
        cwd: dir.path().to_path_buf(),
        command: "exit 7".into(),
        timeout: Duration::from_secs(10),
    };
    let scoped = ScopedTool::new(probe, run_id).with_host(host.clone());
    let mut context = context_with("call-exit");
    let _ = <ScopedTool<ShellProbe> as Tool>::call(&scoped, &mut context, json!({}))
        .await
        .unwrap();
    assert!(context.result::<UnknownSideEffect>().unwrap().is_none());
    assert!(!fence.has_unknown());
    assert!(host.recorded.lock().unwrap().is_empty());
}

/// A real timeout after the sentinel already ran becomes one typed
/// `native_timeout` marker: real identity, a bounded non-raw target, the run
/// fence tripped, and exactly one durable host observation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_timeout_is_a_typed_unknown_in_context_and_fence() {
    let dir = tempfile::tempdir().unwrap();
    let sentinel = dir.path().join("ran.txt");
    let run_id = Uuid::new_v4();
    let fence = fence_for_scope(&run_id.to_string());
    let host = Arc::new(RecordingHost::default());
    let probe = ShellProbe {
        cwd: dir.path().to_path_buf(),
        command: "printf ran > ran.txt; sleep 30".into(),
        timeout: Duration::from_millis(300),
    };
    let scoped = ScopedTool::new(probe, run_id).with_host(host.clone());
    let mut context = context_with("call-timeout");
    let _ = <ScopedTool<ShellProbe> as Tool>::call(&scoped, &mut context, json!({}))
        .await
        .unwrap();
    wait_for(&sentinel);

    let marker = context
        .result::<UnknownSideEffect>()
        .unwrap()
        .expect("a typed unknown marker must land in the real dispatch context");
    assert_eq!(marker.reason, UnknownReason::NativeTimeout);
    assert_eq!(marker.scope, Some(run_id));
    assert_eq!(marker.tool_call_id.as_deref(), Some("call-timeout"));
    assert!(
        !marker.target.contains("run.txt") && !marker.target.contains("sleep"),
        "the raw command must never be copied into the bounded target: {}",
        marker.target
    );
    assert!(marker.target.contains("sha256"), "{}", marker.target);

    assert!(fence.has_unknown());
    let recorded = host.recorded.lock().unwrap();
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0].evidence_id, marker.evidence_id);
    assert_eq!(recorded[0].reason, UnknownReason::NativeTimeout);
}

/// The 8 MiB complete-output bound is a capture loss after the process already
/// ran: typed `native_capture`, and the incomplete artifact is still rolled
/// back rather than published.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_eight_mib_capture_loss_is_a_typed_unknown_and_rolls_back() {
    let dir = tempfile::tempdir().unwrap();
    let run_id = Uuid::new_v4();
    let fence = fence_for_scope(&run_id.to_string());
    let probe = ShellProbe {
        cwd: dir.path().to_path_buf(),
        command: "head -c 8388609 /dev/zero".into(),
        timeout: Duration::from_secs(30),
    };
    let scoped = ScopedTool::new(probe, run_id);
    let mut context = context_with("call-capture");
    let _ = <ScopedTool<ShellProbe> as Tool>::call(&scoped, &mut context, json!({}))
        .await
        .unwrap();

    let marker = context
        .result::<UnknownSideEffect>()
        .unwrap()
        .expect("the capture loss must be a typed unknown");
    assert_eq!(marker.reason, UnknownReason::NativeCapture);
    assert!(fence.has_unknown());
    assert_eq!(
        output_artifact_count(dir.path()),
        0,
        "an incomplete capture must not leave a published artifact"
    );
}

/// The typed loss is also observable directly on the supervision API, so a test
/// can distinguish timeout, wait and capture without parsing a message.
#[tokio::test]
async fn the_legacy_string_api_and_the_typed_loss_agree() {
    let mut command = tokio::process::Command::new("sh");
    command.args(["-c", "sleep 30"]);
    let error = run_supervised_with_timeout(&mut command, Duration::from_millis(200))
        .await
        .unwrap_err();
    assert_eq!(error.loss(), Some(SupervisionLoss::Timeout));
    assert!(error.into_message().contains("timed out"));

    let mut command = tokio::process::Command::new("sh");
    command.args(["-c", "head -c 8388609 /dev/zero"]);
    let error = run_supervised_with_timeout(&mut command, Duration::from_secs(30))
        .await
        .unwrap_err();
    assert_eq!(error.loss(), Some(SupervisionLoss::Capture));
    assert!(error.into_message().contains("limit"));
}

/// After the real shell timeout, a new write needs a fresh tool-call id and an
/// explicit host confirmation of the exact evidence. Nothing here restarts a
/// process or clears the fence: the same in-memory evidence gates the write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_write_is_denied_until_a_separate_host_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let run_id = Uuid::new_v4();
    let fence = fence_for_scope(&run_id.to_string());
    let probe = ShellProbe {
        cwd: dir.path().to_path_buf(),
        command: "sleep 30".into(),
        timeout: Duration::from_millis(300),
    };
    let scoped = ScopedTool::new(probe, run_id);
    let mut context = context_with("call-timeout-2");
    let _ = <ScopedTool<ShellProbe> as Tool>::call(&scoped, &mut context, json!({}))
        .await
        .unwrap();
    assert!(fence.has_unknown());

    let context_args =
        || json!({"operation": "write", "path": "a.txt", "content": "x"}).to_string();

    // No confirmation: the default host refuses a new side effect.
    let denying = ToolPolicy::new(
        ToolFilter::all(),
        false,
        Arc::new(RecordingHost::default()),
        PolicyContext::new(dir.path().display().to_string(), run_id.to_string()),
    )
    .with_unknown_fence(fence.clone());
    let denied = denying
        .admit_with_call_id("workspace", &context_args(), Some("call-denied"))
        .await;
    assert!(denied.is_err(), "the fence must deny a new write");

    // An explicit confirmation of the exact bumped evidence releases a *new*
    // tool-call id, and the call's normal approval still runs afterwards.
    let confirming = Arc::new(ConfirmingHost::default());
    let policy = ToolPolicy::new(
        ToolFilter::all(),
        false,
        confirming.clone(),
        PolicyContext::new(dir.path().display().to_string(), run_id.to_string()),
    )
    .with_unknown_fence(fence.clone());
    let admitted = policy
        .admit_with_call_id("workspace", &context_args(), Some("call-admitted"))
        .await;
    assert!(admitted.is_ok(), "{admitted:?}");
    let seen = confirming.confirmations.lock().unwrap();
    assert!(
        seen.iter().any(|unknown| unknown
            .iter()
            .any(|effect| effect.reason == UnknownReason::NativeTimeout)),
        "the host must be shown the real persisted evidence: {seen:?}"
    );
}

/// A host that cannot durably record the unknown must fail the run closed rather
/// than let the loss disappear: the typed marker still lands in the real
/// dispatch context, and a later write is refused without asking that host to
/// confirm evidence it could not persist.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_persistence_failure_fails_closed_without_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let run_id = Uuid::new_v4();
    let fence = fence_for_scope(&run_id.to_string());
    let probe = ShellProbe {
        cwd: dir.path().to_path_buf(),
        command: "sleep 30".into(),
        timeout: Duration::from_millis(300),
    };
    let scoped = ScopedTool::new(probe, run_id).with_host(Arc::new(FailingHost));
    let mut context = context_with("call-persist-failure");
    let _ = <ScopedTool<ShellProbe> as Tool>::call(&scoped, &mut context, json!({}))
        .await
        .unwrap();

    let marker = context
        .result::<UnknownSideEffect>()
        .unwrap()
        .expect("the typed marker must still land in the dispatch context");
    assert_eq!(marker.reason, UnknownReason::NativeTimeout);
    let evidence = fence.evidence();
    assert!(evidence.has_unknown());
    assert!(
        evidence.persist_failed() && evidence.is_fail_closed(),
        "a host that cannot persist the record must fail the run closed: {evidence:?}"
    );

    let confirming = Arc::new(ConfirmingHost::default());
    let policy = ToolPolicy::new(
        ToolFilter::all(),
        false,
        confirming.clone(),
        PolicyContext::new(dir.path().display().to_string(), run_id.to_string()),
    )
    .with_unknown_fence(fence.clone());
    let denied = policy
        .admit_with_call_id(
            "workspace",
            &json!({"operation": "write", "path": "a.txt", "content": "x"}).to_string(),
            Some("call-after-persist-failure"),
        )
        .await;
    assert!(
        denied.is_err(),
        "unpersisted evidence must refuse a new write: {denied:?}"
    );
    assert!(
        confirming.confirmations.lock().unwrap().is_empty(),
        "the host must not be asked to confirm evidence it could not persist"
    );
}
