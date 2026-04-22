//! Agent-role configs.
//!
//! `ImplementerConfig` (Stage 7) and `ReviewerConfig` (Stage 8) live
//! side by side; `AgentsConfig` composes both. Flat knob bags, no
//! trait, no nested substructs. Keys on disk are kebab-case (see
//! `#[serde(rename_all = "kebab-case")]`) per the project naming
//! convention; Rust field names remain snake_case.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct ImplementerConfig {
    /// Hard upper bound on outer loop iterations per Work. Hitting
    /// this triggers the force-propose path (or its guard-escalate
    /// alternative).
    pub max_iterations: u32,

    /// Maximum LLM re-prompts within the self-correction sub-loop
    /// for a single iteration.
    pub max_requeries: u32,

    /// Consecutive full-iteration parse failures before the Lifeguard
    /// escalates. Distinct from `max_iterations`: this fires only
    /// when every requery in an iteration also fails.
    pub max_parse_failures: u32,

    /// Consecutive identical actions (structurally canonical hash)
    /// before the Lifeguard escalates the action-repeat path.
    pub max_repeat_action: u32,

    /// Force-propose guard: if more than this many tracked files are
    /// modified at iteration cap, escalate instead of committing.
    pub max_force_propose_files: u32,

    /// Force-propose guard: if any single staged file exceeds this
    /// size in bytes at iteration cap, escalate instead of committing.
    pub max_force_propose_file_size_bytes: u64,
}

impl Default for ImplementerConfig {
    fn default() -> Self {
        Self {
            max_iterations: 20,
            max_requeries: 3,
            max_parse_failures: 5,
            max_repeat_action: 3,
            max_force_propose_files: 100,
            max_force_propose_file_size_bytes: 10 * 1024 * 1024,
        }
    }
}

/// Knob bag for the Reviewer agent. One LLM turn per invocation with
/// a bounded parse-retry sub-loop; no outer iterations, so there is
/// no `max_iterations` analog.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct ReviewerConfig {
    /// Maximum LLM re-prompts within the parse-retry sub-loop for a
    /// single `run_reviewer` invocation. Strict-greater-than check:
    /// the first call is "free"; the (N+1)th parse failure triggers
    /// escalation. Default 3 -> up to 4 total completions.
    pub max_requeries: u32,
    /// Maximum aggregate bytes of diff content rendered into the user
    /// message. Larger diffs are truncated with an explicit marker;
    /// the prompt guides the LLM to prefer `change_requested` when
    /// the visible portion alone doesn't demonstrate the AC are met.
    pub diff_byte_cap: usize,
    /// Maximum aggregate bytes of file-content rendered for noop
    /// bundles across ALL files in `bundle.paths`. Per-file cap is
    /// `noop_files_byte_cap / paths.len().max(1)`, floor 2048.
    pub noop_files_byte_cap: usize,
}

impl Default for ReviewerConfig {
    fn default() -> Self {
        Self {
            max_requeries: 3,
            diff_byte_cap: 64 * 1024,
            noop_files_byte_cap: 64 * 1024,
        }
    }
}

/// Composes every role-level agent config. The top-level loopr
/// `Config` embeds `agents: AgentsConfig`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct AgentsConfig {
    pub implementer: ImplementerConfig,
    pub reviewer: ReviewerConfig,
}
