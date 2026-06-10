//! Per-call context propagation.
//!
//! `MeteredLlmClient` is process-wide (one wrapper around the shared
//! `AnthropicClient`), so it cannot know which Plan / Work / role a given
//! `complete_*` call belongs to from its own state. The daemon's spawn
//! task bodies establish that identity once per task; this task-local
//! carries it down to the metered client so `costs.jsonl` can attribute
//! each call to the right Plan/Work/role without threading a context
//! parameter through the `LlmClient` trait (which would ripple to every
//! call site and every test fake).
//!
//! The propagation is task-local, not span-based: tracing span fields
//! are write-only at runtime (no public read-back), whereas a tokio
//! task-local is readable from inside the awaited call. Callers wrap the
//! agent future in [`CallContext::scope`]; callers that don't (CLI
//! one-shots, tests) see [`CallContext::current`] return the default
//! (all-`None`), and the cost line still records run/model/tokens.

use std::future::Future;

/// Identity of the work a single LLM call is attributed to. All fields
/// are optional: the decomposer has no `work_id`, the Director has
/// neither `work_id` nor a per-Work role beyond `director`, and a CLI
/// one-shot has none of them.
#[derive(Clone, Debug, Default)]
pub struct CallContext {
    pub plan_id: Option<String>,
    pub work_id: Option<String>,
    pub role: Option<String>,
}

tokio::task_local! {
    static CALL_CONTEXT: CallContext;
}

impl CallContext {
    /// Run `fut` with this context installed as the task-local for the
    /// duration of the future. Nested scopes shadow outer ones.
    pub async fn scope<F: Future>(ctx: CallContext, fut: F) -> F::Output {
        CALL_CONTEXT.scope(ctx, fut).await
    }

    /// The context installed by the nearest enclosing [`scope`], or the
    /// default (all-`None`) when no scope is active.
    ///
    /// [`scope`]: CallContext::scope
    pub fn current() -> CallContext {
        CALL_CONTEXT.try_with(|c| c.clone()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests;
