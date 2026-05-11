#![allow(clippy::unwrap_used)]
//! Unit tests for the Director pattern tracker (Phase 4 of
//! `docs/design/2026-05-09-director-phase-2.md`).

use domain::{Bundle, BundleStatus, PlanId, Work, WorkStatus};

use super::{ActionFingerprint, DirectorPatternTracker, PatternConfig, PatternObservation, compute_state_hash};

fn override_ready(work_id: &str) -> ActionFingerprint {
    ActionFingerprint::override_work(work_id, "Ready")
}

fn done() -> ActionFingerprint {
    ActionFingerprint::done()
}

fn accept(bundle_id: &str) -> ActionFingerprint {
    ActionFingerprint::accept_bundle(bundle_id)
}

fn assign(work_id: &str) -> ActionFingerprint {
    ActionFingerprint::assign_work(work_id)
}

fn make_work(plan_id: PlanId, title: &str, status: WorkStatus) -> Work {
    let mut w = Work::new(plan_id, title.to_string());
    w.status = status;
    w
}

fn make_bundle(work_id: domain::WorkId, status: BundleStatus) -> Bundle {
    let mut b = Bundle::new(work_id, "branch".to_string(), vec!["claim".to_string()]);
    b.status = status;
    b
}

// ---------------------------------------------------------------------------
// SameActionTripped + variants
// ---------------------------------------------------------------------------

#[test]
fn same_action_three_consecutive_trips() {
    let mut tracker = DirectorPatternTracker::new(PatternConfig::default());
    // Three identical override_work(wk-x, Ready) calls, mutating, with
    // moving state hash so Recovered doesn't preempt SameAction.
    let r1 = tracker.observe(override_ready("wk-x"), 100);
    let r2 = tracker.observe(override_ready("wk-x"), 100);
    let r3 = tracker.observe(override_ready("wk-x"), 100);
    assert!(r1.is_none());
    assert!(r2.is_none());
    match r3 {
        Some(PatternObservation::SameActionTripped { kind, count }) => {
            assert_eq!(kind, "override_work");
            assert_eq!(count, 3);
        }
        other => panic!("expected SameActionTripped, got {other:?}"),
    }
}

#[test]
fn same_action_interrupted_by_done_resets_counter() {
    let mut tracker = DirectorPatternTracker::new(PatternConfig::default());
    tracker.observe(override_ready("wk-x"), 100);
    tracker.observe(override_ready("wk-x"), 100);
    // `done` is a different fingerprint kind; the consecutive run breaks.
    tracker.observe(done(), 100);
    let r = tracker.observe(override_ready("wk-x"), 100);
    // Only 1 consecutive override_ready after the done; below threshold.
    assert!(
        !matches!(r, Some(PatternObservation::SameActionTripped { .. })),
        "consecutive run must reset after a different fingerprint: {r:?}"
    );
}

// ---------------------------------------------------------------------------
// State hash determinism
// ---------------------------------------------------------------------------

#[test]
fn state_hash_is_order_independent() {
    let plan_id = PlanId::new();
    let w1 = make_work(plan_id.clone(), "wk-1", WorkStatus::Pending);
    let w2 = make_work(plan_id.clone(), "wk-2", WorkStatus::Blocked);
    let b1 = make_bundle(w1.id.clone(), BundleStatus::Triaged);

    let h_ab = compute_state_hash(&[w1.clone(), w2.clone()], std::slice::from_ref(&b1));
    let h_ba = compute_state_hash(&[w2, w1], std::slice::from_ref(&b1));
    assert_eq!(h_ab, h_ba, "hash must be order-independent");
}

#[test]
fn state_hash_excludes_attempt_count() {
    let plan_id = PlanId::new();
    let mut w = make_work(plan_id, "wk-1", WorkStatus::Pending);
    let h_before = compute_state_hash(&[w.clone()], &[]);
    w.attempt_count = 5;
    let h_after = compute_state_hash(&[w], &[]);
    assert_eq!(
        h_before, h_after,
        "attempt_count must NOT be part of the state hash; Director retries would mask cycles otherwise"
    );
}

#[test]
fn state_hash_changes_with_work_status() {
    let plan_id = PlanId::new();
    let w_pending = make_work(plan_id.clone(), "wk-1", WorkStatus::Pending);
    let mut w_ready = w_pending.clone();
    w_ready.status = WorkStatus::Ready;
    let h_pending = compute_state_hash(&[w_pending], &[]);
    let h_ready = compute_state_hash(&[w_ready], &[]);
    assert_ne!(h_pending, h_ready);
}

// ---------------------------------------------------------------------------
// Recovered
// ---------------------------------------------------------------------------

#[test]
fn recovered_does_not_fire_from_normal_mode() {
    // Without a prior NoProgress streak, hash motion + variety is just
    // normal progress and emits None. `Recovered` is reserved for
    // *demotion* from a tripped state; firing it from Normal would be
    // noisy and meaningless to the Phase 5 mode FSM.
    let mut tracker = DirectorPatternTracker::new(PatternConfig::default());
    tracker.observe(override_ready("wk-x"), 100);
    tracker.observe(accept("bd-1"), 100);
    let r = tracker.observe(assign("wk-y"), 200);
    assert!(
        !matches!(r, Some(PatternObservation::Recovered)),
        "Recovered must require a prior streak: {r:?}"
    );
}

#[test]
fn recovered_does_not_fire_when_hash_stays_constant() {
    let mut tracker = DirectorPatternTracker::new(PatternConfig::default());
    tracker.observe(override_ready("wk-x"), 100);
    let r = tracker.observe(accept("bd-1"), 100);
    assert!(
        !matches!(r, Some(PatternObservation::Recovered)),
        "hash unchanged; no recovery: {r:?}"
    );
}

#[test]
fn recovered_resets_no_progress_streak() {
    // Use a small window so a few diverse observations actually pull
    // the window's distinct count up and recurrence down out of trip.
    // window=4, half_plus_one=3: need distinct >= 3 AND max_rec <= 2.
    let cfg = PatternConfig {
        window: 4,
        no_progress_threshold: 3,
        same_action_threshold: 99, // disable SameAction preemption
        ..PatternConfig::default()
    };
    let mut tracker = DirectorPatternTracker::new(cfg);
    // Build a NoProgress streak via 4 mutating obs at static hash=100.
    let mut tripped = 0usize;
    let actions = [accept("bd-1"), assign("wk-1")];
    for i in 0..4 {
        let r = tracker.observe(actions[i % 2].clone(), 100);
        if matches!(r, Some(PatternObservation::NoProgressTripped { .. })) {
            tripped += 1;
        }
    }
    assert!(tripped >= 2, "expected sustained NoProgress trips, got {tripped}");

    // Feed diverse hashes one at a time and find the iteration where
    // the no-progress condition clears and Recovered fires. With
    // window=4 and starting history [100,100,100,100], the second
    // recovery observation pushes distinct to 3 and max_rec to 2,
    // clearing both clauses.
    tracker.observe(override_ready("wk-z"), 200);
    let r = tracker.observe(accept("bd-2"), 300);
    match r {
        Some(PatternObservation::Recovered) => {}
        other => panic!("expected Recovered once window clears, got {other:?}"),
    }

    // Streak is reset; following observations don't immediately
    // jump to EscalationTripped even if the window re-trips.
    let r2 = tracker.observe(accept("bd-1"), 100);
    if let Some(PatternObservation::EscalationTripped { .. }) = r2 {
        panic!("streak must reset on Recovered: {r2:?}")
    }
}

// ---------------------------------------------------------------------------
// NoProgressTripped: static, 2-cycle, gravity-state
// ---------------------------------------------------------------------------

#[test]
fn no_progress_static_state_with_mutation_trips() {
    let cfg = PatternConfig {
        no_progress_threshold: 5,
        ..PatternConfig::default()
    };
    let mut tracker = DirectorPatternTracker::new(cfg);
    // Keep hash identical for >= 5 iterations with mutating actions.
    // Alternate two distinct action fingerprints so SameActionTripped
    // doesn't preempt NoProgress.
    for i in 0..5 {
        let action = if i % 2 == 0 { accept("bd-1") } else { assign("wk-1") };
        let r = tracker.observe(action, 100);
        if i == 4 {
            match r {
                Some(PatternObservation::NoProgressTripped {
                    distinct,
                    max_recurrence,
                    streak,
                }) => {
                    assert_eq!(distinct, 1, "static-state distinct count must be 1");
                    assert_eq!(max_recurrence, 5);
                    assert_eq!(streak, 1);
                }
                other => panic!("expected NoProgressTripped at iteration 5, got {other:?}"),
            }
        }
    }
}

#[test]
fn no_progress_two_cycle_trips_via_distinct_clause() {
    let cfg = PatternConfig {
        window: 8,
        no_progress_threshold: 8,
        ..PatternConfig::default()
    };
    let mut tracker = DirectorPatternTracker::new(cfg);
    let actions = [accept("bd-1"), assign("wk-1")];
    let mut last_obs: Option<PatternObservation> = None;
    // Hashes alternate H1, H2, H1, H2, ... — distinct = 2, max_rec = 4,
    // half_plus_one = 5. `distinct <= 2` is the firing clause.
    for i in 0..8 {
        let hash = if i % 2 == 0 { 1 } else { 2 };
        last_obs = tracker.observe(actions[i % 2].clone(), hash);
    }
    match last_obs {
        Some(PatternObservation::NoProgressTripped {
            distinct,
            max_recurrence: _,
            ..
        }) => {
            assert_eq!(distinct, 2, "2-cycle must produce distinct=2");
        }
        other => panic!("expected NoProgressTripped (2-cycle), got {other:?}"),
    }
}

#[test]
fn no_progress_gravity_state_trips_via_recurrence_clause() {
    let cfg = PatternConfig {
        window: 8,
        no_progress_threshold: 8,
        ..PatternConfig::default()
    };
    let mut tracker = DirectorPatternTracker::new(cfg);
    // Hash pattern: A,A,A,B,A,A,C,A. distinct = 3 (so `distinct <= 2`
    // does NOT fire); A recurs 6 times, half_plus_one = 5 -> max_rec >=
    // 5 fires. This isolates the recurrence clause from the distinct
    // clause.
    let hashes = [10, 10, 10, 20, 10, 10, 30, 10];
    let mut last_obs: Option<PatternObservation> = None;
    for (i, h) in hashes.iter().enumerate() {
        let action = if i % 2 == 0 { accept("bd-1") } else { assign("wk-1") };
        last_obs = tracker.observe(action, *h);
    }
    match last_obs {
        Some(PatternObservation::NoProgressTripped {
            distinct,
            max_recurrence,
            ..
        }) => {
            assert_eq!(distinct, 3, "three distinct hash values");
            assert_eq!(max_recurrence, 6, "A recurs six times in the window");
            assert!(max_recurrence >= 5, "recurrence clause fires at half_plus_one=5");
        }
        other => panic!("expected NoProgressTripped (gravity), got {other:?}"),
    }
}

#[test]
fn no_progress_chaotic_three_cycle_does_not_trip() {
    // A,B,C,A,B,C,A,B with window=8: distinct=3, max_recurrence=3,
    // half_plus_one=5. Neither OR-clause fires; the per-iteration
    // detector misses this edge case by design. The design doc's test
    // description names "distinct <= 3" but the formula says "<= 2";
    // we implement the formula and document the limitation. The
    // streak-based EscalationTripped does NOT compensate because the
    // streak only increments on a trip; here it stays at 0.
    let cfg = PatternConfig {
        window: 8,
        no_progress_threshold: 8,
        ..PatternConfig::default()
    };
    let mut tracker = DirectorPatternTracker::new(cfg);
    let hashes = [1, 2, 3, 1, 2, 3, 1, 2];
    let actions = [accept("bd-1"), assign("wk-1"), accept("bd-2")];
    let mut last_obs: Option<PatternObservation> = None;
    for (i, h) in hashes.iter().enumerate() {
        last_obs = tracker.observe(actions[i % actions.len()].clone(), *h);
    }
    assert!(
        !matches!(last_obs, Some(PatternObservation::NoProgressTripped { .. })),
        "chaotic 3-cycle slips the per-iteration detector by design: {last_obs:?}"
    );
}

// ---------------------------------------------------------------------------
// Action-context gate (negative cases)
// ---------------------------------------------------------------------------

#[test]
fn action_context_gate_static_hash_all_done_does_not_trip() {
    let cfg = PatternConfig {
        no_progress_threshold: 5,
        window: 8,
        ..PatternConfig::default()
    };
    let mut tracker = DirectorPatternTracker::new(cfg);
    // 8 `done` actions, identical hash. Without the gate this would
    // trip on distinct=1; with the gate it must NOT.
    let mut last_obs: Option<PatternObservation> = None;
    for _ in 0..8 {
        last_obs = tracker.observe(done(), 42);
    }
    assert!(
        !matches!(last_obs, Some(PatternObservation::NoProgressTripped { .. })),
        "all-passive iterations must not trip NoProgress: {last_obs:?}"
    );
}

#[test]
fn action_context_gate_two_cycle_all_done_does_not_trip() {
    let cfg = PatternConfig {
        no_progress_threshold: 8,
        window: 8,
        ..PatternConfig::default()
    };
    let mut tracker = DirectorPatternTracker::new(cfg);
    let mut last_obs: Option<PatternObservation> = None;
    for i in 0..8 {
        let hash = if i % 2 == 0 { 1 } else { 2 };
        last_obs = tracker.observe(done(), hash);
    }
    assert!(
        !matches!(last_obs, Some(PatternObservation::NoProgressTripped { .. })),
        "2-cycle with no mutating action must not trip: {last_obs:?}"
    );
}

// ---------------------------------------------------------------------------
// EscalationTripped
// ---------------------------------------------------------------------------

#[test]
fn escalation_fires_after_sustained_no_progress() {
    let cfg = PatternConfig {
        no_progress_threshold: 5,
        escalation_threshold: 3,
        window: 8,
        ..PatternConfig::default()
    };
    let mut tracker = DirectorPatternTracker::new(cfg);
    // Build NoProgress condition first (5 iterations of static hash
    // with mutating actions), then sustain it through escalation.
    let mut observations: Vec<Option<PatternObservation>> = Vec::new();
    for i in 0..10 {
        let action = if i % 2 == 0 { accept("bd-1") } else { assign("wk-1") };
        observations.push(tracker.observe(action, 100));
    }
    let escalated = observations.iter().any(|o| {
        matches!(
            o,
            Some(PatternObservation::EscalationTripped {
                reason: "no_progress_sustained",
                ..
            })
        )
    });
    assert!(
        escalated,
        "EscalationTripped must fire once streak >= 3: {observations:?}"
    );
}

// ---------------------------------------------------------------------------
// PatternConfig YAML round-trip
// ---------------------------------------------------------------------------

#[test]
fn pattern_config_yaml_round_trip_kebab_case() {
    let yaml = "\
same-action-threshold: 5
no-progress-threshold: 7
escalation-threshold: 12
window: 32
";
    let cfg: PatternConfig = serde_yaml::from_str(yaml).expect("deserialize");
    assert_eq!(cfg.same_action_threshold, 5);
    assert_eq!(cfg.no_progress_threshold, 7);
    assert_eq!(cfg.escalation_threshold, 12);
    assert_eq!(cfg.window, 32);

    let serialized = serde_yaml::to_string(&cfg).expect("serialize");
    assert!(serialized.contains("same-action-threshold: 5"), "{serialized}");
    assert!(serialized.contains("no-progress-threshold: 7"), "{serialized}");
}

#[test]
fn pattern_config_partial_override_keeps_defaults() {
    let yaml = "window: 24\n";
    let cfg: PatternConfig = serde_yaml::from_str(yaml).expect("deserialize");
    assert_eq!(cfg.window, 24);
    assert_eq!(cfg.same_action_threshold, 3, "default preserved");
    assert_eq!(cfg.no_progress_threshold, 5, "default preserved");
    assert_eq!(cfg.escalation_threshold, 8, "default preserved");
}

#[test]
fn pattern_config_empty_yaml_all_defaults() {
    let cfg: PatternConfig = serde_yaml::from_str("{}").expect("deserialize");
    assert_eq!(cfg.same_action_threshold, 3);
    assert_eq!(cfg.no_progress_threshold, 5);
    assert_eq!(cfg.escalation_threshold, 8);
    assert_eq!(cfg.window, 16);
}

// ---------------------------------------------------------------------------
// OperatorNoteArrived is NOT emitted by `observe()` (Phase 9)
//
// `OperatorNoteArrived` is constructed by the Director loop when
// `list_unread_notes_for_plan` returns non-empty and threaded directly
// into `next_mode`. The pattern tracker's `observe()` never sees the
// note event itself and must never synthesize this variant — that
// would route operator engagement through `next_mode` twice.
// ---------------------------------------------------------------------------

#[test]
fn observe_never_emits_operator_note_arrived() {
    let plan_id = PlanId::new();
    let work = make_work(plan_id, "W", WorkStatus::Ready);
    let hash = compute_state_hash(&[work], &[]);
    let mut tracker = DirectorPatternTracker::new(PatternConfig::default());
    for i in 0..32 {
        let action = if i % 2 == 0 { done() } else { accept("b") };
        let obs = tracker.observe(action, hash.wrapping_add(i as u64));
        if let Some(o) = obs {
            assert!(
                !matches!(o, PatternObservation::OperatorNoteArrived),
                "observe() must never emit OperatorNoteArrived; got {o:?}"
            );
        }
    }
}
