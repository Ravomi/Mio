#![allow(dead_code)]

use mio_derive::rig_tool;

#[rig_tool(name = "search-docs")]
fn search_docs_impl() -> Result<String, mio_core::tool::ToolExecutionError> {
    Ok("ok".to_string())
}

fn main() {}
