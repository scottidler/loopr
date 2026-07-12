//! Agent-role configs.
//!
//! `ImplementerConfig` (Stage 7) and `ReviewerConfig` (Stage 8) live
//! side by side; `AgentsConfig` composes both. Flat knob bags, no
//! trait, no nested substructs. Keys on disk are kebab-case (see
//! `#[serde(rename_all = "kebab-case")]`) per the project naming
//! convention; Rust field names remain snake_case.

use serde::{Deserialize, Serialize};

use crate::director::PatternConfig;

/// Typed rejection for an invalid agent config value. Returned by the
/// `validate` methods so the caller (the daemon's config-load path) can fail
/// closed with a named cause instead of silently accepting a value that would
/// misbehave downstream (a negative/NaN cost cap saturates the `f64 -> u64`
/// cast to 0 and escalates every Work instantly).
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ConfigError {
    /// `per_work_cost_cap_usd` was present but not a non-negative finite
    /// number. Carries the offending value for the error message.
    #[error("per-work cost cap must be a non-negative finite dollar amount, got {0}")]
    InvalidCostCap(f64),
}

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

    /// GPG-sign agent commits. Default `false` appends `--no-gpg-sign`
    /// to every agent commit (the historical behavior); `true` omits it
    /// so git applies its configured signing (vision Git Posture). The
    /// `Loopr-*` commit trailers are emitted regardless of this flag.
    pub gpg_sign: bool,

    /// Per-Work cumulative LLM cost cap in U.S. dollars (vision Budgets).
    /// `None` = unlimited. The implementer accumulates its calls' cost
    /// across iterations and escalates the Work when it exceeds this cap.
    /// NOT a YAML knob on `agents.implementer` (`#[serde(skip)]`): the
    /// canonical surface is the top-level `budgets.per-work-cost-usd`,
    /// which `loopr` overlays onto this field at daemon-context build time
    /// so the budget config stays in one place.
    #[serde(skip)]
    pub per_work_cost_cap_usd: Option<f64>,
}

impl ImplementerConfig {
    /// Validate the overlaid budget knob at config-load time. `loopr` sets
    /// `per_work_cost_cap_usd` from `budgets.per-work-cost-usd` when it builds
    /// the daemon context; a negative or NaN cap is a config error, not a
    /// per-Work runtime surprise. `None` (unlimited) and any non-negative
    /// finite value pass.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if let Some(cap) = self.per_work_cost_cap_usd
            && (!cap.is_finite() || cap < 0.0)
        {
            return Err(ConfigError::InvalidCostCap(cap));
        }
        Ok(())
    }
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
            gpg_sign: false,
            per_work_cost_cap_usd: None,
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
    /// Executed check commands (Phase 10 of
    /// `docs/design/2026-07-11-verified-swarm.md`). Each is run in the
    /// Bundle's checkout BEFORE the LLM turn; a nonzero exit code-gates an
    /// LLM `Accept` down to `ChangeRequested`, and a spawn-level failure
    /// (command not found) Blocks the Work as an environment problem.
    /// Empty (the default) = checks skipped, verdict proceeds LLM-only.
    /// Opt-in per target: the universal merge gate is the Integrator's
    /// validation (Phase 12), not these.
    pub check_commands: Vec<String>,
}

impl Default for ReviewerConfig {
    fn default() -> Self {
        Self {
            max_requeries: 3,
            diff_byte_cap: 64 * 1024,
            noop_files_byte_cap: 64 * 1024,
            check_commands: Vec::new(),
        }
    }
}

/// Knob bag for the Director agent. Director Phase 1 (long-lived per-Plan
/// supervisor running on Opus) reads `poll_interval_secs`,
/// `idle_interval_secs`, `max_restarts`, `max_requeries`,
/// `max_parse_failures`, `model`, and `token_budget`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct DirectorConfig {
    /// Seconds between iterations when actions were taken.
    pub poll_interval_secs: u64,
    /// Seconds between iterations when no actions were taken.
    pub idle_interval_secs: u64,
    /// Max restarts on transient failure.
    pub max_restarts: u32,
    /// Maximum LLM re-prompts within the parse-retry sub-loop for a
    /// single iteration. Mirrors `ImplementerConfig.max_requeries`.
    /// Strict-greater-than check: the first call is "free"; the (N+1)th
    /// parse failure within one iteration triggers a `record_parse_failure`
    /// strike from the lifeguard.
    pub max_requeries: u32,
    /// Consecutive full-iteration parse failures before the lifeguard
    /// escalates. Mirrors `ImplementerConfig.max_parse_failures`. A
    /// strike fires only when every requery within an iteration also
    /// fails.
    pub max_parse_failures: u32,
    /// Anthropic model for Director calls.
    pub model: String,
    /// Token budget per LLM call (system + history + state summary).
    pub token_budget: usize,
    /// Cross-iteration cap on retries of a single Work. The Director's
    /// `Blocked -> Ready` retry path increments `Work.attempt_count`
    /// (Layer 1 in `transition_and_persist_work`); when
    /// `work.attempt_count >= max_work_attempts`, the soft cap (Layer 2)
    /// transitions the Plan to `Stalled` and exits the Director with
    /// `NeedHelp` instead of dispatching another retry. Default 3 to
    /// match the system prompt's "3 attempts" framing.
    pub max_work_attempts: u32,
    /// Grace window (seconds) gating reconcile-sweep stuck-state recovery
    /// against a record's `updated_at`. Bundles / Works whose current
    /// status is fresher than this window skip recovery; the window
    /// absorbs the spawn-chain race (sidecar map insert lands a few
    /// hundred ms after the FSM transition persists). Phase 2 of
    /// `docs/design/2026-05-09-director-phase-2.md`.
    pub reconcile_grace_secs: u64,
    /// Cross-iteration pattern tracker thresholds. Phase 4 of
    /// `docs/design/2026-05-09-director-phase-2.md`. The tracker watches
    /// for repeated actions, recurring state hashes, and sustained
    /// no-progress streaks; the consumer wires (mode FSM in Phase 5,
    /// user-prompt label in Phase 6) live in the Director loop.
    #[serde(default)]
    pub patterns: PatternConfig,
    /// Phase 10 grace counter: consecutive `NeedsOperator` iterations
    /// without an operator note before the Director transitions the
    /// Plan to `Stalled` and exits with `NeedHelp`. Default 5.
    /// `docs/design/2026-05-09-director-phase-2.md` Phase 10. Tunable
    /// via `agents.director.needs-operator-grace-iters`.
    pub needs_operator_grace_iters: u32,
    /// Absolute backstop on Director iterations per supervision session.
    /// The pattern tracker + NeedsOperator grace are the primary brakes;
    /// this is the hard cap so a stuck Plan cannot poll the LLM forever
    /// (the comment on the old `DirectorConfig` claimed a cap existed
    /// where none did). On exhaustion the Director transitions the Plan
    /// to `Stalled` and exits with `NeedHelp`. Counts per-session
    /// (resets on restart); the wall-clock budget is the time-based peer.
    /// Default 10_000 — generous enough for legitimately long Plans.
    pub max_iterations: u32,
    /// Absolute wall-clock backstop (seconds) on a Director supervision
    /// session, measured from session start. The time-based peer of
    /// `max_iterations` (which a `poll_interval_secs = 0` test config or
    /// a fast-idling Plan could blow through quickly). On exhaustion the
    /// Director transitions the Plan to `Stalled` and exits with
    /// `NeedHelp`. Default 86_400 (24h).
    pub max_wall_clock_secs: u64,
}

impl Default for DirectorConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: 5,
            idle_interval_secs: 15,
            max_restarts: 3,
            max_requeries: 3,
            max_parse_failures: 3,
            model: "claude-opus-4-7".to_string(),
            token_budget: 100_000,
            max_work_attempts: 3,
            reconcile_grace_secs: 30,
            patterns: PatternConfig::default(),
            needs_operator_grace_iters: 5,
            max_iterations: 10_000,
            max_wall_clock_secs: 86_400,
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
    pub director: DirectorConfig,
}

#[cfg(test)]
mod tests;
