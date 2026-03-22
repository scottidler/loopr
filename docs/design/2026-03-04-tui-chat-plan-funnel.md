# Design Document: TUI Chat→Plan→Execute Funnel

**Author:** Scott Idler
**Date:** 2026-03-04
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Redesign the TUI Chat→Plan funnel so that Chat and Interview (Plan mode) both use the same TUI-side LLM with prompt augmentation, replacing the current daemon-side Coordinator IPC for the interview phase. The daemon only enters the picture at Executing. This simplifies the architecture, removes unnecessary IPC round-trips, and makes the interview feel like natural conversation.

## Problem Statement

### Background

The TUI currently has `ChatMode::Chat` and `ChatMode::Plan`, a `FunnelState` enum (Chat → Interview → PlanDraft → Executing), border color feedback, and slash commands (`/plan`, `/draft`, `/chat`, `/accept`, `/clear`, `/help`). The interview phase currently sends IPC to the daemon (`coordinator.set_goal`, `agent.start`, `coordinator.interview_respond`) and waits for daemon responses (`coordinator.interview_question`, `coordinator.plan_proposed`).

### Problem

1. **Over-engineered interview** — The daemon-side Coordinator interview adds IPC complexity for what is fundamentally a focused conversation. The interview is just chat with a goal.
2. **No visual state indicator** — Border colors are implemented but the UX doesn't fully leverage them to communicate funnel progression.
3. **Missing footer context** — Footer hints don't fully adapt to available actions per funnel state.
4. **Abrupt transitions** — No prompt augmentation to guide the LLM's behavior when switching modes.

### Goals

- Chat and Interview use the same TUI-side LLM, differentiated only by prompt augmentation
- Remove daemon IPC for interview phase (keep only `coordinator.approve_plan` for Executing)
- Context-sensitive footer hints per funnel state
- Border colors communicate funnel state (already implemented, verify correct)
- Slash commands gate to valid states (already implemented, verify correct)

### Non-Goals

- Rich Markdown rendering of plan drafts (plain text for MVP)
- Plan version history (overwrite only)
- Daemon awareness of funnel states
- New TUI layout or pane structure

## Proposed Solution

### Architecture

**Key Principle:** Chat and Interview are the same TUI-side LLM. The only difference is Plan mode injects extra system prompt text to focus the conversation on coalescing around a plan. No IPC to the daemon during Chat or Interview.

**State Ownership:** `FunnelState` lives exclusively in `ChatState` (TUI-side). The daemon receives only the final approved plan for execution.

### What Changes vs What Stays

| Component | Current | After |
|-----------|---------|-------|
| Interview input routing | IPC → daemon Coordinator | TUI-side LLM (augmented prompt) |
| `/plan` action | `SetGoalAndStart` IPC | Local: inject plan-focus prompt, change state |
| `/draft` action | `InterviewRespond("/draft")` IPC | Local: send "/draft" to TUI-side LLM |
| Interview responses | `InterviewRespond` IPC | TUI-side LLM (same as Chat) |
| `coordinator.set_goal` IPC | Used for interview | Removed |
| `coordinator.interview_respond` IPC | Used for interview | Removed |
| `coordinator.interview_question` event | Daemon pushes questions | Removed |
| `coordinator.plan_proposed` event | Daemon pushes draft | Removed |
| `coordinator.approve_plan` IPC | Used for execution | **Stays** — only IPC entry point |
| Border colors | Already per-FunnelState | Stays |
| FunnelState enum | Already exists | Stays |
| ChatMode enum | Chat/Plan | Stays |
| Slash command parsing | Already exists | Stays (adjust dispatch) |

### Funnel States

```
┌─────────────────────────────────────────────────────────────────┐
│ 1. CHAT (TUI-side LLM, free conversation)                      │
│    User explores ideas, builds context, pastes content          │
│    Border: cyan                                                 │
│    Footer: [Enter] Send  [Shift+Enter] Newline  /plan Plan     │
│                                                                 │
│    Transition: user types /plan                                 │
├─────────────────────────────────────────────────────────────────┤
│ 2. INTERVIEW (TUI-side LLM, plan-focused prompt augmentation)  │
│    Full chat history carried forward as context                 │
│    LLM guided by extra prompt to ask clarifying questions       │
│    User answers in same input area (free text)                  │
│    Border: yellow (in-progress)                                 │
│    Footer: [Enter] Send  /draft Build Draft  /chat Back to Chat │
│                                                                 │
│    Transitions:                                                 │
│      - User types /draft → LLM formats plan as draft           │
│      - User types /chat → drop plan prompt, return to Chat     │
├─────────────────────────────────────────────────────────────────┤
│ 3. PLAN DRAFT (TUI-side LLM generates structured plan)         │
│    Draft shown as plain text in Chat pane                       │
│    User can suggest edits in free text → LLM refines           │
│    Border: green (ready for review)                             │
│    Footer: /accept Accept Plan  /chat Back to Chat             │
│                                                                 │
│    Transitions:                                                 │
│      - /accept or Ctrl+a → hand plan to daemon, begin execution│
│      - User suggests edits → LLM refines, stays here           │
│      - /chat → discard draft, back to Chat                     │
├─────────────────────────────────────────────────────────────────┤
│ 4. EXECUTING (Daemon-side Coordinator automation)              │
│    Plan → Specs → Phases → Works pipeline                       │
│    Dashboard shows progress (switch to Dashboard view)          │
│    Border: blue (executing)                                     │
│    Footer: p:Pause  r:Resume  x:Stop                           │
│                                                                 │
│    Transition: all phases complete → GoalComplete                │
└─────────────────────────────────────────────────────────────────┘
```

### FunnelState Enum (existing — no changes)

```rust
pub enum FunnelState {
    Chat,       // Free conversation (TUI-side LLM)
    Interview,  // Plan-focused chat (TUI-side LLM, augmented prompt)
    PlanDraft,  // Draft plan proposed, awaiting user review
    Executing,  // Plan accepted, daemon automation running
}
```

Drives:
- **Border color**: cyan → yellow → green → blue
- **Footer hints**: context-sensitive per state
- **Prompt augmentation**: Chat→none, Interview→plan-focus prompt, PlanDraft→draft-refine prompt

### Input Routing

| FunnelState | Input goes to | Slash commands available |
|-------------|---------------|------------------------|
| Chat | TUI-side LLM | `/plan`, `/clear`, `/help` |
| Interview | TUI-side LLM (augmented prompt) | `/draft`, `/chat`, `/clear`, `/help` |
| PlanDraft | TUI-side LLM (augmented prompt) | `/accept`, `/chat`, `/clear`, `/help` |
| Executing | Disabled (read-only) | none (keybinds only: p/r/x) |

### Prompt Augmentation

When entering Interview, prepend the following system prompt to LLM calls:

```
You are helping the user coalesce around a concrete, actionable plan.
Your job is to ask clarifying questions until the goal, scope, and
acceptance criteria are clear. Do not propose a plan until the user
signals they are ready by typing /draft. Focus on understanding the
problem, constraints, and desired outcome.
```

When `/draft` is typed, replace the augmentation with:

```
The user is ready for a plan draft. Based on the conversation so far,
produce a structured plan with: Title, Goal, Acceptance Criteria
(numbered list), and Phases (if applicable). Output plain text, not
markdown. Be concise.
```

When in PlanDraft and the user sends free text (edits), use:

```
The user wants to refine the plan draft. Apply their feedback and
output the revised plan in the same format. Only change what they
asked for.
```

### Plan Handoff to Daemon

When the user accepts the plan (`/accept` or `Ctrl+a`):

1. Extract the last assistant message containing the plan draft
2. Send via existing `coordinator.approve_plan` IPC: `{ plan: "<plan text>" }`
3. Set `funnel_state = FunnelState::Executing`
4. Switch to Dashboard view

### Slash Command Reference

| Command   | Available In         | Action                                          |
|-----------|---------------------|-------------------------------------------------|
| `/plan`   | Chat                | Inject plan-focus prompt, enter Interview        |
| `/draft`  | Interview           | Signal LLM to format conversation as plan draft  |
| `/accept` | PlanDraft           | Accept plan, hand off to daemon for execution    |
| `/chat`   | Interview, PlanDraft | Drop augmentation, discard draft, return to Chat |
| `/clear`  | Chat only           | Clear chat history and reset to Chat state       |
| `/help`   | All                 | Show available commands (includes Ctrl+a hint)   |

### Border Color Semantics (existing — no changes)

| State     | Color  | Meaning                    |
|-----------|--------|----------------------------|
| Chat      | Cyan   | Free exploration           |
| Interview | Yellow | In-progress, gathering info |
| PlanDraft | Green  | Ready for review/approval  |
| Executing | Blue   | Automation running         |

### Edge Cases

| Scenario | Behavior |
|----------|----------|
| `/plan` while already in Interview | Ignore, already in Plan mode |
| `/draft` with no interview content | LLM will do its best with available context; user can `/chat` back |
| `/accept` with no draft | Command unavailable (only available in PlanDraft state) |
| `/clear` in Interview or PlanDraft | Reset to Chat state (clear history = lose interview context) |
| `/plan` typed as first message | Works — LLM starts interview with no prior context |
| LLM produces malformed plan on `/draft` | User refines via free text or `/chat` back and retries |

## IPC (Executing Only)

The only IPC call in this funnel:

```
TUI                              Daemon
 │                                 │
 │ coordinator.approve_plan        │
 │ { plan: "<plan text>" }        │
 │ ──────────────────────────────► │
 │                                 │
 │ FunnelState → Executing         │
 │ (switch to Dashboard view)      │
```

All other transitions are local state changes within the TUI.

## UI Mockups

### Chat State (cyan border)
```
┌─ Chat ──────────────────────────────────────────────────────┐
│ Welcome to Loopr Chat                                       │
│                                                             │
│ Type a message and press Enter to explore ideas.            │
│ Type /plan when ready to formalize a plan.                  │
│                                                             │
│ > _                                                         │
└─────────────────────────────────────────────────────────────┘
[Enter] Send  [Shift+Enter] Newline  [Esc] Scroll  /plan Plan
```

### Interview State (yellow border)
```
┌─ Plan ──────────────────────────────────────────────────────┐
│ System: Entering Plan mode. Focusing on your goal.          │
│                                                             │
│ LLM: What problem are you trying to solve? What are the     │
│ constraints I should know about?                            │
│                                                             │
│ > Parallel validation, max 8 validators_                    │
└─────────────────────────────────────────────────────────────┘
[Enter] Send  [Shift+Enter] Newline  /chat Chat  /draft Build Draft
```

### Plan Draft State (green border)
```
┌─ Plan ──────────────────────────────────────────────────────┐
│ === Proposed Plan ===                                       │
│ Title: Parallel Bundle Validation                           │
│                                                             │
│ Acceptance Criteria:                                        │
│ 1. Validation runs in parallel (configurable)               │
│ 2. Max 8 concurrent validators                              │
│ 3. Results aggregated before seal decision                  │
│                                                             │
│ > _                                                         │
└─────────────────────────────────────────────────────────────┘
/accept Accept Plan  /chat Chat
```

## Implementation Plan

### Phase 1: Remove daemon IPC for Interview

1. Remove `IpcAction::SetGoalAndStart` usage from `/plan` command handler
2. Remove `IpcAction::InterviewRespond` usage from Interview message handling
3. Remove handling of `coordinator.interview_question` and `coordinator.plan_proposed` events
4. `/plan` now only changes `chat_mode`, `funnel_state`, and injects system message

### Phase 2: Add prompt augmentation

1. Add prompt augmentation strings (Interview, Draft, PlanDraft refine)
2. Modify LLM call path to prepend augmentation based on `funnel_state`
3. `/draft` sends the draft prompt to LLM instead of IPC

### Phase 3: Plan handoff

1. On `/accept` or `Ctrl+a`, extract plan text from last assistant message
2. Send via `coordinator.approve_plan` IPC with plan text payload
3. Verify Dashboard view switch works with new flow

### Phase 4: Footer and edge case polish

1. Update footer hints per state (verify against spec)
2. Implement edge case handling (command gating, `/clear` reset)
3. Verify border colors match spec (likely already correct)

## Testing Strategy

- Unit tests for `FunnelState` transitions (state machine correctness)
- Unit tests for slash command gating (right commands available in right states)
- Unit tests for prompt augmentation selection based on funnel state
- Integration test: `/plan` → interview messages → `/draft` → plan output → `/accept` flow
- Verify removed IPC paths don't leave dead code

## Decisions

| # | Question | Decision |
|---|----------|----------|
| 1 | Plan versioning | **Overwrite.** No draft history. |
| 2 | Error recovery | **N/A.** Chat and Interview use the same TUI-side LLM. Standard LLM error handling applies. |
| 3 | Timeout handling | **N/A.** Same reasoning as #2. |
| 4 | `/accept` vs `Ctrl+a` | **Equivalent.** `/accept` shown in footer bar. `Ctrl+a` documented in `/help`. |
| 5 | Plan rendering | **Plain text for MVP.** Revisit rich Markdown rendering later. |
| 6 | State ownership | **TUI-only.** `FunnelState` lives in `ChatState`. Daemon unaware of funnel states. |
| 7 | Transcript handoff | **Full message history** sent as context when entering Plan mode and when handing plan to daemon. |

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| LLM produces poor plan drafts without daemon Coordinator guidance | Medium | Medium | Prompt augmentation tuning; user can refine or `/chat` back |
| Removing IPC paths breaks other features that depend on them | Low | High | Check all callers of removed IPC actions before deleting |
| Plan text extraction from chat history is fragile | Medium | Low | Extract last assistant message; if malformed, user refines |

## Alternatives Considered

### Alternative 1: Keep daemon-side Coordinator for Interview
- **Description:** Current implementation — IPC round-trips for each interview question
- **Pros:** Coordinator agent has specialized logic
- **Cons:** Over-engineered, adds latency, IPC complexity for what is just focused conversation
- **Why not chosen:** Interview is fundamentally chat with a goal prompt — no need for daemon involvement

### Alternative 2: Wizard-style flow with numbered steps
- **Description:** Rigid step-by-step form
- **Pros:** Predictable structure
- **Cons:** Too rigid, doesn't allow free-text back-and-forth
- **Why not chosen:** Breaks the conversational feel

### Alternative 3: Separate plan editor pane
- **Description:** Dedicated pane for plan editing
- **Pros:** Clear separation of concerns
- **Cons:** Over-engineered for MVP, adds layout complexity
- **Why not chosen:** Plain text in chat pane is sufficient for MVP

### Alternative 4: Auto-execute on plan proposal
- **Description:** Skip approval step
- **Pros:** Faster workflow
- **Cons:** Dangerous — user must explicitly approve before automation runs
- **Why not chosen:** Safety requires explicit approval gate

## References

- [Loopr v3 MVP4 Design](2026-02-26-multi-level-rwl.md) — Coordinator agent, multi-level RWL
- [Loopr v3 MVP3 Design](2026-02-26-implementer-reviewer-agents.md) — Implementer + Reviewer agents
- `src/tui/app.rs` — FunnelState, ChatMode, App struct
- `src/tui/input.rs` — Slash command parsing and dispatch
- `src/tui/views/chat.rs` — Border color logic
- `src/tui/run.rs` — Footer rendering, IPC dispatch
