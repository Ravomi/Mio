#![allow(dead_code)]

use mio_derive::rig_tool;

#[rig_tool(name = "a2345678a2345678a2345678a2345678a2345678a2345678a2345678a2345678x")]
fn invalid_name_length() -> Result<String, mio_core::tool::ToolExecutionError> {
    Ok("ok".to_string())
}

fn main() {}
