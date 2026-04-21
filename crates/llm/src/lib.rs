//! Network boundary for LLM API calls. Agnostic of prompt content,
//! tool schemas, and context assembly: prompt assembly lives in
//! `agents`; this crate is the API-bounds layer only.
//!
//! Stage 6 surface is small on purpose: one trait, one method, one
//! concrete impl (Anthropic). Streaming, multi-turn history, model-
//! tier resolution, and cost-accounting spans are Stage 7+ concerns
//! tracked in the design doc.

mod config;
mod error;
mod tool;

pub use config::LlmConfig;
pub use error::{FatalReason, LlmError};
pub use tool::{ToolCall, ToolSchema};
