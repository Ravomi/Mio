//! Per-call identity of an inbound tool call, attached as a live scope.

/// The identity of one tool call the engine is dispatching, attached to that
/// call's [`ToolContext`](mio_core::tool::ToolContext) as a live scope.
///
/// The engine mints the id on the bus, takes the model's wire call id verbatim,
/// and records the turn, so two concurrent calls to the same tool are
/// distinguishable by identity alone — never by completion order, tool name, or
/// argument hash. A host reads it while the call is in flight with
/// `context.scope::<ToolInvocation>()` (for example to bind a task-local
/// identity). Seeing the scope reports that a call was dispatched; it is not
/// an authorization and does not mean the tool body ran.
///
/// The scope is live-only. It holds no model arguments, no message content,
/// and no credentials, and it never appears in a serialized
/// [`ToolContext`](mio_core::tool::ToolContext) or in the published result
/// metadata a recorder persists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolInvocation {
    /// The dispatch's id, as minted on the bus for this exact call.
    pub effect_id: u64,
    /// The provider's wire call id, exactly as the model emitted it. It is
    /// adopted verbatim from [`CallId::wire`](mio_core::message::CallId::wire):
    /// never re-created, trimmed, or synthesized.
    pub tool_call_id: String,
    /// The run turn the call belongs to.
    pub turn: usize,
}

impl ToolInvocation {
    /// Capture the identity the engine is dispatching this call as.
    pub(crate) fn capture(
        effect_id: mio_core::effect::EffectId,
        call_id: &mio_core::message::CallId,
        turn: usize,
    ) -> Self {
        Self {
            effect_id: effect_id.as_u64(),
            tool_call_id: call_id.wire().into_owned(),
            turn,
        }
    }
}

#[cfg(test)]
mod tests;
