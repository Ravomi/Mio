//! Offline DES-15 tool-identity binding tests.
//!
//! These tests need no provider, bus or engine: they construct the engine's
//! real dispatch identity ([`ToolInvocation`]) directly on a tool call's
//! runtime scope and drive the wrappers that bind it to the executing task.
//!
//! What is asserted is that the identity a host observes comes from the call
//! that is actually running -- not from dispatch order, the tool name or an
//! argument hash -- and that it cannot leak across `await` boundaries,
//! concurrent tasks, nested scopes or cancelled futures, never becomes part of
//! the model-visible arguments/output, and stays honestly absent when no engine
//! scope was supplied.

use mio_agent::agent::ToolInvocation;
use mio_coding_agent::tool_identity::{self, ScopedTool, ToolIdentity};
use mio_coding_agent::{AgentEvent, Host, HostFuture};
use mio_core::message::ToolName;
use mio_core::tool::{ContextValue, DynamicTool, Tool, ToolContext, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    convert::Infallible,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use uuid::Uuid;

/// Build a call context carrying the engine's real dispatch identity, exactly
/// the way the bus attaches it for a real tool call.
fn context_with(tool_call_id: &str, turn: usize, effect_id: u64) -> ToolContext {
    let invocation = ToolInvocation {
        effect_id,
        tool_call_id: tool_call_id.to_owned(),
        turn,
    };
    ToolContext::new().with_scope(Arc::new(invocation))
}

/// What a real host callback observed: the identity bound to the executing
/// task at the moment the event fired, plus the event's own operation/target.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ObservedEvent {
    identity: Option<ToolIdentity>,
    phase: &'static str,
    target: String,
}

#[derive(Default)]
struct RecordingHost {
    observed: Mutex<Vec<ObservedEvent>>,
}

impl RecordingHost {
    fn observed(&self) -> Vec<ObservedEvent> {
        self.observed.lock().unwrap().clone()
    }
}

impl Host for RecordingHost {
    fn shell(&self, _: &str, _: &Path) -> HostFuture<Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }

    /// The very path `CliHost::emit_agent_event` uses: read the task identity
    /// inside the host callback, never infer it from order or names.
    fn on_event(&self, event: AgentEvent) {
        let (phase, target) = match event {
            AgentEvent::ToolStarted { target, .. } => ("started", target),
            AgentEvent::ToolFinished { target, .. } => ("finished", target),
            AgentEvent::Text(_) => return,
        };
        self.observed.lock().unwrap().push(ObservedEvent {
            identity: tool_identity::current(),
            phase,
            target,
        });
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ProbeArgs {
    tag: String,
}

/// A same-name tool whose body crosses real `await` boundaries before it
/// finishes, so a task-local scope that did not survive them would be visible
/// as a missing id at the host callback.
#[derive(Clone)]
struct Probe {
    host: Arc<RecordingHost>,
    gate: Arc<tokio::sync::Notify>,
}

impl Tool for Probe {
    const NAME: &'static str = "probe";
    type Args = ProbeArgs;
    type Output = Value;
    type Error = Infallible;

    fn description(&self) -> String {
        "probe".into()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"tag": {"type": "string"}},
            "required": ["tag"],
        })
    }

    async fn call(&self, _context: &mut ToolContext, args: ProbeArgs) -> Result<Value, Infallible> {
        let target = args.tag.clone();
        self.host.on_event(AgentEvent::ToolStarted {
            operation: "probe".into(),
            target: target.clone(),
        });
        if target == "slow" {
            // Wait for the fast call to finish, so the two calls complete in
            // the reverse of their dispatch order.
            tokio::task::yield_now().await;
            self.gate.notified().await;
        } else {
            tokio::task::yield_now().await;
        }
        self.host.on_event(AgentEvent::ToolFinished {
            operation: "probe".into(),
            target,
            result: Ok(String::new()),
        });
        if args.tag == "fast" {
            self.gate.notify_one();
        }
        Ok(json!({"tag": args.tag}))
    }
}

fn probe(host: &Arc<RecordingHost>, gate: Arc<tokio::sync::Notify>) -> Probe {
    Probe {
        host: Arc::clone(host),
        gate,
    }
}

/// Two same-name calls dispatched together but completing in reverse order
/// keep their own real identity and their own arguments at the host boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_same_name_calls_keep_their_own_identity_and_arguments() {
    let run_id = Uuid::new_v4();
    let host = Arc::new(RecordingHost::default());
    let gate = Arc::new(tokio::sync::Notify::new());
    let tool = Arc::new(ScopedTool::new(probe(&host, Arc::clone(&gate)), run_id));

    let slow = tokio::spawn({
        let tool = Arc::clone(&tool);
        async move {
            let mut context = context_with("call_slow", 3, 30);
            tool.call(&mut context, ProbeArgs { tag: "slow".into() })
                .await
                .unwrap()
        }
    });
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    let fast = tokio::spawn({
        let tool = Arc::clone(&tool);
        async move {
            let mut context = context_with("call_fast", 3, 31);
            tool.call(&mut context, ProbeArgs { tag: "fast".into() })
                .await
                .unwrap()
        }
    });

    slow.await.unwrap();
    fast.await.unwrap();

    let observed = host.observed();
    assert_eq!(observed.len(), 4, "{observed:?}");
    // Every event's identity matches its own target, item by item, no matter
    // what order the events arrived or completed in.
    for event in &observed {
        let identity = event
            .identity
            .as_ref()
            .unwrap_or_else(|| panic!("engine scope must be bound: {event:?}"));
        assert_eq!(identity.tool_call_id(), format!("call_{}", event.target));
        assert_eq!(identity.turn(), 3);
        assert_eq!(identity.run_id(), run_id);
        match event.target.as_str() {
            "slow" => assert_eq!(identity.effect_id(), 30),
            "fast" => assert_eq!(identity.effect_id(), 31),
            other => panic!("unexpected target {other}"),
        }
    }

    // Completion is deliberately reversed: slow was dispatched first but
    // finishes after fast, so an order-based pairing would cross the two ids.
    let finished: Vec<&str> = observed
        .iter()
        .filter(|event| event.phase == "finished")
        .map(|event| event.target.as_str())
        .collect();
    assert_eq!(finished, ["fast", "slow"], "{observed:?}");
}

/// A call whose context carries no engine scope stays honestly unknown: the
/// wrapper must not fabricate run/turn/call/effect ids.
#[tokio::test]
async fn a_call_without_engine_scope_reports_no_identity() {
    let run_id = Uuid::new_v4();
    let host = Arc::new(RecordingHost::default());
    let tool = ScopedTool::new(probe(&host, Arc::new(tokio::sync::Notify::new())), run_id);

    let mut context = ToolContext::new();
    assert!(ToolIdentity::from_context(run_id, &context).is_none());
    let output = tool
        .call(&mut context, ProbeArgs { tag: "bare".into() })
        .await
        .unwrap();
    assert_eq!(output.as_json(), Some(&json!({"tag": "bare"})));

    let observed = host.observed();
    assert_eq!(observed.len(), 2, "{observed:?}");
    assert!(
        observed.iter().all(|event| event.identity.is_none()),
        "no engine scope must stay unknown, never fabricated: {observed:?}"
    );
    assert_eq!(observed[0].target, "bare");
    assert!(tool_identity::current().is_none());
}

/// Nested scopes restore the outer identity; a dropped nested future cannot
/// leave the inner identity behind.
#[tokio::test]
async fn nested_scopes_restore_the_outer_identity_and_drop_cleanly() {
    let outer = ToolIdentity::new(Uuid::new_v4(), "outer", 1, 1);
    let inner = ToolIdentity::new(Uuid::new_v4(), "inner", 2, 2);
    let outer_expected = outer.clone();

    tool_identity::scope(Some(outer), async move {
        assert_eq!(tool_identity::current(), Some(outer_expected.clone()));

        let mut nested = Box::pin(tool_identity::scope(Some(inner), async {
            std::future::pending::<()>().await;
        }));
        assert!(futures_util::poll!(nested.as_mut()).is_pending());
        // The pending nested scope has yielded its poll; the outer identity is
        // restored, and is still there when the nested future is dropped.
        assert_eq!(tool_identity::current(), Some(outer_expected.clone()));
        drop(nested);
        assert_eq!(tool_identity::current(), Some(outer_expected));
    })
    .await;

    // Nothing outlives the scope.
    assert!(tool_identity::current().is_none());
}

/// The identity is not part of the tool's schema or its model-visible output.
#[tokio::test]
async fn the_identity_never_becomes_arguments_or_output() {
    let run_id = Uuid::new_v4();
    let host = Arc::new(RecordingHost::default());
    let tool = ScopedTool::new(probe(&host, Arc::new(tokio::sync::Notify::new())), run_id);

    assert_eq!(<ScopedTool<Probe> as Tool>::NAME, "probe");
    assert_eq!(
        tool.parameters(),
        json!({
            "type": "object",
            "properties": {"tag": {"type": "string"}},
            "required": ["tag"],
        })
    );

    let mut context = context_with("call-secret", 4, 44);
    let output = tool
        .call(&mut context, ProbeArgs { tag: "t".into() })
        .await
        .unwrap();
    assert_eq!(output.as_json(), Some(&json!({"tag": "t"})));
    let rendered = output.render();
    assert!(!rendered.contains("call-secret"), "{rendered}");
    assert!(!rendered.contains(&run_id.to_string()), "{rendered}");
}

/// A published result-metadata slot used to check context preservation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Mark(u32);

impl ContextValue for Mark {
    const KEY: &'static str = "mark";
}

/// The dynamic wrapper preserves the definition, the liveness probe and the
/// result/context outcome of the tool it wraps.
#[tokio::test]
async fn the_dynamic_wrapper_preserves_definition_liveness_and_results() {
    let run_id = Uuid::new_v4();
    let live = Arc::new(AtomicBool::new(true));
    let probe = Arc::clone(&live);
    let original = DynamicTool::new_with_context(
        ToolName::new("dynamic-probe").unwrap(),
        "Dynamic probe",
        json!({
            "type": "object",
            "properties": {"tag": {"type": "string"}},
        }),
        move |context: &mut ToolContext, arguments: Value| {
            let tag = arguments
                .get("tag")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let seen = tool_identity::current().map(|identity| identity.tool_call_id().to_owned());
            let _ = context.insert_result(Mark(7));
            Box::pin(async move {
                tokio::task::yield_now().await;
                Ok(ToolOutput::json(json!({"tag": tag, "seen": seen})))
            })
        },
    )
    .with_liveness(move || probe.load(Ordering::SeqCst));

    let wrapped = tool_identity::scoped_dynamic_tool(original.clone(), run_id);
    assert_eq!(wrapped.name(), original.name());
    assert_eq!(wrapped.definition(), original.definition());
    assert!(wrapped.is_live());
    live.store(false, Ordering::SeqCst);
    assert!(
        !wrapped.is_live(),
        "the wrapper must delegate to the wrapped tool's own liveness probe"
    );
    live.store(true, Ordering::SeqCst);

    // With the engine's real scope, the wrapped handler sees the real call id,
    // and its published result metadata survives the wrapper.
    let mut context = context_with("call-dyn", 6, 66);
    let output = wrapped
        .execute_with(&mut context, json!({"tag": "t"}))
        .await
        .unwrap();
    assert_eq!(
        output.as_json(),
        Some(&json!({"tag": "t", "seen": "call-dyn"}))
    );
    assert_eq!(context.result::<Mark>().unwrap(), Some(Mark(7)));

    // Without a scope, the wrapped handler still sees no identity (null) and
    // the output stays honest.
    let mut bare = ToolContext::new();
    let output = wrapped
        .execute_with(&mut bare, json!({"tag": "t"}))
        .await
        .unwrap();
    assert_eq!(output.as_json(), Some(&json!({"tag": "t", "seen": null})));
    assert_eq!(bare.result::<Mark>().unwrap(), Some(Mark(7)));
}
