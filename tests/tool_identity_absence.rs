use mio_coding_agent::tool_identity::{self, ToolIdentity};
use uuid::Uuid;

/// A nested call without engine metadata cannot borrow its caller's identity.
#[tokio::test]
async fn absent_nested_identity_is_unknown_and_restores_the_caller() {
    let outer = ToolIdentity::new(Uuid::new_v4(), "outer-call", 1, 7);
    tool_identity::scope(Some(outer.clone()), async {
        assert_eq!(tool_identity::current(), Some(outer.clone()));
        tool_identity::scope(None, async {
            assert!(tool_identity::current().is_none());
            tokio::task::yield_now().await;
            assert!(tool_identity::current_tool_call_id().is_none());
        })
        .await;
        assert_eq!(tool_identity::current(), Some(outer));
    })
    .await;
    assert!(tool_identity::current().is_none());
}
