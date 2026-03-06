# Design Document: Chat Timing Instrumentation

**Author:** Claude
**Date:** 2026-03-06
**Status:** Draft
**Review Passes Completed:** 5/5

## Summary

Add `std::time::Instant`-based timing to the Chat hot path to identify exactly where time is spent during agentic tool loops. Surface timings via: (1) `log::info!` lines in the daemon log file, (2) structured `DaemonEvent`s that the TUI can display inline, and (3) a per-session timing summary emitted when the loop completes.

## Problem Statement

### Background

Chat performance was improved with prompt caching, delegate model splitting, SSE batching, and render throttling. But we're still flying blind — we don't know the actual TTFT, per-iteration time, tool execution vs LLM wait breakdown, or where compaction kicks in. The only timing visible today is `duration_ms` on individual tool calls (shown in TUI as `✓ tool: delegate (46179ms)`).

### Problem

Without timing data we can't:
- Tell if prompt caching is actually hitting (TTFT iteration 1 vs 2+)
- See how much time is LLM wait vs tool execution vs overhead
- Identify which iterations are slow and why
- Know when compaction fires and how much it costs
- Compare before/after when making changes

### Goals

- Instrument the 5 hot paths with `Instant` timing
- Surface timings in daemon logs at `info` level (always visible)
- Emit timing events to TUI for inline display during streaming
- Zero new dependencies — `std::time::Instant` only
- Negligible overhead (<1ms total for all instrumentation)

### Non-Goals

- Full distributed tracing (`tracing` crate migration)
- Metrics collection / dashboards / Prometheus export
- Timing the TUI render loop (already throttled to 30fps)
- Historical timing storage or analytics

## Proposed Solution

### Overview

Wrap 5 critical sections with `Instant::now()` / `elapsed()` and emit the results through two channels: `log::info!` (for the daemon log file) and `DaemonEvent` (for TUI display).

### Instrumentation Points

| # | What | Where | Measures |
|---|------|-------|----------|
| 1 | **LLM complete() call** | `llm_client.rs:complete()` | Total API call time, TTFT (time to first SSE chunk) |
| 2 | **Agentic loop iteration** | `agentic_loop.rs:run_tool_loop()` | Per-iteration wall time (LLM + tools + overhead) |
| 3 | **Agentic loop total** | `agentic_loop.rs:run_tool_loop()` | Total loop time, iteration count |
| 4 | **Auto-compaction** | `agentic_loop.rs:auto_compact()` | Compaction time, token reduction |
| 5 | **Tool batch execution** | `agentic_loop.rs` (tool results section) | Batch wall time (already has per-tool timing) |

### Phase 1: LLM Timing (TTFT + total)

**Where:** `src/agents/llm_client.rs` — `complete()` method and `read_sse_content_blocks()`

**What:**
- Record `Instant::now()` before the HTTP request
- Record first-chunk time when the first `content_block_delta` arrives in `read_sse_content_blocks()`
- Log TTFT and total time when the method returns

```rust
// In complete():
let call_start = Instant::now();

// ... HTTP request + stream ...

let (content_blocks, stop_reason) = self.read_sse_content_blocks(response).await?;
let total_ms = call_start.elapsed().as_millis();

info!(
    "[timing:{}] complete: total={}ms blocks={}",
    self.session_id, total_ms, content_blocks.len()
);
```

```rust
// In read_sse_content_blocks(), capture TTFT on first content delta:
let mut first_chunk_time: Option<Duration> = None;

// Inside content_block_delta handler:
if first_chunk_time.is_none() {
    first_chunk_time = Some(call_start.elapsed());
}
```

Rather than passing `call_start` into `read_sse_content_blocks()`, change the return type to include TTFT:

```rust
async fn read_sse_content_blocks(
    &self,
    response: reqwest::Response,
) -> Result<(Vec<ContentBlock>, Option<StopReason>, Option<Duration>)>
//                                                  ^^^^^^^^^^^^^^^^ TTFT
```

The caller (`complete()`) owns `call_start` and computes `total_ms = call_start.elapsed()`. TTFT is returned from the stream reader which records it on the first `content_block_delta` event using its own `Instant::now()` captured at stream start — this is close enough since the HTTP response has already arrived.

### Phase 2: Iteration + Loop Timing

**Where:** `src/tools/agentic_loop.rs` — `run_tool_loop()`

**What:**
- Wrap each iteration with `Instant` timing
- Track LLM time vs tool time separately within each iteration
- Log a summary when the loop completes

```rust
let loop_start = Instant::now();

for iteration in 0..max_iterations {
    let iter_start = Instant::now();

    // LLM call
    let llm_start = Instant::now();
    let (content_blocks, stop_reason) = llm.complete(...).await?;
    let llm_ms = llm_start.elapsed().as_millis();

    // ... tool execution (already timed per-tool) ...
    let tools_start = Instant::now();
    let results = futures::future::join_all(futures).await;
    let tools_ms = tools_start.elapsed().as_millis();

    let iter_ms = iter_start.elapsed().as_millis();
    info!(
        "[timing:{}] iteration {}: total={}ms llm={}ms tools={}ms tool_count={}",
        ctx.exec_id, iteration, iter_ms, llm_ms, tools_ms, tool_uses.len()
    );
}

// Track actual iteration count (loop may exit early via return)
let iteration_count = iteration + 1;

let loop_ms = loop_start.elapsed().as_millis();
info!(
    "[timing:{}] loop_complete: total={}ms iterations={} tool_calls={}",
    ctx.exec_id, loop_ms, iteration_count, total_tool_calls
);

// Note: the early-return path (no tool calls / end_turn) also needs
// the loop summary. Emit timing in both the early-return and the
// max-iterations-reached exit paths.
```

For the early-return path inside the loop:
```rust
if tool_uses.is_empty() || stop_reason != Some(StopReason::ToolUse) {
    let loop_ms = loop_start.elapsed().as_millis();
    info!(
        "[timing:{}] loop_complete: total={}ms iterations={} tool_calls={}",
        ctx.exec_id, loop_ms, iteration + 1, total_tool_calls
    );
    // ... return AgenticResult
}
```

### Phase 3: TUI Timing Events

**Where:** `src/ipc/protocol.rs` (new event), `src/agents/mod.rs` (AgentEvent variant), `src/tui/run.rs` (handler)

**What:** Emit a lightweight `DaemonEvent` with timing data. The TUI renders it inline as a dim system-style line, reusing the existing `extract_tool_event` → `ChatMessage` pattern (no new widget code needed).

Add one new `AgentEvent` variant:
```rust
AgentEvent::TimingInfo {
    session_id: String,
    label: String,    // e.g. "iter 0", "loop_complete", "complete"
    detail: String,   // e.g. "total=3204ms llm=2891ms tools=298ms"
}
```

One new `DaemonEvent` constructor:
```rust
DaemonEvent::agent_timing_info(session_id, label, detail)
```

TUI handler in `extract_tool_event` (or a sibling `extract_timing_event`):
```rust
"agent.timing_info" => {
    // Render as dim ChatMessage
    ChatMessage {
        role: ChatRole::ToolInvocation,  // reuse dim italic style
        content: format!("⏱ {label}: {detail}"),
    }
}
```

Display in chat:
```
⟳ tool: delegate
✓ tool: delegate (46179ms)
  ⏱ iter 0: total=3204ms llm=2891ms tools=298ms
  ⏱ loop_complete: total=4360ms iterations=2 tool_calls=3
```

### Phase 4: Compaction Timing

**Where:** `src/tools/agentic_loop.rs` — `auto_compact()`

**What:**
```rust
let compact_start = Instant::now();
// ... compaction logic ...
info!(
    "[timing:{}] auto_compact: {}ms tokens_before={} tokens_after={}",
    exec_id, compact_start.elapsed().as_millis(), before, after
);
```

### Implementation Plan

1. **Phase 1: LLM TTFT + total** — `llm_client.rs`. Pass `Instant` into `read_sse_content_blocks`. Log TTFT and total.
2. **Phase 2: Iteration timing** — `agentic_loop.rs`. Wrap iteration body. Log per-iteration breakdown.
3. **Phase 3: TUI display** — New `DaemonEvent` types. Inline timing in chat view.
4. **Phase 4: Compaction timing** — `agentic_loop.rs`. Log compaction cost.

## Alternatives Considered

### Alternative 1: `tracing` crate migration
- **Description:** Replace `log` with `tracing`, use `#[instrument]` for automatic span timing.
- **Pros:** Structured, composable, async-aware. Industry standard. Gets us histograms, span trees, flamegraphs.
- **Cons:** Touches every file in the codebase. Migration is 100+ `use` changes. New subscriber setup. `tracing-subscriber` adds compile time. Overkill for "where is the time going?"
- **Why not chosen:** We need answers now, not an observability platform. Can migrate to `tracing` later if warranted.

### Alternative 2: `metrics` crate
- **Description:** Counters and histograms with Prometheus export.
- **Pros:** Good for production monitoring.
- **Cons:** New dependency. Requires exporter setup. Dashboards. We're a TUI app, not a web service.
- **Why not chosen:** Wrong tool for the job.

### Alternative 3: Log-only (no TUI display)
- **Description:** Just add `info!` timing to the log file, don't emit events to TUI.
- **Pros:** Simplest. No protocol changes.
- **Cons:** User has to tail the log file to see timing. Defeats the purpose — we want visibility while using Chat.
- **Why not chosen:** Phase 3 (TUI display) is small incremental effort for huge usability win.

## Technical Considerations

### Dependencies

None. `std::time::Instant` is in the standard library. Already imported in `agentic_loop.rs`.

### Performance

`Instant::now()` is a single syscall (`clock_gettime`) — ~25ns on Linux. With 10 instrumentation points per iteration and 10 iterations, total overhead is ~2.5μs. Negligible compared to LLM calls (seconds).

### Testing Strategy

- **Unit tests:** Verify timing log lines are emitted (check log output contains `[timing:`)
- **Unit tests:** Verify new `DaemonEvent` types serialize/deserialize correctly
- **Unit tests:** Verify TUI renders timing lines without panic
- **Manual validation:** Run Chat, check daemon log for timing lines, verify TUI shows timing inline

### Rollout Plan

All phases ship in one commit. Zero-config, backward compatible. Timing is always emitted at `info` level — visible when `RUST_LOG=info` or via the default daemon log.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Log spam from timing lines | Low | Low | Use `info` level (not `debug`). One line per iteration, not per chunk. |
| TTFT measurement inaccurate (includes HTTP overhead) | Low | Low | Measure from before `send()` — this is what the user feels. |
| TUI timing display clutters chat | Medium | Low | Use dim styling. Can add config to disable later if needed. |

### Log Format Convention

All timing log lines use a consistent prefix for easy grep:

```
[timing:<session_id>] <event>: <key>=<value> ...
```

Examples from a real session:
```
[timing:default-chat] complete: total=2891ms ttft=487ms blocks=3
[timing:default-chat] iteration 0: total=3204ms llm=2891ms tools=298ms tool_count=1
[timing:default-chat] iteration 1: total=1156ms llm=834ms tools=312ms tool_count=2
[timing:default-chat] loop_complete: total=4360ms iterations=2 tool_calls=3
[timing:default-chat] auto_compact: 1234ms tokens_before=95000 tokens_after=12000
```

To see all timing from a session: `grep '\[timing:' ~/.local/share/loopr/daemon.log`

## Open Questions

- [ ] Should timing be gated behind a config flag or always-on? (Recommendation: always-on. The overhead is negligible and the data is invaluable.)
- [ ] Should we emit TTFT separately as its own event, or bundle it into the complete() log line? (Recommendation: bundle — one line per complete() call with both TTFT and total.)

## References

- `src/agents/llm_client.rs` — LLM HTTP + SSE streaming
- `src/tools/agentic_loop.rs` — agentic tool loop (already has per-tool timing)
- `src/ipc/protocol.rs` — DaemonEvent definitions
- `docs/design/2026-03-06-chat-performance.md` — parent performance design doc
