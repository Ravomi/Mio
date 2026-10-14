//! Ensure generated paths resolve the renamed provider crate.
#![cfg(feature = "derive")]
#![allow(clippy::unwrap_used)]
use mio_core::tool::{ContextValue, ToolContext};

#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize, mio_core::ContextValue)]
struct SessionId(String);

#[test]
fn derived_context_round_trips() {
    assert_eq!(SessionId::KEY, "SessionId");
    let mut context = ToolContext::new();
    context.insert(SessionId("mio".into())).unwrap();
    assert_eq!(
        context.get::<SessionId>().unwrap(),
        Some(SessionId("mio".into()))
    );
}
