//! Network boundary for LLM API calls. Agnostic of prompt content,
//! tool schemas, and context assembly: prompt assembly lives in
//! `agents`; this crate is the API-bounds layer only.
//!
//! Stage 6 surface is small on purpose: one trait, one method, one
//! concrete impl (Anthropic). Streaming, multi-turn history, model-
//! tier resolution, and cost-accounting spans are Stage 7+ concerns
//! tracked in the design doc.

mod anthropic;
mod call;
mod client;
mod config;
mod error;
mod message;
mod metered;
mod tier;
mod tool;
mod usage;

#[cfg(feature = "stub")]
mod stub;

pub use anthropic::AnthropicClient;
pub use call::CallContext;
pub use client::LlmClient;
pub use config::LlmConfig;
pub use error::{FatalReason, LlmError};
pub use message::{Message, MessageContent, MessageRole};
pub use metered::{CostSink, MeteredLlmClient};
pub use tier::ModelTiers;
pub use tool::{ToolCall, ToolSchema};
pub use usage::Usage;

#[cfg(feature = "stub")]
pub use stub::ScriptedLlm;
