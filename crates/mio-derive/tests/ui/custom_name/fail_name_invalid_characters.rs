#![allow(dead_code)]

use mio_derive::rig_tool;

#[rig_tool(name = "bad name!")]
fn invalid_name_characters() -> Result<String, mio_core::tool::ToolExecutionError> {
    Ok("ok".to_string())
}

fn main() {}
