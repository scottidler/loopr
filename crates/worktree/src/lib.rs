//! Per-attempt git worktree lifecycle. Infrastructure-only.
//!
//! Exposes the `Worktree` RAII handle, the `AttemptCleanupPolicy` enum + its
//! `WorktreeConfig` wrapper, and a set of free functions consumed by
//! `loopr::daemon::startup::reconcile`. This crate does **not** depend on
//! `store`, does not own a registry file, and does not perform the crash-
//! recovery join itself — that's the `loopr` binary's job. See
//! `docs/design/2026-04-21-worktree-lifecycle.md`.

mod config;
mod error;
mod handle;
mod info;
mod ops;

pub use config::{AttemptCleanupPolicy, WorktreeConfig};
pub use error::WorktreeError;
pub use handle::Worktree;
pub use info::WorktreeInfo;
