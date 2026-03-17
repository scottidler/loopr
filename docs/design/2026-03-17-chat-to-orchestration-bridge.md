# Design Document: Chat-to-Orchestration Bridge

**Author:** Scott Idler + Claude
**Date:** 2026-03-17
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

The Chat-to-Orchestration Bridge wires loopr's two implemented halves together: the TUI chat with agentic tool loop (Layer 6) and the orchestration pipeline with Coordinator/Implementer/Reviewer/Integrator agents (Layers 1-5). Today, `/accept` in the chat funnel creates a Plan record and sets `plan_approved = true`, but nobody starts the Coordinator agent, no orchestration events flow back to the chat, and the user gets dumped to the Dashboard with no visibility into what happens next. This doc closes those gaps.

## Problem Statement

### Background

Loopr has two fully-built capabilities:

1. **Chat + Agentic Tool Loop** - The user chats with an LLM through the TUI. The LLM has 14+ tools (read, write, edit, grep, shell, delegate, etc.), streaming SSE, context compaction, and a chat funnel with four states: Chat -> Interview -> PlanDraft -> Executing. The chat is useful for exploration, investigation, and one-off changes.

2. **Orchestration Pipeline** - A Coordinator agent manages the Plan -> Spec -> Phase -> Work decomposition. Implementer agents write code in isolated worktrees. Reviewer agents validate Bundles. A deterministic Integrator merges accepted Bundles into Ticks. FSMs enforce correctness. TaskStore persists everything.

The chat funnel was designed to be the on-ramp to orchestration: user explores an idea in Chat mode, transitions to Interview to sharpen the plan, reviews a draft, and `/accept` kicks off autonomous execution.

### Problem

Three specific gaps prevent the handoff from working:

**1. accept_plan doesn't start the Coordinator.**

`handle_coordinator_accept_plan` (handlers.rs:4083) creates a Plan record from text, activates it (Draft -> Active), updates an existing CoordinatorState (sets `plan_approved = true`, transitions FSM to Planning), and emits a `coordinator.plan_accepted` event. But it never dispatches `agent.start` for the Coordinator. The `auto_start_agents` function (handlers.rs:235) only handles `work.transition -> InProgress` (for Implementers) and `bundle.transition -> Triaged` (for Reviewers). There is no trigger for `coordinator.plan_accepted`. Note: `auto_start_agents` matches on IPC method names, not DaemonEvent kinds, so even though `coordinator.plan_accepted` is emitted as an event it has no effect on agent startup.

Additionally, `accept_plan` doesn't create a CoordinatorGoal record. The Coordinator agent's `run()` loop (coordinator.rs:1499) calls `load_or_create_coordinator_state`, which reads the `coordinator_goals` store for an active goal. Without one, the Coordinator sleeps in an idle loop waiting for a goal to appear. The `set_goal` and `accept_plan` paths are disconnected: `set_goal` creates the CoordinatorGoal but doesn't create a Plan or start the Coordinator, while `accept_plan` creates the Plan and updates CoordinatorState but doesn't create a CoordinatorGoal. Neither path starts the Coordinator agent.

Note: the Coordinator *does* auto-create its own CoordinatorState if none exists (coordinator.rs:665-678), so `accept_plan`'s current behavior of updating an existing state will fail if no state exists yet. The bridge must create the CoordinatorGoal (which the Coordinator requires) and can let the Coordinator create its own CoordinatorState on first iteration.

**2. No orchestration events flow to the Chat view.**

When the user hits `/accept`, the TUI switches to Dashboard view (`input.rs:329`). The Chat view enters `FunnelState::Executing` but has no mechanism to receive orchestration events. The Coordinator emits `DaemonEvent`s (record_created, record_updated, agent status changes), and the TUI event loop receives them, but the Chat view doesn't render them. The user must manually check the Works, Bundles, and Agents views to understand progress.

**3. No way to interact during execution.**

Once in `Executing` state, the Chat input is still active but there's no defined behavior for what happens when the user sends a message. Should it go to the chat LLM? Should it be forwarded to the Coordinator? Can the user ask "what's the status?" and get an answer that reflects orchestration state? Currently, sending a message in Executing state starts a new chat.submit call that runs the agentic loop independently - completely disconnected from the running Coordinator.

### Goals

- G1: `/accept` creates a CoordinatorGoal, starts the Coordinator agent, and begins autonomous execution with zero manual steps
- G2: Orchestration events (Work created, Implementer started, Bundle proposed, review result, Tick published) stream into the Chat view as system messages
- G3: The user can send messages during Executing state that are context-aware (know what the orchestration is doing)
- G4: The user can intervene during execution (pause, redirect, stop) from the Chat view
- G5: All changes pass `otto ci` with no regressions

### Non-Goals

- Changing the Coordinator's internal FSM or decomposition logic
- Modifying the Implementer/Reviewer/Integrator agent behavior
- Adding new tools to the chat agentic loop
- Redesigning the TUI layout
- Modifying TaskStore or the persistence layer

## Proposed Solution

### Overview

Three changes, layered:

1. **Accept-to-Launch** - Make `handle_coordinator_accept_plan` a complete handoff: create CoordinatorGoal, start Coordinator agent. The Coordinator creates its own CoordinatorState on first iteration. One IPC call does everything.

2. **Orchestration Event Stream** - Subscribe the Chat view (in Executing state) to a filtered set of orchestration DaemonEvents. Render them as system messages in the chat history. No new event types needed - just surface existing events.

3. **Execution-Aware Chat** - In Executing state, route chat.submit through a modified system prompt that includes orchestration status. The user talks to the same chat LLM, but it has context about what the Coordinator is doing. Add `/pause`, `/stop`, and `/status` slash commands for direct intervention.

### Architecture

#### Change 1: Accept-to-Launch

Current flow:
```
/accept -> TUI extracts plan text -> IpcAction::AcceptPlan(text)
  -> handle_coordinator_accept_plan:
       1. Create Plan record from text (or resolve existing plan_id)
       2. Activate Plan (Draft -> Active)
       3. Find existing CoordinatorState, set plan_approved + FSM=Planning
       4. Emit coordinator.plan_accepted event
  -> TUI switches to Dashboard view
```

Problems with the current flow:
- Step 3 assumes a CoordinatorState already exists (it won't on the chat path)
- No CoordinatorGoal is created (Coordinator idles without one)
- No Coordinator agent is started
- TUI abandons the chat context

New flow:
```
/accept -> TUI extracts plan text -> IpcAction::AcceptPlan(text)
  -> handle_coordinator_accept_plan:
       1. Create Plan record from text (unchanged)
       2. Activate Plan (Draft -> Active) (unchanged)
       3. Create CoordinatorGoal from plan title (NEW)
       4. Start Coordinator agent via agent.start dispatch (NEW)
       5. Emit coordinator.plan_accepted event with goal_id + plan_id (enriched)
  -> TUI stays on Chat view in Executing state (CHANGED)
```

The Coordinator agent, on its first iteration, calls `load_or_create_coordinator_state` which:
- Finds the active CoordinatorGoal (created in step 3)
- Creates a new CoordinatorState with `InterviewMode::Skip` (since the goal text already contains the full plan)
- The Plan already exists and is Active, so the Coordinator's FSM will detect it and advance past Planning to ActivatePhase

Key changes in `handle_coordinator_accept_plan`:

```rust
// After creating + activating the Plan...

// Step 3: Create CoordinatorGoal (the Coordinator needs this to operate)
let goal_text = plan_title.clone(); // or a richer summary
let goal = CoordinatorGoal::new(goal_text);
let goal_id = goal.id.clone();
// persist goal to TaskStore and in-memory store (same pattern as handle_coordinator_set_goal)

// Step 4: Start the Coordinator agent
// Note: handle_coordinator_accept_plan needs worktree_mgr and integrator_config
// added to its signature (passed through from dispatch)
let start_req = DaemonRequest::new(0, "agent.start", json!({
    "agent_type": "coordinator",
    "goal_id": goal_id,
}));
let start_resp = dispatch(stores, event_tx, worktree_mgr, integrator_config, start_req);
// Extract coordinator_session_id from start_resp for the response
```

**Signature change required:** `handle_coordinator_accept_plan` currently takes `(stores, event_tx, req)`. It needs `worktree_mgr` and `integrator_config` added to call `dispatch` internally. Update both the function signature and the dispatch table call site.

The existing step 3 (updating a pre-existing CoordinatorState) should be removed. The Coordinator creates its own state on first iteration. If a stale CoordinatorState exists from a previous goal, the Coordinator's `load_or_create_coordinator_state` handles this (it matches on goal_id).

#### Change 2: Orchestration Event Stream

The TUI event loop (`tui/run.rs`) already receives all DaemonEvents via broadcast. Today it updates view-specific state (Works list, Bundles list, etc.). Add a new path: when `funnel_state == Executing`, also append filtered events to chat_history as system messages.

**Events to surface in Chat (filtered set):**

| Event | Chat Message |
|-------|-------------|
| `record.created` (spec) | "Created Spec: {title}" |
| `record.created` (phase) | "Created Phase: {title}" |
| `record.created` (work) | "Created Work: {title}" |
| `agent.status_changed` (implementer -> Running) | "Implementer started on Work: {work_id}" |
| `record.created` (bundle) | "Bundle proposed for Work: {work_id}" |
| `agent.status_changed` (reviewer -> Running) | "Reviewing Bundle: {bundle_id}" |
| `record.updated` (bundle -> Accepted) | "Bundle accepted: {bundle_id}" |
| `record.updated` (bundle -> Rejected) | "Bundle rejected: {bundle_id} - retrying" |
| `record.updated` (tick -> Published) | "Tick published: {tick_id} ({n} bundles merged)" |
| `record.updated` (work -> Done) | "Work complete: {work_id}" |
| `record.updated` (work -> Abandoned) | "Work abandoned: {work_id}" |
| `coordinator.plan_accepted` | "Plan accepted. Coordinator starting decomposition." |
| `coordinator.fsm_transition` | "Coordinator: {from_state} -> {to_state}" |

Implementation in `tui/run.rs`:

```rust
// In the event processing loop, after existing view updates:
if app.funnel_state == FunnelState::Executing {
    if let Some(chat_msg) = format_orchestration_event(&event) {
        app.chat_history.push(ChatMessage {
            role: ChatMessageRole::System,
            content: chat_msg,
        });
    }
}
```

The `format_orchestration_event` function is a simple match on event type that produces human-readable one-liners. Events not in the filter set are silently dropped - we don't want every `record.updated` for internal bookkeeping fields cluttering the chat.

#### Change 3: Execution-Aware Chat

In `FunnelState::Executing`, the user can still type messages. Route them through chat.submit as today, but with a modified system prompt that includes orchestration context:

```rust
pub const EXECUTING_PROMPT: &str = "\
You are monitoring an active orchestration pipeline in Loopr. \
The Coordinator agent is decomposing a Plan into Specs, Phases, and Works, \
then assigning Implementer agents to write code in isolated worktrees.\n\n\
You can help the user understand progress, answer questions about the pipeline, \
and relay intervention commands. The orchestration status is included below.\n\n\
Available intervention commands the user can type:\n\
- /pause - Pause the Coordinator (implementations in progress will complete)\n\
- /stop - Stop all orchestration (cancel Coordinator and active agents)\n\
- /status - Show detailed orchestration status\n\n\
ORCHESTRATION STATUS:\n";
```

**Problem:** `system_prompt_for_chat` is currently a pure function (no store access). In Executing state, it needs orchestration status from `Stores`. Two options:

1. Change the signature to accept an optional status string, built by the caller (`handle_chat_submit` has `Stores` access).
2. Keep the function pure; have `handle_chat_submit` build the full prompt directly when funnel_state is Executing.

Option 1 is cleaner - the caller passes `orchestration_status: Option<&str>`:

```rust
// In handle_chat_submit, before calling system_prompt_for_chat:
let orch_status = if funnel_state == FunnelState::Executing {
    Some(build_orchestration_status(stores))
} else {
    None
};
let system_prompt = system_prompt_for_chat(funnel_state, is_draft_request, orch_status.as_deref());

// In system_prompt_for_chat:
FunnelState::Executing => {
    let status = orchestration_status.unwrap_or("(no status available)");
    format!("{CHAT_SYSTEM_PROMPT}\n\n{EXECUTING_PROMPT}{status}")
}
```

`build_orchestration_status` is a new helper (in `domain/chat.rs` or extracted from `coordinator.rs`) that reads the `Stores` and produces a compact text summary: active Works with status, running agents, recent Bundle outcomes, current Coordinator FSM state.

New slash commands for intervention:

| Command | State | Effect |
|---------|-------|--------|
| `/pause` | Executing | Dispatch `agent.pause` for coordinator session |
| `/stop` | Executing | Dispatch `agent.stop` for coordinator + all active agents |
| `/status` | Executing | Insert orchestration summary as system message |

### Data Model

No new domain types needed. Existing types used:

- `CoordinatorGoal` - created by accept_plan (NEW usage; currently only created by set_goal)
- `CoordinatorState` - created by Coordinator agent on first iteration (no change; accept_plan no longer touches it)
- `Plan` - already created by accept_plan (no change)
- `FunnelState::Executing` - already exists (no change)
- `DaemonEvent` - already emitted by orchestration (no change)

One new field on `ChatHistory`:

```rust
pub struct ChatHistory {
    // ... existing fields ...
    /// The coordinator goal_id associated with this chat session's execution.
    /// Set when /accept transitions to Executing state. Used to filter events
    /// and build orchestration status for the system prompt.
    pub goal_id: Option<String>,
}
```

### API Design

**Modified IPC Methods:**

`coordinator.accept_plan` - extended params and response:
```json
// Request (unchanged params, new behavior)
{ "method": "coordinator.accept_plan", "params": { "plan": "..." } }

// Response (new fields)
{ "result": { "accepted": true, "plan_id": "pl-xxx", "goal_id": "cg-xxx", "coordinator_session_id": "..." } }
```

`chat.submit` - extended params:
```json
// Request (new optional field)
{ "method": "chat.submit", "params": {
    "session_id": "...",
    "message": "...",
    "funnel_state": "executing",
    "goal_id": "cg-xxx"  // NEW: used to build orchestration status for system prompt
} }
```

**New Slash Commands:**

| Command | IPC Action | Handler |
|---------|-----------|---------|
| `/pause` | `agent.pause` (coordinator session) | existing handler |
| `/stop` | `agent.stop` (coordinator + children) | existing handler, called N times |
| `/status` | `coordinator.get_state` + `system.status` | existing handlers, formatted locally |

### Implementation Plan

**Phase 1: Accept-to-Launch (the critical path)**

Files modified:
- `src/daemon/handlers.rs` - extend `handle_coordinator_accept_plan` to create CoordinatorGoal and dispatch agent.start. Add `worktree_mgr` and `integrator_config` to function signature. Remove the existing CoordinatorState update block (Coordinator creates its own). Update dispatch table call site.
- `src/domain/chat.rs` - add `goal_id: Option<String>` to ChatHistory with `#[serde(default)]`
- `src/tui/input.rs` - keep Chat view on /accept instead of switching to Dashboard; parse goal_id from IPC response and store on app state

Tests:
- `handle_coordinator_accept_plan` creates CoordinatorGoal and Plan records
- `handle_coordinator_accept_plan` dispatches agent.start for coordinator
- Response includes goal_id and plan_id
- Existing accept_plan tests updated (remove CoordinatorState precondition)
- ChatHistory serde roundtrip with goal_id field

**Phase 2: Orchestration Event Stream**

Files modified:
- `src/tui/run.rs` - add event filtering and chat_history injection in Executing state
- `src/tui/views/chat.rs` - render System messages with distinct styling (dimmed, no avatar)

Tests:
- Unit test for `format_orchestration_event` covers all filtered event types
- Events outside the filter set produce None
- System messages render correctly in chat history

**Phase 3: Execution-Aware Chat**

Files modified:
- `src/domain/chat.rs` - add `EXECUTING_PROMPT` constant and Executing arm in `system_prompt_for_chat`
- `src/daemon/handlers.rs` - in `handle_chat_submit`, when funnel_state=Executing and goal_id present, build orchestration status and prepend to system prompt
- `src/tui/input.rs` - add `/pause`, `/stop`, `/status` slash commands gated on Executing state

Tests:
- system_prompt_for_chat with Executing state includes orchestration header
- /pause dispatches agent.pause for coordinator session
- /stop dispatches agent.stop for coordinator + active agents
- /status inserts system message with orchestration summary

## Alternatives Considered

### Alternative 1: Coordinator Reads Chat History Directly

- **Description:** Instead of separate chat and orchestration paths, have the Coordinator agent read the chat conversation as its input context (the Plan is the conversation itself).
- **Pros:** No bridge code needed. Chat and orchestration are unified.
- **Cons:** The Coordinator's RWL (fresh context each iteration) would conflict with a growing chat history. The Coordinator's action-based output format doesn't fit chat UX. Mixing conversational and structured agent roles creates confusion.
- **Why not chosen:** The separation between chat (conversational, flexible) and orchestration (structured, FSM-driven) is an intentional architectural choice. The bridge connects them without merging them.

### Alternative 2: Replace Coordinator with Chat Agent

- **Description:** Skip the Coordinator entirely. After /accept, the chat LLM directly creates Work records via tool calls, and those trigger Implementers.
- **Pros:** Simpler. No Coordinator FSM. The chat agent already has tools.
- **Cons:** Loses phase-gated sequencing, SLA tracking, decomposition quality control, retry logic. The chat agent's 3-iteration cap isn't designed for long-running orchestration. No RWL (context would grow unboundedly).
- **Why not chosen:** The Coordinator's structured approach (decompose, sequence, monitor, retry) is the value proposition. The chat is the UX, the Coordinator is the engine.

### Alternative 3: Fire-and-Forget (Dashboard Only)

- **Description:** On /accept, start the Coordinator and immediately switch to Dashboard. The user monitors via existing TUI views (Works, Bundles, Agents). No chat integration.
- **Pros:** Zero bridge code. Already mostly works today (minus the agent.start gap).
- **Cons:** Breaks the "chat is the interface" principle. The user's conversation context is lost. No way to ask questions about progress. Two completely separate interaction modes.
- **Why not chosen:** The whole point of the chat funnel is continuity. The user's exploration, interview, and plan refinement should flow seamlessly into execution monitoring and intervention.

## Technical Considerations

### Dependencies

- No new crates. All functionality uses existing infrastructure (DaemonEvent broadcast, IPC dispatch, agent lifecycle management).
- The `build_state_summary` function from coordinator.rs is reused for orchestration status in the chat prompt. It may need to be extracted to a shared location since it currently takes `Stores` + `AgentLogger`.

### Performance

- Event filtering in the TUI loop adds a match statement per event - negligible cost.
- Orchestration status in the system prompt adds ~500-1000 tokens per chat.submit in Executing state. This is within the existing token budget.
- No new Tokio tasks, no new broadcast channels.

### Security

- No new external interfaces. All communication is over the existing Unix socket.
- The chat LLM in Executing state gets read-only orchestration status. It cannot modify orchestration state directly - only through slash commands that the TUI dispatches as explicit IPC calls.

### Testing Strategy

- Unit tests for each phase as described in the Implementation Plan.
- Integration test: create a Plan via accept_plan, verify Coordinator starts, verify events flow to a mock TUI event receiver.
- Manual test: run the full flow end-to-end on a small real project - chat, /plan, /draft, /accept, observe orchestration in chat.

### Rollout Plan

Phase 1 is the critical path - it makes `/accept` actually work. Ship it and manually test. Phases 2 and 3 are additive UX improvements that can follow independently.

## Edge Cases

### Coordinator already running when /accept fires

`handle_agent_start` already guards against duplicate Coordinator sessions (handlers.rs:4323-4341). If a Coordinator is already running, agent.start returns an error. `accept_plan` should check the agent.start response and, if the Coordinator is already running, still succeed (the Goal and Plan are created regardless). Return `coordinator_already_running: true` in the response so the TUI can inform the user.

### Plan text is malformed or empty

The existing `accept_plan` handler validates that plan text is non-empty (handlers.rs:4098-4103). The Plan title is extracted from the first non-empty line, truncated to 120 chars, with a fallback to "Accepted Plan". This is sufficient. The Coordinator's decomposition loop will handle a weak Plan by iterating on it (that's what the RWL is for).

### User switches views during Executing state

The TUI view system is independent of funnel_state. The user can switch to Dashboard, Works, Agents views and back to Chat freely. The `funnel_state` persists on `app` state. Orchestration events should accumulate in `chat_history` even when the user is looking at another view, so they see them when they return to Chat.

### Coordinator reaches GoalComplete

When the Coordinator's FSM transitions to `GoalComplete`, it emits a `record.updated` for CoordinatorState. The event stream (Change 2) should detect this and insert a prominent system message: "All work complete. Plan execution finished." The `funnel_state` could auto-transition from Executing back to Chat, or stay in Executing as a read-only log. Proposed: stay in Executing but re-enable the `/plan` command so the user can start a new cycle.

### User sends a chat message while orchestration is in progress

This is the normal case for Change 3. The chat.submit runs independently of the Coordinator. The chat LLM gets orchestration status in its system prompt but has no tools that modify orchestration state. It can only read and report. The user's chat session and the Coordinator's agent session are separate Tokio tasks with separate LLM clients.

### Multiple /accept calls

Second call fails because CoordinatorGoal already active (only one allowed) and Coordinator is already running (agent.start guard). Return a clear error: "Orchestration already in progress."

### Daemon restart during execution

The Coordinator session is lost but CoordinatorGoal, CoordinatorState, Plan, and all Work/Bundle records persist in TaskStore. On daemon restart, if `auto_start_coordinator` is true, the Coordinator resumes from its persisted state. The TUI reconnects and should detect the running orchestration and restore `funnel_state = Executing` if a chat session with a goal_id exists.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Coordinator fails to start after accept_plan | Low | High | accept_plan returns coordinator_session_id; TUI can check agent.status. Add error event if start fails. |
| Event flood overwhelms chat history | Medium | Low | Filter set is small (~13 event types). System messages are one-liners. Can add rate limiting if needed. |
| Chat LLM hallucinates orchestration state | Low | Medium | Orchestration status is injected as structured text, not generated. LLM just needs to relay it. |
| User sends conflicting interventions | Low | Low | /pause and /stop are idempotent. Multiple /stop calls are harmless. |
| Goal/State creation race with Coordinator startup | Low | Medium | accept_plan creates records synchronously before dispatching agent.start. Coordinator reads them on first iteration. |
| Daemon crash loses in-flight chat messages | Medium | Medium | ChatHistory is checkpointed per-iteration. At most one turn of chat is lost. Orchestration state is fully persisted. |

## Open Questions

- [ ] Should `/accept` keep the user on Chat view (proposed) or split-screen Chat + Dashboard?
- [ ] Should orchestration system messages be collapsible/hideable in the chat history?
- [ ] Should the Coordinator emit a new `coordinator.fsm_transition` event for FSM state changes, or is the existing `record.updated` for CoordinatorState sufficient?
- [ ] On daemon restart, should the TUI auto-detect a running orchestration and restore Executing state?

## References

- `docs/design/2026-02-25-orchestration-spine.md` - Layer 1 architecture
- `docs/design/2026-02-26-multi-level-rwl.md` - Coordinator, Researcher, Integrator
- `docs/design/2026-03-05-chat-agentic-tool-loop.md` - Chat with agentic loop
- `docs/design/2026-03-04-tui-chat-plan-funnel.md` - Chat funnel design
- `docs/design/2026-03-04-coordinator-accept-plan.md` - accept_plan handler
- `docs/design/2026-03-03-semantic-decomposition.md` - Coverage evaluator (Draft)
- `docs/design/remaining-gaps.md` - Other open items
