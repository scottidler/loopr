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

In progress. This phase is the system's riskiest code (git crash
recovery); findings land incrementally so each is durable.

### Design decisions
- **`run_git` kill_on_drop (git.rs).** Set `cmd.kill_on_drop(true)` so a
  timed-out `git merge`/`checkout` is killed when the timeout drops the
  future, rather than continuing to run and landing its mutation after
  `integrate` returned `Err`. Matches the posture validation.rs already
  uses.

### Deviations
- None yet.

### Tradeoffs
- None yet.

### Open questions
- Remaining Phase 2 findings (merge-abort crash window, AdoptedExisting
  rollback, is_ancestor false-adopt, dirty-tree guard, conflict-vs-error
  classification, pinned-date determinism, branch deletion, multi-bundle
  re-entry, TOCTOU ordering, validation instrumentation, clean_fd) are
  not yet implemented.
