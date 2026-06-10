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
