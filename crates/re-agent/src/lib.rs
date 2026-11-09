//! AI agents (UI-independent): conversations and their on-disk store, the
//! tool interface and registry, the agent loop as a pure state machine, the
//! model-provider protocol, and small helpers (diffs, prompt templates,
//! argument validation).
//!
//! The host (the editor UI, or a test) drives a [`session::Session`]: it
//! feeds user messages, model events, prepared tools, approvals and tool
//! results, and carries out the [`session::Effect`]s the session returns
//! (start the model, prepare/run a tool, ask the user, save). Nothing here
//! touches the editor state or the network; the session never runs a tool
//! or approves anything by itself.

pub mod conversation;
pub mod diff;
pub mod prompt;
pub mod protocol;
pub mod provider;
pub mod schema;
pub mod session;
pub mod setup;
pub mod store;
pub mod tools;

pub use conversation::{Conversation, Message, Subject, ToolCall};
pub use session::{Effect, Session};
pub use tools::{
    Action, Approval, DocSnapshot, Prepared, Remember, Risk, Tool, ToolContext, ToolOutput,
    ToolRegistry, ToolSpec,
};

/// Agent API version (agent definitions, provider protocol, tools).
pub const API_VERSION: u64 = 0;
