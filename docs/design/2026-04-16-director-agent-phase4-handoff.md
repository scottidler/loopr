# Director Agent - Phase 4+ Handoff Notes

**Author:** Scott A. Idler (via Claude)
**Date:** 2026-04-16
**Status:** Handoff - Phases 1-3 Implemented, Phases 4-8 Pending

## Context

The parent design doc is `docs/design/2026-04-16-director-agent.md`. It was reviewed
by the Architect (Gemini) once and has three architect-raised must-fixes applied
(RecvError::Lagged handling, no `director-state.jsonl` persistence, `has-active-plan`
state query implemented in Rust). It is authoritative for all design decisions.

This file exists to hand off cleanly to the next session. The previous session stopped
at the explicit request of the user ("stop after completing phase 3") after Phase 3
shipped. Phases 4-8 remain to be implemented on top of the infrastructure already
in place.

## What's Shipped (Phases 1-3)

Each phase is a separate commit on branch `v4`:

| Phase | Commit | Summary |
|-------|--------|---------|
| 1 | `feat(director): phase 1 - infrastructure for director agent` | AgentContext.event_rx/user_message_rx, Stores.director_message_tx, AgentConfig.director, DirectorMode enum (4 variants), AgentSession.director_mode, ChatHistory plan_id rename, has-active-plan state query + guard |
| 2 | `feat(director): phase 2 - event-driven run loop` | DirectorAgent event-driven select! loop, DirectorPatternTracker (in-memory), heartbeat, is_plan_terminal, reconcile_from_ipc (stub), legacy_escalation preserved |
| 3 | `feat(director): phase 3 - PlanIntake handoff and chat routing` | director.start_plan_intake + director.user_message IPC handlers, Director PlanIntake conversation loop via run_tool_loop, chat.submit → director.user_message routing, TUI /plan → StartPlanIntake |

Total tests added across Phases 1-3: 26 (13 in Phase 1, 8 in Phase 2, 5 in Phase 3).
All pass. Three pre-existing failures on `v4` branch remain (tracked, not Phase 1-3
regressions):
- `agents::executor::action::work::tests::test_transition_role_inference`
- `agents::executor::action::work::tests::test_transition_role_inference_all_collections`
- `agents::implementer::tests::test_implementer_context_includes_goal`

## Architecture You're Inheriting

### Director as a long-lived event-driven agent

`src/agents/director.rs` holds the `DirectorAgent<L: LlmClient>`. It has four modes
(`DirectorMode`): `PlanIntake`, `Monitoring`, `Escalation`, `UserIntervention`. The
run loop is:

```rust
tokio::select! {
    event = event_rx.recv() => self.process_event(event).await?,
    Some(msg) = OptionFuture(user_message_rx.recv()) => self.handle_user_message(msg).await?,
    _ = tokio::time::sleep(1s_heartbeat) => self.heartbeat().await?,
}
```

`RecvError::Lagged(n)` triggers `reconcile_from_ipc()` which is currently a stub
that clears `pattern_tracker` - Phase 7 fills in the actual IPC queries.

Legacy one-shot Escalation (spawned with a non-`pl-` target_id) short-circuits the
event loop and keeps the v1 behavior. Phase 5 replaces this with a full Escalation
mode that enters from Monitoring via pattern detection.

### Chat-to-Director handoff

TUI `/plan` → `IpcAction::StartPlanIntake` → `director.start_plan_intake` handler →
creates Director session, mpsc channel, stashes Sender in `Stores.director_message_tx`,
stashes Receiver in `Stores.director_user_message_rx_pending`, spawns
`run_agent_task(Director)`. `AgentContext::from_session_id` pops the receiver when it
builds the context for the Director.

`chat.submit` detects `ChatHistory.director_session_id` is set and forwards the
message via the mpsc sender instead of spinning up a Chat agent loop.

### PlanIntake conversation

`handle_user_message` in PlanIntake mode calls `planintake_turn()`:
1. Resolves `chat_session_id` via reverse lookup on `ChatHistory.director_session_id`
2. Appends the user message to `ChatHistory.messages`
3. Lazily creates an `Arc<AgentLlmClient>` (`planintake_llm_client()`)
4. Calls `run_tool_loop(llm, ToolExecutor::standard, interview_prompt, messages, ...)`
5. Streams `agent.llm_output` events (handled by run_tool_loop via `event_tx`)
6. Checkpoints `ChatHistory.messages` per iteration and on completion

Tool loop uses `ToolExecutor::standard(&config.agents.tools)` - the standard tool
set available to all agents. No delegate subagents yet (chat uses
`chat_with_delegation`; Director uses plain `standard`). Phase 3 refinement candidate.

### Pattern tracker signals

`process_event` updates `DirectorPatternTracker` on:
- `doc.plan_accepted` - sets `plan_id`, transitions PlanIntake → Monitoring
- `agent.status_changed` (status `"failed"`) - appends to `work_failure_history` if
  the failed session has a work_id
- `bundle.rejected_stale` / `bundle.rejected` - appends to `rejection_history`

All other events log at trace. The tracker is purely observational in Phase 2 - no
escalation triggers yet.

## What's Next

### Phase 4: Monitoring mode (Opus)

Monitoring is *already* the mode Director enters after PlanIntake. What's missing is
the *intelligence*: pattern detection that triggers Escalation.

**Scope:**
- Implement signal-to-investigation mapping from the design doc (table in Architecture
  section). Key wiring points:
  - `process_event` already records failures/rejections into `pattern_tracker`
  - Add a post-event check: after each `process_event` returns, test whether
    pattern_tracker thresholds exceeded for any work_id. If so, transition to
    Escalation.
- Implement stall detection in `heartbeat`. Already partially there: the `last_event_at`
  tracking exists. When `elapsed > STALL_THRESHOLD_SECS` during Monitoring, decide
  what to do (for Phase 4: emit a `director.stall_detected` event; for Phase 5:
  transition to Escalation).
- Emit `director.mode_changed` events on mode transitions (currently `persist_mode()`
  writes to the session but doesn't emit). Add a broadcast emit alongside the persist.
- Tests:
  - 3 failures on same work_id with same error signature → triggers Escalation mode
  - No activity for STALL_THRESHOLD_SECS → stall event
  - `doc.plan_accepted` → Monitoring mode transition (already tested in Phase 2)

**Files to touch:**
- `src/agents/director.rs` - extend `process_event`, `heartbeat`, add pattern
  threshold checks
- `src/agents/director/tests.rs` - pattern threshold tests
- `src/ipc/protocol.rs` - add `director.mode_changed`, `director.stall_detected` event
  constructors

**Gotchas:**
- Phase 2's `process_event` matches `event.data.get("status")` as `"failed"` (lowercase).
  The architect verified this is correct: `AgentStatus` is `#[serde(rename_all = "lowercase")]`
  and `AgentEvent` is `#[serde(tag = "type", rename_all = "snake_case")]` so the
  status field is flat with value `"failed"`.
- Don't add LLM calls in Phase 4. Judgment (escalation diagnosis) is Phase 5. Phase 4
  is purely pattern-based mode transitions.

### Phase 5: Escalation mode (Opus)

Replace `legacy_escalation()` with full event-driven Escalation. Build a structured
context snapshot (plan hierarchy state, recent failures from pattern_tracker, bundle
rejections, learnings), call the LLM with a structured JSON action prompt, parse
actions, execute via `AgentIpcBridge`, return to Monitoring.

**Scope:**
- Define the structured action JSON format. Candidate:
  ```json
  {"actions":[{"type":"revise-work","work_id":"...","acceptance_criteria":[...]},
               {"type":"abandon-work","work_id":"..."},
               {"type":"spawn-researcher","scope":"...","query":"..."},
               {"type":"message-user","text":"..."}]}
  ```
- Implement `enter_escalation(target: EscalationTarget)` that builds the context
  snapshot. `EscalationTarget` identifies what triggered the escalation (a work_id
  from pattern detection, a stall, a user-level ambiguity, etc.).
- Action execution: use `self.ctx.bridge.request(method, params)` for each action:
  - `revise-work` → `work.update` + `work.transition` (to Pending)
  - `re-decompose` → `spec.transition` / `phase.transition` back to Draft
  - `abandon-work` → `work.transition` to Abandoned
  - `spawn-researcher` → `agent.start` with `agent_type: researcher`
  - `message-user` → emit `director.diagnosis` event (TUI renders)
- Return to Monitoring after all actions execute (keep mode as `Escalation` while
  running, flip back on completion).
- Phase 5 replaces `legacy_escalation()`. Keep the test fixture working:
  tests/director/tests.rs `legacy_escalation_short_circuits_event_loop` expects
  legacy behavior. Either update the test to the new behavior or keep the legacy
  path behind a feature flag for backward compat. Architect recommendation: update
  the test.

**Design doc table to look at:** The "Director Modes → Escalation" section has
the full context-snapshot spec. Also the "Edge Cases" section: "Director's corrective
action makes things worse" - the Lifeguard action dedup catches repeated invalid
actions.

**Files to touch:**
- `src/agents/director.rs` - rewrite `legacy_escalation()` into
  `enter_escalation()`, add `EscalationTarget`, action execution
- New: `src/agents/director/actions.rs` - structured action parsing + execution
- `src/agents/director/tests.rs` - update `legacy_escalation_*` test, add
  escalation-from-pattern-detection tests (needs MockLlm with scripted responses)

### Phase 6: UserIntervention mode

`handle_user_message` in Monitoring mode currently just stubs by transitioning to
UserIntervention, emitting a placeholder, and flipping back. Phase 6 wires the
actual intent-translation LLM call.

**Scope:**
- Similar to Phase 5's escalation handling but driven by user intent rather than
  pattern detection. System prompt should be different from the escalation prompt
  (focus on "translate what the user wants into concrete plan modifications").
- Same action vocabulary as Escalation (revise-work, abandon, re-prioritize,
  message-user). Share the action parser from Phase 5.
- Update `handle_user_message` in Monitoring mode to:
  1. Transition to UserIntervention, persist_mode, emit mode_changed event
  2. Build execution context (similar to escalation context but with the user
     message front-and-center)
  3. Call LLM with intervention prompt
  4. Parse + execute actions (shared helper from Phase 5)
  5. Transition back to Monitoring, persist_mode, emit mode_changed event
- Update chat.submit routing test: verify that when FunnelState is Executing AND
  a Director session is active, the message reaches the Director and the Director
  acts on it. This is an integration test candidate.

### Phase 7: Cross-session pattern detection + reconcile_from_ipc

**Scope:**
- Implement the pattern tracker thresholds per the design doc Phase 7 bullets:
  - error-signature grouping (hash of error message) so "same root cause" across
    multiple sessions triggers escalation rather than mechanical retry
  - bundle rejection correlation: same reviewer feedback across bundles for the
    same work
  - spec-level failure detection: >M% of a spec's works abandoned → flag for
    revision before the engine's bubble-up fires
- Fill in `reconcile_from_ipc()`:
  - Query `stores.works` for all Works with the current `plan_id`, tally
    `session_failure_count`
  - Query `stores.bundles` for rejected bundles under this plan, build rejection
    history
  - Query `stores.specs` for `revision_count`
  - Populate `pattern_tracker` from these instead of accumulating event-by-event
- Property test (design doc specifies this): observe events → clear → reconcile →
  tracker matches event-driven version. This validates that ground-truth-derived
  counts equal event-accumulated counts.

**Gotcha:** The design explicitly says NOT to persist pattern_tracker to JSONL (the
Architect's round-1 feedback was the reason for this). Reconciliation from persistent
Work/Bundle/Spec state is the durability story. Don't add a new Record type.

### Phase 8: Integration tests + cleanup

**Scope:**
- Full E2E test: Chat → /plan → Director interview → /accept → Monitoring →
  GoalComplete. This needs `e2e` skill infrastructure and a small target repo.
- E2E: inject a plan with a broken AC, verify Director detects stuck state and
  takes corrective action via the new Escalation mode.
- E2E: user intervention during execution, verify Director translates intent into
  plan modifications.
- Clean up any remaining dead Coordinator references. (v4-cutover already removed
  the Coordinator struct; check for stale comments, test fixtures, prompt file
  references.)
- Verify `supervision.yml` strategies work with the new Director lifecycle end-to-end:
  kill a running Director, confirm `restart-director-on-event` triggers, confirm
  `has-active-plan` guard prevents spurious restarts after the plan completes.
- Update `CLAUDE.md` codebase map to reflect the Director module.
- Mark the parent design doc `docs/design/2026-04-16-director-agent.md` as Implemented.

**Finalization (after Phase 8 passes otto ci):**
- `/bump -p` (patch) or appropriate level
- `git push && git push --tags`
- `cargo install --path .`
- `systemctl --user restart loopr` if a daemon service runs

## Known Issues / Followups

1. **Director conversation loop lacks a custom interview prompt.** It uses
   `prompts::store().director` which may or may not exist in the current
   `resources/prompts/agents/director.pmt`. Check and seed a proper interview prompt
   if empty. The stub path emits "[Director prompt not configured]" when the prompt
   is empty.

2. **PlanIntake uses plain `ToolExecutor::standard` without delegation.** Chat uses
   `ToolExecutor::chat_with_delegation(..., delegate_llm)` which gives the chat a
   fast sub-LLM for sub-tasks. The Director probably wants the same. Phase 4 or
   Phase 8 candidate.

3. **No Director-specific Lifeguard tests yet.** `lifeguard: Lifeguard::new()` is
   stored on DirectorAgent but never consulted. Phase 5 should wire it (check
   hash of LLM request before each escalation call; escalate the Director to user
   if it's stuck). Phase 2's `let _ = &self.lifeguard;` silences the unused warning
   until then.

4. **`director_message_tx` map grows unboundedly.** Each spawn inserts a sender;
   nothing removes stale senders when the Director terminates. Add a cleanup path
   in the run_agent_task post-loop for `AgentKind::Director` sessions. (Similar
   cleanup exists for chat handles in `agent.stop`.)

5. **Chat-to-Director handoff doesn't migrate chat history messages.** The
   `director.start_plan_intake` handler stamps `director_session_id` on the
   existing ChatHistory but doesn't copy prior chat messages into a new context.
   The PlanIntake conversation loop reads from the same ChatHistory so history
   is implicitly shared - but the design doc says "Handler copies the chat message
   history into the Director's initial context." Current implementation shares
   rather than copies. For now this works (Director sees the same messages); if
   we want chat and director to have *separate* histories, this needs revisiting.

6. **Test stub `NoopLlm` only impls `LlmClient`.** It doesn't impl `AgenticLlm`,
   so we can't write unit tests that exercise the full `planintake_turn()` path
   (which requires `AgentLlmClient` for run_tool_loop). If Phase 4+ needs mockable
   tests for the LLM path, add an `AgenticLlm` impl to `NoopLlm` that returns
   scripted responses.

7. **TUI /plan sends `StartPlanIntake` but doesn't update the chat history renderer
   to distinguish Director vs. Chat output.** Both emit `agent.llm_output` events
   with the same shape, so the TUI renders them identically. If we want to
   visually indicate "Director is talking" vs. "Chat assistant is talking", the
   event payload needs a role hint or the TUI needs to cross-reference
   `session_id` against `AgentKind`.

## Getting Started

```bash
cd ~/repos/scottidler/loopr-v4
git log --oneline -5  # see the three Phase 1/2/3 commits
cat docs/design/2026-04-16-director-agent.md  # authoritative design
cat docs/design/2026-04-16-director-agent-phase4-handoff.md  # this file
cargo test --lib --quiet  # 2489 passing, 3 pre-existing failures
```

Start with Phase 4. The pattern infrastructure is already in place; you're just
adding threshold checks and mode transitions. Don't refactor beyond what Phase 4
requires - Phases 5-7 will extend.

When in doubt, consult the Architect: `/architect docs/design/2026-04-16-director-agent.md
focus on [your question]`.
