#![allow(unused_imports)]

use mio_agent::tool::ToolContext;
use mio_derive::rig_tool;

#[rig_tool(required(context))]
fn context_in_required(
    #[rig(context)]
    context: &mut ToolContext,
    query: String,
) -> Result<String, mio_core::tool::ToolExecutionError> {
    let _ = context;
    Ok(query)
}

fn main() {}
