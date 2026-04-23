# Design Document: Stage 8 Integrator

**Author:** Claude (with Scott)
**Date:** 2026-04-22
**Status:** Implemented
**Review Passes Completed:** 5/5 self-review + Architect R1 folded (2026-04-22): batched-commit transactional boundary, crash-recovery idempotency via `git merge-base --is-ancestor`, `TicksStore::create` duplicate detection, `id_type!`-based `TickId`, accurate Phase 2 crate-deps shopping list + Architect R1b folded (2026-04-22): `--reverse` on `merge_commit_sha_for`, `tick_lock` on `TicksStore`, `DuplicateTick` carries `TickId`, wiring-retry contract promoted to Invariant + Architect R2 post-implementation audit folded (2026-04-22): Loop Contract updated to show Phase 2 prologue, empty-branch check routed on original status, `DuplicateTick` now resolved to `Ok(existing_tick)` via `TickSink::get`, `EXPECTED DIVERGENCE` test comment corrected
**Crates touched:** `domain`, `store`, `agents`, `integrator`. Scoped to the Integrator contract and the `Tick` data model, plus a one-shot relocation of `BundleUpdateSink` / `BundleUpdateError` from `agents` to `store` (see "Cross-doc reconciliation" below). Daemon wiring (how an Accepted Bundle reaches an Integrator task, how a Tick triggers follow-on Work transitions, how a structural conflict drives recovery) is out of scope and lives in the Stage 8 wiring capstone.

## Cross-doc reconciliation

Two existing artifacts conflict with this doc and must be amended as part of Phase 1, not separately:

1. **`BundleUpdateSink` relocation, `agents` -> `store`.** The Reviewer doc (`docs/design/2026-04-22-reviewer.md`) introduced `BundleUpdateSink` / `BundleUpdateError` in `crates/agents/src/reviewer.rs`. The Integrator cannot depend on `agents` - `crates/integrator/CLAUDE.md` enumerates its deps as `domain, store, worktree` and the `llm`-free invariant is Cargo-graph-mechanical (`agents` pulls `llm`). The trait also belongs in `store` on its own merits: the real impl is `impl ... for store::Store`, and the trait describes a store operation (OCC update). Phase 1 moves the trait and its real/forwarding impls to `crates/store/src/bundles.rs` (alongside `BundlesStore::update`), removes them from `crates/agents/src/reviewer.rs`, and updates imports in `agents::reviewer` accordingly. The Reviewer design doc's `Status: Implemented` tag is preserved; a "Post-Implementation Notes" bullet is appended noting the relocation.

2. **`crates/integrator/CLAUDE.md` validation language.** The crate-level CLAUDE.md currently lists in-scope: *"Validation command execution (via `tools`, the `Heavy` lane specifically) and result capture."* This doc defers validation (see Non-Goals). Phase 1 updates the CLAUDE.md to move validation to an "Earned later" list, matching the first-gate exit criterion. No other CLAUDE.md text changes.

Both amendments live inside Phase 1's commit to avoid a coexistence window.

## Summary

The Integrator is a deterministic, non-LLM stage that takes one or more Accepted Bundles and merges their implementer branches into a Plan's integration branch (`loopr/plan-<plan-id>`), producing a `Tick` record of the merge. It classifies failures into structural (path-overlap between bundles) and retryable (textual) conflicts, emits a typed `IntegrationError`, mutates each Bundle's FSM (`Accepted -> Integrating -> Merged | IntegrationFailed`) via the Reviewer-stage OCC primitive, and appends the `Tick` via a new `TicksStore`. No validation in first gate: merge-only. The slice signature `fn integrate(bundles: &[Bundle], ...)` keeps multi-Bundle Ticks earnable later; first gate asserts `bundles.len() == 1`.

Ported from v3/v4's integrator with two principled simplifications: (1) no `combine_conflicting_works` LLM-rescue flow for structural conflicts (return a typed error, daemon decides), and (2) no post-merge validation (Reviewer already validated; a green build is earned by a separate design doc when a real run shows it matters).

## Problem Statement

### Background

Stage 8 Reviewer ends with a persisted `Bundle` at `BundleStatus::Reviewed` (or `Rejected`). The daemon (Stage 8 wiring, forthcoming) transitions `Reviewed -> Accepted` via `Role::Coordinator`. At that point the change is approved but unlanded: the Implementer's commits live on `loopr/wk-<work-id>`, not on `loopr/plan-<plan-id>`. The Integrator is the final stage that makes approval durable by merging the commit range onto the Plan's integration branch and recording the merge as a `Tick`.

### Problem

v5 has no Integrator. An `Accepted` Bundle sits forever at `Accepted` with no mechanism to transition toward `Merged`. The Stage 9 first-gate exit criterion ("approved Bundle lands on the integration branch and produces a Tick record") is unreachable.

### Goals

- `fn integrate(bundles: &[Bundle], plan: &Plan, deps: &IntegratorDeps<...>) -> Result<Tick, IntegrationError>` merges each Bundle's branch into the Plan's integration branch in slice order, produces a single `Tick` record of the merge, and transitions each Bundle `Accepted -> Integrating -> Merged`.
- `domain::Tick` record: `id`, indexed `plan_id`, `integration_branch`, `integration_sha`, `bundles`, `merge_commits`, timestamps. `#[derive(Record)]` only; no FSM.
- `domain::TickId` typed ID with `tk-` prefix, following the `BundleId`/`PlanId` pattern.
- `domain::IntegrationError` enum covering every pre-flight failure, every git failure, and every persistence failure with enough structure for the daemon to route each branch of the pipeline without string-parsing error messages.
- `store::TicksStore` with `create`, `get`, `list_by_plan_id` - parallel to `BundlesStore` / `WorksStore`. `Store::ticks(&self) -> TicksStore<'_>` accessor added.
- `integrator::IntegratorConfig` with `git_timeout` and `allow_multi_bundle` (default `false`).
- Bundle FSM transitions `Accepted -> Integrating`, `Integrating -> Merged`, `Integrating -> IntegrationFailed` exercised via `Role::Integrator`.
- Mutation-on-clone with OCC: reuse the `BundleUpdateSink` / `BundleUpdateError` primitive the Reviewer stage introduced (`crates/store/src/bundles.rs::BundlesStore::update(bundle, expected_updated_at)`). No new OCC machinery.
- Intra-daemon `git_lock: Arc<tokio::sync::Mutex<()>>` on `IntegratorDeps` serializes checkout-merge-rollback sequences against the single working tree.

### Non-Goals

Structural (deferred to other docs):

- **Validation.** No `cargo check`, `cargo test`, `cargo clippy`, no user-configured project commands. A `Tick` records a successful merge only. Validation is earned via a separate design doc driven by a real run where a Reviewer-approved Bundle breaks the integration branch.
- **Integration-branch creation.** The branch `loopr/plan-<plan-id>` is a precondition. The daemon (Stage 8 wiring) creates it at Plan-creation time so the base SHA is a non-drifting snapshot of repo HEAD at Plan start. If the branch is missing at `integrate` time, the Integrator returns `IntegrationError::IntegrationBranchMissing` and the daemon decides whether to create-and-retry or block the Plan.
- **Cross-Tick conflict repair / `combine_conflicting_works`.** v3 had an LLM-rescue flow that combined two conflicting Works into a single re-implemented Work. v5 first gate returns a typed `ConflictStructural` error with file and peer-Bundle detail; the daemon decides whether to re-implement, abandon, or escalate. Re-introducing rescue is its own design doc, driven by a real run where structural conflicts become routine enough to warrant the complexity.
- **Merging the integration branch onto main or any user-owned ref.** Vision line 515 says loopr never pushes; the same spirit applies locally to user-owned refs. The user merges `loopr/plan-<plan-id>` to their branch manually when the Plan is complete.
- **Re-integrating an already-merged Bundle.** `BundleStatus::Merged` is terminal. Re-integration of a terminal Bundle returns `IntegrationError::BundleNotAccepted { current: Merged }`. (`Integrating` is explicitly NOT rejected - see "Crash-recovery idempotency" invariant - because it is a re-entry point, not a terminal state.)
- **Daemon wiring.** How an Accepted Bundle reaches an Integrator task, how a `Tick` triggers `Work` transitioning to `Done`, how a `ConflictStructural` error drives recovery - all in the Stage 8 wiring capstone.
- **Remote push.** Vision line 515. Integrator never runs `git push`.

Feature scope (deferred to a real-run motivation):

- **Multi-Bundle Ticks.** The signature takes `&[Bundle]` for forward compat; first-gate impl asserts `bundles.len() == 1` (gated by `IntegratorConfig::allow_multi_bundle = false`) and returns `IntegrationError::MultiBundleNotSupported` otherwise. Multi-Bundle becomes earned when a real run shows a Plan reliably producing co-approvable Bundle batches where the daemon would benefit from one atomic Tick instead of N.
- **Post-merge rollback after validation failure.** v3 used `git reset --hard <pre-merge-sha>` after validation. No validation in first gate means there is nothing to roll back from a successful merge. Revisit when validation lands.
- **Fast-forward-only merges.** v3/v4 used `--no-ff` unconditionally. Same here: always produce a merge commit, even when the branch is a linear descendant, so every integration has an audit-visible commit.
- **Verification of `bundle.head_commit` vs `git rev-parse <branch>` at integrate time.** No force-push flow exists in first-gate loopr, so the two should match by construction. Tracked as an Open Question.

Data-model (deferred):

- **Tick FSM.** A `Tick` is a record of fact: it exists because a merge succeeded; it never transitions. No `#[derive(Fsm)]`. If a later feature introduces partial success or supersession, the lighter path is additive Record fields (`partial_success: bool`, `superseded_by: Option<TickId>`), not a state machine.
- **`Tick` as the base for subsequent Implementer runs.** `Bundle.base_tick_id` already exists as a field on the Bundle record. The Integrator does not set it; future Implementer runs set it themselves by looking up the latest Tick for the Plan. Wiring concern.

## Proposed Solution

### Overview

Call-flow in one paragraph. The daemon (Stage 8 wiring) collects one or more `Accepted` Bundles belonging to the same Plan and hands `integrate(&[bundle], &plan, &deps)` a non-empty slice. The Integrator (a) pre-flights: asserts the slice length respects `allow_multi_bundle`, every Bundle is at `BundleStatus::Accepted`, every Bundle's `work.parent_id == plan.id`, and the integration branch `loopr/plan-<plan-id>` exists; (b) acquires `deps.git_lock` (intra-daemon Mutex) for the full checkout-merge-rollback sequence; (c) checks out the integration branch, records the pre-merge SHA; (d) for each Bundle in slice order, transitions `Accepted -> Integrating` via OCC, verifies the Bundle branch has commits beyond its merge base, runs `git merge --no-ff loopr/wk-<work-id>`, on success records the merge SHA and transitions `Integrating -> Merged`, on failure aborts the merge, resets the integration branch to the pre-merge SHA, transitions `Integrating -> IntegrationFailed`, classifies the conflict structural vs retryable, and returns the typed error; (e) builds a `Tick` with all successful merges, persists it via `TicksStore::create`, and returns `Ok(tick)`. All Bundle writes go through the Reviewer-stage `BundleUpdateSink::update(bundle, expected_updated_at)` so two parallel Integrators on the same Bundle produce exactly one winner.

### Loop contract

The loop has three phases: **pre-flight** (cheap checks, no mutation), **git sequence** (in-memory state tracking, all git ops, no store writes), and **commit** (all store writes in one batch). Store mutations are deferred until the git sequence has fully succeeded, so a mid-sequence failure leaves the store untouched and triggers a single batched `IntegrationFailed` write for every participating Bundle. This is the transactional boundary: store and git either both advance together or neither does.

```
integrate(bundles, plan, deps):
  //
  // Phase 1: Pre-flight (no mutation)
  //
  1. if bundles.is_empty()                -> Err(NoBundles)
     if bundles.len() > 1 && !allow_multi -> Err(MultiBundleNotSupported { count })

  2. // Per-Bundle precondition + plan consistency. Accept either
     //   `Accepted`  (normal path) or
     //   `Integrating` (crash-recovery: a prior integrate() call
     //                  died mid-sequence; see recovery semantics below)
     // Reject anything else up front.
     for b in bundles:
       match b.status {
         Accepted | Integrating => (),
         other => return Err(BundleNotAccepted { current: other, .. }),
       }
       work = deps.works.get(b.work_id).await?
         .ok_or(WorkNotFound)?
       if work.parent_id != plan.id -> Err(PlanBundleMismatch { .. })
       // NB: `Work::parent_id` is the direct PlanId link (see
       // crates/domain/src/work.rs:104-105). Spec/Phase are documentation
       // layers, not traversal steps.

  3. let integ_branch = format!("loopr/plan-{}", plan.id)
     if !git::verify_branch(&deps.target, &integ_branch)? -> Err(IntegrationBranchMissing)

  //
  // Phase 2: Git sequence
  //
  4. let _guard = deps.git_lock.lock().await

  // Phase 2 prologue: every Accepted Bundle is transitioned to
  // Integrating in a single OCC write BEFORE any git operation.
  // Required because the Bundle FSM has only
  //   Accepted -> Integrating by (Integrator)
  //   Integrating -> Merged by (Integrator)
  //   Integrating -> IntegrationFailed by (Integrator)
  // and no direct Accepted -> Merged / Accepted -> IntegrationFailed
  // transitions. The prologue also satisfies the crash-recovery
  // invariant: a crash between the prologue and Phase 3 leaves the
  // Bundle in Integrating on disk, which is the re-entry signal the
  // retry contract honors. `bundle_states[i]` carries the mutated
  // clone with fresh updated_at; the rest of Phase 2 and Phase 3 use
  // `bundle_states` for OCC, while routing decisions consult the
  // ORIGINAL status from `bundles[i]`.
  5. let mut bundle_states: Vec<Bundle> = Vec::new()
     for b in bundles:
       match b.status {
         Accepted    => bundle_states.push(transition_bundle(sink, b, Integrating))
         Integrating => bundle_states.push(b.clone())  // crash-recovery re-entry
         _           => unreachable!()
       }

  6. git::checkout(&deps.target, &integ_branch)?
     let pre_merge_sha = git::rev_parse_head(&deps.target)?

  7. // Per-Bundle loop. `b_original` is from the caller's input slice
     // (carries the ORIGINAL status so we can route); `b` is from
     // `bundle_states` (post-prologue, used for OCC and classify).
     let mut outcomes: Vec<(BundleId, MergeOutcome)> = Vec::new()

     for (b_original, b) in bundles.iter().zip(bundle_states.iter()):
       match b_original.status {
         Accepted => {
           // Fresh integration: branch CANNOT be already-merged
           // (nothing else writes to loopr/plan-<id>). Safe to use
           // `merge-base HEAD branch == rev-parse branch` to detect
           // empty branch.
           if let Err(e @ EmptyBranch) = git::assert_nontrivial_branch(&b.branch_name):
             return fail_all(&deps, &bundle_states, &pre_merge_sha, e).await
         }
         Integrating => {
           // Crash-recovery re-entry: the branch MAY already be
           // merged. Ancestry check first, so an already-merged
           // branch adopts cleanly instead of falsely tripping
           // EmptyBranch (a merged branch has `merge-base HEAD branch
           // == branch`).
           if git::is_ancestor(&b.head_commit, "HEAD")? {
             let sha = git::merge_commit_sha_for(&b.head_commit)?  // uses --reverse
             outcomes.push((b.id, MergeOutcome::AdoptedExisting { sha }))
             continue
           }
           // Not already merged; safe to use merge-base now.
           if let Err(e @ EmptyBranch) = git::assert_nontrivial_branch(&b.branch_name):
             return fail_all(&deps, &bundle_states, &pre_merge_sha, e).await
         }
         _ => unreachable!()
       }

       match git::merge_no_ff(&deps.target, &b.branch_name) {
         Ok(sha) => {
           outcomes.push((b.id, MergeOutcome::NewMerge { sha }))
         }
         Err(stderr) => {
           git::merge_abort(&deps.target)  // best-effort
           git::reset_hard(&deps.target, &pre_merge_sha)?
           let kind = classify_conflict(b, &bundle_states)
           let err = match kind {
             Structural { files, peers } => ConflictStructural { .. },
             Retryable                   => ConflictRetryable { stderr, .. },
           }
           return fail_all_without_reset(&deps, &bundle_states, err).await
         }
       }

  8. let integration_sha = git::rev_parse_head(&deps.target)?

  //
  // Phase 3: Commit (batched store writes)
  //
  // Git has fully succeeded. Now and only now do we write to the store.
  //
  9. // Persist the Tick FIRST. A pre-existing Tick on retry
     // (crash-recovery case (a)) returns DuplicateTick; resolve the
     // existing Tick via ticks.get(tick_id), then continue to the
     // Merged transitions. Must not bubble DuplicateTick - the
     // crash-recovery invariant mandates Ok(existing_tick).
     let tick = Tick::new(
       plan.id, bundle_ids(bundles), integ_branch, integration_sha,
       outcomes.iter().map(|(_, o)| o.sha()).collect(),
     )
     let tick = match deps.ticks.create(tick).await {
       Ok(t) => t,
       Err(DuplicateTick { tick_id, .. }) => {
         deps.ticks.get(&tick_id).await?
           .ok_or(RecordNotFound { collection: "ticks", id: tick_id })?
       }
       Err(other) => return Err(other.into()),
     }

     // Transition every Bundle that was merged (new or adopted) to Merged.
     // Source from `bundle_states` so OCC `expected_updated_at` matches
     // the on-disk state (post-prologue).
     for (bundle_id, _) in &outcomes:
       let b = lookup_in_slice(&bundle_states, bundle_id)
       transition_bundle(&deps.bundle_sink, b, Merged, Role::Integrator).await?

  10. Ok(tick)
```

Helpers:

```
transition_bundle(sink, bundle: &Bundle, target: BundleStatus, role: Role) -> Result<(), IntegrationError>:
  let mut clone = bundle.clone();
  let expected = clone.updated_at;
  clone.transition(target, role).map_err(|e| Transition(e.to_string()))?;
  sink.update(clone, expected).await.map_err(Into::into)


// Called only from the git-sequence phase on any failure. Rolls git
// back to the snapshot, then transitions every participating Bundle
// (regardless of whether it got as far as being merged in this call)
// to IntegrationFailed in one batch. Preserves the invariant that
// store and git agree: git is back at pre_merge_sha, every Bundle in
// the slice is IntegrationFailed.
fail_all(deps, bundles, outcomes, pre_merge_sha, err) -> Result<Infallible, IntegrationError>:
  git::reset_hard(&deps.target, pre_merge_sha)?    // fatal on failure
  for b in bundles:
    // skip Bundles already terminal (e.g., the pre-flight check
    // accepted them as Integrating from a prior call, and this
    // call never wrote them). Idempotent transition: if a Bundle
    // is already IntegrationFailed, transition returns Unchanged.
    let _ = transition_bundle(&deps.bundle_sink, b, IntegrationFailed, Role::Integrator).await;
  Err(err)


enum MergeOutcome {
  NewMerge { sha: String },       // this call's git merge produced this SHA
  AdoptedExisting { sha: String }, // a prior crashed call had already merged; adopted
}
```

Why `transition_bundle` no longer returns the mutated clone: the Phase 3 commit path reads the Bundle freshly from the slice for each transition, so chained OCC-version carry-forward is unnecessary. The Phase 3 writes touch each Bundle once (`Merged` on success, `IntegrationFailed` on failure via `fail_all`), so a stale `updated_at` on an `Accepted` / `Integrating` Bundle is still the correct OCC expected-version going into `Merged`.

### Invariants

- **Integration branch existence is a precondition, not an affordance.** `integrate` never creates `loopr/plan-<plan-id>`. Creating the branch requires knowing the base SHA (HEAD at Plan start), which only the daemon has the context to snapshot deterministically. If the branch is missing, a typed error escapes; the daemon decides whether to create-and-retry, block, or escalate.
- **Bundle status precondition is `Accepted`.** The Bundle FSM has `Accepted => Integrating by (Integrator)` only. `integrate` checks this up front and returns `IntegrationError::BundleNotAccepted { current }` before any git operation. Defense-in-depth: the later `transition` call would also reject, but pre-flight gives a cleaner error.
- **Plan-Bundle consistency is checked up front.** For each Bundle, the Integrator reads the Work via `WorkLookup` and verifies `work.parent_id == plan.id` - `Work::parent_id` is the direct `PlanId` link (Spec/Phase are documentation layers, not traversal steps; see `crates/domain/src/work.rs:104-105`). A wiring bug that paired a Bundle with the wrong Plan would otherwise attempt a merge onto the wrong integration branch. Early assertion, typed `PlanBundleMismatch`, no git work wasted.
- **Mutation is on a clone, with OCC.** `BundleUpdateSink::update(bundle, expected_updated_at)` is the Reviewer-stage OCC primitive, unchanged. Two parallel Integrators racing on the same Bundle produce exactly one winner; the loser gets `BundleUpdateError::Stale`.
- **Intra-daemon `git_lock` prevents working-tree collisions.** A single target has one working tree; two Integrators running concurrent `git checkout` + `git merge` on it is a corruption hazard. The `Arc<tokio::sync::Mutex<()>>` on `IntegratorDeps` serializes them. First-gate concurrency is bounded to one active Plan; the lock is rarely contended. Multi-Plan concurrency via per-branch worktrees is Alternative 6.
- **Failure state is recoverable.** On merge failure the Integrator (a) aborts any half-merge (`git merge --abort`, best-effort; ignored on error because the merge may not have reached conflict state), (b) resets the integration branch to `pre_merge_sha` via `git reset --hard`, (c) transitions **every participating Bundle** in the slice to `IntegrationFailed` via `fail_all`. After rollback, the working tree and ref are identical to their pre-call state and every Bundle in the slice is a single consistent terminal. The `pre_merge_sha` reset must succeed; a failure at that step is a fatal `IntegrationError::Git` whose recovery is the daemon's worktree-crash-recovery pass at restart.

- **Store-git atomicity: batched commit phase.** Store writes are deferred until the git sequence has fully succeeded. The loop's Phase 2 produces an in-memory `outcomes: Vec<(BundleId, MergeOutcome)>`; Phase 3 writes the Tick first, then transitions every merged Bundle to `Merged`. A git failure mid-sequence never leaves a Bundle in `Merged` with its commits rolled out of the branch, because no Bundle is written to `Merged` until *all* merges have landed. This closes the multi-Bundle FSM-git divergence that the first-gate `allow_multi_bundle = false` guard had only suppressed culturally.

- **Crash-recovery idempotency: `Integrating` is a re-entry point.** The pre-flight accepts Bundles in `Accepted` (normal path) *or* `Integrating` (crash-recovery path). A Bundle in `Integrating` means a prior `integrate` call died after transitioning it but before Phase 3 completed; that state has exactly three resolutions, and the Integrator owns distinguishing them because no one else can:
  - (a) *Merge landed, Tick landed, `Merged` write never happened.* The daemon's retry calls `integrate` with the Bundle still `Integrating`. The idempotency check (`git merge-base --is-ancestor <head_commit> HEAD`) returns true, the `outcome` is `AdoptedExisting`, and Phase 3's `deps.ticks.create` fails with a uniqueness error *or* (if the prior Tick was already written) short-circuits. Either way, Phase 3 transitions the Bundle to `Merged` and the call succeeds. **TODO:** `TicksStore::create` on a duplicate Plan/bundle-set must return a specific `StoreError` variant the Integrator can match on to promote to a no-op rather than a failure. Captured as a Phase 1 requirement on `TicksStore` in the Implementation Plan.
  - (b) *Merge landed, Tick never landed.* Idempotency check returns true, outcome is `AdoptedExisting`, Phase 3 writes the Tick, then `Merged`. Succeeds.
  - (c) *Merge never landed (crash between `Accepted -> Integrating` write and the actual git merge).* Idempotency check returns false. Normal merge path executes. Succeeds (or fails with a typed error, same as a fresh call).

  This is the explicit answer to the Architect's "hardest question" about `Integrating` stranding. Recovery lives in the deterministic stage, not in the daemon. The daemon's only job at restart is to re-enqueue any `Integrating` Bundle for another `integrate` call.

- **Idempotency check uses `git merge-base --is-ancestor`, not Tick-lookup.** The authoritative signal that a merge landed is the git graph, not the taskstore. `git merge-base --is-ancestor <bundle.head_commit> HEAD` on the integration branch returns exit 0 when the Bundle's head is an ancestor of the current HEAD (i.e., the merge commit created by the prior call absorbed it). Looking up "does a Tick exist for this Plan with this Bundle in it?" is a useful secondary signal but not authoritative: the Tick could have been written before the `Merged` transition (Phase 3 ordering) and yet a later sweep could still be recovering. Git first, store second.

- **Wiring retry contract for `Integrating`** (load-bearing on Stage 8 capstone). The Integrator's crash-recovery correctness depends on the daemon *continually* re-enqueueing any Bundle observed in `Integrating` status for another `integrate` call, not just once at restart. Scenarios that leave a Bundle in `Integrating` across an `integrate` return:

  - Daemon crash mid-sequence (Phase 2 or Phase 3). Restart sweep picks this up.
  - Non-fatal `integrate` returns: `IntegrationError::Update(Stale)` during Phase 3's `Merged` write (another writer advanced `updated_at` between the pre-flight read and the Phase 3 write); `IntegrationError::Store(...)` on Tick persistence for any transient storage error; any future variant that represents "retry makes sense."

  In both cases the git branch may already be advanced, a Tick may already be persisted, and the Bundle is stranded at `Integrating` until someone calls `integrate` again. The Stage 8 wiring capstone must honor this contract: "a Bundle at `Integrating` after an `integrate` return is not a terminal failure; re-enqueue it." The Integrator itself is idempotent on re-entry (the three recovery cases in "Crash-recovery idempotency" cover every branch), so the daemon's retry policy is "retry until success, a typed terminal error, or a circuit-breaker cap."

  Corresponding terminal errors (do NOT retry): `BundleNotAccepted { current: Merged | Rejected | Superseded | IntegrationFailed }` (Bundle already in a terminal), `PlanBundleMismatch` (wiring bug, escalate), `IntegrationBranchMissing` (precondition violation, escalate), `ConflictStructural` / `ConflictRetryable` (failure outcome is recorded on the Bundle as `IntegrationFailed`; retry is the daemon's recovery policy, not Integrator's).

  This contract is called out here as an Invariant rather than an Open Question because the Integrator's claim to recoverability is not self-contained; if the wiring ignores this contract, the stranding that Finding 4 of Architect R1b described re-emerges.
- **No fast-forward.** Always `git merge --no-ff -m "Merge bundle branch <name>"`. Produces an audit-visible commit for every integration, even when the Bundle branch is a linear descendant. Matches v3/v4.
- **Empty-branch detection before merge.** `git merge-base HEAD <bundle_branch>` equals `git rev-parse <bundle_branch>` means the branch has no commits beyond the merge base; `git merge --no-ff` would exit 0 ("Already up to date") with no merge commit. `IntegrationError::EmptyBranch` catches this before the merge is attempted. v3 precedent (`loopr/src/agents/integrator.rs:1647-1669`).
- **Multi-Bundle Ticks gated behind config.** `IntegratorConfig::allow_multi_bundle` defaults to `false` in first gate. With one Bundle per Tick, the "rollback leaves already-merged Bundles in `Merged` but out-of-branch" divergence (described in Alternative 2) cannot occur. The guard makes this invariant mechanical, not cultural.
- **The Tick records one merge commit per Bundle.** `tick.merge_commits[i]` is the SHA of the merge produced for `tick.bundles[i]`. First gate: both Vecs have length 1. Multi-Bundle earned later.
- **No LLM calls.** Mechanically enforced: `crates/integrator/Cargo.toml` has no `llm` dep. This doc does not add one.

### Data Model

#### `Tick` (`crates/domain/src/tick.rs`) - the other headline contract

New file. Pure `Record` (no FSM). Persisted at `<target>/.loopr/taskstore/ticks.jsonl`.

```rust
use serde::{Deserialize, Serialize};

use derive::Record;

use crate::id::{BundleId, PlanId, TickId, now_millis};

/// A Tick is a record of a successful integration: one or more
/// Accepted Bundles merged into a Plan's integration branch. Born in
/// its final state; no FSM.
#[derive(Debug, Clone, Serialize, Deserialize, Record)]
#[serde(deny_unknown_fields)]
pub struct Tick {
    pub id: TickId,
    #[record(indexed)]
    pub plan_id: PlanId,
    pub updated_at: i64,
    pub created_at: i64,
    pub integration_branch: String,
    pub integration_sha: String,
    pub bundles: Vec<BundleId>,
    pub merge_commits: Vec<String>,
}

impl Tick {
    pub fn new(
        plan_id: PlanId,
        bundles: Vec<BundleId>,
        integration_branch: String,
        integration_sha: String,
        merge_commits: Vec<String>,
    ) -> Self {
        let now = now_millis();
        Self {
            id: TickId::new(),
            plan_id,
            updated_at: now,
            created_at: now,
            integration_branch,
            integration_sha,
            bundles,
            merge_commits,
        }
    }
}
```

`TickId` is a new typed ID added to `crates/domain/src/id.rs` via the existing `id_type!` macro: `id_type!(TickId, "tk");`. Follows the same shape as `PlanId` / `WorkId` / `BundleId` (`crates/domain/src/id.rs:120-122`): a `String`-backed newtype with a 5-char base36 suffix (`generate_id` at `id.rs:21`), `FromStr`, `Display`, `Serialize`, `Deserialize`, `AsRef<str>`. No ULID; the domain crate does not use ULIDs.

Why `Tick` is not an FSM: every `Tick` is born in its terminal state because integration is synchronous and atomic from the caller's perspective. There is no "pending Tick" that could exist on disk without its merge having already landed. If multi-Bundle later introduces partial success or supersession, a `partial_success: bool` or `superseded_by: Option<TickId>` field is lighter than an FSM. The Reviewer stage established the precedent (`Verdict` is not a `Record`, `Bundle.verification` is the only artifact); Tick carries it forward (`Tick` is a `Record`, but the state-machine weight belongs on `Bundle`).

Why `integration_branch: String` when it is derivable from `plan_id`: saves a `PlansStore` join for audit queries (`list_by_plan_id` + show the branch in one scan), and future-proofs against a rename of the `loopr/plan-<plan-id>` convention. Cost is ~30 bytes per Tick.

Why `merge_commits: Vec<String>` when the `integration_sha` plus `git log` would yield them: git history is mutable (GC, rebase, force-push by a human); the Vec is a durable record independent of the repo's current state. Preserves a stable 1:1 index mapping: `bundles[i]` was merged at `merge_commits[i]`.

#### `TicksStore` (`crates/store/src/ticks.rs`)

New submodule parallel to `BundlesStore` / `WorksStore`. Append-only; no `update` method. Carries an intra-daemon `tick_lock` to serialize the duplicate-detection read-check-write inside `create` (see below).

```rust
use tokio::sync::Mutex;

pub struct TicksStore<'a> {
    inner: &'a AsyncStore,
    /// Serializes the duplicate-detection read-check-write inside `create`.
    /// Without it, two concurrent `integrate` calls that both perform the
    /// crash-recovery idempotency dance would both pass `list_by_plan_id`
    /// returning empty and both append, producing two Ticks for one merge.
    /// Same shape as `BundlesStore::update_lock` introduced in the Reviewer
    /// stage.
    tick_lock: Mutex<()>,
}

impl<'a> TicksStore<'a> {
    pub async fn create(&self, tick: &Tick) -> Result<Tick, StoreError> {
        let _guard = self.tick_lock.lock().await;
        // 1. Check duplicates inside the lock.
        let existing = self.list_by_plan_id_inner(&tick.plan_id).await?;
        let incoming_bundles: BTreeSet<_> = tick.bundles.iter().cloned().collect();
        if let Some(dup) = existing.iter().find(|t| {
            let t_bundles: BTreeSet<_> = t.bundles.iter().cloned().collect();
            t_bundles == incoming_bundles
        }) {
            return Err(StoreError::DuplicateTick {
                tick_id: dup.id.clone(),
                plan_id: tick.plan_id.clone(),
                bundles: tick.bundles.clone(),
            });
        }
        // 2. Append.
        self.inner.create(tick).await?;
        Ok(tick.clone())
    }
    pub async fn get(&self, id: &TickId) -> Result<Option<Tick>, StoreError> { /* indexed lookup */ }
    pub async fn list_by_plan_id(&self, plan_id: &PlanId) -> Result<Vec<Tick>, StoreError> { /* scan index */ }
}
```

`Store::ticks(&self) -> TicksStore<'_>` accessor added to `store/src/store.rs`. No OCC on `create` per se (TickIds are fresh on construction, Ticks are not mutated), but `create` IS serialized against its own duplicate-detection window via `tick_lock`. If multi-Bundle introduces Tick revisions, that is the trigger to add `update` with OCC.

**Why `create` takes `&Tick` but returns `Tick`:** the caller constructs a fresh `Tick` with a fresh `TickId`. On a duplicate-detection hit, the caller needs the pre-existing Tick's identity (the `TickId` on the `StoreError::DuplicateTick` variant, one `TicksStore::get` away) or may prefer to call `list_by_plan_id` again. On success, the caller gets back the Tick it passed in (cloned for ownership clarity). The shape is "create and return on success, return duplicate identity in the error on collision."

#### `IntegrationError`

```rust
#[derive(Debug, thiserror::Error)]
pub enum IntegrationError {
    #[error("no bundles supplied")]
    NoBundles,

    #[error("multi-bundle ticks not supported in first gate: received {count} bundles")]
    MultiBundleNotSupported { count: usize },

    #[error("bundle {bundle_id} is not Accepted (current: {current:?})")]
    BundleNotAccepted { bundle_id: String, current: BundleStatus },

    #[error("work {work_id} not found for bundle {bundle_id}")]
    WorkNotFound { bundle_id: String, work_id: String },

    #[error("bundle {bundle_id} belongs to plan {work_plan_id}, not plan {plan_id}")]
    PlanBundleMismatch { bundle_id: String, work_plan_id: String, plan_id: String },

    #[error("integration branch {branch} does not exist")]
    IntegrationBranchMissing { branch: String },

    #[error("bundle branch {branch} (bundle {bundle_id}) has no commits beyond merge base")]
    EmptyBranch { bundle_id: String, branch: String },

    #[error("structural merge conflict for bundle {bundle_id}: paths {files:?} overlap with peer bundles {peer_bundle_ids:?}")]
    ConflictStructural {
        bundle_id: String,
        files: Vec<String>,
        peer_bundle_ids: Vec<String>,
    },

    #[error("retryable merge conflict for bundle {bundle_id} on branch {branch}: {stderr}")]
    ConflictRetryable {
        bundle_id: String,
        branch: String,
        stderr: String,
    },

    #[error("git operation failed: {0}")]
    Git(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("store error: {0}")]
    Store(#[from] StoreError),

    #[error("bundle update failed: {0}")]
    Update(#[from] BundleUpdateError),

    #[error("fsm transition rejected: {0}")]
    Transition(String),
}
```

`BundleUpdateError::Stale` from the Reviewer stage bubbles via `IntegrationError::Update`. The daemon matches it specifically to distinguish "concurrent writer won the race" from "integrator should escalate."

#### `IntegratorConfig` (`crates/integrator/src/config.rs`)

```rust
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct IntegratorConfig {
    /// Maximum wall-clock duration for any single git subprocess. Default 60s.
    pub git_timeout: Duration,
    /// First-gate guardrail. When `false`, `integrate` rejects any call
    /// with more than one Bundle. Default `false`; flipped when
    /// multi-Bundle Ticks are designed.
    pub allow_multi_bundle: bool,
}

impl Default for IntegratorConfig {
    fn default() -> Self {
        Self {
            git_timeout: Duration::from_secs(60),
            allow_multi_bundle: false,
        }
    }
}
```

### API

```rust
// crates/integrator/src/lib.rs

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use domain::{Bundle, BundleStatus, Plan, Role, Tick, Work};
use store::{BundleUpdateError, BundleUpdateSink, StoreError};  // relocated from `agents` in Phase 1
use tokio::sync::Mutex;

pub trait WorkLookup: Send + Sync {
    fn get<'a>(&'a self, work_id: &'a str)
        -> impl Future<Output = Result<Option<Work>, StoreError>> + Send + 'a;
}

pub trait TickSink: Send + Sync {
    fn create<'a>(&'a self, tick: &'a Tick)
        -> impl Future<Output = Result<(), StoreError>> + Send + 'a;
}

// Real impls live with `Store`:
impl WorkLookup for store::Store { /* delegates to WorksStore::get */ }
impl TickSink  for store::Store { /* delegates to TicksStore::create */ }
impl<T: WorkLookup + ?Sized> WorkLookup for &T { /* forwarding */ }
impl<T: TickSink    + ?Sized> TickSink    for &T { /* forwarding */ }

pub struct IntegratorDeps<U, W, T> {
    pub bundle_sink: U,                  // BundleUpdateSink (OCC update)
    pub works: W,                        // WorkLookup (read-only fetch for plan-id check)
    pub ticks: T,                        // TickSink (append-only)
    pub config: IntegratorConfig,
    pub target: PathBuf,                 // target repo root
    pub git_lock: Arc<Mutex<()>>,        // intra-daemon working-tree serialization
}

pub async fn integrate<U, W, T>(
    bundles: &[Bundle],
    plan: &Plan,
    deps: &IntegratorDeps<U, W, T>,
) -> Result<Tick, IntegrationError>
where
    U: BundleUpdateSink,
    W: WorkLookup,
    T: TickSink,
{
    // body per Loop Contract
}
```

Signature decisions:

- `&[Bundle] + &Plan`: the Plan carries the integration-branch precondition. The daemon has the Plan in hand post-decompose; passing by reference avoids forcing the Integrator to fetch it. Vision line 179's informal `integrate(bundles: &[Bundle])` is upgraded to `(bundles, plan, deps)` the same way Reviewer upgraded `run_reviewer(bundle, deps)` to `(bundle, work, deps)`.
- `IntegratorDeps<U, W, T>`, not `Deps<L, T, S, C>`: no LLM, no `ContextBuilder`, no `ToolExecutor`. Three traits is the minimum: Bundle OCC update, Work lookup (for the plan-id check), Tick append.
- `git_lock: Arc<Mutex<()>>` on `Deps`, not per-call: the same lock serializes all Integrator tasks in the daemon. Sharing happens at `Deps` construction.
- Return `Tick`, not `Vec<Bundle>`: Bundles are mutated on clones and persisted via `bundle_sink`. The caller (daemon) uses the Tick for downstream routing (transition each bundle's Work to `Done`); it can re-read a Bundle if it needs the mutated form.

### Bundle FSM - transitions consumed

Per `crates/domain/src/bundle.rs:42-58` (already in the tree):

```
Accepted    => Integrating        by (Integrator)
Integrating => Merged             by (Integrator)
Integrating => IntegrationFailed  by (Integrator)
```

This doc exercises all three via `Role::Integrator`. No FSM changes required.

## Implementation Plan

### Phase 1: `Tick` record + `TickId` + `TicksStore` + cross-doc reconciliation
**Model:** sonnet

Domain + store:

- `crates/domain/src/id.rs`: add `id_type!(TickId, "tk");` on a line below the existing `id_type!(BundleId, "bd");`. No new macro machinery; the existing `id_type!` stamps out `String`-backed newtype, `FromStr`, `Display`, `Serialize`, `Deserialize`, `AsRef<str>` with the 5-char base36 suffix.
- `crates/domain/src/tick.rs`: new file with the `Tick` Record above. No FSM, no `#[derive(Fsm)]`.
- `crates/domain/src/lib.rs`: re-export `Tick` and `TickId`.
- `crates/store/src/ticks.rs`: new submodule with `TicksStore { create, get, list_by_plan_id }` and an intra-daemon `tick_lock: tokio::sync::Mutex<()>` field (see Data Model). Append-only, no OCC, no `update`. **`create` must detect duplicate-Tick writes and return a dedicated `StoreError::DuplicateTick { tick_id: TickId, plan_id: PlanId, bundles: Vec<BundleId> }` variant** when a Tick with the same `(plan_id, bundles-as-set)` already exists. The variant carries `tick_id` so the Integrator's crash-recovery path can resolve the existing Tick with one `TicksStore::get(tick_id)` rather than a second `list_by_plan_id` scan. This is the signal the Integrator's crash-recovery path uses to short-circuit Phase 3 (see "Crash-recovery idempotency" invariant). Implementation: take `tick_lock`, compare incoming `(plan_id, sorted bundles)` against `TicksStore::list_by_plan_id(plan_id)`; if any existing Tick's bundles-as-set matches, return `DuplicateTick { tick_id, .. }` without appending. The comparison is on the **set** of bundles, not the ordered Vec, so a rewrite that preserves membership but reorders does not create a false non-duplicate.
- `crates/store/src/store.rs`: add `Store::ticks(&self) -> TicksStore<'_>` accessor.

Cross-doc reconciliation (see "Cross-doc reconciliation" section above):

- **Relocate `BundleUpdateSink` + `BundleUpdateError` from `agents` to `store`.** Move the trait, the `impl BundleUpdateSink for store::Store`, the forwarding `impl<B> BundleUpdateSink for &B`, and the `BundleUpdateError` enum from `crates/agents/src/reviewer.rs` into `crates/store/src/bundles.rs` (co-located with `BundlesStore::update`, since that is what the trait wraps). Update `crates/agents/src/reviewer.rs` to `use store::{BundleUpdateSink, BundleUpdateError}`. Update `crates/agents/src/lib.rs` re-exports to remove them (they are now `store::...` everywhere). Expect a small wave of `use` fixups across any tests that imported from `agents`.
- **Amend `crates/integrator/CLAUDE.md`.** Move the validation bullet from "In scope" to an "Earned later" list with a cross-reference to this doc.
- **Append a Post-Implementation Notes bullet to `docs/design/2026-04-22-reviewer.md`** recording the relocation - the Reviewer stays marked `Status: Implemented` because the trait is still available at the same two-argument signature, just under a different module path.

Tests (`crates/domain/src/tick/tests.rs`, following the tests-in-own-files rule):

- Serde round-trip with `merge_commits` length 0 and length 3.
- `TickId` round-trips via `to_string` / `FromStr`.
- `#[record(indexed)]` on `plan_id` produces the same key spelling for index lookup and on-disk JSON (mirrors the BundleStatus lowercase-serialization check in `bundle.rs` tests).

Tests (`crates/store/src/ticks/tests.rs`):

- `create` + `get` round-trip.
- `list_by_plan_id` returns matching and excludes others.
- `list_by_plan_id` returns empty Vec for a plan with no Ticks.

Tests (relocation verification):

- `cargo test -p agents` continues to pass after the `BundleUpdateSink` relocation (the Reviewer still works at the store import path).
- `cargo test -p store` picks up the existing Reviewer-stage OCC tests now hosted in `store/src/bundles/tests.rs`.

### Phase 2: `IntegrationError` + `IntegratorConfig` + DI traits + crate deps
**Model:** sonnet

- `crates/integrator/Cargo.toml`: `cargo add` the required runtime deps. `integrator` currently has no deps; none can be assumed. Add: `tokio` with features `["process", "sync"]`, `thiserror`, `domain` (workspace), `store` (workspace), `worktree` (workspace). Do NOT add `llm`; verify via `cargo tree -p integrator -i llm` returns "not found" (this check also becomes an Acceptance Criterion).
- `crates/integrator/src/error.rs`: full `IntegrationError` enum per Data Model.
- `crates/integrator/src/config.rs`: `IntegratorConfig` with defaults.
- `crates/integrator/src/lib.rs`: define `WorkLookup` and `TickSink` traits; define `IntegratorDeps<U, W, T>` struct. Re-export error and config.
- `crates/integrator/src/lib.rs`: blanket `impl WorkLookup for store::Store` (delegates to `WorksStore::get`) and `impl TickSink for store::Store` (delegates to `TicksStore::create`), plus forwarding impls for `&T`.
- Tests (compile-level):
  - `IntegratorDeps` smoke: a struct instance can be constructed from real `Store` and trivially-typed fakes.
  - `IntegrationError::Display` produces expected prose for each variant.
  - `IntegratorConfig::default()` matches documented defaults.

### Phase 3: Core `integrate` loop
**Model:** opus

Non-mechanical: git subprocess orchestration, pre-merge snapshot, rollback sequencing, conflict classification, OCC-on-clone state mutations with version carry-forward. Judgment work.

- `crates/integrator/src/git.rs`: private helpers (each wraps `tokio::process::Command`, respects `IntegratorConfig::git_timeout`, returns typed `IntegrationError` on non-zero exit).
  - `verify_branch(target, branch) -> Result<bool, IntegrationError>` via `git rev-parse --verify <branch>`.
  - `checkout(target, branch) -> Result<(), IntegrationError>` via `git checkout <branch>`.
  - `rev_parse_head(target) -> Result<String, IntegrationError>` via `git rev-parse HEAD`.
  - `assert_nontrivial_branch(target, bundle_branch) -> Result<(), IntegrationError>` via `git merge-base HEAD <branch>` + `git rev-parse <branch>`; returns `EmptyBranch` when the two SHAs match.
  - `is_ancestor(target, commit, ref_name) -> Result<bool, IntegrationError>` via `git merge-base --is-ancestor <commit> <ref_name>` (exit 0 = true, exit 1 = false, any other exit = `IntegrationError::Git`). Used by the crash-recovery idempotency check on `Integrating` Bundles.
  - `merge_commit_sha_for(target, bundle_head) -> Result<String, IntegrationError>` via `git log --merges --format=%H --ancestry-path --reverse <bundle_head>..HEAD | head -n1`; returns the chronologically **first** merge commit in the ancestry path from `bundle_head` to `HEAD`, i.e., the commit that actually absorbed `bundle_head`. The `--reverse` is load-bearing: git's default order is reverse-chronological, and piping that to `head -n1` would yield the newest merge (whichever Bundle was integrated most recently), not the merge that absorbed the requested `bundle_head`. Used only on the `AdoptedExisting` crash-recovery path.
  - `merge_no_ff(target, branch) -> Result<String, String>` via `git merge --no-ff <branch> -m "Merge bundle branch <branch>"`. On non-zero exit, returns `Err(stderr)`; does **not** abort inside the helper (caller aborts).
  - `merge_abort(target) -> ()` via `git merge --abort`; best-effort, errors ignored.
  - `reset_hard(target, sha) -> Result<(), IntegrationError>` via `git reset --hard <sha>`; a failure here is fatal.
- `crates/integrator/src/classify.rs`:
  - `classify_conflict(failing: &Bundle, peers: &[Bundle]) -> ConflictKind` iterates `failing.paths` vs each peer's `paths`. Returns `Structural { files, peer_bundle_ids }` when any path appears in both `failing` and any peer's path list, otherwise `Retryable`. v3 precedent (`loopr/src/agents/integrator.rs:1602`).
  - `enum ConflictKind { Structural { files: Vec<String>, peer_bundle_ids: Vec<String> }, Retryable }`.
- `crates/integrator/src/lib.rs`: full `integrate` body per Loop Contract. The `transition_bundle` helper is private; callers hand `&Bundle` and receive the mutated clone with fresh `updated_at`.
- `crates/integrator/src/lib.rs`: the `rollback(deps, pre_merge_sha)` helper encapsulates `merge_abort` + `reset_hard`; fatal-on-`reset_hard`-failure.
- Tests (`crates/integrator/src/tests.rs`, using a `FakeGit` harness that canonicalizes sequences of subprocess invocations to canned outputs, plus a `FakeStore` harness from the Reviewer stage):
  - Empty slice -> `NoBundles`.
  - Two-Bundle slice with `allow_multi_bundle = false` -> `MultiBundleNotSupported { count: 2 }`.
  - Bundle with status `Proposed` -> `BundleNotAccepted { current: Proposed }`; no git calls.
  - Bundle with status `Reviewed` (not `Accepted` and not `Integrating`) -> `BundleNotAccepted { current: Reviewed }`; no git calls.
  - `work.parent_id != plan.id` -> `PlanBundleMismatch`; no git calls.
  - Fake git `rev-parse --verify` fails -> `IntegrationBranchMissing`.
  - Fake git `merge-base HEAD branch` == `rev-parse branch` -> `EmptyBranch`; `fail_all` transitions the Bundle to `IntegrationFailed`; no merge attempted.
  - Fake git merge succeeds -> `Tick` has one `merge_commits` entry; `bundles` has one `BundleId`; Bundle status is `Merged`; the `Merged` transition happens in Phase 3 AFTER `TicksStore::create`, verified by the fake store's call-order log.
  - Fake git merge fails -> `merge_abort` called; `reset_hard` called with `pre_merge_sha`; `fail_all` transitions the Bundle to `IntegrationFailed`; classification is `Retryable` (single-Bundle slice, no peers) -> `ConflictRetryable { stderr, .. }`. Critically: the Bundle is NEVER observed in state `Merged` at any point in the store write log, validating the batched-commit invariant.
  - Fake store `BundleUpdateError::Stale` during Phase 3 on the `Merged` write -> `IntegrationError::Update(Stale)`; Tick already persisted; git branch already advanced. Documented in Risks as the "Merge landed, Merged write lost the OCC race" edge; recovery is another `integrate` call with the Bundle in `Integrating`.
  - Fake git `reset --hard` fails after a merge conflict -> fatal `Git` error bubbled; documented in Risks.
  - **Crash-recovery path** (new, load-bearing for Invariant "Crash-recovery idempotency"):
    - Bundle in `Integrating`, `git merge-base --is-ancestor <head_commit> HEAD` returns true, Tick does NOT yet exist -> `AdoptedExisting` outcome; Phase 3 writes the Tick and transitions the Bundle to `Merged`; returns `Ok(tick)`. Verifies case (b) from the Invariant.
    - Bundle in `Integrating`, ancestry check returns true, Tick DOES already exist -> `TicksStore::create` returns `StoreError::DuplicateTick`; Integrator promotes to no-op; transitions Bundle to `Merged`; returns `Ok(existing_tick)`. Verifies case (a).
    - Bundle in `Integrating`, ancestry check returns false -> normal merge path; Phase 2 runs `git merge --no-ff`; succeeds; Phase 3 writes Tick + `Merged`. Verifies case (c).
    - Bundle in `Integrating`, ancestry check returns false, merge fails -> `fail_all`; Bundle -> `IntegrationFailed`; return typed conflict error. Crash-recovery does not shield a legitimately-failing merge.

### Phase 4: Seam tests with real git
**Model:** opus

Unit tests use fakes for speed and determinism; seam tests exercise the real `tokio::process::Command` path to catch regressions that fakes paper over.

- `crates/integrator/tests/integrate_seam.rs`: real tempdir git repo. Initialize, create integration branch at initial commit, create a bundle branch with one commit adding `file.txt`. Build `Bundle` at `Accepted` with `branch_name = "loopr/wk-<id>"`, matching `Work` with `plan_id = plan.id`. Call `integrate(&[bundle], &plan, &deps)`. Assert:
  - Returned Tick has one `merge_commits` entry; `integration_sha` matches the integration branch's HEAD; the merge commit has two parents (pre_merge_sha, bundle_head).
  - Bundle on disk is `Merged`.
  - The integration branch's `git log --oneline` shows the merge commit.
  - A second `integrate` call with the same Bundle returns `BundleNotAccepted { current: Merged }` without touching git.

- `crates/integrator/tests/integrate_empty_branch.rs`: bundle branch is equal to the integration branch's HEAD (no commits beyond merge base). Assert `EmptyBranch`; Bundle transitions to `IntegrationFailed`; working tree unchanged.

- `crates/integrator/tests/integrate_conflict_retryable.rs`: contrive a merge failure with no structural overlap. (Shape: integration branch has commit A; bundle branch was created from an unrelated initial commit B and adds `other.txt`; `git merge --no-ff` without `--allow-unrelated-histories` fails textually.) Assert `ConflictRetryable { stderr, .. }`; Bundle is `IntegrationFailed`; integration branch reset to pre_merge_sha (verified by `git rev-parse HEAD`).

- `crates/integrator/tests/integrate_conflict_structural.rs`: multi-Bundle structural conflict. Requires `allow_multi_bundle = true` locally for the test's `IntegratorConfig`, exercising the code path that multi-Bundle earns. Two Bundles whose `paths` both include `README.md`; first merges; second conflicts on `README.md`. Assert `ConflictStructural { files: ["README.md"], peer_bundle_ids: [<first id>] }`; the integration branch is reset to `pre_merge_sha`; every Bundle in the slice is `IntegrationFailed` in the store (the batched-commit invariant ensures no Bundle is ever observed at `Merged` during a partial-failure sequence, so there is no FSM-git divergence to reconcile).

### Phase 5: Architect review (design + post-impl) + doc close-out
**Model:** opus

- Run Architect R1 in Design Review mode (Mode 1) against this doc before Phase 1 implementation. Fold findings into a revised draft.
- After Phases 1-4 land and pass `otto ci`: run Architect R2 in Implementation Audit mode (Mode 2). Fold any post-implementation findings into a "Post-Implementation Notes" section.
- Update `Status: Implemented`; add `Shipped in: v0.5.X` once the tag lands.
- Update `docs/roadmap.md` Stage 8 row to point at this doc.

## Alternatives Considered

### Alternative 1: Integrator creates the integration branch on first call

- **Description:** If `loopr/plan-<plan-id>` is missing, `integrate` creates it from the target's current HEAD.
- **Pros:** Fewer moving parts; no Stage 8 wiring for branch creation.
- **Cons:** The base SHA becomes non-deterministic (depends on when the first Integrator fires vs when the Plan was filed). Breaks the "same bundles + same base = same Tick SHA" determinism invariant from `crates/integrator/CLAUDE.md`. A user commit between Plan-start and first-integrate would be unknowingly incorporated.
- **Why not chosen:** Base-SHA determinism requires the daemon to snapshot HEAD at Plan creation. That's a wiring concern; keep it explicit and out of the Integrator.

### Alternative 2: Validate as part of integrate (merge-then-validate)

- **Description:** After merge, run `cargo check` (or a configured list); on failure, roll back and transition Bundle to `IntegrationFailed`. Return `IntegrationError::ValidationFailed`.
- **Pros:** Stronger guarantee: a Tick means "builds green."
- **Cons:** (a) Reviewer already validates against acceptance criteria; adding validation here duplicates work and blurs failure attribution ("did the merge fail or did the tests fail?"). (b) Adds a `ToolExecutor` dep, expanding surface and eroding the "deterministic, non-LLM" simplicity. (c) The Stage 9 first-gate exit criterion asks for a Tick on approval, not a green build.
- **Why not chosen:** Merge-only is the minimum viable Tick. Validation is earned via a separate doc when a real run shows an approved Bundle breaking the integration branch.

### Alternative 3: `Tick` as an FSM with `Pending` / `Merged` / `Failed` / `Superseded`

- **Description:** Tick has `#[derive(Fsm)]` and transitions. v3 used this pattern with a `Failed` variant.
- **Pros:** Richer state (e.g., "superseded after revert"); uniform with Bundle/Work.
- **Cons:** Over-modeling. A Tick is born done: `integrate` is synchronous from the caller's perspective, so "in-progress Tick" never exists on disk. A failed merge produces no Tick at all (Bundle goes to `IntegrationFailed`). Adding `Superseded` for "user reverted the merge manually" is hypothetical until a run motivates it.
- **Why not chosen:** Verdict precedent: rich-type agent outputs are not automatically Records, and not every Record is an FSM. Tick is a Record without an FSM; re-enter when a query demands state.

### Alternative 4: No per-Plan integration branch; merge directly onto user's current branch

- **Description:** `git merge --no-ff loopr/wk-<work-id>` onto whatever branch the user was on when the Plan was filed.
- **Pros:** Fewer branches; the user's own branch has loopr's commits immediately.
- **Cons:** Loopr writes to user-owned refs, violating the spirit of vision line 515 (never-push applies to remote; the same separation applies locally). Concurrent editing (user + loopr) races on the ref. Undo is `git reset --hard` and lose work. v3/v4 both used dedicated integration branches for exactly these reasons.
- **Why not chosen:** Blast-radius containment is a v5 thesis; the dedicated branch is its git-layer expression. `git branch -D loopr/plan-<id>` is the one-command undo.

### Alternative 5: No OCC on Bundle state transitions

- **Description:** `integrate` takes `&mut Bundle` or issues a blind `Store::update`. No `expected_updated_at` check.
- **Pros:** One less trait dep.
- **Cons:** Two Integrators racing on the same Bundle (daemon-wiring bug) both write `Merged`. Silent overwrite of the attempt metadata. The Reviewer stage introduced OCC to close this class of race; it is free to reuse.
- **Why not chosen:** Reviewer's OCC is the anti-corruption boundary; extending it to the Integrator costs nothing.

### Alternative 6: Per-integration-branch git worktree

- **Description:** The daemon maintains a dedicated git worktree per `loopr/plan-<plan-id>`, so two Integrators on two Plans do not contend on the single `target/.git` checkout.
- **Pros:** Multi-Plan concurrency at the git layer; no single `git_lock` bottleneck.
- **Cons:** More state (a `WorktreeRegistry` entry per Plan), more crash recovery, more disk. First gate has at most one active Plan; the intra-daemon Mutex suffices.
- **Why not chosen:** YAGNI. Earn when multi-Plan contention is measured.

### Alternative 7: Reuse v3's `combine_conflicting_works` LLM-rescue on structural conflicts

- **Description:** On `ConflictStructural`, synthesize a new `Work` that combines the two conflicting Works, abandon the originals, and let the Implementer re-write.
- **Pros:** Automatic recovery from a common failure mode.
- **Cons:** Re-introduces LLM calls into (or adjacent to) the Integrator, eroding the "deterministic, non-LLM" rule. The combine flow was flaky enough in v3 to warrant many follow-up fixes; it is not core to first-gate correctness. The typed `ConflictStructural` error gives the daemon enough information to mount a recovery later, outside the Integrator.
- **Why not chosen:** Recovery policy is the daemon's; the Integrator's job is to report truth.

## Technical Considerations

### Dependencies

No new external crates.

- `integrator` has no dependencies currently (verified against `crates/integrator/Cargo.toml`; the crate is a stub with only `lib.rs`'s header comment). Phase 2 adds via `cargo add`: `tokio` (with `process` + `sync` features for `tokio::process::Command` and `tokio::sync::Mutex`), `thiserror` (for `IntegrationError`), plus the workspace-inherited `domain`, `store`, `worktree` per the crate's CLAUDE.md dependency list. `store::BundleUpdateSink` / `BundleUpdateError` are pulled in once Phase 1's relocation lands them in `store`.
- `domain`: new file (`tick.rs`), new typed ID (`TickId`). No new crates.
- `store`: new submodule (`ticks.rs`); inherits existing deps.

`integrator` remains `llm`-free at the Cargo graph level. This doc does not add `llm` to `crates/integrator/Cargo.toml`.

### Performance

- Sequential git subprocesses per Bundle: `verify-branch`, `checkout`, `rev-parse HEAD`, `merge-base HEAD <branch>`, `rev-parse <branch>`, `merge --no-ff <branch>`, `rev-parse HEAD`. Each subsecond on a local repo; bounded by `IntegratorConfig::git_timeout` (60s default).
- `TicksStore::create` is one JSONL append plus one SQLite index upsert; microseconds.
- `git_lock` held for the full `integrate` call: with one Plan active at a time in first gate, contention is nil. Multi-Plan contention becomes measurable when first gate opens; Alternative 6 is the escape hatch.
- OCC `BundleUpdateSink::update` holds the `BundlesStore` intra-daemon Mutex for the duration of one read-check-write (microseconds). The `git_lock` serializes the working tree, not the Bundle state; the two locks never share scope.

### Security

- All git subprocess arguments derive from typed fields (`plan.id -> format!("loopr/plan-{}", plan.id)`; `bundle.branch_name` populated by the Implementer's commit flow). `tokio::process::Command::arg` is used per-argument; no `sh -c` invocation with user-influenced strings.
- `IntegratorDeps::target` is the trusted target repo root. The `security.sandbox` posture does not apply here: the Integrator runs no agent code, only git.
- No network I/O; Integrator never dials out.

### Testing Strategy

Per `CLAUDE.md` "seam tests, not only unit tests":

**Unit (in `crates/integrator/src/tests.rs` and per-submodule tests; fake git harness):**

- `Tick` / `TickId` / `TicksStore` CRUD per Phase 1.
- `IntegrationError` display output per variant.
- `classify_conflict` boundary cases: no overlap -> `Retryable`, full overlap -> `Structural`, partial overlap -> `Structural` with `files` = intersection.
- Each precondition failure variant: `NoBundles`, `MultiBundleNotSupported`, `BundleNotAccepted`, `PlanBundleMismatch`, `IntegrationBranchMissing`, `EmptyBranch`.
- Happy path: Bundle transitions `Accepted -> Integrating -> Merged`; Tick persisted.
- `ConflictRetryable` rollback: Bundle transitions to `IntegrationFailed`; `merge_abort` + `reset_hard` sequence observed in the fake.
- OCC stale during `Accepted -> Integrating`: `IntegrationError::Update(Stale)` bubbled.

**Seam (in `crates/integrator/tests/`, real tempdir git):**

- Happy path round-trip with real merge commit (two-parent topology verified).
- `EmptyBranch` on a bundle branch equal to integration HEAD.
- `ConflictRetryable` via unrelated-histories merge failure.
- `ConflictStructural` via two bundles whose `paths` overlap on `README.md` (requires `allow_multi_bundle = true` for the test; otherwise the slice-length check trips first).
- Double-integration rejection: second `integrate` on a Merged Bundle returns `BundleNotAccepted { current: Merged }` with no git calls.

**Integration (to be earned at Stage 8 wiring, not this doc):**

- Reviewer -> Integrator full handoff with the real daemon pipeline lives in the capstone.

**Out of scope:**

- Cross-process git contention (single-daemon-per-target).
- Remote-push semantics (never-push).
- Validation.
- Fuzzed merge failures from git itself.

### Rollout

Single branch (`v5`), per-phase commits. Each phase passes `otto ci` at repo root. Single tag bump after Phase 5 (target: next available `v0.5.X`).

## Acceptance Criteria

All must be true before the doc is marked Implemented:

- `domain::Tick` exists with the fields in Data Model; `TickId` exists with `tk-` prefix; serde round-trip and `FromStr`/`Display` tests pass.
- `store::TicksStore` exists with `create`, `get`, `list_by_plan_id`; `Store::ticks(&self)` accessor exists; all three tests pass.
- `domain::Tick` does **not** have a `#[derive(Fsm)]` (first-gate record-only defer confirmed).
- `integrator::IntegrationError` has every variant named in Data Model; each variant is reachable by some test (unit or seam).
- `integrator::IntegratorConfig` exists with `git_timeout = 60s` and `allow_multi_bundle = false` defaults.
- `integrator::integrate(bundles, plan, deps)` passes all Phase 3 unit tests: `NoBundles`, `MultiBundleNotSupported`, `BundleNotAccepted` for every non-`Accepted`-non-`Integrating` variant, `PlanBundleMismatch`, `IntegrationBranchMissing`, `EmptyBranch`, happy-path Tick creation, `ConflictRetryable` with rollback.
- The store-git atomicity invariant is verified: the test with a merge failure confirms the Bundle is never observed in state `Merged` in the fake store's call-order log.
- All four crash-recovery paths pass: `Integrating` + ancestor=true + no Tick, `Integrating` + ancestor=true + Tick exists (`DuplicateTick` promoted to no-op), `Integrating` + ancestor=false, `Integrating` + ancestor=false + merge fails.
- `TicksStore::create` returns `StoreError::DuplicateTick { plan_id, bundles }` when called with a `(plan_id, bundles set)` that already matches a persisted Tick; dedicated variant, not folded into a generic error.
- Phase 4 seam tests pass: happy path with real two-parent merge, `EmptyBranch`, `ConflictRetryable`, `ConflictStructural` (multi-Bundle gated by config), double-integration rejection.
- `otto ci` at repo root passes; `cargo test -p domain`, `-p store`, `-p integrator` all pass.
- `crates/integrator/Cargo.toml` has no `llm` dep (mechanical check: `cargo tree -p integrator -i llm` errors "not found").
- No existing Record migration required (Tick is additive; Bundle and Plan are unchanged).

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Integration branch missing at integrate time | Low | Med | Typed `IntegrationBranchMissing`; Stage 8 wiring capstone creates at Plan start |
| Wiring bug: Bundle at wrong status reaches integrator | Low | High | Pre-flight `BundleNotAccepted` returns the current status; FSM rejection is the second line of defense |
| Wiring bug: Bundle from wrong Plan reaches integrator | Low | High | Pre-flight `PlanBundleMismatch` via `work.parent_id` check |
| Merge conflict on a single Bundle with no peers to overlap | Med | Med | `ConflictRetryable` returned; daemon decides re-implement vs abandon |
| Empty bundle branch (Implementer committed nothing) | Low | Med | `merge-base HEAD <branch>` == `rev-parse <branch>` guard; `EmptyBranch` before merge |
| `git merge --no-ff` silently FF's | Very Low | Med | `EmptyBranch` pre-check blocks the zero-delta case; `--no-ff` forces a merge commit otherwise |
| Two parallel integrators contend on the working tree | Low | High | Intra-daemon `git_lock: Arc<Mutex<()>>` on `IntegratorDeps` |
| Two parallel integrators race on the same Bundle | Low | High | OCC via `BundleUpdateSink::update(bundle, expected_updated_at)`; second writer gets `Stale` |
| Daemon crashes after `Accepted -> Integrating` write, before git merge runs | Low | Med | Bundle is `Integrating`. Daemon's restart sweep re-calls `integrate`; pre-flight accepts `Integrating`; idempotency check returns false (no merge landed); normal merge path runs. Resolved by "Crash-recovery idempotency" invariant. |
| Daemon crashes after git merge lands, before Tick write | Low | Med | Bundle is `Integrating`, branch is advanced, no Tick exists. Daemon's restart sweep re-calls `integrate`; idempotency check returns true; outcome is `AdoptedExisting`; Phase 3 writes the Tick and transitions `Merged`. |
| Daemon crashes after Tick write, before `Merged` transition | Low | Med | Bundle is `Integrating`, branch is advanced, Tick exists. Restart re-calls `integrate`; idempotency check returns true; `TicksStore::create` returns `DuplicateTick`; Integrator promotes to no-op; `Merged` transition runs. |
| OCC race on Phase 3 `Merged` write (another writer advanced `updated_at`) | Very Low | Med | `BundleUpdateError::Stale` bubbles as `IntegrationError::Update(Stale)`. Git branch is advanced, Tick is persisted, Bundle is still `Integrating`. Same recovery as the "Tick write, before Merged" case above. |
| `reset --hard` during rollback fails (corrupted working tree) | Very Low | High | Fatal `IntegrationError::Git`; daemon restart's worktree crash recovery owns the repair |
| Multi-Bundle silently accepted via code regression | Low | Med | `MultiBundleNotSupported` is the default; explicit test on two-element slice |
| LLM dep sneaks back into integrator | Low | Med | Mechanical: `cargo tree -p integrator -i llm` check in CI (added alongside Phase 1) |
| Implementer branch deleted before integrate | Low | Med | `git merge` errors "not something we can merge"; surfaces as `ConflictRetryable` or `Git`; Bundle -> `IntegrationFailed`; daemon decides |
| `bundle.head_commit` diverges from `git rev-parse <branch>` (force-push) | Very Low | Med | Not in first-gate threat model (no force-push flow exists). Tracked as Open Question |
| User runs loopr on a dirty working tree | Low | Med | `git checkout` errors on dirty tree; surfaces as `Git(...)`; not an Integrator concern to pre-check. Operator error, documented in roadmap for Stage 8 wiring to surface more clearly |

## Open Questions

- [ ] **Integration branch creation timing in Stage 8 wiring.** Created at `handle_plan_create`, or lazily at first integrate-eligible approval? This doc assumes the former (cleaner determinism); the wiring doc should confirm and, if lazy, add an `ensure_branch` hook.
- [ ] **`bundle.head_commit` vs `git rev-parse <branch>` verification.** First gate has no force-push flow, so the two should match by construction. The crash-recovery idempotency check (`git merge-base --is-ancestor <bundle.head_commit> HEAD`) indirectly catches one form of drift: if the Bundle's head was rebased away, the ancestor check is false and the normal merge path runs. A dedicated `HeadCommitDrift` variant is still deferred; revisit if the merge-path error it produces proves insufficient for diagnosis.
- [ ] **Multi-Bundle Tick flip.** When does `allow_multi_bundle` default to `true`? Earn when a real run shows a Plan producing multiple co-approvable Bundles where one-at-a-time integration ordering matters. The pre-condition is a design doc for rollback semantics under multi-Bundle partial failure.
- [ ] **Post-merge validation.** Expected after first gate, motivated by a real run where a Reviewer-approved Bundle breaks the integration branch. Separate design doc.
- [ ] **Tick supersession on manual revert.** If the user `git revert`s a Tick's merge commit, should the Tick be marked superseded? Current answer: no - the git log is truth, the Tick records what happened at merge time. Revisit when real runs show it matters.
- [ ] **Per-integration-worktree for multi-Plan concurrency.** Alternative 6. Earn when measured.
- [ ] **Per-Bundle lock map for `BundlesStore::update`.** Inherited from the Reviewer stage's Open Questions. Not first-gate scope.

## References

- `docs/vision.md`:
  - Line 16: `Goal -> decomposer -> Work DAG -> agents -> Bundles -> integrator -> Tick`
  - Line 49: `integrator` crate role and `llm`-free dependency rule
  - Lines 175-181: Integrator section (signature, determinism invariant, no-LLM at Cargo graph)
  - Lines 269: `ralph.<role>` span convention (`stage.integrate`)
  - Line 515: Never-push policy; branch-ownership boundary
  - Line 521: Branch naming `loopr/plan-<plan-id>` / `loopr/wk-<work-id>`
  - Lines 593-595: First Gate steps 4-5 (merge, publish Tick, manual merge to main)
- `crates/integrator/CLAUDE.md`: in-scope/out-of-scope rules; `llm`-free invariant; determinism invariant
- `crates/domain/src/bundle.rs:42-58`: Bundle FSM transitions consumed (`Accepted -> Integrating -> Merged | IntegrationFailed`)
- `crates/domain/src/role.rs`: `Role::Integrator`
- `docs/design/2026-04-22-reviewer.md`: structural analog for this doc; `Verdict`-as-record-of-fact precedent that justifies `Tick`-as-record-of-fact; introduces `BundleUpdateSink` / `BundleUpdateError` reused here; introduces the Mutex-based OCC pattern on `BundlesStore::update` reused here
- `docs/design/2026-04-21-implementer.md`: Stage 7 Implementer; produces the `loopr/wk-<work-id>` branches this doc consumes
- `docs/design/2026-04-22-stage-7-wiring.md`: Stage 7 daemon wiring; pattern for the eventual Stage 8 wiring capstone
- `crates/store/src/bundles.rs`: after Phase 1 relocation, hosts `BundleUpdateSink` trait, `BundleUpdateError` enum, the `impl BundleUpdateSink for store::Store`, and the forwarding `impl<B> BundleUpdateSink for &B`. Previously lived in `crates/agents/src/reviewer.rs` under the Reviewer design doc; moved here to keep the Integrator's `llm`-free dependency graph
- `crates/store/src/bundles.rs`: `BundlesStore::update` OCC primitive reused
- `loopr/src/agents/integrator.rs` (v3 prior art, verified):
  - `integration_branch_name` (line 1590): v3 uses `integration/<plan-id>`; v5 renames to `loopr/plan-<plan-id>` per vision
  - `classify_conflict` (line 1602): v5 adopts the `bundle.paths` file-overlap classification verbatim, minus the `Stores` access (v5 passes peers by slice)
  - `merge_bundle_branches` (line 1636): empty-branch detection via `merge-base` + `rev-parse`, `--no-ff` rationale, `merge --abort` cleanup
  - Pre-merge SHA capture + `reset --hard` rollback (lines 622, 637-646)
- `loopr-v4/src/daemon/handlers/integrator.rs`: v4 prior art (`handle_integrator_validate` + `handle_integrator_publish` split; v5 collapses to a single `integrate` call since validation is deferred)

## Post-Implementation Notes

### 2026-04-22: Architect R2 audit — four findings folded

Post-implementation audit (Architect R2, Implementation Audit mode) surfaced four findings that were folded in one commit after the Phase 1-4 code landed. All four were legitimate; all four are fixed in the shipped code.

**Finding 1 (defensible addition):** The Loop Contract's original pseudo-code said "Phase 2: Git sequence (no store writes)," yet the crash-recovery invariant required Bundles to exist on disk as `Integrating` for re-entry to work. The implementation introduced a Phase 2 prologue that transitions every `Accepted` Bundle to `Integrating` in one OCC write *before* any git op. The design doc's Loop Contract has been updated to show the prologue explicitly; the prologue is the correct expansion of the invariant, not a deviation.

**Finding 2 (correctness bug):** Initial implementation routed the per-Bundle loop on `bundle_states` (all `Integrating` after prologue) and used `head_commit == pre_merge_sha` as the empty-branch check. Architect noted this is unsound when the integration branch advances between Bundle creation and integration: the naive equality bypasses the guard, `is_ancestor` returns true on the resulting ancestor, and `merge_commit_sha_for` silently adopts the *wrong* merge commit. Fixed by routing on the ORIGINAL bundle status (from the caller's input slice). `Accepted` on entry uses the merge-base-based `assert_nontrivial_branch` (safe because the branch cannot be already-merged); `Integrating` on entry runs `is_ancestor` first (adopt or fall-through), then `assert_nontrivial_branch` only in the fall-through arm.

**Finding 3 (invariant violation):** Initial implementation bubbled `StoreError::DuplicateTick` back to the caller rather than resolving the existing Tick. The design's crash-recovery invariant case (a) mandated `Ok(existing_tick)` with the Bundle transitioned to `Merged`. Fixed by extending the `TickSink` trait with `get(tick_id) -> Option<Tick>`, resolving the existing Tick on `DuplicateTick`, and completing the Phase 3 `Merged` transitions. The corresponding seam test `crash_recovery_a_merge_landed_tick_landed_merged_write_lost` now asserts `Ok(existing_tick)` and `BundleStatus::Merged`.

**Finding 4 (doc/comment paradox):** The "EXPECTED DIVERGENCE" commentary on the structural-conflict seam test claimed both Bundles were `IntegrationFailed` in the store while the first's merge was rolled out of git — a logical contradiction (if the Store says `IntegrationFailed` and git rolled back, store and git agree; there is no divergence). The batched-commit Phase 3 prevents any `Merged` from ever landing if the sequence fails, so the Bundle is never observed at `Merged` in the store. Test comments and design-doc wording updated to state this positively: batched-commit prevented the divergence.

### Shipped in: v0.5.X (tag to be added on release)
