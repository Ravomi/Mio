#![allow(unused_imports)]

use mio_agent::tool::ToolContext;
use mio_derive::rig_tool;

#[rig_tool]
fn multiple_contexts(
    #[rig(context)]
    first: &mut ToolContext,
    #[rig(context)]
    second: &mut ToolContext,
) -> Result<(), mio_core::tool::ToolExecutionError> {
    let _ = (first, second);
    Ok(())
}

fn main() {}
