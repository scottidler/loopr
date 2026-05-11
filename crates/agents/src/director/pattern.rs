//! Cross-iteration pattern tracker for the Director.
//!
//! Phase 4 of `docs/design/2026-05-09-director-phase-2.md`. The tracker
//! observes the Director's emitted actions and the Plan's `state_hash`
//! across iterations and fires when one of three macro-pathologies is
//! detected: a repeating action with no progress, a stuck state with
//! mutating actions, or sustained no-progress (escalation). It lives
//! on the Director task in-memory and resets on restart; the Phase 1
//! follow-ups' `attempt_count` retry budget is an independent per-Work
//! safety net.
//!
//! `ActionFingerprint` only fingerprints LLM-emitted actions
//! (`accept_bundle`, `override_work`, `assign_work`, `done`,
//! `need_help`). The daemon's reconcile-internal `spawn_reviewer` /
//! `spawn_integrator` / `recover_in_progress_work` calls are reactive
//! recovery effects, not Director decisions; they would falsely fire
//! the action-context gate if observed.

use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};

use domain::{Bundle, Work};

/// Knob bag controlling the pattern tracker's thresholds. Defaults are
/// the design doc's placeholders (3, 5, 8, 16); tune from real-Plan
/// traces.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct PatternConfig {
    /// Consecutive identical `ActionFingerprint`s before
    /// `SameActionTripped` fires. Default 3.
    pub same_action_threshold: u32,
    /// Minimum observations before the no-progress detector engages
    /// (and the smallest hash-history length the detector inspects).
    /// Default 5.
    pub no_progress_threshold: u32,
    /// Sustained no-progress streak (consecutive iterations of
    /// `NoProgressTripped`) before `EscalationTripped` fires. Default 8.
    pub escalation_threshold: u32,
    /// Bounded-ring depth of the action and hash histories.
    /// `(window / 2) + 1` is the gravity-state recurrence threshold.
    /// Default 16.
    pub window: usize,
}

impl Default for PatternConfig {
    fn default() -> Self {
        Self {
            same_action_threshold: 3,
            no_progress_threshold: 5,
            escalation_threshold: 8,
            window: 16,
        }
    }
}

/// Canonical fingerprint of a single Director action. Two actions hash
/// equal when their kind, target id (Bundle / Work id), and target
/// status (for `OverrideWork`) all match. `Done` and `NeedHelp` are
/// non-mutating and carry no target id; their fingerprint kind is the
/// distinguishing field.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActionFingerprint {
    pub kind: &'static str,
    pub target_id: String,
    pub target_status: Option<String>,
}

impl ActionFingerprint {
    pub fn accept_bundle(bundle_id: &str) -> Self {
        Self {
            kind: "accept_bundle",
            target_id: bundle_id.to_string(),
            target_status: None,
        }
    }

    pub fn override_work(work_id: &str, target_status: &str) -> Self {
        Self {
            kind: "override_work",
            target_id: work_id.to_string(),
            target_status: Some(target_status.to_string()),
        }
    }

    pub fn assign_work(work_id: &str) -> Self {
        Self {
            kind: "assign_work",
            target_id: work_id.to_string(),
            target_status: None,
        }
    }

    pub fn done() -> Self {
        Self {
            kind: "done",
            target_id: String::new(),
            target_status: None,
        }
    }

    pub fn need_help() -> Self {
        Self {
            kind: "need_help",
            target_id: String::new(),
            target_status: None,
        }
    }

    /// Mutating actions change FSM state; non-mutating actions (`done`,
    /// `need_help`) leave the Plan in place. The action-context gate
    /// in `observe()` consults this so a Director that only emits
    /// `done` while waiting on a long Implementer does not trip.
    pub fn is_mutating(&self) -> bool {
        matches!(self.kind, "accept_bundle" | "override_work" | "assign_work")
    }
}

/// Tracker output. Emitted on every `observe()` call; the caller
/// (Phase 5's mode transition table) routes the observation into the
/// Director's mode FSM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatternObservation {
    /// Same fingerprint emitted `count` consecutive iterations.
    SameActionTripped { kind: &'static str, count: u32 },
    /// State hash recurs over the window with mutating actions present.
    /// Carries the discriminating clause (`distinct` <= 2 OR
    /// `max_recurrence` >= half+1) and the consecutive-streak depth so
    /// the operator can grep `mode_change to=Conservative distinct=1`
    /// vs `max_recurrence=5`.
    NoProgressTripped {
        distinct: usize,
        max_recurrence: usize,
        streak: u32,
    },
    /// `NoProgressTripped` has held `escalation_threshold` consecutive
    /// iterations; mode advances from Conservative to NeedsOperator.
    EscalationTripped { reason: &'static str, streak: u32 },
    /// Hash moved AND the action history shows variety; mode reverts to
    /// Normal. Streak resets.
    Recovered,
    /// Operator-submitted note arrived this iteration. NOT emitted by
    /// `observe()` — this variant is constructed by the Director loop
    /// (Phase 9) when `list_unread_notes_for_plan` returns non-empty
    /// and threaded directly into `next_mode`. Treating operator
    /// engagement as a first-class FSM input keeps the mode demotion
    /// logic in one place.
    OperatorNoteArrived,
}

/// In-memory pattern tracker. One per Director task; reset on restart.
pub struct DirectorPatternTracker {
    action_history: VecDeque<ActionFingerprint>,
    state_hash_history: VecDeque<u64>,
    no_progress_streak: u32,
    config: PatternConfig,
}

impl DirectorPatternTracker {
    pub fn new(config: PatternConfig) -> Self {
        let window = config.window.max(1);
        Self {
            action_history: VecDeque::with_capacity(window),
            state_hash_history: VecDeque::with_capacity(window),
            no_progress_streak: 0,
            config,
        }
    }

    pub fn config(&self) -> &PatternConfig {
        &self.config
    }

    /// Current no-progress streak depth. Used by `DirectorStatusSnapshot`
    /// so the `loopr director status` verb can surface streak progress
    /// toward the `escalation_threshold` without parsing log fields.
    pub fn no_progress_streak(&self) -> u32 {
        self.no_progress_streak
    }

    /// Length of the trailing run of identical fingerprints in the
    /// action history (i.e. the streak `SameActionTripped` watches).
    /// `0` when the history is empty.
    pub fn same_action_streak(&self) -> u32 {
        consecutive_same_action(&self.action_history)
            .map(|(_, count)| count)
            .unwrap_or(0)
    }

    /// Reset the `no_progress_streak` counter. Phase 9: the Director
    /// loop calls this when an operator note arrives AND the mode FSM
    /// is demoting Conservative or NeedsOperator back to Normal — the
    /// streak otherwise persists across the demotion and the next
    /// iteration of stale no-progress can immediately bounce the mode
    /// back. Operator engagement is treated as a fresh start for the
    /// no-progress detector. SameActionTripped's internal counter is
    /// derived from `action_history` and clears naturally when the
    /// LLM emits any new fingerprint, so no reset is needed for it.
    pub fn reset_no_progress_streak(&mut self) {
        self.no_progress_streak = 0;
    }

    /// Record an iteration's emitted action + post-iteration state
    /// hash and return a `PatternObservation` if a pattern fired.
    ///
    /// Evaluation order (matters):
    /// 1. Append to bounded histories.
    /// 2. **SameActionTripped** — last N actions identical and
    ///    N >= `same_action_threshold`. Independent of NoProgress
    ///    state.
    /// 3. **Warm-up gate** — fewer than `no_progress_threshold` samples
    ///    means the window is not yet populated enough to evaluate
    ///    no-progress; return None without touching the streak.
    /// 4. **NoProgress trip** — gated by a mutating action in the
    ///    window AND (`distinct <= 2` OR
    ///    `max_recurrence >= (window/2)+1`). On trip, increment streak;
    ///    emit `EscalationTripped` once streak >= `escalation_threshold`,
    ///    otherwise emit `NoProgressTripped`.
    /// 5. **Recovered** — non-trip iteration AFTER a streak existed,
    ///    with hash motion + action variety. Resets streak to 0 and
    ///    emits `Recovered`. NOT fired on routine progress (streak=0);
    ///    Phase 5's mode FSM treats `Recovered` as the demotion signal
    ///    only meaningful when the Director was previously in a
    ///    Conservative or NeedsOperator mode.
    ///
    /// Edge case the formula does NOT catch: chaotic three-value
    /// rotation (e.g. A,B,C,A,B,C,...). `distinct=3, max_rec=3,
    /// window=8, half_plus_one=5` so neither OR-clause fires. If
    /// real traces show this is a recurring pathology, add a
    /// dedicated `distinct_threshold` config knob.
    pub fn observe(&mut self, action: ActionFingerprint, state_hash: u64) -> Option<PatternObservation> {
        let prev_hash = self.state_hash_history.back().copied();
        push_bounded(&mut self.action_history, action.clone(), self.config.window);
        push_bounded(&mut self.state_hash_history, state_hash, self.config.window);

        // 2. SameActionTripped — last N actions identical.
        if let Some((kind, count)) = consecutive_same_action(&self.action_history)
            && count >= self.config.same_action_threshold
        {
            return Some(PatternObservation::SameActionTripped { kind, count });
        }

        // 3. Warm-up: not enough samples yet.
        if self.state_hash_history.len() < self.config.no_progress_threshold as usize {
            return None;
        }

        let mutating = self.action_history.iter().any(ActionFingerprint::is_mutating);
        let distinct = distinct_count(&self.state_hash_history);
        let max_rec = max_recurrence(&self.state_hash_history);
        let half_plus_one = (self.config.window / 2) + 1;
        let trip = mutating && (distinct <= 2 || max_rec >= half_plus_one);

        // 4. NoProgress trip / EscalationTripped.
        if trip {
            self.no_progress_streak = self.no_progress_streak.saturating_add(1);
            if self.no_progress_streak >= self.config.escalation_threshold {
                return Some(PatternObservation::EscalationTripped {
                    reason: "no_progress_sustained",
                    streak: self.no_progress_streak,
                });
            }
            return Some(PatternObservation::NoProgressTripped {
                distinct,
                max_recurrence: max_rec,
                streak: self.no_progress_streak,
            });
        }

        // 5. Recovered — only meaningful after a prior streak. Hash
        //    motion AND action variety are required so a single
        //    unrelated `done` after a long static run does not bounce
        //    the mode back to Normal prematurely.
        if self.no_progress_streak > 0
            && let Some(prev) = prev_hash
            && state_hash != prev
            && action_variety(&self.action_history) >= 2
        {
            self.no_progress_streak = 0;
            return Some(PatternObservation::Recovered);
        }
        self.no_progress_streak = 0;
        None
    }
}

/// Stable hash over Work + Bundle status tuples; `attempt_count` is
/// intentionally excluded so Director-emitted retries (`Blocked ->
/// Ready`) do not change the hash and mask a stuck cycle as
/// "progress." Sorting before hash makes the result order-independent.
pub fn compute_state_hash(works: &[Work], bundles: &[Bundle]) -> u64 {
    let mut hasher = DefaultHasher::new();
    let mut work_tuples: Vec<_> = works.iter().map(|w| (w.id.to_string(), w.status)).collect();
    work_tuples.sort_by(|a, b| a.0.cmp(&b.0));
    work_tuples.hash(&mut hasher);
    let mut bundle_tuples: Vec<_> = bundles.iter().map(|b| (b.id.to_string(), b.status)).collect();
    bundle_tuples.sort_by(|a, b| a.0.cmp(&b.0));
    bundle_tuples.hash(&mut hasher);
    hasher.finish()
}

fn push_bounded<T>(deque: &mut VecDeque<T>, value: T, cap: usize) {
    if cap == 0 {
        return;
    }
    if deque.len() == cap {
        deque.pop_front();
    }
    deque.push_back(value);
}

fn action_variety(history: &VecDeque<ActionFingerprint>) -> usize {
    let mut seen: Vec<&ActionFingerprint> = Vec::new();
    for a in history.iter() {
        if !seen.contains(&a) {
            seen.push(a);
        }
    }
    seen.len()
}

fn consecutive_same_action(history: &VecDeque<ActionFingerprint>) -> Option<(&'static str, u32)> {
    let last = history.back()?;
    let mut count: u32 = 0;
    for a in history.iter().rev() {
        if a == last {
            count += 1;
        } else {
            break;
        }
    }
    Some((last.kind, count))
}

fn distinct_count(history: &VecDeque<u64>) -> usize {
    let mut seen: Vec<u64> = Vec::new();
    for h in history.iter() {
        if !seen.contains(h) {
            seen.push(*h);
        }
    }
    seen.len()
}

fn max_recurrence(history: &VecDeque<u64>) -> usize {
    let mut max_count = 0usize;
    for h in history.iter() {
        let count = history.iter().filter(|x| **x == *h).count();
        if count > max_count {
            max_count = count;
        }
    }
    max_count
}

#[cfg(test)]
mod tests;
