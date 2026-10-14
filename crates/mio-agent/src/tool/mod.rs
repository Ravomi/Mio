//! Tool authoring, ordered registrations, and structured execution results.
//!
//! [`Tool`] supplies typed calls, [`ToolContext`] carries inbound and host-only
//! result metadata, and [`ToolSet`] or [`server::ToolServer`] manages registrations.
//! Arbitrary source errors receive safe model feedback by default; explicit
//! [`ToolExecutionError`] messages are model-visible.
//!
//! ```
//! use mio_agent::tool::ToolOutput;
//! let output = ToolOutput::text("Search complete.");
//! assert_eq!(output.as_content().len(), 1);
//! ```

pub mod builtin;
pub mod catalog;
pub mod registry;
pub mod server;

pub use catalog::{ToolCatalog, ToolLease};
pub use mio_core::tool::{
    DynamicTool, ErasedTool, IntoToolOutput, Tool, ToolEmbedding, ToolErrorKind,
    ToolExecutionError, ToolOutput, ToolResult, tool_definition, tool_name,
};
pub use mio_core::tool::{ToolContext, ToolContextError};
pub use registry::{RegisteredTool, ToolDispatch, ToolSet, execute_tool};

#[cfg(test)]
mod toolset_clone_tests;

#[cfg(test)]
mod migrated_tests;
