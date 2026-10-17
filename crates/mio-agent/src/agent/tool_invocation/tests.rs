//! Tests for the per-call [`ToolInvocation`] identity scope.
//!
//! These run offline against a scripted mock model: no provider, no network.
//! The concurrency cases use the engine's real `tool_concurrency` path and a
//! gate that makes the first call finish *after* its same-named sibling, so
//! identity is proven without relying on completion order, tool name, or an
//! argument hash.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU32, Ordering::SeqCst},
};

use mio_core::message::{AssistantContent, ToolCall as MessageToolCall, ToolFunction, ToolName};
use mio_core::tool::ContextValue;
use serde_json::json;
use tokio::sync::Notify;

use crate::agent::{
    AgentBuilder, AgentHook, DispatchAction, DispatchEvent, HookContext, OutcomeAction,
    OutcomeEvent, ToolInvocation,
};
use crate::test_utils::{MockCompletionModel, MockTurn};
use crate::tool::{Tool, ToolContext, ToolExecutionError};

/// An unrelated scope type: a replacement of another type must not disturb it.
#[derive(Debug)]
struct ParentMarker(u32);

/// A host-only inbound value that must never surface in result metadata.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct PrivateInput(String);

impl ContextValue for PrivateInput {
    const KEY: &'static str = "test.private_input";
}

/// The result metadata the probe tool publishes.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct PublishedNote(String);

impl ContextValue for PublishedNote {
    const KEY: &'static str = "test.published_note";
}

/// One observation of a tool call, recorded when its body completes.
#[derive(Debug, Clone)]
struct ObservedCall {
    /// The identity scope the call saw, if the engine attached one.
    invocation: Option<ToolInvocation>,
    /// The value of the unrelated marker scope, still attached.
    marker_value: Option<u32>,
    /// Live strong count of the marker while the body ran.
    marker_count: usize,
    /// The tool's context serialized while its scopes were live. Live scopes
    /// must contribute nothing.
    live_context: serde_json::Value,
}

/// What the dispatch hook saw for one tool call.
#[derive(Debug, Clone)]
struct DispatchObservation {
    effect_id: u64,
    call_id: String,
    invocation: Option<ToolInvocation>,
    marker_value: Option<u32>,
}

/// What the outcome hook saw for one tool call.
#[derive(Debug, Clone)]
struct OutcomeObservation {
    effect_id: u64,
    call_id: String,
    /// The live identity still on the published context, which must be `None`.
    identity_scope: Option<ToolInvocation>,
    /// The durable result metadata, serialized.
    result: serde_json::Value,
}

/// Records the identity both boundaries see, without steering the run.
#[derive(Debug, Clone, Default)]
struct IdentityProbeHook {
    dispatches: Arc<Mutex<Vec<DispatchObservation>>>,
    outcomes: Arc<Mutex<Vec<OutcomeObservation>>>,
}

impl AgentHook for IdentityProbeHook {
    async fn on_dispatch(&self, _ctx: &HookContext, event: DispatchEvent<'_>) -> DispatchAction {
        if let (Some(call_id), Some(context)) = (event.call_id, event.context) {
            self.dispatches
                .lock()
                .expect("dispatch observations")
                .push(DispatchObservation {
                    effect_id: event.id.as_u64(),
                    call_id: call_id.wire().into_owned(),
                    invocation: context
                        .scope::<ToolInvocation>()
                        .map(|scope| (*scope).clone()),
                    marker_value: context.scope::<ParentMarker>().map(|marker| marker.0),
                });
        }
        DispatchAction::proceed()
    }

    async fn on_outcome(&self, _ctx: &HookContext, event: OutcomeEvent<'_>) -> OutcomeAction {
        if let Some(call_id) = event.call_id {
            let identity_scope = event
                .context
                .and_then(|context| context.scope::<ToolInvocation>())
                .map(|scope| (*scope).clone());
            let result = event.context.map_or(serde_json::Value::Null, |context| {
                serde_json::to_value(context.result_context()).unwrap_or(serde_json::Value::Null)
            });
            self.outcomes
                .lock()
                .expect("outcome observations")
                .push(OutcomeObservation {
                    effect_id: event.id.as_u64(),
                    call_id: call_id.wire().into_owned(),
                    identity_scope,
                    result,
                });
        }
        OutcomeAction::proceed()
    }
}

/// A tool named `add` that records the identity scope of every call, publishes
/// result metadata, and can gate its first call so a later sibling completes
/// first.
#[derive(Clone)]
struct IdentityProbeTool {
    records: Arc<Mutex<Vec<ObservedCall>>>,
    marker: Arc<ParentMarker>,
    gate: Option<Arc<Notify>>,
    order: Arc<AtomicU32>,
}

impl Tool for IdentityProbeTool {
    const NAME: &'static str = "add";
    type Error = ToolExecutionError;
    type Args = serde_json::Value;
    type Output = String;

    fn description(&self) -> String {
        "Records the identity scope of its call".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": { "i": { "type": "integer" } },
        })
    }

    async fn call(
        &self,
        context: &mut ToolContext,
        _args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let invocation = context
            .scope::<ToolInvocation>()
            .map(|scope| (*scope).clone());
        let marker_value = context.scope::<ParentMarker>().map(|marker| marker.0);
        let marker_count = Arc::strong_count(&self.marker);
        context.insert_result(PublishedNote("note".to_string()))?;
        let live_context = serde_json::to_value(&*context)
            .map_err(|error| ToolExecutionError::other(error.to_string()))?;

        let nth = self.order.fetch_add(1, SeqCst);
        if let Some(gate) = &self.gate {
            if nth == 0 {
                // The first call cannot finish until a later sibling ran.
                gate.notified().await;
            } else {
                gate.notify_one();
            }
        }

        self.records
            .lock()
            .expect("call records")
            .push(ObservedCall {
                invocation,
                marker_value,
                marker_count,
                live_context,
            });
        Ok(format!("ran {nth}"))
    }
}

/// A model turn emitting one or more `add` calls, in emission order.
fn call_content(call_id: &str, args: serde_json::Value) -> AssistantContent {
    AssistantContent::ToolCall(MessageToolCall::from_wire(
        call_id,
        ToolFunction::new(ToolName::new("add".to_string()).expect("tool name"), args),
    ))
}

/// A `ToolInvocation` built from parts, for cases that need distinct values.
fn invocation(call_id: &str, effect_id: u64) -> ToolInvocation {
    ToolInvocation {
        effect_id,
        tool_call_id: call_id.to_string(),
        turn: 42,
    }
}

/// A `ToolInvocation` no engine minted: the identity an inherited parent or a
/// host could have attached before this call.
fn forged_invocation() -> ToolInvocation {
    invocation("forged", 999_999)
}

/// The base context every engine test supplies: a private inbound value, a
/// forged `ToolInvocation`, and an unrelated marker scope.
fn base_context(marker: &Arc<ParentMarker>, forged: &ToolInvocation) -> ToolContext {
    let mut context = ToolContext::new();
    context
        .insert(PrivateInput("host-only-credential".to_string()))
        .expect("private input inserts");
    context
        .with_scope(Arc::new(forged.clone()))
        .with_scope(marker.clone())
}

/// The identity the tool recorded for `call_id`.
fn recorded_invocation<'a>(observed: &'a [ObservedCall], call_id: &str) -> &'a ToolInvocation {
    observed
        .iter()
        .find_map(|call| {
            call.invocation
                .as_ref()
                .filter(|invocation| invocation.tool_call_id == call_id)
        })
        .expect("the call was observed")
}

/// The serialized tool context, with its live scopes excluded.
fn expected_live_context() -> serde_json::Value {
    json!({
        "inbound": { "test.private_input": "host-only-credential" },
        "result": { "test.published_note": "note" },
    })
}

/// Replacing a scope drops every value of that type — not just the one `scope`
/// would have shadowed — and leaves every other scope, and its position,
/// untouched.
#[test]
fn replacing_a_scope_drops_only_that_type() {
    let old_first = Arc::new(invocation("old-first", 1));
    let old_second = Arc::new(invocation("old-second", 2));
    let fresh = invocation("fresh", 3);
    let marker = Arc::new(ParentMarker(7));

    let context = ToolContext::new()
        .with_scope(old_first.clone())
        .with_scope(marker.clone())
        .with_scope(old_second.clone())
        .with_replaced_scope(Arc::new(fresh.clone()));

    // The fresh value is what the first-of-type accessor answers with.
    assert_eq!(context.scope::<ToolInvocation>().as_deref(), Some(&fresh));
    // Every superseded value was actually dropped, not hidden behind the new one.
    assert_eq!(Arc::strong_count(&old_first), 1);
    assert_eq!(Arc::strong_count(&old_second), 1);
    // The unrelated scope keeps its value and its lifetime.
    assert_eq!(
        context.scope::<ParentMarker>().map(|marker| marker.0),
        Some(7)
    );
    assert!(Arc::strong_count(&marker) >= 2);
    // Live scopes are never serialized.
    assert_eq!(
        serde_json::to_value(&context).expect("serialize"),
        json!({})
    );
}

/// `with_scope` still appends and `scope` still answers with the first of its
/// type: the replacement API does not change the existing semantics.
#[test]
fn appended_scopes_keep_first_of_type() {
    let context = ToolContext::new()
        .with_scope(Arc::new(ParentMarker(1)))
        .with_scope(Arc::new(ParentMarker(2)));
    assert_eq!(
        context.scope::<ParentMarker>().map(|marker| marker.0),
        Some(1)
    );
}

/// Two same-named calls in one batch, completing out of order, each see their
/// own provider call id, turn, and bus-minted effect id. A forged scope on the
/// base context is replaced rather than reused, the unrelated scope stays alive
/// through both calls, and neither the serialized context nor the published
/// result metadata carries identity or the private input.
#[tokio::test]
async fn concurrent_same_name_calls_see_their_own_identity() {
    let marker = Arc::new(ParentMarker(7));
    let forged = forged_invocation();
    let base = base_context(&marker, &forged);
    let records = Arc::new(Mutex::new(Vec::new()));
    let hook = IdentityProbeHook::default();
    let tool = IdentityProbeTool {
        records: records.clone(),
        marker: marker.clone(),
        gate: Some(Arc::new(Notify::new())),
        order: Arc::new(AtomicU32::new(0)),
    };
    let agent = AgentBuilder::new(MockCompletionModel::from_turns([
        MockTurn::from_contents([
            call_content("tc1", json!({ "i": 1 })),
            call_content("tc2", json!({ "i": 2 })),
        ]),
        MockTurn::from_contents([call_content("tc3", json!({ "i": 3 }))]),
        MockTurn::text("done"),
    ]))
    .tool(tool)
    .add_hook(hook.clone())
    .build();

    let before = Arc::strong_count(&marker);
    agent
        .prompt("go")
        .tool_context(base)
        .tool_concurrency(2)
        .max_turns(3)
        .run()
        .await
        .expect("run succeeds");

    let observed = records.lock().expect("call records").clone();
    assert_eq!(observed.len(), 3, "all three calls ran");

    // Genuinely out of order: the gated first call finished after its sibling,
    // so the recorded order is completion order, not call order.
    let completion_order: Vec<&str> = observed
        .iter()
        .map(|call| {
            call.invocation
                .as_ref()
                .expect("every dispatch carries identity")
                .tool_call_id
                .as_str()
        })
        .collect();
    assert_eq!(completion_order, ["tc2", "tc1", "tc3"]);

    let tc1 = recorded_invocation(&observed, "tc1");
    let tc2 = recorded_invocation(&observed, "tc2");
    let tc3 = recorded_invocation(&observed, "tc3");
    // Distinct bus-minted ids, and the turn of the model call that emitted each.
    assert_ne!(tc1.effect_id, tc2.effect_id);
    assert_ne!(tc1.effect_id, tc3.effect_id);
    assert_ne!(tc2.effect_id, tc3.effect_id);
    assert_eq!(
        (tc1.turn, tc2.turn),
        (1, 1),
        "both calls are the first turn"
    );
    assert_eq!(tc3.turn, 2, "the third call is the second turn");

    for call in &observed {
        let invocation = call
            .invocation
            .as_ref()
            .expect("every dispatch carries identity");
        // The provider's wire id verbatim, never re-created or trimmed.
        assert!(
            matches!(invocation.tool_call_id.as_str(), "tc1" | "tc2" | "tc3"),
            "unexpected call id {:?}",
            invocation.tool_call_id
        );
        // The forged identity on the base context is replaced, never reused.
        assert_ne!(invocation.effect_id, forged.effect_id);
        assert_ne!(invocation.tool_call_id, forged.tool_call_id);
        assert_ne!(invocation.turn, forged.turn);
        // The unrelated scope stayed attached and alive through the call.
        assert_eq!(call.marker_value, Some(7));
        assert!(
            call.marker_count > before,
            "the replaced scope must not drop other scopes ({})",
            call.marker_count
        );
        // Neither scope type contributed anything serialized.
        assert_eq!(call.live_context, expected_live_context());
    }

    // The dispatch hook saw the same identity as the tool, carrying the id the
    // bus minted for that dispatch.
    let dispatches = hook
        .dispatches
        .lock()
        .expect("dispatch observations")
        .clone();
    assert_eq!(dispatches.len(), 3);
    for dispatch in &dispatches {
        let invocation = dispatch
            .invocation
            .as_ref()
            .expect("the dispatch hook sees the scope");
        assert_eq!(invocation.tool_call_id, dispatch.call_id);
        assert_eq!(
            invocation.effect_id, dispatch.effect_id,
            "the scope carries the dispatch's own minted id"
        );
        assert_eq!(dispatch.marker_value, Some(7));
        assert_eq!(
            invocation,
            recorded_invocation(&observed, &dispatch.call_id)
        );
    }

    // The published context keeps clearing scopes, and durable result metadata
    // holds only what the tool published: no identity, no private input.
    let outcomes = hook.outcomes.lock().expect("outcome observations").clone();
    assert_eq!(outcomes.len(), 3);
    let published = json!({ "test.published_note": "note" });
    for outcome in &outcomes {
        assert!(
            outcome.identity_scope.is_none(),
            "published result metadata must not carry the identity scope"
        );
        assert_eq!(outcome.result, published);
        let serialized = outcome.result.to_string();
        assert!(!serialized.contains("host-only-credential"));
        assert!(!serialized.contains("forged"));
        assert!(!serialized.contains(&outcome.call_id));
        assert_eq!(
            outcome.effect_id,
            recorded_invocation(&observed, &outcome.call_id).effect_id
        );
    }

    // Scopes are released with the run and its registrations: the handles the
    // run held are gone and nothing accumulated, while this test's own handle
    // (and any registration the tool itself still owns) is all that remains.
    drop(agent);
    assert!(
        Arc::strong_count(&marker) < before,
        "scope handles are released at their original release point ({} vs {})",
        Arc::strong_count(&marker),
        before
    );
}

/// The default `tool_concurrency` is 1: the same two calls run in call order
/// and still each see their own identity.
#[tokio::test]
async fn default_concurrency_runs_in_call_order_with_identity() {
    let marker = Arc::new(ParentMarker(7));
    let forged = forged_invocation();
    let base = base_context(&marker, &forged);
    let records = Arc::new(Mutex::new(Vec::new()));
    let tool = IdentityProbeTool {
        records: records.clone(),
        marker: marker.clone(),
        // No gate: at the default concurrency this must not wait for a sibling.
        gate: None,
        order: Arc::new(AtomicU32::new(0)),
    };
    let agent = AgentBuilder::new(MockCompletionModel::from_turns([
        MockTurn::from_contents([
            call_content("tc1", json!({ "i": 1 })),
            call_content("tc2", json!({ "i": 2 })),
        ]),
        MockTurn::text("done"),
    ]))
    .tool(tool)
    .build();

    agent
        .prompt("go")
        .tool_context(base)
        .max_turns(2)
        .run()
        .await
        .expect("run succeeds");

    let observed = records.lock().expect("call records").clone();
    let call_order: Vec<&str> = observed
        .iter()
        .map(|call| {
            call.invocation
                .as_ref()
                .expect("every dispatch carries identity")
                .tool_call_id
                .as_str()
        })
        .collect();
    assert_eq!(call_order, ["tc1", "tc2"]);

    let tc1 = recorded_invocation(&observed, "tc1");
    let tc2 = recorded_invocation(&observed, "tc2");
    assert_ne!(tc1.effect_id, tc2.effect_id);
    assert_eq!((tc1.turn, tc2.turn), (1, 1));
    assert_ne!(tc1.effect_id, forged.effect_id);
    for call in &observed {
        assert_eq!(call.marker_value, Some(7));
        assert_eq!(call.live_context, expected_live_context());
    }

    drop(agent);
}

/// A direct `Tool::call` — no engine, no dispatch scope — is unchanged: it runs,
/// publishes its metadata, and sees neither an identity nor an inherited scope.
#[tokio::test]
async fn direct_call_without_a_dispatch_scope_is_unchanged() {
    let marker = Arc::new(ParentMarker(7));
    let records = Arc::new(Mutex::new(Vec::new()));
    let tool = IdentityProbeTool {
        records: records.clone(),
        marker,
        gate: None,
        order: Arc::new(AtomicU32::new(0)),
    };
    let mut context = ToolContext::new();
    context
        .insert(PrivateInput("host-only-credential".to_string()))
        .expect("private input inserts");

    let output = tool
        .call(&mut context, json!({}))
        .await
        .expect("direct call runs");

    assert_eq!(output, "ran 0");
    // The caller's own slots survive a direct call unchanged.
    assert_eq!(
        context
            .require::<PrivateInput>()
            .expect("private input reads")
            .0,
        "host-only-credential"
    );
    assert_eq!(
        context
            .result::<PublishedNote>()
            .expect("result reads")
            .map(|note| note.0),
        Some("note".to_string())
    );

    let observed = records.lock().expect("call records").clone();
    assert_eq!(observed.len(), 1);
    assert!(observed[0].invocation.is_none());
    assert!(observed[0].marker_value.is_none());
    assert_eq!(observed[0].live_context, expected_live_context());
}
