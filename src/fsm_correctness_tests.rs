//! Exhaustive FSM correctness tests for every state machine in Loopr v3.
//!
//! These tests systematically prove:
//! 1. Every valid transition succeeds with the correct role
//! 2. Every invalid transition is rejected (wrong role, skip state, terminal, reverse, self)
//! 3. Terminal states cannot transition to ANY other state
//! 4. Self-transitions are idempotent (return Transition::Unchanged)
//! 5. Records serialize/deserialize correctly through the full lifecycle
//!
//! Organized by FSM, with N×N matrix coverage for each.

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use crate::agents::AgentStatus;
    use crate::domain::bundle::{Bundle, BundleStatus};
    use crate::domain::lock::{Lock, LockStatus};
    use crate::domain::plan::{HierarchyStatus, Plan};
    use crate::domain::role::Role;
    use crate::domain::tick::{Tick, TickStatus};
    use crate::domain::transition::Transition;
    use crate::domain::work::{Work, WorkStatus};

    // ========================================================================
    // Helper: all roles for exhaustive wrong-role testing
    // ========================================================================

    const ALL_ROLES: [Role; 5] = [
        Role::Coordinator,
        Role::Integrator,
        Role::Implementer,
        Role::Reviewer,
        Role::Researcher,
    ];

    fn assert_valid(from: impl Into<String>, to: impl Into<String>, result: &crate::error::Result<Transition>) {
        let f = from.into();
        let t = to.into();
        assert!(result.is_ok(), "{} -> {} should be valid but got: {:?}", f, t, result);
    }

    fn assert_invalid(from: impl Into<String>, to: impl Into<String>, result: &crate::error::Result<Transition>) {
        let f = from.into();
        let t = to.into();
        assert!(result.is_err(), "{} -> {} should be INVALID but succeeded", f, t);
    }

    // ========================================================================
    // FSM 1: HierarchyStatus (Plan, Spec, Phase)
    // States: Draft, Active, Complete, Abandoned
    // Terminal: Complete, Abandoned
    // ========================================================================

    mod hierarchy {
        use super::*;

        const ALL_STATES: [HierarchyStatus; 4] = [
            HierarchyStatus::Draft,
            HierarchyStatus::Active,
            HierarchyStatus::Complete,
            HierarchyStatus::Abandoned,
        ];

        const TERMINAL: [HierarchyStatus; 2] = [HierarchyStatus::Complete, HierarchyStatus::Abandoned];

        // --- Valid transitions: all 4 with Coordinator ---

        #[test]
        fn valid_draft_to_active() {
            let r = HierarchyStatus::Draft.validate_transition(HierarchyStatus::Active, Role::Coordinator);
            assert_valid("Draft", "Active", &r);
        }

        #[test]
        fn valid_active_to_complete() {
            let r = HierarchyStatus::Active.validate_transition(HierarchyStatus::Complete, Role::Coordinator);
            assert_valid("Active", "Complete", &r);
        }

        #[test]
        fn valid_draft_to_abandoned() {
            let r = HierarchyStatus::Draft.validate_transition(HierarchyStatus::Abandoned, Role::Coordinator);
            assert_valid("Draft", "Abandoned", &r);
        }

        #[test]
        fn valid_active_to_abandoned() {
            let r = HierarchyStatus::Active.validate_transition(HierarchyStatus::Abandoned, Role::Coordinator);
            assert_valid("Active", "Abandoned", &r);
        }

        // --- Wrong role: every valid transition with every wrong role ---

        #[test]
        fn wrong_role_on_every_valid_transition() {
            let valid_pairs = [
                (HierarchyStatus::Draft, HierarchyStatus::Active),
                (HierarchyStatus::Active, HierarchyStatus::Complete),
                (HierarchyStatus::Draft, HierarchyStatus::Abandoned),
                (HierarchyStatus::Active, HierarchyStatus::Abandoned),
            ];
            let wrong_roles = [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator];

            for (from, to) in &valid_pairs {
                for role in &wrong_roles {
                    let r = from.validate_transition(*to, *role);
                    assert_invalid(format!("{:?}", from), format!("{:?}", to), &r);
                }
            }
        }

        // --- Terminal states: no outbound transitions allowed ---

        #[test]
        fn terminal_states_reject_all_outbound() {
            for terminal in &TERMINAL {
                for target in &ALL_STATES {
                    if terminal == target {
                        continue;
                    }
                    for role in &ALL_ROLES {
                        let r = terminal.validate_transition(*target, *role);
                        assert_invalid(format!("{:?}", terminal), format!("{:?}", target), &r);
                    }
                }
            }
        }

        // --- Self-transitions: idempotent ---

        #[test]
        fn self_transitions_idempotent() {
            for state in &ALL_STATES {
                for role in &ALL_ROLES {
                    let r = state.validate_transition(*state, *role);
                    assert_valid(format!("{:?}", state), format!("{:?}", state), &r);
                    assert_eq!(r.unwrap(), Transition::Unchanged);
                }
            }
        }

        // --- Skip states ---

        #[test]
        fn skip_draft_to_complete_rejected() {
            let r = HierarchyStatus::Draft.validate_transition(HierarchyStatus::Complete, Role::Coordinator);
            assert_invalid("Draft", "Complete", &r);
        }

        // --- Reverse direction ---

        #[test]
        fn reverse_active_to_draft_rejected() {
            let r = HierarchyStatus::Active.validate_transition(HierarchyStatus::Draft, Role::Coordinator);
            assert_invalid("Active", "Draft", &r);
        }

        // --- Record serde roundtrip ---

        #[test]
        fn plan_serde_all_statuses() {
            for status in &ALL_STATES {
                let mut plan = Plan::new("T".into(), "D".into(), "C".into());
                plan.status = *status;
                let json = serde_json::to_string(&plan).unwrap();
                let restored: Plan = serde_json::from_str(&json).unwrap();
                assert_eq!(restored.status, *status);
                assert_eq!(restored.id, plan.id);
            }
        }
    }

    // ========================================================================
    // FSM 2: WorkStatus
    // States: Draft, Ready, InProgress, Blocked, InReview, Integrated, Done, Abandoned
    // Terminal: Done, Abandoned
    // ========================================================================

    mod work {
        use super::*;

        const ALL_STATES: [WorkStatus; 8] = [
            WorkStatus::Draft,
            WorkStatus::Ready,
            WorkStatus::InProgress,
            WorkStatus::Blocked,
            WorkStatus::InReview,
            WorkStatus::Integrated,
            WorkStatus::Done,
            WorkStatus::Abandoned,
        ];

        const TERMINAL: [WorkStatus; 2] = [WorkStatus::Done, WorkStatus::Abandoned];

        // --- All 15 valid transitions ---

        #[test]
        fn valid_draft_to_ready() {
            let r = WorkStatus::Draft.validate_transition(WorkStatus::Ready, Role::Coordinator);
            assert_valid("Draft", "Ready", &r);
        }

        #[test]
        fn valid_ready_to_in_progress() {
            let r = WorkStatus::Ready.validate_transition(WorkStatus::InProgress, Role::Coordinator);
            assert_valid("Ready", "InProgress", &r);
        }

        #[test]
        fn valid_in_progress_to_blocked_any_role() {
            for role in &ALL_ROLES {
                let r = WorkStatus::InProgress.validate_transition(WorkStatus::Blocked, *role);
                assert_valid("InProgress", format!("Blocked ({:?})", role), &r);
            }
        }

        #[test]
        fn valid_blocked_to_ready() {
            let r = WorkStatus::Blocked.validate_transition(WorkStatus::Ready, Role::Coordinator);
            assert_valid("Blocked", "Ready", &r);
        }

        #[test]
        fn valid_in_progress_to_in_review() {
            let r = WorkStatus::InProgress.validate_transition(WorkStatus::InReview, Role::Implementer);
            assert_valid("InProgress", "InReview", &r);
        }

        #[test]
        fn valid_in_review_to_in_progress() {
            let r = WorkStatus::InReview.validate_transition(WorkStatus::InProgress, Role::Coordinator);
            assert_valid("InReview", "InProgress", &r);
        }

        #[test]
        fn valid_in_review_to_integrated() {
            let r = WorkStatus::InReview.validate_transition(WorkStatus::Integrated, Role::Integrator);
            assert_valid("InReview", "Integrated", &r);
        }

        #[test]
        fn valid_integrated_to_done_coordinator() {
            let r = WorkStatus::Integrated.validate_transition(WorkStatus::Done, Role::Coordinator);
            assert_valid("Integrated", "Done (Coordinator)", &r);
        }

        #[test]
        fn valid_integrated_to_done_integrator() {
            let r = WorkStatus::Integrated.validate_transition(WorkStatus::Done, Role::Integrator);
            assert_valid("Integrated", "Done (Integrator)", &r);
        }

        #[test]
        fn valid_abandoned_from_all_non_terminal() {
            let non_terminal = [
                WorkStatus::Draft,
                WorkStatus::Ready,
                WorkStatus::InProgress,
                WorkStatus::Blocked,
                WorkStatus::InReview,
                WorkStatus::Integrated,
            ];
            for from in &non_terminal {
                let r = from.validate_transition(WorkStatus::Abandoned, Role::Coordinator);
                assert_valid(format!("{:?}", from), "Abandoned", &r);
            }
        }

        // --- Terminal states: no outbound transitions allowed ---

        #[test]
        fn terminal_states_reject_all_outbound() {
            for terminal in &TERMINAL {
                for target in &ALL_STATES {
                    if terminal == target {
                        continue;
                    }
                    for role in &ALL_ROLES {
                        let r = terminal.validate_transition(*target, *role);
                        assert_invalid(format!("{:?}", terminal), format!("{:?}", target), &r);
                    }
                }
            }
        }

        // --- Self-transitions: idempotent ---

        #[test]
        fn self_transitions_idempotent() {
            for state in &ALL_STATES {
                for role in &ALL_ROLES {
                    let r = state.validate_transition(*state, *role);
                    assert_valid(format!("{:?}", state), format!("{:?}", state), &r);
                    assert_eq!(r.unwrap(), Transition::Unchanged);
                }
            }
        }

        // --- Wrong role tests ---

        #[test]
        fn wrong_role_draft_to_ready() {
            for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
                let r = WorkStatus::Draft.validate_transition(WorkStatus::Ready, role);
                assert_invalid("Draft", format!("Ready ({:?})", role), &r);
            }
        }

        #[test]
        fn wrong_role_ready_to_in_progress() {
            for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
                let r = WorkStatus::Ready.validate_transition(WorkStatus::InProgress, role);
                assert_invalid("Ready", format!("InProgress ({:?})", role), &r);
            }
        }

        #[test]
        fn wrong_role_in_progress_to_in_review() {
            for role in [Role::Coordinator, Role::Reviewer, Role::Researcher, Role::Integrator] {
                let r = WorkStatus::InProgress.validate_transition(WorkStatus::InReview, role);
                assert_invalid("InProgress", format!("InReview ({:?})", role), &r);
            }
        }

        #[test]
        fn wrong_role_in_review_to_in_progress() {
            for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
                let r = WorkStatus::InReview.validate_transition(WorkStatus::InProgress, role);
                assert_invalid("InReview", format!("InProgress ({:?})", role), &r);
            }
        }

        #[test]
        fn wrong_role_in_review_to_integrated() {
            for role in [Role::Coordinator, Role::Implementer, Role::Reviewer, Role::Researcher] {
                let r = WorkStatus::InReview.validate_transition(WorkStatus::Integrated, role);
                assert_invalid("InReview", format!("Integrated ({:?})", role), &r);
            }
        }

        #[test]
        fn wrong_role_integrated_to_done() {
            for role in [Role::Implementer, Role::Reviewer, Role::Researcher] {
                let r = WorkStatus::Integrated.validate_transition(WorkStatus::Done, role);
                assert_invalid("Integrated", format!("Done ({:?})", role), &r);
            }
        }

        #[test]
        fn wrong_role_abandoned() {
            let non_terminal = [
                WorkStatus::Draft,
                WorkStatus::Ready,
                WorkStatus::InProgress,
                WorkStatus::Blocked,
                WorkStatus::InReview,
                WorkStatus::Integrated,
            ];
            for from in &non_terminal {
                for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
                    let r = from.validate_transition(WorkStatus::Abandoned, role);
                    assert_invalid(format!("{:?}", from), format!("Abandoned ({:?})", role), &r);
                }
            }
        }

        // --- Skip state tests ---

        #[test]
        fn skip_states_rejected() {
            let skip_pairs = [
                (WorkStatus::Draft, WorkStatus::InProgress),
                (WorkStatus::Draft, WorkStatus::InReview),
                (WorkStatus::Draft, WorkStatus::Integrated),
                (WorkStatus::Draft, WorkStatus::Done),
                (WorkStatus::Ready, WorkStatus::InReview),
                (WorkStatus::Ready, WorkStatus::Integrated),
                (WorkStatus::Ready, WorkStatus::Done),
                (WorkStatus::Blocked, WorkStatus::InProgress),
                (WorkStatus::Blocked, WorkStatus::InReview),
                (WorkStatus::Blocked, WorkStatus::Integrated),
                (WorkStatus::Blocked, WorkStatus::Done),
                (WorkStatus::InProgress, WorkStatus::Integrated),
                (WorkStatus::InProgress, WorkStatus::Done),
                (WorkStatus::InReview, WorkStatus::Done),
            ];
            for (from, to) in &skip_pairs {
                for role in &ALL_ROLES {
                    let r = from.validate_transition(*to, *role);
                    assert_invalid(format!("{:?}", from), format!("{:?} ({:?})", to, role), &r);
                }
            }
        }

        // --- Reverse direction ---

        #[test]
        fn reverse_directions_rejected() {
            let reverse_pairs = [
                (WorkStatus::Ready, WorkStatus::Draft),
                (WorkStatus::InProgress, WorkStatus::Draft),
                (WorkStatus::InProgress, WorkStatus::Ready),
                (WorkStatus::Integrated, WorkStatus::InReview),
                (WorkStatus::Integrated, WorkStatus::InProgress),
                (WorkStatus::Integrated, WorkStatus::Ready),
                (WorkStatus::Integrated, WorkStatus::Draft),
                (WorkStatus::Done, WorkStatus::Integrated),
            ];
            for (from, to) in &reverse_pairs {
                for role in &ALL_ROLES {
                    let r = from.validate_transition(*to, *role);
                    assert_invalid(format!("{:?}", from), format!("{:?} ({:?})", to, role), &r);
                }
            }
        }

        // --- Full happy-path lifecycle ---

        #[test]
        fn full_lifecycle_happy_path() {
            let chain: Vec<(WorkStatus, WorkStatus, Role)> = vec![
                (WorkStatus::Draft, WorkStatus::Ready, Role::Coordinator),
                (WorkStatus::Ready, WorkStatus::InProgress, Role::Coordinator),
                (WorkStatus::InProgress, WorkStatus::InReview, Role::Implementer),
                (WorkStatus::InReview, WorkStatus::Integrated, Role::Integrator),
                (WorkStatus::Integrated, WorkStatus::Done, Role::Coordinator),
            ];
            for (from, to, role) in &chain {
                let r = from.validate_transition(*to, *role);
                assert_valid(format!("{:?}", from), format!("{:?}", to), &r);
            }
        }

        // --- Record serde roundtrip ---

        #[test]
        fn work_serde_all_statuses() {
            for status in &ALL_STATES {
                let mut wi = Work::new("ph-1".into(), "T".into(), "D".into());
                wi.status = *status;
                let json = serde_json::to_string(&wi).unwrap();
                let restored: Work = serde_json::from_str(&json).unwrap();
                assert_eq!(restored.status, *status);
            }
        }
    }

    // ========================================================================
    // FSM 3: BundleStatus
    // States: Proposed, Triaged, Reviewed, Accepted, Integrating, Merged, Rejected, Superseded
    // Terminal: Merged, Rejected, Superseded
    // ========================================================================

    mod bundle {
        use super::*;

        const ALL_STATES: [BundleStatus; 8] = [
            BundleStatus::Proposed,
            BundleStatus::Triaged,
            BundleStatus::Reviewed,
            BundleStatus::Accepted,
            BundleStatus::Integrating,
            BundleStatus::Merged,
            BundleStatus::Rejected,
            BundleStatus::Superseded,
        ];

        const TERMINAL: [BundleStatus; 3] = [BundleStatus::Merged, BundleStatus::Rejected, BundleStatus::Superseded];

        // --- All 19 valid transitions ---

        #[test]
        fn valid_happy_path() {
            let chain = [
                (BundleStatus::Proposed, BundleStatus::Triaged, Role::Coordinator),
                (BundleStatus::Triaged, BundleStatus::Reviewed, Role::Coordinator),
                (BundleStatus::Reviewed, BundleStatus::Accepted, Role::Coordinator),
                (BundleStatus::Accepted, BundleStatus::Integrating, Role::Integrator),
                (BundleStatus::Integrating, BundleStatus::Merged, Role::Integrator),
            ];
            for (from, to, role) in &chain {
                let r = from.validate_transition(*to, *role);
                assert_valid(format!("{:?}", from), format!("{:?}", to), &r);
            }
        }

        #[test]
        fn valid_triaged_to_reviewed_by_reviewer() {
            let r = BundleStatus::Triaged.validate_transition(BundleStatus::Reviewed, Role::Reviewer);
            assert_valid("Triaged", "Reviewed (Reviewer)", &r);
        }

        #[test]
        fn valid_integrating_to_rejected() {
            let r = BundleStatus::Integrating.validate_transition(BundleStatus::Rejected, Role::Integrator);
            assert_valid("Integrating", "Rejected", &r);
        }

        #[test]
        fn valid_accepted_to_rejected() {
            let r = BundleStatus::Accepted.validate_transition(BundleStatus::Rejected, Role::Integrator);
            assert_valid("Accepted", "Rejected (Integrator)", &r);
        }

        #[test]
        fn valid_early_rejection_coordinator() {
            for from in [BundleStatus::Proposed, BundleStatus::Triaged, BundleStatus::Reviewed] {
                let r = from.validate_transition(BundleStatus::Rejected, Role::Coordinator);
                assert_valid(format!("{:?}", from), "Rejected (Coordinator)", &r);
            }
        }

        #[test]
        fn valid_early_rejection_reviewer() {
            for from in [BundleStatus::Proposed, BundleStatus::Triaged, BundleStatus::Reviewed] {
                let r = from.validate_transition(BundleStatus::Rejected, Role::Reviewer);
                assert_valid(format!("{:?}", from), "Rejected (Reviewer)", &r);
            }
        }

        #[test]
        fn valid_superseded_from_all_non_terminal() {
            let non_terminal = [
                BundleStatus::Proposed,
                BundleStatus::Triaged,
                BundleStatus::Reviewed,
                BundleStatus::Accepted,
                BundleStatus::Integrating,
            ];
            for from in &non_terminal {
                let r = from.validate_transition(BundleStatus::Superseded, Role::Coordinator);
                assert_valid(format!("{:?}", from), "Superseded", &r);
            }
        }

        // --- Terminal states: no outbound transitions allowed ---

        #[test]
        fn terminal_states_reject_all_outbound() {
            for terminal in &TERMINAL {
                for target in &ALL_STATES {
                    if terminal == target {
                        continue;
                    }
                    for role in &ALL_ROLES {
                        let r = terminal.validate_transition(*target, *role);
                        assert_invalid(format!("{:?}", terminal), format!("{:?}", target), &r);
                    }
                }
            }
        }

        // --- Self-transitions: idempotent ---

        #[test]
        fn self_transitions_idempotent() {
            for state in &ALL_STATES {
                for role in &ALL_ROLES {
                    let r = state.validate_transition(*state, *role);
                    assert_valid(format!("{:?}", state), format!("{:?}", state), &r);
                    assert_eq!(r.unwrap(), Transition::Unchanged);
                }
            }
        }

        // --- Wrong role tests ---

        #[test]
        fn wrong_role_proposed_to_triaged() {
            for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
                let r = BundleStatus::Proposed.validate_transition(BundleStatus::Triaged, role);
                assert_invalid("Proposed", format!("Triaged ({:?})", role), &r);
            }
        }

        #[test]
        fn wrong_role_triaged_to_reviewed() {
            // Only Coordinator and Reviewer are valid
            for role in [Role::Implementer, Role::Researcher, Role::Integrator] {
                let r = BundleStatus::Triaged.validate_transition(BundleStatus::Reviewed, role);
                assert_invalid("Triaged", format!("Reviewed ({:?})", role), &r);
            }
        }

        #[test]
        fn wrong_role_reviewed_to_accepted() {
            for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
                let r = BundleStatus::Reviewed.validate_transition(BundleStatus::Accepted, role);
                assert_invalid("Reviewed", format!("Accepted ({:?})", role), &r);
            }
        }

        #[test]
        fn wrong_role_accepted_to_integrating() {
            for role in [Role::Coordinator, Role::Implementer, Role::Reviewer, Role::Researcher] {
                let r = BundleStatus::Accepted.validate_transition(BundleStatus::Integrating, role);
                assert_invalid("Accepted", format!("Integrating ({:?})", role), &r);
            }
        }

        #[test]
        fn wrong_role_integrating_to_merged() {
            for role in [Role::Coordinator, Role::Implementer, Role::Reviewer, Role::Researcher] {
                let r = BundleStatus::Integrating.validate_transition(BundleStatus::Merged, role);
                assert_invalid("Integrating", format!("Merged ({:?})", role), &r);
            }
        }

        #[test]
        fn wrong_role_superseded() {
            let non_terminal = [
                BundleStatus::Proposed,
                BundleStatus::Triaged,
                BundleStatus::Reviewed,
                BundleStatus::Accepted,
                BundleStatus::Integrating,
            ];
            for from in &non_terminal {
                for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
                    let r = from.validate_transition(BundleStatus::Superseded, role);
                    assert_invalid(format!("{:?}", from), format!("Superseded ({:?})", role), &r);
                }
            }
        }

        // --- Skip state tests ---

        #[test]
        fn skip_states_rejected() {
            let skip_pairs = [
                (BundleStatus::Proposed, BundleStatus::Reviewed),
                (BundleStatus::Proposed, BundleStatus::Accepted),
                (BundleStatus::Proposed, BundleStatus::Integrating),
                (BundleStatus::Proposed, BundleStatus::Merged),
                // Triaged->Accepted is now valid for Coordinator (advisory review bypass)
                (BundleStatus::Triaged, BundleStatus::Integrating),
                (BundleStatus::Triaged, BundleStatus::Merged),
                (BundleStatus::Reviewed, BundleStatus::Integrating),
                (BundleStatus::Reviewed, BundleStatus::Merged),
                (BundleStatus::Accepted, BundleStatus::Merged),
            ];
            for (from, to) in &skip_pairs {
                for role in &ALL_ROLES {
                    let r = from.validate_transition(*to, *role);
                    assert_invalid(format!("{:?}", from), format!("{:?} ({:?})", to, role), &r);
                }
            }
            // Triaged->Accepted: valid for Coordinator only
            let r = BundleStatus::Triaged.validate_transition(BundleStatus::Accepted, Role::Coordinator);
            assert_valid("Triaged", "Accepted (Coordinator)", &r);
            for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
                let r = BundleStatus::Triaged.validate_transition(BundleStatus::Accepted, role);
                assert_invalid("Triaged", format!("Accepted ({:?})", role), &r);
            }
        }

        // --- Reverse direction ---

        #[test]
        fn reverse_directions_rejected() {
            let reverse_pairs = [
                (BundleStatus::Triaged, BundleStatus::Proposed),
                (BundleStatus::Reviewed, BundleStatus::Triaged),
                (BundleStatus::Reviewed, BundleStatus::Proposed),
                (BundleStatus::Accepted, BundleStatus::Reviewed),
                (BundleStatus::Accepted, BundleStatus::Triaged),
                (BundleStatus::Accepted, BundleStatus::Proposed),
                (BundleStatus::Integrating, BundleStatus::Accepted),
                (BundleStatus::Integrating, BundleStatus::Reviewed),
                (BundleStatus::Integrating, BundleStatus::Triaged),
                (BundleStatus::Integrating, BundleStatus::Proposed),
            ];
            for (from, to) in &reverse_pairs {
                for role in &ALL_ROLES {
                    let r = from.validate_transition(*to, *role);
                    assert_invalid(format!("{:?}", from), format!("{:?} ({:?})", to, role), &r);
                }
            }
        }

        // --- Record serde roundtrip ---

        #[test]
        fn bundle_serde_all_statuses() {
            for status in &ALL_STATES {
                let mut b = Bundle::new("wi-1".into(), None, "branch".into(), vec!["claims".into()]);
                b.status = *status;
                let json = serde_json::to_string(&b).unwrap();
                let restored: Bundle = serde_json::from_str(&json).unwrap();
                assert_eq!(restored.status, *status);
            }
        }
    }

    // ========================================================================
    // FSM 4: TickStatus
    // States: Open, Sealing, Validating, Published, Failed
    // Terminal: Published, Failed
    // ========================================================================

    mod tick {
        use super::*;

        const ALL_STATES: [TickStatus; 5] = [
            TickStatus::Open,
            TickStatus::Sealing,
            TickStatus::Validating,
            TickStatus::Published,
            TickStatus::Failed,
        ];

        const TERMINAL: [TickStatus; 2] = [TickStatus::Published, TickStatus::Failed];

        // --- All 4 valid transitions ---

        #[test]
        fn valid_open_to_sealing() {
            let r = TickStatus::Open.validate_transition(TickStatus::Sealing, Role::Integrator);
            assert_valid("Open", "Sealing", &r);
        }

        #[test]
        fn valid_sealing_to_validating() {
            let r = TickStatus::Sealing.validate_transition(TickStatus::Validating, Role::Integrator);
            assert_valid("Sealing", "Validating", &r);
        }

        #[test]
        fn valid_validating_to_published() {
            let r = TickStatus::Validating.validate_transition(TickStatus::Published, Role::Integrator);
            assert_valid("Validating", "Published", &r);
        }

        #[test]
        fn valid_open_to_failed() {
            let r = TickStatus::Open.validate_transition(TickStatus::Failed, Role::Integrator);
            assert_valid("Open", "Failed", &r);
        }

        #[test]
        fn valid_validating_to_failed() {
            let r = TickStatus::Validating.validate_transition(TickStatus::Failed, Role::Integrator);
            assert_valid("Validating", "Failed", &r);
        }

        // --- Wrong role on every valid transition ---

        #[test]
        fn wrong_role_on_every_valid_transition() {
            let valid_pairs = [
                (TickStatus::Open, TickStatus::Sealing),
                (TickStatus::Open, TickStatus::Failed),
                (TickStatus::Sealing, TickStatus::Validating),
                (TickStatus::Sealing, TickStatus::Failed),
                (TickStatus::Validating, TickStatus::Published),
                (TickStatus::Validating, TickStatus::Failed),
            ];
            let wrong_roles = [Role::Coordinator, Role::Implementer, Role::Reviewer, Role::Researcher];

            for (from, to) in &valid_pairs {
                for role in &wrong_roles {
                    let r = from.validate_transition(*to, *role);
                    assert_invalid(format!("{:?}", from), format!("{:?} ({:?})", to, role), &r);
                }
            }
        }

        // --- Terminal states: no outbound transitions allowed ---

        #[test]
        fn terminal_states_reject_all_outbound() {
            for terminal in &TERMINAL {
                for target in &ALL_STATES {
                    if terminal == target {
                        continue;
                    }
                    for role in &ALL_ROLES {
                        let r = terminal.validate_transition(*target, *role);
                        assert_invalid(format!("{:?}", terminal), format!("{:?}", target), &r);
                    }
                }
            }
        }

        // --- Self-transitions: idempotent ---

        #[test]
        fn self_transitions_idempotent() {
            for state in &ALL_STATES {
                for role in &ALL_ROLES {
                    let r = state.validate_transition(*state, *role);
                    assert_valid(format!("{:?}", state), format!("{:?}", state), &r);
                    assert_eq!(r.unwrap(), Transition::Unchanged);
                }
            }
        }

        // --- Skip states ---

        #[test]
        fn skip_states_rejected() {
            let skip_pairs = [
                (TickStatus::Open, TickStatus::Validating),
                (TickStatus::Open, TickStatus::Published),
                // Open->Failed is now valid (crash recovery path), removed from skip list
                (TickStatus::Sealing, TickStatus::Published),
                // B3: Sealing->Failed is now valid (merge failure path), removed from skip list
            ];
            for (from, to) in &skip_pairs {
                for role in &ALL_ROLES {
                    let r = from.validate_transition(*to, *role);
                    assert_invalid(format!("{:?}", from), format!("{:?} ({:?})", to, role), &r);
                }
            }
        }

        // --- Reverse direction ---

        #[test]
        fn reverse_directions_rejected() {
            let reverse_pairs = [
                (TickStatus::Sealing, TickStatus::Open),
                (TickStatus::Validating, TickStatus::Sealing),
                (TickStatus::Validating, TickStatus::Open),
            ];
            for (from, to) in &reverse_pairs {
                for role in &ALL_ROLES {
                    let r = from.validate_transition(*to, *role);
                    assert_invalid(format!("{:?}", from), format!("{:?} ({:?})", to, role), &r);
                }
            }
        }

        // --- is_terminal() correctness ---

        #[test]
        fn is_terminal_correct() {
            assert!(!TickStatus::Open.is_terminal());
            assert!(!TickStatus::Sealing.is_terminal());
            assert!(!TickStatus::Validating.is_terminal());
            assert!(TickStatus::Published.is_terminal());
            assert!(TickStatus::Failed.is_terminal());
        }

        // --- Record serde roundtrip ---

        #[test]
        fn tick_serde_all_statuses() {
            for status in &ALL_STATES {
                let mut t = Tick::new(1);
                t.status = *status;
                let json = serde_json::to_string(&t).unwrap();
                let restored: Tick = serde_json::from_str(&json).unwrap();
                assert_eq!(restored.status, *status);
            }
        }

        // --- Full lifecycle ---

        #[test]
        fn full_lifecycle_happy_path() {
            let chain = [
                (TickStatus::Open, TickStatus::Sealing),
                (TickStatus::Sealing, TickStatus::Validating),
                (TickStatus::Validating, TickStatus::Published),
            ];
            for (from, to) in &chain {
                let r = from.validate_transition(*to, Role::Integrator);
                assert_valid(format!("{:?}", from), format!("{:?}", to), &r);
            }
        }

        #[test]
        fn full_lifecycle_failure_path() {
            let chain = [
                (TickStatus::Open, TickStatus::Sealing),
                (TickStatus::Sealing, TickStatus::Validating),
                (TickStatus::Validating, TickStatus::Failed),
            ];
            for (from, to) in &chain {
                let r = from.validate_transition(*to, Role::Integrator);
                assert_valid(format!("{:?}", from), format!("{:?}", to), &r);
            }
        }
    }

    // ========================================================================
    // FSM 5: LockStatus
    // States: Active, Released, Expired
    // Terminal: Released, Expired
    // Note: Lock uses imperative methods, not validate_transition()
    // ========================================================================

    mod lock {
        use super::*;

        // --- Valid transitions via methods ---

        #[test]
        fn valid_release_from_active() {
            let mut lock = Lock::new("file.rs".into(), "wi-1".into(), "coord".into());
            assert_eq!(lock.status, LockStatus::Active);
            lock.release();
            assert_eq!(lock.status, LockStatus::Released);
        }

        #[test]
        fn valid_expire_from_active() {
            let mut lock = Lock::new("file.rs".into(), "wi-1".into(), "coord".into());
            assert_eq!(lock.status, LockStatus::Active);
            lock.expire();
            assert_eq!(lock.status, LockStatus::Expired);
        }

        // --- is_active correctness ---

        #[test]
        fn is_active_correct() {
            let mut lock = Lock::new("file.rs".into(), "wi-1".into(), "coord".into());
            assert!(lock.is_active());
            lock.release();
            assert!(!lock.is_active());
        }

        #[test]
        fn is_active_after_expire() {
            let mut lock = Lock::new("file.rs".into(), "wi-1".into(), "coord".into());
            assert!(lock.is_active());
            lock.expire();
            assert!(!lock.is_active());
        }

        // --- is_expired correctness ---

        #[test]
        fn is_expired_no_ttl() {
            let lock = Lock::new("file.rs".into(), "wi-1".into(), "coord".into());
            assert!(!lock.is_expired());
        }

        #[test]
        fn is_expired_with_future_ttl() {
            let mut lock = Lock::new("file.rs".into(), "wi-1".into(), "coord".into());
            lock.expires_at = Some(crate::id::now_millis() + 60_000);
            assert!(!lock.is_expired());
        }

        #[test]
        fn is_expired_with_past_ttl() {
            let mut lock = Lock::new("file.rs".into(), "wi-1".into(), "coord".into());
            lock.expires_at = Some(crate::id::now_millis() - 1);
            assert!(lock.is_expired());
        }

        // --- Double-release and cross-state transitions (documenting current behavior) ---
        // Note: Lock methods don't guard against current state — these document that gap.

        #[test]
        fn double_release_succeeds_currently() {
            // KNOWN GAP: release() doesn't check current status
            let mut lock = Lock::new("file.rs".into(), "wi-1".into(), "coord".into());
            lock.release();
            assert_eq!(lock.status, LockStatus::Released);
            // This should arguably fail but currently succeeds
            lock.release();
            assert_eq!(lock.status, LockStatus::Released);
        }

        #[test]
        fn expire_after_release_succeeds_currently() {
            // KNOWN GAP: expire() doesn't check current status
            let mut lock = Lock::new("file.rs".into(), "wi-1".into(), "coord".into());
            lock.release();
            assert_eq!(lock.status, LockStatus::Released);
            // This should arguably fail but currently succeeds
            lock.expire();
            assert_eq!(lock.status, LockStatus::Expired);
        }

        #[test]
        fn release_after_expire_succeeds_currently() {
            // KNOWN GAP: release() doesn't check current status
            let mut lock = Lock::new("file.rs".into(), "wi-1".into(), "coord".into());
            lock.expire();
            assert_eq!(lock.status, LockStatus::Expired);
            // This should arguably fail but currently succeeds
            lock.release();
            assert_eq!(lock.status, LockStatus::Released);
        }

        // --- Serde roundtrip ---

        #[test]
        fn lock_serde_all_statuses() {
            for status in [LockStatus::Active, LockStatus::Released, LockStatus::Expired] {
                let mut lock = Lock::new("file.rs".into(), "wi-1".into(), "coord".into());
                lock.status = status;
                let json = serde_json::to_string(&lock).unwrap();
                let restored: Lock = serde_json::from_str(&json).unwrap();
                assert_eq!(restored.status, status);
            }
        }

        // --- updated_at changes on transition ---

        #[test]
        fn release_updates_timestamp() {
            let mut lock = Lock::new("file.rs".into(), "wi-1".into(), "coord".into());
            let before = lock.updated_at;
            std::thread::sleep(std::time::Duration::from_millis(2));
            lock.release();
            assert!(lock.updated_at >= before);
        }

        #[test]
        fn expire_updates_timestamp() {
            let mut lock = Lock::new("file.rs".into(), "wi-1".into(), "coord".into());
            let before = lock.updated_at;
            std::thread::sleep(std::time::Duration::from_millis(2));
            lock.expire();
            assert!(lock.updated_at >= before);
        }
    }

    // ========================================================================
    // FSM 6: AgentStatus
    // States: Starting, Running, WaitingForLlm, Paused, Completed, Failed, Cancelled
    // Terminal: Completed, Failed, Cancelled
    // ========================================================================

    mod agent_status {
        use super::*;

        const ALL_STATES: [AgentStatus; 7] = [
            AgentStatus::Starting,
            AgentStatus::Running,
            AgentStatus::WaitingForLlm,
            AgentStatus::Paused,
            AgentStatus::Completed,
            AgentStatus::Failed,
            AgentStatus::Cancelled,
        ];

        const TERMINAL: [AgentStatus; 3] = [AgentStatus::Completed, AgentStatus::Failed, AgentStatus::Cancelled];

        // --- All 13 valid transitions ---

        #[test]
        fn all_valid_transitions() {
            let valid = [
                (AgentStatus::Starting, AgentStatus::Running),
                (AgentStatus::Starting, AgentStatus::Failed),
                (AgentStatus::Starting, AgentStatus::Cancelled),
                (AgentStatus::Running, AgentStatus::WaitingForLlm),
                (AgentStatus::Running, AgentStatus::Paused),
                (AgentStatus::Running, AgentStatus::Completed),
                (AgentStatus::Running, AgentStatus::Failed),
                (AgentStatus::Running, AgentStatus::Cancelled),
                (AgentStatus::WaitingForLlm, AgentStatus::Running),
                (AgentStatus::WaitingForLlm, AgentStatus::Failed),
                (AgentStatus::WaitingForLlm, AgentStatus::Cancelled),
                (AgentStatus::Paused, AgentStatus::Running),
                (AgentStatus::Paused, AgentStatus::Cancelled),
            ];
            for (from, to) in &valid {
                assert!(from.can_transition_to(*to), "{:?} -> {:?} should be valid", from, to);
            }
        }

        // --- Terminal states: no outbound transitions ---

        #[test]
        fn terminal_states_reject_all_outbound() {
            for terminal in &TERMINAL {
                for target in &ALL_STATES {
                    assert!(
                        !terminal.can_transition_to(*target),
                        "{:?} -> {:?} should be INVALID (terminal)",
                        terminal,
                        target
                    );
                }
            }
        }

        // --- Self-transitions: always rejected ---

        #[test]
        fn self_transitions_rejected() {
            for state in &ALL_STATES {
                assert!(
                    !state.can_transition_to(*state),
                    "{:?} -> {:?} self-transition should be INVALID",
                    state,
                    state
                );
            }
        }

        // --- Invalid non-terminal transitions ---

        #[test]
        fn invalid_transitions() {
            let invalid = [
                // Starting cannot go to these
                (AgentStatus::Starting, AgentStatus::WaitingForLlm),
                (AgentStatus::Starting, AgentStatus::Paused),
                (AgentStatus::Starting, AgentStatus::Completed),
                // WaitingForLlm cannot go to these
                (AgentStatus::WaitingForLlm, AgentStatus::Paused),
                (AgentStatus::WaitingForLlm, AgentStatus::Completed),
                (AgentStatus::WaitingForLlm, AgentStatus::Starting),
                // Paused cannot go to these
                (AgentStatus::Paused, AgentStatus::WaitingForLlm),
                (AgentStatus::Paused, AgentStatus::Completed),
                (AgentStatus::Paused, AgentStatus::Failed),
                (AgentStatus::Paused, AgentStatus::Starting),
                // Running cannot go back to Starting
                (AgentStatus::Running, AgentStatus::Starting),
            ];
            for (from, to) in &invalid {
                assert!(!from.can_transition_to(*to), "{:?} -> {:?} should be INVALID", from, to);
            }
        }

        // --- is_terminal correctness ---

        #[test]
        fn is_terminal_correct() {
            assert!(!AgentStatus::Starting.is_terminal());
            assert!(!AgentStatus::Running.is_terminal());
            assert!(!AgentStatus::WaitingForLlm.is_terminal());
            assert!(!AgentStatus::Paused.is_terminal());
            assert!(AgentStatus::Completed.is_terminal());
            assert!(AgentStatus::Failed.is_terminal());
            assert!(AgentStatus::Cancelled.is_terminal());
        }

        // --- Full lifecycle chains ---

        #[test]
        fn lifecycle_happy_path() {
            let chain = [
                AgentStatus::Starting,
                AgentStatus::Running,
                AgentStatus::WaitingForLlm,
                AgentStatus::Running,
                AgentStatus::Completed,
            ];
            for window in chain.windows(2) {
                assert!(
                    window[0].can_transition_to(window[1]),
                    "{:?} -> {:?} should be valid in lifecycle",
                    window[0],
                    window[1]
                );
            }
        }

        #[test]
        fn lifecycle_pause_resume() {
            let chain = [
                AgentStatus::Starting,
                AgentStatus::Running,
                AgentStatus::Paused,
                AgentStatus::Running,
                AgentStatus::Completed,
            ];
            for window in chain.windows(2) {
                assert!(
                    window[0].can_transition_to(window[1]),
                    "{:?} -> {:?} should be valid",
                    window[0],
                    window[1]
                );
            }
        }

        #[test]
        fn lifecycle_failure_during_llm() {
            let chain = [
                AgentStatus::Starting,
                AgentStatus::Running,
                AgentStatus::WaitingForLlm,
                AgentStatus::Failed,
            ];
            for window in chain.windows(2) {
                assert!(
                    window[0].can_transition_to(window[1]),
                    "{:?} -> {:?} should be valid",
                    window[0],
                    window[1]
                );
            }
        }

        #[test]
        fn lifecycle_cancel_from_pause() {
            let chain = [
                AgentStatus::Starting,
                AgentStatus::Running,
                AgentStatus::Paused,
                AgentStatus::Cancelled,
            ];
            for window in chain.windows(2) {
                assert!(
                    window[0].can_transition_to(window[1]),
                    "{:?} -> {:?} should be valid",
                    window[0],
                    window[1]
                );
            }
        }
    }

    // ========================================================================
    // Handler-level dispatch tests: full lifecycle through IPC dispatch
    // ========================================================================

    mod dispatch_lifecycle {
        use std::sync::Arc;

        use serde_json::json;
        use tokio::sync::broadcast;

        use crate::config::IntegratorConfig;
        use crate::daemon::context::Stores;
        use crate::daemon::handlers::dispatch;
        use crate::ipc::protocol::{DaemonEvent, DaemonRequest};
        use crate::worktree::manager::WorktreeManager;

        use std::path::PathBuf;

        fn setup() -> (
            Arc<Stores>,
            broadcast::Sender<DaemonEvent>,
            WorktreeManager,
            IntegratorConfig,
        ) {
            let stores = Arc::new(Stores::new());
            let (tx, _) = broadcast::channel(64);
            let wm = WorktreeManager::new(PathBuf::from("/tmp/noop"), PathBuf::from("/tmp/noop-wt"));
            let ic = IntegratorConfig {
                validation_commands: vec!["echo ok".to_string()],
                ..Default::default()
            };
            (stores, tx, wm, ic)
        }

        fn dispatch_ok(
            stores: &Arc<Stores>,
            tx: &broadcast::Sender<DaemonEvent>,
            wm: &WorktreeManager,
            ic: &IntegratorConfig,
            method: &str,
            params: serde_json::Value,
        ) -> serde_json::Value {
            let req = DaemonRequest::new(1, method, params);
            let resp = dispatch(stores, tx, wm, ic, req);
            assert!(!resp.is_error(), "{method} failed: {:?}", resp.error);
            resp.result.unwrap()
        }

        fn dispatch_err(
            stores: &Arc<Stores>,
            tx: &broadcast::Sender<DaemonEvent>,
            wm: &WorktreeManager,
            ic: &IntegratorConfig,
            method: &str,
            params: serde_json::Value,
        ) -> i32 {
            let req = DaemonRequest::new(1, method, params);
            let resp = dispatch(stores, tx, wm, ic, req);
            assert!(
                resp.is_error(),
                "{method} expected error but got success: {:?}",
                resp.result
            );
            resp.error.unwrap().code
        }

        // --- Plan full lifecycle through dispatch ---

        #[test]
        fn plan_full_lifecycle_through_dispatch() {
            let (s, tx, wm, ic) = setup();
            let plan = dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "plan.create",
                json!({"title": "P", "description": "D"}),
            );
            let id = plan["id"].as_str().unwrap().to_string();

            // Draft -> Active
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "plan.transition",
                json!({"id": id, "target_status": "active"}),
            );
            let got = dispatch_ok(&s, &tx, &wm, &ic, "plan.get", json!({"id": id}));
            assert_eq!(got["status"], "active");

            // Active -> Complete
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "plan.transition",
                json!({"id": id, "target_status": "complete"}),
            );
            let got = dispatch_ok(&s, &tx, &wm, &ic, "plan.get", json!({"id": id}));
            assert_eq!(got["status"], "complete");

            // Complete -> anything should fail
            let code = dispatch_err(
                &s,
                &tx,
                &wm,
                &ic,
                "plan.transition",
                json!({"id": id, "target_status": "active"}),
            );
            assert_eq!(code, -32000); // transition_rejected
        }

        #[test]
        fn plan_abandon_from_draft_through_dispatch() {
            let (s, tx, wm, ic) = setup();
            let plan = dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "plan.create",
                json!({"title": "P", "description": "D"}),
            );
            let id = plan["id"].as_str().unwrap().to_string();

            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "plan.transition",
                json!({"id": id, "target_status": "abandoned"}),
            );
            let got = dispatch_ok(&s, &tx, &wm, &ic, "plan.get", json!({"id": id}));
            assert_eq!(got["status"], "abandoned");

            // Abandoned -> anything should fail
            let code = dispatch_err(
                &s,
                &tx,
                &wm,
                &ic,
                "plan.transition",
                json!({"id": id, "target_status": "active"}),
            );
            assert_eq!(code, -32000);
        }

        // --- Work full lifecycle through dispatch ---

        #[test]
        fn work_full_lifecycle_through_dispatch() {
            let (s, tx, wm, ic) = setup();

            // Create hierarchy
            let plan = dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "plan.create",
                json!({"title": "P", "description": "D"}),
            );
            let plan_id = plan["id"].as_str().unwrap();
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "plan.transition",
                json!({"id": plan_id, "target_status": "active"}),
            );

            let spec = dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "spec.create",
                json!({"plan_id": plan_id, "title": "S", "description": "D"}),
            );
            let spec_id = spec["id"].as_str().unwrap();
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "spec.transition",
                json!({"id": spec_id, "target_status": "active"}),
            );

            let phase = dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "phase.create",
                json!({"spec_id": spec_id, "title": "Ph", "description": "D"}),
            );
            let phase_id = phase["id"].as_str().unwrap();
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "phase.transition",
                json!({"id": phase_id, "target_status": "active"}),
            );

            let wi = dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "work.create",
                json!({"phase_id": phase_id, "title": "WI", "description": "D", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            );
            let wi_id = wi["id"].as_str().unwrap().to_string();

            // Ready -> InProgress -> InReview -> Integrated -> Done
            // (auto-promoted from Draft to Ready since acceptance_criteria present)
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "work.transition",
                json!({"id": wi_id, "target_status": "InProgress", "role": "coordinator", "assignee": "agent-1"}),
            );

            // Create bundle before InReview (precondition)
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "bundle.create",
                json!({"work_id": wi_id, "branch_name": "feature/test"}),
            );

            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "work.transition",
                json!({"id": wi_id, "target_status": "InReview", "role": "implementer"}),
            );
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "work.transition",
                json!({"id": wi_id, "target_status": "Integrated", "role": "integrator"}),
            );
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "work.transition",
                json!({"id": wi_id, "target_status": "Done", "role": "coordinator"}),
            );

            let got = dispatch_ok(&s, &tx, &wm, &ic, "work.get", json!({"id": wi_id}));
            assert_eq!(got["status"], "Done");

            // Done -> anything should fail
            let code = dispatch_err(
                &s,
                &tx,
                &wm,
                &ic,
                "work.transition",
                json!({"id": wi_id, "target_status": "Ready", "role": "coordinator"}),
            );
            assert_eq!(code, -32000);
        }

        // --- Bundle full lifecycle through dispatch ---

        #[test]
        fn bundle_full_lifecycle_through_dispatch() {
            let (s, tx, wm, ic) = setup();

            // Create hierarchy + work item
            let plan = dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "plan.create",
                json!({"title": "P", "description": "D"}),
            );
            let plan_id = plan["id"].as_str().unwrap();
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "plan.transition",
                json!({"id": plan_id, "target_status": "active"}),
            );
            let spec = dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "spec.create",
                json!({"plan_id": plan_id, "title": "S", "description": "D"}),
            );
            let spec_id = spec["id"].as_str().unwrap();
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "spec.transition",
                json!({"id": spec_id, "target_status": "active"}),
            );
            let phase = dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "phase.create",
                json!({"spec_id": spec_id, "title": "Ph", "description": "D"}),
            );
            let phase_id = phase["id"].as_str().unwrap();
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "phase.transition",
                json!({"id": phase_id, "target_status": "active"}),
            );
            let wi = dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "work.create",
                json!({"phase_id": phase_id, "title": "WI", "description": "D", "resource_tags": ["src/"]}),
            );
            let wi_id = wi["id"].as_str().unwrap();

            let bundle = dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "bundle.create",
                json!({"work_id": wi_id, "branch_name": "feature/test"}),
            );
            let bid = bundle["id"].as_str().unwrap().to_string();
            assert_eq!(bundle["status"], "Proposed");

            // Proposed -> Triaged -> Reviewed -> Accepted -> Integrating -> Merged
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "bundle.transition",
                json!({"id": bid, "target_status": "Triaged", "role": "coordinator"}),
            );
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "bundle.transition",
                json!({"id": bid, "target_status": "Reviewed", "role": "reviewer", "verification": "tests passed"}),
            );
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "bundle.transition",
                json!({"id": bid, "target_status": "Accepted", "role": "coordinator"}),
            );
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "bundle.transition",
                json!({"id": bid, "target_status": "Integrating", "role": "integrator"}),
            );
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "bundle.transition",
                json!({"id": bid, "target_status": "Merged", "role": "integrator"}),
            );

            let got = dispatch_ok(&s, &tx, &wm, &ic, "bundle.get", json!({"id": bid}));
            assert_eq!(got["status"], "Merged");

            // Merged -> anything should fail
            let code = dispatch_err(
                &s,
                &tx,
                &wm,
                &ic,
                "bundle.transition",
                json!({"id": bid, "target_status": "Proposed", "role": "coordinator"}),
            );
            assert_eq!(code, -32000);
        }

        // --- Bundle rejection flow ---

        #[test]
        fn bundle_rejection_at_every_stage() {
            let (s, tx, wm, ic) = setup();
            let plan = dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "plan.create",
                json!({"title": "P", "description": "D"}),
            );
            let plan_id = plan["id"].as_str().unwrap();
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "plan.transition",
                json!({"id": plan_id, "target_status": "active"}),
            );
            let spec = dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "spec.create",
                json!({"plan_id": plan_id, "title": "S", "description": "D"}),
            );
            let spec_id = spec["id"].as_str().unwrap();
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "spec.transition",
                json!({"id": spec_id, "target_status": "active"}),
            );
            let phase = dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "phase.create",
                json!({"spec_id": spec_id, "title": "Ph", "description": "D"}),
            );
            let phase_id = phase["id"].as_str().unwrap();
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "phase.transition",
                json!({"id": phase_id, "target_status": "active"}),
            );
            let wi = dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "work.create",
                json!({"phase_id": phase_id, "title": "WI", "description": "D", "resource_tags": ["src/"]}),
            );
            let wi_id = wi["id"].as_str().unwrap();

            // Reject from Proposed (Reviewer)
            let b1 = dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "bundle.create",
                json!({"work_id": wi_id, "branch_name": "f/1"}),
            );
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "bundle.transition",
                json!({"id": b1["id"].as_str().unwrap(), "target_status": "Rejected", "role": "reviewer"}),
            );

            // Reject from Triaged (Coordinator)
            let b2 = dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "bundle.create",
                json!({"work_id": wi_id, "branch_name": "f/2"}),
            );
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "bundle.transition",
                json!({"id": b2["id"].as_str().unwrap(), "target_status": "Triaged", "role": "coordinator"}),
            );
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "bundle.transition",
                json!({"id": b2["id"].as_str().unwrap(), "target_status": "Rejected", "role": "coordinator"}),
            );

            // Reject from Reviewed (Reviewer)
            let b3 = dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "bundle.create",
                json!({"work_id": wi_id, "branch_name": "f/3"}),
            );
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "bundle.transition",
                json!({"id": b3["id"].as_str().unwrap(), "target_status": "Triaged", "role": "coordinator"}),
            );
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "bundle.transition",
                json!({"id": b3["id"].as_str().unwrap(), "target_status": "Reviewed", "role": "reviewer", "verification": "tests passed"}),
            );
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "bundle.transition",
                json!({"id": b3["id"].as_str().unwrap(), "target_status": "Rejected", "role": "reviewer"}),
            );
        }

        // --- Tick full lifecycle through dispatch ---

        #[test]
        fn tick_full_lifecycle_through_dispatch() {
            let (s, tx, wm, ic) = setup();

            let tick = dispatch_ok(&s, &tx, &wm, &ic, "tick.create", json!({"number": 1}));
            let tid = tick["id"].as_str().unwrap().to_string();
            assert_eq!(tick["status"], "Open");

            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "tick.transition",
                json!({"id": tid, "target_status": "Sealing", "role": "integrator"}),
            );
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "tick.transition",
                json!({"id": tid, "target_status": "Validating", "role": "integrator"}),
            );
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "tick.transition",
                json!({"id": tid, "target_status": "Published", "role": "integrator"}),
            );

            let got = dispatch_ok(&s, &tx, &wm, &ic, "tick.get", json!({"id": tid}));
            assert_eq!(got["status"], "Published");

            // Published -> anything should fail
            let code = dispatch_err(
                &s,
                &tx,
                &wm,
                &ic,
                "tick.transition",
                json!({"id": tid, "target_status": "Open", "role": "integrator"}),
            );
            assert_eq!(code, -32000);
        }

        #[test]
        fn tick_failure_path_through_dispatch() {
            let (s, tx, wm, ic) = setup();

            let tick = dispatch_ok(&s, &tx, &wm, &ic, "tick.create", json!({"number": 1}));
            let tid = tick["id"].as_str().unwrap().to_string();

            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "tick.transition",
                json!({"id": tid, "target_status": "Sealing", "role": "integrator"}),
            );
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "tick.transition",
                json!({"id": tid, "target_status": "Validating", "role": "integrator"}),
            );
            dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "tick.transition",
                json!({"id": tid, "target_status": "Failed", "role": "integrator"}),
            );

            let got = dispatch_ok(&s, &tx, &wm, &ic, "tick.get", json!({"id": tid}));
            assert_eq!(got["status"], "Failed");
        }

        // --- Wrong role through dispatch ---

        #[test]
        fn wrong_role_rejected_through_dispatch() {
            let (s, tx, wm, ic) = setup();

            let plan = dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "plan.create",
                json!({"title": "P", "description": "D"}),
            );
            let id = plan["id"].as_str().unwrap();

            // Implementer cannot transition plans
            let code = dispatch_err(
                &s,
                &tx,
                &wm,
                &ic,
                "plan.transition",
                json!({"id": id, "target_status": "active", "role": "implementer"}),
            );
            assert_eq!(code, -32000);
        }

        // --- Lock lifecycle through dispatch ---

        #[test]
        fn lock_full_lifecycle_through_dispatch() {
            let (s, tx, wm, ic) = setup();

            let lock = dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "lock.create",
                json!({"resource": "src/main.rs", "holder_id": "wi-1", "granted_by": "coord"}),
            );
            let lid = lock["id"].as_str().unwrap().to_string();
            assert_eq!(lock["status"], "active");

            dispatch_ok(&s, &tx, &wm, &ic, "lock.release", json!({"id": lid}));
            let got = dispatch_ok(&s, &tx, &wm, &ic, "lock.get", json!({"id": lid}));
            assert_eq!(got["status"], "released");
        }

        #[test]
        fn lock_expire_through_dispatch() {
            let (s, tx, wm, ic) = setup();

            let lock = dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "lock.create",
                json!({"resource": "src/main.rs", "holder_id": "wi-1", "granted_by": "coord"}),
            );
            let lid = lock["id"].as_str().unwrap().to_string();

            dispatch_ok(&s, &tx, &wm, &ic, "lock.expire", json!({"id": lid}));
            let got = dispatch_ok(&s, &tx, &wm, &ic, "lock.get", json!({"id": lid}));
            assert_eq!(got["status"], "expired");
        }

        // --- Tick singleton guard ---

        #[test]
        fn tick_singleton_guard() {
            let (s, tx, wm, ic) = setup();

            dispatch_ok(&s, &tx, &wm, &ic, "tick.create", json!({"number": 1}));
            // Second non-terminal tick should be rejected
            let code = dispatch_err(&s, &tx, &wm, &ic, "tick.create", json!({"number": 2}));
            assert_eq!(code, -32005); // precondition_failed
        }

        // --- Learning lifecycle through dispatch ---

        #[test]
        fn learning_reinforce_contradict_promote_demote() {
            let (s, tx, wm, ic) = setup();

            let learning = dispatch_ok(
                &s,
                &tx,
                &wm,
                &ic,
                "learning.create",
                json!({"content": "Always run tests", "scope": "global", "source_id": "plan-1"}),
            );
            let lid = learning["id"].as_str().unwrap().to_string();
            assert_eq!(learning["reinforcements"], 0);
            assert_eq!(learning["contradictions"], 0);

            // Reinforce 3 times
            for _ in 0..3 {
                dispatch_ok(&s, &tx, &wm, &ic, "learning.reinforce", json!({"id": lid}));
            }
            let got = dispatch_ok(&s, &tx, &wm, &ic, "learning.get", json!({"id": lid}));
            assert_eq!(got["reinforcements"], 3);

            // Contradict
            dispatch_ok(&s, &tx, &wm, &ic, "learning.contradict", json!({"id": lid}));
            let got = dispatch_ok(&s, &tx, &wm, &ic, "learning.get", json!({"id": lid}));
            assert_eq!(got["contradictions"], 1);

            // Promote
            dispatch_ok(&s, &tx, &wm, &ic, "learning.promote", json!({"id": lid}));
            let got = dispatch_ok(&s, &tx, &wm, &ic, "learning.get", json!({"id": lid}));
            assert_eq!(got["promoted"], true);

            // Demote
            dispatch_ok(&s, &tx, &wm, &ic, "learning.demote", json!({"id": lid}));
            let got = dispatch_ok(&s, &tx, &wm, &ic, "learning.get", json!({"id": lid}));
            assert_eq!(got["promoted"], false);
        }
    }
}
