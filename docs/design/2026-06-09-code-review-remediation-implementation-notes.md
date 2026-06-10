# Implementation Notes: Code Review Remediation

Running, append-only record of how the implementation interprets or
diverges from `2026-06-09-code-review-remediation.md`. One section per
phase; four buckets each ("None." where empty).

## Pre-phase: CI unblock

### Design decisions
- Pulled the telemetry `unnecessary_sort_by` clippy fix
  (`crates/telemetry/src/query.rs:57`) forward from Phase 9 into its own
  commit — the Testing Strategy says CI (`-D warnings`) must be green for
  `otto ci` to gate each phase, and telemetry's scoped CI was red. Fixed
  the descending sort as `sort_by_key(|c| std::cmp::Reverse(c.0))`.

### Deviations
- None.

### Tradeoffs
- None.

### Open questions
- None.

## Phase 1: Agent-pipeline correctness

Split into two commits for size/durability (the phase touches 7 crates
and threads a new `CommitContext` through ~20 call sites):
- **Part 1** — the three independent panics/drops (llm metered usage,
  tools truncate, telemetry SessionId::parse).
- **Part 2** — the git/commit/bundle pipeline cluster (findings 1-6).

### Design decisions
- **Finding 9 (`llm` metered usage on error path).** Added
  `LlmError::billed_usage(&self) -> Option<&Usage>` rather than having
  the metered client re-classify error variants. `Usage` now rides on
  `FatalReason::ContextExhausted { .., usage }` and
  `FatalReason::SchemaValidation { message, usage }` (the latter
  converted from a tuple variant to a struct variant). `MeteredLlmClient`
  records `billed_usage()` on its error path so max_tokens-truncated and
  tool-refusal 200s reach the cost counters.
- **`LlmClient::model()` accessor.** Added to support the `Loopr-Model`
  commit trailer (Phase 1 finding 6, Part 2). Given a documented default
  body (`"unknown-model"`) so the ~6 workspace test fakes that model no
  particular ID don't need touching — keeping the blast radius inside
  `llm`. Every production backend (`AnthropicClient`, `MeteredLlmClient`,
  the `Arc<L>` forward) overrides it; the default is never reached on a
  real call path.
- **Finding 7 (`tools` truncate panic).** Floor the cut index to the
  nearest UTF-8 char boundary at or below `MAX_INLINE_OUTPUT` before
  `String::truncate`.
- **Finding 8 (`telemetry` SessionId::parse).** Replaced the `&s[..15]`
  byte-slice with `s.get(..15)`/`s.get(15..)`; a non-boundary split now
  falls through to `(s, None)` and fails `is_valid_base` → `Malformed`,
  never a panic.

### Deviations
- The metered-client Finding 9 regression tests were added to the
  existing inline `#[cfg(test)] mod tests` block in `metered.rs` rather
  than a sibling file. The design doc schedules extracting all inline
  test blocks (incl. llm metered/usage/stub) in Phase 9; adding a sibling
  now would be a half-migration. Extraction stays a Phase 9 task.

### Tradeoffs
- `model()` default body vs. explicit impls on all fakes: chose the
  default to keep the change inside `llm` (the "crate is the unit of
  blast radius" rule). The cost is a sentinel default value, mitigated by
  the fact that production never reaches it and it is documented.
- Kept the redundant `used`/`limit` fields on `ContextExhausted`
  alongside the new `usage` (rather than deriving them from
  `usage.output_tokens`) to avoid churning the existing error Display and
  the decomposer's match expectations.

### Open questions
- None for Part 1.

### Part 2 — git/commit/bundle pipeline (findings 1-6)

#### Design decisions
- **`CommitContext` (dispatch.rs).** New type carrying `run_id`,
  `plan_id`, `work_id`, `role`, `model`, `gpg_sign`, threaded into every
  agent commit helper (`commit_changes`, `propose_bundle`,
  `commit_partial_for_inspection`, `force_propose`). Built once per
  `run_implementer` invocation. `commit_args()` emits the sign posture
  (`--no-gpg-sign` when `gpg_sign == false`, the default) plus one
  `--trailer Key=Val` per populated field. git normalizes the trailers
  to `Key: value` on disk (tests assert that form).
- **`Loopr-Run` source.** Added `run_id: Option<String>` to agents
  `Deps`; the daemon sets it from `self.process_id` at the implementer
  spawn site. `Loopr-Model` comes from `deps.llm.model()` (the trait
  accessor added in Part 1). `Loopr-Plan`/`Loopr-Work` from
  `work.parent_id`/`work.id`; `Loopr-Role` is the literal `implementer`.
- **`gpg_sign` config.** New `ImplementerConfig.gpg_sign` (kebab
  `gpg-sign`), default `false` — preserves the historical
  `--no-gpg-sign` behavior. Only the implementer commits, so the knob
  lives there (not Reviewer/Director).
- **Finding 1 (zero-commit propose).** `propose_bundle` returns
  `ActionResult::Error` when `HEAD == worktree.sha()` (no new commits).
  Skipped when the base sha is empty (fabricated test worktrees).
- **Finding 4 (`Bundle.base_commit`).** New additive
  `#[serde(default)] base_commit: Option<String>`. Populated by
  `propose_bundle` and `force_propose`; the reviewer diffs
  `base_commit..head_commit` via a new `git_diff_range` helper, falling
  back to `git show <head>` when `base_commit` is `None` (noop bundles
  and pre-existing rows). `git diff` output begins with `diff --git`, so
  the existing `strip_commit_header` handles both shapes.
- **Finding 2 (`commit_partial_for_inspection`).** Rerouted from
  `git add -u` + plain commit to the scoped
  `git_status_porcelain -> partition_by_scope -> git commit --only`
  pipeline. Removed the now-dead `is_staging_empty` helper.
- **Finding 5 (noop evidence).** The `Done` arm now sets
  `bundle.paths = work.files` and `bundle.claims = [message]`;
  `build_evidence_section` renders a `### Noop Justification` block from
  `noop_reason` ahead of the file contents.

#### Deviations
- The force-propose guard previously counted `git ls-files --modified`
  (tracked modifications); it now counts the in-scope set that actually
  gets committed (porcelain + `partition_by_scope`). This is a slightly
  different denominator but is the correct thing to bound, and the doc's
  finding 2 mandates the porcelain pipeline. `list_modified_tracked` was
  deleted (its only caller was rewritten).

#### Tradeoffs
- Made `git_status_porcelain` / `git_diff_name_only` `pub(crate)` so
  `force_propose` reuses them instead of duplicating the git calls
  (Phase 9 consolidates the remaining implementer/dispatch git-helper
  duplication; this avoids adding to it).
- Phase 1 split into two commits (Part 1 already shipped). The trailers
  + Bundle-field change touched 7 crates and ~20 call sites; committing
  the independent fixes first kept durable progress against context
  limits.

#### Open questions
- The `Loopr-Model` trailer records the *configured* model
  (`deps.llm.model()`), not the per-response model the provider echoes.
  Phase 6's model-pinning detector adds the response-reported model on
  the Bundle; the trailer can be reconciled to that then if desired.

## Phase 2: Integrator git-state integrity

Complete (all 12 findings). This phase is the system's riskiest code
(git crash recovery); findings landed incrementally across five commits
so each is durable.

Landing across commits (mirrors Phase 1's Part split):
- **Commit 1 (shipped)** — `run_git` kill_on_drop.
- **Commit A** — entry-sequence guards (findings: TOCTOU ordering,
  unconditional dirty-tree guard, crash-recovery merge-abort).
- **Commit B** — merge-loop correctness (conflict-vs-error
  classification, merge infra errors through fail_all, is_ancestor
  false-adopt).
- **Commit C** — rollback + validation correctness (AdoptedExisting
  rollback skip, clean_fd both paths, validation instrumentation).
- **Commit D** — determinism, branch deletion, multi-bundle re-entry.

### Commit 1 — run_git kill_on_drop

#### Design decisions
- **`run_git` kill_on_drop (git.rs).** Set `cmd.kill_on_drop(true)` so a
  timed-out `git merge`/`checkout` is killed when the timeout drops the
  future, rather than continuing to run and landing its mutation after
  `integrate` returned `Err`. Matches the posture validation.rs already
  uses.

#### Deviations / Tradeoffs / Open questions
- None.

### Commit A — entry-sequence guards (TOCTOU, dirty-guard, merge-abort)

#### Design decisions
- **TOCTOU ordering (lib.rs).** Moved integration-branch resolution,
  `verify_branch`, the new crash-recovery merge-abort, and the
  dirty-tree guard ALL under `git_lock`. Previously branch
  resolution/verify/dirty ran before the lock, racing a concurrent
  `integrate` mutating the same working tree.
- **Crash-recovery merge-abort (`git::merge_in_progress` + lib.rs).**
  New `merge_in_progress` helper (`git rev-parse --verify --quiet
  MERGE_HEAD`). At Phase-2 entry, before the dirty-tree guard and before
  checkout, a detected in-progress merge (conflicted index + MERGE_HEAD
  left by a prior crash) is aborted best-effort. This heals the "you
  need to resolve your current index first" checkout wedge. Ordered
  before the dirty-tree guard because a conflicted index reads as dirty.
- **Unconditional dirty-tree guard (lib.rs).** `working_tree_dirty` now
  runs in BOTH modes. The per-Plan-branch path was previously unguarded
  on the false premise that `git checkout loopr/plan-<id>` protects it;
  it does not (non-conflicting dirty state is carried silently across
  the checkout and later misclassified as a terminal conflict). Updated
  the `DirtyWorkingTree` Display text to drop the "(no-branch override)"
  qualifier.

#### Deviations
- None.

#### Tradeoffs
- **HEAD-parking decision.** Finding 5 asked to "decide and document HEAD
  restoration." Decision: per-Plan-branch mode intentionally leaves HEAD
  parked on `loopr/plan-<id>` after `integrate` returns — that branch is
  the daemon's integration workspace (subsequent integrates and the
  operator's eventual merge-to-main happen there); restoring HEAD to the
  prior branch would force a redundant re-checkout every integrate. The
  unconditional dirty guard makes the parked HEAD safe (the tree is
  always clean at entry). Documented inline in `integrate`.

#### Open questions
- None.

### Commit B — merge-loop correctness (conflict-vs-error, infra->fail_all, false-adopt)

#### Design decisions
- **Conflict-vs-infra classification (`classify::is_merge_conflict`).**
  A non-zero `git merge` exit is now split: output containing `CONFLICT`
  or `Automatic merge failed` is a genuine conflict (terminal - the same
  content cannot merge on retry - kept on the `fail_all_without_reset`
  path with the existing structural/retryable sub-classification). Any
  other non-zero exit (deleted/missing branch, would-overwrite, ENOSPC,
  index-lock contention) is a retryable infrastructure error: the tree
  is reset (`merge_abort` + `reset_hard`) but the Bundles are LEFT
  `Integrating` and a bare `IntegrationError::Git` is returned, so the
  driver's retry contract re-enqueues them. `merge_no_ff` now returns
  the COMBINED stdout+stderr (git prints conflict markers to stdout) so
  the classifier sees them.
- **Merge subprocess failure routes through `fail_all` (lib.rs).** The
  `merge_no_ff` call changed from `.await?` to a three-arm match;
  `Err(infra)` (spawn error / timeout) now routes through `fail_all`
  (reset git + transition Bundles to `IntegrationFailed`) instead of the
  bare `?`, closing the git-advanced/DB-silent gap that `kill_on_drop`
  alone did not.
- **`find_adopting_merge` replaces `merge_commit_sha_for`
  (git.rs + lib.rs).** The crash-recovery adopt path now confirms the
  adopted merge commit's SECOND parent (`<merge>^2`) resolves to exactly
  the Bundle's `head_commit`. A trivially-ancestral `head_commit` (the
  integration base, or one absorbed by a DIFFERENT bundle's merge) no
  longer false-adopts that other bundle's merge SHA; `find_adopting_merge`
  returns `None` and the loop falls through to the normal merge path
  (where `EmptyBranch` correctly reports a genuinely empty branch).
  `merge_commit_sha_for` was deleted (its only caller was rewritten).

#### Deviations
- The terminal conflict path keeps the name `ConflictRetryable` for a
  no-peer-overlap textual conflict even though that Bundle goes
  `IntegrationFailed` (terminal). "Retryable" describes the driver's
  Work-level re-attempt, not Bundle-level re-integration. Pre-existing
  naming; not renamed here (out of Phase 2 scope).

#### Tradeoffs
- Infra-class merge failures: subprocess-incomplete (timeout/spawn) is
  terminal (`fail_all`, per finding 1's explicit instruction) while a
  non-zero-but-completed non-conflict exit is retryable (finding 6).
  Justification: a completed merge that exited non-zero leaves the tree
  in a known state (clean after reset, safe to retry); an incomplete
  subprocess leaves uncertain tree state, so the safe consistent choice
  is reset + record-terminal. Both keep git and DB consistent.

#### Open questions
- The non-conflict completed-merge retryable path is covered by the pure
  `is_merge_conflict` unit test (CONFLICT-marker vs infra-message). An
  end-to-end seam test is not included: the reliable git triggers
  (would-overwrite, ENOSPC) are blocked by the unconditional dirty-tree
  guard or are not reproducible in CI. The classification is the
  load-bearing logic and is unit-tested directly.

### Commit C — rollback + validation correctness

#### Design decisions
- **AdoptedExisting validation-failure (lib.rs + new error variant).**
  When every Phase-2 outcome is `AdoptedExisting`, `pre_merge` was
  captured AFTER a prior crashed call's merge landed, so
  `reset_hard(pre_merge)` cannot un-merge and `fail_all` would mark the
  Bundles `IntegrationFailed` while their commits sit durably on the
  integration branch. The validation-failure path now branches on
  `outcomes.iter().all(|o| AdoptedExisting)`: if all adopted, skip
  rollback + fail_all entirely, run `clean_fd`, and return the new
  NON-terminal `IntegrationError::ValidationFailedAfterAdopt` (Bundles
  stay `Integrating`). The mixed/fresh-merge path is unchanged
  (reset + terminal `ValidationFailed`).
- **clean_fd on the success path (finding 12).** `git clean -fd` now
  runs after validation passes (or is skipped), not only on failure, so
  untracked build artifacts validation produced don't linger in the
  operator-visible tree on every successful Tick. `-fd` (not `-fdx`)
  leaves ignored paths (`.loopr/`) intact; the entry-time dirty-tree
  guard guarantees anything untracked at this point was produced by this
  integrate, so the clean is safe.
- **Validation instrumentation + head+tail output (validation.rs,
  finding 11).** `run_one` gains `#[instrument]` (command, timeout,
  recorded `elapsed_ms`) plus `debug!`/`warn!` on each terminal branch.
  Output truncation switched from head-only `truncate(CAP)` to
  `cap_head_tail` (first CAP/2 + elision marker + last CAP/2): cargo/test
  failures live at the tail, which head-only truncation discarded.
  `from_utf8_lossy` on the byte-boundary slices is panic-safe (the
  Phase-1 multibyte-truncate lesson).

#### Deviations
- None.

#### Tradeoffs
- `ValidationFailedAfterAdopt` leaves the Bundles `Integrating`, so the
  driver's retry contract re-enqueues them and validation will fail
  again - a potential loop. Chosen over the alternatives (silently
  un-merge durable commits, or mark terminal-while-merged) because both
  of those corrupt state; a visible, distinct, non-terminal error is the
  honest signal for operator/driver intervention. A smarter recovery
  (e.g., escalate after N adopt-validation failures) is out of Phase 2
  scope.

#### Open questions
- None.

### Commit D — determinism, branch deletion, multi-bundle re-entry

#### Design decisions
- **Pinned merge identity + dates (git.rs, finding 7).** `merge_no_ff`
  now stamps the merge commit with a fixed identity
  (`loopr-integrator <integrator@loopr>`, via `-c user.name/.email`) and
  pins `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE` to the bundle branch head's
  committer date (a fixed fact across runs). `run_git` was split into a
  thin delegate plus `run_git_with_env` (instrumented worker) so the
  merge can set env vars. Result: same bundles + same base = same merge
  SHA = same Tick SHA.
- **Bundle-branch deletion (lib.rs, finding 8).** After the Tick + all
  Merged writes are durable, each merged Bundle branch is deleted via
  `worktree::delete_branch` inside `spawn_blocking` (the worktree crate's
  sync git, kept off the tokio executor). Best-effort: a failed delete
  warns and does not fail an otherwise-successful Tick. Wires the
  previously-declared-but-unused `worktree` dep.
- **Multi-bundle partial-crash re-entry (lib.rs, finding 9).**
  `preflight_status` now tolerates `Merged` Bundles in the input slice;
  the prologue carries them as-is; the merge loop routes `Merged` through
  the same adopt-or-merge re-entry arm as `Integrating`
  (`Integrating | Merged`); the Phase-4 loop skips re-transitioning
  already-`Merged` Bundles. A driver re-entering with the FULL original
  slice reproduces the Tick's bundle-set key, so `DuplicateTick` resolves
  the existing Tick instead of double-writing.

#### Deviations
- **Behavior change: double-integration is now idempotent, not an
  error.** Tolerating `Merged` in `preflight_status` (finding 9) means a
  re-submitted fully-Merged Bundle is adopted and returns the existing
  Tick rather than `BundleNotAccepted{Merged}`. The seam test
  `double_integration_rejected_with_bundle_not_accepted_merged` was
  rewritten to `double_integration_is_idempotent_returns_existing_tick`
  (the old test pinned the now-incorrect reject behavior - same pattern
  as Phase 1 inverting `propose_bundle_with_no_changes`). Idempotent
  re-entry is the more correct semantics and consistent with the
  crash-recovery philosophy.

#### Tradeoffs
- Finding 9's re-entry correctness relies on the driver passing the FULL
  original slice (the documented retry contract). SUBSET re-entry (only
  the still-`Integrating` Bundles) would produce a different Tick
  bundle-set key and double-write; that remains unsupported and is noted
  here rather than guarded, since multi-bundle is gated behind
  `allow_multi_bundle = false` and this is preparatory work for that flip.

#### Open questions
- The determinism test verifies the pinned INPUTS (identity + dates on
  the merge commit) rather than cross-run SHA equality, because two
  independent test repos would also need identical plan/work ids (the
  branch name is in the merge message). Pinning the inputs is the
  mechanism finding 7 specifies; full cross-run SHA equality follows from
  it once the bundle commits themselves are identical.

## Phase 3: State integrity - OCC and FSM divergence

Landing across commits (mirrors Phase 1/2):
- **Commit A** — domain + store + integrator + decomposer local fixes
  with no Plan-OCC signature ripple (F2, F9, F10, F11, F12, F13).
- **Commit B** — Plan OCC + monotonic floor on Plans + routing (F1, F3,
  F8).
- **Commit C** — loopr daemon concurrency (F5, F6, F7).
- **Commit D** — `create_many` collision + handler re-mint (F4).

### Commit A — domain/store/integrator/decomposer local fixes

#### Design decisions
- **F2 (works/bundles monotonic floor) returns the floored
  `updated_at`.** The naive floor (`record.updated_at = max(now,
  current + 1)` applied only to the stored clone) introduced a
  regression: a caller chaining a second transition on the same
  in-memory record within one millisecond (the Integrator's
  Accepted->Integrating then Integrating->Merged, daemon's
  Integrated->Done) would capture the pre-floor `expected_updated_at`
  and hit a spurious `Stale`. Fix: `WorksStore::update` /
  `BundlesStore::update` (and the `WorkUpdateSink` / `BundleUpdateSink`
  traits + all impls) now RETURN the persisted floored `updated_at`;
  the chaining call sites (`integrator::transition_bundle_returning`,
  daemon `transition_and_persist_work`, `SummaryFanout`) write it back
  into their in-memory record. Non-chaining call sites ignore the
  `Ok(i64)`. This is the principled OCC discipline (the store owns the
  version; the caller refreshes from the store's answer).
- **F13 (store errors).** Added `StoreError::Closed` (maps
  `taskstore_async::Error::StoreClosed`, previously folded into `Io`) so
  shutdown paths can special-case the benign writer-channel-closed race.
  Added `StoreError::VersionMismatch { found, expected }` and
  `store::STORE_VERSION = 1`; `Store::open` reads `.version` (which
  taskstore writes write-if-absent) and rejects a mismatch or
  unparseable value.
- **F13 (notes collection label).** `NotesStore` error labels were
  `"operator_notes"` but the on-disk collection is `operatornotes`
  (struct ident lowercased+pluralized). Fixed both `create`/`get`
  labels; pinned the spelling with `notes_collection_name_is_operatornotes`.

#### Deviations
- **F12 `Corruption` variant: DELETED, not wired.** The doc offered
  "wire from list_tolerant totals or delete." There is no natural
  production construction site — corruption surfaces via the
  `CorruptionEntry` sidecar from `list_tolerant`, never as a
  `StoreError` — so the variant was genuinely never constructed (hence
  the `#[allow(dead_code)]`). Deleted it, its `map_store_error` arm, and
  its test; the operator-facing corruption gate (`LooprError::CorruptionGate`)
  is separate and untouched.
- **F12 `base_tick_id` removal is a breaking JSONL change.** `Bundle`
  is `#[serde(deny_unknown_fields)]`, so a `bundles.jsonl` row written
  before this change (carrying `"base_tick_id": null`) will fail to
  deserialize and surface as a `CorruptionEntry`. Acceptable on the v5
  pre-release branch where stores are recreated per E2E run; noted so a
  future reader doesn't mistake it for a regression.
- **F12 "false comment" (lib.rs:634-638 claiming an `Accepted =>
  IntegrationFailed` edge):** not found in the current code. The
  doc's line numbers predate Phase 2's full `lib.rs` rewrite, which
  already corrected the merge-loop comments. Treated as
  resolved-by-Phase-2.

#### Tradeoffs
- Returning `updated_at` from `update` (vs. re-fetching between chained
  transitions) keeps the existing in-memory-chaining call pattern intact
  and adds no extra store round-trip; the cost is a wider sink-trait
  return type, absorbed by the ~50 non-chaining call sites that simply
  ignore the `Ok(i64)`.
- `fresh_tick` test helper (store ticks tests) now generates one
  merge_commit per bundle to satisfy F12's new `Tick::new`
  `debug_assert_eq!` parity check; the affected tests assert on the
  bundle-set dedup key, not merge-commit content, so the change is
  semantics-preserving.

#### Open questions
- None for Commit A.

### Commit B — Plan OCC + monotonic floor + routing

#### Design decisions
- **F1 (Plan OCC).** `PlansStore::update` now takes `expected_updated_at`,
  acquires a new `Store::plan_update_lock`, pre-`get`s the record (which
  doubles as F3's missing-id guard — a missing id is `RecordNotFound`
  instead of taskstore's silent upsert-create), checks the OCC token,
  floors `updated_at` (F2-plans), and returns the floored value. The
  `PlanUpdateSink` trait gained `expected_updated_at` + a `Stale` variant
  and returns `i64`, exactly mirroring `BundleUpdateSink`/`WorkUpdateSink`.
  This is the signature-ripple tranche the design doc's Rollout note 2
  calls out: `SummaryFanout`, `transition_and_persist_plan`, the agents
  `DirectorStore::update_plan` (+ its two Stalled-persist call sites), and
  the override handler all compile against the new shape in this one
  commit (no coexistence).
- **F1 (re-fetch).** `transition_and_persist_plan` captures
  `expected_updated_at` from `plan.updated_at` immediately before the FSM
  call and writes the persisted floored value back. The integrator
  completion path (`spawn_integrator_for_bundle`) now RE-FETCHES the Plan
  fresh right before the `Active -> Complete` transition instead of
  reusing the snapshot loaded minutes earlier at method entry — so a
  concurrent Director/IPC write since then doesn't make the OCC
  `expected` stale and reject the Complete spuriously.
- **F8 (override routing).** `handle_plan_override` now routes through
  `transition_and_persist_plan` (gaining OCC + summary fanout + the
  terminal-state event it previously bypassed via raw
  `store.plans().update`). To support the operator override edges
  (`Stalled => Active|Abandoned`), `transition_and_persist_plan` gained an
  `override_: bool` that selects `override_status` vs `transition` —
  mirroring `transition_and_persist_work`'s existing `override_` flag.
  Same-class fix: the reviewer Step-3 InProgress->InReview repair now
  routes through `&*self.summary_fanout` instead of `&self.store`.

#### Deviations
- The override handler's error mapping collapses FSM-reject and OCC-Stale
  to `RpcError::InvalidRequest` (both client-actionable) by matching the
  helper's `String` error. A genuine IO persist failure would also map to
  InvalidRequest rather than Internal — a minor RPC-code imprecision
  accepted because `transition_and_persist_plan` returns `String`, not a
  typed error (a typed-result refactor for the plan helper is not a Phase
  3 finding; the work helper's typed result is F5/Commit C). The error
  message carries the detail regardless.
- The override handler passes `PlanSummaryExtras::default()` (0 ticks /
  bundles) rather than computing them — `compute_plan_summary_extras` is
  private to `context.rs`. The terminal-state event on an operator-driven
  abandon therefore shows 0 for those counts; the per-Work counts (from
  the fetched children) are accurate. F8's goal is the routing, not
  extras precision.

#### Tradeoffs
- Pre-existing test flake surfaced and fixed: `tests/work_plan_summary_visibility.rs`'s
  two `current_thread` tests both install a thread-local `init_for_test`
  subscriber and read their own tempdir `events.log`; run concurrently in
  one binary they raced on subscriber capture (~20-33% flake, present on
  the Commit A tree too — NOT a Commit B regression). Serialized them
  behind a `static tokio::sync::Mutex` (async lock to avoid the std-Mutex
  `await_holding_lock` clippy lint). Verified 12/12 green after.
- `plan_override_rejected_fsm_edge_yields_invalid_request` asserted the
  error message contained `"FSM"`; the routed-through-helper message is
  now lowercase `"...fsm override rejected..."`, so the assertion is
  case-insensitive. Behavior (InvalidRequest + plan unmutated) is
  unchanged.

#### Open questions
- None for Commit B.

### Commit C — loopr daemon concurrency (F5, F6, F7)

#### Design decisions
- **F5 (typed result + spawn gate).** `transition_and_persist_work` now
  returns a typed `TransitionError` (`Fsm` / `HardCap` / `Stale` /
  `Persist`) instead of `String`. `override_work` gates the Implementer
  spawn on PERSIST SUCCESS: the pre-fix code spawned off the
  locally-mutated `work.status == Ready` even when the persist failed
  (spawning for a Work whose persisted state belongs to the racing
  winner). A `Stale` is benign (logs debug, no spawn); any other error
  logs warn and does not spawn. All ~18 other call sites use
  `let _ =`/`if let Err(e)` and consume the typed error via `Display`
  unchanged.
- **F6 (reviewer Stale not conflated).** `run_reviewer` returns
  `ReviewerError::Update(BundleUpdateError::Stale)` when it loses the
  Bundle OCC race (the winning Reviewer already persisted the verdict).
  The daemon's reviewer match added an explicit arm that drops the losing
  verdict silently and leaves the Work untouched, instead of force-
  Blocking it (which manufactured Bundle-Reviewed-while-Work-Blocked
  divergence). Mirrors spawner.rs's accept_bundle Stale handling.
- **F7 (runtime + transitive blocking).** `block_dependent_siblings` now
  blocks the FULL transitive closure of dependents (BFS over
  `WorkGraph::dependents_of` to a fixpoint) rather than only the direct
  ones — a Work depending even transitively on a terminal Work can never
  have all deps reach Done. It is now invoked at RUNTIME from
  `override_work` when a Director override terminalizes the Work
  (Abandoned/Superseded), not only at startup reconcile.

#### Deviations
- The doc says "invoke from `transition_and_persist_work`'s terminal
  path." That free function is generic over `S: WorkUpdateSink` and has no
  `DaemonContext` handle (needed for the sibling listing + recursion), so
  the invocation lives at the runtime call site that actually terminalizes
  a Work — `override_work` — which is the only daemon path that reaches
  Abandoned/Superseded today (a grep confirms no other site transitions a
  Work to those states). Same effect, correct seam.
- F7's `block_dependent_siblings` blocked_reason still names the
  originating terminal Work even for transitively-blocked dependents
  (whose direct dep is the now-Blocked intermediate, not the terminal
  Work). The root-cause attribution is the useful operator signal; not
  worth threading the intermediate.

#### Tradeoffs
- **Test regression-test placement.** `block_dependent_siblings` is
  `pub(crate)` and needs a full `DaemonContext`, and the F5 spawn-gate /
  F6 reviewer-Stale paths are daemon-orchestration scenarios — the design
  doc's Phase 11 explicitly lists these daemon integration tests
  ("override_work persist-failure suppresses spawn", reviewer/director
  scenarios). The testable CORE of F7 (the transitive-dependents BFS over
  the public `WorkGraph` API) landed as a domain test here; the
  end-to-end daemon scenarios land in Phase 11 per the doc's structure.
- **Same-ms chained-transition test helpers.** The F2 floor (Commit A)
  makes `updated_at` strictly increasing, so hand-rolled test helpers that
  chain `transition` + `store.update(.., expected)` on one in-memory
  record WITHOUT writing back the returned floored ts hit a spurious
  `Stale` when two writes land in the same millisecond. Fixed the one such
  helper found (`director_stuck_states.rs` bundle seed); the two work-seed
  loops re-fetch after each update and are already safe. Production chains
  go through `transition_and_persist_work` / `transition_bundle_returning`,
  which write the floored ts back (Commit A).

#### Open questions
- None for Commit C.

### Commit D — create_many collision + re-mint (F4)

#### Design decisions
- **Store pre-check.** `WorksStore::create_many` now pre-checks every
  incoming id against the store AND for intra-batch duplicates, returning
  `StoreError::AlreadyExists` on the first collision instead of letting
  taskstore's `INSERT OR REPLACE` silently overwrite an earlier Plan's
  Work. The stale "a fresh decomposition never has id collisions" comment
  was replaced with the real ~0.8%-per-1k-records math.
- **Handler re-mint.** `decompose_and_dispatch` persists through a new
  `persist_works_with_remint` helper: on `AlreadyExists` it re-mints EVERY
  id in the batch (`remint_work_batch`) and remaps each Work's dependency
  edges through the old->new map, then retries (bounded to 5 attempts).
  The helper returns the (possibly re-minted) works so the downstream
  dep-gate partition + Implementer spawns operate on the ids that actually
  landed on disk.

#### Deviations
- The doc says "re-mint the colliding ids"; the implementation re-mints
  the WHOLE batch on any collision rather than surgically re-minting only
  the offender. Reason: `create_many` returns one colliding id per call,
  and re-minting the whole batch is simpler, keeps the dep-edge remap
  trivially total, and is cheap given collisions are rare. The net effect
  (no overwrite, deps stay consistent) is identical.

#### Tradeoffs
- The store pre-check is N sequential `get`s per batch (decompose batches
  are single-digit Works), not a bulk existence query — acceptable at this
  cardinality and consistent with `create`'s existing per-id pre-check.
- The handler's persist-failure path (non-collision store error, or
  re-mint exhaustion) folds into the existing `stall_plan_after_decompose_failure`
  via an early `return`, replacing the old inline `Err` arm — same
  operator-visible outcome (Plan -> Stalled).

#### Open questions
- None for Commit D. The end-to-end "handler re-mints on a real collision"
  integration test is a Phase 11 item (the doc's store/domain test-gap
  list); the store-level collision rejection and the pure re-mint remap
  are unit-tested here.

## Phase 4: Doom-loop brakes and panic posture

Landing across commits (mirrors Phases 1-3):
- **Commit A** — domain `FailureReason` foundation (the typed failure
  enum + the `failure_reason` field on Work/Bundle).
- **Commit B** — loopr panic posture (catch_unwind, persist Panic,
  cleanup-on-panic, startup CrashInterrupted) + `session_failure_count`
  increment.
- **Commit C** — Director brakes (config caps, pattern/mode escalation,
  parse-requery counters, need_help->Stalled, restart backoff, operator
  notes cap, minors).
- **Commit D** — lifeguard consecutive-streak semantics +
  rejected-bundle feedback wiring.
- **Commit E** — decomposer validation-retry + max-children bound.
- **Commit F** — handler Stalled->Active re-decompose + spawner
  supersede-Triaged-on-Blocked.

### Commit A — domain FailureReason foundation

#### Design decisions
- **`FailureReason` lives in its own `domain/src/failure.rs` module**
  (single-word filename per the naming rule) with the exact variant set
  the vision specifies: `TokenBudget`, `ToolFailure { tool }`,
  `ReviewerRejection`, `AcUnmet`, `Panic`, `CrashInterrupted`,
  `Other(String)`. Externally-tagged serde, kebab-case variant tags.
- **No new `error: String` companion field.** The Data Model line says
  the enum is "carried on Work/Bundle with a companion error: String";
  both records already have that companion in domain-appropriate form
  (`Work.blocked_reason`, `Bundle.verification`), so `FailureReason` is
  added as the discriminant alongside the existing prose field rather
  than introducing a redundant third field. Documented on each field.
- **Field is `Option<FailureReason>` with `#[serde(default)]`** on both
  `Work` and `Bundle` — additive, so JSONL rows written before this
  enum existed deserialize clean as `None` (verified by a unit test).

#### Deviations
- None.

#### Tradeoffs
- Reusing `blocked_reason`/`verification` as the companion prose (vs. a
  dedicated `error: String`) avoids a fourth Work field and a breaking
  JSONL change; the cost is that the detail's field name differs by
  record kind, which the doc comments call out.

#### Open questions
- None for Commit A. The enum's *writers* (panic posture, reconcile,
  failure arms) land in Commits B-F; Commit A is the type + field only,
  hence the `dead_code`-free export (the field is constructed by
  `Work::new`/`Bundle::new` as `None`).

### Commit B — loopr panic posture + session_failure_count

#### Design decisions
- **`catch_unwind` at all three role invocation sites.** `run_implementer`
  (context.rs spawn_implementer_for_work), `run_reviewer`
  (spawn_reviewer_for_bundle), and `run_director` (both spawn sites:
  handler.rs `spawn_director_for_plan` and startup.rs
  `startup_reconcile_directors`) are wrapped in
  `std::panic::AssertUnwindSafe(fut).catch_unwind().await`. The match arms
  gained an `Err(panic)` (panic) variant and the old `Err(..)` arms became
  `Ok(Err(..))`. A pure `panic_message(&(dyn Any + Send)) -> String` helper
  (pub(crate) in context.rs, downcasts `&str`/`String`, else opaque)
  renders the payload; unit-tested in context/tests.rs.
- **Implementer panic posture (the leak fix).** A panic used to abort the
  whole `spawn_implementer_for_work` task BEFORE its worktree-cleanup tail
  ran, leaking the worktree. Catching the panic records
  `FailureReason::Panic` + `blocked_reason`, increments
  `session_failure_count`, transitions the Work to `Blocked`, and falls
  through to the SAME cleanup tail — so the worktree is reclaimed.
- **`session_failure_count` is now live (bullet 9).** Incremented (saturating)
  in all three implementer failure arms (panic / EscalationNeeded / other
  error) before the Blocked persist. The doc's "fold bundle_rejections
  into the existing attempt/rejection accounting" is satisfied by the
  existing `attempt_count` (Layer-1 retry budget) + this now-live
  `session_failure_count`; no separate `bundle_rejections` field is added
  (it never existed and the two live counters cover the advertised nets).
- **`failure_reason` populated on the non-panic failure arms too.** The
  implementer/reviewer EscalationNeeded and generic-error arms set
  `FailureReason::Other(detail)` so the typed discriminant is meaningful,
  not only set on the Panic path. The reviewer panic arm records `Panic`
  on the Work (the record that gets persisted Blocked); the Bundle is left
  Triaged and superseded by the next recovery sweep's Work-Blocked entry
  guard (Commit F).
- **Director panic posture.** A `run_director` panic is logged at `error!`
  with the payload; the per-Plan `Notify` + status-snapshot cleanup that
  follows the match still runs (the JoinSet would otherwise swallow the
  panic and leak both sidecar entries). The Plan is NOT force-Stalled from
  the panic arm — see Open questions.
- **`CrashInterrupted` reconcile (startup.rs).** The carry-forward branch
  of `sweep_worktrees` (a non-terminal Work with a worktree on disk at boot
  = a prior crash mid-flight) now stamps `FailureReason::CrashInterrupted`
  on the Work via a best-effort OCC `update`, replacing the "Stage 7 will
  mark crash-interrupted when that field exists" placeholder. Idempotent:
  skipped when already stamped, so repeated boots don't churn `updated_at`.

#### Deviations
- The doc bullet says "wrap ... a panic persists FailureReason::Panic on
  the Work/Bundle." For the Director there is no Work/Bundle (it supervises
  a Plan, which has no `failure_reason` field), so the Director panic arm
  logs + lets cleanup run rather than persisting a typed reason. This is the
  honest mapping of the requirement onto the Director's record shape.

#### Tradeoffs
- `failure_reason` is set on failure but never CLEARED on a subsequent
  successful transition (e.g. a CrashInterrupted Work that later reaches
  InReview keeps the stale reason). Clearing would mean threading reason
  resets through `transition_and_persist_work`; out of scope for Phase 4
  and low-harm (the field is diagnostic, the live status is authoritative).

#### Open questions
- A panicked Director leaves its Plan `Active` with no supervisor until a
  daemon restart's `startup_reconcile_directors` respawns it. Forcing the
  Plan to `Stalled` from the panic arm (matching the NeedHelp posture)
  would make the stall visible immediately, but the panic arm lacks a
  summary-fanout handle there; deferred as a possible follow-up.
- The 3 `failure_paths.rs` E2E tests still fail after Commit B (unchanged
  from the pre-Phase-4 baseline): the root cause is an unscripted extra
  implementer/reviewer `complete_free` call (a doom-loop symptom). Commit B
  catches the resulting panic instead of letting the JoinSet swallow it,
  but the EXTRA call itself is what the later Phase 4 brakes + Phase 7
  target. Re-checked at phase end.

### Commit C — Director brakes (config caps, pattern/mode, parse-requery, stalls, restart, notes cap, minors)

#### Design decisions
- **Iteration + wall-clock caps (bullet 2).** `DirectorConfig` gains
  `max_iterations` (default 10_000) and `max_wall_clock_secs` (default
  86_400). Enforced in `run_director_inner`'s loop after each iteration;
  exhaustion routes through the shared stall helper. Per-session (reset on
  restart). The pattern tracker + NeedsOperator grace remain the primary
  brakes; these are the absolute backstop the old "comment claims a cap"
  referenced but never had.
- **`stall_plan_and_need_help` helper (bullets 2/6 + DRY of existing
  sites).** One owner for the "persist Plan -> Stalled, then return
  NeedHelp" exit, shared by retry-budget exhaustion, NeedsOperator-grace
  timeout, the new iteration/wall-clock caps, and the LLM-emitted
  need_help. **Best-effort**: a get/transition/persist failure logs a
  `warn!` and STILL returns NeedHelp — a budget/need_help exit must
  TERMINATE the Director, never restart-loop on a failed write (the old
  inline sites used `?`, which would re-enter the restart dispatcher). The
  Plan-Active-then-reconcile fallback covers a failed stall.
- **need_help stalls the Plan (bullet 6).** An LLM `need_help` now routes
  through the helper instead of returning NeedHelp with the Plan still
  Active (the invisible-stall bug). Test updated to assert Stalled.
- **SameAction feeds escalation + is_mutating gate (bullets 3, 4).**
  `consecutive_same_action` returns `None` for a non-mutating trailing run
  (idle `done` no longer trips SameAction -> Conservative). A SameAction
  trip now increments the shared `no_progress_streak` and escalates to
  `EscalationTripped { reason: "same_action_sustained" }` once the streak
  reaches `escalation_threshold` — so a repeated mutating action against
  static state reaches NeedsOperator instead of pinning Conservative
  forever. This also makes the `Recovered` demotion (gated on
  `streak > 0`) reachable from a Conservative entered via SameAction,
  closing the one-way trap WITHOUT mutating the mode FSM (which already
  demotes on Recovered). New pattern tests + the rewritten
  `grace_counter_does_not_trip_outside_needs_operator` comment.
- **Per-iteration parse-requery counters (bullet 5).** Both
  `director.rs` and `implementer.rs` replaced the
  `(messages.len() - 1) / 2` derivation (which folds in cross-iteration
  history, starving requeries from iteration 3 on) with an explicit
  per-iteration `requeries_used` counter.
- **Restart backoff + budget reset + in-iteration Id (bullet 7).**
  `run_director` sleeps `restart_backoff(n)` (250ms x 2^(n-1), cap 10s,
  shutdown-interruptible) before each restart; resets the restart counter
  when the failed session completed >= `HEALTHY_ITERS_BEFORE_RESTART_RESET`
  (10) iterations (a long-lived Plan must not die on its Nth-ever
  transient blip). `run_director_inner` reports `iterations_completed` via
  an out-param. Hallucinated/invalid ids (bundle_id/work_id/target_status)
  are now skipped in-iteration with a `warn!` (`continue`) instead of
  returning `DirectorError::Id` and burning a restart.
- **Operator-notes render cap (bullet 11a).** Notes rendered into the user
  prompt are capped at `OPERATOR_NOTES_RENDER_CAP` (8) newest + an
  "[N older omitted]" marker. ALL unread notes are still marked read (the
  cap is render-only).
- **Post-assembly token_budget check (bullet 11b).** `build_for_director`
  and `build_for_researcher` warn when the assembled context exceeds
  `token_budget` (history trimming only bounds history turns; a large
  system+state could still overshoot).
- **Config-driven retry budget in prompt (bullet 16).** `system.pmt`
  de-hardcoded from "3" to "the retry budget shown in the user message"
  (keeping it byte-stable for the prompt cache); `user.pmt` renders the
  actual `max_work_attempts`. Threaded via `DirectorState.max_work_attempts`
  (set from config in `run_once`, like `mode`).
- **Other minors (bullet 16).** In-memory Director `history` truncated to
  `DIRECTOR_HISTORY_MAX_MESSAGES` (40); Notify exit-cleanup is
  compare-before-remove (`Arc::ptr_eq`) in both spawn sites so a respawned
  Director's fresh Notify is not deleted; triple-parse in
  `parse_director_actions` reduced to capturing the first (array-shape)
  error; `#[instrument]` added to `parse_director_actions` and
  `build_director_state`; mode.rs operator-note comment aligned to the
  actual "reset only on demotion" behavior; off-by-one grace pinned with
  `needs_operator_grace_stalls_exactly_on_nth_iteration`.

#### Deviations
- **`DirectorError::Id` is now constructed nowhere** (the three id-parse
  sites skip in-iteration). The variant + its `restart_reason_for` arm are
  retained (pub enum; harmless) rather than removed, to keep the error
  taxonomy stable and avoid a churny removal.
- **Director panic posture (Commit B) does not stall the Plan**; a
  panicked Director leaves the Plan Active for restart-reconcile. The
  stall helper added here is only reached on the graceful terminal exits.

#### Tradeoffs
- The SameAction escalation reuses the single `no_progress_streak` rather
  than a separate same-action streak: simpler, and both pathologies are
  "no measurable progress," so one shared counter toward NeedsOperator is
  the right granularity. `same_action_streak()` (status snapshot) still
  reports the derived trailing-run length independently.

#### Open questions
- `DirectorStatusSnapshot.unread_note_count` is set AFTER mark-read, so it
  counts notes-observed-this-iteration rather than currently-unread. The
  rename suggested by bullet 16 ripples across `ipc` + the loopr handler +
  tests for a wire-serialized field; deferred as disproportionate to a
  cosmetic nit. The field's meaning is unchanged.

### Commit D — lifeguard consecutive streak + rejected-bundle feedback

#### Design decisions
- **Lifeguard consecutive-run semantics (bullet 10).** Replaced the
  cumulative-per-hash `action_counts: HashMap<u64, u32>` with a
  `consecutive_count: u32` keyed off the now-load-bearing `last_hash`:
  same-as-previous extends the run, a different action resets it to 1.
  A,B,A,B,A (3 total A's, never 3 in a row) no longer escalates; the
  doc-promised "consecutive" semantics now hold. New tests:
  `interleaved_repeats_do_not_escalate_consecutive_only` and
  `consecutive_run_resets_after_interruption`. All existing lifeguard
  tests (a,a,a escalates; key-reorder dedup) remain green — they were
  already consecutive-shaped.
- **Rejected-bundle feedback wired (bullet 8).** `spawn_implementer_for_work`
  loads the work's most-recent `Rejected` Bundle (max by `updated_at`) and
  threads its non-empty `verification` into
  `StateSummary { rejected_bundle_reason }`, replacing the hardcoded
  `StateSummary::default()`. The retry Implementer now sees WHY the prior
  bundle was rejected — the doom-loop feedback channel that existed
  end-to-end except this one wire. Lookup failure is best-effort (warn +
  retry without feedback).

#### Deviations
- None.

#### Tradeoffs
- Rejected-bundle lookup is a per-spawn `list_by_work_id` + in-memory
  filter/max rather than an indexed "latest rejected" query. Bundle
  cardinality per Work is tiny (a handful of attempts), so the scan is
  cheaper than adding a new indexed accessor.

#### Open questions
- None.

### Commit E — decomposer validation-retry + max-children bound

#### Design decisions
- **Unified attempt loop (bullet 12).** `decompose`'s tail was rewritten
  so a single retry covers BOTH a transient LLM error AND a post-parse
  validation error: the first failure (either kind) re-prompts via
  `assemble_user(goal, Some(err))` and a second failure bails. Pre-fix the
  seven validation errors bailed immediately, never triggering the
  retry-with-error the error machinery + design doc promised. Structured
  as a labeled block (attempt 1) + a post-block (attempt 2) rather than a
  real loop, since `MAX_DECOMPOSE_ATTEMPTS = 2` means exactly one retry.
- **`parse_and_validate` extracted.** Pure parse + the seven validation
  checks (no I/O, no transcript, no LLM); returns `(works, response)` or a
  `Box<ValidationFailure>` carrying the typed error, the stable transcript
  outcome label, and the pre-rendered child lines. The caller owns retry +
  transcript writing, so the validation logic is testable in isolation and
  reusable across both attempts.
- **`DecomposerConfig` + `max_children` (bullet 14).** New `DecomposerConfig`
  (the crate's own config, per its CLAUDE.md) with `max_children` (default
  10), composed into the top-level loopr `Config` as `decomposer.*` and
  threaded through `DaemonContext::decomposer_config` into
  `decompose(plan, target, llm, config)`. A decomposition exceeding the
  bound is a `TooManyChildren` validation error (new variant) checked
  beside the zero-children floor — it goes through the same retry path.
- **`DecomposerError::LlmFailed(Box<LlmError>)`.** Boxed the large
  `LlmError` so `DecomposerError` (and anything carrying it) shrinks.
- **`ValidationFailure` boxed.** `DecomposerError` still embeds the large
  `context::PromptError`, so `parse_and_validate`'s `Result` tripped
  `clippy::result_large_err`; the `Err` is `Box<ValidationFailure>` via a
  `boxed` constructor.

#### Deviations
- The unified budget (2 attempts) slightly changes the old behavior: the
  initial LLM-error retry and the (previously absent) validation retry now
  share one budget. A validation failure on attempt 1 always retries; a
  failure on attempt 2 always bails. Net: validation gets the "once before
  bailing" the doc specifies; LLM-error retry is unchanged (still 2 calls
  max).

#### Tradeoffs
- Validation-failure transcripts use the plain outcome label on BOTH the
  retry write and the final bail write (no `_retrying` suffix, unlike the
  LLM path's `llm_failed_retrying`). The existing transcript tests assert
  the plain label, and the iteration number distinguishes the two blocks.

#### Open questions
- None.

### Commit F — Stalled->Active re-decompose + supersede-Triaged-on-Blocked

#### Design decisions
- **Zero-Works revival (bullet 13).** `handle_plan_override`'s
  Stalled->Active branch now checks whether the Plan has Works (captured
  before `children` is moved into the transition). With Works -> respawn
  the Director (unchanged). With ZERO Works -> spawn
  `decompose_and_dispatch` on `plan_create_tasks`, which re-decomposes,
  persists Works, spawns Implementers, AND spawns the Director — closing
  the documented dead-end where `plan override --to active` neither
  re-decomposed nor revisited a Plan that stalled during decomposition.
- **Boot-time zero-Works reconcile (bullet 13, shutdown/drain gap).**
  `startup_reconcile_directors` now re-decomposes an Active Plan with zero
  Works (the same `decompose_and_dispatch` path, made `pub(crate)`)
  instead of spawning a Director over nothing. This covers the
  `plan_create_tasks` `shutting_down` early-return and the drain-timeout
  abort, both of which can leave an Active-zero-Works Plan. A store error
  during the work-count check falls through to the normal Director spawn
  (safe default: don't re-decompose on uncertainty).
- **Supersede Triaged Bundle when Work is Blocked (bullet 15).**
  `spawn_reviewer_for_bundle`'s Step-3 Work-status repair gained an
  explicit `WorkStatus::Blocked` arm: the (already-Triaged) Bundle is
  transitioned to `Superseded` (Reactor) and stamped
  `FailureReason::Other`. Pre-fix a Triaged Bundle whose Work went Blocked
  was re-driven by every recovery sweep forever (the reviewer exits at
  entry; the Bundle never reached a terminal state).

#### Deviations
- `cold_boot_respawns_director_for_active_plan` was updated to seed a Work
  alongside the Plan — a zero-Works Active Plan now re-decomposes rather
  than respawning a Director, so the test's original premise (director
  respawn) is expressed with a realistic mid-flight Plan. New test
  `cold_boot_redecomposes_active_plan_with_zero_works` pins the zero-Works
  re-decompose path (asserts one `plan_create_tasks` task, zero Director
  tasks). Same inversion pattern as Phase 1/2's behavior-change tests.

#### Tradeoffs
- The boot reconcile and the override both run `decompose_and_dispatch`
  for the zero-Works case; the reconcile passes `request_id = 0` (boot has
  no IPC request id). The shared helper keeps the two revival paths
  identical.

#### Open questions
- None.

### Commit G — bloat fix (context.rs decomposition) + cargo fmt

#### Design decisions
- Phase 4's additions to `loopr/src/daemon/context.rs` (panic posture,
  rejected-bundle feedback, supersede-on-Blocked arm, decomposer_config,
  panic_message helper) pushed it to 1549 lines, over the 1500 bloat gate.
  Extracted `spawn_integrator_for_bundle` (~220 lines) into
  `context/integration.rs` (an inherent-impl method on `DaemonContext` in
  a child module — same pattern as the existing `spawner.rs`), bringing
  context.rs to 1332. Named `integration` (not `integrator`) to avoid
  shadowing the external `integrator` crate inside the module.
- `cargo fmt --all` normalized the phase's new code (pre-existing repo
  convention, mirrors Phase 3's housekeeping commit).

#### Deviations / Tradeoffs / Open questions
- None.

## Phase 5: Containment - denylist, sandbox, prompt injection, scope

Landing across commits (mirrors Phases 1-4):
- **Commit A** — denylist hardening (findings 1-3): `sh|bash|zsh -c`
  recursion, argv[0] basename normalization, structural `rm` matching.
- **Commit B** — Bash bwrap containment + lane shape (finding 4).
- **Commit C** — builtin hardening (findings 5-8): edit/write
  non-UTF8 + atomic, grep/glob excludes, spawn drain bound, read cap.
- **Commit D** — tools config/error (findings 13, 14).
- **Commit E** — prompt-injection fencing (finding 9).
- **Commit F** — work-scope enforcement end-to-end (finding 10).
- **Commit G** — telemetry + worktree path/branch guards (findings 11, 12).

### Commit A — denylist hardening (findings 1-3)

#### Design decisions
- **`check`/`check_tree` delegate to a new `check_inner(tree, source,
  depth)`** so the `sh|bash|zsh -c <payload>` recursion (finding 1) has a
  bound (`MAX_SHELL_C_DEPTH = 8`). The payload is a single argv token
  (quotes already stripped by `argv_text`); `shell_c_index` locates the
  token after `-c` and `check_inner` re-parses + re-checks it. Nested
  `bash -c "bash -c '...'"` terminates at the depth cap.
- **`normalized_argv` (finding 2).** Per-command, index 0 is reduced to its
  basename (`basename`: strips a leading `./` then everything through the
  last `/`). Patterns are matched against BOTH the raw argv and the
  normalized one: the raw match preserves user-extension patterns written
  as literal paths (`./deploy.sh`, exercised by `extend_from_*` tests),
  while the normalized match catches `/usr/bin/git push`-style absolute
  invocations of the built-in denials. `is_shell_sink_command`
  (pipe-to-shell detection) also normalizes its head so `... | /bin/sh`
  is caught.
- **Structural `rm` (finding 3): `dangerous_rm`.** Parses flags
  (`-rf`/`-fr`/`-r -f`, `--recursive`/`--force`) in any order/grouping and
  requires BOTH recursive and force, then a catastrophic target (`/`,
  `/*`, `~`, `~/...`, `$HOME`, `$HOME/...`). The two literal `rm -rf /` /
  `rm -rf ~` `base()` patterns are retained ONLY as the reason carriers at
  `RM_ROOT_IDX`/`RM_HOME_IDX`; their `tokens` are no longer matched (added
  to `SYNTHETIC_IDXS` alongside the pipe-to-shell carrier).

#### Deviations
- None.

#### Tradeoffs
- `dangerous_rm`'s target set is the doc's listed catastrophic roots plus
  the `~/` and `$HOME/` prefixes (a recursive-force against the home tree).
  It deliberately does NOT flag `rm -rf /usr` or other absolute subpaths —
  the goal is the unambiguous footguns, not a filesystem-policy engine; the
  bwrap filesystem containment (Commit B) is the backstop for the rest.

#### Open questions
- None.

### Commit B — Bash bwrap containment + lane shape (finding 4)

#### Design decisions
- **`LanePolicy.sandbox_net` split into `sandbox` + `network`.** The old
  flag conflated "wrap in bwrap" with "unshare network." New shape:
  `Local` (sandbox=true, network=false → `--unshare-net`), `Net`/Bash
  (sandbox=true, network=true → bwrap WITHOUT `--unshare-net`, full
  filesystem containment), `Heavy` (sandbox=false → unsandboxed, builds
  write outside the worktree). The router wrap decision is now
  `policy.sandbox && bwrap_functional && !Off`, passing `policy.network`
  into `bwrap_command`.
- **`bwrap_command(cmd, working_dir, network)`.** `network=false` adds
  `--unshare-net`; `true` omits it. All other containment flags
  (`--die-with-parent`, `--ro-bind / /`, `--dev`, `--proc`,
  `--bind /tmp`, `--bind <cwd>`, `--chdir <cwd>`) are unconditional.
- **`detect_bwrap_functional` now probes the full flag set.** Was
  `--unshare-net --ro-bind / /` only; now mirrors `bwrap_command`'s mount
  flags (`--dev`/`--proc`/`--bind`/`--chdir`) so a kernel that rejects
  `--proc` surfaces at startup, not first tool call. Probes with
  `--unshare-net` (the strictest, Local shape); the Net lane uses a strict
  subset, so a probe pass guarantees both lanes wrap.
- **Vision amended (finding 4's "amend the lane table").** The lane table,
  the `classify` ABI bullet, and the `security.sandbox` posture row now
  describe the `Net`-under-bwrap-with-network shape and call out the
  behavior change + the `preferred`/`off` escape hatch. (The amendment-log
  `a8` entry itself lands in Phase 10's doc-truth batch.)

#### Deviations
- None. (The doc left the exact `Net`-lane shape as a decision — "wrap Bash
  in bwrap WITH network" — which is implemented verbatim.)

#### Tradeoffs
- Network-allowed (`Net`) is a strict subset of the Local restrictions, so
  reusing the `--unshare-net` probe for detection is sound; a separate
  network-allowed probe would add a second `bwrap` fork at startup for no
  additional coverage.
- Behavior-changing for target build scripts that assume an unconfined
  shell (noted in the doc's Rollout). The escape hatch is the existing
  `security.sandbox: preferred|off` knob, not a new flag.

#### Open questions
- None.

### Commit C — builtin hardening (findings 5-8)

#### Design decisions
- **`atomic_write` (finding 5) in `builtin/path.rs`.** Both Edit and Write
  now write a sibling `.{name}.loopr-tmp-<uuid>` file then `rename` over the
  target (atomic on POSIX within one fs); the temp is best-effort removed on
  rename failure. Uses `Uuid::now_v7()` (the feature enabled on the `tools`
  `uuid` dep; `v4` is not).
- **Edit rejects non-UTF8 (finding 5).** `String::from_utf8(bytes)` →
  `Error::NonUtf8(path)` (mapped to `ToolError::ExecutionFailed`) instead of
  `from_utf8_lossy`, which would U+FFFD-corrupt a binary file on write-back.
  Write's input is a `String`, so only the atomicity half applies there.
- **grep excludes `.git`/`.loopr` (finding 6).** `--exclude-dir=.git
  --exclude-dir=.loopr` on the `grep -rn` argv.
- **glob `require_literal_leading_dot: true` (finding 6).** A `*` wildcard
  no longer matches a leading dot, so `**/*.rs` does not descend `.git`/
  `.loopr`. Note: glob 0.3's `.*` does NOT then match arbitrary dotfiles
  (verified empirically — it returns only `.`/`..`); an explicitly-named
  dotfile pattern (`.env`) still matches, which is the intended escape.
- **Bounded spawn drain (finding 7).** After the foreground child exits,
  the reader-task drain is wrapped in a `DRAIN_TIMEOUT_SECS` (5s) timeout. On
  expiry — a backgrounded grandchild holding the pipe write end — a new
  `force_kill_group` SIGKILLs the process group (Pgid) to close the pipes,
  then a 1s grace lets the readers observe EOF. `BwrapChild` needs no extra
  kill (the PID namespace + `--die-with-parent` already cascade).
- **read byte cap (finding 8).** `File::open` + `take(MAX_READ_BYTES + 1)`
  (16 MiB) replaces the unbounded `fs::read`; reading one byte past the cap
  detects oversize, truncates to the cap, and flags `truncated`.

#### Deviations
- The glob "literal dotfile still matches" test asserts an exact-name
  pattern (`.env`) rather than `.*`, because glob 0.3 + leading-dot does not
  match `.env` via `.*`. This is glob library behavior, not a divergence
  from the finding (which only requires that wildcards stop descending
  dotdirs).

#### Tradeoffs
- `force_kill_group` on the drain-timeout path is a hard SIGKILL (no
  SIGTERM grace) because the foreground child has already exited and the
  only remaining group members are leaked pipe-holders we explicitly want
  gone; a graceful term would just add latency.

#### Open questions
- None.

### Commit D — tools config/error (findings 13, 14)

#### Design decisions
- **Per-lane tighten-only overrides (finding 13).** New `LaneOverrides` /
  `LaneTighten` config structs on `ToolsConfig`
  (`lane-overrides.{local,net,heavy}.{slots,default-timeout-secs,
  max-timeout-secs}`), all `Option`. A new `LaneRouter::with_config(sandbox,
  cfg)` (production path; `new` delegates with a default config) clamps each
  override with `min(default)` (slots also floor at 1) via a `tighten` free
  fn, so a target can only narrow a lane, never widen it. Threaded from the
  daemon (`LaneRouter::with_config(sandbox, &config.tools)`).
- **`ToolError::Timeout` deleted (finding 14).** The variant was never
  constructed — timeouts surface as `SpawnResult.timed_out: true` inside an
  `Ok`. Removed the variant + its `display_timeout` test; documented the
  timeout-as-output-flag contract in `crates/tools/CLAUDE.md`.

#### Deviations
- None.

#### Tradeoffs
- The `tighten` clamp lives in `router.rs` (which already imports `config`)
  rather than as a method on `LanePolicy` in `lane.rs`, keeping `lane.rs`
  free of a `config` dependency (it currently imports nothing internal).

#### Open questions
- None.

### Commit E — prompt-injection fencing (finding 9)

#### Design decisions
- **Dynamic fencing in `context/src/reviewer.rs`.** New `push_fenced` +
  `longest_backtick_run`: the evidence fence (diff and per-file contents) is
  sized to one backtick longer than the longest backtick run in the content
  (floor 3), so untrusted target content carrying its own ``` line cannot
  break out of the fence into instruction position — the forged-verdict
  vector at the reviewer's accept gate. Replaces the two fixed ```` ``` ````
  literals.
- **Untrusted-input framing in the three system prompts.** Reviewer
  (`reviewer/system.pmt`) gets a "read first" SECURITY section: all user-
  message content is untrusted data, planted "emit accept"/"review passed"
  text is itself a finding, and the verdict derives solely from the AC +
  review criteria. Implementer gets a shorter note (tool output / rejection
  reason / prior summaries are data, not commands). Director gets a
  Constraints bullet distinguishing repo-derived state strings (titles,
  blocked_reason — DATA) from the `## Operator Notes` section (authoritative
  human guidance).

#### Deviations
- The finding lists claims / rejection reasons / prior summaries / operator
  notes as candidates for dynamic fencing too. They render as plain markdown
  bullets in the `.pmt` templates, not inside code fences, so a fence-escape
  is not their vector — direct instruction injection is, which the
  system-prompt untrusted-input framing addresses. Dynamic fencing is
  applied where content genuinely sits inside a fence (the reviewer
  evidence). Operator notes are deliberately NOT labeled untrusted for the
  Director (they are the trusted operator channel).

#### Tradeoffs
- The three system-prompt edits invalidate the Anthropic prompt cache once
  (the prompts are otherwise byte-stable). A one-time cache-creation cost on
  the next call per role; negligible against the injection-hardening.

#### Open questions
- None.

### Commit F — work-scope enforcement end-to-end (finding 10)

#### Design decisions
- **Decomposer scope validation.** New `DecomposerError::InvalidFiles
  { child, path, why }` + `invalid_scope_path` helper: each child `files`
  entry is rejected at produce time if it is absolute, contains a `..`
  traversal, or uses a backslash separator. Checked in `parse_and_validate`
  after the duplicate-titles check, so it routes through the existing
  retry-with-error-in-prompt path (the model re-emits a clean scope once
  before bailing).
- **Scope rendered into both prompts.** `ImplementerUserCtx` gains
  `files: &[String]` (rendered as an "## Allowed Files (scope)" section in
  `implementer/user.pmt`, gated on non-empty); `ReviewerUserCtx` gains
  `allowed_files: &[String]` (rendered as an "### Allowed Files (scope)"
  section in `reviewer/user.pmt`). Previously `work.files` lived on the
  record but reached no prompt — agents were told to respect a list they
  could not see.
- **Ghost `resource_tags` renamed.** The field has been `files` in Rust for
  some time; the two prompts that still said `resource_tags`
  (`implementer/system.pmt` Scope Enforcement, `reviewer/system.pmt`
  criterion 4 + blocking-issue 5) now reference the "Allowed Files (scope)"
  list that actually renders.

#### Deviations
- None.

#### Tradeoffs
- `invalid_scope_path` uses `std::path::Path` component inspection rather
  than a regex, so it is OS-portable (`is_absolute`, `Component::ParentDir`)
  while still catching the backslash case explicitly (backslash is not a
  separator on Unix, so `Path` would treat `a\b` as one component).

#### Open questions
- None.

### Commit G — telemetry + worktree path/branch guards (findings 11, 12)

#### Design decisions
- **`telemetry::safe_id_segment` (finding 11).** A single `pub(crate)`
  guard in `lib.rs` (rejects empty, `/`, `\`, `..`, leading dot, NUL),
  called at the top of both fanout layers' `writer_for` before the id is
  `Path::join`ed into the sessions/work tree. An unsafe id (e.g. a
  wire-supplied `client_session_id` of `../../escape`) yields `None` — the
  layer silently skips the fanout file; the event still lands in the primary
  `events.log`, matching the layers' existing best-effort posture. One
  shared guard rather than `SessionId::parse`, because `WorkFanoutLayer`
  routes on `work_id` (a `wk-…`, not a `SessionId`), so the parse approach
  would not cover both layers uniformly.
- **`worktree::delete_branch` requires `loopr/` prefix (finding 12).** The
  pub wrapper rejects non-`loopr/` branches with the
  never-before-constructed `InvalidBranchName` before reaching
  `git branch -D`. (The internal `ops::delete_branch` is unguarded and still
  used by its own tests.)
- **`worktree::cleanup_at` requires a `.loopr/worktrees/` root (finding
  12).** New `under_worktrees_root` checks for consecutive `.loopr` →
  `worktrees` path components; a path outside it is refused with `NotFound`
  before `git worktree remove --force`. The production callers (reconcile)
  pass `list()`-derived paths already under that root.
- **Worktree lib gains a `src/tests.rs`** (the crate root had no `mod
  tests`) for the two guards, per the sibling-test-file rule.

#### Deviations
- `cleanup_at`'s rejection uses `NotFound` per the doc's explicit "wire the
  never-constructed NotFound guard" instruction, even though the path may
  exist — the semantics here is "not a loopr-managed worktree we will
  remove," and `NotFound(path)` is the closest existing variant.

#### Tradeoffs
- None.

#### Open questions
- None.

## Phase 6: Cost accounting, budgets, llm hardening

Landing across commits (mirrors Phases 1-5):
- **Commit A** — `models:` tier table + resolution (finding 4).
- **Commit B** — model pinning + cost span fields (finding 1).
- **Commit C** — `.loopr/costs.jsonl` writer in `MeteredLlmClient` (finding 2).
- **Commit D** — budgets: per-run/per-work cost cap + token accumulation (finding 3).
- **Commit E** — `RetryableReason` enum + timeout scaling (findings 5, 6).
- **Commit F** — llm minors batch (finding 7).

### Commit A — models tier table + resolution (finding 4)

#### Design decisions
- **`ModelTiers` lives in `llm/src/tier.rs`** (single-word filename) with
  `primary`/`lightweight`/`advisor` (kebab serde, `default`). `resolve`
  maps a tier name to its model and passes any other string through as a
  literal model id — the vision's "deserializer tries the table first,
  falls back to literal," expressed at resolution time (deserialization
  has no table in scope). Defaults match the workspace's current
  concrete ids (sonnet-4-6 / haiku-4-5 / opus-4-7) so an absent
  `models:` block resolves every reference to a working model and the
  digest rate table (`telemetry::digest::cost`) keeps finding rates.
- **`loopr` owns the wiring.** Top-level `Config` gains
  `#[serde(default)] models: ModelTiers`; `Config::load` calls a new
  `resolve_model_tiers` after deserialize that rewrites `llm.model` and
  `agents.director.model` to concrete ids. Resolving post-load (rather
  than at the call site) means `AnthropicClient`, the `ProcessSnapshot`
  model field, and the per-role agent configs all see concrete ids and
  never a tier name — no call-site changes in `agents`.

#### Deviations
- The vision example uses `claude-sonnet-4-7` for `primary`; the
  codebase standard is `claude-sonnet-4-6` (the rate table + existing
  `LlmConfig::default`), so the default table uses `4-6`. The `4-7` in
  the vision is illustrative, not a pin.

#### Tradeoffs
- Only `llm.model` and `agents.director.model` are resolved — the
  implementer/reviewer/decomposer call `complete_*` with `model: None`
  (use the configured default), so their tier reference IS `llm.model`.
  No separate per-role `model` field exists on `ImplementerConfig` /
  `ReviewerConfig` today; if one is added later it resolves the same way.

#### Open questions
- None.

### Commit B — model pinning + cost span fields (finding 1)

#### Design decisions
- **`Usage.model: Option<String>` is the model-pinning carrier.** The
  response's top-level `model` (concrete dated id the provider actually
  ran) is captured in `extract_usage` (set manually — it lives at the
  response root, not inside the `usage` object) and rides on `Usage`,
  which already flows to every call site and up to the Bundle producer.
  Lighter than widening every return tuple to `(payload, Usage, String)`.
  `#[serde(default)]` so it deserializes clean and stays `None` for
  stub/fake responses.
- **Cost + token span fields on every call span.** Both
  `complete_with_tool` and `complete_free` now record `input_tokens`,
  `output_tokens`, `cost_micros`, and `model_returned` via a shared
  `record_usage_span` helper. `cost_micros` reuses the digest rate table
  (`telemetry::digest::cost::cost_micros`) against the model the API
  actually ran (`usage.model`, falling back to the configured effective
  model) — span and per-process digest agree on cost. A `usage_to_record`
  helper records on BOTH success and the billed-error path (max_tokens
  truncation / schema refusal): those 200s cost tokens and must show on
  the cost span. Cache fields moved into the same helper (one place usage
  lands on the span).
- **`Bundle.model: Option<String>`.** Additive `#[serde(default)]`. The
  implementer tracks the last concrete `usage.model` across its LLM calls
  (`last_response_model`) and stamps it onto the Bundle at every
  propose/done arm (4 sites: main + corrected sub-loop). A Bundle whose
  model differs from the configured tier flags a silent provider-side
  model swap (vision pinning discipline).

#### Deviations
- None.

#### Tradeoffs
- `Usage` now carries a non-token field (`model`). The struct's doc
  comment still reads "token usage"; the semantic stretch is acceptable
  because `Usage` is the value that already travels with every call to
  every caller, so it is the natural pin carrier and avoids a tuple-shape
  ripple through ~every call site and fake.
- The implementer keeps only the LAST response model, not a per-call
  list. One Work runs one role on one configured model, so a single
  pin is the right granularity; a mid-run swap still surfaces because the
  last call's concrete id is recorded.

#### Open questions
- The `Loopr-Model` commit trailer (Phase 1) still records the
  *configured* model (`deps.llm.model()`), while `Bundle.model` now
  records the *concrete returned* model. Reconciling the trailer to the
  concrete model is possible but not required by finding 1 (which asks
  for the Bundle); left as-is to keep the trailer byte-stable.

### Commit C — costs.jsonl writer in MeteredLlmClient (finding 2)

#### Design decisions
- **Per-call context via a tokio task-local (`llm::CallContext`).** The
  metered client is process-wide, so it cannot know a call's Plan/Work/
  role from its own state. `CallContext { plan_id, work_id, role }` is a
  task-local (new `llm/src/call.rs`); the daemon's five spawn task bodies
  wrap their agent future in `CallContext::scope(ctx, fut)`
  (`spawn_implementer_for_work` → implementer, `spawn_reviewer_for_bundle`
  → reviewer, both director spawn sites → director, `decompose_and_dispatch`
  → decomposer). `CostSink::append` reads `CallContext::current()`. Chosen
  over a trait-signature context param (would ripple to every call site +
  every fake) and over span-field read-back (tracing fields are write-only
  at runtime). Added `tokio` (feature `rt`) as an llm production dep — the
  crate is the async network boundary and the CLAUDE.md already lists
  tokio among its expected deps.
- **`CostSink` in `metered.rs`.** `MeteredLlmClient::with_costs(inner,
  snapshot, Arc<CostSink>)` is the new production constructor; `new`
  (snapshot-only) is unchanged so the existing tests and any
  `.loopr`-less caller don't churn. One JSON line per call (success AND
  billed-error), the vision's exact shape (`ts`, `run_id`, `plan_id`,
  `work_id`, `role`, `model`, `input_tokens`, `output_tokens`,
  `cost_usd`). `cost_usd` derives from the shared digest rate table
  (`cost_micros / 1e6`). Best-effort: an open/write failure logs `warn!`
  and is swallowed — the cost ledger must never break the call path.
- **`run_id` is the daemon's process id**, the same `Loopr-Run`
  correlation key the Phase 1 commit trailer uses. The sink writes
  `<target>/.loopr/costs.jsonl`; `loopr costs` stays a `jq` consumer (no
  Rust CLI, per the finding).
- **`.loopr/costs.jsonl` added to the git-exclude list** (`worktree::
  excludes`). Phase 6 introduces the file, so excluding it is this
  phase's responsibility (Phase 8's exclude-list finding can't list a
  file that didn't exist when it was written).

#### Deviations
- `ts` is emitted as unix-epoch milliseconds (a number), not an RFC3339
  string. The vision shows `"ts": "..."` (a placeholder); a numeric ms
  timestamp is jq-friendlier and avoids pulling `chrono` into `llm`.

#### Tradeoffs
- `CostSink::append` opens the file per call (`OpenOptions::append`)
  rather than holding a long-lived `Mutex<File>`. LLM calls are
  seconds-apart and the line is small (well under `PIPE_BUF`, so the
  `O_APPEND` write is atomic across concurrent tasks); per-call open is
  simpler and avoids a shared file handle across the daemon's tasks.
- The stub's `Usage` has no model, so stub-backed cost lines record
  `model: "unknown"` and `cost_usd: 0.0`. That is correct (the rate
  table has no "unknown" entry) and the attribution fields still prove
  the wiring; real calls carry `usage.model` from Commit B.

#### Open questions
- None.

### Commit D — budgets: per-run/per-work cost cap + token accumulation (finding 3)

#### Design decisions
- **Two caps, homed where each is enforced.** Top-level `budgets:
  { per-run-cost-usd, per-work-cost-usd }`, both `Option<f64>` defaulting
  to `None` (unlimited) per the options-with-sane-defaults rule. The
  per-RUN cap is a daemon spawn-gate concern; the per-WORK cap is an
  implementer-loop brake. Both knobs live in the one `budgets:` section;
  `loopr` overlays `budgets.per-work-cost-usd` onto a `#[serde(skip)]`
  carrier field `ImplementerConfig.per_work_cost_cap_usd` at
  daemon-context build time, so the config surface stays single-sourced
  while the value reaches the implementer (which only ever sees its own
  `ImplementerConfig`).
- **Per-run soft pause at the spawn gates.** `DaemonContext` gains
  `per_run_cost_usd` + a one-shot `budget_event_sent` AtomicBool.
  `budget_blocks_spawn(role, id)` returns `true` when the live
  `ProcessSnapshot.llm_cost_micros` has reached the cap; on the FIRST
  breach it emits exactly one `DaemonEvent { event: "budget.exceeded",
  data: { scope, cost_usd, cap_usd } }` (the bus channel already exists;
  Phase 7 wires the rest of its senders). Wired at all three spawn sites:
  `spawn_implementer_for_work` (before the FSM advance, so the Work stays
  Pending and re-drives cheaply), `spawn_director_for_plan`, and
  `startup_reconcile_directors`. Soft pause per the vision: in-flight
  agents finish, no kill, resume requires operator action (raising/clearing
  the cap and restarting).
- **Per-Work cost cap + token accumulation in the ralph loop.**
  `run_implementer` accumulates `work_input_tokens` / `work_output_tokens`
  / `work_cost_micros` across every `complete_free` call (both the main
  loop and the corrected sub-loop — the two sites that previously
  discarded `_usage`). A `debug!("implementer: per-work usage", ...)` emits
  the running totals. `over_work_budget(cap, spent)` escalates the Work
  (`ImplementerError::EscalationNeeded`) once the accumulated cost reaches
  the per-Work cap — the implementer's natural "stop this Work" signal,
  surfacing the over-budget Work to the Director.

#### Deviations
- The vision groups both caps under one "Budgets" concept and says
  hitting EITHER cap "stops new agent spawns." The implementation splits
  enforcement: per-run does the global spawn-gate soft pause; per-Work
  escalates the offending Work rather than globally pausing (a per-Work
  overrun is a property of one Work, so escalating that Work is the honest
  signal — escalation routes it to the Director, the operator-visible
  path). The per-run gate remains the global soft-pause mechanism.
- Cost for the per-Work cap is computed from `last_response_model`
  (the concrete model from Commit B) against the shared digest rate
  table; a stub-backed run (model `None` → rate table miss → 0) never
  trips the cap, so existing stub tests are unaffected even if a cap were
  set.

#### Tradeoffs
- `budget_blocks_spawn` re-locks the snapshot mutex per spawn attempt.
  Spawns are infrequent and the lock is held for a single field read; the
  cost is negligible against an LLM call.
- A suppressed implementer spawn leaves the Work Pending, so the reactor
  may re-enter the gate on later sweeps — cheap (one mutex read, the
  `budget_event_sent` flag suppresses repeat events). This is the
  intended soft-pause shape: the daemon idles new work without busy-spam.

#### Open questions
- The spawn-gate end-to-end daemon scenario (cap set → snapshot over cap →
  spawn suppressed → one `budget.exceeded` event) and the per-Work-cap
  escalation E2E (a fake LLM returning non-zero usage with a known model)
  are daemon/agent integration tests; per the doc's structure they land in
  Phase 11 alongside the other daemon-orchestration gaps. Commit D unit-
  tests the pure pieces (`over_work_budget`, `budgets` config parse).

### Commit E — RetryableReason enum + timeout scaling (findings 5, 6)

#### Design decisions
- **`RetryableReason` enum** (`error.rs`) replaces the bare
  `Retryable { reason: String }`: `RateLimited { retry_after }`,
  `Overloaded`, `ServerError { status }`, `Network { detail }`,
  `MalformedBody { detail }`. Implements `Display` so `LlmError`'s
  `#[error("...{reason}")]` and the span `warn!(reason = %reason)` still
  render a readable message. A backoff strategy can now `match` the class
  (honor `retry-after` on a rate limit, exponential on a 5xx) instead of
  scraping a string.
- **Header capture before body consumption (`anthropic.rs`).**
  `send_request` / `send_free_request` read `retry-after` and the
  request-id header off `response.headers()` BEFORE `response.bytes()`.
  `retry_after` (parsed to whole seconds) flows into the typed
  `RateLimited`; `request_id` is logged at `debug!` ("anthropic response
  headers") for correlation — captured for diagnostics without bloating
  the enum. A shared `classify_status(code, body, retry_after)` builds the
  typed error: 401/403→Auth(fatal), 429→RateLimited, 529→Overloaded,
  408/5xx→ServerError, 400/other→BadRequest(fatal). Both classify paths
  call it; non-JSON bodies and body-read failures become `MalformedBody`;
  transport errors become `Network`.
- **Timeout scaling (finding 6).** Replaced the flat
  `REQUEST_TIMEOUT_SECS = 120` with `scaled_timeout_secs(max_tokens) =
  BASE (60) + max_tokens / 40` (a pessimistic 40 tok/s output-rate
  floor). Sonnet at 8192 max_tokens → ~265s; a 1024-token call → ~85s. A
  near-`max_tokens` response is no longer aborted mid-flight (then billed
  in full and retried into the same wall). Computed once at client
  construction from `config.max_tokens` — chose the "scale with
  max_tokens" option over a new `timeout-secs` config field to avoid
  churning every `LlmConfig { .. }` literal in the test suite (the
  config-field option is the documented alternative, deferrable).

#### Deviations
- The doc lists `request-id` capture alongside `retry-after`. `retry_after`
  is captured into the typed enum (it changes retry behavior);
  `request_id` is captured into the debug log/span rather than the enum
  (it is purely diagnostic), keeping `RetryableReason` to the five listed
  variants without a per-variant id field.

#### Tradeoffs
- `408` is bucketed as `ServerError { status: 408 }` rather than a
  distinct timeout reason — it is rare and retryable like a 5xx, so a
  dedicated variant adds no caller value.
- The timeout is per-client (sized from `config.max_tokens`), not
  per-call. All calls from one client share one `max_tokens`, so a
  per-call ceiling would be identical; the client-level timeout is where
  reqwest applies it.

#### Open questions
- None.

### Commit F — llm minors batch (finding 7)

Several items in the finding were already resolved in Commit E:
- **Duplicated status-classification arms** (the two near-identical match
  blocks) — folded into one shared `classify_status`.
- **`reqwest_err_to_llm_error` dead-branch distinction** — the
  `network`/`reqwest` split collapsed into one `RetryableReason::Network`.

#### Design decisions (remaining items)
- **Dead `domain` dep removed** from `llm/Cargo.toml` (`cargo remove`;
  no `domain::` usage anywhere in `llm/src`).
- **Empty `system` omits the field.** `build_system_block("")` returns
  `Value::Null`; both request structs mark `system` with
  `skip_serializing_if = "Value::is_null"`, so an empty system prompt no
  longer ships an invalid empty text block.
- **Error body capped before logging.** `classify_status` runs
  `truncate_preview` on the body text before embedding it in
  `Auth`/`BadRequest`, so a multi-KB error body can't flood `events.log`.
- **`extract_usage` warns on a malformed usage object.** A present-but-
  unparseable `usage` now logs `warn!` instead of silently zeroing (which
  under-counts cost without a trace).
- **`u32` truncation made saturating.** `usage.output_tokens.min(u32::MAX
  as u64) as u32` at the two `ContextExhausted.used` sites.
- **Temperature range validation** in `AnthropicClient::new`: a
  `temperature` outside `[0, 1]` returns `Fatal(ConfigInvalid)` at
  construction instead of 400ing every call.
- **Span guard no longer held across `.await`.** `complete_with_tool` /
  `complete_free` now `.instrument(span.clone())` the awaited send and
  re-enter the span only for the synchronous post-await recording; the
  same fix applied to `promote_unblocked_siblings` /
  `block_dependent_siblings` in `loopr` (the `Box::pin(async {...})`
  bodies now `.instrument(span)` instead of `let _enter` across awaits).
  The `dispatch.rs` site the finding named is already `#[instrument]`
  (fixed in an earlier phase).
- **`ScriptedLlm` usage scripting.** New `set_usage(Usage)` + an internal
  `usage()` accessor; the stub returns the scripted usage with every
  response (default still all-zero / `model: None`), enabling Phase 11
  metering tests to assert token + model flow through `MeteredLlmClient`
  and costs.jsonl.

#### Deviations
- None.

#### Tradeoffs
- The error-body cap reuses `truncate_preview` (4096-byte ceiling, the
  same bound used for prompt previews) rather than a dedicated smaller
  error-body cap — one ceiling constant is simpler and 4 KB of error body
  is already plenty for diagnosis.

#### Open questions
- None.

## Phase 7: Daemon lifecycle and visibility

Landing across six commits (mirrors Phases 1-6):
- **Commit A** — JoinSet panic visibility: panic hook + drain-loop
  JoinError logging + background pool reaper (finding 1).
- **Commit B** — spawn-gate drain race re-check + accept-loop transient-
  error resilience (findings 4, 5).
- **Commit C** — corruption gate before spawn sweeps + stale-pid
  tolerance + foreground preflight + startup-error sentinel (findings 3,
  6, 7, 11).
- **Commit D** — SIGKILL escalation window derived from the graceful-drain
  budget (finding 2).
- **Commit E** — DaemonEvent bus senders + system.status counts +
  no-fork `daemon status` + integration-branch orphan cleanup (findings
  8, 9, 10, 12).
- **Commit F** — config override chain: XDG user layer + generic
  `LOOPR_<SECTION>__<KEY>` env pass (finding 13).

### Commit A — JoinSet panic visibility (finding 1)

#### Design decisions
- **Background periodic reaper, not per-spawn reaping.** The finding
  offered "periodic/per-spawn try_join_next reaping." Per-spawn would
  touch ~20 spawn sites across four files; a single `spawn_pool_reaper`
  task (wakes every `POOL_REAP_INTERVAL_SECS = 30`, reaps all six pools,
  exits on `shutdown_notify`) is one place and follows the existing
  signal-watcher pattern. Crucially it respects the Arc-shutdown
  discipline: `serve()` joins the reaper (with the watcher timeout) BEFORE
  `Arc::try_unwrap`, so its `Arc<DaemonContext>` clone drops in time. A
  long-lived clone is exactly the hazard the `serve()` doc comment warns
  about, so the reaper is structured to release on shutdown like the
  watcher. `serve_core` (the test path) spawns no reaper.
- **`drain_pool` shared helper.** The six near-identical inline pool
  drains collapsed into one `drain_pool(tasks, timeout_secs, pool)` that
  logs non-cancelled JoinErrors on the normal-drain path and aborts the
  remainder on timeout (aborted = Cancelled, not logged). The six pool-
  specific `drain_*_tasks` fns keep their ordering doc comments and call
  `drain_pool`.
- **Panic hook chains the previous hook** and is installed in
  `run_active_daemon` right after `telemetry::init` (so the error reaches
  `events.log`, not the `/dev/null` stdio). Routes payload + location
  through `tracing::error!`.

#### Deviations
- None — finding 1 explicitly lists periodic OR per-spawn; periodic chosen
  for the reasons above.

#### Tradeoffs
- The reaper adds one always-on task and a 30s mutex-touch per pool. The
  contention is negligible (microsecond reaps) and the bound on JoinSet
  growth during a long multi-Work run is the payoff.

#### Open questions
- None.

### Commit B — spawn-gate race + accept-loop resilience (findings 4, 5)

#### Design decisions
- **Re-check `shutting_down` under the pool lock** in all six WorkSpawner
  shims (one `replace_all` — the lock+spawn opening is identical). This is
  the substantive fix for "insert into an already-drained JoinSet": the
  drain holds the same lock and `shutting_down` is set before any drain,
  so observing it true after acquiring the lock means skip.
- **Transient accept-error classification.** `is_transient_accept_error`
  matches resource-pressure errnos (EMFILE/ENFILE/ENOBUFS/ENOMEM/
  ECONNABORTED/EINTR) and the equivalent `io::ErrorKind`s; the accept arm
  logs-and-continues with a 50ms backoff on transient, propagates only a
  fatal listener error (EINVAL/EBADF).

#### Deviations
- **"track the shim handles" not implemented.** The finding's secondary
  ask (tracking the outer detached `tokio::spawn` handles) is deferred:
  the re-check-under-lock closes the actual drained-JoinSet-insert bug,
  and an outer shim that is still parked on the lock at `try_unwrap` time
  is already covered by the documented `try_unwrap` -> `Store::Drop`
  fallback (a sub-millisecond window, no crash). Tracking the shim handles
  would add a seventh pool to the carefully-ordered drain sequence for
  marginal benefit; left as future hardening.

#### Tradeoffs
- A persistent transient condition (fd exhaustion that never clears) means
  the accept loop spins at 50ms cadence logging each time, rather than
  dying. That is the intended behavior — a daemon that stays up and noisy
  beats one that dies on the first EMFILE burst.

#### Open questions
- None.

### Commit C — gate ordering, stale-pid, preflight, startup diag (findings 3, 6, 7, 11)

#### Design decisions
- **`reconcile` split into scan phase / gate / spawn phase.**
  `sweep_bundles` decomposed into `scan_bundles` (tolerant-list, surface
  corruption, return records; no spawns) and `requeue_bundles` (consume
  records, spawn). `reconcile` now: scan (sweep_worktrees + scan_bundles)
  -> mirror corruption_count onto the snapshot -> corruption gate ->
  spawn (requeue_bundles + dep promotions + Director respawn). The gate +
  snapshot-mirror moved OUT of `build_context` into `reconcile`;
  `build_context` just propagates the `CorruptionGate` error via `?`.
  `reconcile` gained an `accept_corruption: bool` param.
- **`read_pid` treats an unparseable pid as `Ok(None)` + `warn!`** rather
  than propagating, so a SIGKILL-truncated pid file no longer bricks every
  client command (`status`/`stop`/auto-fork) on a file the daemon owns.
  The caller's preflight/clean path removes the stale file.
- **Startup-error sentinel.** `daemon.startup-error` written by the
  grandchild (in `run_grandchild`, AFTER `daemon_main`'s `sentinel::clean`
  so it survives) on a failed boot. `wait_for_socket` reads it — both
  fail-fast inside the poll loop and at the timeout — so the parent
  surfaces the real reason instead of a generic socket-timeout. Added to
  `clean()`'s sweep and to `preflight_clean` (transitively) so a stale one
  never misleads a later boot.
- **Foreground preflight.** `preflight_clean` made `pub(crate)` and called
  in the `daemon start --foreground` branch (which bypasses
  `ensure_daemon`); the alive-check above already rejected a live daemon,
  so anything left is stale.

#### Deviations
- None.

#### Tradeoffs
- `scan_bundles` + `requeue_bundles` re-walk the same Bundle list the old
  single `sweep_bundles` walked once, but the list is already in memory
  (the scan returns the records the spawn phase consumes), so there is no
  double-list — only a function-boundary split.

#### Open questions
- None.

### Commit D — SIGKILL escalation window (finding 2)

#### Design decisions
- **`GRACEFUL_SHUTDOWN_BUDGET_SECS` in daemon.rs** sums the actual drain
  constants (handler + six pools + 2x watcher join), so the budget tracks
  the drains automatically. `kill_stale` derives its window as
  `budget + STOP_MARGIN_SECS (15)` and prints a progress line every 5s.
  Replaces the flat 3s window that SIGKILLed busy daemons mid-LLM-call.

#### Deviations
- None.

#### Tradeoffs
- `daemon stop` against a genuinely-wedged daemon now blocks up to ~174s
  (budget + margin) before SIGKILL, versus 3s before. That is the point:
  each pool drain has its own internal abort-on-timeout, so a healthy
  daemon exits far sooner; the long window only applies to a daemon
  ignoring SIGTERM entirely, and the 5s progress lines keep it from
  looking hung.

#### Open questions
- None.

### Commit E — DaemonEvent bus, status counts, no-fork status, branch cleanup (findings 8, 9, 10, 12)

#### Design decisions
- **DaemonEvent emission lives in `SummaryFanout`, not the
  `transition_and_persist_*` free functions.** Those two functions are
  generic over the sink (`S: WorkUpdateSink` / `PlanUpdateSink`) and are
  called at 54 sites (production + tests with fake sinks), so threading an
  event sender through them would ripple everywhere. `SummaryFanout` is
  the single production sink threaded through every production call site;
  it already does side-effects (summary writes) and sees the final
  persisted record. A `with_events` constructor wires the daemon's
  `events` broadcast sender; the test `new` constructor leaves it `None`.
  Emits `work.blocked`/`work.terminal` and `plan.stalled`/`plan.terminal`.
  This is the second documented sender on the bus (Phase 6 added
  `budget.exceeded`).
- **`daemon status` no longer auto-forks**: `DaemonCmd::Status` removed
  from the `ensure_daemon_if_needed` arm; it falls through to
  `daemon_status`, which prints "no daemon running" when none exists.
- **`system.status` counts** Active plans + non-terminal works from the
  store (degrading to 0 with a `warn!` on store-read failure);
  `handle_status` became async.
- **Integration-branch orphan cleanup**: a `plan.create` that fails at the
  Plan persist after `ensure_integration_branch` created the branch now
  best-effort `git branch -D`s it (`daemon::git::delete_integration_branch`)
  rather than reordering (which would risk an orphan Plan record instead).

#### Deviations
- The generic `DaemonEvent { event: String, data: Value }` shape (already
  in `ipc`) is reused rather than introducing the vision's typed
  `DaemonEvent::Error { plan_id, work_id, reason, message }` variant. The
  bus is string-keyed today (the `budget.exceeded` precedent); event names
  carry the semantics and `data` carries the ids. Promoting the bus to a
  typed enum is a larger `ipc`-crate change out of this finding's scope.

#### Tradeoffs
- Event emission on EVERY Work/Plan update checks the status and no-ops for
  non-terminal/non-Blocked transitions — a cheap branch on the hot path,
  cheaper than threading a sender through 54 call sites.

#### Open questions
- None.

### Commit F — config override chain (finding 13)

#### Design decisions
- **Layered load: baked-in < XDG < target < env.** XDG
  (`~/.config/loopr/loopr.yml` via a local `xdg_config_dir` helper, NOT
  `dirs::config_dir()`) and target (`.loopr/config.yml`) are parsed to
  `serde_yaml::Value` and deep-merged (`deep_merge`), then deserialized
  once through the `deny_unknown_fields` `Config`. Deep-merge is required
  because serde has no native merge and a target file that omits a key
  must not erase the XDG layer's value.
- **Generic env pass: `LOOPR_<SECTION>__<KEY>`.** Double-underscore `__`
  separates nesting levels; within a segment a single `_` becomes `-` and
  the segment lowercases. The `__` marker is REQUIRED, so `LOOPR_TARGET` /
  `LOOPR_LOG` / `LOOPR_WORKTREE_CLEANUP_POLICY` (no `__`) are not treated
  as field overrides and cannot trip `deny_unknown_fields`. Set into the
  merged Value before deserialize.
- **Test isolation via `load_guard`.** Every `Config::load` test now
  points `$XDG_CONFIG_HOME` at a private empty tempdir (restored on drop)
  so the suite never reads the developer's real
  `~/.config/loopr/loopr.yml`.

#### Deviations
- **Env var scheme changed from the vision's single-`_` sketch to
  double-underscore nesting.** The vision showed `models.primary ->
  LOOPR_MODELS_PRIMARY`, which is ambiguous once a key itself contains a
  hyphen (`per-run-cost-usd` -> `LOOPR_BUDGETS_PER_RUN_COST_USD` is
  unparseable back to a path). The `__` convention (figment/config-rs
  standard) disambiguates. vision.md amended to match (this is the `a8`
  chain-text amendment Phase 10 will reference).
- **CLI-flag layer deferred.** The vision chain ended in `< CLI flag`; no
  general CLI surface for config knobs is built (only `--log-level`, a
  telemetry concern, and the dedicated worktree-cleanup env var). vision.md
  amended to note CLI as future.

#### Tradeoffs
- `deny_unknown_fields` applies to the merged Value, so an unknown key in
  EITHER layer fails the whole load. That is the intended fail-loud
  behavior (a typo or a stale config surfaces immediately), but see the
  open question below for its interaction with a pre-existing legacy
  global config.

#### Open questions
- **A pre-existing `~/.config/loopr/loopr.yml` from loopr v3/v4 will now
  break v5 daemon startup.** The XDG layer reads that shared path; a v3/v4
  config (keys like `debug`, `agents.enabled`, `validator`) hits
  `deny_unknown_fields` and the daemon refuses to boot with an "unknown
  field" error. This is the intended fail-loud behavior, but it is a real
  operational gotcha for anyone upgrading in place. **Remedy:** remove or
  migrate the stale global config (`rkvr rmrf ~/.config/loopr/loopr.yml`),
  or populate it with v5-schema keys. Flagged for the operator; not
  worked around in code (silently tolerating unknown fields in the XDG
  layer would hide exactly the drift this chain is meant to surface).

### Commit G — XDG layer is best-effort; tests isolate XDG_CONFIG_HOME (finding 13 follow-up)

#### Design decisions
- **The shared XDG user config is best-effort, not strict.** Commit F's
  open question (a pre-existing v3/v4 `~/.config/loopr/loopr.yml` would
  brick v5 startup under `deny_unknown_fields`) is RESOLVED here, and the
  resolution surfaced immediately as 12 failing `tests/daemon.rs`
  integration tests — they fork a real daemon, which read the developer's
  real legacy global config and refused to boot. The principled fix:
  `~/.config/loopr/loopr.yml` is a single file shared by ALL loopr
  versions on the machine, so v5 cannot demand it conform to the v5
  schema. `Config::read_optional_layer` validates the XDG file standalone
  (parse + `deny_unknown_fields` deserialize into `Config`); if it fails
  for ANY reason (unreadable / unparseable / unknown key), the daemon
  WARNS and skips the XDG layer instead of aborting. The TARGET config
  (`.loopr/config.yml`, written by `loopr init` — v5-owned) stays strict:
  an unknown field there is still fatal. "Strict where you own the schema,
  tolerant where you share it."
- **Test hygiene.** The `tests/daemon.rs` and `tests/smoke.rs` binary
  helpers already isolated `XDG_DATA_HOME` to a per-test dir; they now also
  isolate `XDG_CONFIG_HOME`, so the daemon never reads the developer's real
  global config (a valid one could otherwise perturb tests). The
  `config/tests.rs` `load_guard` does the same for the in-process load
  tests.
- **`ac15` retargeted + no-fork smoke test added.** `ac15` used `daemon
  status` to trigger an auto-fork; Phase 7 finding 9 removed status from
  the auto-fork arm, so `ac15` now uses `plans` (still auto-forks) and a
  new `daemon_status_does_not_fork` asserts status leaves no pid file.

#### Deviations
- This SUPERSEDES Commit F's strict-XDG stance and the "fail-loud, don't
  silently tolerate" framing in F's notes / the vision text F wrote. The
  vision's chain prose should be read with this refinement: the env/target
  layers are strict; the XDG layer is best-effort. (A follow-up vision
  tweak to state the strict-vs-tolerant split explicitly is left for the
  Phase 10 doc-truth pass / amendment `a8`.)

#### Tradeoffs
- A user's INTENDED v5 XDG config with a single typo'd key is dropped
  WHOLESALE (with a warning naming the bad key) rather than partially
  applied. Acceptable for a shared cross-version file; the warning is the
  signal. Per-key tolerance would require deserializing into a parallel
  non-strict struct, not worth it.

#### Open questions
- None. (Scott's existing `~/.config/loopr/loopr.yml` is a v3/v4 file; v5
  now warns-and-ignores it rather than bricking — no operator action
  required, though removing the stale file would silence the boot warning.)

## Phase 8: CLI and init correctness

Landing across commits (mirrors Phases 1-7):
- **Commit A** — init + target correctness (target `-C` strict for init,
  init non-git excludes skip, init hook content-marker, worktree excludes
  3-part fix).
- **Commit B** — CLI output/enum correctness (`--output` ignore_case,
  `plan override --to` ValueEnum, `--output` rendering for the four
  hardcoded-`println!` verbs, `show` validate_kind_match).
- **Commit C** — sessions correctness (skip implicit allocation for
  `Command::Sessions`, compare-and-delete TOCTOU, `--session`/resume
  requires the manifest to exist).
- **Commit D** — CLI minors batch (connect-error in timeout message, init
  help text, `ipc_call` consolidation + config wiring, dead `AtomicU64`,
  `mod tests` placement, client-body instrumentation).

### Commit A — init + target correctness

#### Design decisions
- **Finding 1 (target `-C` re-root).** Extracted `target::canonical_start`
  (steps 1-2: pick `-C`/env/CWD start, canonicalize, dir-check) out of
  `resolve` (which still appends step-3's walk to the git toplevel /
  `.loopr` ancestor). `lib::run` enforces, ONLY for `Command::Init`, that
  the named path equals the walked root; a mismatch returns the new
  `LooprError::InitTargetMismatch { named, resolved }`. Read verbs keep the
  convenient walk. The check lives in `run` (not `target::resolve`) because
  resolve has no command context and read verbs must NOT be made strict.
- **Finding 2 (non-git excludes).** `step_ensure_git_excludes` now mirrors
  the hooks step's `.git` is-dir guard and returns `Skipped` on a non-git
  target, so init no longer fabricates `.git/info/exclude` (which a later
  run would mistake for a real repo).
- **Finding 3 (hook detection).** Replaced the filename-existence check
  (which also listed the WRONG canonical set — taskstore installs
  `post-merge`, not `post-commit`) with a CONTENT-marker check: a husky/user
  `pre-commit` no longer reads as "taskstore installed." Decision: **always
  run the idempotent installer** (`taskstore::install_hook` is itself
  content-aware — it appends its `taskstore sync` line only when absent), so
  the merge driver + `.gitattributes` always land; the pre-existing
  `taskstore sync` marker in `pre-commit` decides only the
  Created-vs-Preserved label.
- **Finding 4 (worktree excludes, 3-in-1).** (a) Read errors other than
  `NotFound` propagate (`match` on `ErrorKind`) instead of
  `unwrap_or_default`, so a present-but-unreadable exclude file is never
  clobbered. (b) Per-pattern append: membership is decided line-by-line
  against the existing file, so a list that GROWS in a later release reaches
  already-init'd targets (the old marker-gate skipped them entirely). The
  `# loopr-managed` marker is still written once for readability but no
  longer gates the whole block. (c) Pattern list aligned with reality:
  added `.loopr/active-session`, `.loopr/daemon.*` (glob covering pid /
  version / process-id / startup-error), `.loopr/prompts/`; dropped the
  stale `.loopr/runs/` and the now-subsumed `.loopr/daemon.pid`.

#### Deviations
- The doc says `plan override --to` etc. are Phase 8; this commit is only
  the init/target/excludes cluster. The CLI-enum and output-rendering
  findings land in Commit B (split for durability, per the phase pattern).

#### Tradeoffs
- `InitTargetMismatch` is a hard error rather than a re-root-with-warning.
  Chosen because writing `.loopr/`, hooks, and excludes into the wrong
  directory is silent and hard to notice; an explicit refusal naming both
  paths is the safe default. The user re-runs from the toplevel or points
  `-C` directly at it.
- Always-run-installer (finding 3) over a marker-gated skip: one extra
  idempotent git-hook write per init re-run, in exchange for the guarantee
  that the merge driver is never silently absent. The installer's own
  content-check keeps it a no-op on already-installed hooks.

#### Open questions
- None.

### Commit B — CLI output/enum correctness

#### Design decisions
- **`--output` ignore_case.** Added `ignore_case = true` to the global
  `--output` arg. `Format` is already a `ValueEnum`; `--output JSON` now
  parses as well as `--output json` (CLI rule: enum flags are
  case-insensitive).
- **`plan override --to` as a typed `ValueEnum`.** Replaced the bare
  `to: String` with a new `cli::PlanOverrideTo` enum (`Draft`/`Active`/
  `Complete`/`Stalled`, `rename_all = "lower"`, `ignore_case = true`). clap
  now rejects typos at parse time and `--to Stalled` (the cased form `show`
  displays) parses. `PlanOverrideTo::as_str()` produces the canonical
  lowercase string for the `plan.override` RPC's `target_status` param, so
  the wire contract with the daemon's `parse_plan_status` is unchanged.
  `plan_override_command` takes the enum and renders `to.as_str()` into its
  span field.
- **`show` defensive kind check (drop the `_kind` crutch).** `kind_from_prefix`'s
  result is now bound to `kind` (not `_kind`) and fed to a new
  `validate_kind_match(&RecordResult, RecordKind)` that confirms the
  daemon-returned record arm matches the prefix-implied kind, mirroring
  `list::validate_kind_match`. A mismatch is a `ClientIo` "protocol
  mismatch" error rather than silently rendering the wrong sum-type arm.

#### Deviations
- `PlanOverrideTo` intentionally omits `Abandoned`/`Superseded` even though
  `Stalled => Abandoned` is a valid FSM override edge (added in Phase 3):
  the daemon's `parse_plan_status` does not accept those strings today, so
  exposing them in the CLI enum would produce a confusing daemon-side
  rejection. Aligning the CLI enum to the RPC's accepted set is the
  faithful Phase 8 scope; widening the override RPC is a daemon-surface
  change, not a CLI concern. Noted as a known gap.
- Finding 3 of the phase (render the four hardcoded-`println!` verbs
  through `output::render`) is deferred to Commit D, where the `ipc_call`
  consolidation reshapes those verb bodies — doing the rendering there
  avoids editing the same lines twice.

#### Tradeoffs
- A typed `ValueEnum` + `as_str()` round-trip (vs. keeping `String` and
  lowercasing) costs one small enum + impl but buys parse-time rejection
  and the case-insensitivity the CLI rule requires; the daemon stays the
  single source of truth for which statuses are actually permitted.

#### Open questions
- None.

### Commit C — sessions correctness

#### Design decisions
- **Skip implicit allocation for `Command::Sessions`.** New
  `session::resolve_session_id_readonly(target, flag)` used by `lib::run`
  ONLY for the sessions verbs. It (1) validates an explicit `--session`
  without attaching the pointer, (2) reuses a live pointer, or (3) allocates
  an EPHEMERAL session — a dir-only `SessionId` under XDG `sessions/` with
  NO manifest and NO pointer claim — purely to home this process's logs.
  This kills both bugs: `sessions new` no longer creates two sessions
  (one orphaned), and `sessions end` on a pointer-less target no longer
  allocates-then-ends. The ephemeral dir is manifest-less so `list_all`
  (which skips manifest-less dirs) never shows it as a phantom session.
- **`--session`/resume requires the manifest to exist (finding 3).** New
  shared `validate_existing_session(s)` requires `manifest.yml` to exist
  (a never-allocated id is an error: "no manifest ... use `loopr sessions
  new`") in addition to the not-ended check. Used by BOTH
  `resolve_session_id` (then attaches) and `resolve_session_id_readonly`
  (does not), so `resume` (which calls `resolve_session_id`) is covered.
  New `session_manifest_exists` is distinct from `session_ended` (which
  treats a missing manifest as not-ended, the right call for its own
  racing-allocation case).
- **Compare-and-delete at both pointer-removal sites (finding 2).** New
  `remove_pointer_if_matches(pointer, expected)` re-reads the pointer and
  removes it only if the content still equals `expected` (trimmed); a
  content mismatch (a concurrent `sessions new` claimed a fresh session) or
  `NotFound` is a no-op. `PointerState::Stale` now carries the raw content
  so `resolve_session_id`'s stale-cleanup compares against what it read;
  `end_active` compares against the id it just ended. Narrows the TOCTOU
  window where a blind `remove_file` would delete a concurrent claim.

#### Deviations
- The `lib::run` selection of readonly-vs-claiming resolver is the wiring
  the finding asks for; its end-to-end test ("sessions end/new through
  `lib::run`") is a Phase 11 item (lib::run drives telemetry/fork and isn't
  unit-testable). The readonly resolver, manifest-exists rejection, and
  compare-and-delete helper are unit-tested directly here.

#### Tradeoffs
- The ephemeral telemetry session leaves a manifest-less dir under XDG
  `sessions/<id>/` for the duration of a sessions-verb invocation's logs.
  Acceptable: it is invisible to `list_all`, and the alternative (a fixed
  sentinel id) would collide across concurrent invocations. Logs for
  `loopr sessions <verb>` are diagnostic noise that needs *some* home.
- `remove_pointer_if_matches` is compare-and-delete, not a true atomic CAS
  (a residual read→remove window remains). It is the mitigation the finding
  specifies and shrinks the window from "always racy" to "racy only inside
  a few syscalls"; a hard-link/rename CAS would be heavier than the hazard
  warrants for a best-effort pointer cleanup.

#### Open questions
- None.
