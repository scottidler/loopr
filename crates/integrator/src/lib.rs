//! Deterministic, non-LLM merge-publish. Accepted Bundles into Tick.
//!
//! Entry point: `integrate(bundles, plan, deps) -> Result<Tick, IntegrationError>`.
//! First-gate scope is merge-only; validation is deferred (see the crate's
//! CLAUDE.md and docs/design/2026-04-22-integrator.md).
//!
//! This crate does not depend on `llm` (mechanically enforced at the Cargo
//! graph level; `cargo tree -p integrator -i llm` returns "not found").
//! Agent-specific plumbing (`agents::run_reviewer`, etc.) is out of scope.

mod classify;
mod config;
mod error;
mod git;
mod validation;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{debug, info, instrument, warn};

use domain::{Bundle, BundleId, BundleStatus, Plan, Role, Tick, TickId, Work, WorkId};
use store::{BundleUpdateSink, StoreError};

pub use config::IntegratorConfig;
pub use error::IntegrationError;

use crate::classify::{ConflictKind, classify_conflict, is_merge_conflict};

// ---------------------------------------------------------------------------
// DI traits: the `IntegratorDeps` struct bundles these.
// ---------------------------------------------------------------------------

/// Read-only `Work` lookup. The Integrator fetches Work records by
/// `bundle.work_id` to verify `work.parent_id == plan.id` during
/// pre-flight. Read-only; the Integrator never transitions a Work
/// (that's the daemon's job after an `Ok(Tick)` return).
#[trait_variant::make(Send)]
pub trait WorkLookup: Send + Sync {
    async fn get(&self, work_id: &WorkId) -> Result<Option<Work>, StoreError>;
}

/// Append-only `Tick` persistence plus read-back. The Integrator
/// calls `create` in Phase 3 after the git sequence succeeds. On a
/// duplicate `(plan_id, bundles-as-set)`, the store returns
/// `StoreError::DuplicateTick { tick_id, .. }`; the Integrator then
/// calls `get(tick_id)` to resolve the existing Tick so it can
/// return `Ok(existing_tick)` to the daemon after completing the
/// `Merged` transitions.
#[trait_variant::make(Send)]
pub trait TickSink: Send + Sync {
    async fn create(&self, tick: Tick) -> Result<Tick, StoreError>;
    async fn get(&self, tick_id: &TickId) -> Result<Option<Tick>, StoreError>;
}

// Real impls backed by `store::Store`.

impl WorkLookup for store::Store {
    async fn get(&self, work_id: &WorkId) -> Result<Option<Work>, StoreError> {
        // `WorksStore::get` returns `Err(RecordNotFound)` for missing;
        // the WorkLookup contract is `Option` so the Integrator can
        // distinguish "wiring bug (bundle references a non-existent
        // work)" from other store failures.
        match self.works().get(work_id).await {
            Ok(w) => Ok(Some(w)),
            Err(StoreError::RecordNotFound { .. }) => Ok(None),
            Err(other) => Err(other),
        }
    }
}

impl<T: WorkLookup + ?Sized> WorkLookup for &T {
    async fn get(&self, work_id: &WorkId) -> Result<Option<Work>, StoreError> {
        (*self).get(work_id).await
    }
}

impl TickSink for store::Store {
    async fn create(&self, tick: Tick) -> Result<Tick, StoreError> {
        self.ticks().create(tick).await
    }
    async fn get(&self, tick_id: &TickId) -> Result<Option<Tick>, StoreError> {
        match self.ticks().get(tick_id).await {
            Ok(t) => Ok(Some(t)),
            Err(StoreError::RecordNotFound { .. }) => Ok(None),
            Err(other) => Err(other),
        }
    }
}

impl<T: TickSink + ?Sized> TickSink for &T {
    async fn create(&self, tick: Tick) -> Result<Tick, StoreError> {
        (*self).create(tick).await
    }
    async fn get(&self, tick_id: &TickId) -> Result<Option<Tick>, StoreError> {
        (*self).get(tick_id).await
    }
}

// ---------------------------------------------------------------------------
// `IntegratorDeps`: the single handle passed to `integrate`.
// ---------------------------------------------------------------------------

/// Bundles the Integrator's injected dependencies. One generic
/// parameter flows through `integrate`'s signature; concrete trait
/// bounds live on the struct. Matches the `Deps<L, T, W, S, C>`
/// pattern from `agents`, but with a different set of traits:
/// no `LlmClient`, no `ToolExecutor`, no `ContextBuilder`.
pub struct IntegratorDeps<U, W, T>
where
    U: BundleUpdateSink,
    W: WorkLookup,
    T: TickSink,
{
    /// OCC update sink for Bundle transitions (`Accepted -> Integrating`,
    /// `Integrating -> Merged | IntegrationFailed`). Same trait the
    /// Reviewer consumes, relocated from `agents` to `store` in Phase 1.
    pub bundle_sink: U,

    /// Read-only Work lookup for the `work.parent_id == plan.id`
    /// pre-flight check.
    pub works: W,

    /// Append-only Tick persistence for Phase 3.
    pub ticks: T,

    /// Runtime knobs (timeouts, multi-Bundle guardrail).
    pub config: IntegratorConfig,

    /// Target repo root. Used for `git -C <target>` subprocesses.
    pub target: PathBuf,

    /// Intra-daemon working-tree serializer. Held for the full
    /// checkout/merge/rollback sequence (Phase 2). Two parallel
    /// Integrator tasks on the same `Store` share this lock; two
    /// parallel Integrators on different `Store`s (impossible under
    /// single-daemon-per-target) would need separate locks.
    pub git_lock: Arc<Mutex<()>>,
}

// ---------------------------------------------------------------------------
// Entry point (Phase 3 will fill this in).
// ---------------------------------------------------------------------------

/// Outcome of the per-Bundle step in Phase 2. Bound to a `BundleId`
/// by the `outcomes` Vec; never written to the store.
#[derive(Debug, Clone)]
enum MergeOutcome {
    /// This call's `git merge --no-ff` produced a new merge commit.
    NewMerge { sha: String },
    /// A prior crashed `integrate` call had already merged this
    /// Bundle's `head_commit`; the ancestry check found the existing
    /// merge commit and the Integrator adopts its SHA.
    AdoptedExisting { sha: String },
}

impl MergeOutcome {
    fn sha(&self) -> String {
        match self {
            MergeOutcome::NewMerge { sha } | MergeOutcome::AdoptedExisting { sha } => sha.clone(),
        }
    }
}

/// Merge Accepted Bundles into a Plan's integration branch and
/// produce a Tick. See docs/design/2026-04-22-integrator.md for the
/// full loop contract.
///
/// Four phases:
/// 1. Pre-flight (no mutation): shape + status + plan + branch.
/// 2. Git sequence under `git_lock`: in-memory `outcomes` only; no
///    store writes.
/// 3. Validation: run `IntegratorConfig.validation_commands` in the
///    target directory; on failure, roll back the merge and return
///    `ValidationFailed`. Skipped when the list is empty.
/// 4. Commit: write Tick first, then transition every Bundle to
///    `Merged`. Store and git either both advance or neither does.
///
/// Bundles in `BundleStatus::Integrating` are accepted as a
/// crash-recovery re-entry point: the idempotency check
/// (`git merge-base --is-ancestor`) adopts an existing merge commit
/// if the prior call succeeded before the crash, and falls through
/// to the normal merge path otherwise.
#[instrument(
    name = "integrator.integrate",
    level = "info",
    skip_all,
    fields(
        plan_id = %plan.id,
        bundle_count = bundles.len(),
        target = %deps.target.display(),
        integration_branch = tracing::field::Empty,
        phase = tracing::field::Empty,
    ),
    err,
)]
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
    let span = tracing::Span::current();
    span.record("phase", "preflight");
    info!(
        phase = "preflight",
        bundle_count = bundles.len(),
        "integrator: phase begin"
    );
    // Phase 1: pre-flight (no git, no lock)
    preflight_shape(bundles, deps.config.allow_multi_bundle)?;
    preflight_status(bundles)?;
    preflight_plan_consistency(bundles, plan, &deps.works).await?;

    // Phase 2: git sequence (serialize against the working tree).
    //
    // TOCTOU (findings: review 2026-06-09): branch resolution,
    // `verify_branch`, the crash-recovery merge-abort, and the
    // dirty-tree guard ALL run UNDER `git_lock`. Resolving or verifying
    // the branch outside the lock raced a concurrent `integrate`
    // mutating the same working tree.
    span.record("phase", "git_sequence");
    let _git_guard = deps.git_lock.lock().await;

    // Phase C: per-Plan branch is the default; the no-branch override
    // integrates onto the currently-checked-out branch (loopr never
    // creates loopr/plan-<id> in that mode, and never merges to main).
    let integ_branch = if deps.config.integration_branch {
        format!("loopr/plan-{}", plan.id)
    } else {
        git::current_branch(&deps.target, deps.config.git_timeout).await?
    };
    span.record("integration_branch", integ_branch.as_str());
    info!(
        phase = "git_sequence",
        integration_branch = %integ_branch,
        "integrator: phase begin"
    );
    if !git::verify_branch(&deps.target, &integ_branch, deps.config.git_timeout).await? {
        return Err(IntegrationError::IntegrationBranchMissing { branch: integ_branch });
    }

    // Crash-recovery merge-abort: a daemon crash mid-merge leaves a
    // conflicted index + MERGE_HEAD on disk; the re-entry's `git
    // checkout` below then fails ("you need to resolve your current
    // index first") and wedges integration permanently. Abort any
    // in-progress merge BEFORE the dirty-tree guard (a conflicted index
    // reads as dirty) and BEFORE checkout. Best-effort: a no-op when no
    // merge is in progress.
    if git::merge_in_progress(&deps.target, deps.config.git_timeout).await? {
        warn!("integrator: aborting in-progress merge left by a prior crash before re-entry");
        git::merge_abort(&deps.target, deps.config.git_timeout).await;
    }

    // Dirty-tree guard (unconditional, both modes): refuse to integrate
    // onto a dirty working tree. The per-Plan-branch path was previously
    // unguarded on the false premise that `git checkout loopr/plan-<id>`
    // protects it - it does not: non-conflicting dirty state is carried
    // silently across the checkout and a later merge then misclassifies
    // it as a terminal conflict. The no-branch override path is even
    // more exposed (`git checkout <current-branch>` is a no-op that
    // never fails on a dirty tree). Guard both.
    //
    // HEAD-parking decision: per-Plan-branch mode intentionally leaves
    // HEAD on `loopr/plan-<id>` after `integrate` returns. That branch
    // is the daemon's integration workspace, where subsequent integrates
    // and the operator's eventual merge-to-main happen; restoring HEAD
    // to the prior branch would force a redundant re-checkout on every
    // integrate with no benefit. The dirty-tree guard makes the parked
    // HEAD safe (the tree is always clean at entry).
    if git::working_tree_dirty(&deps.target, deps.config.git_timeout).await? {
        return Err(IntegrationError::DirtyWorkingTree { branch: integ_branch });
    }

    // Phase 2 prologue: transition every Accepted Bundle to Integrating.
    // This is a single OCC write per Bundle, BEFORE any git operation.
    // Bundles already in Integrating (crash-recovery re-entry) skip
    // the transition. After this prologue, every in-memory
    // `bundle_states[i]` carries the Integrating status + fresh
    // updated_at; Phase 2's merge loop and Phase 3's commit path use
    // `bundle_states` instead of the caller's input slice.
    //
    // Phase 2 prologue may partially succeed if a later Bundle's OCC
    // write fails. Bundles transitioned to Integrating before that
    // failure stay Integrating on disk; the daemon's retry sees them
    // and the crash-recovery path resolves them (ancestry check falls
    // through to normal merge since no merge landed).
    let mut bundle_states: Vec<Bundle> = Vec::with_capacity(bundles.len());
    for b in bundles {
        match b.status {
            BundleStatus::Accepted => {
                let next = transition_bundle_returning(&deps.bundle_sink, b, BundleStatus::Integrating).await?;
                bundle_states.push(next);
            }
            BundleStatus::Integrating => {
                // Crash-recovery: already Integrating on disk.
                bundle_states.push(b.clone());
            }
            BundleStatus::Merged => {
                // Partial-crash re-entry: a prior call already finished
                // this Bundle (Merged + its merge on the integration
                // branch). Carry it as-is; the merge loop adopts its
                // existing merge and the Phase-4 loop skips re-writing it.
                bundle_states.push(b.clone());
            }
            _ => unreachable!("pre-flight rejects non-Accepted/non-Integrating/non-Merged bundles"),
        }
    }

    git::checkout(&deps.target, &integ_branch, deps.config.git_timeout).await?;
    let pre_merge = git::rev_parse_head(&deps.target, deps.config.git_timeout).await?;

    let mut outcomes: Vec<(BundleId, MergeOutcome)> = Vec::with_capacity(bundles.len());

    // The per-Bundle loop routes on the ORIGINAL bundle status
    // (from the input slice), not the post-prologue `bundle_states`.
    // A bundle that entered Accepted is a fresh integration whose
    // branch cannot already be merged; one that entered Integrating
    // is a crash-recovery re-entry whose branch MAY already be merged.
    // Architect R2 finding: routing on bundle_states (all Integrating
    // after prologue) and using `head_commit == pre_merge` as
    // the empty-branch check is unsound when the integration branch
    // advances between Bundle creation and its integration - the
    // naive equality bypasses the guard and a later is_ancestor +
    // merge_commit_sha_for silently grabs the wrong merge commit.
    for (b_original, b) in bundles.iter().zip(bundle_states.iter()) {
        let head_commit = match b.head_commit.as_deref() {
            Some(sha) => sha,
            None => {
                return fail_all(
                    &bundle_states,
                    &pre_merge,
                    deps,
                    IntegrationError::Git(format!("bundle {} has no head_commit", b.id)),
                )
                .await;
            }
        };

        match b_original.status {
            BundleStatus::Accepted => {
                // Fresh integration: the branch cannot be already-
                // merged into the integration branch (nothing else
                // writes to loopr/plan-<id> in first gate). Use the
                // standard merge-base check to detect an empty branch;
                // it cannot falsely trip here.
                if let Err(err) =
                    git::assert_nontrivial_branch(&deps.target, b.id.as_ref(), &b.branch_name, deps.config.git_timeout)
                        .await
                {
                    return fail_all(&bundle_states, &pre_merge, deps, err).await;
                }
                // No ancestry check: if Accepted, the branch has never
                // been merged, so is_ancestor is guaranteed false.
            }
            BundleStatus::Integrating | BundleStatus::Merged => {
                // Crash-recovery re-entry: a prior call may have
                // already merged this Bundle's head_commit (Integrating:
                // crashed mid-merge or before Tick; Merged: a prior
                // multi-Bundle Phase-4 finished THIS Bundle before
                // crashing). Ancestry check first, so an already-merged
                // branch adopts cleanly instead of falsely tripping
                // EmptyBranch (a merged branch has
                // `merge-base HEAD branch == branch`).
                //
                // `is_ancestor` alone false-adopts: a trivially-ancestral
                // head_commit (the integration base, or one absorbed by a
                // DIFFERENT bundle's merge) reads as already-merged and the
                // old `merge_commit_sha_for` then grabbed the wrong merge
                // commit. `find_adopting_merge` confirms the adopted merge's
                // SECOND parent is exactly this head_commit; on no match it
                // returns None and we fall through to the normal merge path.
                let already_merged =
                    git::is_ancestor(&deps.target, head_commit, "HEAD", deps.config.git_timeout).await?;
                let adopted = if already_merged {
                    git::find_adopting_merge(&deps.target, head_commit, deps.config.git_timeout).await?
                } else {
                    None
                };
                if let Some(sha) = adopted {
                    outcomes.push((b.id.clone(), MergeOutcome::AdoptedExisting { sha }));
                    continue;
                }
                // Not actually merged by this Bundle (or false-positive
                // trivial ancestry): fall through to the normal merge.
                // The empty-branch check is safe here - either ancestry
                // was false, or it was trivial and no real merge of this
                // Bundle exists, so merge-base HEAD branch != branch tip
                // unless the branch is genuinely empty (which EmptyBranch
                // correctly reports).
                if let Err(err) =
                    git::assert_nontrivial_branch(&deps.target, b.id.as_ref(), &b.branch_name, deps.config.git_timeout)
                        .await
                {
                    return fail_all(&bundle_states, &pre_merge, deps, err).await;
                }
            }
            _ => unreachable!("pre-flight rejects non-Accepted/non-Integrating bundles"),
        }

        match git::merge_no_ff(&deps.target, &b.branch_name, deps.config.git_timeout).await {
            Ok(Ok(sha)) => {
                outcomes.push((b.id.clone(), MergeOutcome::NewMerge { sha }));
            }
            Ok(Err(output)) => {
                // The merge ran and exited non-zero. Restore the tree
                // first (both the conflict and non-conflict paths need
                // a clean tree before returning).
                git::merge_abort(&deps.target, deps.config.git_timeout).await;
                git::reset_hard(&deps.target, &pre_merge, deps.config.git_timeout).await?;

                if is_merge_conflict(&output) {
                    // Genuine merge conflict: terminal. The same content
                    // cannot merge on retry, so fail the Bundle. Classify
                    // structural (peer path overlap) vs retryable
                    // (textual/environmental).
                    let kind = classify_conflict(b, &bundle_states);
                    let err = match kind {
                        ConflictKind::Structural { files, peer_bundle_ids } => IntegrationError::ConflictStructural {
                            bundle_id: b.id.as_ref().to_string(),
                            files,
                            peer_bundle_ids,
                        },
                        ConflictKind::Retryable => IntegrationError::ConflictRetryable {
                            bundle_id: b.id.as_ref().to_string(),
                            branch: b.branch_name.clone(),
                            stderr: output,
                        },
                    };
                    // After rollback: every Integrating Bundle in the
                    // slice transitions to IntegrationFailed in one batch.
                    return fail_all_without_reset(&bundle_states, deps, err).await;
                }

                // Non-conflict merge failure (deleted/missing branch,
                // "local changes would be overwritten", ENOSPC, index-lock
                // contention): NOT a conflict and NOT a permanent Bundle
                // failure. The tree is already restored; leave the Bundles
                // Integrating (the driver's retry contract re-enqueues
                // them) and surface a retryable infrastructure error
                // WITHOUT marking the Bundles terminally IntegrationFailed.
                return Err(IntegrationError::Git(format!(
                    "git merge {} exited non-zero with no conflict marker (infrastructure failure): {}",
                    b.branch_name, output
                )));
            }
            Err(infra) => {
                // The merge subprocess failed to complete (spawn error
                // or timeout). kill_on_drop killed any orphan, but the
                // tree may be partially advanced; route through fail_all
                // so git is reset AND the DB records the failure -
                // closing the git-advanced/DB-silent gap the bare `?`
                // left open.
                return fail_all(&bundle_states, &pre_merge, deps, infra).await;
            }
        }
    }

    // Phase 3: validation (skipped when validation_commands is empty)
    span.record("phase", "validation");
    info!(
        phase = "validation",
        validation_commands = deps.config.validation_commands.len(),
        "integrator: phase begin"
    );
    if !deps.config.validation_commands.is_empty()
        && let Err(val_err) = validation::run_validation(
            &deps.config.validation_commands,
            deps.config.validation_timeout,
            &deps.target,
        )
        .await
    {
        // AdoptedExisting rollback hazard: when EVERY outcome is
        // AdoptedExisting, a prior crashed call already merged this
        // slice, so `pre_merge` was captured AFTER that merge landed.
        // `reset_hard(pre_merge)` would be a no-op that cannot un-merge,
        // and `fail_all` would then mark the Bundles IntegrationFailed
        // while their commits sit durably on the integration branch -
        // manufactured git/DB divergence. Skip the rollback-and-fail
        // path entirely: clean up validation's untracked artifacts and
        // surface a distinct, NON-terminal error (the Bundles stay
        // Integrating; the driver/operator decides recovery).
        let all_adopted = outcomes
            .iter()
            .all(|(_, o)| matches!(o, MergeOutcome::AdoptedExisting { .. }));
        if all_adopted {
            git::clean_fd(&deps.target, deps.config.git_timeout).await;
            return Err(IntegrationError::ValidationFailedAfterAdopt {
                command: val_err.command,
                exit_code: val_err.exit_code,
                log: val_err.log,
            });
        }

        // At least one fresh NewMerge: roll back to pre_merge (which
        // precedes this call's merges) and fail the Bundles terminally.
        git::reset_hard(&deps.target, &pre_merge, deps.config.git_timeout).await?;
        git::clean_fd(&deps.target, deps.config.git_timeout).await;
        return fail_all_without_reset(
            &bundle_states,
            deps,
            IntegrationError::ValidationFailed {
                command: val_err.command,
                exit_code: val_err.exit_code,
                log: val_err.log,
            },
        )
        .await;
    }

    // Validation passed (or was skipped). Remove any untracked build
    // artifacts validation produced so the operator-visible tree stays
    // clean on the SUCCESS path too (previously clean_fd ran only on
    // failure, leaving build output behind on every successful Tick).
    // `git clean -fd` leaves ignored paths (`.loopr/`) untouched, so the
    // Store's files survive. The entry-time dirty-tree guard guarantees
    // anything untracked here was produced by this integrate.
    git::clean_fd(&deps.target, deps.config.git_timeout).await;

    let sha = git::rev_parse_head(&deps.target, deps.config.git_timeout).await?;

    // Phase 4: commit (batched store writes)
    span.record("phase", "commit");
    info!(
        phase = "commit",
        merged_count = outcomes.len(),
        head_sha = %sha,
        "integrator: phase begin"
    );
    // Tick first. On `DuplicateTick`, adopt the existing Tick (prior
    // crashed call wrote it) and continue to the Merged transitions.
    let tick = Tick::new(
        plan.id.clone(),
        outcomes.iter().map(|(id, _)| id.clone()).collect(),
        integ_branch,
        sha,
        outcomes.iter().map(|(_, o)| o.sha()).collect(),
    );
    let tick = match deps.ticks.create(tick).await {
        Ok(t) => t,
        Err(StoreError::DuplicateTick { tick_id, .. }) => {
            // Crash-recovery case (a): a prior call wrote this Tick
            // before dying. Resolve the existing Tick via
            // `ticks.get(tick_id)` and continue to the Merged
            // transitions. The design doc's crash-recovery invariant
            // mandates `Ok(existing_tick)` rather than bubbling the
            // DuplicateTick error.
            match deps.ticks.get(&tick_id).await? {
                Some(existing) => existing,
                None => {
                    // DuplicateTick promised an id that TicksStore no
                    // longer resolves: store corruption or race. Bubble
                    // as Git/Store error; the daemon's worktree-crash-
                    // recovery pass at restart owns diagnosis.
                    return Err(IntegrationError::Store(StoreError::RecordNotFound {
                        collection: "ticks",
                        id: tick_id.to_string(),
                    }));
                }
            }
        }
        Err(other) => return Err(IntegrationError::Store(other)),
    };

    // Transition every successfully-merged Bundle to Merged.
    // Sources from `bundle_states` (the post-prologue Bundles), not the
    // input slice, so OCC expected_updated_at matches the on-disk state.
    // Bundles already Merged (a partial-crash re-entry finished them in
    // a prior call) are skipped - re-transitioning Merged->Merged is a
    // redundant write.
    for (bundle_id, _) in &outcomes {
        let bundle = bundle_states
            .iter()
            .find(|b| &b.id == bundle_id)
            .expect("outcome refers to a Bundle from the input slice");
        if bundle.status == BundleStatus::Merged {
            continue;
        }
        transition_bundle(&deps.bundle_sink, bundle, BundleStatus::Merged).await?;
    }

    // Branch cleanup (strictly after the Tick + Merged writes are
    // durable): delete each merged Bundle branch. The commits live on
    // the integration branch now, so the per-attempt `loopr/wk-*` branch
    // is dead clutter (vision.md: the integrator deletes them post-Tick).
    // Best-effort - a failed delete (e.g. branch still checked out in a
    // lingering worktree) must not fail an otherwise-successful Tick.
    for (bundle_id, _) in &outcomes {
        let branch = match bundle_states.iter().find(|b| &b.id == bundle_id) {
            Some(b) => b.branch_name.clone(),
            None => continue,
        };
        let repo = deps.target.clone();
        let branch_for_log = branch.clone();
        match tokio::task::spawn_blocking(move || worktree::delete_branch(&repo, &branch)).await {
            Ok(Ok(())) => debug!(branch = %branch_for_log, "integrator: deleted merged bundle branch"),
            Ok(Err(e)) => {
                warn!(branch = %branch_for_log, error = %e, "integrator: bundle branch delete failed (best-effort)")
            }
            Err(join) => {
                warn!(branch = %branch_for_log, error = %join, "integrator: branch-delete task panicked (best-effort)")
            }
        }
    }

    Ok(tick)
}

// ---------------------------------------------------------------------------
// Pre-flight helpers
// ---------------------------------------------------------------------------

fn preflight_shape(bundles: &[Bundle], allow_multi_bundle: bool) -> Result<(), IntegrationError> {
    if bundles.is_empty() {
        return Err(IntegrationError::NoBundles);
    }
    if bundles.len() > 1 && !allow_multi_bundle {
        return Err(IntegrationError::MultiBundleNotSupported { count: bundles.len() });
    }
    Ok(())
}

fn preflight_status(bundles: &[Bundle]) -> Result<(), IntegrationError> {
    for b in bundles {
        match b.status {
            // Accepted: fresh integration. Integrating: crash-recovery
            // re-entry. Merged: a prior multi-Bundle integrate crashed
            // PART-way through the Phase-4 Merged loop, leaving a mixed
            // slice; tolerating Merged here lets the driver re-enter with
            // the FULL original slice (required so the Tick's bundle-set
            // key still matches the prior Tick and DuplicateTick adopts
            // it rather than double-writing).
            BundleStatus::Accepted | BundleStatus::Integrating | BundleStatus::Merged => {}
            other => {
                return Err(IntegrationError::BundleNotAccepted {
                    bundle_id: b.id.as_ref().to_string(),
                    current: other,
                });
            }
        }
    }
    Ok(())
}

#[instrument(
    name = "integrator.preflight_plan_consistency",
    level = "debug",
    skip_all,
    fields(plan_id = %plan.id, bundle_count = bundles.len()),
    err,
)]
async fn preflight_plan_consistency<W: WorkLookup>(
    bundles: &[Bundle],
    plan: &Plan,
    works: &W,
) -> Result<(), IntegrationError> {
    for b in bundles {
        let work = works
            .get(&b.work_id)
            .await?
            .ok_or_else(|| IntegrationError::WorkNotFound {
                bundle_id: b.id.as_ref().to_string(),
                work_id: b.work_id.as_ref().to_string(),
            })?;
        if work.parent_id != plan.id {
            return Err(IntegrationError::PlanBundleMismatch {
                bundle_id: b.id.as_ref().to_string(),
                work_plan_id: work.parent_id.as_ref().to_string(),
                plan_id: plan.id.as_ref().to_string(),
            });
        }
    }
    debug!(bundle_count = bundles.len(), "integrator: preflight ok");
    Ok(())
}

// ---------------------------------------------------------------------------
// Transition + rollback helpers
// ---------------------------------------------------------------------------

async fn transition_bundle<U: BundleUpdateSink>(
    sink: &U,
    bundle: &Bundle,
    target: BundleStatus,
) -> Result<(), IntegrationError> {
    let _ = transition_bundle_returning(sink, bundle, target).await?;
    Ok(())
}

/// Transition a Bundle and return the mutated clone with its fresh
/// `updated_at`. Used by the Phase 2 prologue so subsequent Phase 3
/// writes have the correct OCC expected-version.
#[instrument(
    name = "integrator.transition_bundle",
    level = "debug",
    skip_all,
    fields(
        bundle_id = %bundle.id,
        work_id = %bundle.work_id,
        from = ?bundle.status,
        target_status = ?target,
        expected_updated_at = bundle.updated_at,
    ),
    err,
)]
async fn transition_bundle_returning<U: BundleUpdateSink>(
    sink: &U,
    bundle: &Bundle,
    target: BundleStatus,
) -> Result<Bundle, IntegrationError> {
    let mut clone = bundle.clone();
    let expected = clone.updated_at;
    clone
        .transition(target, Role::Integrator)
        .map_err(|e| IntegrationError::Transition(e.to_string()))?;
    // Sync the in-memory clone to the persisted (monotonically-floored)
    // `updated_at` so a chained next transition (prologue Accepted ->
    // Integrating, then Phase-3 Integrating -> Merged) carries the
    // correct OCC expected-version even when both writes land in the
    // same millisecond.
    let persisted = sink.update(clone.clone(), expected).await?;
    clone.updated_at = persisted;
    debug!(
        bundle_id = %clone.id,
        from = ?bundle.status,
        target_status = ?target,
        "integrator: bundle transitioned"
    );
    Ok(clone)
}

/// On git-sequence failure: roll git back to `pre_merge`, then
/// transition every Bundle in the slice to `IntegrationFailed` in one
/// batch. A failure inside `reset_hard` is fatal and bubbles.
#[instrument(
    name = "integrator.fail_all",
    level = "warn",
    skip_all,
    fields(bundle_count = bundles.len(), pre_merge_sha = pre_merge, error = %err),
)]
async fn fail_all<U, W, T>(
    bundles: &[Bundle],
    pre_merge: &str,
    deps: &IntegratorDeps<U, W, T>,
    err: IntegrationError,
) -> Result<Tick, IntegrationError>
where
    U: BundleUpdateSink,
    W: WorkLookup,
    T: TickSink,
{
    warn!(
        bundle_count = bundles.len(),
        pre_merge_sha = pre_merge,
        error = %err,
        "integrator: failing all bundles (with reset)"
    );
    git::merge_abort(&deps.target, deps.config.git_timeout).await;
    git::reset_hard(&deps.target, pre_merge, deps.config.git_timeout).await?;
    fail_all_without_reset(bundles, deps, err).await
}

/// Variant used inside the merge-failure arm: the merge path already
/// called `merge_abort` + `reset_hard` before invoking this.
#[instrument(
    name = "integrator.fail_all_without_reset",
    level = "warn",
    skip_all,
    fields(bundle_count = bundles.len(), error = %err),
)]
async fn fail_all_without_reset<U, W, T>(
    bundles: &[Bundle],
    deps: &IntegratorDeps<U, W, T>,
    err: IntegrationError,
) -> Result<Tick, IntegrationError>
where
    U: BundleUpdateSink,
    W: WorkLookup,
    T: TickSink,
{
    warn!(
        bundle_count = bundles.len(),
        error = %err,
        "integrator: failing all bundles"
    );
    for b in bundles {
        // `transition` is a no-op if the Bundle is already
        // IntegrationFailed (FSM returns `Unchanged`). Bundles that
        // were `Integrating` on entry transition cleanly; Bundles
        // that were `Accepted` on entry also have a valid transition.
        // Swallow transition errors here: we're on the error path
        // already and the primary `err` is more informative.
        let _ = transition_bundle(&deps.bundle_sink, b, BundleStatus::IntegrationFailed).await;
    }
    Err(err)
}

#[cfg(test)]
mod tests;
