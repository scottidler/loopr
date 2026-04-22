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

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use domain::{Bundle, BundleId, BundleStatus, Plan, Role, Tick, Work};
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
pub trait WorkLookup: Send + Sync {
    fn get<'a>(&'a self, work_id: &'a str) -> impl Future<Output = Result<Option<Work>, StoreError>> + Send + 'a;
}

/// Append-only `Tick` persistence. The Integrator calls `create` in
/// Phase 3 after the git sequence succeeds. On a duplicate
/// `(plan_id, bundles-as-set)`, the store returns
/// `StoreError::DuplicateTick { tick_id, .. }` and the Integrator
/// promotes to a no-op on the crash-recovery path.
pub trait TickSink: Send + Sync {
    fn create<'a>(&'a self, tick: Tick) -> impl Future<Output = Result<Tick, StoreError>> + Send + 'a;
}

// Real impls backed by `store::Store`.

impl WorkLookup for store::Store {
    #[allow(clippy::manual_async_fn)]
    fn get<'a>(&'a self, work_id: &'a str) -> impl Future<Output = Result<Option<Work>, StoreError>> + Send + 'a {
        async move {
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
}

impl<T: WorkLookup + ?Sized> WorkLookup for &T {
    #[allow(clippy::manual_async_fn)]
    fn get<'a>(&'a self, work_id: &'a str) -> impl Future<Output = Result<Option<Work>, StoreError>> + Send + 'a {
        async move { (*self).get(work_id).await }
    }
}

impl TickSink for store::Store {
    #[allow(clippy::manual_async_fn)]
    fn create<'a>(&'a self, tick: Tick) -> impl Future<Output = Result<Tick, StoreError>> + Send + 'a {
        async move { self.ticks().create(tick).await }
    }
}

impl<T: TickSink + ?Sized> TickSink for &T {
    #[allow(clippy::manual_async_fn)]
    fn create<'a>(&'a self, tick: Tick) -> impl Future<Output = Result<Tick, StoreError>> + Send + 'a {
        async move { (*self).create(tick).await }
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
/// Three phases:
/// 1. Pre-flight (no mutation): shape + status + plan + branch.
/// 2. Git sequence under `git_lock`: in-memory `outcomes` only; no
///    store writes.
/// 3. Commit: write Tick first, then transition every Bundle to
///    `Merged`. Store and git either both advance or neither does.
///
/// Bundles in `BundleStatus::Integrating` are accepted as a
/// crash-recovery re-entry point: the idempotency check
/// (`git merge-base --is-ancestor`) adopts an existing merge commit
/// if the prior call succeeded before the crash, and falls through
/// to the normal merge path otherwise.
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
    // Phase 1: pre-flight
    preflight_shape(bundles, deps.config.allow_multi_bundle)?;
    preflight_status(bundles)?;
    preflight_plan_consistency(bundles, plan, &deps.works).await?;

    let integ_branch = format!("loopr/plan-{}", plan.id);
    if !git::verify_branch(&deps.target, &integ_branch, deps.config.git_timeout).await? {
        return Err(IntegrationError::IntegrationBranchMissing { branch: integ_branch });
    }

    // Phase 2: git sequence (serialize against the working tree)
    let _git_guard = deps.git_lock.lock().await;

    git::checkout(&deps.target, &integ_branch, deps.config.git_timeout).await?;
    let pre_merge_sha = git::rev_parse_head(&deps.target, deps.config.git_timeout).await?;

    let mut outcomes: Vec<(BundleId, MergeOutcome)> = Vec::with_capacity(bundles.len());

    for b in bundles {
        // Empty-branch guard applies to both the normal merge path
        // and the crash-recovery adopt path: if the branch has no
        // commits, there is nothing to merge OR adopt.
        if let Err(err) =
            git::assert_nontrivial_branch(&deps.target, b.id.as_ref(), &b.branch_name, deps.config.git_timeout).await
        {
            return fail_all(bundles, &pre_merge_sha, deps, err).await;
        }

        match b.status {
            BundleStatus::Integrating => {
                // Crash-recovery: is this Bundle's head_commit already
                // an ancestor of the integration branch's HEAD?
                let head_commit = match b.head_commit.as_deref() {
                    Some(sha) => sha,
                    None => {
                        // Integrating bundle with no head_commit is a
                        // wiring bug; treat as Git error.
                        return fail_all(
                            bundles,
                            &pre_merge_sha,
                            deps,
                            IntegrationError::Git(format!("bundle {} is Integrating but has no head_commit", b.id)),
                        )
                        .await;
                    }
                };
                let already_merged =
                    git::is_ancestor(&deps.target, head_commit, "HEAD", deps.config.git_timeout).await?;
                if already_merged {
                    let sha = git::merge_commit_sha_for(&deps.target, head_commit, deps.config.git_timeout).await?;
                    outcomes.push((b.id.clone(), MergeOutcome::AdoptedExisting { sha }));
                    continue;
                }
                // Fall through to the normal merge path: the prior
                // call died before its merge completed.
            }
            BundleStatus::Accepted => {
                // Normal path below.
            }
            _ => unreachable!("pre-flight rejects non-Accepted/non-Integrating bundles"),
        }

        match git::merge_no_ff(&deps.target, &b.branch_name, deps.config.git_timeout).await? {
            Ok(sha) => {
                outcomes.push((b.id.clone(), MergeOutcome::NewMerge { sha }));
            }
            Err(stderr) => {
                git::merge_abort(&deps.target, deps.config.git_timeout).await;
                git::reset_hard(&deps.target, &pre_merge_sha, deps.config.git_timeout).await?;
                let kind = classify_conflict(b, bundles);
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
                // After rollback: every Bundle in the slice transitions
                // to IntegrationFailed in one batch (including Bundles
                // that were `Integrating` on entry - they transition
                // from Integrating to IntegrationFailed).
                return fail_all_without_reset(bundles, deps, err).await;
            }
        }
    }

    let integration_sha = git::rev_parse_head(&deps.target, deps.config.git_timeout).await?;

    // Phase 3: commit (batched store writes)
    // Tick first. On `DuplicateTick`, adopt the existing Tick (prior
    // crashed call wrote it) and continue to the Merged transitions.
    let tick = Tick::new(
        plan.id.clone(),
        outcomes.iter().map(|(id, _)| id.clone()).collect(),
        integ_branch,
        integration_sha,
        outcomes.iter().map(|(_, o)| o.sha()).collect(),
    );
    let tick = match deps.ticks.create(tick).await {
        Ok(t) => t,
        Err(StoreError::DuplicateTick { tick_id, .. }) => {
            // Resolve via `ticks.get(tick_id)` - one extra indexed
            // lookup, payload carries the id so no scan required.
            // The `TickSink` trait does not expose `get`; the daemon
            // that owns `Store` can resolve through `store.ticks().get()`.
            // For `TickSink`-only deps, we return an error describing
            // the need; production always uses `store::Store`.
            return Err(IntegrationError::Store(StoreError::DuplicateTick {
                tick_id,
                plan_id: plan.id.clone(),
                bundles: outcomes.iter().map(|(id, _)| id.clone()).collect(),
            }));
        }
        Err(other) => return Err(IntegrationError::Store(other)),
    };

    // Transition every successfully-merged Bundle to Merged.
    for (bundle_id, _) in &outcomes {
        let bundle = bundles
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
    let mut clone = bundle.clone();
    let expected = clone.updated_at;
    clone
        .transition(target, Role::Integrator)
        .map_err(|e| IntegrationError::Transition(e.to_string()))?;
    sink.update(clone, expected).await.map_err(Into::into)
}

/// On git-sequence failure: roll git back to `pre_merge_sha`, then
/// transition every Bundle in the slice to `IntegrationFailed` in one
/// batch. A failure inside `reset_hard` is fatal and bubbles.
async fn fail_all<U, W, T>(
    bundles: &[Bundle],
    pre_merge_sha: &str,
    deps: &IntegratorDeps<U, W, T>,
    err: IntegrationError,
) -> Result<Tick, IntegrationError>
where
    U: BundleUpdateSink,
    W: WorkLookup,
    T: TickSink,
{
    git::merge_abort(&deps.target, deps.config.git_timeout).await;
    git::reset_hard(&deps.target, pre_merge_sha, deps.config.git_timeout).await?;
    fail_all_without_reset(bundles, deps, err).await
}

/// Variant used inside the merge-failure arm: the merge path already
/// called `merge_abort` + `reset_hard` before invoking this.
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
