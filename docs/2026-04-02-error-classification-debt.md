# Error Classification Debt

## The CONFIG_PATTERNS Problem

`src/agents/lifeguard.rs` has an `is_config_error()` function that pattern-matches error
strings to decide whether an error is an infrastructure failure (not the agent's fault) vs
an agent loop (the agent is stuck and should be escalated).

This is the wrong abstraction. A string whitelist will never cover everything:
- Every new tool needs a new entry
- Every OS, shell, or environment phrases errors differently
- Errors from new sources (e.g. `failed to spawn command`) fall through to Unknown

## What Should Happen Instead

Errors need to be **typed at the source**, not pattern-matched at the sink.

When the tool runner fails to spawn a process, that is `AgentErrorKind::ToolFailure` - a
typed error, emitted at the point of failure. The lifeguard and coordinator should route on
the kind, not grep the message.

`AgentErrorKind` already has the right variants:
```rust
pub enum AgentErrorKind {
    ContextOverflow,
    ParseExhausted,
    LlmTransient,
    ToolFailure,  // <- exists, but classify_error() never produces it
    Unknown,
}
```

`ToolFailure` is never produced by `classify_error()`. It has to be. The executor needs to
catch spawn failures and other tool-level errors at the point they occur and wrap them in a
typed `AgentError::ToolFailure { .. }` so the lifeguard gets a kind, not a string.

## Scope of Work

1. Add `AgentError::ToolFailure { command: String, reason: String }` to `src/agents/error.rs`
2. Emit it in the tool runner / executor when spawn fails, command not found, etc.
3. Update `classify_error()` to downcast it to `AgentErrorKind::ToolFailure`
4. Update lifeguard to handle `ToolFailure` distinctly - don't count toward loop detection,
   escalate immediately with the typed reason so the coordinator knows it's infrastructure

Delete `is_config_error()` and `CONFIG_PATTERNS` when done. They are a band-aid on a
missing type.
