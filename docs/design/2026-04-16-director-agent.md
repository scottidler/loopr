# Design Document: Director Agent

**Author:** Scott A. Idler
**Date:** 2026-04-16
**Status:** Implemented
**Review Passes Completed:** 5/5 + Architect round 1 (broadcast semantics, pattern-tracker persistence, state-query registration)
**Implementation:** v4 branch, commits `54b77fa` (Phase 1), `d68427c` (Phase 2), `82767be` (Phase 3), `f8ba895` (Phase 4), `1dba9c4` (Phase 5), `1ce214d` (Phase 6), `5247ec6` (Phase 7)

## Summary

The Director is a long-lived Opus-class agent that owns all judgment in the Loopr system. It replaces the v3 Coordinator's intelligence while the engine handles mechanics. The Director activates when the user invokes `/plan`, conducts the interview, shapes the plan, then transitions to monitoring mode where it watches all system signals via the broadcast channel and intervenes when the engine's mechanical strategies cannot resolve a problem. This is the final piece required to complete the v4 vision: YAML-composable engine for mechanics, Director for judgment.

## Problem Statement

### Background

The v4 cutover (2026-04-15) deleted the Coordinator (1,244 LOC), Integrator (1,935 LOC), and Supervisor. Their mechanical scheduling was replaced by 11 YAML strategy files and 59 primitives. The engine now handles: hierarchy promotion, agent spawning, bundle triage/review, integration, completion detection, retry/abandon, and sweeps.

What was NOT replaced: the Coordinator's judgment. The Coordinator was long-lived - it ran for the duration of a plan's execution, watching state, diagnosing stuck situations, revising decomposition, and making decisions that no mechanical rule could encode. A stub `DirectorAgent` was scaffolded during the cutover but it is non-functional: PlanIntake is a no-op, Escalation makes a single LLM call and discards the result, monitoring mode does not exist.

### Problem

1. **No chat-to-plan bridge.** The chat funnel (Chat -> Interview -> PlanDraft -> Executing) is driven by prompt selection, not an agent with tools. The LLM drafting the plan cannot read the target repo, query prior Learnings, or understand codebase state. Plans are produced ungrounded.

2. **No persistent observer.** When a plan is executing, nobody watches the whole board. The engine fires strategies reactively when individual triggers match, but no agent synthesizes cross-cutting signals: "Work A and Work C both failed on the same missing dependency" or "three consecutive bundles were rejected for the same reason."

3. **No escalation intelligence.** The `escalate-to-director` strategy spawns a Director, but the Director cannot diagnose anything - it has no tool access, no state query capability, and no action vocabulary beyond a single LLM call.

4. **No user intervention path.** When the user types during execution, the chat handler does not involve the Director. User intent ("focus on X first", "this isn't what I wanted") goes to a generic chat LLM with no ability to modify the plan hierarchy.

5. **Agents cannot receive events.** `AgentIpcBridge` is request/response only. No `broadcast::Receiver` exists in `AgentContext`. A long-lived Director has no way to receive push notifications from the daemon.

### Goals

- Director conducts grounded plan intake interviews using tools (read files, grep, glob, query learnings)
- Director runs for the lifetime of a plan's execution in monitoring mode
- Director receives all daemon events via broadcast channel subscription (push, not poll)
- Director diagnoses stuck states by synthesizing cross-cutting signals
- Director takes concrete corrective actions (revise work, re-decompose, abandon, spawn researcher, communicate with user)
- Director handles user intervention during execution
- Director has its own Lifeguard and cross-session failure pattern detection
- Director config is properly loaded (not borrowed from Researcher)
- All infrastructure changes are complete - nothing deferred

### Non-Goals

- Director does not write code (Implementer's job)
- Director does not review bundles (Reviewer's job)
- Director does not run the scheduling loop (engine's job)
- Director does not decompose plans (Decomposer's job, triggered by engine)
- Director does not replace the Chat agent for general-purpose conversation
- AutoResearch trial integration (already designed in Doc 7, orthogonal to Director)

## Proposed Solution

### Overview

The Director is a singleton, long-lived Opus agent with four operating modes arranged as a state machine:

```
              /plan
Chat agent ---------> Director (PlanIntake)
                          |
                      /accept
                          |
                          v
                   Director (Monitoring) <---> Director (Escalation)
                          ^
                          |
                   user chat during
                     execution
                          |
                          v
                   Director (UserIntervention) --> Director (Monitoring)
```

The Director's run loop is event-driven, matching the engine's pattern: `tokio::select!` on `broadcast::Receiver` for push events and a heartbeat timer for periodic health checks.

### Architecture

#### Signal Delivery: Broadcast Channel Subscription

Add an optional `broadcast::Receiver<DaemonEvent>` to `AgentContext`:

```rust
pub struct AgentContext {
    // ... existing fields ...
    /// Optional event subscription for long-lived agents (Director).
    /// Short-lived agents (Implementer, Reviewer) leave this as None.
    pub event_rx: Option<broadcast::Receiver<DaemonEvent>>,
}
```

Wire it in `AgentContext::from_session_id` - subscribe when the agent kind is `Director`:

```rust
let event_rx = if session.agent_type == AgentKind::Director {
    Some(event_tx.subscribe())
} else {
    None
};
```

This is the same pattern `run_engine` uses. The Director's run loop becomes:

```rust
loop {
    tokio::select! {
        event = self.ctx.event_rx.as_mut().unwrap().recv() => {
            match event {
                Ok(ev) => self.process_event(ev).await?,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("director event stream lagged by {} events; reconciling from IPC", n);
                    self.reconcile_from_ipc().await?;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
        msg = self.user_message_rx.recv() => {
            if let Some(message) = msg {
                self.handle_user_message(message).await?;
            }
        }
        _ = tokio::time::sleep(HEARTBEAT_INTERVAL) => {
            self.heartbeat().await?;
        }
    }
    if self.is_plan_terminal() || self.ctx.is_cancelled() {
        break;
    }
}
```

The `user_message_rx` arm serves double duty: during PlanIntake it delivers follow-up conversation messages; during Monitoring/Executing it delivers user intervention messages. The same channel, different handling based on `self.mode`.

The `RecvError::Lagged(n)` arm is mandatory, not optional. `tokio::sync::broadcast` is a fixed-capacity circular buffer (256 events in `src/daemon/context.rs`); if the sender outpaces the receiver by more than capacity, the oldest events are **permanently overwritten** - they are not delivered, not buffered, not recoverable from the channel. The Director must treat any `Lagged` as a signal that its in-memory state is out of sync and re-derive from persistent state via IPC. See `State Reconciliation` below.

#### State Reconciliation

Every counter the Director's pattern tracker holds is derived from persistent domain state - there is no independent source of truth for Director state. `reconcile_from_ipc()` rebuilds the tracker from scratch by querying `Stores` through `AgentIpcBridge`:

```rust
async fn reconcile_from_ipc(&mut self) -> Result<()> {
    let plan_id = self.plan_id.as_ref().expect("monitoring requires plan_id");

    self.pattern_tracker.work_failure_history.clear();
    for work in self.bridge.work_list(plan_id).await? {
        if work.session_failure_count > 0 {
            self.pattern_tracker.observe_work(&work);
        }
    }

    self.pattern_tracker.rejection_history.clear();
    for bundle in self.bridge.bundle_list(plan_id).await? {
        if bundle.is_rejected() {
            self.pattern_tracker.observe_rejection(&bundle);
        }
    }

    self.pattern_tracker.spec_revision_count = self
        .bridge
        .spec_list(plan_id)
        .await?
        .into_iter()
        .map(|s| (s.id, s.revision_count))
        .collect();

    Ok(())
}
```

Reconciliation is invoked from three places:
1. **Director startup** (cold start or supervision restart): first action after entering Monitoring mode, so a freshly spawned Director inherits the full observation history.
2. **Broadcast lag**: any `RecvError::Lagged` triggers it - the tracker is brought back to ground truth before the next event is processed.
3. **Heartbeat (optional)**: periodic reconciliation as a defense-in-depth backstop. Default off; enable if cross-checking shows drift.

This preserves the JSONL-is-truth invariant (from `.claude/rules/taskstore.md`): `Work.session_failure_count`, `Bundle` rejection records, and `Spec.revision_count` are the persistent truth; the tracker is an in-memory read cache over them. No new domain type, no `director-state.jsonl`, no new `Stores` registration is required.

#### Director Modes

**PlanIntake**: activated when `/plan` is invoked. Receives chat context (message history) and an `mpsc::Receiver<String>` for user messages. The conversation loop works like this:

1. Director makes an initial LLM turn on startup (analyzing the chat context, asking clarifying questions or proposing a draft). This turn uses `run_tool_loop` which handles multi-step tool use within a single turn (e.g., LLM decides to read 3 files before responding).
2. After the LLM turn completes, the Director returns to the `select!` loop and waits for: a new user message, a broadcast event, or the heartbeat timer.
3. When a user message arrives via mpsc, the Director appends it to the message history and calls `run_tool_loop` again for the next turn.
4. When `doc.plan_accepted` arrives on the broadcast channel, the Director stores the `plan_id` from the event payload and transitions to Monitoring.

Between LLM turns, broadcast events buffer in `event_rx` (capacity 256). After each `run_tool_loop` call, the Director drains the event backlog. This means the Director can detect `/accept` even if the user invokes it during an LLM call - it processes the event on the next loop iteration.

**Monitoring**: entered after `/accept` fires `doc.accept`. The Director's run loop watches the broadcast event stream. Most events are informational (logged, tallied). Specific event patterns trigger investigation:

| Signal | Director Response |
|--------|------------------|
| `agent.status_changed` (Failed) | Check failure reason; if pattern detected (same error across sessions), escalate |
| `reconciliation.failed` (catastrophic) | Immediate investigation via state queries |
| `transition.rejected` | Log; if repeated for same record, investigate |
| `bundle.rejected_stale` | Log; if frequent, check for integration bottleneck |
| `agent.staleness_detected` | Check if stale agent is blocking progress |
| Multiple works abandoned for same parent | Bubble-up: parent needs revision |
| `tick.validation_failed` | Check validation log; diagnose environment issue |
| No progress for N seconds | Heartbeat detects stall; investigate |

In monitoring mode, the Director does NOT make LLM calls for every event. It maintains in-memory tallies and pattern counters. LLM calls happen only when the Director needs judgment: diagnosing a complex failure, deciding whether to revise a spec, or composing a message to the user.

**Escalation**: entered from Monitoring when a pattern requires judgment. The Director builds a context snapshot (plan hierarchy state, recent failures, agent session logs, bundle rejection reasons, learning contradictions), calls the LLM with the escalation prompt, and executes the LLM's recommended actions via IPC. After taking action, returns to Monitoring.

**UserIntervention**: entered from Monitoring when the user sends a chat message while `FunnelState::Executing`. The Director receives the user message (forwarded from the chat handler via a `director.user_message` event), builds execution context, calls the LLM with the intervention prompt, and translates user intent into plan modifications. Returns to Monitoring.

#### Chat-to-Director Handoff

The `/plan` command in the TUI triggers the handoff:

1. TUI transitions `FunnelState` from `Chat` to `Interview`
2. TUI sends a new IPC message: `director.start_plan_intake` with `{ chat_session_id, messages }`
3. Handler creates a Director `AgentSession` with `mode: plan-intake`
4. Handler copies the chat message history into the Director's context (stored in `ChatHistory`)
5. Handler creates an `mpsc::channel<String>` pair; stores the `Sender` in `Stores.director_message_tx` keyed by session_id; passes the `Receiver` to the Director agent
6. Handler spawns the Director agent via `run_agent_task`
7. Director enters PlanIntake mode. It sends an initial LLM turn (analyzing the chat context and responding with clarifying questions or a draft plan). Output streams to TUI via `agent.llm_output` events.
8. User continues conversation with the Director (not the Chat agent). Subsequent user messages are sent via `director.user_message` IPC, which writes to the mpsc channel.
9. Director receives each user message, appends to message history, calls LLM again with updated context and tools
10. When user invokes `/accept`:
    - TUI sends `doc.accept` with the plan markdown
    - `doc.accept` handler creates the Plan, emits `doc.plan_accepted`
    - Director receives `doc.plan_accepted` event on its broadcast receiver
    - Director transitions from PlanIntake to Monitoring mode

The Chat agent's session is concluded when `/plan` fires. The Director takes over the conversation. The TUI renders Director output in the same chat view - the user sees a seamless transition but the model behind it upgrades to Opus.

**Session timeout handling:** The Director's session timeout (1hr default from config) applies only to PlanIntake mode. When the Director transitions to Monitoring, the timeout is effectively disabled - the Director lives for the plan's lifetime. The run loop checks `is_plan_terminal()` on each iteration instead of relying on external timeout. The supervision strategies (`supervision.yml`) provide the safety net if the Director fails.

#### IPC and Tools for the Director

**During PlanIntake**: the Director uses the standard agentic tool loop (`run_tool_loop`) with these tools:
- `read-file`, `grep`, `glob`, `shell` (read the target repo, understand codebase state)
- IPC-backed tools: `learning.list` (query prior learnings), `plan.list` (query prior plans) - these are implemented as tool definitions that call the bridge internally, same pattern as chat delegate tools
- `agent.start` via bridge (spawn a Researcher for deep codebase exploration if needed)

**During Monitoring/Escalation/UserIntervention**: the Director uses IPC via `AgentIpcBridge`:
- Read state: `plan.get`, `spec.list`, `phase.list`, `work.list`, `bundle.list`, `agent.list`, `learning.list`, `tick.list`
- Modify state: `work.transition`, `spec.transition`, `phase.transition`, `work.update` (revise AC/description), `spec.update`
- Spawn agents: `agent.start` (spawn Researcher for investigation)
- Communicate: emit `director.user_message` events that the TUI renders in the chat view

New IPC methods required:

| Method | Purpose |
|--------|---------|
| `director.start_plan_intake` | Handoff from Chat to Director with message history |
| `director.user_message` | Forward user chat message to running Director during execution |

New event types:

| Event | Purpose |
|-------|---------|
| `director.status` | Director mode changes (plan-intake, monitoring, escalation, intervention) |
| `director.diagnosis` | Director's analysis of a stuck state (for TUI display) |
| `director.action` | Director took a corrective action (for TUI display and audit trail) |

#### Lifeguard for the Director

The Director needs its own Lifeguard, but the detection modes differ from Implementer/Researcher:

**Within-session detection** (existing Lifeguard patterns):
- Repeated identical LLM calls (same escalation context submitted multiple times)
- Repeated failed IPC requests (trying to transition a record that won't move)
- Parse failures on LLM response

**Cross-session detection** (new capability, unique to Director):
The Director tracks cross-session patterns via an in-memory cache derived from persistent domain state:

```rust
struct DirectorPatternTracker {
    /// Work IDs that have failed across multiple implementer sessions.
    /// Derived from Work.session_failure_count + AgentSession failure logs.
    /// Key: work_id, Value: Vec<(session_id, failure_reason)>
    work_failure_history: HashMap<String, Vec<(String, String)>>,

    /// Bundle rejection patterns.
    /// Derived from rejected Bundle records in Stores.
    /// Key: work_id, Value: Vec<rejection_reason>
    rejection_history: HashMap<String, Vec<String>>,

    /// Specs that have been revised via bubble-up.
    /// Mirrors Spec.revision_count from persistent state.
    /// Key: spec_id, Value: revision_count
    spec_revision_count: HashMap<String, u32>,
}
```

The tracker is maintained in two ways:
1. **Event-driven updates**: on relevant broadcast events (`agent.status_changed(Failed)`, `bundle.rejected`, `spec.revised`), the tracker updates the affected entry incrementally.
2. **IPC reconciliation**: on Director startup and on any `RecvError::Lagged`, the tracker is rebuilt from scratch via `reconcile_from_ipc()` (see State Reconciliation).

Because every counter is derived from already-persistent state, the tracker is an in-memory performance cache - not a separate source of truth. No `director-state.jsonl` file, no new `Record` type, no additional `Stores` registration. When the Director restarts (supervision-driven or daemon restart), the new Director's first action is `reconcile_from_ipc()`, which rebuilds the full history from Work/Bundle/Spec records.

When the Director detects a cross-session pattern (same work failing 3+ times with similar errors), it escalates to full LLM diagnosis rather than letting the engine retry mechanically.

**Director Lifeguard escalation**: when the Director's own Lifeguard fires, there is no higher agent to escalate to. Instead:
- Emit a `director.lifeguard` event
- Log the escalation pattern with full context
- If the supervision strategy detects a failed Director, it restarts with a fresh session (existing `supervision.yml` handles this)
- After `max_restarts` (5), emit `director-max-restarts-exceeded` escalation - this is a terminal failure that requires user intervention

**Supervision guard**: the existing `supervision.yml` strategies (`restart-director-on-event`, `restart-director-on-state`) need a guard added: `has-active-plan`. Without it, the level-triggered fallback would try to spawn a Director even when no plan is executing. Implementing this guard is not a YAML-only change; it requires two code changes in the Rust engine: (1) register a new state query `has-active-plan` in the state query registry - the query returns true iff any Plan record is in an active (non-terminal) status (`Draft`/`Accepted`/`Executing`, excluding `Complete`/`Abandoned`), and (2) register the corresponding guard condition that references the new state query so strategies can name it. Once registered, `supervision.yml` can reference `has-active-plan` by name. This work lands in Phase 1.

#### Config Integration

Add `director: AgentRoleConfig` to `AgentConfig`:

```rust
pub struct AgentConfig {
    // ... existing fields ...
    pub director: AgentRoleConfig,
}
```

With default:

```rust
director: AgentRoleConfig {
    model: "claude-opus-4-6".to_string(),
    max_tokens: 16384,
    temperature: 0.3,
    max_pool: 1,
    session_timeout_secs: Some(3600),
    ..Default::default()
}
```

Load from `resources/roles/director.yml` during config initialization. Remove the hack in `lifecycle.rs:466` that borrows Researcher's config.

#### Bubble-up Counter

Already resolved: `Plan.bubble_up_count` exists (`src/domain/plan.rs:94`). The `increment-bubble-up` primitive in `mutation.rs` increments it. The `feedback.yml` strategy checks the threshold. No new storage needed - the Director reads `plan.bubble_up_count` via IPC when evaluating whether to escalate vs. attempt another revision.

#### FunnelState Update

Update `FunnelState` and `ChatHistory` to reflect the Director handoff:

```rust
pub enum FunnelState {
    Chat,
    Interview,    // Now means: Director is conducting the interview
    PlanDraft,    // Director has proposed a plan, awaiting /accept
    Executing,    // Plan accepted, Director in monitoring mode
}
```

Update `ChatHistory`:
- Rename `goal_id` field to `plan_id` (the coordinator goal concept is gone)
- Add `director_session_id: Option<String>` to track the active Director session
- Remove the stale comment referencing "coordinator goal_id"

### Data Model

No new domain types. Changes to existing types:

**AgentContext** - add `event_rx: Option<broadcast::Receiver<DaemonEvent>>`, add `user_message_rx: Option<mpsc::Receiver<String>>`

**Stores** - add `director_message_tx: RwLock<HashMap<String, mpsc::Sender<String>>>` for forwarding user messages to running Director sessions

**AgentConfig** - add `director: AgentRoleConfig`

**ChatHistory** - rename `goal_id` to `plan_id`, add `director_session_id: Option<String>`, remove stale coordinator comment

**AgentSession** - add `director_mode: Option<DirectorMode>` for observability (which mode the Director is operating in). The Director's `target_id` field (already on AgentSession) stores the `plan_id` being monitored, set when `doc.plan_accepted` is received.

**DirectorMode** (existing enum in `director.rs`) - expand:

```rust
enum DirectorMode {
    PlanIntake,
    Monitoring,
    Escalation,
    UserIntervention,
}
```

### API Design

New IPC methods:

```
director.start_plan_intake
  params: { chat_session_id: String, messages: Vec<Message> }
  returns: { session_id: String, status: "Running" }

director.user_message
  params: { session_id: String, message: String }
  returns: { status: "Received" }
  Note: also emits a director.user_message event on the broadcast channel
```

New events:

```
director.mode_changed    { session_id, mode: "monitoring" | "escalation" | "intervention" | "plan-intake" }
director.diagnosis       { session_id, target_id, diagnosis: String, recommended_actions: Vec<String> }
director.action_taken    { session_id, action: String, target_id: String, result: String }
director.user_message    { session_id, message: String }
```

### Implementation Plan

#### Phase 1: Infrastructure - broadcast receiver, mpsc channel, and config
**Model:** sonnet

- Add `event_rx: Option<broadcast::Receiver<DaemonEvent>>` to `AgentContext`
- Add `user_message_rx: Option<mpsc::Receiver<String>>` to `AgentContext`
- Wire broadcast subscription in `AgentContext::from_session_id` for Director kind
- Add `director_message_tx: RwLock<HashMap<String, mpsc::Sender<String>>>` to `Stores`
- Add `director: AgentRoleConfig` to `AgentConfig` with proper defaults
- Load Director config from `resources/roles/director.yml`
- Remove Researcher config borrowing hack in `lifecycle.rs`
- Update `DirectorMode` enum to include Monitoring, Escalation, UserIntervention
- Add `director_mode` field to `AgentSession`
- Clean up `ChatHistory`: rename `goal_id` to `plan_id`, add `director_session_id`, remove stale comment
- Register new state query `has-active-plan` in the Rust state query registry (returns true iff any Plan is in a non-terminal status); register the corresponding guard condition; update `restart-director-on-event` and `restart-director-on-state` in `supervision.yml` to include the new guard
- Tests: verify broadcast receiver works in AgentContext, verify config loading, verify `has-active-plan` query returns correct boolean for each Plan status
- `otto ci`

#### Phase 2: Director event-driven run loop
**Model:** opus

- Rewrite `DirectorAgent::run()` as a long-lived event-driven loop
- Implement `tokio::select!` on event_rx + heartbeat timer
- Implement event classification: which events require attention vs. are informational
- Implement in-memory pattern tracking (`DirectorPatternTracker`)
- Implement `heartbeat()`: check for stalls (no progress for configurable interval), check plan terminal state
- Implement `is_plan_terminal()`: query plan status via bridge, break loop when Complete/Abandoned
- Add Lifeguard to Director with appropriate thresholds
- Tests: verify event-driven wakeup, verify heartbeat fires, verify terminal plan exits loop
- `otto ci`

#### Phase 3: PlanIntake mode and Chat-to-Director handoff
**Model:** opus

- Implement `director.start_plan_intake` IPC handler:
  - Creates Director AgentSession with `mode: plan-intake`
  - Copies chat message history from ChatHistory into the Director's initial context
  - Creates `mpsc::channel<String>` (capacity 16); stores Sender in `Stores.director_message_tx`; passes Receiver to Director
  - Spawns Director via `run_agent_task`
- Implement `director.user_message` IPC handler (shared with Phase 6):
  - Looks up `director_message_tx` in Stores by Director session_id
  - Sends user message string via the mpsc channel
  - Returns immediately
- Implement Director PlanIntake conversation loop:
  - On startup: build initial LLM context from chat message history + Director interview system prompt
  - Call LLM with tool definitions (read-file, grep, glob, shell); stream response via `agent.llm_output`
  - After each LLM response, wait on the select! loop for the next user message or broadcast event
  - On user message: append to message history, call LLM again with updated context
  - On `doc.plan_accepted` event: transition to Monitoring mode
- Update TUI: `/plan` sends `director.start_plan_intake` instead of just changing FunnelState
- Update TUI: route subsequent user messages to `director.user_message` instead of `chat.submit` when Director is active
- Render Director output in the same chat view via `agent.llm_output` events (already works for Chat)
- Tests: verify PlanIntake tool access, verify multi-turn conversation, verify transition to Monitoring on plan_accepted
- `otto ci`

#### Phase 4: Monitoring mode
**Model:** opus

- Implement `process_event()` dispatch: classify events and update pattern tracker
- Implement signal-to-investigation mapping (table from Architecture section)
- Implement stall detection in heartbeat: query active work count, check for progress since last heartbeat
- Implement cross-session failure detection: when `agent.status_changed(Failed)` arrives, look up work_id, check failure_history, if pattern detected -> enter Escalation
- Emit `director.mode_changed` events on mode transitions
- Tests: verify pattern detection (3 failures on same work triggers escalation), verify stall detection
- `otto ci`

#### Phase 5: Escalation mode
**Model:** opus

- Implement `enter_escalation()`: build context snapshot from Stores (plan hierarchy, recent failures, agent logs, rejection reasons, learnings)
- Implement LLM call with escalation prompt and structured context
- Implement action parsing: LLM returns structured JSON actions (revise-work, re-decompose, abandon, spawn-researcher, message-user)
- Implement action execution via IPC bridge:
  - `revise-work`: update work AC/description via `work.update`, transition back to Pending
  - `re-decompose`: transition parent spec/phase to Draft, engine triggers re-decomposition
  - `abandon-work`: transition via `work.transition`
  - `spawn-researcher`: create Researcher session via `agent.start`
  - `message-user`: emit `director.diagnosis` event for TUI display
- Emit `director.action_taken` events for audit trail
- After taking action, return to Monitoring
- Tests: verify escalation context building, verify action execution, verify return to Monitoring
- `otto ci`

#### Phase 6: User Intervention mode
**Model:** opus

- The mpsc channel and `director.user_message` IPC handler already exist from Phase 3
- Implement UserIntervention handling in the Director's `handle_user_message()`:
  - When in Monitoring mode and a user message arrives, enter UserIntervention mode
  - Build execution context: current plan hierarchy state, active works, recent events, recent failures
  - Call LLM with the intervention system prompt + user message + execution context
  - LLM translates user intent to actions (same action vocabulary as Escalation: revise-work, re-decompose, abandon-work, spawn-researcher, message-user)
  - Execute actions via IPC bridge
  - Stream response to TUI via `agent.llm_output` (user sees the Director's interpretation)
  - Return to Monitoring
- Update `chat.submit` handler: when `FunnelState::Executing` and a Director session is active, route the message via `director.user_message` instead of spawning a new chat loop
- Tests: verify user message forwarding during execution, verify intent translation, verify return to Monitoring
- `otto ci`

#### Phase 7: Cross-session pattern detection
**Model:** sonnet

- Extend `DirectorPatternTracker` with configurable thresholds (from Director config)
- Implement work failure correlation: group failures by error signature (hash of error message), detect when N implementations of the same work fail with the same root cause
- Implement bundle rejection correlation: detect when the same reviewer feedback appears across multiple bundles for the same work
- Implement spec-level failure detection: when >M% of a spec's works are abandoned, flag the spec for revision before the engine's bubble-up threshold fires
- Implement `reconcile_from_ipc()` as specified in the State Reconciliation section; wire it into the run loop (startup + `RecvError::Lagged` arm, both from Phase 2)
- No new domain type, no `director-state.jsonl`, no new `Stores` registration - the tracker is derived from persistent `Work.session_failure_count`, `Bundle` rejection records, and `Spec.revision_count`
- Tests: verify pattern detection across simulated session failures; verify reconciliation rebuilds the tracker identically whether populated event-by-event or rebuilt from scratch via IPC (property test: observe → clear → reconcile → equal)
- `otto ci`

#### Phase 8: Integration tests and cleanup
**Model:** sonnet

- E2E test: full pipeline from Chat -> /plan -> Director interview -> /accept -> Monitoring -> GoalComplete
- E2E test: inject a plan that requires escalation (broken AC), verify Director detects stuck state and takes corrective action
- E2E test: user intervention during execution, verify Director translates intent
- Clean up dead code from the cutover (any remaining coordinator references in comments, stale test fixtures)
- Verify all supervision strategies work with the new Director lifecycle (restart-on-failure, restart-on-state)
- Update CLAUDE.md codebase map to reflect Director architecture
- `otto ci`

## Alternatives Considered

### Alternative 1: Spawn-per-event Director (reactive, not persistent)
- **Description:** Director spawns for each escalation/intervention event, runs once, terminates. No monitoring mode.
- **Pros:** Simple lifecycle, no long-lived state, no broadcast subscription needed
- **Cons:** Cannot detect cross-cutting patterns (each spawn has no memory of prior events). Cannot detect stalls (nobody watching). This is what the current stub does and it's insufficient.
- **Why not chosen:** The whole point is persistent observation. The v3 Coordinator was long-lived for a reason - cross-cutting pattern detection requires memory across events.

### Alternative 2: Engine-only (no Director agent)
- **Description:** Extend the engine with more sophisticated trigger conditions that detect patterns, stalls, and user intent.
- **Pros:** No new agent, all orchestration in one system
- **Cons:** Violates the v4 design principle: "strategies are single-tick." Pattern detection across time requires state accumulation, which is judgment, not mechanics. You would end up building a Coordinator inside the engine - Greenspun's tenth rule applied to orchestration.
- **Why not chosen:** The engine/Director split is the correct architectural boundary. Engine handles reactive mechanics, Director handles temporal judgment.

### Alternative 3: External observer process
- **Description:** Director runs as a separate OS process connected via Unix socket, not an in-process agent.
- **Pros:** Full process isolation, can use different runtime
- **Cons:** Loses access to in-process Stores, requires IPC for every state query (latency), complicates deployment (two binaries), violates the "light loops, heavy tools" principle (agents are tokio tasks, not processes)
- **Why not chosen:** The existing agent infrastructure (AgentContext, AgentIpcBridge, event_tx) is perfectly suited. Adding a broadcast::Receiver is minimal infrastructure change.

## Technical Considerations

### Dependencies

**Internal:**
- `AgentContext` (add event_rx field)
- `AgentConfig` (add director field)
- `ChatHistory` (rename goal_id, add director_session_id)
- `AgentSession` (add director_mode)
- `run_tool_loop` (reuse for PlanIntake agentic loop)
- `AgentIpcBridge` (reuse for Monitoring/Escalation state queries and actions)

**External:**
- `tokio::sync::broadcast` (already used everywhere)
- `tokio::sync::mpsc` (for user message forwarding to Director)
- No new crate dependencies

### Performance

- Director LLM calls are Opus (expensive but infrequent). During monitoring, the Director processes events without LLM calls. LLM is only invoked for: plan intake interview turns, escalation diagnosis, user intervention translation.
- Broadcast channel capacity is 256 events. During high-activity periods (e.g., a 30-60s Opus LLM call while implementers churn), the Director's receiver can lag beyond capacity. `tokio::sync::broadcast` is a fixed-capacity circular buffer: when the sender outpaces a receiver by more than capacity, the oldest events are **permanently overwritten** and the next `recv()` returns `RecvError::Lagged(n)` naming the count of lost events. Lagged events are not recoverable from the channel. The Director handles this by running `reconcile_from_ipc()` (see State Reconciliation) whenever `Lagged` is observed - rebuilding the pattern tracker from persistent `Work`/`Bundle`/`Spec` state. The broadcast channel is a low-latency push optimization; the IPC-backed reconciliation is the correctness guarantee.
- In-memory pattern tracker is O(work_count) space. For typical plans (10-50 works), this is negligible.

### Security

- Director has full read/write authority over the plan hierarchy via IPC. This is intentional - it's the top-level agent.
- Director's shell tool access during PlanIntake is sandboxed to the target repo (same sandbox as Chat agent).
- Director cannot access other plans' data - scoped to the active plan.

### Testing Strategy

- Unit tests: event classification, pattern detection, mode transitions, heartbeat stall detection
- Integration tests: full lifecycle (PlanIntake -> Monitoring -> Escalation -> Monitoring -> Complete)
- E2E tests: real LLM calls against disposable target repos (lua-todo, rust-version)
- Lifeguard tests: verify Director's own Lifeguard detects loops and the supervision strategy restarts it

### Rollout Plan

All 8 phases are sequential and each phase produces a compilable, testable increment. Phase 1 (infrastructure) unblocks all other phases. Phases 2-6 are the core Director implementation. Phase 7 (cross-session patterns) extends monitoring intelligence. Phase 8 (integration tests) validates the full pipeline.

## Edge Cases

**User never invokes `/accept`**: Director stays in PlanIntake. The 1hr session timeout applies to PlanIntake mode and terminates the Director cleanly. No active plan exists, so the supervision strategy does NOT restart. User can `/plan` again.

**Daemon crash during Monitoring**: Director session is lost. On restart, JSONL replay restores the plan hierarchy. The level-triggered supervision strategy (`restart-director-on-state`) detects no-running-director + has-active-plan and spawns a new Director. The new Director enters Monitoring mode directly (no PlanIntake needed - the plan already exists). The pattern tracker is rebuilt from persistent `Work`/`Bundle`/`Spec` state via `reconcile_from_ipc()` as the first step of entering Monitoring - no separate Director state file is needed.

**Broadcast lag during escalation or long LLM call**: The Director is blocked in a 30-60s Opus call while the daemon emits more than 256 events. Oldest events overflow the ring buffer and are destroyed. On the next `recv()` the Director observes `RecvError::Lagged(n)`, logs the loss count, and runs `reconcile_from_ipc()` before processing any further events. After reconciliation, the tracker reflects ground truth (Work session_failure_count, Bundle rejection history, Spec revision_count) even though the intervening events were not individually observed. Missed pattern detection thresholds are caught on the next event that would have triggered them, or on the next reconciliation.

**Concurrent plans**: Not supported. `max_pool=1` means one Director at a time, which means one active plan at a time. This is consistent with the v3 architecture (one Coordinator per session). Multi-plan support is a future consideration, not a v4 goal.

**LLM call blocks event processing during Escalation**: When the Director is in an escalation LLM call (potentially 30-60s), broadcast events accumulate in `event_rx`. If fewer than 256 events arrive before the call completes, they are drained in order on return to the `select!` loop. If more arrive, `RecvError::Lagged(n)` fires and `reconcile_from_ipc()` restores the tracker from persistent state. Heartbeat may fire during the LLM call, but `heartbeat()` checks `self.mode == DirectorMode::Escalation` and skips stall detection since the Director is actively working.

**Director's corrective action makes things worse**: If the Director revises a work item and the revision also fails, the pattern tracker detects the growing failure history. After the threshold (configurable, default 3 failures of the same work including the revised version), the Director escalates to the user with a `director.diagnosis` event explaining what it tried and why it failed. The Director does not retry the same corrective action twice - the Lifeguard's action dedup catches this.

**User sends a message while Director is in Escalation mode**: The mpsc message buffers. When the escalation LLM call completes and the Director returns to Monitoring, it drains the mpsc channel and enters UserIntervention for the queued message. Messages are FIFO, no loss.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Director LLM calls during escalation produce invalid actions | Med | Med | Structured JSON action format with validation; Lifeguard catches repeated invalid actions |
| Broadcast channel lag during Opus LLM calls permanently drops events (`tokio::sync::broadcast` is a circular buffer; lagged events are lost from the stream, not queued) | High | Med | Explicit `RecvError::Lagged(n)` arm in the run loop triggers `reconcile_from_ipc()`; every counter the tracker holds is derived from persistent `Work`/`Bundle`/`Spec` state, so reconciliation restores ground truth. The broadcast channel is a latency optimization; IPC reconciliation is the correctness guarantee |
| Director run loop blocks on LLM call, misses events during that time | Med | Low | LLM calls are async; event_rx buffers up to 256 events during the call; drain backlog after |
| Director and engine both try to act on same record simultaneously | Med | Med | Director uses same IPC/FSM validation as all other agents; FSM guards prevent invalid transitions |
| User message forwarding races with Director mode transitions | Low | Low | mpsc channel is buffered; Director drains user messages in each loop iteration regardless of mode |
| Pattern tracker state lost on Director restart | Med | Med | Phase 7 persists tracker to JSONL; supervision strategy restarts with state recovery |
| Director's Lifeguard fires during a legitimate long-running escalation | Low | Med | Director's Lifeguard thresholds are higher than Implementer's (10 consecutive vs. 5); escalation timeout is separate from Lifeguard |

## Open Questions

None. Every aspect required to make the v4 vision reality is specified in this document.

## References

- `docs/v4-vision.md` - v4 YAML-composable engine vision
- `docs/design/2026-04-15-v4-full-cutover.md` - v4 cutover that deleted Coordinator/Integrator/Supervisor
- `docs/design/2026-04-11-strategy-composition.md` - composition engine design
- `docs/design/2026-04-11-trigger-guard-system.md` - trigger and guard system
- `docs/design/2026-04-09-reactive-execution-model.md` - reactive reconciliation model
- `docs/design/2026-03-05-chat-agentic-tool-loop.md` - chat agentic loop (reused for PlanIntake)
- `docs/design/2026-02-26-multi-level-rwl.md` - original Coordinator/Integrator/Researcher agent design
- `src/agents/director.rs` - current stub implementation
- `src/agents/lifeguard.rs` - per-session circuit breaker
- `src/daemon.rs:237-424` - engine tick loop (pattern for Director's event-driven loop)
- `resources/engine/strategies/recovery.yml` - escalation strategies
- `resources/engine/strategies/supervision.yml` - Director restart strategies
