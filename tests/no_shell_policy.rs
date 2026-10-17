//! Acceptance for the shared `--no-shell` / `ToolFilter::without_shell` policy.
//!
//! `--no-shell` is not only a host string check. `src/bin/mio.rs` folds it into
//! the run's single [`ToolFilter`] before the CLI ever starts, so the same
//! admission gate the Rust SDK uses refuses `shell`, `bash` and the aggregate
//! `workspace { operation: "shell" }` call. This file pins that gate directly:
//!
//! * the alias denial happens on [`ToolPolicy::admit`], before any approval or
//!   dispatch, and never as a host `shell` error string;
//! * file tooling that was already allowed (`read` / `write` / `workspace`
//!   read+write) stays allowed;
//! * an explicit allowlist that names `bash`/`shell` does not mask the refusal;
//! * `without_shell` only narrows: an existing exclusion or delegation ceiling
//!   is never relaxed, and repeating it is idempotent.
//!
//! No provider is contacted and no network is used: every assertion drives the
//! in-process policy gate. Side-effect sentinels stay inside a temp dir.

use mio_coding_agent::policy::{ApprovalBinding, PolicyContext, ToolPolicy};
use mio_coding_agent::{AgentEvent, Host, HostFuture, ToolFilter};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// The allowlist that makes the refusal meaningful: `bash`/`shell`/`workspace`
/// are explicitly named, so only the `without_shell` exclusion can deny them.
const SHELL_PERMITTING_ALLOWLIST: &str = "workspace,bash,shell,read,write";

/// A command that would create a marker file if it ever executed.
const SENTINEL_COMMAND: &str = "printf shared-no-shell-sentinel > shared-no-shell-sentinel.txt";

fn permissive_without_shell() -> ToolFilter {
    ToolFilter::from_cli(Some(SHELL_PERMITTING_ALLOWLIST), None, false)
        .expect("builtin allowlist must be accepted")
        .without_shell()
}

/// Records real approval requests; never runs a shell.
#[derive(Default)]
struct RecordingHost {
    approvals: Mutex<Vec<String>>,
}

impl RecordingHost {
    fn approvals(&self) -> Vec<String> {
        self.approvals.lock().unwrap().clone()
    }
}

impl Host for RecordingHost {
    fn shell(&self, _command: &str, _cwd: &Path) -> HostFuture<Result<String, String>> {
        panic!("the shared policy gate must refuse shell before the host is asked to run it")
    }

    fn on_event(&self, _event: AgentEvent) {}

    fn approve_tool_binding(
        &self,
        operation: &str,
        target: &str,
        _binding: &ApprovalBinding,
    ) -> HostFuture<Result<(), String>> {
        self.approvals
            .lock()
            .unwrap()
            .push(format!("{operation}:{target}"));
        Box::pin(async { Ok(()) })
    }
}

// ---------------------------------------------------------------------------
// Pure filter behavior (no run, no host)
// ---------------------------------------------------------------------------

#[test]
fn without_shell_denies_both_aliases_but_keeps_file_tools() {
    let filter = permissive_without_shell();
    for alias in ["shell", "bash"] {
        assert!(
            !filter.allows(alias),
            "`{alias}` must be denied once without_shell narrows an allowlist that names it"
        );
    }
    assert!(
        !filter.allows_workspace_operation("shell"),
        "the aggregate workspace shell operation must be denied"
    );
    for allowed in ["read", "write", "workspace"] {
        assert!(filter.allows(allowed), "`{allowed}` must stay allowed");
    }
    assert!(filter.allows_workspace_operation("read"));
    assert!(filter.allows_workspace_operation("write"));
    assert!(
        !filter.allows_aliased(&["plugin__probe__shell", "shell"]),
        "deny wins across every alias of a shell tool"
    );
}

#[test]
fn without_shell_is_idempotent() {
    let once = permissive_without_shell();
    let twice = once.clone().without_shell().without_shell();
    assert!(
        once.same_base(&twice),
        "repeating without_shell must not add rules a second time"
    );
    assert_eq!(once.allows("shell"), twice.allows("shell"));
    assert_eq!(once.allows("bash"), twice.allows("bash"));
    assert!(twice.allows("read") && twice.allows("write"));
}

#[test]
fn without_shell_never_relaxes_an_existing_exclusion() {
    let existing = ToolFilter::from_cli(None, Some("write"), false)
        .expect("exclusion filter")
        .without_shell();
    assert!(!existing.allows("write"), "an existing write denial stays");
    assert!(!existing.allows("bash"));
    assert!(!existing.allows("shell"));
    assert!(existing.allows("read"));
}

#[test]
fn without_shell_never_relaxes_a_delegation_ceiling() {
    // Outer ceiling denies write and shell; the inner filter allows everything.
    let ceiling = ToolFilter::all()
        .without_shell()
        .narrowed_by(&ToolFilter::from_cli(None, Some("write"), false).unwrap());
    let inner = ToolFilter::all().narrowed_by(&ceiling);
    // Narrowing again with the shared shell rule must not remove the ceiling.
    let narrowed = inner.without_shell();
    assert!(
        !narrowed.allows("shell"),
        "ceiling shell denial must survive"
    );
    assert!(!narrowed.allows("bash"), "ceiling bash denial must survive");
    assert!(
        !narrowed.allows("write"),
        "ceiling write denial must survive"
    );
    assert!(narrowed.allows("read"));
}

// ---------------------------------------------------------------------------
// The unified admission gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn policy_gate_denies_shell_aliases_before_approval_or_host_shell() {
    let host = Arc::new(RecordingHost::default());
    // A temp workspace root is only hashed into approval bindings; no file is
    // ever created, and the sentinel command never runs.
    let workspace = tempfile::tempdir().unwrap();
    let policy = ToolPolicy::new(
        permissive_without_shell(),
        false,
        host.clone(),
        PolicyContext::new(
            workspace.path().to_str().expect("temp path is UTF-8"),
            "run-without-shell",
        ),
    );

    let malicious = [
        ("shell", format!(r#"{{"command":"{SENTINEL_COMMAND}"}}"#)),
        ("bash", format!(r#"{{"command":"{SENTINEL_COMMAND}"}}"#)),
        (
            "workspace",
            format!(r#"{{"operation":"shell","command":"{SENTINEL_COMMAND}"}}"#),
        ),
    ];
    for (name, args) in &malicious {
        let denial = policy
            .admit(name, args)
            .await
            .expect_err("the shared policy gate must deny every shell alias");
        let reason = denial.to_string();
        assert!(
            reason.contains("tool filter does not allow"),
            "denial must come from the unified filter gate, got: {reason}"
        );
        assert!(
            !reason.contains("Shell disabled by --no-shell"),
            "the refusal must not be a host shell string error: {reason}"
        );
    }
    assert_eq!(
        policy.approval_count(),
        0,
        "a denied call must not request an approval"
    );
    assert_eq!(
        host.approvals(),
        Vec::<String>::new(),
        "the host must never see an approval for a denied shell call"
    );
    assert_eq!(policy.denial_count(), malicious.len());

    // File tooling that the same allowlist grants is still admitted.
    policy
        .admit("read", r#"{"path":"seed.txt"}"#)
        .await
        .expect("read stays allowed");
    policy
        .admit("workspace", r#"{"operation":"read","path":"seed.txt"}"#)
        .await
        .expect("aggregate read stays allowed");
    policy
        .admit("write", r#"{"path":"result.txt","value":"x"}"#)
        .await
        .expect("write stays allowed");
    policy
        .admit(
            "workspace",
            r#"{"operation":"write","path":"result.txt","value":"x"}"#,
        )
        .await
        .expect("aggregate write stays allowed");
    assert_eq!(
        policy.approval_count(),
        2,
        "only the two real write side effects ask for approval"
    );
    assert!(
        host.approvals()
            .iter()
            .all(|approval| !approval.contains("shell") && !approval.contains("bash")),
        "no shell alias may reach approval: {:?}",
        host.approvals()
    );
}
