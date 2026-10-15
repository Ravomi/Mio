#![allow(dead_code)]

use mio_derive::rig_tool;

#[rig_tool(name = "9bad")]
fn invalid_name_start() -> Result<String, mio_core::tool::ToolExecutionError> {
    Ok("ok".to_string())
}

fn main() {}
