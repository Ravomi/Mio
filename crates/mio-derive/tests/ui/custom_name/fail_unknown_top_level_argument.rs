#![allow(dead_code)]

use mio_derive::rig_tool;

#[rig_tool(nam = "search-docs")]
fn unknown_argument() -> Result<String, mio_core::tool::ToolExecutionError> {
    Ok("ok".to_string())
}

fn main() {}
