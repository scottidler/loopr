//! Worktree cleanup policy + config composition.
//!
//! `AttemptCleanupPolicy` is the tunable; `WorktreeConfig` wraps it into the
//! shape expected by the top-level `loopr::Config` composition pattern. The
//! enum defines the variants; the **coordinator in `loopr`** is what actually
//! applies the policy (by deciding when to drop `Worktree` handles). The
//! `worktree` crate itself is oblivious to policy — `Drop` cleans whenever it
//! fires, full stop.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
#[clap(rename_all = "kebab-case")]
pub enum AttemptCleanupPolicy {
    /// Clean the worktree immediately when a Bundle is rejected.
    /// Minimum disk usage; no forensic artifact for failed attempts.
    Immediate,
    /// Keep rejected-attempt worktrees until the Work reaches Done/Abandoned,
    /// then sweep all prior attempts. DEFAULT.
    #[default]
    OnWorkTerminal,
    /// Keep all attempts (including successful) until the run completes,
    /// then sweep. Most disk; best forensics.
    OnRunEnd,
    /// Never clean automatically. Strict debug-only.
    ///
    /// The coordinator parks handles in a long-lived `Vec` to keep them alive,
    /// which leaks memory and file descriptors over multi-week daemon uptime.
    /// Changing the config mid-flight from `Never` to any other variant does
    /// NOT retroactively clean Works that already terminated under `Never` —
    /// cleanup fires only on the edge transition to terminal. Daemon restart
    /// is required to trigger `reconcile` and clear accumulated state.
    Never,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct WorktreeConfig {
    pub cleanup_policy: AttemptCleanupPolicy,
}

#[cfg(test)]
mod tests;
