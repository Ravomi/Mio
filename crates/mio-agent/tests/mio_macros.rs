//! Exercise the public macro re-export after renaming the runtime.
#![cfg(feature = "derive")]
#![allow(clippy::unwrap_used)]
use mio_agent::tool::Tool;

#[mio_agent::rig_tool(description = "Echo a value", params(value = "Value to echo"))]
fn echo(value: String) -> Result<String, mio_agent::core::tool::ToolExecutionError> {
    Ok(value)
}

#[tokio::test]
async fn generated_tool_runs() {
    let result = Echo
        .call(
            &mut mio_agent::tool::ToolContext::new(),
            EchoParameters {
                value: "mio".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(result, "mio");
}
