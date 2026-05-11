#![allow(clippy::unwrap_used)]
//! Unit tests for `DirectorMode` and the `next_mode` transition
//! function (Phase 5 of `docs/design/2026-05-09-director-phase-2.md`).
//! Also verifies the cache-locality property of `system.pmt`: no
//! `{{mode}}` Handlebars variables, so the rendered system prompt is
//! byte-stable across iterations regardless of the Director's mode.

use super::super::pattern::PatternObservation;
use super::{DirectorMode, next_mode};

// ---------------------------------------------------------------------------
// Transition matrix
// ---------------------------------------------------------------------------

#[test]
fn normal_to_conservative_on_same_action() {
    let obs = PatternObservation::SameActionTripped {
        kind: "override_work",
        count: 3,
    };
    assert_eq!(next_mode(DirectorMode::Normal, &obs), DirectorMode::Conservative);
}

#[test]
fn normal_to_conservative_on_no_progress() {
    let obs = PatternObservation::NoProgressTripped {
        distinct: 1,
        max_recurrence: 5,
        streak: 1,
    };
    assert_eq!(next_mode(DirectorMode::Normal, &obs), DirectorMode::Conservative);
}

#[test]
fn conservative_to_needs_operator_on_escalation() {
    let obs = PatternObservation::EscalationTripped {
        reason: "no_progress_sustained",
        streak: 8,
    };
    assert_eq!(next_mode(DirectorMode::Conservative, &obs), DirectorMode::NeedsOperator);
}

#[test]
fn conservative_stays_on_repeated_no_progress() {
    // Reobserving NoProgressTripped while already Conservative keeps
    // mode Conservative; the streak inside the observation drives
    // EscalationTripped at the tracker's escalation_threshold.
    let obs = PatternObservation::NoProgressTripped {
        distinct: 1,
        max_recurrence: 5,
        streak: 3,
    };
    assert_eq!(next_mode(DirectorMode::Conservative, &obs), DirectorMode::Conservative);
}

#[test]
fn conservative_stays_on_same_action() {
    let obs = PatternObservation::SameActionTripped {
        kind: "override_work",
        count: 5,
    };
    assert_eq!(next_mode(DirectorMode::Conservative, &obs), DirectorMode::Conservative);
}

#[test]
fn any_mode_to_normal_on_recovered() {
    let obs = PatternObservation::Recovered;
    assert_eq!(next_mode(DirectorMode::Normal, &obs), DirectorMode::Normal);
    assert_eq!(next_mode(DirectorMode::Conservative, &obs), DirectorMode::Normal);
    assert_eq!(next_mode(DirectorMode::NeedsOperator, &obs), DirectorMode::Normal);
}

#[test]
fn conservative_to_normal_requires_recovered_not_absence_of_trip() {
    // The design doc rejects time-based auto-revert. Re-observing
    // (anything other than Recovered) while Conservative must NOT
    // demote to Normal. The pattern tracker emits None when no signal
    // fires; `next_mode` is only called when a signal fires (Phase 6
    // consumer skips the call otherwise), but we test the in-flight
    // observation variants for completeness.
    let trips = [
        PatternObservation::SameActionTripped {
            kind: "accept_bundle",
            count: 3,
        },
        PatternObservation::NoProgressTripped {
            distinct: 2,
            max_recurrence: 4,
            streak: 2,
        },
    ];
    for obs in &trips {
        assert_eq!(
            next_mode(DirectorMode::Conservative, obs),
            DirectorMode::Conservative,
            "Conservative must not demote on {obs:?}"
        );
    }
}

#[test]
fn needs_operator_stays_on_non_recovered_observations() {
    let trips = [
        PatternObservation::SameActionTripped {
            kind: "override_work",
            count: 4,
        },
        PatternObservation::NoProgressTripped {
            distinct: 1,
            max_recurrence: 6,
            streak: 9,
        },
        PatternObservation::EscalationTripped {
            reason: "no_progress_sustained",
            streak: 10,
        },
    ];
    for obs in &trips {
        assert_eq!(
            next_mode(DirectorMode::NeedsOperator, obs),
            DirectorMode::NeedsOperator,
            "NeedsOperator must not demote on {obs:?}"
        );
    }
}

#[test]
fn any_mode_to_normal_on_operator_note_arrived() {
    // Operator engagement (Phase 9 chat) demotes Conservative and
    // NeedsOperator back to Normal so the Director's prompt reverts to
    // the standard block on the very next iteration. Normal +
    // OperatorNoteArrived is the idempotent edge.
    let obs = PatternObservation::OperatorNoteArrived;
    assert_eq!(next_mode(DirectorMode::Normal, &obs), DirectorMode::Normal);
    assert_eq!(next_mode(DirectorMode::Conservative, &obs), DirectorMode::Normal);
    assert_eq!(next_mode(DirectorMode::NeedsOperator, &obs), DirectorMode::Normal);
}

#[test]
fn mode_as_str_returns_pascal_case() {
    assert_eq!(DirectorMode::Normal.as_str(), "Normal");
    assert_eq!(DirectorMode::Conservative.as_str(), "Conservative");
    assert_eq!(DirectorMode::NeedsOperator.as_str(), "NeedsOperator");
}

#[test]
fn director_mode_defaults_to_normal() {
    assert_eq!(DirectorMode::default(), DirectorMode::Normal);
}

// ---------------------------------------------------------------------------
// System prompt cache-locality regression guard
//
// The Anthropic ephemeral cache hits when the system prompt is
// byte-stable across iterations. Phase 5's mode-aware behavior is
// driven by a FIXED `## Mode-Aware Recovery` section the LLM reads
// against the user-message mode label (Phase 6). The system prompt
// template MUST NOT interpolate the mode — a `{{mode}}` Handlebars
// variable would invalidate the cache on every mode transition.
// ---------------------------------------------------------------------------

const SYSTEM_PMT: &str = include_str!("../../../../context/prompts/agents/director/system.pmt");

#[test]
fn system_prompt_has_no_mode_interpolation() {
    assert!(
        !SYSTEM_PMT.contains("{{mode}}"),
        "system.pmt must not interpolate {{{{mode}}}} — that would invalidate the Anthropic prompt cache on every mode transition"
    );
    assert!(
        !SYSTEM_PMT.contains("{{state_mode}}"),
        "system.pmt must not interpolate {{{{state_mode}}}} — same cache hazard"
    );
}

#[test]
fn system_prompt_contains_mode_aware_recovery_section() {
    assert!(
        SYSTEM_PMT.contains("## Mode-Aware Recovery"),
        "system.pmt must include the Mode-Aware Recovery section so the LLM can read mode-specific guidance"
    );
    for mode in [
        DirectorMode::Normal,
        DirectorMode::Conservative,
        DirectorMode::NeedsOperator,
    ] {
        let header = format!("### {} mode", mode.as_str());
        assert!(
            SYSTEM_PMT.contains(&header),
            "system.pmt must include `{header}` subsection so the LLM can match the user-prompt label"
        );
    }
}
