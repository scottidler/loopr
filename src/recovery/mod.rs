//! Recovery module for crash recovery and resilience.
//!
//! This module provides:
//! - **Crash recovery**: Resume interrupted loops after daemon restart
//! - **Disk space management**: Monitor and enforce disk quotas
//! - **Orphaned worktree cleanup**: Clean up worktrees that lost their loops
//! - **Error recovery**: Graceful handling of failures

#![allow(dead_code)]
#![allow(unused_imports)]

mod crash;
mod disk;
mod orphan;

pub use crash::{RecoveryConfig, RecoveryManager, RecoveryResult, RecoveryStats, is_stale};
pub use disk::{DiskManager, DiskQuotaConfig, DiskUsage, DiskWarning, estimate_directory_size};
pub use orphan::{
    background_cleanup_task, cleanup_orphaned_worktrees, cleanup_stale_worktrees, find_orphaned_worktrees,
    find_stale_worktrees,
};
