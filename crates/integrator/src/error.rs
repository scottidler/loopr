//! `IntegrationError`: the typed failure surface of `integrate`.
//!
//! Variants are grouped by pipeline position:
//! - pre-flight: NoBundles, MultiBundleNotSupported, BundleNotAccepted,
//!   WorkNotFound, PlanBundleMismatch, IntegrationBranchMissing
//! - git sequence: EmptyBranch, ConflictStructural, ConflictRetryable, Git
//! - commit: Store, Update, Transition
//! - wrappers: Io
//!
//! The Stage 8 wiring capstone MUST retry on `Update(Stale)` by re-
//! enqueueing the `Integrating` Bundle (see "Wiring retry contract for
//! Integrating" in docs/design/2026-04-22-integrator.md).

use domain::BundleStatus;
use store::{BundleUpdateError, StoreError};

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
    PlanBundleMismatch {
        bundle_id: String,
        work_plan_id: String,
        plan_id: String,
    },

    #[error("integration branch {branch} does not exist")]
    IntegrationBranchMissing { branch: String },

    #[error("bundle branch {branch} (bundle {bundle_id}) has no commits beyond merge base")]
    EmptyBranch { bundle_id: String, branch: String },

    #[error(
        "structural merge conflict for bundle {bundle_id}: paths {files:?} overlap with peer bundles {peer_bundle_ids:?}"
    )]
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
