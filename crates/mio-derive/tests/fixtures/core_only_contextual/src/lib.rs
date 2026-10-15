#[renamed::rig_tool(description = "Echo", params(value = "Input"))]
async fn echo(
    _context: &mut renamed::tool::ToolContext,
    value: String,
) -> Result<String, renamed::tool::ToolExecutionError> {
    Ok(value)
}
