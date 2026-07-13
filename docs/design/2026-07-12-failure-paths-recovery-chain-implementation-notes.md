# Implementation Notes: failure_paths Recovery Chain (Link 4 + OCC Phases Fold-In)

Design doc: `docs/design/2026-07-12-failure-paths-recovery-chain.md`

## Phase 0: director module decomposition

### Design decisions
- Extracted `reconcile_director` (the reconcile sweep: Integrated->Done
  promotion + the three Phase 3 stuck-state recovery cases) out of
  `crates/agents/src/director.rs` into a new sibling submodule
  `crates/agents/src/director/reconcile.rs`, mirroring the existing
  `mode.rs` / `pattern.rs` submodule pattern. `reconcile_director` was the
  single largest self-contained function in the file (128 lines including
  its doc comment and `#[instrument]` attribute), operates only on the two
  traits already defined earlier in `director.rs` (`DirectorStore`,
  `WorkSpawner`) plus `DirectorError`, and has exactly one call site inside
  `director.rs` (`run_once`, step 1) plus external callers via the crate's
  public re-export (`agents::reconcile_director`, consumed by
  `crates/loopr/tests/director_stuck_states.rs`). This made it the one
  cohesive seam that both (a) got the file under the 1500-line bloat cap
  and (b) reads as a truthful, single-word module name (`reconcile`) rather
  than a grab-bag split.
- Extracted the matching `reconcile_director` test block (goal-complete
  detection + the three Phase 3 stuck-state-recovery test groups, ~253
  lines) out of `crates/agents/src/director/tests.rs` into a new
  `crates/agents/src/director/tests/reconcile.rs`, mirroring the existing
  `operator.rs` / `restart.rs` test-submodule pattern already used in that
  same file for the identical reason (both of those modules' own header
  comments say so explicitly). Same seam name on both sides (production
  `director/reconcile.rs`, tests `director/tests/reconcile.rs`) keeps the
  production/test module structure symmetric.
- Both extractions are `pub mod` / `pub use` re-exports at `director.rs`'s
  existing bottom-of-file module block (alongside `mode`/`pattern`), so
  `crate::director::reconcile_director` and the crate-root
  `agents::reconcile_director` re-export continue to resolve unchanged for
  every existing caller.

### Deviations
- None. Pure code movement + `mod`/`use` wiring; zero logic changes
  (verified via `git diff`: the extracted function/test bodies are
  byte-identical to their pre-move form, only the enclosing file and
  import lines changed).

### Tradeoffs
- Considered extracting the mode-FSM helpers instead, but `DirectorMode` /
  `next_mode` already live in their own `mode.rs` module (pre-existing);
  there was no mode-FSM code left in `director.rs` to extract.
- Considered a smaller extraction (just enough lines to clear 1500) rather
  than the whole `reconcile_director` function, but a partial function
  split would produce exactly the "helpers.rs junk-drawer" shape this
  phase was told to avoid. Extracting the whole function is both simpler
  and more cohesive.

### Open questions
- None.

## Phase 1: NoProgress engaged-gate

### Design decisions
- `DirectorPatternTracker::observe()` gains an `actionable: bool` third
  parameter (`crates/agents/src/director/pattern.rs`). Step 4's gate
  becomes `let engaged = action.is_mutating() || actionable; let trip =
  engaged && (distinct <= 2 || max_rec >= half_plus_one);`, replacing the
  window-wide `self.action_history.iter().any(ActionFingerprint::is_mutating)`
  check. This is a current-iteration signal (the action just observed),
  not a window-any signal, which is the exact defect the design doc
  root-caused: a stale mutating fingerprint elsewhere in the 16-deep
  window could no longer defeat idle-`done` detection.
- The caller (`run_director_inner` step 6, `crates/agents/src/director.rs`)
  computes `actionable` from the same `works_after` / `bundles_after`
  snapshot it already feeds `compute_state_hash`: `bundles_after.iter().any(|b|
  b.status == BundleStatus::Reviewed) || works_after.iter().any(|w| w.status
  == WorkStatus::Blocked)`. No new store read; the snapshot already exists
  at that point in the loop.
- Corrected `is_mutating()`'s doc comment and the `observe()` step-4
  rustdoc to describe the actual two-clause guarantee (current action
  mutating OR actionable state present) instead of the old, overclaiming
  text ("a Director that only emits `done` while waiting... does not
  trip" — true only when nothing is actionable; the corrected comment
  says so explicitly).
- `PatternConfig` untouched; no new config knobs, per the design doc's
  explicit constraint.
- Five new unit tests in `crates/agents/src/director/pattern/tests.rs`,
  one per the design doc's Phase 1 bullet list: the Link 4 shape (stale
  mutating fingerprint + idle `done` + no actionable state -> never
  trips, streak stays 0), idle-`done`-with-actionable-state (trips,
  sustained -> `EscalationTripped`), current-mutating-action-without-
  actionable (regression: still trips and escalates), interleaved
  mutating-no-op/`done` against an actionable target (streak accumulates
  monotonically across both iteration kinds — asserted via
  `streaks.windows(2)` all differing by exactly 1), and a repeated
  identical mutating action (SameAction path, step 2, untouched by this
  change).

### Deviations
- None. Matches the design doc's `Architecture` section snippet exactly;
  the only difference from the doc's illustrative code is the variable
  names already present in `run_director_inner` (`works_after`/
  `bundles_after`), which the doc itself flagged as illustrative ("Find
  the actual variable names/paths in the code").

### Tradeoffs
- All 21 pre-existing `observe()` call sites in `pattern/tests.rs` pass
  `false` for the new `actionable` parameter. Verified analytically and
  by running the full suite that none of those pre-existing tests
  exercise a scenario where `actionable` would change the outcome (they
  either use a mutating current action, where `engaged` is `true`
  regardless of `actionable`, or exercise steps 1-3/5 which run before
  step 4 ever reads `actionable`). Passing `true` everywhere instead
  would have been equally arbitrary; `false` is the more conservative
  choice (closer to "no actionable state" as a default) and keeps every
  existing test's intent unchanged.
- The `current_mutating_action_static_hash_trips_and_escalates_without_actionable`
  and `repeated_identical_mutating_action_same_action_path_unchanged`
  tests are regression-safety tests, not bug-differentiators: under the
  break-to-proven pre-fix gate they pass unchanged (a mutating *current*
  action is, by construction, also "any mutating action in the window"),
  so restoring the old gate does not turn them red. This is expected and
  reported honestly rather than contrived into a false differential —
  see break-to-proven results below.

### Break-to-proven results
Temporarily replaced the fix with the exact pre-fix gate (`let engaged =
self.action_history.iter().any(ActionFingerprint::is_mutating);`,
ignoring the `actionable` parameter), ran the full `director::pattern`
suite, then restored the fix (`git diff` confirmed a clean, artifact-free
restore):
- `link4_stale_mutating_fingerprint_idle_done_no_actionable_never_trips`
  — RED under the old gate: escalates all the way to
  `EscalationTripped{streak:12}` on pure idle waiting, reproducing the
  Link 4 mechanism exactly (a stale mutating fingerprint elsewhere in the
  window trips idle `done`).
- `idle_done_with_actionable_state_trips_and_escalates` — RED under the
  old gate: never trips (no mutating action ever appears in this
  scenario), reproducing the Reviewed-rot pathology the panel's Finding 1
  flagged.
- `current_mutating_action_static_hash_trips_and_escalates_without_actionable`,
  `interleaved_mutating_noop_and_done_with_actionable_target_accumulates_streak`,
  `repeated_identical_mutating_action_same_action_path_unchanged` — GREEN
  under both gates (regression-safety, not differentiators; see
  Tradeoffs above for why).
- All 21 pre-existing tests — GREEN under both gates (unaffected by the
  gate change, as expected).

### Open questions
- None.
