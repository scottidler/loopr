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

## Phase 2: recovery-suite stability gate

### Design decisions
- Added the Link 3 audit-trail pin to `reject_then_recover_reaches_tick`
  (`crates/loopr/tests/failure_paths.rs`): after the existing Tick
  assertion, a new `load_reviews(&target, work_id)` helper reads
  `<target>/.loopr/taskstore/reviews.jsonl` (mirroring the existing
  `load_bundles`/`load_ticks`/`read_jsonl` idiom in the same file — latest
  row per id via a `HashMap`, filtered here to the Reviews whose
  `bundle_id` belongs to one of the Work's own Bundles), and asserts
  exactly 2 persisted `Review` rows survive the Tick: one `Verdict::Reject`
  (round 1) and one `Verdict::Accept` (round 2). This pins the exact claim
  Link 3's fix (`2ba07f0f`) makes — that the scoped `git clean -fd
  ':(exclude).loopr/'` on the integrate success path no longer erases the
  untracked `.loopr/taskstore/*.jsonl` truth files — with a test, not just
  a design-doc narrative.
- Added `domain::Review` to the test file's `domain` import list and a
  `HasId` impl for `Review` (`self.id.as_ref().to_string()`), matching the
  existing `Work`/`Bundle`/`Tick` impls exactly.
- Ran the design doc's Phase 2 stability matrix: 5 tests
  (`reject_then_recover_reaches_tick`,
  `multi_work_dag_unblocks_and_completes`,
  `mid_dag_failure_recovers_then_unblocks_downstream`, `stage_8`'s
  `plan_to_tick_happy_path`, `stage_9`'s
  `director_plan_to_tick_happy_path`) x 3 consecutive isolated runs each
  (one test-binary invocation per run, `-- --exact --test-threads=1`, no
  parallel siblings) = 15/15 green. Full matrix + `grep -l "complete_free
  called with empty queue"` + `grep -l "panicked at"` across all 15 logs:
  zero matches for both. Logs retained at
  `/tmp/claude-1000/-home-saidler-repos-scottidler-loopr-v5/bf694fab-e82e-4c4c-9380-8e909c050b1f/scratchpad/phase2runs/`
  for this session; not committed (scratchpad, not repo-tracked).
- Root `otto ci` green (confirmed twice consecutively via `otto ci;
  echo $?` -> `0`), including the full `failure_paths.rs`,
  `stage_8_plan_to_tick.rs`, and `stage_9_director_plan_to_tick.rs` runs
  inside the workspace `cargo test --workspace` pass.

### Deviations
- None from the design doc's Phase 2 bullet list.

### Tradeoffs
- `load_reviews` re-derives the Work's Bundle ids via the existing
  `load_bundles` helper rather than adding a `work_id` field lookup
  through `Bundle -> Review`; `Review` only carries `bundle_id`, not
  `work_id`, so joining through Bundle is the correct (and only) path
  from a Work to its Reviews, not an arbitrary shortcut.
- Chose 3x isolated single-test invocations (`--test-threads=1`,
  `--exact`) over one `cargo test` invocation covering all 3 in one
  binary run, per the design doc's explicit instruction ("one test-binary
  invocation per run... no parallel siblings"): this isolates each run's
  daemon/tempdir lifecycle completely and rules out cross-test
  contention as a confound if a run had failed.

### Open questions
- One `otto ci` run (of ~5 total across this phase's work: the first
  post-fmt-fix run plus reruns while investigating) surfaced a single
  failure in `crates/llm`:
  `metered::pricing_tests::cost_sink_append_unknown_model_warns_once_and_still_ledgers_usage`
  (`crates/llm/src/metered.rs:517`), asserting exactly 1 WARN log line
  for a repeated unknown-model warning and getting 0. This test is
  wholly unrelated to this design doc's scope (no `failure_paths`/
  Director/recovery-chain code touches `crates/llm`, and Phase 2's diff
  touches only `crates/loopr/tests/failure_paths.rs`). Root-caused as far
  as reproducibility allows: 30 isolated `cargo test -p llm --lib` runs
  and 4 additional full `cargo test --workspace` / `otto ci` runs all
  passed clean; the failure did not reproduce again. The test's
  `tracing::warn!` call for the unknown-model path
  (`crates/llm/src/metered.rs:86`) executes synchronously before the
  `spawn_blocking` hop, on the same thread that installed the
  `tracing::subscriber::set_default` thread-local subscriber (the test is
  a bare `#[tokio::test]`, i.e. current-thread flavor, so no
  cross-thread hop for that call), which rules out the obvious
  "warn happened on the wrong thread" explanation. The remaining
  plausible mechanism is `tracing-core`'s process-global per-callsite
  interest cache racing against concurrent `tracing::subscriber::
  set_default` thread-local installs from OTHER tests running
  concurrently in the same `llm` test binary (a documented class of
  tracing-testing flake, not specific to this callsite) — this is a
  hypothesis, not confirmed, since it did not reproduce under
  deliberate retry. Flagged here rather than fabricated into a "Link 5"
  of this doc's recovery chain: it shares no mechanism with the
  Director/OCC/integrator chain this doc tracks (wrong crate, wrong
  subsystem, a testing-infrastructure race rather than a product bug).
  Recommend a separate follow-up (own design doc or a targeted fix) if
  it recurs; not blocking this phase since 5 independent `otto ci` /
  `cargo test --workspace` runs after the one failure were all green.

## Phase 3: OCC regression tests

### Design decisions
- **Store-seam test** —
  `review_transition_succeeds_on_synced_copy_across_same_millisecond_floor`
  in `crates/store/src/bundles/tests.rs` (alongside the existing
  `update_round_trip_ok` / `update_stale_version_rejected` OCC tests it's
  modeled on). Forces the on-disk `updated_at` into the future
  (`domain::now_millis() + 1_000_000`) before creating the Bundle — the
  same deterministic-floor idiom `update_floors_updated_at_strictly_above_
  prior` (`crates/store/tests/plans.rs`) already uses — so the triage
  write's floor (`max(now_millis(), current.updated_at + 1)`) lands at
  exactly `current.updated_at + 1` regardless of wall-clock scheduling.
  This reproduces "create and triage land in the same millisecond"
  without depending on test-runner timing luck. Asserts: (a) the floor
  value is exactly `future + 1`; (b) the local transition's own
  `now_millis()`-stamped `updated_at` (`unsynced_local_ts`) is strictly
  less than the floored value (proving a same-ms gap genuinely exists to
  reproduce); (c) a subsequent `Triaged -> Reviewed` write, snapshotting
  its `expected_updated_at` from a copy SYNCED to the returned floored
  value, succeeds.
- **Integration test** — new file
  `crates/loopr/tests/reviewer_occ_regression.rs`, two tests
  (`spawn_reviewer_for_bundle_lands_reviewed_with_one_review_row`,
  `..._rejected_with_one_review_row`) mirroring `accept_gate.rs`'s
  `build_test_context`/`teardown`/seed-then-assert idiom. Each seeds a
  Plan + Work (`InReview` fixture status, FSM bypassed as a fixture per
  the `director_reconcile.rs` precedent) + a `Proposed` Bundle whose
  `updated_at`/`created_at` are ALSO forced into the future (same
  determinism technique as the store-seam test, needed because a real
  `DaemonContext`'s startup — git init, `build_context` — burns enough
  wall-clock time between Bundle construction and the triage write that
  the natural gap is not reliably sub-millisecond). Calls
  `Arc::clone(&ctx).spawn_reviewer_for_bundle(bundle).await` directly (not
  spawned — awaited in-test so the assertions run against settled state,
  no polling needed) with a `ScriptedLlm` queued with one free-form
  verdict (`queue_free`, untargeted — matches any model, mirroring
  `failure_paths.rs`'s `REVIEWER_ACCEPT`/`REVIEWER_REJECT` idiom). Asserts
  the Bundle status (`Reviewed`/`Rejected`) and exactly one
  `store.reviews().list_by_bundle(...)` row.

### Deviations
- None from the design doc's Phase 3 bullet list. One implementation
  detail not spelled out in either source doc: the integration test
  drives `spawn_reviewer_for_bundle` directly against a hand-seeded
  Bundle/Work rather than through the full decompose->implement->review
  daemon loop (`failure_paths.rs`'s heavier idiom) — same effect at the
  correct seam, and the doc's own wording ("driving
  `spawn_reviewer_for_bundle` end-to-end") names the function directly
  rather than the full pipeline.

### Tradeoffs
- The integration test forces the same-millisecond floor via a future
  `updated_at` (deterministic) rather than relying on the real timing gap
  between Bundle creation and the daemon's own triage write. Confirmed
  empirically: with the fix reverted (see Break-to-proven below), the
  *undoctored* real-timing version of this test did NOT reliably fail —
  `DaemonContext` construction (git init, worktree reconcile) consumes
  enough wall-clock time that the natural create-to-triage gap exceeds a
  millisecond, so the floor's +1 bump is rarely exercised without forcing
  it. The store-seam test's forced-future technique was ported into the
  integration test for exactly this reason; without it, the integration
  test would not have been a faithful regression guard for Link 1's
  specific mechanism (it would still catch OTHER Bundle-review
  regressions, just not reliably this one).
- `queue_free` (untargeted) over `queue_free_for("claude-sonnet-4-6", ...)`
  (the reviewer's default configured model): keeps the test independent
  of `ReviewerConfig::model`'s literal default, matching `failure_paths.rs`'s
  existing plain-`queue_free` reviewer verdicts rather than inventing a
  new keyed idiom for this one file.

### Break-to-proven results
- **Store-seam test**: temporarily changed
  `expected_updated_at = synced.updated_at` to
  `expected_updated_at = unsynced_local_ts` (the pre-fix discard shape —
  keep the caller's pre-write snapshot instead of the store's returned
  floored value). RED: `Stale { expected: <local>, actual: <floored> }`.
  Restored the fix; confirmed green again (`git diff` on the test file
  after restore showed no residual change).
- **Integration test**: temporarily changed
  `crates/loopr/src/daemon/context/transition.rs`'s
  `transition_and_persist_bundle` to discard the store's returned value
  (`let _ = persisted;` in place of `bundle.updated_at = persisted;` —
  the exact Link 1 discard shape). RED, both tests: Bundle left `Triaged`
  (assertion `left: Triaged, right: Reviewed`/`Rejected`), reproducing the
  pre-fix doom-loop symptom exactly — the reviewer's own final write lost
  to a self-inflicted Stale and the Bundle never advanced past Triaged.
  Restored the fix; `git diff` on `transition.rs` confirmed a clean,
  artifact-free restore (empty diff) before the commit.

### Open questions
- None.

## Phase 4: loud-fail Stale discrimination

### Design decisions
- **Extracted a shared helper `discriminate_stale_bundle_write`** into
  `crates/loopr/src/daemon/context/transition.rs` (re-exported from
  `context`), consumed byte-identically by BOTH arms. The two arms now
  differ ONLY in the single `expected: BundleStatus` argument they pass
  (`Triaged` for the reviewer-result / F6 arm, `Reviewed` for
  `accept_bundle`); everything else — the re-read, the three-way branch,
  every log line and its fields — is one code path. This is the
  "siblings behave identically" instinct expressed structurally: two
  hand-copied blocks can drift, one shared helper called with a different
  status cannot. `transition.rs` is where the sibling `BundleTransitionError`
  / `BundleUpdateError::Stale` types already live and where
  `transition_and_persist_bundle` (the write that produces the Stale)
  lives, so it is the truthful home for the post-Stale discriminator.
- **Three-way branch, matching the OCC doc's Phase 3 enumeration exactly:**
  re-read fails -> `error!` carrying both Stale timestamps AND the re-read
  error (cannot distinguish lost-race from violation without the current
  status); current status != expected -> a legitimate lost race, `debug!`
  names the winning status; current status == expected -> OCC invariant
  violation with no winner, `error!` carrying bundle id, expected/actual
  Stale timestamps, and the latest Review round.
- **Round fetched ONLY on the loud invariant-violation path.** A private
  `latest_review_round` helper lists the bundle's Review rows and returns
  the max `round` (or 0 if none / list error); it is called only inside the
  `current == expected` arm, so the benign `debug!` winner branch never pays
  for the extra store read (the OCC doc's "extra read confined to the loud
  path" requirement).
- **Logging rule (scope-identifying keys on the loud line):** the
  invariant-violation `error!` carries `bundle_id`, `expected_status`,
  `stale_expected`, `stale_actual`, and `round` so an operator diagnoses the
  break from the single log line without a rerun. The benign `debug!` names
  the `winner_status`.
- **F6 arm** (`crates/loopr/src/daemon/context.rs`, inside
  `spawn_reviewer_for_bundle`): the `ReviewerError::Update(BundleUpdateError
  ::Stale { .. })` arm now binds `{ expected, actual }` and calls
  `discriminate_stale_bundle_write(&self.store, &bundle.id,
  BundleStatus::Triaged, expected, actual)` instead of the bare
  `debug!("...another reviewer won...")` swallow.
- **accept_bundle arm** (`crates/loopr/src/daemon/context/spawner.rs`): the
  `BundleTransitionError::Stale { .. }` arm now binds `{ expected, actual }`
  and calls the same helper with `BundleStatus::Reviewed`, replacing the
  bare `debug!("...another writer beat us...")` swallow.
- Six unit tests in `crates/loopr/src/daemon/context/transition/tests.rs`
  (new 2018-style submodule beside the helper), a per-branch test for BOTH
  arms: `f6_still_triaged_is_loud_invariant_violation`,
  `f6_advanced_to_reviewed_is_silent_winner`, `f6_reread_failure_is_loud`,
  `accept_still_reviewed_is_loud_invariant_violation`,
  `accept_advanced_to_accepted_is_silent_winner`,
  `accept_reread_failure_is_loud`. Log assertions capture the JSON
  `tracing` output via a `VecWriter` MakeWriter (the exact pattern from
  `crates/llm/src/metered.rs`), so the loud branches are verified by the
  precise ERROR line + fields they emit, and the silent branches by the
  absence of any ERROR plus the single winner `debug!`.

### Deviations
- **The OCC doc labels this the "F6 arm + accept_bundle arm" and describes
  two arms hardened separately; implemented as ONE shared helper both arms
  call.** Same effect at the correct seam — the design doc's own Phase 4
  bullet explicitly invited this ("If there's a shared helper opportunity
  that keeps them genuinely identical... prefer extracting it"). This makes
  the "siblings behave identically" requirement structural rather than a
  reviewer-verified copy match.
- The OCC doc's Phase 3 prose says the reviewer error "carries only the
  timestamps... round is obtained by listing the bundle's Review rows at
  error time." Implemented exactly: the helper takes the timestamps from the
  matched `Stale { expected, actual }` and reads the round itself on the
  loud path only. No signature change to `ReviewerError` / `BundleUpdateError`
  was needed.

### Tradeoffs
- **Helper does the logging itself vs. returning an outcome enum for the
  caller to log.** Chose helper-owns-the-logging so the two arms are
  guaranteed byte-identical in what they emit; an outcome enum would have
  reintroduced two hand-written `match`/log blocks — exactly the drift the
  extraction removes. The cost is that the log message strings live in the
  helper rather than the call site, which is acceptable because the message
  is about the Stale discrimination, not about the specific caller (the
  `expected_status` field already disambiguates which arm fired).
- **Testing the shared helper directly with each `expected` value, vs.
  driving each arm end-to-end through a full daemon loop.** The two arms are
  literally one call each into the helper differing only by the status
  argument, so exercising the helper with `Triaged` and with `Reviewed` IS
  the per-arm coverage — a faithful test at the correct seam, far cheaper
  and more deterministic than standing up a `DaemonContext` and racing a
  real Stale. The seam is the helper; that is where the branch logic lives
  and where it is tested.
- **`round` = max of persisted Review rounds, 0 on empty/error.** `Review`
  rows are append-only with 1-based `round`; the max is the current round.
  0 is an honest "no rounds / couldn't read" sentinel on the loud path (the
  round is diagnostic context, not a control signal), and a list-read error
  there emits its own `warn!` rather than masking silently.

### Break-to-proven results
Temporarily replaced the helper body with the exact pre-fix silent-swallow
shape (`debug!(bundle_id, "OCC Stale; another writer beat us"); return;` —
no re-read, no discrimination), ran the full transition-tests suite, then
restored the fix (`grep` confirmed zero `BREAK-TO-PROVEN` markers remained;
`otto ci` green after restore):
- `f6_still_triaged_is_loud_invariant_violation` — RED under the pre-fix
  shape: `count_lines(ERROR, "OCC invariant violation, no winner")` was 0,
  not 1 (the loud line never fired; the Stale was swallowed silently). This
  is the reviewer-result loud branch proven to bite.
- `accept_still_reviewed_is_loud_invariant_violation` — RED under the
  pre-fix shape, identically: 0 ERROR lines, not 1. The accept-arm loud
  branch proven to bite.
- The other four tests (both silent-winner, both re-read-failure) also went
  RED under the pre-fix shape (winner tests expected the new `debug!`
  message + `winner_status` field, absent in the swallow; re-read tests
  expected the loud ERROR the swallow never emits), confirming every branch
  assertion is load-bearing, not just the two invariant-violation ones.

### Sibling symmetry
Both arms are byte-equivalent in behavior: each is a single
`discriminate_stale_bundle_write(&<store>, &<bundle id>, <expected status>,
expected, actual).await` call, and `<expected status>` is the ONLY
difference (`Triaged` vs `Reviewed`). Identical re-read, identical three-way
branch, identical log messages and fields. There is no second copy of the
discrimination logic to drift.

### Open questions
- None. (Phase 5 owns the store-seam Stale WARN-vs-ERROR log-level change in
  `store::works` / `store::bundles`; this phase deliberately does not touch
  the store crate — the caller-owns-severity split is exactly why the loud
  `error!` lives here in the caller, not at the store seam.)
