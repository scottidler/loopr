# Design Document: Conversational Funnel Testing Strategy

**Author:** Scott Idler
**Date:** 2026-03-29
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

A "Constrained Persona Matrix" testing strategy for automatically validating Loopr's Coordinator interview path - the `Interviewing` FSM state where the Coordinator negotiates with a user to scope a project and produce a Draft Plan. A lightweight Rust harness feeds scripted turn-indexed responses via `coordinator.interview_respond` IPC, then runs two assertion tiers (structural keyword checks and an optional single-shot Evaluator LLM) against the resulting Plan record.

## Problem Statement

### Background

Loopr has two distinct ways the "user interviews Coordinator to build a plan" interaction can happen:

1. **Coordinator FSM path** - The Coordinator agent runs in `Interviewing` state. It emits `coordinator.interview_question` DaemonEvents to the TUI, which the user answers via `coordinator.interview_respond` IPC. Eventually the Coordinator proposes a Draft Plan via `ProposePlan`, which transitions to Planning on approval.

2. **Chat funnel path** - The user chats with an LLM via `chat.submit` under `FunnelState::Interview`. The plan is drafted via `/draft` and accepted via `/accept` → `coordinator.accept_plan`. This triggers autonomous execution.

This design tests path 1: the Coordinator's own `Interviewing` FSM. Path 2 (chat funnel) is related but a separate concern. The recent additions of `InterviewMode::Auto` and `InterviewMode::Skip` (see `2026-03-03-headless-testing-auto-interview.md`) cover the downstream execution pipeline; this design covers the upstream interview phase that `InterviewMode::Interactive` (the default) requires.

### Problem

Testing conversational AI is inherently difficult. Two naive approaches fail:

1. **Inject a pre-formed Plan** via `coordinator.accept_plan` - bypasses interview prompt engineering entirely. Tests execution, not negotiation.
2. **Unbounded synthetic user LLM** - CI anti-pattern: expensive, slow, non-deterministic, prone to deadlocks, impossible to assert programmatically.

We need a middle path: deterministic enough inputs to be CI-safe, expressive enough to exercise the Coordinator's handling of ambiguity, scope creep, and pushback.

### Goals

- Automate end-to-end testing of the Coordinator's `Interviewing` FSM state
- Drive the interview via the existing Unix socket JSON-RPC IPC layer, simulating a real TUI client
- Assert properties of the resulting Draft Plan programmatically
- Keep fast structural assertions separate from slower LLM quality assertions

### Non-Goals

- Testing full plan execution (covered by `InterviewMode::Skip` + `loopr run` tests)
- Making the Coordinator LLM fully deterministic
- Testing the Chat FunnelState path (`FunnelState::Interview`, `chat.submit`) - separate design
- Building a general-purpose chatbot evaluation framework

## Proposed Solution

### Overview

The **Constrained Persona Matrix** uses a Rust integration test (the "Persona Driver") that feeds hardcoded, turn-indexed answer arrays into the daemon via `coordinator.interview_respond` IPC. After the Coordinator proposes a Draft Plan, the harness asserts against the Plan record in two tiers:

1. **Structural assertions** (always, fast) - Plan exists, has required fields, matches keyword criteria from the fixture. No LLM.
2. **LLM Evaluator assertions** (nightly, slow) - Single-shot Claude call grades the plan against a rubric. Structured JSON output only.

### Architecture

```
┌─────────────────────────────────────────────────────────┐
│  Rust Integration Test (tests/funnel.rs)                │
│                                                         │
│  ┌────────────────────┐    Unix socket / JSON-RPC       │
│  │   Persona Driver   │ ─── coordinator.set_goal ──►    │
│  │                    │ ─── agent.start ──────────►  ┌──┴──┐
│  │  - Turn counter    │ ◄── DaemonEvent (question) ─ │Dmn  │
│  │  - responses[]     │ ─── coordinator.interview_   │  Co-│
│  │  - fallback        │     respond ──────────────►  │  ord│
│  └─────────┬──────────┘ ◄── DaemonEvent             │     │
│            │                (record.created: plan)  └──┬──┘
│            │                                           │
│            │ on: record.created (plans)                │
│            ▼                                           │
│  ┌─────────────────────┐                              │
│  │  Assertion Layer    │ ─── plan.get ──────────────► │
│  │  Tier 1: structural │                              │
│  │  Tier 2: evaluator  │                              │
│  └─────────────────────┘                              │
└─────────────────────────────────────────────────────────┘
```

### IPC Flow

No new IPC methods are required. The Persona Driver uses existing methods:

| Step | Method | Direction | Params / Notes |
|------|--------|-----------|----------------|
| 1. Set goal | `coordinator.set_goal` | Client → Daemon | `{"goal": "<initial_goal>"}` |
| 2. Start agent | `agent.start` | Client → Daemon | `{"agent_type": "coordinator"}` |
| 3. Wait for question | `coordinator.interview_question` | DaemonEvent | `{"questions": [...]}` - listen on event stream |
| 4. Send answer | `coordinator.interview_respond` | Client → Daemon | `{"answer": "<response>"}` |
| 5. Repeat 3-4 | - | - | Until `record.created` for plans collection |
| 6. Fetch plan | `plan.get` | Client → Daemon | `{"id": "<plan_id>"}` from event |
| 7. Assert | - | Harness-local | Tier 1 always; Tier 2 nightly |

**Note on method name:** The IPC method is `coordinator.interview_respond` (verb form), not `coordinator.interview_response` (noun). This matches the handler name `handle_coordinator_interview_respond` in `src/daemon/handlers.rs:210`.

**Note on agent startup:** `coordinator.set_goal` only persists the `CoordinatorGoal` record - it does not spawn the agent. The `agent.start` call is required separately. In sync test contexts (no Tokio runtime), the auto-start inside `coordinator.accept_plan` is skipped (see `handlers.rs:4218`), so tests must always call `agent.start` explicitly.

### Persona Fixtures

Defined in `tests/fixtures/personas/` as YAML files with hyphen-cased keys:

```yaml
# tests/fixtures/personas/scope-creeper.yml
name: "The Scope Creeper"
initial-goal: "Build a simple Python CLI to read a CSV."
responses:
  - "Actually, make it output to a Postgres database instead of JSON."
  - "I'm not sure about the DB schema, just figure it out. Also add Redis caching."
fallback: "Just do whatever you think is best."
assertions:
  required-keywords:
    - "postgres"
    - "redis"
    - "python"
  plan-must-have-phases: true
  rubric: |
    The plan should reflect the user's evolved requirements toward a Postgres/Redis
    architecture, not the original CSV-only request. The initial goal should not
    dominate the plan if the user explicitly redirected toward a database backend.
```

Alternatively, fixtures can be Rust constants in `tests/personas.rs` for compile-time checking:

```rust
pub struct PersonaFixture {
    pub name: &'static str,
    pub initial_goal: &'static str,
    pub responses: &'static [&'static str],
    pub fallback: &'static str,
    pub required_keywords: &'static [&'static str],
    pub rubric: Option<&'static str>,
}

pub const SCOPE_CREEPER: PersonaFixture = PersonaFixture {
    name: "The Scope Creeper",
    initial_goal: "Build a simple Python CLI to read a CSV.",
    responses: &[
        "Actually, make it output to a Postgres database instead of JSON.",
        "I'm not sure about the DB schema, just figure it out. Also add Redis caching.",
    ],
    fallback: "Just do whatever you think is best.",
    required_keywords: &["postgres", "redis", "python"],
    rubric: Some("Plan should reflect evolved Postgres/Redis requirements, not original CSV-only request."),
};
```

### Persona Driver

```rust
// tests/funnel.rs (sketch - not final implementation)

async fn run_persona(daemon_addr: &str, fixture: &PersonaFixture) -> eyre::Result<String> {
    let mut client = IpcClient::connect(daemon_addr).await?;
    let mut events = client.subscribe_events();

    // 1. Set goal and start agent
    client.send("coordinator.set_goal", json!({"goal": fixture.initial_goal})).await?;
    client.send("agent.start", json!({"agent_type": "coordinator"})).await?;

    let mut turn = 0usize;
    let timeout = Duration::from_secs(120);

    // 2. Drive interview loop
    let plan_id = tokio::time::timeout(timeout, async {
        loop {
            match events.recv().await? {
                DaemonEvent { event, data } if event == "coordinator.interview_question" => {
                    let answer = fixture.responses.get(turn)
                        .copied()
                        .unwrap_or(fixture.fallback);
                    client.send("coordinator.interview_respond", json!({"answer": answer})).await?;
                    turn += 1;
                }
                DaemonEvent { event, data } if event == "record.created"
                    && data["collection"].as_str() == Some("plans") =>
                {
                    break Ok(data["id"].as_str().unwrap_or_default().to_string());
                }
                _ => {} // ignore other events
            }
        }
    }).await??;

    Ok(plan_id)
}
```

### Assertion Tiers

**Tier 1 (structural, always):**
- Plan record exists and is retrievable via `plan.get`
- Plan `title` and `description` are non-empty
- All `required_keywords` appear in the plan text (case-insensitive)
- Plan has at least one Spec/Phase if `plan-must-have-phases: true`

**Tier 2 (LLM evaluator, nightly only):**
- Sends plan text + rubric to a single Claude API call
- Requests structured JSON: `{"passed": bool, "reason": string}`
- Asserts `passed == true`
- Use a smaller/cheaper model (e.g., `claude-haiku-4-5`) than the plan author to reduce grading bias and cost

### Test Persona Matrix

| Persona | Initial Goal | Tests |
|---------|-------------|-------|
| Golden Path | "Build a CLI todo app in Rust with SQLite persistence" | Coordinator produces a coherent plan matching the goal |
| Vague User | "Build something to manage my stuff" | Coordinator extracts a scoped, actionable plan |
| Scope Creeper | "Build a Python CSV reader" → adds Postgres, Redis | Plan reflects evolved requirements, not original |
| Pushback User | Rejects first proposal, asks for alternatives | Plan still reaches a usable, accepted state |
| Silent User | Only fallback responses | No deadlock; Coordinator reaches ProposePlan eventually |

### Implementation Plan

**Phase 1: Plumbing**
- Create `tests/funnel.rs` integration test module
- Implement `PersonaDriver` struct with turn-based IPC event loop
- Wire up Golden Path persona
- Single assertion: `record.created` event received with a plan ID within timeout

**Phase 2: Structural Assertions**
- Implement `AssertionRunner` for Tier 1 keyword checks
- Add `required_keywords` to Golden Path persona
- Add Vague User persona

**Phase 3: Full Matrix**
- Add Scope Creeper, Pushback User, Silent User personas
- Add timeout handling and failure messages
- Integrate into `otto ci` as a separate `e2e` suite (not part of the fast loop)

**Phase 4: LLM Evaluator (optional)**
- Add Tier 2 evaluator via Loopr's internal `LlmClient`
- Configure with nightly-only feature flag or separate `otto e2e-nightly` task
- Add `rubric` strings to each persona fixture

## Alternatives Considered

### Alternative 1: Hand-Crafted Plan Injection

- **Description:** Call `coordinator.accept_plan` with a pre-written plan text, bypassing the interview entirely.
- **Pros:** 100% deterministic, extremely fast. Already the mechanism for `InterviewMode::Skip` tests.
- **Cons:** Tests zero percent of Coordinator prompt engineering and user negotiation logic.
- **Why not chosen:** Solves a different problem. This approach is correct for execution tests; it is not useful for funnel tests.

### Alternative 2: Unbounded Synthetic User LLM

- **Description:** Point a second LLM agent at the Coordinator interview event stream and have it "act like a user."
- **Pros:** Highest realism for simulating human interaction.
- **Cons:** CI anti-pattern - expensive, slow, non-deterministic, prone to AI arguments and deadlocks, virtually impossible to assert programmatically.
- **Why not chosen:** Flakiness. Scripted personas give 80% of the realism with near-zero non-determinism on the input side.

### Alternative 3: Bash Script Harness

- **Description:** Drive IPC via a `bin/test-funnel.sh` script using `socat` to talk to the Unix socket.
- **Pros:** Simple to sketch quickly.
- **Cons:** No typed JSON parsing; awkward Unix socket handling in bash; no reuse of Loopr's existing IPC client or test infrastructure; hard to maintain as the protocol evolves.
- **Why not chosen:** The project has a Rust IPC client and established integration test patterns. A Rust test gets compiler-checked types and direct reuse of `IpcClient` for free.

### Alternative 4: `InterviewMode::Auto` as Test Oracle

- **Description:** Set `InterviewMode::Auto`, run the Coordinator headlessly, and assert on the auto-generated plan.
- **Pros:** Already implemented; zero additional code.
- **Cons:** Auto mode bypasses the interview Q&A loop entirely by synthesizing its own answers. It does not test how the Coordinator handles *user* responses - specifically scope creep, pushback, and vague requirements.
- **Why not chosen:** Tests a different thing. Auto mode validates that the Coordinator can work headlessly; persona testing validates that it responds correctly to specific user archetypes. Both are valuable.

## Technical Considerations

### Dependencies

- No new external dependencies for Tier 1
- Tier 2 reuses `src/agents/llm_client.rs` (already in-tree)
- Structured output (JSON schema mode) is supported by the Claude API and already used in production

### Performance

- Each persona test requires 3-10 LLM calls (interview turns + ProposePlan). Slow for unit test feedback but acceptable for e2e.
- Persona tests must run in an isolated daemon per test (temp dir, unique socket path) to allow parallelism.
- Tier 2 adds one additional LLM call per persona. Run nightly, not in `otto ci`.

### Security

- No new daemon attack surface. All methods used are existing IPC.
- Nightly Tier 2 needs `ANTHROPIC_API_KEY` in CI - same as existing LLM tests.

### Testing Strategy

Meta-validation: deliberately corrupt the Coordinator's interview system prompt (e.g., tell it to ignore user answers). The Scope Creeper persona's `required_keywords` assertions should fail because `postgres` and `redis` will be absent from the plan.

### Rollout Plan

1. Golden Path - basic plumbing, assert `record.created` for plans
2. Structural keyword assertions - add `required_keywords` tier
3. Full persona matrix - edge cases, Silent User timeout
4. LLM Evaluator (nightly) - optional quality gate

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Coordinator exhausts persona responses before proposing plan | High | Medium | `fallback` string applied on every extra turn after `responses[]` is exhausted; driver does not deadlock |
| Coordinator proposes plan immediately (no questions) | Low | Low | Driver handles `record.created` event at any turn; turn counter stays at 0 and assertions proceed normally |
| Tier 1 keyword assertions become brittle as prompts evolve | Medium | Low | Keep `required_keywords` to 2-3 high-signal terms; avoid checking exact phrasing |
| Evaluator LLM grades incorrectly | Medium | Medium | Rubrics ask for binary facts, not subjective scores; use `reason` field to debug failures |
| Parallel persona tests contend on daemon resources | Low | Medium | Each test spawns an isolated daemon in a `TempDir` with a unique socket path |
| Sync test context skips auto-start (handlers.rs:4218) | Medium | High | Persona Driver always calls `agent.start` explicitly; do not rely on auto-start side effect |

## Open Questions

- [ ] Does `plan.get` IPC exist, or should the harness use `plan.list` + filter by the ID from the `record.created` event? Check `handlers.rs` routing table.
- [ ] Should Tier 2 use the same model as the Coordinator (to test self-consistency) or a different model (to reduce grading bias)? Lean toward different model (`claude-haiku-4-5`) for cost and independence.
- [ ] Should persona fixtures be YAML files (easy to add without recompile) or Rust constants (compiler-checked)? Start with Rust constants; migrate to YAML if the matrix grows past 10 personas.
- [ ] What is the correct timeout per persona? 120 seconds covers most cases but slow models may need more. Consider a configurable `LOOPR_TEST_TIMEOUT_SECS` env var.

## References

- `docs/design/2026-03-03-headless-testing-auto-interview.md` - `InterviewMode` design (Auto/Skip variants)
- `docs/design/2026-03-21-tui-testing-strategy.md` - TUI testing layers and `TestBackend` patterns
- `docs/design/2026-03-04-tui-chat-plan-funnel.md` - Chat FunnelState path (the other interview mechanism)
- `src/domain/chat.rs` - `FunnelState` enum (the chat path, not covered here)
- `src/daemon/handlers.rs:205-218` - IPC routing table for all Coordinator methods
- `src/daemon/handlers.rs:4022` - `handle_coordinator_interview_respond` (note: verb `respond`, not noun `response`)
- `src/daemon/handlers.rs:4241` - `handle_coordinator_interview_question` (emits DaemonEvent to TUI)
- `src/agents/executor.rs:1151` - `ProposePlan` action handler (creates Draft Plan, emits `record.created`)
