//! Confirmation and registry limits must not erase an existing unknown effect.
use mio_coding_agent::{
    AgentEvent, Host, HostFuture, ToolFilter,
    policy::{NormalizedCall, PolicyContext, ToolPolicy},
    unknown_effect::{
        MAX_FENCE_SCOPES, MAX_UNKNOWN_EFFECTS, UnknownEffectFence, UnknownReason,
        UnknownSideEffect, alias_scope, fence_for_scope,
    },
};
use std::{
    ffi::OsStr,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Default)]
struct ConfirmingHost {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    confirmed_count: AtomicUsize,
    approvals: AtomicUsize,
}

impl Host for ConfirmingHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { panic!("admission tests must not execute a shell") })
    }
    fn on_event(&self, _: AgentEvent) {}
    fn approve_tool(&self, _: &str, _: &str) -> Result<(), String> {
        self.approvals.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn confirm_unknown_side_effect(
        &self,
        _: &NormalizedCall,
        unknown: &[UnknownSideEffect],
    ) -> HostFuture<Result<(), String>> {
        self.confirmed_count.store(unknown.len(), Ordering::SeqCst);
        let entered = self.entered.clone();
        let release = self.release.clone();
        Box::pin(async move {
            entered.notify_one();
            release.notified().await;
            Ok(())
        })
    }
}

fn unknown(id: &str) -> UnknownSideEffect {
    UnknownSideEffect::new(
        "mcp__probe__write",
        "target",
        UnknownReason::McpTransport,
        Some(id.to_owned()),
    )
}

/// A registry pressure case fills the process-global bounded fence registry
/// (`MAX_FENCE_SCOPES` live scopes). Two such cases sharing one libtest process
/// fight over that single registry: whichever reaches the bound first makes the
/// other's parent scope resolve to an unavailable guard instead of a live
/// registration, so the loser fails for a reason unrelated to its assertions.
///
/// Each pressure case therefore re-executes itself in a dedicated child process
/// of this same test binary (`--exact <case> --test-threads=1`) so it owns a
/// private registry. The parent only supervises the child: it requires a zero
/// exit *and* proof that the child executed exactly one test and failed none, so
/// a filtered-out child (zero tests) can never be mistaken for a pass. The case
/// body, including every assertion, still runs in the child; only the registry
/// pressure is isolated. Non-pressure cases keep the default in-process
/// parallelism.
const ISOLATION_ENV: &str = "MIO_UNKNOWN_EFFECT_BOUNDARY_CASE";
const ISOLATION_TIMEOUT: Duration = Duration::from_secs(15);
const ISOLATION_POLL: Duration = Duration::from_millis(10);

/// Loader and temp-path variables a re-executed test binary may need. The child
/// is started with a cleared environment, so the harness' API keys, tokens and
/// workspace secrets are neither inherited nor echoed back on failure.
const RETAINED_ENVIRONMENT: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "TMP",
    "TEMP",
    "LD_LIBRARY_PATH",
    "LD_PRELOAD",
    "LD_AUDIT",
    "DYLD_LIBRARY_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "SystemRoot",
    "SYSTEMROOT",
    "windir",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "RUST_BACKTRACE",
    "RUST_MIN_STACK",
];

/// `true` only in the dedicated child process for `case`, where the caller runs
/// the real body; otherwise this supervises such a child and returns `false` so
/// the caller returns without touching the parent's shared registry.
fn runs_in_isolated_case(case: &str) -> bool {
    if std::env::var_os(ISOLATION_ENV).as_deref() == Some(OsStr::new(case)) {
        return true;
    }
    supervise_isolated_case(case);
    false
}

fn supervise_isolated_case(case: &str) {
    let binary = std::env::current_exe()
        .unwrap_or_else(|error| panic!("cannot locate the test binary for `{case}`: {error}"));
    let mut command = Command::new(&binary);
    command
        .arg("--exact")
        .arg(case)
        .arg("--test-threads=1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear();
    for name in RETAINED_ENVIRONMENT {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.env(ISOLATION_ENV, case);

    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("failed to spawn the isolated `{case}` case: {error}"));
    let stdout = child
        .stdout
        .take()
        .expect("the isolated case pipes its stdout");
    let stderr = child
        .stderr
        .take()
        .expect("the isolated case pipes its stderr");
    let stdout_reader = std::thread::spawn(move || drain(stdout));
    let stderr_reader = std::thread::spawn(move || drain(stderr));

    let deadline = Instant::now() + ISOLATION_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(ISOLATION_POLL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let stdout = stdout_reader.join().unwrap_or_default();
                let stderr = stderr_reader.join().unwrap_or_default();
                panic!(
                    "the isolated `{case}` case did not finish within {ISOLATION_TIMEOUT:?}\n\
                     stdout:\n{stdout}\nstderr:\n{stderr}"
                );
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("failed to observe the isolated `{case}` case: {error}");
            }
        }
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();

    assert!(
        status.success(),
        "the isolated `{case}` case exited with {status}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains(&format!("test {case} ... ok")),
        "the isolated child never reported `{case}` as passed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains(" 1 passed; 0 failed"),
        "the isolated child must execute exactly one test and fail none\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

fn drain(mut stream: impl Read) -> String {
    let mut bytes = Vec::new();
    match stream.read_to_end(&mut bytes) {
        Ok(_) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => format!("<failed to read the child output: {error}>"),
    }
}

#[tokio::test]
async fn confirmation_of_old_evidence_cannot_authorize_a_new_unknown() {
    let host = Arc::new(ConfirmingHost::default());
    let fence = UnknownEffectFence::new();
    fence.mark(unknown("old-call"));
    let policy = ToolPolicy::new(
        ToolFilter::all(),
        false,
        host.clone(),
        PolicyContext::new("/workspace", uuid::Uuid::new_v4().to_string()),
    )
    .with_unknown_fence(fence.clone())
    .with_approval_timeout(Duration::from_secs(2));
    let admission = tokio::spawn(async move {
        policy
            .admit_with_call_id("write", r#"{"path":"a","value":"x"}"#, Some("new-call"))
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), host.entered.notified())
        .await
        .unwrap();
    assert_eq!(host.confirmed_count.load(Ordering::SeqCst), 1);
    fence.mark(unknown("arrived-while-confirming"));
    host.release.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(5), admission)
        .await
        .unwrap()
        .unwrap();
    assert!(
        result.is_err(),
        "old evidence was confirmed, not the new unknown"
    );
    assert_eq!(host.approvals.load(Ordering::SeqCst), 0);
}

#[derive(Default)]
struct AutoConfirmHost;

impl Host for AutoConfirmHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { panic!("admission tests must not execute a shell") })
    }
    fn on_event(&self, _: AgentEvent) {}
    fn confirm_unknown_side_effect(
        &self,
        _: &NormalizedCall,
        _: &[UnknownSideEffect],
    ) -> HostFuture<Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn overflow_cannot_be_confirmed_by_an_incomplete_evidence_snapshot() {
    let fence = UnknownEffectFence::new();
    for number in 0..=MAX_UNKNOWN_EFFECTS {
        fence.mark(unknown(&format!("unknown-{number}")));
    }
    assert!(fence.overflowed());
    let policy = ToolPolicy::new(
        ToolFilter::all(),
        false,
        Arc::new(AutoConfirmHost),
        PolicyContext::new("/workspace", uuid::Uuid::new_v4().to_string()),
    )
    .with_unknown_fence(fence);
    let result = policy
        .admit_with_call_id("write", r#"{"path":"a","value":"x"}"#, Some("fresh-call"))
        .await;
    assert!(
        result.is_err(),
        "the host could not inspect the omitted unknown effects"
    );
}

#[test]
fn registry_pressure_cannot_replace_a_live_runs_existing_fence() {
    if !runs_in_isolated_case("registry_pressure_cannot_replace_a_live_runs_existing_fence") {
        return;
    }
    let parent_scope = uuid::Uuid::new_v4().to_string();
    let parent = fence_for_scope(&parent_scope);
    parent.mark(unknown("parent-unknown"));
    let mut held = Vec::with_capacity(MAX_FENCE_SCOPES + 1);
    for _ in 0..=MAX_FENCE_SCOPES {
        held.push(fence_for_scope(&uuid::Uuid::new_v4().to_string()));
    }
    let recovered = fence_for_scope(&parent_scope);
    assert!(
        Arc::ptr_eq(&parent, &recovered),
        "a live parent's children must retain its actual fence under registry pressure"
    );
    assert!(recovered.has_unknown());
    drop(held);
}

/// A persistence failure recorded while the host is still answering an earlier
/// confirmation must fail closed: the confirmation only covered the evidence it
/// was shown, and no parameter-bound approval may run afterwards.
#[tokio::test]
async fn persistence_failure_during_confirmation_fails_closed() {
    let host = Arc::new(ConfirmingHost::default());
    let fence = UnknownEffectFence::new();
    fence.mark(unknown("old-call"));
    let policy = ToolPolicy::new(
        ToolFilter::all(),
        false,
        host.clone(),
        PolicyContext::new("/workspace", uuid::Uuid::new_v4().to_string()),
    )
    .with_unknown_fence(fence.clone())
    .with_approval_timeout(Duration::from_secs(2));
    let admission = tokio::spawn(async move {
        policy
            .admit_with_call_id("write", r#"{"path":"a","value":"x"}"#, Some("persist-call"))
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), host.entered.notified())
        .await
        .unwrap();
    fence.mark_persist_failed();
    host.release.notify_one();
    let error = tokio::time::timeout(Duration::from_secs(5), admission)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(
        error.reason().contains("persist"),
        "the durable-record failure must be named: {}",
        error.reason()
    );
    assert_eq!(host.approvals.load(Ordering::SeqCst), 0);
    let read = ToolPolicy::new(
        ToolFilter::all(),
        false,
        Arc::new(AutoConfirmHost),
        PolicyContext::new("/workspace", uuid::Uuid::new_v4().to_string()),
    )
    .with_unknown_fence(fence.clone());
    assert!(
        read.admit("read", r#"{"path":"a"}"#).await.is_ok(),
        "reads stay verifiable while the fence fails closed"
    );
}

/// A host that only answers the ordinary parameter-bound approval: the fence
/// starts empty, so no unknown confirmation is asked.
#[derive(Default)]
struct ApprovingHost {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    approvals: AtomicUsize,
    confirmations: AtomicUsize,
}

impl Host for ApprovingHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { panic!("admission tests must not execute a shell") })
    }
    fn on_event(&self, _: AgentEvent) {}
    fn approve_tool_binding(
        &self,
        _: &str,
        _: &str,
        _: &mio_coding_agent::policy::ApprovalBinding,
    ) -> HostFuture<Result<(), String>> {
        self.approvals.fetch_add(1, Ordering::SeqCst);
        let entered = self.entered.clone();
        let release = self.release.clone();
        Box::pin(async move {
            entered.notify_one();
            release.notified().await;
            Ok(())
        })
    }
    fn confirm_unknown_side_effect(
        &self,
        _: &NormalizedCall,
        _: &[UnknownSideEffect],
    ) -> HostFuture<Result<(), String>> {
        self.confirmations.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

/// A fence that starts empty must not be bypassed by an ordinary approval: an
/// unknown recorded while the approval is pending refuses the final admit.
#[tokio::test]
async fn an_unknown_during_ordinary_approval_cannot_be_admitted() {
    let host = Arc::new(ApprovingHost::default());
    let fence = UnknownEffectFence::new();
    let policy = ToolPolicy::new(
        ToolFilter::all(),
        false,
        host.clone(),
        PolicyContext::new("/workspace", uuid::Uuid::new_v4().to_string()),
    )
    .with_unknown_fence(fence.clone())
    .with_approval_timeout(Duration::from_secs(2));
    let admission = tokio::spawn(async move {
        policy
            .admit_with_call_id(
                "write",
                r#"{"path":"a","value":"x"}"#,
                Some("approval-call"),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), host.entered.notified())
        .await
        .unwrap();
    assert_eq!(
        host.confirmations.load(Ordering::SeqCst),
        0,
        "an unknown-free fence asks no confirmation"
    );
    fence.mark(unknown("arrived-during-approval"));
    host.release.notify_one();
    let error = tokio::time::timeout(Duration::from_secs(5), admission)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(
        error.reason().contains("new unknown"),
        "the newly observed unknown must be named: {}",
        error.reason()
    );
    assert_eq!(host.approvals.load(Ordering::SeqCst), 1);
}

/// The bounded registry must not evict a live alias either: a child registered
/// before the bound keeps resolving its parent fence, and a later alias is
/// refused into an unavailable (fail-closed) guard instead of an empty one.
#[test]
fn a_refused_alias_never_clears_parent_protection() {
    if !runs_in_isolated_case("a_refused_alias_never_clears_parent_protection") {
        return;
    }
    let parent_scope = uuid::Uuid::new_v4().to_string();
    let parent = fence_for_scope(&parent_scope);
    parent.mark(unknown("parent-unknown"));
    let child_scope = uuid::Uuid::new_v4().to_string();
    assert!(alias_scope(&child_scope, &parent));
    assert!(Arc::ptr_eq(&parent, &fence_for_scope(&child_scope)));

    let mut held = Vec::with_capacity(MAX_FENCE_SCOPES + 1);
    for _ in 0..=MAX_FENCE_SCOPES {
        held.push(fence_for_scope(&uuid::Uuid::new_v4().to_string()));
    }

    assert!(Arc::ptr_eq(&parent, &fence_for_scope(&child_scope)));
    assert!(Arc::ptr_eq(&parent, &fence_for_scope(&parent_scope)));
    let overflow_child = uuid::Uuid::new_v4().to_string();
    assert!(
        !alias_scope(&overflow_child, &parent),
        "a live alias must not be evicted to save another one"
    );
    let resolved = fence_for_scope(&overflow_child);
    assert!(
        Arc::ptr_eq(&resolved, &parent) || resolved.unavailable(),
        "an untracked child must share the parent fence or fail closed, never be an empty executable fence"
    );
    assert!(parent.has_unknown());
    drop(held);
}

/// An association the bounded registry cannot save must fail closed at the
/// gate: a scope must never be gated on a healthy fence its own tools cannot
/// reach.
#[tokio::test]
async fn an_unregistered_association_fails_closed() {
    if !runs_in_isolated_case("an_unregistered_association_fails_closed") {
        return;
    }
    let mut held = Vec::with_capacity(MAX_FENCE_SCOPES + 1);
    for _ in 0..=MAX_FENCE_SCOPES {
        held.push(fence_for_scope(&uuid::Uuid::new_v4().to_string()));
    }

    let parent = UnknownEffectFence::new();
    let policy = ToolPolicy::new(
        ToolFilter::all(),
        false,
        Arc::new(AutoConfirmHost),
        PolicyContext::new("/workspace", uuid::Uuid::new_v4().to_string()),
    )
    .with_unknown_fence(parent.clone());
    let write = policy
        .admit_with_call_id("write", r#"{"path":"a","value":"x"}"#, Some("assoc"))
        .await
        .unwrap_err();
    assert!(
        write.reason().contains("unavailable"),
        "an unassociable scope must fail closed: {}",
        write.reason()
    );
    assert!(policy.admit("read", r#"{"path":"a"}"#).await.is_ok());
    assert!(parent.is_empty(), "the shared fence must be left untouched");
    drop(held);
}

/// Under registry pressure a new scope gets an explicitly unavailable guard:
/// writes and external calls are refused, reads stay verifiable, and no unknown
/// evidence is invented to justify the refusal.
#[tokio::test]
async fn an_unavailable_guard_fails_closed_without_fabricating_evidence() {
    if !runs_in_isolated_case("an_unavailable_guard_fails_closed_without_fabricating_evidence") {
        return;
    }
    let mut held = Vec::with_capacity(MAX_FENCE_SCOPES + 2);
    let mut unavailable = None;
    for _ in 0..=MAX_FENCE_SCOPES {
        let fence = fence_for_scope(&uuid::Uuid::new_v4().to_string());
        if fence.unavailable() {
            unavailable = Some(fence.clone());
        }
        held.push(fence);
    }
    let fence = unavailable.expect("the bounded registry must hand back an unavailable guard");
    assert!(
        !fence.has_unknown(),
        "an unavailable guard must not fabricate unknown evidence"
    );
    assert!(fence.evidence().is_fail_closed());

    let policy = ToolPolicy::new(
        ToolFilter::all(),
        false,
        Arc::new(AutoConfirmHost),
        PolicyContext::new("/workspace", uuid::Uuid::new_v4().to_string()),
    )
    .with_unknown_fence(fence.clone());
    assert!(policy.admit("read", r#"{"path":"a"}"#).await.is_ok());
    let write = policy
        .admit_with_call_id("write", r#"{"path":"a","value":"x"}"#, Some("guarded"))
        .await
        .unwrap_err();
    assert!(
        write.reason().contains("unavailable"),
        "the unavailable guard must be named: {}",
        write.reason()
    );
    assert!(policy.admit("mcp__probe__write", r#"{}"#).await.is_err());
    drop(held);
}
