# Design Document: Multi-turn LLM History and Context Builder v2

**Author:** Scott A. Idler
**Date:** 2026-05-08
**Status:** Implemented
**Review Passes Completed:** 5/5
**Crates touched:** `llm`, `context`, `agents`

---

## Summary

Three tightly coupled changes that together unlock the Director, Researcher, and any other multi-turn LLM agent in v5:

1. Replace `llm::ChatMessage { role, content: String }` with a richer `Message` type whose content is `Vec<MessageContent>` — supporting `Text`, `ToolUse`, and `ToolResult` blocks.
2. Replace `context::AssembledContext.user_message: String` with `messages: Vec<Message>` so the assembled context carries the full opening history rather than just one string.
3. Extend `ContextBuilder` with `build_for_director` and `build_for_researcher` entry points that assemble multi-turn state-summary histories, with token-budget-aware trimming.

The Implementer migrates to the new types mechanically. `AnthropicClient::send_free_request` is updated to serialise the richer content types; `ToolUse`/`ToolResult` variants are wired through but return `Fatal(NotImplemented)` until the Researcher ships (2.1).

---

## Problem Statement

### Background

`LlmClient::complete_free` was added in the Tier-1 cleanup to support the Implementer's self-correction sub-loop. Its signature takes `&[ChatMessage]`, where `ChatMessage { role: String, content: String }` is a flat text-only representation. That is enough for the Implementer because it only exchanges text messages with the model (actions JSON in, corrections out).

The Stage 6 scope memo explicitly deferred `context-builder.md` to Stage 7 (`docs/design/2026-04-20-stage-6-scope.md` D11). `AssembledContext` currently carries a `user_message: String` field that callers convert to a one-element message vec themselves.

### Problem

Three concrete gaps prevent building the Director (1.2) and Researcher (2.1):

1. **No tool-use/tool-result content blocks.** The Researcher's ralph loop calls tools (file reads, searches) and must fold the tool invocations and their results back into the LLM history. `ChatMessage.content: String` has no representation for structured blocks; there is no way to send `tool_use` + `tool_result` turns.

2. **Single-turn `AssembledContext`.** Director assembles a state summary then carries the conversation across multiple turns: `user: state_summary → assistant: actions → user: execution_results + new_state → …`. The current `AssembledContext.user_message: String` field forces callers to build the history themselves from scratch each turn, duplicating token-budget logic and bypassing the `context` crate.

3. **No multi-turn `ContextBuilder` entry points.** `build_for_implementer` and `build_for_reviewer` are the only two entry points. Director and Researcher have no contract defined; each would hand-roll its own prompt assembly, defeating the SSOT purpose of the `context` crate.

### Goals

- A typed `Message` / `MessageContent` pair that covers text, tool-use, and tool-result turns (all three needed by Researcher, only text needed by Director Phase 1).
- `AssembledContext.messages: Vec<Message>` replaces `user_message: String`; Implementer and Reviewer callers are migrated mechanically.
- `ContextBuilder::build_for_director(state, history, token_budget)` and `::build_for_researcher(query, history, token_budget)` defined as trait methods; `InlineContextBuilder` implements them backed by `.pmt` templates.
- History-trimming utility in `context`: respects token budget, preserves tool-use/tool-result pairing invariant, drops oldest turns first.
- `complete_free` signature updated to `&[Message]`; no semantic change for text-only callers.

### Non-Goals

- **Full Director agent** (`run_director`) — that is 1.2.
- **Full Researcher agent** (`run_researcher`) — that is 2.1.
- **`ToolUse`/`ToolResult` round-trip through `AnthropicClient`** — typed but `NotImplemented` until 2.1 needs it.
- **SSE / streaming** — deferred; no use case yet.
- **Model tier resolution** (`primary`/`lightweight`/`advisor`) — deferred to AutoResearch (4.2).
- **LLM response cache** — deferred (4.4).
- **Per-session turn persistence** — Director's history lives in-memory in its ralph loop; TaskStore persistence of LLM conversation history is a separate deferred item.
- **`complete_free` return type** — `complete_free` returns `(String, Usage)`, extracting the first `Text` content block. When the Researcher needs the LLM to emit `ToolUse` blocks back to the caller (so the agent can execute the tool and fold the result into history), a new trait method `complete_agentic` returning `(Vec<MessageContent>, Usage)` is required. That method is **deferred to 2.1**; 2.4 only redesigns the input type. Callers in 2.1 that need the raw content blocks will add `complete_agentic` at that time.

---

## Proposed Solution

### Overview

```
Before:                              After:

ChatMessage { role, content:String } Message { role, content:Vec<MessageContent> }
AssembledContext.user_message:String AssembledContext.messages:Vec<Message>
LlmClient::complete_free(&[ChatMessage]) complete_free(&[Message])
ContextBuilder: 2 methods           ContextBuilder: 4 methods
```

All changes are internal to three crates. The public API of each crate changes at the type boundary (argument types), not the call shape. No new IPC messages, no new domain records, no new config keys.

### Architecture

#### `llm`: richer message types

```rust
// llm/src/message.rs

pub struct Message {
    pub role: MessageRole,
    pub content: Vec<MessageContent>,
}

pub enum MessageRole {
    User,
    Assistant,
}

pub enum MessageContent {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}
```

Convenience constructors:

```rust
impl Message {
    pub fn user(text: impl Into<String>) -> Self { ... }
    pub fn assistant(text: impl Into<String>) -> Self { ... }
    pub fn tool_result(tool_use_id: impl Into<String>, content: impl Into<String>) -> Self { ... }
}
```

`MessageRole` is an enum rather than a bare `String` to prevent role-string typos at construction sites. Serde representation: `"user"` / `"assistant"` (lowercase, matches Anthropic wire).

`ToolUse` and `ToolResult` are defined now. `AnthropicClient::send_free_request` handles them in the wire serialisation loop; until 2.1 calls them in production, encountering them returns `Fatal(FatalReason::NotImplemented { feature: "tool-use message content" })`. The type definition is final; the transport implementation is staged.

`FatalReason` gains a new variant in `llm/src/error.rs`:

```rust
/// Called a code path that is type-level-defined but not yet wired.
/// Returned when AnthropicClient encounters ToolUse/ToolResult content
/// blocks before the 2.1 Researcher ships. This is always a caller error
/// (no agent sends these blocks until 2.1 activates the Researcher).
NotImplemented { feature: String },
```

**Wire body shape for multi-block messages.** Anthropic's Messages API accepts either a plain string or a content-block array for `content`. For single-`Text` messages (the current case and all Director turns), the wire body continues to use a plain string: `{"role":"user","content":"..."}`. For multi-block turns (tool-use turns in the Researcher path), the wire body uses the array form: `{"role":"assistant","content":[{"type":"tool_use","id":"...","name":"...","input":{...}}]}`. The `AnthropicFreeRequest` wire struct changes `messages: Vec<AnthropicMessage<'a>>` where `AnthropicMessage.content` becomes `serde_json::Value` (a string for single-text blocks, a JSON array for multi-block). A helper `fn to_wire_content(blocks: &[MessageContent]) -> Value` handles the conversion; if the slice is exactly one `Text` block it returns `Value::String`; otherwise it returns a JSON array of content-block objects.

#### `llm`: updated trait signature

```rust
pub trait LlmClient {
    fn complete_with_tool<'a>(...) -> ...; // unchanged

    fn complete_free<'a>(
        &'a self,
        system: &'a str,
        messages: &'a [Message],
    ) -> impl Future<Output = Result<(String, Usage), LlmError>> + Send + 'a;
}
```

No semantic change: callers that build `vec![Message::user(text)]` call exactly the same wire path as before. The `ChatMessage` type is deleted; all uses in `agents` are mechanically replaced.

#### `context`: updated `AssembledContext`

```rust
pub struct AssembledContext {
    pub system_prompt: String,
    pub messages: Vec<Message>,   // replaces user_message: String
    pub token_estimate: usize,
}

impl AssembledContext {
    /// Convenience accessor: returns the text of the first message if it
    /// is a single-text-block user message. Used by the Implementer's
    /// transcript writer (which logs the opening user prompt); not the
    /// primary API. Returns `None` if `messages` is empty or the first
    /// message's first content block is not `Text`.
    pub fn first_user_text(&self) -> Option<&str> { ... }
}
```

`build_for_implementer` returns `messages: vec![Message::user(rendered_user)]`. The Implementer's per-iteration local `messages` vec is initialised from `ctx.messages.clone()` instead of `vec![ChatMessage::user(ctx.user_message.clone())]`.

#### `context`: new `ContextBuilder` methods

```rust
pub trait ContextBuilder: Send + Sync {
    fn build_for_implementer(...) -> Result<AssembledContext, ContextError>; // updated return type
    fn build_for_reviewer(...) -> Result<AssembledContext, ContextError>;    // unchanged

    /// Assemble the Director's context for one turn. Message order:
    ///   [trimmed prior history] + [fresh state summary (current user turn)]
    ///
    /// The state summary is the LAST message — the one the model responds to.
    /// Prior history (assistant responses to earlier turns) is prepended so
    /// the model has conversational context, but the current world-state
    /// description ends the array as the most recent user input.
    /// `history` must already alternate user→assistant; the caller maintains
    /// this invariant before calling in.
    fn build_for_director(
        &self,
        state: &DirectorState,
        history: &[Message],
        token_budget: usize,
    ) -> Result<AssembledContext, ContextError>;

    /// Assemble the Researcher's context for one turn. Message order:
    ///   [trimmed prior history] + [current question (user turn)]
    ///
    /// Same ordering contract as `build_for_director`: current query last.
    /// System prompt defines the Researcher role and tool-use conventions.
    fn build_for_researcher(
        &self,
        query: &ResearchQuery,
        history: &[Message],
        token_budget: usize,
    ) -> Result<AssembledContext, ContextError>;
}
```

`DirectorState` and `ResearchQuery` are new minimal structs in `context` (not in `domain`) because they carry assembled-prompt data, not persisted records:

```rust
pub struct DirectorState {
    pub plan_id: String,
    /// One line per Work: id, title, status string, attempt count.
    /// All strings - context does NOT import domain types.
    pub works: Vec<WorkLine>,
    pub bundles: Vec<BundleLine>,
    pub blocked_reason: Option<String>,
}

pub struct WorkLine {
    pub id: String,
    pub title: String,
    pub status: String,       // "Pending", "InProgress", etc. — stringified by caller
    pub attempt_count: u32,
}

pub struct BundleLine {
    pub id: String,
    pub work_id: String,
    pub status: String,
}

pub struct ResearchQuery {
    pub question: String,
    pub context_hints: Vec<String>,  // file paths, symbol names
}
```

These structs live in `context/src/lib.rs`. Note: `context` already depends on `domain` (it uses `Work`, `Bundle` etc. in `build_for_implementer`/`build_for_reviewer`). The reason `DirectorState`/`ResearchQuery` use `String` for status fields rather than `domain::WorkStatus` / `domain::BundleStatus` is display-orientation: the caller (a future `agents::run_director`) assembles a human-readable view of the state. Using raw domain status enums here would propagate FSM details into the prompt-assembly contract unnecessarily.

#### `context`: history-trimming utility

```rust
/// Trim `history` to fit within `budget` (in rough tokens) by dropping
/// the OLDEST turns first. Never splits a contiguous `ToolUse`/`ToolResult`
/// pair. If even a single turn exceeds the budget, returns an empty slice
/// and callers must warn and proceed with system + state only.
pub fn trim_history(history: &[Message], budget: usize) -> Vec<Message> { ... }
```

**Token estimate per message:** `Text(s)` → `s.len() / CHARS_PER_TOKEN`; `ToolUse { input, .. }` → `serde_json::to_string(&input).map(|s| s.len()).unwrap_or(0) / CHARS_PER_TOKEN`; `ToolResult { content, .. }` → `content.len() / CHARS_PER_TOKEN`. Sum across all content blocks in the message, then across all messages. Same `CHARS_PER_TOKEN = 4` heuristic as the rest of the codebase.

**Alternating-role invariant.** Anthropic's Messages API requires strict alternation: `user → assistant → user → assistant → …`. `trim_history` must trim in complete **Logical Turn Arcs** to preserve this invariant and to avoid dangling `ToolResult` turns.

**Logical Turn Arc definition:** An arc begins at any `User` message whose content does NOT include `ToolResult` blocks (i.e., a fresh prompt or state summary, not a tool response). The arc extends forward through all subsequent messages — including any number of `[Assistant(ToolUse...), User(ToolResult...)]` sub-cycles — up to and including the final `Assistant` message before the next fresh `User` message. In other words: scan forward from a non-ToolResult User message until you find the next non-ToolResult User message; everything up to (not including) that boundary is one arc.

Examples of complete arcs:
- Plain-text exchange: `[User(text), Assistant(text)]` — 2 messages.
- Single tool call: `[User(prompt), Assistant(ToolUse), User(ToolResult), Assistant(text)]` — 4 messages.
- Sequential tool calls: `[User(prompt), Assistant(ToolUse1), User(ToolResult1), Assistant(ToolUse2), User(ToolResult2), Assistant(text)]` — 6 messages.

`trim_history` drops the oldest complete arc as one unit; it never drops a partial arc. After trimming, the remaining history always starts with a fresh non-ToolResult User message, preserving both the alternation rule and the ToolUse-must-precede-ToolResult rule.

**Budget exhausted before history can be added.** If `trim_history` empties the entire history but the system prompt + state summary alone already exceed `token_budget`, `build_for_director` emits a `warn!` and proceeds with `messages: [state_summary_only]`. It does NOT return `ContextError::BudgetExceeded` — a partial context is better than a hard failure here; the Director will see an incomplete view but can still operate. If the state summary itself exceeds the per-crate `MAX_CONTEXT_CHARS` constant (a future addition, not defined in 2.4), truncate the summary with an ellipsis marker using the same `cap_chars` helper already in the codebase.

### Data Model

No new domain records. No new config fields. The `llm::LlmConfig` is unchanged.

### API Design

See Architecture section above. Summary of changed public signatures:

| Location | Before | After |
|---|---|---|
| `llm::LlmClient::complete_free` | `&[ChatMessage]` | `&[Message]` |
| `context::AssembledContext.user_message` | `String` | removed; `messages: Vec<Message>` added |
| `context::ContextBuilder` | 2 methods | 4 methods |
| `agents::run_implementer` (local) | `Vec<ChatMessage>` | `Vec<Message>` |

### Implementation Plan

#### Phase 1: `Message` type in `llm`
**Model:** sonnet
- Create `llm/src/message.rs` with `Message`, `MessageRole`, `MessageContent`.
- Add `serde` derives; `MessageRole` serialises as `"user"` / `"assistant"`.
- Add `Message::user`, `::assistant`, `::tool_result` constructors.
- Delete `ChatMessage` (it was internal; any consumers in `agents` are caught at compile time).
- Update `LlmClient::complete_free` signature to `&[Message]`.
- Update `Arc<L: LlmClient>` forwarding impl.
- Update `AnthropicClient::send_free_request` wire loop:
  - `Text` blocks: serialise as `{"role":"...", "content":"..."}` (existing shape).
  - `ToolUse` / `ToolResult` blocks: serialise correctly per Anthropic Messages API spec (multi-block content array); return `Fatal(NotImplemented)` if encountered until 2.1 activates.
- Update `ScriptedLlm` in `llm/src/stub.rs` (the `complete_free` signature and its tests that call `complete_free("", &[])` — these compile fine since an empty slice is valid for any element type, but internal imports of `ChatMessage` must be removed).
- Update `MeteredLlmClient` in `llm/src/metered.rs` (same signature change, one-line fix).
- `otto ci` at `crates/llm/` passes.

#### Phase 2: `AssembledContext` and `InlineContextBuilder` update
**Model:** sonnet
- Change `AssembledContext.user_message: String` to `messages: Vec<Message>` in `context/src/lib.rs`.
- Add `AssembledContext::first_user_text() -> Option<&str>` convenience accessor.
- Update `InlineContextBuilder::build_for_implementer` to return `messages: vec![Message::user(rendered)]`.
- Update `InlineContextBuilder::build_for_reviewer` accordingly.
- Fix all compile errors in `agents` that reference `assembled.user_message`:
  - `let mut messages = vec![ChatMessage::user(assembled.user_message.clone())]` becomes `let mut messages = ctx.messages.clone()`.
  - `assembled.user_message` references in transcript writer (`write_implementer_transcript` call sites in `crates/agents/src/implementer.rs`) become `assembled.first_user_text().unwrap_or_default()`. This is a transcript-quality tradeoff: if the context builder ever returns a first message with non-Text content, the transcript logs an empty string for the user prompt. Acceptable for now; the Implementer always gets a Text-only opening message.
  - `ChatMessage::assistant(raw)` becomes `Message::assistant(raw)`.
  - `ChatMessage::user(...)` becomes `Message::user(...)`.
- Update test files (all `use llm::ChatMessage` imports must be replaced with `use llm::Message`):
  - `crates/agents/src/implementer/tests.rs`
  - `crates/context/src/implementer/tests.rs`
  - `crates/context/src/reviewer/tests.rs`
  - Any test that constructs `AssembledContext { system_prompt, user_message, token_estimate }` directly must switch to `AssembledContext { system_prompt, messages: vec![Message::user(...)], token_estimate }`.
- `otto ci` at `crates/context/` and `crates/agents/` passes.

#### Phase 3: `build_for_director` and `build_for_researcher`
**Model:** opus
- Add `DirectorState` and `ResearchQuery` structs to `context/src/lib.rs` (public, in scope; not in `domain`).
- Add `build_for_director` and `build_for_researcher` to the `ContextBuilder` trait.
- Implement in `InlineContextBuilder`:
  - Both methods: render the current state/query as a new `Message::user(...)`, call `trim_history(history, remaining_budget)` to get the trimmed prior turns, then assemble `[trimmed_history..., fresh_state_message]`. The fresh state message is always last.
  - Create `.pmt` stub files in `crates/context/prompts/agents/director/system.pmt`, `user.pmt` and `agents/researcher/system.pmt`, `user.pmt`. These must be real files with valid handlebars templates (even if the content is placeholder text) because they are baked into the binary via `include_dir!()`. Placeholder text: `"# Director\nYou are the Director."` etc. 1.2's design doc replaces the content with real prompt text.
  - The `PromptLoader` baked set must include these paths so `render(...)` does not panic.
- Add `trim_history` utility to `context/src/history.rs` (new file).
- `otto ci` at `crates/context/` passes.

#### Phase 4: Seam tests
**Model:** sonnet
- `crates/llm/tests/message.rs`: serde round-trip for each `MessageContent` variant; check `role` serialises to `"user"` / `"assistant"`.
- `crates/llm/tests/anthropic.rs`: add a test for `complete_free` with a multi-element history (two text turns) — mock returns a successful text response; assert the wire body sent contains both turns.
- `crates/context/tests/history.rs`: `trim_history` tests covering:
  - `budget=0` returns empty slice.
  - Budget large enough for all messages returns all messages unchanged.
  - 2-message plain-text arc dropped atomically: `[user0, asst0, user1, asst1]` → drop oldest arc → `[user1, asst1]`.
  - 4-message single-tool arc dropped atomically: `[user0, asst0(ToolUse), user0_result(ToolResult), asst0_final, user1, asst1]` — oldest arc is the 4-message unit `[user0..asst0_final]`; trimmed result is `[user1, asst1]`.
  - 6-message sequential-tool arc dropped atomically: `[user0, asst0(TU1), user0_r1(TR1), asst0(TU2), user0_r2(TR2), asst0_final, user1, asst1]` — oldest arc is all 6 messages; trimmed result is `[user1, asst1]`.
  - Mixed history: 2-message arc followed by 4-message arc — the 2-message arc is dropped first (it is oldest).
  - Token estimate for `ToolUse.input` uses `serde_json` serialized length.
- `crates/context/tests/instrumentation.rs`: add `build_for_director` and `build_for_researcher` smoke spans (stub state, empty history); assert spans emit `role`, `history_len`, `token_estimate`.
- `otto ci` at workspace root passes.

#### Phase 5: Update deferred roadmap + ship
**Model:** sonnet
- Update `docs/deferred-roadmap.md`: mark 2.4 as `Status: Implemented`.
- Update `docs/design/2026-05-08-multi-turn-llm.md` status to `Implemented`.
- Commit, `/bump -a`.

---

## Alternatives Considered

### Alternative 1: Keep `ChatMessage`, add a separate `RichMessage` for multi-turn agents

- **Description:** Leave the existing `ChatMessage { role, content: String }` unchanged; add a new `Message` enum with full content types. `complete_free` keeps taking `ChatMessage`; a new `complete_agentic` takes `RichMessage`.
- **Pros:** Zero migration cost for the Implementer and existing tests.
- **Cons:** Two parallel message-type hierarchies. Callers must decide which type to use; there is no inherent constraint stopping someone from accidentally mixing them. The `ContextBuilder` return type must account for both shapes, leading to `AssembledContext` carrying a `messages: Option<Vec<RichMessage>>` field and a `user_message: Option<String>` — an Either that encodes the decision in the wrong place.
- **Why not chosen:** Vision.md's typed-seams thesis. One type that covers the full requirement is strictly cleaner than two types that cover different sub-requirements.

### Alternative 2: Use `serde_json::Value` for message content

- **Description:** `Message { role: String, content: Value }` — the content is an untyped JSON blob, exactly as Anthropic sends it.
- **Pros:** No conversion layer; wire bytes go directly into the struct.
- **Cons:** Callers that want to inspect content (token budgeting, pairing-invariant trimming, transcript writing) must pattern-match on `Value::Array` with string field checks — the exact string-keyed dispatch that v5 exists to eliminate.
- **Why not chosen:** Same reason `domain` records are typed structs, not `HashMap<String, Value>`.

### Alternative 3: Defer `ToolUse`/`ToolResult` entirely to 2.1

- **Description:** Ship only `Text` in 2.4; add `ToolUse`/`ToolResult` in 2.1 when the Researcher ships.
- **Pros:** Smaller diff in 2.4.
- **Cons:** 2.1 would need to extend the `Message` type and update every call site again. Two partial migrations is more total work than one complete one. The type design is the same either way; better to define it once.
- **Why not chosen:** The `Message`/`MessageContent` type definition is compile-time-only overhead. Deferring the `AnthropicClient` serialisation of those variants to 2.1 is the right split — not the type definitions.

---

## Technical Considerations

### Dependencies

No new crate dependencies. Changes are additive to existing types.

### Performance

- `Vec<MessageContent>` adds one allocation per message vs. the current `String`. For the Implementer's self-correction sub-loop (typically 1-3 turns), this is unmeasurable.
- History trimming in `build_for_director` / `build_for_researcher` runs in O(turns × content-blocks-per-turn) — linear in history length; not a hotspot.

### Security

- `MessageContent::ToolUse.input: serde_json::Value` carries tool invocation arguments. These must not be emitted to telemetry spans (same rule as `ToolCall.input` in `llm`). Span fields in `complete_free`'s span must not include raw message content beyond previews.

### Testing Strategy

- **Unit:** `MessageContent` serde round-trip per variant.
- **Seam:** `complete_free` integration test with multi-turn history (wiremock); check wire body shape.
- **Context:** `trim_history` unit tests covering boundary conditions.
- **Instrumentation:** `build_for_director` / `build_for_researcher` smoke spans.

### Rollout Plan

Single PR touching three crates. No behaviour change for existing code paths (Implementer, Reviewer, Decomposer). Compile-time enforcement catches any missed migration sites.

---

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| `ToolUse`/`ToolResult` in `send_free_request` produces malformed Anthropic wire body | Low | High | Return `Fatal(NotImplemented)` immediately; integration tests verify the `Text` path; 2.1 adds the full test for tool-use turns. |
| History trimming splits a multi-tool arc (6+ messages), leaving dangling `ToolResult` or leading `Assistant` | Medium | High | Defined as a Logical Turn Arc (dynamic N-message boundary from non-ToolResult User to non-ToolResult User); tests cover 2-message, 4-message, and 6-message arcs; Anthropic returns 400 on violations, surfacing regressions immediately. |
| `AssembledContext.messages` vs. `user_message` confusion in calling code | Low | Low | The field is renamed; any caller using the old field name will not compile. |
| `DirectorState` / `ResearchQuery` structs in `context` leak domain knowledge into a non-domain crate | Low | Low | They carry assembled-prompt data (strings and summaries), not record identities. The crate dependency (`context` does not import `domain`) is enforced at the Cargo level — if we accidentally add a `domain` type to these structs, `cargo check` will catch it. |
| `trim_history` token estimate diverges from actual Anthropic token count, leading to context overflows | Medium | Medium | The `chars / 4` heuristic errs generous (overestimates tokens), so actual counts are typically lower. A real tokenizer is a deferred enhancement; the current heuristic is already in use elsewhere in the codebase. |
| `trim_history` violates Anthropic's alternating-role invariant | Medium | High | Spec requires trimming whole exchange pairs (user+assistant) atomically. Tests pin the invariant. History passed to these builders must already alternate; the caller is responsible for maintaining this before passing history in. |

---

## Open Questions

- [ ] Should `DirectorState` / `ResearchQuery` live in `context` (as proposed) or be defined in `agents` and passed by value? `context` has no `agents` dep; these structs are prompt-assembly inputs that only `context` knows how to render, which is why `context` owns them.
- [ ] `trim_history` budget unit: rough tokens (chars/4) or a hard message count? Starting with tokens (same as token_estimate elsewhere); if the rough estimate causes repeated context overflows in practice, switch to message-count as a simpler ceiling.
- [x] **Closed.** The Director's state summary is the LAST message (current user turn) of every `build_for_director` call. Assembly order: `[trimmed prior history] + [fresh state summary]`. The model responds to the last message in the array; putting the state summary last ensures the Director sees the current world-state, not stale history. Prior history provides conversational context but does not determine the response target.

---

## References

- `docs/deferred-roadmap.md` §2.4 — source stub, keywords, acceptance criteria for this doc
- `docs/design/2026-04-20-llm-client.md` — Stage 6 `LlmClient` trait; "Multi-turn history: Stage 7" non-goal
- `docs/design/2026-04-20-stage-6-scope.md` D11 — `context-builder.md` deferred to Stage 7
- `crates/llm/src/message.rs` — current `ChatMessage` type
- `crates/llm/src/client.rs` — current `complete_free` signature
- `crates/context/src/lib.rs` — current `AssembledContext`, `ContextBuilder` trait
- `crates/agents/src/implementer.rs` — current `Vec<ChatMessage>` usage in self-correction sub-loop
- `~/repos/scottidler/loopr/src/agents/coordinator.rs` — v3 multi-turn coordinator pattern reference
- `~/repos/scottidler/loopr-v4/src/agents/director.rs` — v4 Director's multi-turn shape (state reconciliation across turns)
