//! Common imports for Rig's classic runtime.
//!
//! ```
//! use mio_agent::prelude::*;
//! use mio_core::operation::Completion;
//!
//! fn assistant(model: impl Into<DynModel<Completion>>) -> Agent {
//!     AgentBuilder::new(model).build()
//! }
//! ```

pub use crate::agent::{
    Agent, AgentBuilder, AgentHook, HookContext, ModelHandle, ModelRef, ModelSelection,
    ModelSelectionAction, MultiTurnStreamItem, RunEvents, StreamingResult,
};
pub use crate::completion::{Message, PromptError, StructuredOutputError};
pub use crate::tool::{Tool, ToolSet};
pub use mio_core::driver::{DynModel, Model};
pub use mio_core::error::ProviderError;

pub use mio_core::Embed;
pub use mio_core::embeddings::EmbeddingsBuilder;
pub use mio_core::vector_store::VectorStoreIndex;
pub use mio_core::vector_store::in_memory_store::InMemoryVectorStore;
pub use mio_core::vector_store::request::VectorSearchRequest;
