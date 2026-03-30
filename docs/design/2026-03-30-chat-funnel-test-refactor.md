# Design Document: Chat Funnel Test Refactor

**Author:** Scott Idler
**Date:** 2026-03-30
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Replace the legacy Coordinator Interviewing FSM persona tests in `tests/funnel.rs` with a test suite that exercises the actual user flow: TUI chat funnel -> `chat.submit` IPC -> interview/draft/accept -> `coordinator.accept_plan` -> Coordinator starts. The new suite separates deterministic bridge tests from single-shot LLM smoke tests and adds unit tests for the prompt machinery.

## Problem Statement

### Background

The chat funnel is fully implemented: `FunnelState` enum (`Chat`, `Interview`, `PlanDraft`, `Executing`), slash commands (`/plan`, `/draft`, `/accept`), `system_prompt_for_chat` with per-state `.pmt` augmentation, `chat.submit` handler with agentic tool loop, and the `coordinator.accept_plan` bridge that creates a Plan, activates it, creates a CoordinatorGoal, and starts the Coordinator agent.

`tests/funnel.rs` was written to test the previous architecture: a daemon-side Coordinator FSM with an `Interviewing` state that drove questions over IPC via `coordinator.interview_question` events and `coordinator.interview_respond` responses. That path was reverted (commits ddf7de0, 6c8f1f4, eaf162e) because the design doc moved the interview to the TUI-side chat funnel instead.

### Problem

The tests in `tests/funnel.rs` drive the wrong path. They:

1. Call `coordinator.set_goal` + `agent.start` to enter the daemon-side Interviewing FSM
2. Listen for `coordinator.interview_question` events
3. Respond with `coordinator.interview_respond`
4. Wait for `record.created` (collection: "plan")

None of this exercises the actual user flow: `chat.submit` with `funnel_state: "interview"` -> LLM asks questions -> user answers via `chat.submit` -> `/draft` -> LLM drafts plan -> `/accept` -> `coordinator.accept_plan`.

The IPC methods `coordinator.interview_question` and `coordinator.interview_respond` still exist as event-forwarding stubs (handlers.rs:4241), but the Coordinator FSM no longer enters an `Interviewing` state after the revert. The `run_persona()` driver would hang indefinitely waiting for `coordinator.interview_question` events that the Coordinator never emits.

### Goals

- Test the `coordinator.accept_plan` bridge deterministically (no LLM) - Plan creation, activation, CoordinatorGoal creation, Coordinator agent startup
- Test `system_prompt_for_chat` produces correct prompt augmentation for each `FunnelState`
- Smoke-test `chat.submit` with a real LLM to verify the interview and draft prompts steer behavior correctly
- Extract reusable test infrastructure (DaemonHandle, TempTestDir) into a shared module
- Keep the existing fast structural tests (persona fixture validation, keyword matching)

### Non-Goals

- Multi-turn scripted conversation testing against a real LLM (inherently flaky, anti-pattern)
- Testing the full end-to-end flow from `chat.submit` through Coordinator execution to Implementer output
- Testing TUI input handling or slash command parsing (those are TUI-layer concerns)
- Replacing the Tier 2 LLM evaluator infrastructure (it's reusable as-is for future nightly checks)

## Proposed Solution

### Overview

Three test files, one shared harness:

| File | Purpose | LLM? | Speed |
|------|---------|------|-------|
| `tests/common/mod.rs` | Shared DaemonHandle, TempTestDir, helpers | No | N/A |
| `tests/bridge.rs` | `coordinator.accept_plan` -> Coordinator starts | No | Fast (<2s) |
| `tests/chat_prompts.rs` | Prompt unit tests + single-shot LLM smoke tests | Yes (smoke only) | Medium (1 LLM call each) |
| `tests/funnel.rs` | Persona fixtures + fast structural tests (trimmed) | No | Fast (<1s) |

### Architecture

#### 1. Shared Test Harness: `tests/common/mod.rs`

Extract from `tests/funnel.rs`:

- **`TempTestDir`** - RAII temp directory (already implemented, move as-is)
- **`DaemonHandle`** - in-process daemon with isolated socket/temp dir (already implemented, move as-is)
- **`test_id()`** - unique ID generator (already implemented, move as-is)

These are already battle-tested in the current funnel.rs. No logic changes needed.

#### 2. Bridge Tests: `tests/bridge.rs`

Deterministic integration tests for the `coordinator.accept_plan` handler via IPC. No LLM calls.

**Test cases:**

| Test | What it does | Asserts |
|------|-------------|---------|
| `test_bridge_accept_plan_creates_plan` | Call `accept_plan` with hardcoded plan text | Plan record exists in TaskStore, status is Active, title extracted correctly |
| `test_bridge_accept_plan_creates_goal` | Call `accept_plan` with plan text | CoordinatorGoal record exists, is Active, references plan |
| `test_bridge_accept_plan_starts_coordinator` | Call `accept_plan`, listen for agent status event | `agent.status_changed` event with status `Starting` emitted (this is synchronous in `agent.start`, before the async task spawns). The Coordinator task may subsequently fail (no API key) - that's fine; we're testing the bridge wiring, not the Coordinator itself. |
| `test_bridge_accept_plan_with_existing_plan` | Create a Plan via `plan.create`, then `accept_plan` with `plan_id` | Plan activated, goal created, coordinator starts |
| `test_bridge_accept_plan_deactivates_prior_goal` | Set a goal, accept a new plan | Previous CoordinatorGoal is deactivated, new one is active |

These tests spin up a real daemon via `DaemonHandle`, connect via `IpcClient`, and verify state via IPC queries (`plan.get`, `coordinator_goal.list`, event stream).

**Why not just use the existing handler unit tests?** The handler unit tests in handlers.rs:13188 use `dispatch()` directly in a sync `#[test]` context. Inside `accept_plan`, line 4205 checks `tokio::runtime::Handle::try_current()` - in sync tests this returns `Err`, so the handler skips Coordinator auto-start. The bridge integration tests use `#[tokio::test]`, giving a real async runtime, so `accept_plan` will actually call `dispatch("agent.start", ...)` and spawn the Coordinator. This is the critical gap the bridge tests fill.

#### 3. Chat Prompt Tests: `tests/chat_prompts.rs`

Two categories:

**A. Unit tests (deterministic, no LLM):**

| Test | Asserts |
|------|---------|
| `test_prompt_chat_base` | `system_prompt_for_chat(Chat, false, None)` returns non-empty string containing base chat prompt content |
| `test_prompt_interview_augmented` | `system_prompt_for_chat(Interview, false, None)` contains interview-specific text ("clarifying questions") |
| `test_prompt_draft_augmented` | `system_prompt_for_chat(PlanDraft, true, None)` contains draft-specific text ("structured plan") |
| `test_prompt_refine_augmented` | `system_prompt_for_chat(PlanDraft, false, None)` contains refine-specific text |
| `test_prompt_executing_includes_status` | `system_prompt_for_chat(Executing, false, Some("...status..."))` contains the status text |

**B. Single-shot LLM smoke tests (`#[ignore]`, require ANTHROPIC_API_KEY):**

| Test | Input | Asserts |
|------|-------|---------|
| `test_chat_interview_asks_questions` | `chat.submit` with message "Build a web app", `funnel_state: "interview"` | Response does NOT contain code fences (` ``` `); response contains at least one `?` |
| `test_chat_draft_produces_structure` | `chat.submit` with interview-like history + `is_draft_request: true` | Response contains a markdown heading (`#`) or numbered list; response contains "acceptance criteria" or "deliverables" (case-insensitive) |

These are single-shot - one LLM call each. No multi-turn scripted conversations. Each test spins up a `DaemonHandle`, sends one `chat.submit` IPC request, waits for the `agent.llm_output` event stream to complete (`is_final: true`), then reads the final assistant message from `chat.history` for assertions.

#### 4. Rewritten `tests/funnel.rs`

Keep:
- `PersonaFixture` struct and all 5 fixtures (GOLDEN_PATH, VAGUE_USER, etc.)
- `EvalResult` and `assert_plan_tier2()` - reusable for nightly evaluation
- `assert_plan_tier1()` - reusable structural assertion
- All 4 fast structural tests at the bottom

Remove:
- `TempTestDir` (moved to common)
- `DaemonHandle` (moved to common)
- `test_id()` (moved to common)
- `run_persona()` - drives the dead IPC interview path (would hang waiting for events the Coordinator no longer emits)

Replace:
- `run_persona()` -> `run_chat_persona()` - new driver that uses `chat.submit` instead of `coordinator.interview_question`/`interview_respond`
- All 5 `test_persona_*` async tests rewritten to call `run_chat_persona()` instead of `run_persona()`

**`run_chat_persona()` driver flow:**

```
1. chat.submit(message=fixture.initial_goal, funnel_state="interview")
   -> wait for agent.llm_output(is_final=true)
   -> read assistant response from chat.history

2. For each fixture.responses[i]:
   chat.submit(message=responses[i], funnel_state="interview")
   -> wait for agent.llm_output(is_final=true)

3. chat.submit(message="Please draft a plan based on our discussion.",
               funnel_state="plan_draft", is_draft_request=true)
   -> wait for agent.llm_output(is_final=true)
   -> extract plan text from last assistant message

4. coordinator.accept_plan(plan=plan_text)
   -> receive plan_id from response

5. assert_plan_tier1(plan_id, fixture)
6. assert_plan_tier2(plan_text, fixture)  // nightly only
```

The critical difference from the old `run_persona()`: state transitions are **driver-controlled** via the `funnel_state` parameter, not LLM-controlled via FSM events. The LLM responds within whatever state it's told. This makes the flow deterministic - flakiness is limited to assertion quality (do plan keywords match?), not flow completion.

The driver waits for completion using the event stream: `agent.llm_output` events with `is_final: true` signal that the agentic tool loop has finished for that `chat.submit` call. Between calls, the driver reads the updated chat history via `chat.history` IPC to extract the assistant's response.

These tests remain `#[ignore]` (require ANTHROPIC_API_KEY, 3-10 LLM calls each).

### Implementation Plan

**Phase 1: Extract shared harness**
- Create `tests/common/mod.rs`
- Move `TempTestDir`, `DaemonHandle`, `test_id()` from `tests/funnel.rs`
- Update `tests/funnel.rs` to use `mod common;` import
- Verify `otto ci` passes

**Phase 2: Bridge tests**
- Create `tests/bridge.rs`
- Implement the 5 bridge test cases
- Verify all pass without ANTHROPIC_API_KEY

**Phase 3: Chat prompt tests**
- Create `tests/chat_prompts.rs`
- Implement the 5 unit tests for `system_prompt_for_chat`
- Implement the 2 `#[ignore]` LLM smoke tests
- Verify unit tests pass in CI, smoke tests pass manually

**Phase 4: Rewrite funnel.rs persona driver**
- Remove `run_persona()` and the moved infrastructure (TempTestDir, DaemonHandle, test_id)
- Implement `run_chat_persona()` using `chat.submit` IPC with driver-controlled `funnel_state` transitions
- Rewrite all 5 `test_persona_*` tests to use `run_chat_persona()`
- Keep fixtures, tier assertions, and fast structural tests
- Verify `otto ci` passes (fast tests), then `cargo test --test funnel -- --ignored` manually

## Alternatives Considered

### Alternative 1: Delete tests/funnel.rs entirely, start fresh
- **Description:** Nuke funnel.rs. Write bridge.rs and chat_prompts.rs from scratch.
- **Pros:** Clean slate, no legacy baggage
- **Cons:** Loses PersonaFixture definitions, Tier 2 evaluator, structural assertions - all reusable. Duplicates TempTestDir/DaemonHandle work.
- **Why not chosen:** The infrastructure is good; only the driver (`run_persona`) targets the wrong path. Refactor > rewrite.

### Alternative 2: Multi-turn chat.submit personas as CI tests (not #[ignore])
- **Description:** Same as the chosen approach for `run_chat_persona()`, but run in CI instead of as `#[ignore]` tests.
- **Pros:** Full CI coverage of the conversational funnel.
- **Cons:** Each persona test makes 3-10 LLM calls. Non-deterministic assertion results (keyword matching against LLM output). Slow (30-120s per persona). Requires ANTHROPIC_API_KEY in CI.
- **Why not chosen:** The bridge tests (deterministic) cover the critical handoff in CI. The persona tests provide valuable end-to-end coverage but belong in manual/nightly runs where flakiness and cost are acceptable. They remain `#[ignore]`.

### Alternative 3: Pure unit tests only (mock everything)
- **Description:** Test `system_prompt_for_chat` output, TUI state transitions, and handler logic with no daemon, no IPC, no LLM.
- **Pros:** Fast, deterministic, no API keys needed.
- **Cons:** Doesn't test that `accept_plan` actually starts the Coordinator in a full async context (the handler unit tests run sync, skipping auto-start). Doesn't test that the prompts actually work against a real LLM.
- **Why not chosen:** Misses the critical integration point (accept_plan -> Coordinator starts) and provides zero confidence that prompts steer the LLM correctly.

## Technical Considerations

### Dependencies

- **Internal:** `loopr::ipc::client::IpcClient`, `loopr::daemon::context::DaemonContext`, `loopr::domain::chat::{FunnelState, system_prompt_for_chat}`, `loopr::prompts::init_defaults()`
- **External:** `tokio`, `serde_json`, `eyre`, `reqwest` (for Tier 2 evaluator only)

### Testing Strategy

**CI (`otto ci` / `cargo test`):**
- All bridge tests run (deterministic, no LLM)
- All prompt unit tests run (deterministic, no LLM)
- All funnel.rs structural tests run (deterministic, no LLM)
- LLM smoke tests are `#[ignore]` - skipped in CI

**Manual / nightly:**
- `cargo test --test chat_prompts -- --ignored` - runs LLM smoke tests
- `LOOPR_TIER2_EVAL=1 cargo test -- --ignored` - runs Tier 2 evaluator

### Performance

- Bridge tests: <2s each (daemon startup + IPC round-trips, no LLM)
- Prompt unit tests: <100ms each (string manipulation only)
- LLM smoke tests: 5-15s each (single API call)

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Bridge test can't verify Coordinator actually starts (async timing) | Low | Medium | `agent.start` emits `AgentStatus::Starting` synchronously before `tokio::spawn`. The bridge test asserts on this event, not on subsequent async execution. No timing race. |
| Coordinator task fails after Starting (no API key in bridge tests) | High | Low | Expected and harmless. The bridge tests verify the wiring (accept_plan -> agent.start dispatched), not the Coordinator's LLM execution. The task will fail with an LLM client error, which is fine. |
| LLM smoke test assertions too loose (always pass) | Low | Medium | Negative assertions (no code fences in interview) are harder to false-positive than positive ones |
| LLM smoke test assertions too tight (flaky) | Medium | Medium | Keep to structural markers (headings, question marks), not content; `#[ignore]` keeps them out of CI |
| chat.submit persona driver hangs (LLM never finishes) | Low | Medium | Timeout per `chat.submit` call (configurable via LOOPR_TEST_TIMEOUT_SECS). The `agent.llm_output(is_final=true)` event is emitted by the handler on both success and error paths. |
| Persona tier1 keywords don't appear in chat-drafted plans | Medium | Low | The chat-draft.pmt prompt instructs the LLM to produce a structured plan from the conversation. Keyword presence depends on the LLM absorbing the persona's answers. If flaky, loosen required_keywords for chat-driven tests or add a retry count. These are `#[ignore]` tests - occasional flakiness is acceptable. |
| Shared `tests/common/mod.rs` causes compilation coupling | Low | Low | Cargo compiles each integration test as a separate binary; `mod common;` is standard Cargo convention. The common module is not compiled as its own test binary because it has no `#[test]` functions - Cargo only compiles `tests/*.rs` files as binaries, not subdirectories. |

## Open Questions

- [ ] Should the bridge test verify Coordinator reaches a specific FSM state (e.g., `Planning`), or just that it starts (`agent.status_changed` to `Starting`)? Recommendation: `Starting` only - the Coordinator task will fail without an API key, so it won't reach `Planning`. Testing FSM progression is the Coordinator's own concern.
- [x] ~~Should the persona fixtures move to `tests/common/` alongside the harness, or stay in `tests/funnel.rs`?~~ Stay in funnel.rs - they're used by the rewritten `test_persona_*` tests alongside `run_chat_persona()`, `assert_plan_tier1`, and `assert_plan_tier2`.
- [x] ~~Is there value in a separate chat E2E test file?~~ No - the rewritten persona tests in funnel.rs serve this purpose. The single-shot smoke tests in chat_prompts.rs cover prompt mechanics.

## References

- `docs/design/2026-03-04-tui-chat-plan-funnel.md` - TUI chat plan funnel design
- `docs/design/2026-03-30-revert-coordinator-interviewing.md` - Why the Interviewing FSM was reverted
- `tests/funnel.rs` - Current test file being refactored
- `src/daemon/handlers.rs:4085-4239` - `coordinator.accept_plan` handler
- `src/daemon/handlers.rs:4883-5082` - `chat.submit` handler
- `src/domain/chat.rs` - `FunnelState`, `ChatHistory`, `system_prompt_for_chat`
