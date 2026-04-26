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
use crate::usage::Usage;

/// Swappable LLM backend. Implementations MUST be cancel-safe at the
/// boundary: callers may drop the returned future without leaking
/// state.
///
/// Phase 4 of the Tier-1 cleanup widened both methods to return
/// `Usage` alongside the existing payload so the daemon's per-process
/// digest can record cache-hit ratios without re-instrumenting every
/// call site. Callers that don't care about token counts destructure
/// with `let (payload, _usage) = ...`.
#[allow(clippy::manual_async_fn)] // explicit `+ Send` bound required for tokio::spawn (design doc Alt 2)
pub trait LlmClient {
    /// Send a tool-constrained completion. The backend forces the
    /// model to invoke exactly the supplied tool (`tool_choice = {type:
    /// "tool", name: tool.name}` in Anthropic's shape) and returns
    /// the extracted `ToolCall` plus the response's `Usage`.
    ///
    /// `system` and `user` are fully-assembled prompt strings; this
    /// crate does NOT touch them beyond forwarding.
    fn complete_with_tool<'a>(
        &'a self,
        system: &'a str,
        user: &'a str,
        tool: ToolSchema,
    ) -> impl Future<Output = Result<(ToolCall, Usage), LlmError>> + Send + 'a;

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
    /// blocks are discarded by the backend. The returned `Usage`
    /// carries the same per-call counts as `complete_with_tool`.
    fn complete_free<'a>(
        &'a self,
        system: &'a str,
        messages: &'a [ChatMessage],
    ) -> impl Future<Output = Result<(String, Usage), LlmError>> + Send + 'a;
}

/// Forwarding impl for `Arc<L>` so daemon code can build
/// `agents::Deps { llm: Arc::clone(&ctx.llm), .. }` without
/// unwrapping or cloning the underlying client.
impl<L: LlmClient + ?Sized> LlmClient for std::sync::Arc<L> {
    fn complete_with_tool<'a>(
        &'a self,
        system: &'a str,
        user: &'a str,
        tool: ToolSchema,
    ) -> impl Future<Output = Result<(ToolCall, Usage), LlmError>> + Send + 'a {
        (**self).complete_with_tool(system, user, tool)
    }

    fn complete_free<'a>(
        &'a self,
        system: &'a str,
        messages: &'a [ChatMessage],
    ) -> impl Future<Output = Result<(String, Usage), LlmError>> + Send + 'a {
        (**self).complete_free(system, messages)
    }
}
