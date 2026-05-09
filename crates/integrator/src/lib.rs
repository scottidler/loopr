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
use tracing::instrument;

use domain::{Bundle, BundleId, BundleStatus, Plan, Role, Tick, TickId, Work};
use store::{BundleUpdateSink, StoreError};

pub use config::IntegratorConfig;
pub use error::IntegrationError;

use crate::classify::{ConflictKind, classify_conflict};

// ---------------------------------------------------------------------------
// DI traits: the `IntegratorDeps` struct bundles these.
// ---------------------------------------------------------------------------

/// Read-only `Work` lookup. The Integrator fetches Work records by
/// `bundle.work_id` to verify `work.parent_id == plan.id` during
/// pre-flight. Read-only; the Integrator never transitions a Work
/// (that's the daemon's job after an `Ok(Tick)` return).
#[trait_variant::make(Send)]
pub trait WorkLookup: Send + Sync {
    async fn get(&self, work_id: &str) -> Result<Option<Work>, StoreError>;
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
    async fn get(&self, work_id: &str) -> Result<Option<Work>, StoreError> {
        // `WorksStore::get` returns `Err(RecordNotFound)` for missing;
        // the WorkLookup contract is `Option` so the Integrator can
        // distinguish "wiring bug (bundle references a non-existent
        // work)" from other store failures.
        use domain::WorkId;
        use std::str::FromStr;
        let wid = WorkId::from_str(work_id).expect("WorkId::from_str is Infallible");
        match self.works().get(&wid).await {
            Ok(w) => Ok(Some(w)),
            Err(StoreError::RecordNotFound { .. }) => Ok(None),
            Err(other) => Err(other),
        }
    }
}

impl<T: WorkLookup + ?Sized> WorkLookup for &T {
    async fn get(&self, work_id: &str) -> Result<Option<Work>, StoreError> {
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
    // Phase 1: pre-flight
    preflight_shape(bundles, deps.config.allow_multi_bundle)?;
    preflight_status(bundles)?;
    preflight_plan_consistency(bundles, plan, &deps.works).await?;

    let integ_branch = format!("loopr/plan-{}", plan.id);
    span.record("integration_branch", integ_branch.as_str());
    if !git::verify_branch(&deps.target, &integ_branch, deps.config.git_timeout).await? {
        return Err(IntegrationError::IntegrationBranchMissing { branch: integ_branch });
    }

    // Phase 2: git sequence (serialize against the working tree)
    span.record("phase", "git_sequence");
    let _git_guard = deps.git_lock.lock().await;

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
            _ => unreachable!("pre-flight rejects non-Accepted/non-Integrating bundles"),
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
            BundleStatus::Integrating => {
                // Crash-recovery re-entry: a prior call may have
                // already merged this Bundle's head_commit. Ancestry
                // check first, so an already-merged branch adopts
                // cleanly instead of falsely tripping EmptyBranch
                // (a merged branch has `merge-base HEAD branch == branch`).
                let already_merged =
                    git::is_ancestor(&deps.target, head_commit, "HEAD", deps.config.git_timeout).await?;
                if already_merged {
                    let sha = git::merge_commit_sha_for(&deps.target, head_commit, deps.config.git_timeout).await?;
                    outcomes.push((b.id.clone(), MergeOutcome::AdoptedExisting { sha }));
                    continue;
                }
                // Branch not merged; check for empty branch. Safe to
                // use the merge-base check here because we just ruled
                // out already-merged.
                if let Err(err) =
                    git::assert_nontrivial_branch(&deps.target, b.id.as_ref(), &b.branch_name, deps.config.git_timeout)
                        .await
                {
                    return fail_all(&bundle_states, &pre_merge, deps, err).await;
                }
            }
            _ => unreachable!("pre-flight rejects non-Accepted/non-Integrating bundles"),
        }

        match git::merge_no_ff(&deps.target, &b.branch_name, deps.config.git_timeout).await? {
            Ok(sha) => {
                outcomes.push((b.id.clone(), MergeOutcome::NewMerge { sha }));
            }
            Err(stderr) => {
                git::merge_abort(&deps.target, deps.config.git_timeout).await;
                git::reset_hard(&deps.target, &pre_merge, deps.config.git_timeout).await?;
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
                        stderr,
                    },
                };
                // After rollback: every Integrating Bundle in the
                // slice transitions to IntegrationFailed in one batch.
                return fail_all_without_reset(&bundle_states, deps, err).await;
            }
        }
    }

    // Phase 3: validation (skipped when validation_commands is empty)
    span.record("phase", "validation");
    if !deps.config.validation_commands.is_empty()
        && let Err(val_err) = validation::run_validation(
            &deps.config.validation_commands,
            deps.config.validation_timeout,
            &deps.target,
        )
        .await
    {
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

    let sha = git::rev_parse_head(&deps.target, deps.config.git_timeout).await?;

    // Phase 4: commit (batched store writes)
    span.record("phase", "commit");
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
    // Sources from `bundle_states` (the post-prologue Integrating
    // Bundles), not the input slice, so OCC expected_updated_at
    // matches the on-disk state.
    for (bundle_id, _) in &outcomes {
        let bundle = bundle_states
            .iter()
            .find(|b| &b.id == bundle_id)
            .expect("outcome refers to a Bundle from the input slice");
        transition_bundle(&deps.bundle_sink, bundle, BundleStatus::Merged).await?;
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
            BundleStatus::Accepted | BundleStatus::Integrating => {}
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
            .get(b.work_id.as_ref())
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
    sink.update(clone.clone(), expected).await?;
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
