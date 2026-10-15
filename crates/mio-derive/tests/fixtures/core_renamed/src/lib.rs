#[renamed::rig_tool(description = "Echo", params(value = "Input"))]
async fn echo(value: String) -> Result<String, renamed::tool::ToolExecutionError> {
    Ok(value)
}
