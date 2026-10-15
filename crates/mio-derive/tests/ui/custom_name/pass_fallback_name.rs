#![allow(dead_code)]

use mio_derive::rig_tool;

#[rig_tool]
fn fallback_name_tool() -> Result<String, mio_core::tool::ToolExecutionError> {
    Ok("fallback".to_string())
}

fn main() {}
