#![allow(unused_imports)]

use mio_agent::tool::ToolContext;
use mio_derive::rig_tool;

#[rig_tool]
fn immutable_context(
    #[rig(context)] context: &ToolContext,
) -> Result<(), mio_core::tool::ToolExecutionError> {
    let _ = context;
    Ok(())
}

fn main() {}
