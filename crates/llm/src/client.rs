//! The `LlmClient` trait: a single buffered, non-streaming
//! tool-constrained completion call.
//!
//! Stage 6 shape locked by scope memo D6. Streaming, multi-turn
//! history, and model-tier resolution are Stage 7+ concerns and land
//! as additional trait methods on this same trait when the agents
//! crate motivates them.
//!
//! Generics rather than `dyn` (scope memo U+4). `impl Future<...> +
//! Send` rather than `async fn` in trait so the Send bound is explicit
//! (tokio-spawned tasks need it); the explicit `+ '_` pins the
//! returned future's lifetime to `&self` and the `&str` parameters.
//! On edition 2024 the `+ '_` is the default for RPIT; keeping it
//! written out makes the capture contract legible without depending
//! on the reader knowing 2024's RPIT rules.

use std::future::Future;

use crate::error::LlmError;
use crate::tool::{ToolCall, ToolSchema};

/// Swappable LLM backend. Stage 6 surface is intentionally small: one
/// method, one response shape. Implementations MUST be cancel-safe at
/// the boundary: callers may drop the returned future without leaking
/// state.
pub trait LlmClient {
    /// Send a tool-constrained completion. The backend forces the
    /// model to invoke exactly the supplied tool (`tool_choice = {type:
    /// "tool", name: tool.name}` in Anthropic's shape) and returns
    /// the extracted `ToolCall`.
    ///
    /// `system` and `user` are fully-assembled prompt strings; this
    /// crate does NOT touch them beyond forwarding.
    fn complete_with_tool(
        &self,
        system: &str,
        user: &str,
        tool: ToolSchema,
    ) -> impl Future<Output = Result<ToolCall, LlmError>> + Send + '_;
}
