//! Director escalation mode: in-memory FSM the Director task uses to
//! adapt strategy as the pattern tracker observes macro-pathologies.
//!
//! Phase 5 of `docs/design/2026-05-09-director-phase-2.md`. Mode is
//! task-local; a Director restart reverts to `Normal` and the pattern
//! tracker re-warms from a fresh history. The system prompt has a
//! fixed `## Mode-Aware Recovery` section listing guidance for each
//! mode; the Phase 6 user-prompt label tells the LLM which block to
//! apply. The system prompt is byte-stable across iterations
//! regardless of mode (cache-locality rule from `agents/CLAUDE.md`).

use serde::{Deserialize, Serialize};

use super::pattern::PatternObservation;

/// Director's adaptive strategy mode. Drives a label in the user
/// prompt that the LLM reads to apply the matching block from the
/// system prompt's `## Mode-Aware Recovery` section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum DirectorMode {
    /// Default. Standard prompt, standard retry cap. Pattern tracker
    /// is observing; no escalation has fired yet.
    #[default]
    Normal,
    /// Pattern tracker fired `SameActionTripped` or `NoProgressTripped`.
    /// Prompt nudges the LLM to prefer `done` over `override_work` and
    /// to emit `need_help` sooner. The Phase 1 follow-ups'
    /// `max_work_attempts` cap is unchanged — this is a bias, not a
    /// hard limit.
    Conservative,
    /// Pattern tracker fired `EscalationTripped` (sustained
    /// no-progress) OR sustained `NoProgressTripped`. Prompt instructs
    /// the LLM to emit `need_help` unless an operator note arrived
    /// this iteration (Phase 9). Phase 10's
    /// `needs_operator_grace_iters` countdown begins; if the mode
    /// persists without a note, the Director transitions the Plan to
    /// `Stalled`.
    NeedsOperator,
}

impl DirectorMode {
    /// Stable PascalCase string for the user-prompt label and event
    /// fields. Phase 6 renders this into the user message.
    pub fn as_str(self) -> &'static str {
        match self {
            DirectorMode::Normal => "Normal",
            DirectorMode::Conservative => "Conservative",
            DirectorMode::NeedsOperator => "NeedsOperator",
        }
    }
}

/// Pure mode-transition function. The Phase 4 pattern tracker emits
/// a `PatternObservation` per iteration; this function maps the
/// (current mode, observation) pair to the next mode. No I/O, no
/// `self`, exhaustively tested in `mode/tests.rs`.
///
/// Transition rules:
/// - `Normal -> Conservative` on first sign of trouble
///   (`SameActionTripped` or `NoProgressTripped`).
/// - `Conservative -> NeedsOperator` only on `EscalationTripped`.
///   Re-observing `NoProgressTripped` keeps mode Conservative; the
///   streak embedded in the observation drives `EscalationTripped`
///   when the tracker's `escalation_threshold` is reached.
/// - Any mode -> `Normal` on `Recovered`. Recovery from
///   `Conservative` and `NeedsOperator` is the SOLE demotion path; no
///   time-based auto-revert (Architect Round 1 rejection).
/// - All other (mode, observation) pairs are sticky.
pub fn next_mode(current: DirectorMode, obs: &PatternObservation) -> DirectorMode {
    use DirectorMode::*;
    use PatternObservation::*;
    match (current, obs) {
        (Normal, SameActionTripped { .. }) => Conservative,
        (Normal, NoProgressTripped { .. }) => Conservative,
        (Conservative, EscalationTripped { .. }) => NeedsOperator,
        (_, Recovered) => Normal,
        (mode, _) => mode,
    }
}

#[cfg(test)]
mod tests;
