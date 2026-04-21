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
//! (tokio-spawned tasks need it). A single named lifetime `'a` binds
//! `&self`, `system`, and `user` so the returned future captures all
//! three under one constraint; using anonymous `'_` instead fails to
//! unify the parameter lifetimes and the compiler rejects it.

use std::future::Future;

use crate::error::LlmError;
use crate::tool::{ToolCall, ToolSchema};

/// Swappable LLM backend. Stage 6 surface is intentionally small: one
/// method, one response shape. Implementations MUST be cancel-safe at
/// the boundary: callers may drop the returned future without leaking
/// state.
#[allow(clippy::manual_async_fn)] // explicit `+ Send` bound required for tokio::spawn (design doc Alt 2)
pub trait LlmClient {
    /// Send a tool-constrained completion. The backend forces the
    /// model to invoke exactly the supplied tool (`tool_choice = {type:
    /// "tool", name: tool.name}` in Anthropic's shape) and returns
    /// the extracted `ToolCall`.
    ///
    /// `system` and `user` are fully-assembled prompt strings; this
    /// crate does NOT touch them beyond forwarding.
    fn complete_with_tool<'a>(
        &'a self,
        system: &'a str,
        user: &'a str,
        tool: ToolSchema,
    ) -> impl Future<Output = Result<ToolCall, LlmError>> + Send + 'a;
}
