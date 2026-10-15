use mio_derive::rig_tool;

#[rig_tool]
fn owned_context(
    context: mio_agent::tool::ToolContext,
) -> Result<(), mio_core::tool::ToolExecutionError> {
    let _ = context;
    Ok(())
}

fn main() {}
