#![allow(dead_code)]

use mio_derive::rig_tool;

#[rig_tool(name = 123)]
fn invalid_name_value() -> Result<String, mio_core::tool::ToolExecutionError> {
    Ok("ok".to_string())
}

fn main() {}
