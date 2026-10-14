//! User-facing integrations for classic agents.
//!
//! ```
//! use mio_agent::integrations::cli_chatbot::ChatBotBuilder;
//! let builder = ChatBotBuilder::new();
//! ```
/// A terminal chat loop over an agent.
pub mod cli_chatbot;
