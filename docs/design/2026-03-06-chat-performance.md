# Design Document: Chat Performance Optimization

**Author:** Claude (with architectural direction from Gemini)
**Date:** 2026-03-06
**Status:** Implemented
**Review Passes Completed:** 3/5

## Summary

Loopr's Chat mode is functionally correct but painfully slow compared to Claude Code, Codex, and Gemini CLI. A user request like "read all .md files in ~/pd and summarize them" takes 60-225+ seconds across 8-10 LLM round trips. The root causes are: no Anthropic prompt caching (re-processing 100K+ tokens per iteration), the LLM choosing sequential tool strategies instead of bulk delegation, and context bloat from inlining large tool results. This document proposes three targeted fixes that together should reduce total response time by 3-5x.

## Problem Statement

### Background

Chat mode is the interactive analog to Claude Code — the user types a message, the daemon runs an agentic tool loop (LLM → tool calls → LLM → ... → final answer), and text streams to the TUI in real-time via IPC.

The architecture is sound: daemon-side execution provides durability (tmux effect), shared tool sandbox, and session persistence. IPC over Unix domain sockets adds microseconds of latency — imperceptible. SSE streaming from the Anthropic API was added in v0.1.5 so text appears live.

### Problem

Despite correct architecture, Chat feels dead compared to competitors:

1. **8-10 API round trips per user message** — Each iteration pays full TTFT (2-3s for Sonnet) plus generation time. For "summarize 50 files," the LLM makes 25 individual `read` tool calls across multiple iterations instead of one `delegate` call.

2. **No prompt caching** — The system prompt (~500 tokens), tool definitions (~2000 tokens), and all prior messages are re-sent and re-processed on every `complete()` call. By iteration 8 with 50 file contents inlined, the input is 100K+ tokens — all re-tokenized from scratch each time. Anthropic's prompt caching could make ~95% of this a cache hit.

3. **Context bloat** — Tool results (file contents) accumulate in the message history. The auto-compaction threshold is 150K tokens, but even before that, each API call is processing massive input. The `delegate` tool exists to isolate bulk work into a subagent with its own context, but the LLM rarely uses it.

4. **Single model for all roles** — Chat uses the implementer config (`claude-sonnet-4-6`). Sonnet is powerful but slow (2-3s TTFT). Subagent delegation tasks (bulk reads) don't need Sonnet — Haiku is 5-10x faster for mechanical tool execution.

### Goals

- Reduce perceived latency for typical Chat interactions by 3-5x
- Enable prompt caching to eliminate redundant token processing across iterations
- Make the LLM use `delegate` for bulk operations instead of inline tool spam
- Allow different models for Chat (Sonnet) vs delegate subagents (Haiku)

### Non-Goals

- Moving Chat execution out of the daemon (architecture is correct as-is)
- Changing the IPC protocol (not the bottleneck)
- Rewriting the agentic tool loop (it works; just needs better inputs)
- Implementing conversation persistence across daemon restarts (separate concern)

## Proposed Solution

### Overview

Three changes, ordered by impact and independence:

1. **Prompt caching** — Add `cache_control` breakpoints to the API request so Anthropic caches the system prompt and tool definitions. ~10 lines of code, immediate 50-80% reduction in per-iteration processing time.

2. **Configurable chat model + delegate model** — Add `chat` section to config with its own model (default Sonnet) and a `delegate_model` (default Haiku). The delegate tool uses the faster model for mechanical subagent work.

3. **Stronger delegate guidance** — Improve the system prompt to make the LLM reliably use `delegate` for bulk operations, reducing round trips from 8-10 to 2-3.

### Phase 1: Prompt Caching

**What:** Add Anthropic prompt caching headers and `cache_control` markers to the `complete()` API request.

**Where:** `src/agents/llm_client.rs` — the `complete()` method.

**How:**

1. Add `anthropic-beta: prompt-caching-2024-07-31` header to the HTTP request.

2. Change the `system` field from a plain string to an array of content blocks with a cache breakpoint:
```json
"system": [
  {
    "type": "text",
    "text": "<system prompt>",
    "cache_control": {"type": "ephemeral"}
  }
]
```

3. Add `cache_control` to the last tool definition:
```json
"tools": [
  { "name": "read", ... },
  ...
  { "name": "delegate", ..., "cache_control": {"type": "ephemeral"} }
]
```

This creates two cache breakpoints. On iteration 2+, Anthropic serves the system prompt and tool definitions from cache (~2500 tokens saved from re-processing). On multi-turn conversations, early messages also get cached automatically by Anthropic's prefix matching.

**Expected impact:** 50-80% reduction in TTFT for iteration 2+ (Anthropic charges 90% less for cached input tokens and processes them ~5x faster).

### Phase 2: Configurable Chat and Delegate Models

**What:** Add a `chat` config section with `model` and `delegate_model` fields. The Chat session uses `model` (Sonnet by default), and the `delegate` tool overrides its child LLM client with `delegate_model` (Haiku by default).

**Where:**
- `src/config.rs` — add `ChatConfig` struct
- `src/daemon/handlers.rs` — `handle_chat_submit` uses `ChatConfig` instead of implementer config
- `src/tools/builtin/delegate.rs` — accept optional model override for child LLM

**Config schema:**
```yaml
chat:
  model: "claude-sonnet-4-6"           # Parent chat model
  delegate_model: "claude-haiku-4-5-20251001"  # Subagent model (fast)
  max_tokens: 8192
  temperature: 0.3
  max_iterations: 10
```

**How:**
1. Add `ChatConfig` to config with defaults
2. `handle_chat_submit` creates `AgentLlmClient` from `config.chat` instead of `config.agents.implementer`
3. `DelegateTool::new()` accepts an optional `delegate_model` override
4. When delegate spawns a subagent, it creates a new `AgentLlmClient` with the delegate model

**Expected impact:** Haiku TTFT is ~300ms vs Sonnet's 2-3s. Haiku generates 3-5x faster. For bulk read+summarize, the delegate subagent completes in ~30s instead of 225s.

### Phase 3: Stronger Delegate Guidance

**What:** Improve the Chat system prompt to make the LLM reliably delegate bulk operations.

**Where:** `src/domain/chat.rs` — `CHAT_SYSTEM_PROMPT`

**How:** Add explicit guidance:
```
IMPORTANT: For tasks involving more than 3 files or bulk operations
(reading, searching, summarizing across many files), ALWAYS use the
`delegate` tool. Do NOT call read/grep/glob repeatedly yourself.
The delegate subagent handles bulk work in its own context window,
keeping your conversation clean and fast.
```

**Expected impact:** Reduces round trips from 8-10 to 2-3 for bulk operations. The parent Chat makes one delegate call, receives a summary, and responds.

### Implementation Plan

1. **Phase 1: Prompt Caching** — Isolated to `llm_client.rs`. No config changes. Can ship immediately.
2. **Phase 2: Chat Config + Delegate Model** — Config + handler + delegate changes. Ship after Phase 1.
3. **Phase 3: Prompt Tuning** — One-line system prompt change. Ship alongside Phase 2.

## Alternatives Considered

### Alternative 1: Move Chat to TUI (bypass daemon/IPC)
- **Description:** Run the agentic loop directly in the TUI process, eliminating IPC.
- **Pros:** Simplest possible token-to-screen path.
- **Cons:** Destroys durability (closing TUI kills the LLM mid-thought). Duplicates tool sandbox. Fractures state management. Cannot share sessions between TUI instances.
- **Why not chosen:** IPC adds microseconds; the API takes seconds. Architectural cost far exceeds negligible latency benefit.

### Alternative 2: Switch to a single-shot streaming call (no tool loop)
- **Description:** Make one API call with extended thinking, let the LLM do everything in one shot.
- **Pros:** One round trip. Maximum streaming. How Claude Code works.
- **Cons:** Requires Anthropic's extended thinking (not available for all models). No tool execution. Fundamentally different architecture. Loses the ability to read files, run commands, etc.
- **Why not chosen:** Tool use is essential for Chat functionality. Extended thinking is complementary, not a replacement.

### Alternative 3: Pre-warm tool results
- **Description:** Before calling the LLM, pre-read likely files (based on user message keywords) and include them in the first message.
- **Pros:** Reduces round trips for common patterns.
- **Cons:** Speculative. May include irrelevant data. Hard to generalize. Increases first-call input size.
- **Why not chosen:** Prompt caching + delegate is more general and reliable.

## Technical Considerations

### Dependencies

- **Anthropic prompt caching API** — Requires `anthropic-beta: prompt-caching-2024-07-31` header. Documented at docs.anthropic.com. Ephemeral cache entries last 5 minutes (extended on hit).
- **Haiku model availability** — `claude-haiku-4-5-20251001` must be available on the user's API key.

### Performance

**Current baseline (session 20260306T024411):**
- TTFT: 3s (iteration 1)
- Total time for "summarize 50 files": 43s (8 iterations)
- Delegate subagent: 225s

**Expected after all phases:**
- TTFT: 2-3s (iteration 1, uncached), ~500ms (iteration 2+, cached)
- Total time for "summarize 50 files": 15-20s (2-3 iterations with delegate)
- Delegate subagent: 30-60s (Haiku)

### Security

No new security implications. Prompt caching is server-side at Anthropic. Model configuration uses the same API key.

### Testing Strategy

- **Unit tests:** Verify cache_control headers are present in API request body (mock HTTP client)
- **Unit tests:** Verify ChatConfig parsing and defaults
- **Integration test:** Chat session with delegate call uses correct model
- **Manual benchmark:** Time the "read all .md files in ~/pd and summarize" task before and after

### Rollout Plan

Each phase ships as its own version bump. Phase 1 is zero-config and backward compatible. Phase 2 adds optional config (defaults match current behavior). Phase 3 is a prompt change with no code impact.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Prompt caching not effective (cache misses) | Low | Medium | Ephemeral cache has 5-min TTL; multi-turn conversations naturally extend it. Monitor cache hit rates in logs. |
| Haiku too weak for complex delegate tasks | Medium | Medium | Default to Sonnet for delegate; make model configurable. User can override in loopr.yml. |
| LLM still ignores delegate guidance | Medium | High | Iterate on system prompt. Consider tool-forcing (require delegate for 5+ file operations). |
| Prompt caching API changes | Low | Low | Pin anthropic-beta header version. Update when needed. |

## Open Questions

- [ ] Should delegate subagent events stream to TUI? Currently `event_tx: None` — the user sees nothing while delegate runs for 30+ seconds.
- [ ] Should we add a `chat.max_context_tokens` config to control when compaction kicks in, separate from the agent default?
- [ ] Should the Chat config support per-funnel-state model selection (e.g., Haiku for Chat, Sonnet for Interview/PlanDraft)?

## References

- [Anthropic Prompt Caching docs](https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching)
- Gemini architectural evaluation (conversation context, 2026-03-06)
- Session logs: `20260306T015133`, `20260306T020106`, `20260306T022423`, `20260306T023437`, `20260306T024411`
- Existing design docs: `docs/design/2026-02-26-implementer-reviewer-agents.md` (agent architecture)
