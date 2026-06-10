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
