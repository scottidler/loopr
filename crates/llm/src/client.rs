//! The `LlmClient` trait: a single buffered, non-streaming
//! tool-constrained completion call.
//!
//! Stage 6 shape locked by scope memo D6. Streaming, multi-turn
//! history, and model-tier resolution are Stage 7+ concerns and land
//! as additional trait methods on this same trait when the agents
//! crate motivates them.
//!
//! Generics rather than `dyn` (scope memo U+4). `#[trait_variant::make(Send)]`
//! generates the explicit `+ Send` bound on returned futures (tokio-spawned
//! tasks need it) from a clean `async fn` source.

use crate::error::LlmError;
use crate::message::Message;
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
#[trait_variant::make(Send)]
pub trait LlmClient {
    /// Send a tool-constrained completion. The backend forces the
    /// model to invoke exactly the supplied tool (`tool_choice = {type:
    /// "tool", name: tool.name}` in Anthropic's shape) and returns
    /// the extracted `ToolCall` plus the response's `Usage`.
    ///
    /// `system` and `user` are fully-assembled prompt strings; this
    /// crate does NOT touch them beyond forwarding.
    ///
    /// `model` overrides the configured default when `Some`. `None`
    /// uses the client's configured model (the common case). Callers
    /// that need a specific model tier (e.g. Director using Opus) pass
    /// `Some("claude-opus-4-7")`; all other callers pass `None`.
    async fn complete_with_tool(
        &self,
        system: &str,
        user: &str,
        tool: ToolSchema,
        model: Option<&str>,
    ) -> Result<(ToolCall, Usage), LlmError>;

    /// Send a free-form multi-turn completion. No `tool_choice`, no
    /// `tools`: the model replies with plain text (possibly JSON, at
    /// the caller's request inside the prompt). Used by the
    /// Implementer's self-correction sub-loop, Director state-summary
    /// turns, and (later) the Researcher's iterative inquiry.
    ///
    /// Contract: the last message in `messages` MUST have
    /// `role = User` (Anthropic's Messages API requires the turn list
    /// to end with user). The returned `String` is the first
    /// `{"type": "text"}` content block from the response; thinking
    /// blocks are discarded by the backend. The returned `Usage`
    /// carries the same per-call counts as `complete_with_tool`.
    ///
    /// `ToolUse`/`ToolResult` content blocks in `messages` are type-
    /// level defined but not yet wired; `AnthropicClient` returns
    /// `Fatal(NotImplemented)` if encountered until 2.1 ships.
    ///
    /// `model` overrides the configured default when `Some`. `None`
    /// uses the client's configured model (the common case).
    async fn complete_free(
        &self,
        system: &str,
        messages: &[Message],
        model: Option<&str>,
    ) -> Result<(String, Usage), LlmError>;

    /// The client's configured default model ID (the value used when a
    /// call passes `model: None`). Surfaced so callers that record the
    /// model in side channels - e.g. the implementer's `Loopr-Model`
    /// commit trailer - don't have to thread the config string
    /// separately. Returns the literal configured ID, not the
    /// per-response model the provider echoes back (that is the Phase 6
    /// model-pinning detector's concern).
    ///
    /// Defaults to `"unknown-model"` for the benefit of test fakes that
    /// model no particular ID. Every production backend
    /// (`AnthropicClient`, `MeteredLlmClient`, the `Arc<L>` forward)
    /// overrides this; the default is never reached on a real call path.
    fn model(&self) -> &str {
        "unknown-model"
    }
}

/// Forwarding impl for `Arc<L>` so daemon code can build
/// `agents::Deps { llm: Arc::clone(&ctx.llm), .. }` without
/// unwrapping or cloning the underlying client.
impl<L: LlmClient + Send + Sync + ?Sized> LlmClient for std::sync::Arc<L> {
    async fn complete_with_tool(
        &self,
        system: &str,
        user: &str,
        tool: ToolSchema,
        model: Option<&str>,
    ) -> Result<(ToolCall, Usage), LlmError> {
        (**self).complete_with_tool(system, user, tool, model).await
    }

    async fn complete_free(
        &self,
        system: &str,
        messages: &[Message],
        model: Option<&str>,
    ) -> Result<(String, Usage), LlmError> {
        (**self).complete_free(system, messages, model).await
    }

    fn model(&self) -> &str {
        (**self).model()
    }
}
