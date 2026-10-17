//! The state contract must reject terminal rewrites, independently of storage.
use mio_coding_agent::{
    AgentEvent, Host, HostFuture,
    tool_lifecycle::{ToolCallPhase, ToolCallRecord, ToolCallTracker},
};
use std::{
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

#[test]
fn terminal_tool_states_never_resume_or_change_the_observed_outcome() {
    use ToolCallPhase::*;
    let terminal = [Denied, Executed, Failed, Unknown, NotDispatched];
    for phase in terminal {
        for pending in [Normalized, Admitted, Dispatched] {
            assert!(
                !ToolCallPhase::transition_allowed(phase, pending),
                "terminal {phase:?} cannot become {pending:?}"
            );
        }
        for other in terminal {
            if other != phase {
                assert!(
                    !ToolCallPhase::transition_allowed(phase, other),
                    "observed {phase:?} cannot be rewritten as {other:?}"
                );
            }
        }
        assert!(ToolCallPhase::transition_allowed(phase, phase));
    }
}

#[test]
fn dispatch_cannot_be_reclassified_as_pre_dispatch_refusal() {
    use ToolCallPhase::*;
    for earlier in [Normalized, Admitted, Denied, NotDispatched] {
        assert!(!ToolCallPhase::transition_allowed(Dispatched, earlier));
    }
    assert!(!ToolCallPhase::transition_allowed(Normalized, Executed));
    assert!(!ToolCallPhase::transition_allowed(Admitted, Unknown));
    for observed in [Executed, Failed, Unknown] {
        assert!(ToolCallPhase::transition_allowed(Dispatched, observed));
    }
}

#[derive(Default)]
struct SnapshotHost {
    tracker: Mutex<Weak<ToolCallTracker>>,
    observations: Mutex<Vec<ToolCallRecord>>,
}

impl Host for SnapshotHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { panic!("state callbacks must not execute shell") })
    }
    fn on_event(&self, _: AgentEvent) {}
    fn on_tool_call_lifecycle(&self, record: &ToolCallRecord) -> Result<(), String> {
        let tracker = self.tracker.lock().unwrap().upgrade().unwrap();
        let _last_committed_snapshot = tracker.calls();
        self.observations.lock().unwrap().push(record.clone());
        Ok(())
    }
}

#[test]
fn host_can_read_the_tracker_snapshot_from_a_persistence_callback() {
    const CASE: &str = "MIO_LIFECYCLE_SNAPSHOT_CASE";
    const NAME: &str = "host_can_read_the_tracker_snapshot_from_a_persistence_callback";
    if std::env::var(CASE).as_deref() == Ok(NAME) {
        let host = Arc::new(SnapshotHost::default());
        let tracker = ToolCallTracker::new(uuid::Uuid::new_v4().to_string(), host.clone());
        *host.tracker.lock().unwrap() = Arc::downgrade(&tracker);
        let index = tracker
            .begin("read", "read", "a.txt", Some("call-one"), Some(1), Some(2))
            .unwrap();
        tracker.admit(index, Some(1), Some(2)).unwrap();
        tracker.mark_dispatched(2, Some("call-one")).unwrap();
        tracker
            .observe_outcome(2, Some("call-one"), ToolCallPhase::Executed, Some("done"))
            .unwrap();
        assert_eq!(tracker.calls()[0].phase, ToolCallPhase::Executed);
        assert_eq!(host.observations.lock().unwrap().len(), 4);
        return;
    }

    // A blocking callback deadlock cannot be cancelled by a Tokio timeout.
    // An isolated child lets this regression fail promptly and be reaped.
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
        .env_clear()
        .env(CASE, NAME)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait().unwrap().is_some() {
            let output = child.wait_with_output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(stdout.contains("1 passed; 0 failed"), "{stdout}");
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the host's snapshot query deadlocked inside its persistence callback");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
