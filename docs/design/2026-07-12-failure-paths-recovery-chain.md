# Design Document: failure_paths Recovery Chain (Link 4 + OCC Phases Fold-In)

**Author:** Scott Idler
**Date:** 2026-07-12
**Status:** In Review (panel closed 2026-07-12; awaiting owner sign-off)
**Review Passes Completed:** 5/5
**Crates touched:** agents, loopr, store

## Summary

The 3 `failure_paths` recovery tests hid a chain of latent bugs, each masked by
the one before. Links 1-3 are fixed and shipped (reviewer OCC self-stale
`5dfd3112`, integrator dirty-guard `ac42cfa3`, integrator clean-wipe
`2ba07f0f`). Link 4 is root-caused and open: the Director's no-progress
detector trips on idle waiting whenever a stale mutating action sits in its
observation window, escalating a healthy Plan to Stalled and killing the
Director before downstream work lands. Fix: a two-clause "engaged" gate on
the NoProgress trip: it counts only when the CURRENT action is mutating, or
when Director-actionable state (a Reviewed bundle | a Blocked Work) sits
unaddressed. Idle waiting on in-flight work never trips; idling on records
that await the Director's decision escalates loudly, as it should. This doc
also folds in the unstarted Phases 2-4 of
`2026-07-12-reviewer-occ-stale-race.md`, which were blocked on `failure_paths`
going green.

## Problem Statement

### Background

The chain, in unmasking order (full detail for 1-2 in the OCC doc's addendum):

1. **Link 1 (fixed, `5dfd3112`):** reviewer OCC self-stale. Every bundle wedged
   at Triaged in a ~34s doom loop.
2. **Link 2 (fixed, `ac42cfa3`):** integrator dirty-guard tripped on loopr's
   own untracked `.loopr/taskstore/`. Recovery could never reach a Tick.
3. **Link 3 (fixed, `2ba07f0f`):** the integrate success path ran an unscoped
   `git clean -fd`, deleting the untracked `.loopr/taskstore/*.jsonl` truth
   files on every Tick: Rejected-bundle audit rows, Review rows (the
   `Review.round` continuity the OCC doc's deferral assumed), and the F8
   retry-feedback source, all silently erased. Same false premise as Link 2,
   on the clean path. Fixed with the same `':(exclude).loopr/'` pathspec,
   break-to-proven. `reject_then_recover_reaches_tick` green (first time
   ever, 3 consecutive runs) and `multi_work_dag_unblocks_and_completes`
   green (3 consecutive runs).
4. **Link 4 (open, root-caused 2026-07-12):**
   `mid_dag_failure_recovers_then_unblocks_downstream` red, 0/3 isolated runs.

### Problem

Link 4 mechanism, from a WARN-level trace of a failing run (all timestamps
within ~300ms):

1. Work A's recovery works end to end: reject -> Blocked -> Director override
   -> retry -> accept -> merge -> Tick. Both A bundles persist (Rejected +
   Merged). Work B unblocks, implements, and its bundle reaches `Reviewed`.
2. Meanwhile the Director's pattern tracker
   (`crates/agents/src/director/pattern.rs`) kills the Plan:
   - `observe()` step 4 gates the NoProgress trip on **any mutating action
     anywhere in the 16-deep window** (`pattern.rs:276`:
     `self.action_history.iter().any(ActionFingerprint::is_mutating)`), not
     on the current action.
   - During A's recovery the Director legitimately acts (`override_work`,
     `accept_bundle`). Those fingerprints sit in the window for the next 16
     iterations while the Director correctly idles on `done` waiting for
     in-flight agents.
   - Record-state is static between agent writes, so `distinct <= 2` holds
     and the trip fires every idle iteration: 8 consecutive trips
     (`escalation_threshold`) -> `EscalationTripped` -> mode `NeedsOperator`.
   - `needs_operator_iters` then counts 5 idle iterations
     (`needs_operator_grace_iters`) -> Plan -> Stalled, Director exits
     NeedHelp (observed: `iteration=32`, `needs_operator_iters=5`).
3. B's `Reviewed` bundle has no Director alive to accept it. Wedge; test
   deadline.

The protection for idle waiting EXISTS but only on the SameAction path:
`consecutive_same_action` returns None for non-mutating fingerprints
(`pattern.rs:355-360`, comment: "healthy waiting, not a doom loop"), and the
`is_mutating` doc comment claims the gate means "a Director that only emits
`done` while waiting on a long Implementer does not trip"
(`pattern.rs:112-118`). The NoProgress path does not honor that claim: one
stale mutating action in the window defeats it.

**This is a production bug, not a harness artifact.** The harness's
zero-interval Director loop (`harness.rs:156-157`) compresses the death spiral
to ~300ms, but the trip arithmetic is iteration-based, not wall-clock: after
any mutating Director action, escalation completes at trip 8 and Stalled lands
at iteration ~13, all inside the 16-iteration window the stale action occupies.
At production defaults (idle iterations sleep `idle_interval_secs = 15`,
`config.rs:242`, since `done` does not set `took_action`) that is ~3 minutes
of record-static state following any accept/override: a single real
implementer run routinely exceeds that. The `stage_8`/`stage_9` flake is
plausibly this same mechanism (unverified; Phase 2 verifies).

Two secondary observations from the same trace, cataloged here so they are not
re-derived:

- A `store::works` Stale ERROR with the +1-floor signature fired during the
  post-integration window (hypothesis: the Director's `override_work` racing
  the integrator's Work-Done write; Phase 5 confirms the caller). OCC refused
  the stale write (fail-closed, correct) and the system self-healed. Not a
  wedge mechanism.
- The historical `ScriptedLlm: complete_free called with empty queue` panic in
  some failing interleavings is a downstream symptom of the stall (reconcile's
  30s-grace reviewer re-spawn consuming no matching scripted entry inside the
  40s test deadline). Expected to vanish with the Link 4 fix; Phase 2 confirms.

### Goals

- `mid_dag_failure_recovers_then_unblocks_downstream` green; all 3
  `failure_paths` tests green, 3 consecutive isolated runs each.
- A Director waiting on in-flight work can never escalate a healthy Plan to
  Stalled. Genuine doom loops (repeated mutating action, no effect) still
  escalate to NeedsOperator.
- `stage_8_plan_to_tick` / `stage_9_director_plan_to_tick` stable: 3
  consecutive green runs each; the flake's cause confirmed as this mechanism
  or cataloged as a new link.
- OCC doc Phases 2-4 (regression tests, loud-fail Stale discrimination, live
  e2e) land; that doc's remaining plan is folded in here and marked
  superseded there.
- Root `otto ci` green (workspace `cargo test` includes `failure_paths`).

### Non-Goals

- Committing `.loopr/taskstore` into the target repo (merge driver + hooks
  exist in taskstore, nothing wires them). Vision-level feature, separate
  design. Revisit condition: designing the taskstore-commit flow.
- Multi-reviewer / multi-daemon support and the `Review.round` allocation
  race: stays deferred per the OCC doc's signed-off Resolved Decision.
- Harness parity rework. Policy decided in this doc (see Resolved Decisions):
  the zero-interval harness is a legitimate stress amplifier and stays;
  robustness lands product-side. Links 2, 3, 4 were all product bugs.
- The pattern tracker's chaotic-rotation edge (`distinct=3` cycling) noted in
  `pattern.rs`: unchanged, still waits on a real trace.
- Wall-clock-based threshold conversion for the pattern tracker (Alternative
  2): rejected below, recorded with rationale.

## Proposed Solution

### Overview

Replace the window-wide gate with a two-clause "engaged" gate: an iteration
counts toward no-progress only when the Director is either acting without
effect (current action mutating, hash static) or idling while records await
its decision (Director-actionable state present, hash static). Idle waiting
on in-flight work (no actionable records) never trips.

**Director-actionable state** = any bundle in `Reviewed` (only `accept_bundle`
clears it; reconcile never touches Reviewed, `director.rs:631/649/667`) or
any Work in `Blocked` (the Director must override / supersede / abandon it;
dependency-waiting Works are `Pending`, not Blocked, `work.rs:169`). Both are
computed from the same records snapshot the caller already hashes.

Doom-loop and stuck-state coverage under the new gate:

- Repeated mutating action, static state -> SameAction path (unchanged) feeds
  the shared streak and escalates to NeedsOperator.
- Mutating actions that keep executing without effect -> NoProgress trips on
  every such iteration.
- Idle `done` with a Reviewed bundle or Blocked Work rotting -> trips: this is
  the LLM-emits-done-wrongly pathology, and escalation to NeedsOperator ->
  Stalled is the correct outcome (the pre-fix tracker only caught it by
  accident, within 16 iterations of an unrelated action).
- Interleaved mutating-no-op / `done` with static hash -> the mutating
  iterations trip, and the `done` iterations also trip whenever the no-op'd
  target is still actionable, so the streak accumulates and escalates.
- Idle `done` with only in-flight / Pending state -> streak resets (the
  existing fallthrough), which is the "waiting is not a pathology" claim the
  code already documents, now actually true.

Residual gap, accepted: an interleaved loop acting on NON-actionable records
(pointless `assign_work` against a Ready Work, alternating with `done`, hash
static) resets the streak on the `done` iterations and never escalates. This
shape was uncovered before the fix too only when it stayed inside a mutating
window; it belongs to the same family as the documented chaotic-rotation edge
(`pattern.rs`) and waits on a real trace, same revisit condition.

### Architecture

`DirectorPatternTracker::observe()` gains an `actionable: bool` parameter and
replaces the window-wide gate (`crates/agents/src/director/pattern.rs:276`):

```rust
// before: any mutating action anywhere in the window
let mutating = self.action_history.iter().any(ActionFingerprint::is_mutating);
let trip = mutating && (distinct <= 2 || max_rec >= half_plus_one);
// after: acting-without-effect OR idling-on-actionable-state
let engaged = action.is_mutating() || actionable;
let trip = engaged && (distinct <= 2 || max_rec >= half_plus_one);
```

The caller (`run_director_inner`, step 6) computes `actionable` from the same
`works_after` / `bundles_after` snapshot it already feeds
`compute_state_hash`:

```rust
let actionable = bundles_after.iter().any(|b| b.status == BundleStatus::Reviewed)
    || works_after.iter().any(|w| w.status == WorkStatus::Blocked);
```

Plus the doc-comment corrections: step 4's rustdoc and the `is_mutating`
comment now describe the actual guarantee. No config knobs added or changed;
`PatternConfig` untouched.

Consequence, deliberate: a Director that idles while a Reviewed bundle or
Blocked Work awaits its decision escalates NeedsOperator -> Stalled. That is
the mode FSM doing its job: the Plan genuinely needs an operator when its
Director won't disposition actionable records (including a budget-exhausted
Blocked Work it declines to override). Stalled is loud and recoverable
(`loopr plan override <plan-id> --to active`), the opposite of the silent
Reviewed-bundle rot the panel flagged.

Coverage unchanged for the act-wait-act loop (override, idle while the Work
re-runs, re-override): record-state cycles during the re-run, so the hash
moves and the pre-fix detector never caught this shape either. The retry
budget (Layer 2, `max_work_attempts`) owns it, before and after this change.

`needs_operator_iters` (Phase 10 grace) is unchanged: it only counts while
mode is NeedsOperator, which after this fix requires a genuine sustained doom
loop, which is exactly when counting iterations toward Stalled is correct.

### Data Model

No record or schema changes. The only store-crate change is Phase 5's
log-level discrimination (WARN on Stale, ERROR otherwise); no persistence
behavior changes.

### API Design

No public API changes. `PatternObservation`, `next_mode`, `DirectorMode`
untouched.

### Implementation Plan

#### Phase 1: NoProgress engaged-gate (current action | actionable state)
**Model:** sonnet
- Apply the `observe()` gate change + `actionable` parameter + doc-comment
  corrections in `crates/agents/src/director/pattern.rs`; thread `actionable`
  from `run_director_inner`'s existing `works_after` / `bundles_after`
  snapshot.
- Unit tests in `pattern/tests.rs`, each break-to-proven against the pre-fix
  gate:
  - stale mutating fingerprint in window + current `done` + static hash + NO
    actionable state -> None, streak stays 0 (the Link 4 shape).
  - current `done` + static hash + actionable state -> trips; sustained ->
    EscalationTripped (the Reviewed-rot pathology escalates).
  - current mutating action + static hash -> trips; sustained ->
    EscalationTripped (real doom loop still escalates).
  - interleaved mutating-no-op / `done` + static hash + actionable target ->
    streak accumulates across both iteration kinds.
  - repeated identical mutating action -> SameAction path unchanged.
- **Success criteria:** new tests green; each demonstrated red with the
  pre-fix `any()` gate restored; `otto ci` green inside `crates/agents`.

#### Phase 2: recovery-suite stability gate
**Model:** sonnet
- Add to `reject_then_recover_reaches_tick`: post-Tick assert that
  `reviews.jsonl` retains both review rounds (Link 3's audit-trail claim,
  pinned by a test).
- Run each of the 5 tests 3 consecutive isolated times (one test binary
  invocation per run, single-test filter, no parallel siblings):
  `reject_then_recover_reaches_tick`, `multi_work_dag_unblocks_and_completes`,
  `mid_dag_failure_recovers_then_unblocks_downstream`,
  `stage_8_plan_to_tick`, `stage_9_director_plan_to_tick`.
- Confirm the `ScriptedLlm` empty-queue panic no longer occurs in any run.
- Any failure here is a new link: root-cause it and extend this doc's chain
  catalog before proceeding (do not paper over with retries).
- **Success criteria:** 15/15 runs green; zero empty-queue panics; root
  `otto ci` green.

#### Phase 3: OCC regression tests (folds in OCC doc Phase 2)
**Model:** sonnet
- Store-seam test forcing the same-millisecond floor: create + immediately
  triage + review-transition on a synced copy succeeds; the pre-fix discard
  shape fails.
- Integration test driving `spawn_reviewer_for_bundle` end-to-end with a
  scripted verdict: bundle lands Reviewed/Rejected with exactly 1 Review row.
- **Success criteria:** both tests green and break-to-proven (discard shape
  restored temporarily -> red).

#### Phase 4: loud-fail Stale discrimination (folds in OCC doc Phase 3)
**Model:** opus
- F6 arm (`context.rs`, reviewer-result Stale) and `accept_bundle`'s
  Stale-swallow arm (`spawner.rs`): on Stale, re-read the bundle and branch
  exactly per the OCC doc's Phase 3 enumeration (re-read fails -> `error!`;
  advanced past the expected status -> silent winner `debug!`; not advanced ->
  `error!` invariant violation with bundle id, expected/actual timestamps,
  round from a Review-row list on the loud path only).
- Siblings behave identically: both arms get the same discrimination.
- **Success criteria:** unit test per branch, both arms; loud branches
  break-to-proven.

#### Phase 5: store-seam Stale log-level fix + works-race caller audit
**Model:** sonnet
- Decision (made here, not by the phase): an OCC `Stale` refusal is an
  expected, recoverable outcome at the store seam and logs at **WARN**; all
  other update errors (I/O, corruption) stay **ERROR**. Rationale: the caller
  owns the severity verdict (Phase 4's discrimination emits the ERROR when a
  Stale is an invariant violation), and benign-race ERROR noise trains
  operators to ignore ERROR entirely. Implementation: replace the blanket
  `#[instrument(err)]`-driven ERROR on `update` in `store::works` AND
  `store::bundles` (siblings behave identically) with hand-logged
  discrimination: `Stale -> warn!`, other variants -> `error!`.
- Identify the caller behind the observed works-Stale race (hypothesis:
  Director `override_work` racing the integrator's Work-Done write). Document
  the race and its fail-closed handling in a code comment at the losing call
  site.
- **Success criteria:** unit test asserting a Stale refusal emits WARN (not
  ERROR) and a non-Stale update failure emits ERROR, both stores; the racing
  caller's call site carries the race comment (cited `path:line` in the
  implementation notes).

#### Phase 6: live e2e verification (operator step, not a phase agent)
**Model:** n/a (operator)
- `bin/e2e rust-version` with a real API key (folds in OCC doc Phase 4).
- **Success criteria:**
  - Run reaches the integrator; ticks > 0.
  - events.log: zero reviewer-span stale errors, zero
    `reconcile: Triaged Bundle with no live Reviewer` WARNs, zero Phase 4
    invariant-violation ERRORs, zero `director: NeedsOperator grace exceeded`
    WARNs.
  - `.loopr/taskstore/bundles.jsonl` retains all pre-Tick rows after the run
    (Link 3 verified live).
  - Exactly one Review row per genuine review round.

## Acceptance Criteria

- [ ] All 3 `failure_paths` tests + `stage_8_plan_to_tick` +
      `stage_9_director_plan_to_tick` green, 3 consecutive isolated runs each
      (15/15).
- [ ] Pattern-tracker unit tests prove: idle `done` after a mutating action
      never trips NoProgress; a sustained mutating doom loop still reaches
      EscalationTripped. Both break-to-proven.
- [ ] A Stale bundle write whose record never advanced emits ERROR in both
      arms (unit-tested, break-to-proven); a genuine lost-race stays silent.
- [ ] Store-seam OCC Stale refusals log WARN, non-Stale update failures log
      ERROR, in both `store::works` and `store::bundles` (unit-tested).
- [ ] Live e2e (`bin/e2e rust-version`): ticks > 0 and
      `.loopr/taskstore/*.jsonl` history survives the Tick.
- [ ] `otto ci` green at repo root.

## Resolved Decisions

- 2026-07-12: Option 1 (targeted fixes first, design doc after full
  unmasking) chosen by Scott over doc-first and investigate-everything-first.
  Links 3 fix and the test-diagnostics improvement shipped as targeted
  commits (`2ba07f0f`, `ec4b3e2e`) before this doc; everything else lands
  through this doc.
- 2026-07-12: harness-fidelity policy: product-side robustness is the fix
  pattern. All four links were product bugs; the in-process harness (no
  taskstore commit, zero-interval Director) is a legitimate amplifier that
  surfaced them and stays as-is. Harness parity with `loopr init` is NOT a
  goal.
- 2026-07-12: `.loopr/taskstore` commit wiring stays out of scope (vision
  feature, separate design). The integrator treats `.loopr/` as loopr-owned
  state via pathspec exclusion in both the dirty guard and the clean.
- 2026-07-12: panel round 1 (Architect + Staff Engineer) findings
  dispositioned:
  - Finding 1 (both; MUST-FIX): the draft's claim that reconcile covers the
    Reviewed+`done` wedge was factually wrong (reconcile handles only
    Triaged / Accepted / InProgress). Resolved by design change: the
    actionable-state clause in the engaged gate makes idle-on-Reviewed
    escalate correctly instead of silently rotting. Folded in.
  - Finding 2 (staff; MUST-FIX): "doom-loop coverage preserved by
    construction" was false for interleaved mutating-no-op / `done` static-
    hash loops. Resolved: actionable-target interleaving now covered by the
    same clause; the non-actionable interleaved residual is named and
    accepted with the chaotic-rotation family's revisit condition; Phase 1
    gains the break-to-proven interleaved test. Folded in.
  - Finding 3 (both; MUST-FIX): Phase 5 punted the WARN-vs-ERROR call and had
    a non-falsifiable AC. Resolved: decision made in-doc (Stale -> WARN at
    the store seam, callers own ERROR), unit-tested AC, added to overall
    Acceptance Criteria, crates touched extended to `store`.
  - Finding 4 (staff; cheap win): production timing corrected from ~65s to
    ~3 minutes (idle iterations sleep `idle_interval_secs = 15`).
  - Finding 5 (architect): keep Phase 5 rather than split it out, per the
    staff position, now that it is tightened; it came from this
    investigation's trace and is small. Author's call as owner.
  - Finding 6 (architect): stage_8/9 flake mechanism is hypothesis; already
    gated by Phase 2's root-cause-any-failure rule. No change.
- 2026-07-12: panel round 2 (closure): findings 1, 2, 3, 5, 6 CLOSED by both
  reviewers against the code (Reviewed cleared only by accept_bundle;
  reconcile never touches Reviewed; engaged-gate streak math verified;
  store-seam sibling symmetry verified). Finding 4's residual (Alternative
  3's ~65s figure ambiguous with the corrected default-config number) plus
  two doc-consistency findings (Summary described the round-1 one-clause fix;
  Data Model said "no store changes" contradicting Phase 5) fixed in this
  revision. Design closed; doc is the accepted two-clause engaged gate
  throughout.

## Alternatives Considered

### Alternative 1: wall-clock thresholds for the pattern tracker
- **Description:** convert `escalation_threshold` / `needs_operator_grace_iters`
  from iteration counts to durations so cadence changes cannot alter
  escalation semantics.
- **Pros:** cadence-independent; the harness and production behave identically
  in wall time.
- **Cons:** still wrong: a legitimately long implementer run after a Director
  action would still escalate once the duration passes. Larger config surface,
  more invasive change, and it does not use the signal that actually
  distinguishes the cases (what the Director is doing NOW).
- **Why not chosen:** fixes the timescale, not the semantics. The defect is
  counting idle waiting as no-progress at all.

### Alternative 2: live-set awareness in the Director
- **Description:** thread the daemon's in-flight agent count (implementer /
  reviewer / integrator tasks for this Plan) into `observe()`; suppress
  no-progress while work is in flight.
- **Pros:** the daemon's ground-truth signal for "work is happening."
- **Cons:** cross-crate seam (loopr daemon live sets -> agents DirectorDeps);
  the Director deliberately consumes only record-state today. The panel's
  Reviewed-rot finding is covered more directly by the actionable-state
  clause, which is computable from records the tracker's caller already
  holds.
- **Why not chosen (parked):** the engaged gate (current action | actionable
  records) kills both the observed bug class and the Reviewed-rot pathology
  inside the crate that owns them. Revisit condition: a real trace showing a
  wedge the record-derived signal cannot see (e.g. a live agent whose crash
  leaves no record trace and that reconcile fails to reap).

### Alternative 3: fix the test (raise deadline / slow the harness Director)
- **Description:** give the harness a non-zero Director interval or a longer
  deadline so the death spiral loses the race.
- **Pros:** no product change.
- **Cons:** enshrines a production bug (a healthy Plan Stalls after ~3
  minutes of record-static state at default intervals; ~65s under a
  hypothetical uniform 5s cadence); the test is doing its job.
- **Why not chosen:** the test found a real defect. Matching the test to the
  defect is the Link 3 "fix the expectation" mistake again.

## Technical Considerations

### Dependencies

- Internal only: `agents` (gate fix), `loopr` (tests + Phase 4 arms),
  `store` (Phase 5 log-level discrimination), `integrator` (already shipped,
  referenced).
- Blast radius: agents + loopr + store. No cross-repo impact; no ship-order
  constraint beyond phase order.

### Performance

- Neutral. The gate change removes redundant trip bookkeeping on idle
  iterations; no new I/O.

### Security

- None. No new inputs, no auth surface.

### Testing Strategy

- Break-to-prove on every new test (pre-fix shape restored temporarily,
  demonstrated red, not kept in the suite).
- Stability gates are 3 consecutive isolated runs, not single passes: this
  chain's tests were committed red and stayed red for weeks because a single
  green was never required at landing. Root `otto ci` (workspace test run)
  is the structural guard going forward; it now bites because the suite is
  green.
- Live e2e as the final gate (Phase 6): the unit gap is exactly what let
  Links 1-3 ship.

### Rollout Plan

- Single branch (`v5`), one commit per phase, `otto ci` green per phase.
- Rollback: `git revert` per phase; no schema changes.
- On landing: mark OCC doc Phases 2-4 as folded into this doc (addendum line
  there), keeping both docs' status fields truthful.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| A fifth link hides behind Link 4 | Med | Med | Phase 2 is a hard gate: any failure is root-caused and cataloged before Phases 3-6 proceed |
| Actionable-idle clause over-escalates a Plan whose Director briefly idles between accept and the accept landing | Low | Low | escalation needs 8 consecutive static-hash trips; the accept's write moves the hash and resets the streak within 1-2 iterations |
| stage_8/9 flake has a second cause beyond Link 4 | Low | Med | Phase 2's 3-consecutive-run criterion; a failure there is a finding, not a pass |
| Phase 6 blocked on API key availability | Med | Low | operator step, explicitly not a phase agent |

## Open Questions

None.

## References

- `docs/design/2026-07-12-reviewer-occ-stale-race.md` (Links 1-2, OCC Phases
  2-4 specs folded in here, implementation-status addendum)
- Shipped chain commits: `5dfd3112` (Link 1), `ac42cfa3` (Link 2),
  `2ba07f0f` (Link 3), `ec4b3e2e` (deadline state-dump diagnostics)
- Link 4 evidence: WARN-trace of failing `mid_dag` run (Director
  `iteration=32`, `needs_operator_iters=5`, Plan -> Stalled ~300ms in; Work B
  wedged InReview with a Reviewed bundle)
- `crates/agents/src/director/pattern.rs` (`observe()` step 4 gate,
  `is_mutating`, `consecutive_same_action`)
- `crates/loopr/tests/common/harness.rs:156-157` (zero-interval Director)
- `docs/design/2026-05-09-director-phase-2.md` (pattern tracker + mode FSM
  origin)
