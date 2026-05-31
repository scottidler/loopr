# Loopr v5 Deferred Roadmap

**Status:** living index of design docs, most not yet written but known to be needed. Companion to [roadmap.md](roadmap.md). Where roadmap.md tracks the build order from Stage 0 through First Gate, this doc tracks everything past First Gate plus the Stage 7-9 completion gaps that the existing roadmap labels as "earned features" or "deferred."

**Reconciled 2026-05-30:** several entries have shipped since this doc's "not yet written" framing. **Tier 1 is essentially complete:** 1.1, 1.2, 1.4 shipped with their own docs, and 1.3 shipped in substance via Director Phase 1 + the `max_work_attempts` retry budget (no dedicated doc). **Tier 2:** 2.4 shipped; 2.3 is *partial* (attempt tracking yes, wall-clock SLA no); 2.1 and 2.2 are scaffolding-only / unstarted. **Tier 3:** 3.1 (Director Phase 2) shipped; 3.2's event-bus type exists for client streaming but the poll->subscribe migration is unbuilt; 3.3 and 3.4 are unstarted. **Tier 5:** the `trait_variant` cleanup shipped. The per-entry **Status:** markers below are the source of truth.

> **Naming note (ADR-0002):** the `Coordinator` Role variant has been renamed to `Reactor`, and the v3 "Coordinator agent" concept that originally lived as Tier 1.2 is reframed here as **Director — Phase 1 (routine orchestration)**. Tier 3.1 becomes **Director — Phase 2 (judgment plane)**. The same agent rolls out in two phases; there is no separate "Coordinator agent" planned. Headings, the 1.2/3.1 entries, and the dependency graph reflect the rename below; body prose in entries that mention "Coordinator" by name still uses the old term and will sweep when the entry becomes live work. See [`docs/adr/0002-rename-role-coordinator-to-reactor.md`](adr/0002-rename-role-coordinator-to-reactor.md).

**How to use this doc.** Each entry below is a stub for a future design doc. Each entry carries enough source-material pointers, keywords, and acceptance criteria that a future session can run `/create-design-doc` against it: read the keywords and grep the v5 docs they reference, read the v3 / v4 source files cited (at the paths and line ranges given), and produce the dated design doc under `docs/design/YYYY-MM-DD-<slug>.md` matching the conventions in [docs/CLAUDE.md](CLAUDE.md).

**How entries are sized.** Tier numbers are priority bands, not effort estimates. Tier 1 closes Stage 7-9 exit-criterion gaps that real runs have already exercised. Tiers 2 and 3 reach v3 and v4 feature parity respectively. Tier 4 is the vision.md "Beyond First Gate (earned features)" set. Tier 5 is small items that will likely fold into adjacent docs rather than getting their own.

**Build order.** Within each tier, follow the dependency graph at the bottom of this doc. Across tiers, finish Tier 1 before starting Tier 2 unless a specific Tier 2 item is a prerequisite for fixing a Tier 1 issue (Tier 2's `multi-turn-history` is the only such case).

---

## Tier 1: First Gate completion gaps

These items are framed in the existing design docs as Stage 7+ deferrals, but the python-api E2E (2026-04-27) demonstrated that without them, no plan with more than one Work and any failure path completes. Tier 1 is the actual "First Gate" finish line for non-trivial targets.

### 1.1 Dependency DAG enforcement

- **Proposed filename:** [`docs/design/2026-05-07-dependency-gate.md`](design/2026-05-07-dependency-gate.md) **Status: Implemented**
- **Crates touched:** `loopr` (daemon), `domain` (work), `agents` (dispatch)
- **Depends on:** none
- **What it covers.** A hard gate before any Work transitions `Pending -> Ready -> InProgress`. Reads `work.dependencies: Vec<WorkId>`, requires every named dep to be `Done`. Two design choices to settle in the doc: (a) gate at the daemon's `spawn_implementer_for_work` call site, returning early if deps unmet, vs. (b) gate at the action layer with a typed `DependencyNotMet` result fed back to a Coordinator (which v5 does not yet have, so option (a) is the Tier-1-compatible choice). Also resolves whether the gate is hard (refuse to dispatch) or soft (dispatch and let the agent see deps in its prompt context). Companion: a `next_assignable_work` selector in `crates/loopr/src/daemon/` that filters Ready Works whose deps are all Done.
- **Source material.**
  - v3: `~/repos/scottidler/loopr/src/daemon/work_queue.rs:next_assignable_work()` (lines 27-64) - the reference filter.
  - v3: `~/repos/scottidler/loopr/src/agents/executor/action/work.rs:handle_assign_agent()` (lines 48-100, plus tests at 250-300) - the action-layer dependency check returning `DependencyNotMet`.
  - v3 doc: `~/repos/scottidler/loopr/docs/design/2026-04-09-dependency-hardening.md` - the design rationale for hardening dep enforcement.
  - v5 references: [docs/design/2026-04-20-hierarchy.md](design/2026-04-20-hierarchy.md) frames FSM guards as "Stage 7 Coordinator concerns"; the python-api e2e summary at `/tmp/loopr/e2e/python-api/<ts>/.monitor/results.md` shows three Works hitting `inprogress` simultaneously despite a 3-node DAG.
- **Keywords to grep.** `dependencies`, `next_assignable`, `topological`, `topo_sort`, `DependencyNotMet`, `work_queue`.
- **Acceptance criteria for the design doc.**
  - Names the call site that owns the gate.
  - Defines behavior when a dep is in `Blocked` (does the dependent stay Pending forever, or escalate?).
  - Defines behavior when the DAG has a cycle that the decomposer's cycle detection somehow missed (defense in depth).
  - Specifies where the gate is tested - unit (action handler) and integration (daemon end-to-end with a 2-Work plan).
  - States whether this is a permanent home or a Tier-2 stepping stone toward a full Coordinator.

### 1.2 Director — Phase 1 (routine orchestration)

> Was "Coordinator / reactor agent" pre-ADR-0002. Reframed as Phase 1 of the Director agent (the v3-Coordinator-equivalent). Tier 3.1 is Phase 2. The split is by *delivery* — Phase 1 unblocks Stage 9, Phase 2 reaches v4 parity — not by *role*; there is one agent.

- **Proposed filename:** [`docs/design/2026-05-08-director-phase-1.md`](design/2026-05-08-director-phase-1.md) **Status: Implemented**
- **Crates touched:** `agents`, `domain`, `loopr` (daemon dispatch), `context` (state-summary prompt assembly), `store`
- **Depends on:** 1.1 (dep gate is referenced by the Coordinator's dispatch logic), 2.4 (multi-turn LLM history)
- **What it covers.** A long-lived LLM agent (Opus tier per vision.md model budget) that owns the orchestration plane: it polls TaskStore (or, post-2.2 event bus, subscribes), assembles a state summary, decides on actions, and emits them via a typed action vocabulary. v3 had this as `~/repos/scottidler/loopr/src/agents/coordinator.rs` (47KB). The doc must define: (a) the FSM Coordinator runs through (`Interviewing -> Decomposing -> Planning -> Executing -> GoalComplete` is the v3 shape; v5 may collapse since interview is currently CLI-side); (b) the action vocabulary (`assign_agent`, `override_work`, `accept_bundle`, `redecompose`, `abandon`, plus how each action lands as a state mutation); (c) the state-summary builder that surfaces "Rejected Bundles", "Blocked Works", "SLA breaches", "stuck Bundles in Review", to the LLM as a structured prompt; (d) the reconciliation sweep cadence during Executing; (e) the loop's failure / restart story (v3's `max_restarts: 3`).
- **Source material.**
  - v3: `~/repos/scottidler/loopr/src/agents/coordinator.rs` whole file. Pay attention to `build_state_summary_with_sla()` (lines 67-86, 154-191), `build_fsm_footer()` (lines 862-876), the run loop, and the action dispatch.
  - v3: `~/repos/scottidler/loopr/src/agents/executor/action/work.rs` and `bundle.rs` for the action handler shape.
  - v3 docs: `~/repos/scottidler/loopr/docs/design/2026-02-28-coordinator-sequencing.md`, `2026-02-28-loopr-v3-mvp5-coordinator-sequencing.md`, `2026-04-03-reconciliation-sweep.md`.
  - v5 references: vision.md's "Anthropic leaked primitive #6" (typed event bus) framing; [roadmap.md](roadmap.md) "Director agent" entry under Beyond First Gate (Coordinator is the v3-equivalent that v5 has been calling Director-by-another-name); [docs/design/2026-04-22-stage-8-wiring.md](design/2026-04-22-stage-8-wiring.md) "deferred reactor pattern" section.
- **Keywords to grep.** `coordinator`, `Coordinator`, `run_coordinator`, `state_summary`, `build_fsm_footer`, `assign_agent`, `override_work`, `reconcile`, `reactor`.
- **Acceptance criteria for the design doc.**
  - Defines the Opus model selection; reuses the model-tier hooks added in 2.4 if they exist by then, else hardcodes Opus for now.
  - Defines the action vocabulary as a Rust enum; every variant maps to a `domain::Role`-authorized FSM transition.
  - Defines how state changes reach the Coordinator (poll vs. event subscription; pick polling for now if 3.2 event bus is not yet shipped, with a clear migration note).
  - Defines the reconciliation sweep contents (Integrated -> Done promotion, Bundle FSM cleanup, stuck-Work detection).
  - Defines what the Coordinator does NOT do (it is not the Director; cross-session pattern tracking is 3.1's scope).
  - Specifies prompt-assembly contract with the `context` crate (likely a new `context::coordinator::build_state_summary` function).

### 1.3 Blocked-Work and Rejected-Bundle recovery

- **Proposed filename:** `docs/design/<YYYY-MM-DD>-recovery-loop.md` **Status: Implemented** (in substance, no dedicated doc). Delivered via [`2026-05-08-director-phase-1.md`](design/2026-05-08-director-phase-1.md) (Director emits `override_work {target: Ready}` to retry rejected/Blocked Works) + the `max_work_attempts` retry budget (`crates/agents/src/config.rs`, 3-layer enforcement, bumps `Work.attempt_count` until the cap promotes the Plan to `Stalled`). The `blocked_reason` field shipped (`crates/domain/src/work.rs:131`) and reaches the retried Implementer via `context::build_for_implementer` (`crates/context/src/implementer.rs:328`), satisfying the rejection-feedback acceptance criterion below.
- **Crates touched:** `agents`, `domain` (work, bundle), `loopr` (daemon)
- **Depends on:** 1.2 (the recovery actions are emitted by the Coordinator)
- **What it covers.** Two specific failure paths that today dead-end:
  - **Bundle rejected:** today, Reviewer rejects -> Bundle terminal -> Work goes Blocked -> nothing happens. Recovery loop: Coordinator sees Rejected Bundle in state summary, emits `override_work { target: <work_id>, target_status: Ready, reason: "bundle <id> rejected: <summary>" }`. Worker pool re-spawns Implementer, which now sees the rejection summary in its context (this requires the rejection summary to be appended to the Work's history; design doc must specify where).
  - **Implementer escalates (NeedHelp / Lifeguard / force-propose guard):** today, Work -> Blocked, terminal. Recovery loop: Coordinator sees Blocked Work, decides retry vs. abandon vs. re-decompose based on attempt-count (which requires 2.3 SLA tracking). For Tier 1, ship a simple "retry once with rejection feedback" path; richer judgment is 3.1 Director.
- Adds the `blocked_reason` field on Work that hierarchy.md deferred per scope memo D3.
- **Source material.**
  - v3: `~/repos/scottidler/loopr/src/agents/coordinator.rs:154-191` - the "Rejected Bundles (Work needs reset to Ready)" recipe in the state summary.
  - v3: `~/repos/scottidler/loopr/src/agents/executor/util.rs:78-96` - `determine_work_handback()`, the v3 logic for picking the next status after a session ends.
  - v3: `~/repos/scottidler/loopr/src/agents/executor/lifecycle.rs:207-225` - max-failure escalation block.
  - v3 doc: `~/repos/scottidler/loopr/docs/design/2026-03-31-rejection-recovery-circuit-breaker.md`.
  - v5 references: [docs/design/2026-04-20-hierarchy.md](design/2026-04-20-hierarchy.md) D3 (`blocked_reason` deferral); [docs/design/2026-04-21-implementer.md](design/2026-04-21-implementer.md) "EscalationNeeded" return type; the python-api e2e where `wk-r0fad` blocked terminally after `bd-jdekt` rejection.
  - v5 code: `crates/loopr/src/daemon/context.rs:334-341` (the spot that today writes Blocked and returns).
- **Keywords to grep.** `Blocked`, `rejected`, `recovery`, `override_work`, `determine_work_handback`, `EscalationNeeded`, `RequestChanges`, `circuit_breaker`.
- **Acceptance criteria for the design doc.**
  - Specifies how rejection summary text reaches the next Implementer attempt (history appendage, prompt slot, both).
  - Specifies the `blocked_reason` field on Work and what writes it.
  - Defines the retry budget without depending on full SLA (a simple per-Work counter is fine; 2.3 generalizes).
  - States what happens after the budget is exhausted (Tier 1: stop with a clear log; Tier 3 Director can decide).

### 1.4 Validation execution

- **Proposed filename:** [`docs/design/2026-05-08-validation.md`](design/2026-05-08-validation.md) **Status: Implemented**
- **Crates touched:** `integrator`, `domain` (tick), `tools` (probably; we likely run a configured shell command), `loopr` (daemon orchestration)
- **Depends on:** none (orthogonal to 1.1-1.3, but probably authored after them since 1.3 covers what happens when validation fails)
- **What it covers.** Post-merge validation that the integration branch is not broken. The design doc was deferred by [docs/design/2026-04-22-integrator.md](design/2026-04-22-integrator.md) and [docs/design/2026-04-22-stage-8-wiring.md](design/2026-04-22-stage-8-wiring.md) explicitly as needing its own doc driven by a real-run failure. The python-api run shipped a Tick on `b04af23` and `fe250f1` while `main.py` only had a stub; validation would have caught this. Doc must define: (a) where validation commands come from (config file in target's `.loopr/validate.yml`? CLI flag? convention-detected `cargo check` / `pytest` / `npm test`?); (b) what counts as failure (non-zero exit, timeout, output pattern); (c) what happens on failure (Tick goes to a new `IntegrationFailed` Tick state? roll back the merge? mark the contributing Bundles for re-review?); (d) how validation interacts with `docker compose run --rm test`-style commands that the python-api target uses.
- **Source material.**
  - v3 / v4: validation patterns in their integrators (search for `validate`, `verify_step`, `Verification`).
  - v5 references: [docs/design/2026-04-22-integrator.md](design/2026-04-22-integrator.md) "Validation deferred"; [docs/design/2026-04-22-stage-8-wiring.md](design/2026-04-22-stage-8-wiring.md) "Validation - earns its own design doc"; `crates/integrator/CLAUDE.md` "Validation will be earned via its own design doc"; `bin/e2e-targets/*.md` PRDs which all carry a "Final Validation" section showing the user-facing contract.
- **Keywords to grep.** `validation`, `validate`, `verify`, `Verification`, `IntegrationFailed`.
- **Acceptance criteria for the design doc.**
  - Defines the configuration surface (per-target validate file vs. config in `.loopr/config.yml`).
  - Defines the Tick FSM extension (or new state) that records validation pass/fail.
  - Defines the rollback story or the clear "we don't roll back, we report" choice.
  - Specifies the lane the validation command runs in (`tools::router` lanes: heavy if it builds, net if it pulls images).
  - Names the tools-crate touch points (likely a new `Tool` kind, or reuses Bash with a sandbox tier).

---

## Tier 2: v3 parity

These items existed in v3 and are still relevant; the python-api run did not directly need them but they round out the orchestration plane.

### 2.1 Researcher agent

- **Proposed filename:** `docs/design/<YYYY-MM-DD>-researcher.md` **Status: Not shipped** (scaffolding only). `Role::Researcher`, `context::build_for_researcher` (`crates/context/src/implementer.rs:234`), and a prompt template exist, but there is no researcher agent loop, no `Learning` record, and no `spawn_researcher` surface.
- **Crates touched:** `agents`, `context`, `tools`, `loopr` (dispatch)
- **Depends on:** 2.4 (multi-turn LLM)
- **What it covers.** On-demand investigation agent that runs in parallel to Implementer to gather context before or during implementation. v3 has it as `~/repos/scottidler/loopr/src/agents/researcher.rs` (40KB, Sonnet, max-iters=10). Inputs: a question and optional file/path hints. Outputs: a structured "learning record" that Coordinator or Implementer can read. Doc must define: invocation surface (Coordinator action `spawn_researcher`? Implementer "ask researcher" tool?), output record type (probably new `domain::Learning`), tool registry restrictions (read-only? full?), iteration cap, when researcher results expire.
- **Source material.**
  - v3: `~/repos/scottidler/loopr/src/agents/researcher.rs` whole file.
  - v4: `~/repos/scottidler/loopr-v4/src/agents/researcher.rs` for the refactored shape.
  - v5 references: vision.md "Researcher agent" Beyond First Gate; [roadmap.md](roadmap.md) "before this ships, the Implementer does its own lookup"; the python-api `=0.115` malformed-pip artifact (a Researcher could have verified `uv add` syntax before the implementer flailed).
- **Keywords to grep.** `researcher`, `Researcher`, `Learning`, `investigate`, `spawn_researcher`.
- **Acceptance criteria for the design doc.**
  - Defines the trigger surface (action emitted by Coordinator vs. tool callable from Implementer; pick one for v5, motivate).
  - Defines the `Learning` record (or chosen alternative) and its persistence.
  - Defines tool-registry scoping (read-only set probably).
  - Specifies parallelism story relative to 3.3 parallel-implementers.

### 2.2 Re-decomposition vocabulary

- **Proposed filename:** `docs/design/<YYYY-MM-DD>-redecompose.md`
- **Crates touched:** `decomposer`, `domain` (plan + work), `agents` (Coordinator emits the action)
- **Depends on:** 1.2 (Coordinator owns the action), 3.4 (Spec/Phase hierarchy if re-decomposing at sub-levels)
- **What it covers.** Calling the decomposer again on an existing Plan after a failure. v4 had `DirectorAction::ReDecompose { target_type: ReDecomposeTarget, target_id, reason }`. v5 will likely ship a Coordinator action first, then later a Director action that wraps it. Doc must define: idempotency (do existing Works survive? are they Superseded? abandoned?), how the new DAG merges with the old (probably "all old non-Done Works become Superseded; new ones replace them"), how the ratification call avoids reproducing the same broken decomposition (feedback signal in the system prompt). Pulls in `Plan.decomposition_attempts` and `Plan.bubble_up_count` fields deferred by [docs/design/2026-04-20-hierarchy.md](design/2026-04-20-hierarchy.md).
- **Source material.**
  - v3 stub: `~/repos/scottidler/loopr/src/agents/integrator.rs:306-310` - the `bundle.stale_replan_needed` event that v3 logs but does not act on.
  - v3 callable: `~/repos/scottidler/loopr/src/decomposer.rs` (`decompose_into`, `persist_hierarchy`).
  - v4 real: `~/repos/scottidler/loopr-v4/src/agents/director/actions.rs:62-67` - `DirectorAction::ReDecompose`. Tests at `~/repos/scottidler/loopr-v4/src/agents/director/tests.rs`.
  - v5 references: [docs/design/2026-04-20-hierarchy.md](design/2026-04-20-hierarchy.md) deferral of `decomposition_attempts` and `bubble_up_count`; [docs/design/2026-04-20-plan-then-decompose.md](design/2026-04-20-plan-then-decompose.md) "Stage 7's reconcile-on-restart clears the half-written state."
- **Keywords to grep.** `redecompose`, `re_decompose`, `ReDecompose`, `decomposition_attempts`, `bubble_up_count`, `Superseded`, `replan`.
- **Acceptance criteria for the design doc.**
  - Defines what "re-decompose" means: replace, augment, or supersede.
  - Defines the failure-signal payload that the next decomposition LLM call gets (so it does not produce the same broken DAG).
  - Defines the FSM transitions on existing Works (Superseded vs. Abandoned vs. survive).
  - Updates the decomposer's idempotency story.

### 2.3 SLA, attempt tracking, and goal timeout

- **Proposed filename:** `docs/design/<YYYY-MM-DD>-sla-tracking.md` **Status: Partial.** Attempt tracking shipped (`Work.attempt_count` + the `max_work_attempts` budget with 3-layer enforcement, `crates/agents/src/config.rs`). The wall-clock half is NOT shipped: no per-Work max-wall-clock, no `goal_timeout_secs`, no `first_assigned_at`. A dedicated doc is still needed only for the wall-clock budgets. (The IPC/transport timeouts in [`2026-05-09-ipc-timeouts.md`](design/2026-05-09-ipc-timeouts.md) are unrelated; they bound socket waits, not goal/Work SLA.)
- **Crates touched:** `domain` (work, plan, coordinator state), `agents` (Coordinator surfaces SLA in state summary), `loopr` (config surface for SLA limits)
- **Depends on:** 1.2 (Coordinator state lives somewhere)
- **What it covers.** Per-Work attempt counter, first-assigned timestamp, max-attempts and max-wall-clock budgets. Goal-level wall-clock timeout. v3 has `WorkSlaConfig`, `work_attempts`, `work_first_assigned_at`, `goal_timeout_secs`. Doc must define: where the counters live (Work fields? a sidecar `coordinator_state.jsonl`?), defaults for the budgets, surfacing in state summary, what happens at breach (state summary flag for Coordinator LLM to act on; not auto-abandon).
- **Source material.**
  - v3: `~/repos/scottidler/loopr/src/config.rs` `WorkSlaConfig` definition.
  - v3: `~/repos/scottidler/loopr/src/agents/coordinator_state.rs` `work_attempts: HashMap<String,u32>`, `work_first_assigned_at: HashMap<String,i64>`.
  - v3: `~/repos/scottidler/loopr/src/agents/coordinator.rs:67-86` `build_state_summary_with_sla()`; `1068-1073` goal timeout check.
  - v3 doc: `~/repos/scottidler/loopr/docs/design/2026-03-01-coordinator-override-sla-recovery.md`.
- **Keywords to grep.** `sla`, `Sla`, `WorkSlaConfig`, `max_attempts`, `goal_timeout`, `wall_clock`, `first_assigned_at`.
- **Acceptance criteria for the design doc.**
  - Defines storage location and serialization for the counters.
  - Defines defaults and where they live in `.loopr/config.yml`.
  - Defines the breach surfacing in Coordinator state summary.
  - Defines whether breach is informational or actionable for the Coordinator LLM.

### 2.4 Multi-turn LLM history and context builder v2

- **Proposed filename:** [`docs/design/2026-05-08-multi-turn-llm.md`](design/2026-05-08-multi-turn-llm.md) **Status: Implemented**
- **Crates touched:** `llm`, `context`, `agents`
- **Depends on:** none (this is the gating Tier 2 item; many later items depend on it)
- **What it covers.** v5's `LlmClient` trait today takes bare `&str` (per [docs/design/2026-04-20-llm-client.md](design/2026-04-20-llm-client.md)). For Coordinator state-summary turns, Director judgment turns, and Researcher iterative inquiry, we need typed multi-turn `Message` history. Doc must define: the `Message` enum (system / user / assistant / tool-use / tool-result), the conversion from / to Anthropic SDK shapes, how `context` crate consumers build histories, and how the Implementer's existing single-iteration `messages` Vec migrates to this typed shape without churn. Also picks up the deferred `context-builder.md` from the Stage 6 scope memo.
- **Source material.**
  - v3: `~/repos/scottidler/loopr/src/agents/implementer.rs` and `coordinator.rs` for how multi-turn looks in practice.
  - v4: `~/repos/scottidler/loopr-v4/src/agents/director.rs` for the most evolved shape (Director needs state reconciliation across turns).
  - v5 references: [docs/design/2026-04-20-llm-client.md](design/2026-04-20-llm-client.md) "Stage 7 earns complete_streaming and complete_agentic"; [docs/design/2026-04-20-plan-then-decompose.md](design/2026-04-20-plan-then-decompose.md) "defers context-builder.md to Stage 7."
- **Keywords to grep.** `Message`, `multi_turn`, `complete_agentic`, `context_builder`, `assemble_prompt`, `system_prompt`, `user_prompt`.
- **Acceptance criteria for the design doc.**
  - Defines the `Message` enum / struct.
  - Defines the LlmClient API extension (`complete_with_history`).
  - Defines how Implementer's existing message accumulation migrates.
  - Defines context-crate prompt assembly contracts for Coordinator, Researcher, Director.
  - Specifies token budgeting per role.

---

## Tier 3: v4 parity

These items existed in v4 (the `loopr-v4` repo) and represent the most-evolved shape of the orchestration plane. Tier 3 is what "feature complete relative to last shipped version" looks like.

### 3.1 Director — Phase 2 (judgment plane)

> Phase 2 of the Director agent introduced in 1.2. Phase 1 is routine orchestration; Phase 2 layers judgment, escalation, pattern tracking, and the four-mode model. Same agent.

- **Proposed filename:** [`docs/design/2026-05-09-director-phase-2.md`](design/2026-05-09-director-phase-2.md) **Status: Implemented** (followups: [`2026-05-12-director-phase-2-followups.md`](design/2026-05-12-director-phase-2-followups.md)). Shipped v0.7.17 to v0.7.20. The shipped vocabulary differs from the v4-derived sketch below: modes landed as Normal / Conservative / NeedsOperator (not the four v4 modes), and operator interaction is `loopr director chat` rather than a `UserIntervention` mode.
- **Crates touched:** `agents`, `context`, `domain`, `ipc`, `loopr`
- **Depends on:** 1.2 (Coordinator), 2.4 (multi-turn), 3.2 (event bus, for Monitoring mode)
- **What it covers.** The judgment-plane Opus agent that v4 introduced and v5 has been calling "the deferred escalation agent." Four modes per v4's design doc: PlanIntake (interviews user), Monitoring (subscribes to event bus, maintains pattern tracker), Escalation (called by Coordinator when threshold breached, judges next move via Opus), UserIntervention (handles in-flight chat). Action vocabulary: `ReviseWork`, `ReDecompose`, `AbandonWork`, `SpawnResearcher`. Pattern tracker fields: `work_failure_history`, `rejection_history`, `spec_revision_count`. Lifeguard variant for Director itself. State reconciliation on `RecvError::Lagged`.
- **Source material.**
  - v4: `~/repos/scottidler/loopr-v4/src/agents/director.rs` (64KB, whole file).
  - v4: `~/repos/scottidler/loopr-v4/src/agents/director/actions.rs` `DirectorAction` enum.
  - v4: `~/repos/scottidler/loopr-v4/src/agents/director/tests.rs` for action parsing and state transitions.
  - v4 doc: `~/repos/scottidler/loopr-v4/docs/design/2026-04-16-director-agent.md` (the 7-phase design).
  - v4 doc: `~/repos/scottidler/loopr-v4/docs/design/2026-04-16-director-agent-phase4-handoff.md`.
  - v5 references: vision.md and [roadmap.md](roadmap.md) Beyond First Gate "Director agent. Escalation handling; before this ships, escalation is 'exit with error.'"
- **Keywords to grep.** `director`, `Director`, `DirectorAction`, `DirectorPatternTracker`, `PatternTracker`, `ReviseWork`, `ReDecompose`, `AbandonWork`, `SpawnResearcher`, `PlanIntake`, `Monitoring`, `Escalation`, `UserIntervention`.
- **Acceptance criteria for the design doc.**
  - Defines the four modes and how a single Director instance switches between them.
  - Defines the pattern tracker storage (in-memory only? persistent?).
  - Defines the action vocabulary and its mapping to FSM transitions.
  - Defines the threshold logic that triggers escalation (probably configurable).
  - Defines the relationship to Coordinator (does Coordinator emit `escalate_to_director` or does Director auto-monitor?).
  - Defines lifeguard variant for Director (broadcast lag handling).
  - Specifies migration: which Coordinator (1.2) responsibilities move to Director, which stay.

### 3.2 Typed event bus

- **Proposed filename:** `docs/design/<YYYY-MM-DD>-event-bus.md` **Status: Not shipped as specified.** A `DaemonEvent` type and a *client-facing* broadcast exist (`crates/ipc/src/envelope.rs`; `crates/loopr/src/transport/server.rs` streams events to connected IPC clients with `RecvError::Lagged` handling); but the *internal agent event-bus* this entry specifies (Director/Coordinator reacting to state changes instead of polling TaskStore) is NOT built. The Director still polls, per the Phase 1 design. This entry stays open for the poll->subscribe migration; the wire-level `DaemonEvent` plumbing is a head start, not the feature.
- **Crates touched:** `domain` (DaemonEvent variants), `loopr` (broadcast::Sender), `agents` (subscribers), `telemetry` (event emission alongside spans)
- **Depends on:** 1.2 (Coordinator is the first non-trivial subscriber)
- **What it covers.** A `tokio::sync::broadcast` channel carrying `DaemonEvent` enum (already partially present in vision.md: "Anthropic leaked primitive #6"). Variants: Plan/Work/Bundle/Tick state changes, agent spawns and terminations, validation results. Subscribers: Coordinator (state-change driven instead of polling), Director Monitoring mode, eventual TUI. Doc must define: variants, retention semantics (how many lagged events before reconciliation kicks in), `RecvError::Lagged` recovery, and how subscribers reconcile from store after lag.
- **Source material.**
  - v4: `~/repos/scottidler/loopr-v4/` search for `broadcast::Sender<DaemonEvent>`, `DaemonEvent` enum.
  - v4 design pattern: Director's "subscribes to broadcast channel" with `RecvError::Lagged` reconciliation per `2026-04-16-director-agent.md` lines 142-169.
  - v5 references: vision.md "typed event bus (Anthropic leaked primitive #6)"; [roadmap.md](roadmap.md) "Subscribers react to state-change events instead of polling TaskStore"; `crates/telemetry/CLAUDE.md` (the spans-vs-events distinction).
- **Keywords to grep.** `DaemonEvent`, `broadcast::Sender`, `subscribe`, `RecvError::Lagged`, `reconcile`.
- **Acceptance criteria for the design doc.**
  - Defines the `DaemonEvent` variant set.
  - Defines retention / channel size.
  - Defines lag reconciliation contract.
  - Specifies the relationship to telemetry spans (not redundant; events are state changes, spans are causality).
  - Names initial subscribers and the migration path for Coordinator from polling.

### 3.3 Parallel Implementers and parallel worktrees

- **Proposed filename:** `docs/design/<YYYY-MM-DD>-parallel-execution.md`
- **Crates touched:** `worktree`, `agents`, `loopr` (daemon JoinSet management), `tools` (lane contention)
- **Depends on:** 1.1 (dep gate enforces topological order even with parallelism), 1.2 (Coordinator owns the dispatch decision), 3.2 (event bus carries termination signals)
- **What it covers.** Today, Stage 7's exit criterion declares "serial dispatch per Plan." Stage 9+ unblocks this. Doc must define: how many concurrent Implementers per Plan (workers pool size, configurable), worktree allocation under contention (registry already exists per `2026-04-21-worktree-lifecycle.md`; verify it scales), tool-lane contention (heavy lane has 1 slot today; with parallel implementers, multiple Works compete), shared-state hazards (two Implementers editing the same file in different worktrees is fine; both committing to the integration branch is the merge story already handled). Also cleans up the worktree-lifecycle.md follow-ups: `worktrees ls` CLI, orphan branch cleanup, immediate cleanup for crash-interrupted attempts.
- **Source material.**
  - v3: `~/repos/scottidler/loopr/src/daemon/work_queue.rs` for the worker-pool shape.
  - v5 references: [docs/design/2026-04-21-worktree-lifecycle.md](design/2026-04-21-worktree-lifecycle.md) "Parallel worktrees - Stage 7 is serial per vision.md line 598; Stage 9+ is parallel"; [docs/design/2026-04-21-implementer.md](design/2026-04-21-implementer.md) "Parallel Implementer loops - Stage 9+"; vision.md and [roadmap.md](roadmap.md) "Parallel worktrees - Multiple Works running simultaneously."
- **Keywords to grep.** `parallel`, `concurrent`, `JoinSet`, `worker_pool`, `lane_contention`, `SemaphorePermit`.
- **Acceptance criteria for the design doc.**
  - Defines the per-Plan and global Implementer concurrency limits.
  - Defines worktree-registry behavior under contention.
  - Defines the worktree-followups cleanup.
  - Specifies test coverage for two-Work concurrent execution with overlapping file edits.

### 3.4 Spec / Phase hierarchy

- **Proposed filename:** `docs/design/<YYYY-MM-DD>-spec-phase-hierarchy.md`
- **Crates touched:** `domain` (Spec, Phase records, FSMs), `decomposer` (multi-level), `store` (new collections), `context` (prompt assembly per level), `agents` (Coordinator handles Phase activation)
- **Depends on:** 1.2 (Coordinator gates Phase activation), 2.2 (re-decompose at Spec/Phase level)
- **What it covers.** Pulls v5's flat Plan -> Work into Plan -> Spec -> Phase -> Work. Both v3 and v4 had this. Doc must define: Spec record, Phase record, FSM for each (Spec status, Phase status), the decomposer's strategy selector that decides which level to decompose to, markdown emission at each level (deferred per scope memo D5 in [docs/design/2026-04-20-hierarchy.md](design/2026-04-20-hierarchy.md)), Phase activation gates, dependency semantics across levels.
- **Source material.**
  - v3: `~/repos/scottidler/loopr/src/domain/` for the four-level shape.
  - v4: `~/repos/scottidler/loopr-v4/src/domain/` and `src/agents/decomposer.rs` (9KB, the rule-driven decomposer that picks levels).
  - v5 references: [docs/design/2026-04-20-hierarchy.md](design/2026-04-20-hierarchy.md) "Spec record and Phase record - deferred"; [docs/design/2026-04-20-plan-then-decompose.md](design/2026-04-20-plan-then-decompose.md) "scope memo D11 defers Spec/Phase entirely"; [docs/design/2026-04-20-stage-6-scope.md](design/2026-04-20-stage-6-scope.md) "Flat Plan -> Work only in Stage 6."
- **Keywords to grep.** `Spec`, `Phase`, `SpecStatus`, `PhaseStatus`, `decompose_strategy`, `DecomposeStrategy`, `multi_level`.
- **Acceptance criteria for the design doc.**
  - Defines `Spec` and `Phase` records and FSMs.
  - Defines the decomposer's strategy selector.
  - Defines markdown emission at each level (resolves D5 deferral).
  - Defines Phase activation semantics (Phase becomes active when its Spec is active and its preceding Phases are Done).
  - Defines dependency semantics: do Works depend only on sibling Works, or across phases / specs?

---

## Tier 4: Beyond First Gate

These items map directly to vision.md's "Deferred Enhancements" and "Beyond First Gate (earned features)" lists. Each is its own design doc; most are already named in vision.md or [roadmap.md](roadmap.md).

### 4.1 TUI

- **Proposed filename:** `docs/design/<YYYY-MM-DD>-tui-crate.md` (likely the first of multiple TUI docs)
- **Crates touched:** new `tui` crate, plus `loopr` (spawn / handoff), `ipc` (subscription protocol)
- **Depends on:** 3.2 (event bus carries TUI subscription)
- **What it covers.** A Ratatui app that subscribes to the event bus and renders Plan / Work / Bundle / Tick state in real time, with keybindings for retry / approve / reject / inspect. Vision.md frames it as a separate crate. Likely needs companion docs for: tool-output streaming (`DaemonEvent::ToolOutputChunk`), permission tier UI, retry/approve keybindings. Doc must define: launch shape (`loopr tui` subcommand vs. dedicated binary), attach/detach against running daemon, layout, keybindings, error display.
- **Source material.**
  - v4: any TUI work in `~/repos/scottidler/loopr-v4/`.
  - v5 references: vision.md "TUI" entry; [roadmap.md](roadmap.md) "TUI" Beyond First Gate; [crates/loopr/CLAUDE.md](../crates/loopr/CLAUDE.md) "TUI - deferred to its own future crate. When it lands, loopr may spawn or exec into it; rendering never lives here"; [docs/design/2026-04-21-tool-registry.md](design/2026-04-21-tool-registry.md) deferrals tied to TUI consumer.
- **Keywords to grep.** `tui`, `TUI`, `ratatui`, `ToolOutputChunk`, `permission_tier`.
- **Acceptance criteria for the design doc.**
  - Defines the new crate, its dependencies, its place in the workspace.
  - Defines launch UX.
  - Defines event-subscription protocol.
  - Defers tool-streaming and permission UI to follow-up docs if scope grows too large.

### 4.2 AutoResearch harness

- **Proposed filename:** `docs/design/<YYYY-MM-DD>-autoresearch.md`
- **Crates touched:** new `autoresearch` crate (probably), `llm` (model-tier resolution), `context` (prompt sweeping)
- **Depends on:** 2.4 (multi-turn), 4.5 (LLM cache makes sweeps tractable)
- **What it covers.** Vision.md describes this as "config sweeping + scoring, not YAML-composed orchestration." Sweeps prompts / models / parameters across a fixed set of e2e targets, scores results, surfaces winners. Replaces v4's YAML rule-driven decomposer experiments. Doc must define: target set, scoring rubric, sweep grid, output format.
- **Source material.**
  - v5 references: vision.md "AutoResearch harness" Deferred Enhancements; [roadmap.md](roadmap.md) Beyond First Gate; [docs/design/2026-04-20-llm-client.md](design/2026-04-20-llm-client.md) "Tier resolution lands when AutoResearch... needs to sweep configurations."
- **Keywords to grep.** `autoresearch`, `AutoResearch`, `sweep`, `score`, `model_tier`.
- **Acceptance criteria for the design doc.**
  - Defines the sweep grid and the targets it runs against.
  - Defines the scoring function.
  - Defines the model-tier resolution that this work motivates.

### 4.3 Prompts on disk migration

- **Proposed filename:** the doc is already drafted at [docs/design/2026-04-24-prompts-on-disk.md](design/2026-04-24-prompts-on-disk.md). This entry exists to track that the migration itself remains deferred.
- **Crates touched:** `context`, `loopr` (init seeding)
- **Depends on:** 2.4 (multi-turn, since handlebars partials per turn complicate the templating)
- **What it covers.** Move inline Rust prompt constants to `.pmt` files resolved through three layers: `.loopr/prompts/` -> `~/.config/loopr/prompts/` -> baked-in via `include_dir!()`. Use handlebars-rust as the engine. Vision.md and [roadmap.md](roadmap.md) deferral signal: "edit prompt, cargo install, rerun" must become painful enough.
- **Source material.**
  - Existing draft: [docs/design/2026-04-24-prompts-on-disk.md](design/2026-04-24-prompts-on-disk.md).
  - vision.md "Prompts" section.
- **Keywords to grep.** `pmt`, `handlebars`, `include_dir`, `render_system_prompt`, `TODO(pmt-migration)`.
- **Acceptance criteria for the design doc.**
  - Promote the existing draft from Status: Draft to Implemented when the migration ships.

### 4.4 LLM response cache

- **Proposed filename:** `docs/design/<YYYY-MM-DD>-llm-cache.md`
- **Crates touched:** `llm`, optionally a new `cache` crate
- **Depends on:** 2.4 (cache key includes the message history hash)
- **What it covers.** Cross-repo prompt-hash-keyed cache at `~/.local/share/loopr/llm-cache/`. Doc must define: cache key (hash of model + system + messages + tools schema), eviction, opt-in / opt-out, behavior on cache miss with quota exceeded.
- **Source material.** vision.md "LLM response cache"; [roadmap.md](roadmap.md) "Cross-repo prompt-hash dedup."
- **Keywords to grep.** `cache`, `llm_cache`, `prompt_hash`.
- **Acceptance criteria for the design doc.**
  - Defines key, value, eviction.
  - Defines hit-rate observability.

### 4.5 Global runs-index

- **Proposed filename:** `docs/design/<YYYY-MM-DD>-global-runs-index.md`
- **Crates touched:** `loopr`, `telemetry`
- **Depends on:** none
- **What it covers.** Cross-repo index at `~/.local/share/loopr/runs-index.jsonl` enabling `loopr runs list --all`. Doc must define: schema, write cadence, query shape, retention.
- **Source material.** vision.md "Global runs-index"; [roadmap.md](roadmap.md).
- **Keywords to grep.** `runs_index`, `runs list --all`.
- **Acceptance criteria for the design doc.**
  - Defines schema and writers.
  - Defines query CLI.

### 4.6 Supersession pattern

- **Proposed filename:** `docs/design/<YYYY-MM-DD>-supersession.md`
- **Crates touched:** `domain`, `store`, `derive` (the Record macro likely picks up a `superseded_by` field)
- **Depends on:** 3.4 (Spec/Phase use this most)
- **What it covers.** Cloudflare-pattern record revisions with forward pointers instead of deletion. Doc must define: `superseded_by` field convention, query semantics ("latest live revision"), migration story for existing records.
- **Source material.** vision.md "Supersession over deletion"; [roadmap.md](roadmap.md).
- **Keywords to grep.** `superseded`, `Supersede`, `Cloudflare`.
- **Acceptance criteria for the design doc.**
  - Defines the field convention.
  - Defines query semantics in `store`.
  - Defines migration impact on existing JSONL.

### 4.7 Graph memory

- **Proposed filename:** `docs/design/<YYYY-MM-DD>-graph-memory.md`
- **Crates touched:** `store`, possibly a new `graph` crate
- **Depends on:** 4.6 (supersession's forward pointers are graph edges)
- **What it covers.** Grafeo-pattern indexed lookups for record recall. Replaces LLM-based "find similar records" with graph traversal at microsecond scale. Doc must define: edge types, index storage, query API.
- **Source material.** vision.md "Graph memory for record recall"; [roadmap.md](roadmap.md).
- **Keywords to grep.** `graph`, `Grafeo`, `Cersei`.
- **Acceptance criteria for the design doc.**
  - Defines edge model.
  - Defines storage.
  - Defines query API.

### 4.8 Keychain integration

- **Proposed filename:** `docs/design/<YYYY-MM-DD>-keychain.md`
- **Crates touched:** `llm`, `loopr` (config)
- **Depends on:** none
- **What it covers.** API key resolution from system keychain (macOS Keychain, Linux Secret Service, etc.) instead of env / config. Doc must define: precedence order, fallback when keychain is unavailable, migration from env-var-based config.
- **Source material.** vision.md "keychain integration is deferred."
- **Keywords to grep.** `keychain`, `secret_service`, `keyring`.
- **Acceptance criteria for the design doc.**
  - Defines the precedence chain.
  - Defines the fallback path.

---

## Tier 5: small / opportunistic

These items are paragraph-sized. Most should fold into adjacent docs rather than getting their own. Listed here so they are not lost.

- **`--as` role-override flag.** From [docs/design/2026-04-19-cli-skeleton.md](design/2026-04-19-cli-skeleton.md). Folds into 1.2 Director Phase 1 (Director is the agent that emits role overrides).
- **`loopr worktrees ls` subcommand.** From [docs/design/2026-04-21-worktree-lifecycle.md](design/2026-04-21-worktree-lifecycle.md). Folds into 3.3 parallel execution.
- **`loopr prompts edit` command + init seeding `.loopr/prompts/`.** From [roadmap.md](roadmap.md). Folds into 4.3 prompts-on-disk.
- **FSM `on_enter` / `on_exit` hooks.** From [docs/design/2026-04-20-fsm-macro.md](design/2026-04-20-fsm-macro.md). Earn when a use case shows up.
- **FSM `dot` / graphviz export.** From [docs/design/2026-04-20-fsm-macro.md](design/2026-04-20-fsm-macro.md). Earn when documentation needs it.
- **FSM runtime guards.** From [docs/design/2026-04-20-fsm-macro.md](design/2026-04-20-fsm-macro.md). Probably earned by 1.1 (dep gate is the first concrete guard).
- **Tool derive macro.** From [docs/design/2026-04-21-tool-registry.md](design/2026-04-21-tool-registry.md). Earn at 15+ tools.
- **Dynamic tool registration via IPC.** From [docs/design/2026-04-21-tool-registry.md](design/2026-04-21-tool-registry.md). Earn when third-party tool plugins are wanted.
- **More builtins.** From [docs/design/2026-04-21-tool-registry.md](design/2026-04-21-tool-registry.md). Earn per-builtin when a real run wants it.
- **Cross-provider tool-schema normalizers** (OpenAI, Gemini). From [docs/design/2026-04-21-tool-registry.md](design/2026-04-21-tool-registry.md). Earn when a non-Anthropic backend ships.
- **Per-role crate split.** From `crates/agents/CLAUDE.md`. Earn when `crates/agents/src/` exceeds 1500 lines.
- **Cargo / npm / otto autodetection.** From [docs/design/2026-04-21-tool-registry.md](design/2026-04-21-tool-registry.md). Probably absorbed into 1.4 validation execution.
- **Streaming `DaemonEvent::ToolOutputChunk`.** From [docs/design/2026-04-21-tool-registry.md](design/2026-04-21-tool-registry.md). Folds into 4.1 TUI.
- **Permission tier UI / approval flows.** From [docs/design/2026-04-21-tool-registry.md](design/2026-04-21-tool-registry.md), `crates/tools/CLAUDE.md`. Folds into 4.1 TUI.
- **`extract_referenced_signatures` for Reviewer.** From [docs/design/2026-04-22-reviewer.md](design/2026-04-22-reviewer.md). Earn when reviewer false-rejects from missing context.
- **Cross-process OCC and git contention.** From [docs/design/2026-04-22-reviewer.md](design/2026-04-22-reviewer.md), [docs/design/2026-04-22-integrator.md](design/2026-04-22-integrator.md). Out of scope until multi-daemon-per-target is wanted.
- **Protocol evolution beyond v1, full method vocabulary, record-typed payloads, outer message discriminator.** From [docs/design/2026-04-19-protocol.md](design/2026-04-19-protocol.md). Earn each per use case.
- **Streaming chunked IPC responses.** From [docs/design/2026-04-19-protocol.md](design/2026-04-19-protocol.md). Folds into 4.1 TUI or 3.2 event bus.
- **`trait_variant` cleanup of async-fn-in-trait desugaring.** **Status: Implemented** ([docs/design/2026-05-08-trait-variant-cleanup.md](design/2026-05-08-trait-variant-cleanup.md)). Replaced the manual `fn method<'a>(&'a self, ...) -> impl Future<...> + Send + 'a` pattern with `#[trait_variant::make(Send)]` plus plain `async fn` across 8 traits and 52 sites in `store`, `integrator`, `agents`, `llm`, `loopr`. Dropped roughly 50 of the workspace's 133 lifetime annotations and removed every `#[allow(clippy::manual_async_fn)]`. Mechanical, no API change, no runtime cost.

---

## Dependency graph

Build order across all tiers. Dependencies flow top-to-bottom; an item cannot be designed before everything it depends on.

```
Tier 1
  1.1 dependency-gate          (no deps)
  1.4 validation               (no deps; can run parallel with 1.1)

Tier 2
  2.4 multi-turn-llm           (no deps; gating Tier 2 item)
       |
       v
  1.2 director-phase-1         (depends on 1.1, 2.4)   [routine orchestration]
       |
       v
  1.3 recovery-loop            (depends on 1.2)
  2.3 sla-tracking             (depends on 1.2)
  2.1 researcher               (depends on 2.4)
  2.2 redecompose              (depends on 1.2; later tightened by 3.4)

Tier 3
  3.2 event-bus                (depends on 1.2)
       |
       v
  3.1 director-phase-2         (depends on 1.2, 2.4, 3.2)   [judgment plane]
  3.3 parallel-execution       (depends on 1.1, 1.2, 3.2)
  3.4 spec-phase-hierarchy     (depends on 1.2, 2.2)

Tier 4
  4.1 tui-crate                (depends on 3.2)
  4.2 autoresearch             (depends on 2.4, 4.4)
  4.3 prompts-on-disk          (depends on 2.4; existing draft)
  4.4 llm-cache                (depends on 2.4)
  4.5 global-runs-index        (no deps)
  4.6 supersession             (depends on 3.4)
  4.7 graph-memory             (depends on 4.6)
  4.8 keychain                 (no deps)
```

The critical path through the orchestration plane: `2.4 -> 1.2 -> 3.2 -> 3.1`. Without 2.4 the LLM crate cannot carry multi-turn history; without 1.2 (Director Phase 1) there is nothing for state changes to drive; without 3.2 Director Phase 2 cannot subscribe to events; without 3.1 the v4-parity judgment plane does not exist.

---

## Cross-cutting concerns

Every doc in this roadmap inherits the v5 working rules:

- **Seam tests, not only unit tests.** Per repo CLAUDE.md, every crate boundary touched gets a round-trip serde test and an integration test.
- **No coexistence migrations.** Per repo CLAUDE.md, paradigm changes replace their predecessor in one commit, not dual-pathed.
- **`#[tracing::instrument]` coverage.** Per [docs/design/2026-04-24-instrumentation-sweep.md](design/2026-04-24-instrumentation-sweep.md), every non-trivial public + crate-private function in every touched crate gets instrumented at the level appropriate for its role.
- **Status field on every doc.** Per [docs/CLAUDE.md](CLAUDE.md), `Draft | In Review | Implemented | Superseded`.
- **`Crates touched:` line.** Per [docs/CLAUDE.md](CLAUDE.md), every doc names every crate it affects, even when it is only one.

---

## How to update this doc

When a future design doc is written for an item:

1. Update its entry's filename from the placeholder `<YYYY-MM-DD>-<slug>.md` to the actual dated filename.
2. Add a "Status: Drafted" or "Status: Implemented" marker to the entry.
3. Move completed entries to a "Completed" section at the bottom of their tier (do not delete; preserve the index).
4. If the design doc reveals new follow-on work, add an entry here for the follow-on.
5. If the design doc supersedes an existing v5 design doc, mark the superseded one in [roadmap.md](roadmap.md) as `Superseded by <new doc>`.

When new deferrals appear in newly-shipped design docs, add their entries here with the same template.

---

## See also

- [vision.md](vision.md) - architectural shape and the canonical "Deferred Enhancements" / "Explicitly Not in First Gate" lists.
- [roadmap.md](roadmap.md) - stage-by-stage build order; the "Beyond First Gate (earned features)" section is the abbreviated form of this doc.
- [CLAUDE.md](../CLAUDE.md) - project rules and crate map.
- [docs/CLAUDE.md](CLAUDE.md) - design-doc conventions.
- `~/repos/scottidler/loopr/` - v3 reference repo (latest at `18dd47c`).
- `~/repos/scottidler/loopr-v4/` - v4 reference repo (Director-era).
