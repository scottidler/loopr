# Design Document: Interview Context Injection

**Author:** Scott Idler
**Date:** 2026-03-30
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

The Coordinator's `Interviewing` FSM state builds a static prompt that never includes prior interview exchanges. The LLM never sees user answers, causing it to repeat the same questions indefinitely. This document designs the fix: inject accumulated `interview_context` into the FSM footer prompt and fix the data capture gap where questions aren't recorded in `InterviewExchange`.

## Problem Statement

### Background

The Coordinator agent operates as an FSM. In the `Interviewing` state, it emits `InterviewQuestion` actions with clarifying questions. The daemon broadcasts these to the TUI. The user responds via `coordinator.interview_respond` IPC, which appends an `InterviewExchange` to `CoordinatorState.interview_context`. On the next iteration, the Coordinator should see the accumulated Q&A history and either ask follow-up questions or emit `ProposePlan`.

### Problem

Two bugs break this flow:

1. **Deaf Coordinator**: `build_fsm_footer` for `Interviewing` returns a hard-coded string that never references `coord_state.interview_context`. The LLM prompt contains zero history of prior exchanges. The Coordinator re-asks the same questions indefinitely, hitting the test timeout.

2. **Lost questions**: `handle_coordinator_interview_respond` creates `InterviewExchange { questions: vec![], answer, timestamp }` - the questions field is always empty. Even if we inject interview context into the prompt, we'd only see answers, not the questions that prompted them.

3. **Fragile key parsing (minor)**: The LLM sometimes emits `"question": "..."` (singular) instead of `"questions": [...]`. The `string_or_vec` deserializer handles the value type mismatch (string vs array), but not the key name mismatch. A serde `alias = "question"` would catch this.

### Goals

- Coordinator sees full interview history (questions AND answers) in every iteration prompt
- Questions are captured alongside answers in `InterviewExchange`
- Test `test_persona_golden_path` progresses past the `Interviewing` state

### Non-Goals

- Changing the interview flow architecture (IPC, event broadcasting)
- Token budget management for long interview histories (future concern)
- Changing how `InterviewMode::Auto` synthesizes answers

## Proposed Solution

### Overview

Four surgical changes across 5 files, no new crates:

1. **Data capture**: Record questions in `InterviewExchange` via `pending_questions` on `CoordinatorState`
2. **Formatter**: Add `format_interview_context()` to render Q&A history as markdown
3. **Prompt injection**: Prepend formatted history in `build_fsm_footer`'s Interviewing arm
4. **Defensive serde**: Add `alias = "question"` for LLM key-name hallucination

### Data Flow (After Fix)

```
Interactive mode:
  LLM -> InterviewQuestion{questions} -> executor
    -> IPC coordinator.interview_question -> handler stores pending_questions on state
    -> TUI displays questions
    -> User types answer
    -> IPC coordinator.interview_respond -> handler moves pending_questions into exchange
    -> CoordinatorState.interview_context[N] = {questions, answer, timestamp}

Auto mode:
  LLM -> InterviewQuestion{questions} -> executor
    -> IPC coordinator.interview_question -> handler stores pending_questions on state
    -> IPC coordinator.interview_respond(synthetic_answer) -> handler moves pending_questions into exchange
    -> CoordinatorState.interview_context[N] = {questions, answer, timestamp}

Next iteration:
  build_fsm_footer -> coord_state.format_interview_context() -> history in prompt
  LLM sees Q&A history -> asks follow-ups or ProposePlan
```

### Fix 1: Record Questions in InterviewExchange

**File:** `src/daemon/handlers.rs`, `src/domain/coordinator_state.rs`, `src/agents/executor.rs`

When the `InterviewQuestion` action fires, the executor calls different IPC depending on mode:
- **Interactive mode**: calls `coordinator.interview_question` (broadcasts to TUI), then later the user calls `coordinator.interview_respond`
- **Auto mode**: skips `coordinator.interview_question` entirely and calls `coordinator.interview_respond` directly with a synthetic answer

The questions need to be captured in both paths so the interview history is complete.

**Approach A - `pending_questions` on state, set by `interview_question` handler:**

Add `pending_questions: Vec<String>` to `CoordinatorState`. Modify `handle_coordinator_interview_question` (currently takes `_stores` unused) to store questions on state and persist. Then `handle_coordinator_interview_respond` moves `pending_questions` into the exchange.

Problem: Auto mode never calls `interview_question`, so questions are lost in that path. Would need the Auto mode executor path to also call `interview_question` first.

**Approach B - Route both modes through `interview_question` first:**

In the executor's Auto mode path, call `coordinator.interview_question` before `coordinator.interview_respond`. This ensures questions are always stored via the same handler.

**Approach C - Pass questions in `interview_respond`:**

Add an optional `questions` parameter to the `interview_respond` IPC call. The executor passes them in Auto mode. In Interactive mode, the daemon reads from `pending_questions` (set by `interview_question` handler). This avoids changing the Auto mode executor flow.

**Chosen: Approach A + route Auto through interview_question** - cleanest data path, both modes store questions the same way.

Changes:

1. Add `pending_questions: Vec<String>` to `CoordinatorState` (with `#[serde(default)]`)
2. In `handle_coordinator_interview_question`: change `_stores` to `stores`, find active coordinator state, write questions to `state.pending_questions`, persist
3. In `handle_coordinator_interview_respond`: move `state.pending_questions` into `exchange.questions`, then clear `pending_questions`
4. In executor Auto mode path (`src/agents/executor.rs`): call `coordinator.interview_question` before `coordinator.interview_respond` so questions are stored

### Fix 2: Format Interview Context

**File:** `src/domain/coordinator_state.rs`

Add a `format_interview_context(&self) -> String` method:

```rust
pub fn format_interview_context(&self) -> String {
    if self.interview_context.is_empty() {
        return String::new();
    }
    let mut s = String::from("### Interview History\n\n");
    for (i, exchange) in self.interview_context.iter().enumerate() {
        s.push_str(&format!("**Round {}:**\n", i + 1));
        if !exchange.questions.is_empty() {
            s.push_str("Questions:\n");
            for q in &exchange.questions {
                s.push_str(&format!("- {}\n", q));
            }
        }
        s.push_str(&format!("User response: {}\n\n", exchange.answer));
    }
    s
}
```

### Fix 3: Inject History into FSM Footer

**File:** `src/agents/coordinator.rs`

In `build_fsm_footer`, the `Interviewing` match arm prepends the formatted interview context:

```rust
CoordinatorFsmState::Interviewing => {
    let history = coord_state.format_interview_context();
    let base = "## Interviewing\n\n\
        You are in the Interviewing state. Generate interview questions to clarify the user's goal, \
        or propose a Plan if you have enough context.\n\n\
        Use InterviewQuestion to ask the user questions, or ProposePlan to propose a Plan draft.\n\n\
        Respond with a JSON array of actions.";
    if history.is_empty() {
        base.to_string()
    } else {
        format!("{}\n\n{}", history, base)
    }
}
```

### Fix 4: Serde Alias for InterviewQuestion (defensive)

**File:** `src/agents/mod.rs`

```rust
InterviewQuestion {
    #[serde(default, alias = "question", deserialize_with = "string_or_vec")]
    questions: Vec<String>,
},
```

### Implementation Plan

**Phase 1** - Data capture (Fix 1):
- Add `pending_questions: Vec<String>` to `CoordinatorState` (with `#[serde(default)]`)
- Update `handle_coordinator_interview_question`: change `_stores` to `stores`, find active coordinator state, write questions to `state.pending_questions`, persist to TaskStore
- Update `handle_coordinator_interview_respond`: set `exchange.questions = std::mem::take(&mut state.pending_questions)` instead of `vec![]`
- Update executor Auto mode path: call `coordinator.interview_question` before `coordinator.interview_respond`

**Phase 2** - Prompt injection (Fixes 2 + 3):
- Add `format_interview_context(&self) -> String` method to `CoordinatorState`
- Update `build_fsm_footer` Interviewing arm to prepend formatted history

**Phase 3** - Defensive serde (Fix 4):
- Add `alias = "question"` to `InterviewQuestion` in `src/agents/mod.rs`

**Phase 4** - Validate:
- Run `otto ci`
- Run `test_persona_golden_path` against live LLM

## Alternatives Considered

### Alternative 1: Store Full Exchange in interview_respond Only
- **Description:** Instead of pending_questions, pass questions as a parameter in the `interview_respond` IPC call (the TUI would echo them back).
- **Pros:** No new state field; single write path.
- **Cons:** Requires TUI changes to echo questions back. Breaks abstraction - the TUI shouldn't need to track coordinator internals.
- **Why not chosen:** Adds coupling between TUI and coordinator state management.

### Alternative 2: Reconstruct Questions from Event Log
- **Description:** Instead of storing questions, replay `coordinator.interview_question` events to reconstruct the Q&A timeline.
- **Pros:** No schema changes.
- **Cons:** Events are ephemeral (broadcast channel); not persisted. Fragile reconstruction logic.
- **Why not chosen:** Events aren't durable. The coordinator needs guaranteed history across daemon restarts.

### Alternative 3: Include Full Conversation History (message-level)
- **Description:** Instead of just interview context, pass the full message history from the coordinator's prior iterations.
- **Pros:** Most complete context.
- **Cons:** Massive token overhead. The coordinator already gets fresh context each iteration; only the interview Q&A is missing.
- **Why not chosen:** Overkill. The FSM footer already provides state-specific context. Only interview exchanges are absent.

## Technical Considerations

### Dependencies
- No new crates. Only internal changes to `CoordinatorState`, executor, daemon handlers, and coordinator prompt builder.

### Performance
- `format_interview_context` is O(n) over exchanges. Typical interview has 2-4 rounds - negligible.

### Testing Strategy
- Unit test: `format_interview_context` with empty, single, and multi-round exchanges
- Unit test: serde round-trip for `InterviewExchange` with populated questions
- Integration: `test_persona_golden_path` must progress past Interviewing to produce a plan

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| pending_questions desync (questions set but no respond) | Low | Low | pending_questions overwritten each round; stale data is harmless context |
| Multiple InterviewQuestion actions in one LLM turn | Low | Low | Last-write-wins for pending_questions; all questions still broadcast to TUI |
| Token budget exceeded with long interviews | Low | Med | Future: add a max_rounds config or summarize older exchanges |
| LLM ignores history and re-asks | Low | Med | The history format is explicit; if it still loops, the prompt wording can be strengthened |
| Daemon restart between question and respond | Low | None | pending_questions persisted in CoordinatorState via TaskStore; survives restart |

## Open Questions

- [ ] Should there be a max interview rounds before forcing a ProposePlan?

## References

- `src/agents/coordinator.rs` - FSM footer and action handling
- `src/domain/coordinator_state.rs` - CoordinatorState and InterviewExchange
- `src/daemon/handlers.rs` - interview_question and interview_respond IPC handlers
- `src/agents/mod.rs` - InterviewQuestion action definition
- `tests/funnel.rs` - test_persona_golden_path e2e test
