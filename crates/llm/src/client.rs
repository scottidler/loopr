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
use crate::message::ChatMessage;
use crate::tool::{ToolCall, ToolSchema};

/// Swappable LLM backend. Implementations MUST be cancel-safe at the
/// boundary: callers may drop the returned future without leaking
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

    /// Send a free-form multi-turn completion. No `tool_choice`, no
    /// `tools`: the model replies with plain text (possibly JSON, at
    /// the caller's request inside the prompt). Used by the
    /// Implementer's self-correction sub-loop, where parse failures
    /// append `(assistant: raw, user: error)` pairs to `messages` and
    /// the next call sees the full conversation.
    ///
    /// Contract: the last message in `messages` MUST have
    /// `role = "user"` (Anthropic's Messages API requires the turn
    /// list to end with user). The returned `String` is the first
    /// `{"type": "text"}` content block from the response; thinking
    /// blocks are discarded by the backend.
    fn complete_free<'a>(
        &'a self,
        system: &'a str,
        messages: &'a [ChatMessage],
    ) -> impl Future<Output = Result<String, LlmError>> + Send + 'a;
}
