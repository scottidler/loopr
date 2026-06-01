# Design Document: First-Gate Hardening + Failure-Path Tests

**Author:** Scott Idler
**Date:** 2026-05-31
**Status:** Draft
**Review Passes Completed:** 5/5 (converged)
**Crates touched:** integrator, loopr, domain, tools, telemetry, llm, agents

## Summary

The First Gate (Stage 9) passed on v0.7.21, but only on a trivial target: one
Work, zero dependencies, one attempt, no failure path. The entire
Director/recovery/retry stack built across v0.7.11–v0.7.21 has never run against
a multi-Work plan or a single failure. This doc hardens the defects the one
passing run exposed (client-facing timeout on success, ~16.6s Director poll
dead-air, an undecided main-promotion contract, missing tool/lane spans) and
adds controlled in-process tests that exercise the failure paths, so the next
non-trivial e2e is a confirmation rather than a discovery.

## Problem Statement

### Background

The 2026-05-30 e2e run (`rust-version`, v0.7.21) completed end-to-end in 46s:
decompose → implement → review → integrate → Tick, fully autonomous. It proved
the happy path on a toy target. It also surfaced, in the run telemetry, several
frictions that are confirmed facts rather than hypotheses:

- `loopr plan "..."` returned `error: request timed out after 10s` followed by a
  broken pipe, even though the daemon completed the plan. The client gave up at
  its 10s budget while the synchronous decompose (an LLM call) took ~18s.
- The Director sat idle for ~16.6s between the reviewer accepting the Bundle
  (`23:23:56`) and the Director dispatching `accept_bundle` (`23:24:14`). The
  decision itself took 1.35s; the rest was the Director sleeping through a poll
  cycle it had no event to wake from.
- The Tick landed on `loopr/plan-<id>` and `main` never advanced — consistent
  with v5's per-Plan integration-branch model, but the "merges to main" clause
  in the Stage 9 goal was never satisfied and the intended terminal state was
  never decided.
- `events.log` carried no per-tool or per-lane spans, only `LaneRouter
  initialized`, despite the 2026-04-24 instrumentation-sweep doc mandating them.

Separately, the run was trivially small (one Work, no dependencies,
`attempt_count=1`, no retry). The recovery machinery — Blocked-Work re-drive,
rejected-Bundle retry, the `max_work_attempts` budget, the stuck-state sweep,
the pattern tracker — has unit and component coverage but has never been
exercised as a *composition*: a multi-Work plan where a Work fails mid-DAG,
recovers, and downstream dependents still unblock. The single passing e2e did
not stress any of it.

### Problem

Two distinct problems, addressed together because they share the same goal —
making the First Gate trustworthy on non-trivial targets:

1. **Known happy-path defects** ship in every run today: a timeout error on
   success, ~36% of wall-clock lost to Director poll latency, an undecided
   main-promotion contract, and an observability gap that will blind any harder
   run's diagnosis.
2. **The failure paths are unproven.** The recovery stack has never run
   end-to-end against a failure, and the build has outrun the validation.

### Goals

- `loopr plan` returns cleanly and immediately on success.
- The Director reacts to state changes promptly, not on a poll cycle.
- The main-promotion contract is decided, documented, and enforced; an operator
  override exists for working directly on the target branch.
- Tool/lane spans are present (or the absence is understood and corrected).
- The three core failure-path compositions have deterministic, fast, in-process
  tests that run in CI.

### Non-Goals

- **The non-trivial e2e run itself (#4).** Deferred. It is the capstone that
  runs *after* these phases land, so that a failure cleanly implicates the
  recovery logic rather than a known wart. Tracked in `deferred-roadmap.md`.
- **The full typed event bus (#2b, deferred-roadmap 3.2).** Phase A is the
  narrow-mechanism interim (wake the existing per-Plan `Notify`); the
  poll→subscribe migration to a `DaemonEvent` broadcast remains deferred until a
  real run motivates it.
- **Parallel execution (deferred-roadmap 3.3).** Dispatch stays serial per Plan.
  Phase F's stub keying makes the tests *ready* for parallelism but does not
  introduce it.
- **Auto-promotion to `main`.** Explicitly rejected in Phase C: loopr never
  merges to the target's `main` itself.

## Proposed Solution

### Overview

Six phases. **The phase letters are identifiers, not execution order** — the
recommended order is in [Implementation Plan](#implementation-plan) (F → B → E →
A → C → D). Phase F (stub keying) is a prerequisite for two of the Phase E
tests; Phase B lands before E so the tests are written against the final
async-ACK contract (Architect review, 2026-05-31). Phases A, C, D are
independent hardening fixes. The order keeps the tests ahead of the bulk of the
hardening so a recovery bug they surface can reshape it before it is written.

| Phase | What | Crates |
|---|---|---|
| A | Director event-driven wake-up | loopr, agents |
| B | async `plan.create` ACK | loopr |
| C | integration-branch policy + skip-branch override | integrator, loopr, domain |
| D | tool/lane span instrumentation (confirm-first) | tools, telemetry |
| E | controlled failure-path tests (3 scenarios) | loopr (tests) |
| F | `ScriptedLlm` prompt-content keying | llm |

### Architecture

#### Phase A — Director event-driven wake-up

The Director loop sleeps `poll_interval_secs` after acting and
`idle_interval_secs` when idle (`crates/agents/src/director.rs`). It already owns
a per-Plan wake-up channel: `DaemonContext::operator_notifies:
Arc<RwLock<HashMap<PlanId, Arc<Notify>>>>`, with one `Arc<Notify>` per live
Director task. Today only `handle_director_chat` fires it (`notify_one()` after
persisting an operator note). Every other state change the Director cares about
goes unsignalled, so the Director discovers it on the next poll tick.

The fix: at each Director-actionable transition, look up the Plan's `Notify` in
the wake map and call `notify_one()`. The transition sites live in
`crates/loopr/src/daemon/context.rs` (`transition_and_persist_work`,
`transition_and_persist_plan`, and the reviewer-accept handoff at ~`:689`). The
Director's `run_once` dispatches exactly three triggering actions —
`AcceptBundle`, `OverrideWork`, `AssignWork` (`director.rs:914-966`); `Done` and
`NeedHelp` are terminal outputs, not triggers. The state changes that should
wake it map to those:

- Bundle → `Reviewed` (the measured case — reviewer accepted, drives
  `AcceptBundle`).
- Bundle → `Rejected` (drives `OverrideWork` retry-vs-abandon).
- Work → `Blocked` (drives `OverrideWork` recovery).
- Work → `Ready` / sibling unblock (a newly-assignable Work drives `AssignWork`).

A `Notify` lookup miss is benign (the same contract `handle_director_chat`
already relies on at `handler.rs:423`): the next poll picks the change up
regardless, so the wake is a latency optimization, never a correctness
dependency. `notify_one()` (not `notify_waiters()`) targets exactly the one
Director task for that Plan. The wake map is a tokio (async) `RwLock`, so the
helper acquires it with `.read().await`.

**Ordering invariant (critical):** the wake fires *after* the transition's
persist completes, never before. The Director, on waking, reads state from the
store; a wake issued before the write lands would race the Director against its
own trigger and it could re-sleep on stale state. `notify_one` stores a single
permit, so multiple transitions firing before the Director next waits collapse
to one wake — no busy-loop, no missed wake.

This is the narrow mechanism. The eventual #2b event bus replaces "fire a
per-Plan `Notify` at each transition site" with "publish a `DaemonEvent`;
subscribers react," but the reactive *semantics* land here.

#### Phase B — async `plan.create` ACK

`handle_plan_create` (`crates/loopr/src/transport/handler.rs:~155-244`) today
runs this whole sequence before responding:

1. `ensure_integration_branch` (`:161`),
2. persist the Plan (`:169`),
3. `await` `decomposer::decompose(...)` (`:178`, the ~18s LLM call),
4. persist Works + dep-gate partition (`:181-203`),
5. spawn an Implementer per unblocked Work (`:213-217`),
6. `spawn_director_for_plan` (`:219`),
7. return `PlanCreateResult { plan }` (`:239`).

The client's single global request budget (`client_request_secs: 10`,
`crates/loopr/src/config.rs:95`) fires at step 3, so the user sees a timeout on
every success. The daemon, however, keeps going after the client disconnects
(the run completed despite the broken pipe) — so steps 3-6 are *already*
effectively detached from the client's fate.

The fix: make that detachment explicit. The ACK boundary moves to right after
the Plan persist:

1. `ensure_integration_branch` + persist the Plan stay synchronous (the ACK
   genuinely confirms the Plan landed on disk and its branch exists),
2. steps 3-6 (decompose → persist Works → dep-gate → spawn Implementers + spawn
   Director) move into a daemon-owned spawned task. Because this task spawns both
   Implementers and a Director, it sits at the **root of the spawn DAG**, so its
   drain (`drain_plan_create_tasks`) runs **first** in the shutdown sequence —
   ahead of `implementer → reviewer → director → work_spawner → integrator` —
   preserving the "no pool receives a new spawn after its drain returns"
   invariant. The drain is bounded by a **soft timeout** so a hung decompose
   (an LLM call) cannot block shutdown indefinitely, consistent with the IPC/
   startup timeout posture in `2026-05-09-ipc-timeouts.md`.
3. return `PlanCreateResult { plan }` immediately.

**The response payload is unchanged** — it already carries only the `plan`
(`work_count`/`ids` are merely logged at `:204`, never returned). So no `ipc`
type changes. Post-ACK, the user observes Works via the existing `loopr works
<plan-id>` once decompose completes. This restores the Stage 4 ACK-and-exit
intent (`docs/design/2026-04-19-daemon-stage-4.md`).

**Preserved edge case:** the decompose-failure path
(`handler.rs:229-236`) logs and spawns *neither* Implementers nor a Director,
leaving an `Active` Plan with no Works until Stage 7 reconcile. Moving the chain
to a background task must preserve this exactly — the failure is still
non-fatal, the Plan still persists, and the client (already ACK'd) is unaffected.
The only behavioral change is *where* the failure is observed: logs + on-disk,
never the client response (which it already wasn't, post-disconnect).

#### Phase C — integration-branch policy + skip-branch override

Decision: **loopr never touches the target's `main`.** Two modes:

- **Default (integration branch):** loopr creates the per-Plan branch
  `loopr/plan-<id>` (already built at `crates/integrator/src/lib.rs:223`), does
  all work there, produces a Tick on that branch, and **the operator merges the
  branch to `main`** by hand. No auto-promote step exists.
- **Override (no branch):** a config/CLI flag skips branch creation; the
  integrator merges accepted Bundles directly onto the target repo's
  currently-checked-out branch (e.g. `main`).

The default is already the implemented behavior; the work is (a) making the
contract explicit in CLI output so "Plan complete" is never read as "on main",
and (b) adding the override code path. The branch is touched at **two** sites,
both of which the override must short-circuit:

- `crates/loopr/src/daemon/git.rs::ensure_integration_branch` — *creates*
  `loopr/plan-<id>`, called from `handle_plan_create:161`. With the override on,
  this becomes a no-op (stay on the checked-out branch).
- `crates/integrator/src/lib.rs:223` — builds the *merge target* branch name.
  With the override on, the integrator targets the checked-out branch instead.

The override threads a single boolean from config/CLI to both sites. Routing is
structurally sound: `crates/integrator/src/lib.rs` receives an `IntegratorDeps`
carrying `IntegratorConfig` (`crates/integrator/src/config.rs`), built by the
daemon from `IntegratorSection`; `handle_plan_create` reads the same parsed
config via `ctx`. Adding the boolean to `IntegratorConfig` makes both sites read
one value — neither can desync. A config that creates no branch but integrates
onto `loopr/plan-<id>` (or vice versa) is made impossible by the shared field.

**Working-tree cleanliness (required for the override path).** The integrator
today has **no** explicit cleanliness check — its pre-flight
(`integrator/src/lib.rs:174`) verifies shape/status/plan/branch only. The
*default* path is implicitly protected: `git checkout loopr/plan-<id>` fails
when the working tree has conflicting uncommitted changes. The *override* path
checks out the already-current branch, where `git checkout <current>` returns
`Ok(())` even on a dirty tree — so without a new guard the Integrator would
`git merge` on top of uncommitted user changes. The override path must therefore
add an explicit `git status --porcelain` pre-flight and refuse on a dirty tree.
Open question (below): whether untracked files alone should also refuse, or only
tracked-file modifications.

Config key and flag name are settled in this phase (proposed:
`integrator.integration-branch: true` default; CLI `--no-branch` override). Per
`rules/general.md`, config keys are kebab-case; per
`feedback-config-knobs-with-defaults`, the knob is a typed boolean with a
defensible default (branch on).

#### Phase D — tool/lane span instrumentation (confirm-first)

The 2026-04-24 instrumentation-sweep doc mandates per-tool and per-lane spans,
but the e2e `events.log` showed none. **This phase begins with a confirm-first
investigation**, not a fix:

1. Determine whether the spans are genuinely unwired in `crates/tools` /
   `crates/telemetry`, or whether they simply did not fire / did not route into
   the `events.log` fanout in a 3-iteration run.
2. Only after the root cause is known, wire or correct the instrumentation.

The outcome of step 1 decides the shape of step 2; the doc does not pre-commit
to a fix it cannot yet justify.

#### Phase E — controlled failure-path tests (three scenarios)

The in-process harness already exists: `spawn_test_daemon`
(`crates/loopr/tests/common/harness.rs`) boots the real daemon (`serve_core`,
real `Store` on a tempdir, Director on a 0-interval tight loop) backed by the
`ScriptedLlm` stub. Existing tests (`stage_8_plan_to_tick.rs`,
`stage_9_director_plan_to_tick.rs`) drive `plan.create` over real IPC and poll
on-disk JSONL for terminal state — but each scripts exactly one Work. Three new
tests, same pattern:

- **Scenario 2 — reject → recover → success.** Script: decompose → one Work;
  implementer → a bundle; reviewer → reject; Director → re-drive Work to Ready;
  implementer (retry) → a good bundle; reviewer → accept; Director → accept;
  integrator → Tick. Assert the Work's `attempt_count` increments, the
  `blocked_reason` is set and reaches the retry, and the Plan reaches `Complete`.
  Works with plain FIFO (serial dispatch, one Work).
- **Scenario 1 — multi-Work DAG.** Script: decompose → Work A and Work B (B
  depends on A); A runs to Done; B unblocks, dispatches, runs to Done; Plan
  `Complete`. Asserts dependency ordering and sibling unblock end-to-end.
  Requires Phase F keying (two Works draining the implementer queue).
- **Scenario 3 — combination.** Multi-Work where one Work fails mid-DAG,
  recovers, and a downstream dependent still unblocks afterward. The most
  demanding; requires Phase F keying (re-drive desyncs FIFO order).

#### Phase F — `ScriptedLlm` prompt-content keying

`ScriptedLlm` (`crates/llm/src/stub.rs`) routes responses by model only
(`ModelKey = Option<String>`), FIFO within a model. For a multi-Work plan, both
Works' implementer calls drain the same `None` queue in pop order, so a
multi-Work test is only deterministic if dispatch order is — and re-drive
(scenario 3) breaks that. Add a prompt-content-keyed route: a queued response
may carry a match predicate (a substring expected in the user prompt, e.g. the
Work title or an AC line), and `complete_*` selects the first queued response
whose predicate matches the incoming prompt, falling back to the model-FIFO
queue when no predicate is attached. ~30 lines, behind the existing `stub` cargo
feature, fully backward-compatible with current FIFO callers.

### Data Model

- **Phase C:** one config field. `IntegratorSection { integration_branch: bool }`
  (serde `rename_all = "kebab-case"` → `integration-branch`), default `true`,
  composed into the top-level `Config`; CLI `--no-branch` overrides per the
  precedence chain (CLI > env > config > default). No record/FSM change unless
  the correctness pass finds the override needs a Tick field to record which
  branch it targeted (open question).
- **Phase F:** `ScriptedLlm` gains a keyed route alongside the existing
  model-FIFO maps. Sketch:

  ```rust
  struct Keyed<T> { needle: String, value: Result<T, LlmError> }
  // free_keyed: Arc<Mutex<Vec<Keyed<String>>>>
  // tool_keyed: Arc<Mutex<Vec<Keyed<ToolCall>>>>
  // selection: first keyed entry whose `needle` is a substring of `user`,
  // else fall back to the model-FIFO queue.
  ```

### API Design

- **Phase A:** no public API change. An internal `async` helper, e.g.
  `DaemonContext::wake_director(&self, plan_id: &PlanId)`, encapsulates the
  `.read().await` + lookup + `notify_one()`, called from each transition site.
- **Phase B:** no IPC payload change — `PlanCreateResult { plan }` is unchanged.
  The change is internal: the ACK returns after the Plan persist, and steps 3-6
  run on a spawned task. Only the handler's control flow moves.
- **Phase C:** new config field + `--no-branch` CLI flag; the branch-policy
  boolean lives on `DaemonContext` and is read by both `ensure_integration_branch`
  and `integrator::integrate` (or its branch-selection helper).
- **Phase F:** `ScriptedLlm::queue_free_keyed(needle, result)` and
  `queue_tool_keyed(needle, result)`; existing `queue_free`/`queue_tool`
  unchanged.

### Implementation Plan

Execution order: **F → B → E → A → C → D**. F (stub keying) unblocks the E
tests. **B lands before E** (Architect review, 2026-05-31): B flips
`plan.create` from synchronous to async-ACK, so writing the E tests first risks
baking in a synchronous assumption (Works exist the moment `plan.create`
returns) that B would then break — exactly as it would break the existing
`stage_8`/`stage_9` tests. Doing B first means E is written against the final
async contract from the start. E still precedes the bulk of the hardening (A, C,
D) so a recovery bug it surfaces reshapes that work. The deferred #4 e2e is the
capstone after all six.

#### Phase F: `ScriptedLlm` prompt-content keying
**Model:** sonnet
- Add keyed routes to `crates/llm/src/stub.rs` behind the `stub` feature.
- Selection: keyed-by-substring first, model-FIFO fallback; preserve all current
  callers.
- Unit tests for keyed selection, fallback, and drained-queue assertions.

#### Phase B: async `plan.create` ACK
**Model:** opus
- Move steps 3-6 (decompose → persist Works → dep-gate → spawn Implementers +
  Director) into a daemon-owned task. Drain it **first** in the shutdown
  sequence (root of the spawn DAG) under a **bounded soft timeout** so a hung
  decompose cannot block shutdown.
- Return `PlanCreateResult { plan }` after the Plan persist. No `ipc` payload
  change.
- Verify shutdown drains the new task; verify decompose failure still leaves the
  Plan persisted (existing `plan_create_with_failing_llm_still_persists_plan`
  invariant holds, now from the spawned task).
- Migrate the existing `stage_8`/`stage_9` tests if they assume synchronous
  `plan.create` (they already poll for terminal state, so the change should be
  small or nil).

#### Phase E: controlled failure-path tests
**Model:** sonnet (scenario 3 scripting is fiddly; escalate to opus if the
re-drive sequencing fights the harness)
- Scenario 2 (FIFO, one Work): reject → recover → success.
- Scenario 1 (keyed, two Works): multi-Work DAG + sibling unblock.
- Scenario 3 (keyed): mid-DAG failure + recovery + downstream unblock.
- **Poll the on-disk store for every assertion** (including that Works exist) —
  never assume synchronous return — so the tests hold against Phase B's async
  ACK. Race the poll against the daemon task so a panic in a spawned agent
  surfaces as a JoinError, per the existing `stage_8` pattern.

#### Phase A: Director event-driven wake-up
**Model:** opus
- Add an `async` `DaemonContext::wake_director(plan_id)` helper
  (`operator_notifies.read().await` + `notify_one()`).
- Audit `run_once` for the full set of Director-actionable transitions
  (`AcceptBundle`/`OverrideWork`/`AssignWork` → Bundle Reviewed/Rejected, Work
  Blocked/Ready).
- Fire the wake at each transition site in `daemon/context.rs`, **after** the
  persist completes (ordering invariant above).
- Test: a transition to a Director-actionable state wakes a Director blocked on
  its idle sleep without waiting a full interval (use a non-zero interval in this
  test so the wake is observable).

#### Phase C: integration-branch policy + override
**Model:** opus
- Add `IntegratorSection.integration_branch` config + `--no-branch` CLI flag;
  thread it into `IntegratorConfig` so `ensure_integration_branch` and the
  integrator read one value.
- Override path: `ensure_integration_branch` becomes a no-op; the integrator
  targets the checked-out branch instead of `loopr/plan-<id>`.
- **Add a `git status --porcelain` cleanliness pre-flight to the override path**
  and refuse on a dirty tree (the default path's checkout-fails-on-dirty side
  effect does not protect the same-branch case).
- Make CLI output state the deliverable branch explicitly.
- Tests: default path unchanged; override path merges onto the target branch;
  override refuses on a dirty working tree.

#### Phase D: tool/lane span instrumentation (confirm-first)
**Model:** sonnet
- **First:** investigate whether tool/lane spans are unwired vs. not-fired/not-
  routed. Document the finding.
- **Then:** wire or correct per the finding; add a span-name assertion test in
  the touched crate.

## Alternatives Considered

### Phase B alt: raise the client timeout / per-verb timeout
- **Description:** bump `client_request_secs`, or give `plan.create` its own
  longer budget, instead of async-ACK.
- **Pros:** minimal change; response keeps `work_count`/`ids`.
- **Cons:** raising the global budget is brittle (breaks the moment a decompose
  is slower); per-verb still blocks the user on an LLM call for no reason.
- **Why not chosen:** async-ACK matches the daemon's actual behavior and the
  Stage 4 design intent; coupling client latency to an LLM round-trip is the bug,
  not the timeout value.

### Phase A alt: full typed event bus now (#2b)
- **Description:** build the `DaemonEvent` broadcast and migrate the Director to
  subscribe.
- **Pros:** the structural end-state; generalizes to all subscribers and the TUI.
- **Cons:** larger blast radius (domain + loopr + agents); not needed to kill the
  measured latency.
- **Why not chosen:** the existing per-Plan `Notify` already provides the wake
  mechanism; firing it at the transition sites reclaims the latency now. The bus
  is deferred until a real run motivates the generalization.

### Phase C alt: pipeline auto-promotes to `main`
- **Description:** the daemon advances `main` after validation passes.
- **Pros:** "Plan complete" == "feature on main".
- **Cons:** loopr mutates the operator's primary branch unattended; largest blast
  radius; removes the human gate.
- **Why not chosen:** rejected by decision — loopr operates on a branch and the
  operator owns the merge to `main`.

### Phase F alt: keep FIFO, lean on deterministic dispatch
- **Description:** queue responses in dispatch order, rely on serial dispatch.
- **Pros:** zero stub change for scenario 2.
- **Cons:** scenario 3's re-drive desyncs the queue; tests pass by accident of
  ordering and rot the moment dispatch changes.
- **Why not chosen:** all three scenarios are in scope; robustness beats a
  30-line saving.

## Technical Considerations

### Dependencies
- Internal: Phase E depends on Phase F (scenarios 1, 3). Phases A/B/C/D are
  mutually independent. The deferred #4 e2e depends on all six.
- External: none new. Phase F stays behind the existing `stub` cargo feature so
  the production `loopr` binary never links it.

### Performance
- Phase A reclaims ~16.6s of the measured 46s run (the poll-wait portion; the
  1.35s decision is irreducible here).
- Phase B removes a 10s client-visible stall on every plan.
- Phases C/D/E/F have no production hot-path cost.

### Security
- No new external surface. Phase C's override lets loopr commit to the
  checked-out branch directly; the source-guard and `.loopr` excludes are
  unchanged, but the override must add an explicit `git status --porcelain`
  guard and refuse on a dirty working tree — the integrator has no cleanliness
  check today, and the default path's protection (checkout fails on a dirty
  tree) does not extend to the same-branch override (Architect review).

### Testing Strategy
- Per the v5 working rules: every touched crate boundary gets a round-trip serde
  test (Phase B's changed IPC payload; Phase C's config field) and an
  integration test crossing the seam.
- Phase E *is* the integration-test deliverable for the recovery seam.
- `#[tracing::instrument]` coverage on every non-trivial touched function, per
  `2026-04-24-instrumentation-sweep.md`.

### Rollout Plan
- Land F → B → E → A → C → D as separate commits (each phase is its own blast
  radius per working rule 3; B and C are IPC/CLI-visible and version-bump
  worthy). Then run the deferred #4 e2e as the capstone.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Phase A fires spurious wakes (every transition wakes the Director, busy-looping) | Med | Med | `notify_one` is idempotent between waits; the Director's loop is cheap when there is nothing to do; cap with the existing idle sleep as the floor |
| Phase B background decompose task escapes the shutdown drain (orphaned on SIGTERM) | Med | High | Place the task in the reverse-spawn-chain drain order; assert drain in a shutdown test |
| Phase C override merges onto a dirty target working tree and corrupts uncommitted work | Med | High | No cleanliness check exists today; the override path adds an explicit `git status --porcelain` pre-flight and refuses on a dirty tree (Architect review) |
| Phase E scenario 3 is flaky from harness/re-drive races | Med | Med | Phase F keying removes order-dependence; race the poll against the daemon JoinHandle per the `stage_8` pattern |
| Phase D confirm-first finds the spans fire but are not routed to `events.log` (a fanout bug, not a tools bug) | Med | Low | The investigation step explicitly scopes this before any edit |

## Open Questions
- [ ] Phase C: does the override need a Tick field recording which branch it
  targeted, or is the branch name on the existing Tick record enough?
- [ ] Phase C: exact config key (`integration-branch`) and CLI flag
  (`--no-branch`) names — confirm against existing `loopr` flag conventions.
- [ ] Phase C: should the override's `git status --porcelain` guard refuse on
  untracked files alone, or only on tracked-file modifications? (Untracked files
  are common and benign; tracked dirty state is the real hazard.)
- [ ] Phase B: with the ACK returning before decompose, should the daemon emit a
  `DaemonEvent` (decompose-complete) so a future TUI/`loopr works` poll knows
  when Works exist, or is the existing on-disk JSONL poll sufficient? (Leans
  toward sufficient; the event-bus surface is deferred #2b.)
- [x] Phase A: Director-actionable transitions enumerated from `run_once` —
  `AcceptBundle`, `OverrideWork`, `AssignWork`; wake on Bundle→`Reviewed`,
  Bundle→`Rejected`, Work→`Blocked`, Work→`Ready`.

## References
- `docs/roadmap.md` — Stage 9 status + the main-promotion caveat.
- `docs/deferred-roadmap.md` — #2b event bus (3.2), #3.3 parallel execution, #4
  capstone framing.
- `docs/design/2026-04-19-daemon-stage-4.md` — the ACK-and-exit intent Phase B
  restores.
- `docs/design/2026-05-09-director-phase-2.md` — the per-Plan `Notify` wake
  channel Phase A extends.
- `docs/design/2026-05-09-ipc-timeouts.md` — the `client_request_secs` budget.
- `docs/design/2026-04-24-instrumentation-sweep.md` — the tool/lane span mandate
  Phase D checks.
- `/tmp/loopr/e2e/rust-version/20260530-232319/.monitor/{results,evaluation}.md`
  — the run that surfaced these findings.
