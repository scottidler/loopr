#![allow(clippy::unwrap_used)]

//! Seam tests for `Work` + `WorkStatus` + `AcceptanceCriteria`. Exercises
//! the crate's public API as an external consumer (integration tests
//! compile as a separate crate).

use domain::{AcceptanceCriteria, FsmErrorKind, PlanId, Role, Transition, Work, WorkStatus};
use taskstore_traits::{IndexValue, Record};

// ---------------------------------------------------------------------------
// AcceptanceCriteria
// ---------------------------------------------------------------------------

#[test]
fn ac_default_is_empty() {
    let ac = AcceptanceCriteria::default();
    assert!(ac.is_empty());
    assert_eq!(ac.len(), 0);
}

#[test]
fn ac_len_matches_inner() {
    let ac = AcceptanceCriteria::from_texts(vec!["a".into(), "b".into(), "c".into()]);
    assert_eq!(ac.len(), 3);
    assert!(!ac.is_empty());
}

#[test]
fn ac_serde_structured_wire_form() {
    // Phase 8: the wire form is now a bare JSON array of `{id,text}`
    // objects (was a bare string array pre-Phase-8). `from_texts` mints
    // sequential 1-based ids. This inverts the old
    // `ac_serde_transparent_wire_form` assertion by name.
    let ac = AcceptanceCriteria::from_texts(vec!["first".into(), "second".into()]);
    let json = serde_json::to_string(&ac).unwrap();
    assert_eq!(json, r#"[{"id":1,"text":"first"},{"id":2,"text":"second"}]"#);
}

#[test]
fn ac_serde_backcompat_old_string_array() {
    // Break-to-prove back-compat: a pre-Phase-8 `works.jsonl` stored the
    // criteria as a bare string array. That exact on-disk shape MUST still
    // load, its entries becoming Criteria with sequential 1-based ids.
    let old = r#"["first","second"]"#;
    let ac: AcceptanceCriteria = serde_json::from_str(old).unwrap();
    let expect = AcceptanceCriteria::from_texts(vec!["first".into(), "second".into()]);
    assert_eq!(ac, expect);
    assert_eq!(ac.iter().map(|c| c.id).collect::<Vec<_>>(), vec![1, 2]);
}

#[test]
fn ac_serde_roundtrip() {
    let ac = AcceptanceCriteria::from_texts(vec!["a".into(), "b".into()]);
    let json = serde_json::to_string(&ac).unwrap();
    let back: AcceptanceCriteria = serde_json::from_str(&json).unwrap();
    assert_eq!(ac, back);
}

#[test]
fn ac_unicode_roundtrip() {
    let ac = AcceptanceCriteria::from_texts(vec!["emoji: \u{1f389}".into(), "\u{65e5}\u{672c}\u{8a9e}".into()]);
    let json = serde_json::to_string(&ac).unwrap();
    let back: AcceptanceCriteria = serde_json::from_str(&json).unwrap();
    assert_eq!(ac, back);
}

// ---------------------------------------------------------------------------
// Work::new
// ---------------------------------------------------------------------------

#[test]
fn work_new_defaults_to_pending() {
    // Scope memo U+1 / memory project-reactive-execution-model: new
    // Works start Pending, not Draft.
    let work = Work::new(PlanId::new(), "impl --version".to_string());
    assert_eq!(work.status(), WorkStatus::Pending);
}

#[test]
fn work_new_preserves_parent_id() {
    let pid = PlanId::new();
    let work = Work::new(pid.clone(), "t".to_string());
    assert_eq!(work.parent_id, pid);
}

#[test]
fn work_new_preserves_title() {
    let work = Work::new(PlanId::new(), "title text".to_string());
    assert_eq!(work.title, "title text");
}

#[test]
fn work_new_id_has_wk_prefix() {
    let work = Work::new(PlanId::new(), "t".to_string());
    assert!(
        work.id.as_ref().starts_with("wk-"),
        "WorkId must be wk-prefixed: {}",
        work.id
    );
}

#[test]
fn work_new_created_at_equals_updated_at() {
    let work = Work::new(PlanId::new(), "t".to_string());
    assert_eq!(work.created_at, work.updated_at);
}

#[test]
fn work_new_distinct_calls_produce_distinct_ids() {
    let a = Work::new(PlanId::new(), "a".to_string());
    let b = Work::new(PlanId::new(), "b".to_string());
    assert_ne!(a.id, b.id);
}

// ---------------------------------------------------------------------------
// Work serde
// ---------------------------------------------------------------------------

#[test]
fn work_serde_roundtrip_json() {
    let work = Work::new(PlanId::new(), "t".to_string());
    let json = serde_json::to_string(&work).unwrap();
    let back: Work = serde_json::from_str(&json).unwrap();
    assert_eq!(work.id, back.id);
    assert_eq!(work.parent_id, back.parent_id);
    assert_eq!(work.title, back.title);
    assert_eq!(work.status, back.status);
    assert_eq!(work.created_at, back.created_at);
    assert_eq!(work.updated_at, back.updated_at);
    assert_eq!(work.assignee, back.assignee);
    assert_eq!(work.dependencies, back.dependencies);
    assert_eq!(work.files, back.files);
    assert_eq!(work.acceptance_criteria, back.acceptance_criteria);
    assert_eq!(work.attempt_count, back.attempt_count);
    assert_eq!(work.session_failure_count, back.session_failure_count);
}

#[test]
fn work_serde_rejects_unknown_fields() {
    let bogus = r#"{
        "id": "wk-abc12",
        "parent_id": "pl-xyz34",
        "updated_at": 1700000000000,
        "created_at": 1700000000000,
        "title": "t",
        "status": "pending",
        "bogus_field": "fail"
    }"#;
    let result: Result<Work, _> = serde_json::from_str(bogus);
    assert!(result.is_err(), "deny_unknown_fields must reject extra keys");
}

#[test]
fn work_serde_accepts_minimal_json() {
    // Every optional-ish field carries #[serde(default)] so older
    // JSONL written before a later stage added a field still loads.
    let minimal = r#"{
        "id": "wk-abc12",
        "parent_id": "pl-xyz34",
        "updated_at": 1700000000000,
        "created_at": 1700000000000,
        "title": "t",
        "status": "pending"
    }"#;
    let w: Work = serde_json::from_str(minimal).unwrap();
    assert_eq!(w.assignee, None);
    assert!(w.dependencies.is_empty());
    assert!(w.files.is_empty());
    assert_eq!(w.acceptance_criteria.len(), 0);
    assert_eq!(w.attempt_count, 0);
    assert_eq!(w.session_failure_count, 0);
}

#[test]
fn work_serde_backcompat_old_string_criteria() {
    // Break-to-prove: a full `works.jsonl` row written before Phase 8
    // stored `acceptance_criteria` as a bare string array. The whole Work
    // MUST still deserialize, its criteria becoming `{id,text}` with
    // sequential 1-based ids.
    let old_row = r#"{
        "id": "wk-abc12",
        "parent_id": "pl-xyz34",
        "updated_at": 1700000000000,
        "created_at": 1700000000000,
        "title": "t",
        "status": "pending",
        "acceptance_criteria": ["module exists", "tests pass"]
    }"#;
    let w: Work = serde_json::from_str(old_row).unwrap();
    assert_eq!(w.acceptance_criteria.len(), 2);
    let ids: Vec<u32> = w.acceptance_criteria.iter().map(|c| c.id).collect();
    assert_eq!(ids, vec![1, 2]);
    let texts: Vec<&str> = w.acceptance_criteria.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(texts, vec!["module exists", "tests pass"]);
}

#[test]
fn work_serde_status_wire_form_lowercase() {
    // Pins #[serde(rename_all = "lowercase")] on WorkStatus.
    let work = Work::new(PlanId::new(), "t".to_string());
    let json = serde_json::to_string(&work).unwrap();
    assert!(
        json.contains("\"status\":\"pending\""),
        "status wire form must be lowercase: {json}"
    );
}

#[test]
fn work_serde_unknown_status_rejects() {
    let bogus = r#"{
        "id": "wk-abc12",
        "parent_id": "pl-xyz34",
        "updated_at": 1700000000000,
        "created_at": 1700000000000,
        "title": "t",
        "status": "not-a-state"
    }"#;
    let result: Result<Work, _> = serde_json::from_str(bogus);
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// WorkStatus Display
// ---------------------------------------------------------------------------

#[test]
fn work_status_display_lowercase() {
    // Display output (used by #[derive(Record)] to populate the index
    // map) must match the serde wire form. Divergence here silently
    // breaks any future query layer that matches index against raw JSON.
    assert_eq!(format!("{}", WorkStatus::Draft), "draft");
    assert_eq!(format!("{}", WorkStatus::Pending), "pending");
    assert_eq!(format!("{}", WorkStatus::Ready), "ready");
    assert_eq!(format!("{}", WorkStatus::InProgress), "inprogress");
    assert_eq!(format!("{}", WorkStatus::Blocked), "blocked");
    assert_eq!(format!("{}", WorkStatus::InReview), "inreview");
    assert_eq!(format!("{}", WorkStatus::Integrated), "integrated");
    assert_eq!(format!("{}", WorkStatus::Done), "done");
    assert_eq!(format!("{}", WorkStatus::Superseded), "superseded");
    assert_eq!(format!("{}", WorkStatus::Abandoned), "abandoned");
}

// ---------------------------------------------------------------------------
// Record trait impl
// ---------------------------------------------------------------------------

#[test]
fn work_record_id_matches_as_ref() {
    let work = Work::new(PlanId::new(), "t".to_string());
    assert_eq!(<Work as Record>::id(&work), work.id.as_ref());
}

#[test]
fn work_record_updated_at_matches_field() {
    let work = Work::new(PlanId::new(), "t".to_string());
    assert_eq!(<Work as Record>::updated_at(&work), work.updated_at);
}

#[test]
fn work_record_collection_name() {
    assert_eq!(Work::collection_name(), "works");
}

#[test]
fn work_record_indexed_fields_two_entries() {
    // Load-bearing consistency check: the index map values must match
    // the JSON wire form exactly. A typo or rename that drifts one
    // without the other silently breaks any query that cross-references
    // the index against the persisted record.
    let pid = PlanId::new();
    let work = Work::new(pid.clone(), "t".to_string());
    let fields = work.indexed_fields();
    assert_eq!(fields.len(), 2, "exactly two indexed fields expected");
    assert_eq!(
        fields.get("status"),
        Some(&IndexValue::String("pending".to_string())),
        "indexed status must use lowercase Display output"
    );
    assert_eq!(
        fields.get("parent_id"),
        Some(&IndexValue::String(pid.as_ref().to_string())),
        "indexed parent_id must match PlanId wire form"
    );
}

#[test]
fn work_record_indexed_status_tracks_transitions() {
    let mut work = Work::new(PlanId::new(), "t".to_string());
    work.transition(WorkStatus::Ready, Role::Reactor).unwrap();
    let fields = work.indexed_fields();
    assert_eq!(
        fields.get("status"),
        Some(&IndexValue::String("ready".to_string())),
        "indexed status must reflect current state after transition"
    );
}

// ---------------------------------------------------------------------------
// FSM transitions - happy path (one test per transition edge, 25 total).
// Role choice for multi-role edges spreads coverage: Director for the
// cascade edges (Superseded/Abandoned), Implementer/Integrator for the
// edges that define those roles' whole contract.
// ---------------------------------------------------------------------------

fn work_in(status: WorkStatus) -> Work {
    let mut work = Work::new(PlanId::new(), "t".to_string());
    work.status = status;
    work
}

fn assert_changed(w: &mut Work, to: WorkStatus, role: Role) {
    let before = w.updated_at;
    std::thread::sleep(std::time::Duration::from_millis(2));
    let result = w.transition(to, role).unwrap();
    assert_eq!(
        result,
        Transition::Changed,
        "expected Changed for {:?} -> {:?} by {:?}",
        w.status,
        to,
        role
    );
    assert_eq!(w.status, to, "status must mutate");
    assert!(w.updated_at > before, "updated_at must advance");
}

#[test]
fn transition_draft_pending_by_reactor() {
    assert_changed(&mut work_in(WorkStatus::Draft), WorkStatus::Pending, Role::Reactor);
}
#[test]
fn transition_draft_ready_by_reactor() {
    assert_changed(&mut work_in(WorkStatus::Draft), WorkStatus::Ready, Role::Reactor);
}
#[test]
fn transition_draft_superseded_by_director() {
    assert_changed(&mut work_in(WorkStatus::Draft), WorkStatus::Superseded, Role::Director);
}
#[test]
fn transition_draft_abandoned_by_director() {
    assert_changed(&mut work_in(WorkStatus::Draft), WorkStatus::Abandoned, Role::Director);
}
#[test]
fn transition_pending_ready_by_reactor() {
    assert_changed(&mut work_in(WorkStatus::Pending), WorkStatus::Ready, Role::Reactor);
}
#[test]
fn transition_pending_blocked_by_reactor() {
    assert_changed(&mut work_in(WorkStatus::Pending), WorkStatus::Blocked, Role::Reactor);
}
#[test]
fn transition_pending_blocked_rejected_by_director() {
    let mut w = work_in(WorkStatus::Pending);
    assert!(
        w.transition(WorkStatus::Blocked, Role::Director).is_err(),
        "Pending => Blocked must require Reactor, not Director"
    );
}
#[test]
fn transition_pending_superseded_by_director() {
    assert_changed(
        &mut work_in(WorkStatus::Pending),
        WorkStatus::Superseded,
        Role::Director,
    );
}
#[test]
fn transition_pending_abandoned_by_reactor() {
    assert_changed(&mut work_in(WorkStatus::Pending), WorkStatus::Abandoned, Role::Reactor);
}
#[test]
fn transition_ready_inprogress_by_reactor() {
    assert_changed(&mut work_in(WorkStatus::Ready), WorkStatus::InProgress, Role::Reactor);
}
#[test]
fn transition_ready_blocked_by_reactor() {
    assert_changed(&mut work_in(WorkStatus::Ready), WorkStatus::Blocked, Role::Reactor);
}
#[test]
fn transition_ready_superseded_by_director() {
    assert_changed(&mut work_in(WorkStatus::Ready), WorkStatus::Superseded, Role::Director);
}
#[test]
fn transition_ready_abandoned_by_director() {
    assert_changed(&mut work_in(WorkStatus::Ready), WorkStatus::Abandoned, Role::Director);
}
#[test]
fn transition_inprogress_blocked_by_implementer() {
    assert_changed(
        &mut work_in(WorkStatus::InProgress),
        WorkStatus::Blocked,
        Role::Implementer,
    );
}
#[test]
fn transition_inprogress_inreview_by_implementer() {
    assert_changed(
        &mut work_in(WorkStatus::InProgress),
        WorkStatus::InReview,
        Role::Implementer,
    );
}
#[test]
fn transition_inprogress_superseded_by_director() {
    assert_changed(
        &mut work_in(WorkStatus::InProgress),
        WorkStatus::Superseded,
        Role::Director,
    );
}
#[test]
fn transition_inprogress_abandoned_by_director() {
    assert_changed(
        &mut work_in(WorkStatus::InProgress),
        WorkStatus::Abandoned,
        Role::Director,
    );
}
#[test]
fn transition_blocked_ready_by_reactor() {
    assert_changed(&mut work_in(WorkStatus::Blocked), WorkStatus::Ready, Role::Reactor);
}
#[test]
fn transition_blocked_superseded_by_director() {
    assert_changed(
        &mut work_in(WorkStatus::Blocked),
        WorkStatus::Superseded,
        Role::Director,
    );
}
#[test]
fn transition_blocked_abandoned_by_reactor() {
    assert_changed(&mut work_in(WorkStatus::Blocked), WorkStatus::Abandoned, Role::Reactor);
}
#[test]
fn transition_inreview_inprogress_by_reactor() {
    assert_changed(
        &mut work_in(WorkStatus::InReview),
        WorkStatus::InProgress,
        Role::Reactor,
    );
}
#[test]
fn transition_inreview_integrated_by_integrator() {
    assert_changed(
        &mut work_in(WorkStatus::InReview),
        WorkStatus::Integrated,
        Role::Integrator,
    );
}
#[test]
fn transition_inreview_superseded_by_director() {
    assert_changed(
        &mut work_in(WorkStatus::InReview),
        WorkStatus::Superseded,
        Role::Director,
    );
}
#[test]
fn transition_inreview_abandoned_by_director() {
    assert_changed(
        &mut work_in(WorkStatus::InReview),
        WorkStatus::Abandoned,
        Role::Director,
    );
}
#[test]
fn transition_integrated_done_by_reactor() {
    assert_changed(&mut work_in(WorkStatus::Integrated), WorkStatus::Done, Role::Reactor);
}
#[test]
fn transition_integrated_superseded_by_director() {
    assert_changed(
        &mut work_in(WorkStatus::Integrated),
        WorkStatus::Superseded,
        Role::Director,
    );
}
#[test]
fn transition_integrated_abandoned_by_director() {
    assert_changed(
        &mut work_in(WorkStatus::Integrated),
        WorkStatus::Abandoned,
        Role::Director,
    );
}

// ---------------------------------------------------------------------------
// FSM transitions - reject paths
// ---------------------------------------------------------------------------

#[test]
fn transition_wrong_role_rejects() {
    // Draft -> Pending is Reactor-only; Implementer must reject.
    let mut w = work_in(WorkStatus::Draft);
    let err = w.transition(WorkStatus::Pending, Role::Implementer).unwrap_err();
    assert_eq!(err.kind, FsmErrorKind::RoleNotAuthorized);
    assert_eq!(w.status, WorkStatus::Draft, "state must not mutate on Err");
}

#[test]
fn transition_no_edge_rejects() {
    // Pending -> Done is not in the transitions table.
    let mut w = work_in(WorkStatus::Pending);
    let err = w.transition(WorkStatus::Done, Role::Reactor).unwrap_err();
    assert_eq!(err.kind, FsmErrorKind::NoTransition);
    assert_eq!(w.status, WorkStatus::Pending);
}

#[test]
fn transition_ready_done_via_transition_rejects() {
    // Structural enforcement of the no-AC-skipping rule: Ready -> Done
    // is in overrides, not transitions. A stray transition() call from
    // a Stage 7 Reactor attempting to bypass AC must surface as a
    // typed error, not a silent success.
    let mut w = work_in(WorkStatus::Ready);
    let err = w.transition(WorkStatus::Done, Role::Reactor).unwrap_err();
    assert_eq!(err.kind, FsmErrorKind::NoTransition);
    assert_eq!(w.status, WorkStatus::Ready);
}

#[test]
fn transition_from_terminal_done_rejects() {
    let mut w = work_in(WorkStatus::Done);
    let err = w.transition(WorkStatus::Ready, Role::Reactor).unwrap_err();
    assert_eq!(err.kind, FsmErrorKind::NoTransition);
}

#[test]
fn transition_from_terminal_superseded_rejects() {
    let mut w = work_in(WorkStatus::Superseded);
    let err = w.transition(WorkStatus::Abandoned, Role::Director).unwrap_err();
    assert_eq!(err.kind, FsmErrorKind::NoTransition);
}

#[test]
fn transition_from_terminal_abandoned_rejects() {
    let mut w = work_in(WorkStatus::Abandoned);
    let err = w.transition(WorkStatus::Ready, Role::Reactor).unwrap_err();
    assert_eq!(err.kind, FsmErrorKind::NoTransition);
}

// ---------------------------------------------------------------------------
// FSM overrides
// ---------------------------------------------------------------------------

#[test]
fn override_inprogress_ready_by_reactor_mutates_state() {
    let mut w = work_in(WorkStatus::InProgress);
    std::thread::sleep(std::time::Duration::from_millis(2));
    let before = w.updated_at;
    let result = w.override_status(WorkStatus::Ready, Role::Reactor).unwrap();
    assert_eq!(result, Transition::Override);
    assert_eq!(w.status, WorkStatus::Ready);
    assert!(w.updated_at > before, "override must advance updated_at");
}

#[test]
fn override_ready_done_by_reactor_succeeds() {
    // Companion to transition_ready_done_via_transition_rejects: the
    // same edge, via override_status, must succeed with Transition::Override.
    let mut w = work_in(WorkStatus::Ready);
    let result = w.override_status(WorkStatus::Done, Role::Reactor).unwrap();
    assert_eq!(result, Transition::Override);
    assert_eq!(w.status, WorkStatus::Done);
}

#[test]
fn override_ready_done_wrong_role_rejects() {
    let mut w = work_in(WorkStatus::Ready);
    let err = w.override_status(WorkStatus::Done, Role::Director).unwrap_err();
    assert!(
        err.kind == FsmErrorKind::NoTransition || err.kind == FsmErrorKind::RoleNotAuthorized,
        "unexpected kind: {:?}",
        err.kind
    );
    assert_eq!(w.status, WorkStatus::Ready);
}

#[test]
fn override_returns_changed_for_normal_edge() {
    // validate_override falls through to validate_transition first;
    // a valid normal edge (Pending -> Ready by Reactor) should
    // return Changed, not Override.
    let mut w = work_in(WorkStatus::Pending);
    let result = w.override_status(WorkStatus::Ready, Role::Reactor).unwrap();
    assert_eq!(result, Transition::Changed);
    assert_eq!(w.status, WorkStatus::Ready);
}

#[test]
fn override_inreview_ready_by_reactor() {
    let mut w = work_in(WorkStatus::InReview);
    let result = w.override_status(WorkStatus::Ready, Role::Reactor).unwrap();
    assert_eq!(result, Transition::Override);
    assert_eq!(w.status, WorkStatus::Ready);
}

#[test]
fn override_inprogress_blocked_by_director_is_operator_abort_edge() {
    // Phase 18: operator abort. `InProgress -> Blocked` is an authored
    // transition for (Reactor, Implementer) but NOT Director, so the
    // operator's Director-role abort must land on the new override edge.
    let mut w = work_in(WorkStatus::InProgress);
    let result = w.override_status(WorkStatus::Blocked, Role::Director).unwrap();
    assert_eq!(result, Transition::Override);
    assert_eq!(w.status, WorkStatus::Blocked);
}

#[test]
fn override_inprogress_blocked_wrong_role_rejects() {
    // Break-to-prove the guard: the abort edge is Director-only. A
    // Reviewer trying the same override has neither an authored
    // transition nor an override entry, so it must be rejected.
    let mut w = work_in(WorkStatus::InProgress);
    let err = w.override_status(WorkStatus::Blocked, Role::Reviewer).unwrap_err();
    assert!(
        err.kind == FsmErrorKind::NoTransition || err.kind == FsmErrorKind::RoleNotAuthorized,
        "unexpected kind: {:?}",
        err.kind
    );
    assert_eq!(w.status, WorkStatus::InProgress);
}

// ---------------------------------------------------------------------------
// Unchanged + is_terminal
// ---------------------------------------------------------------------------

#[test]
fn transition_same_state_is_unchanged() {
    let mut w = Work::new(PlanId::new(), "t".to_string());
    let before = w.updated_at;
    let result = w.transition(WorkStatus::Pending, Role::Reactor).unwrap();
    assert_eq!(result, Transition::Unchanged);
    assert_eq!(w.status, WorkStatus::Pending);
    assert_eq!(w.updated_at, before, "Unchanged must not advance updated_at");
}

#[test]
fn work_status_terminal_states() {
    assert!(WorkStatus::Done.is_terminal());
    assert!(WorkStatus::Superseded.is_terminal());
    assert!(WorkStatus::Abandoned.is_terminal());
    assert!(!WorkStatus::Draft.is_terminal());
    assert!(!WorkStatus::Pending.is_terminal());
    assert!(!WorkStatus::Ready.is_terminal());
    assert!(!WorkStatus::InProgress.is_terminal());
    assert!(!WorkStatus::Blocked.is_terminal());
    assert!(!WorkStatus::InReview.is_terminal());
    assert!(!WorkStatus::Integrated.is_terminal());
}

// ---------------------------------------------------------------------------
// all_deps_done + any_dep_irrecoverable
// ---------------------------------------------------------------------------

fn make_work_with_status(status: WorkStatus) -> Work {
    let mut w = Work::new(PlanId::new(), "dep".to_string());
    if status != WorkStatus::Pending {
        w.status = status;
    }
    w
}

#[test]
fn all_deps_done_no_deps_returns_true() {
    let w = Work::new(PlanId::new(), "w".to_string());
    assert!(w.all_deps_done(&[]));
    assert!(w.all_deps_done(&[make_work_with_status(WorkStatus::Pending)]));
}

#[test]
fn all_deps_done_all_done_returns_true() {
    let dep = make_work_with_status(WorkStatus::Done);
    let mut w = Work::new(PlanId::new(), "w".to_string());
    w.dependencies = vec![dep.id.clone()];
    assert!(w.all_deps_done(&[dep]));
}

#[test]
fn all_deps_done_one_pending_returns_false() {
    let dep = make_work_with_status(WorkStatus::Pending);
    let mut w = Work::new(PlanId::new(), "w".to_string());
    w.dependencies = vec![dep.id.clone()];
    assert!(!w.all_deps_done(&[dep]));
}

#[test]
fn all_deps_done_unknown_id_returns_false() {
    let unknown_id = Work::new(PlanId::new(), "ghost".to_string()).id;
    let mut w = Work::new(PlanId::new(), "w".to_string());
    w.dependencies = vec![unknown_id];
    assert!(!w.all_deps_done(&[]));
}

#[test]
fn all_deps_done_mixed_returns_false() {
    let done = make_work_with_status(WorkStatus::Done);
    let pending = make_work_with_status(WorkStatus::Pending);
    let mut w = Work::new(PlanId::new(), "w".to_string());
    w.dependencies = vec![done.id.clone(), pending.id.clone()];
    assert!(!w.all_deps_done(&[done, pending]));
}

#[test]
fn any_dep_irrecoverable_no_deps_returns_none() {
    let w = Work::new(PlanId::new(), "w".to_string());
    assert!(w.any_dep_irrecoverable(&[]).is_none());
}

#[test]
fn any_dep_irrecoverable_abandoned_returns_some() {
    let dep = make_work_with_status(WorkStatus::Abandoned);
    let dep_id = dep.id.clone();
    let mut w = Work::new(PlanId::new(), "w".to_string());
    w.dependencies = vec![dep_id.clone()];
    assert_eq!(w.any_dep_irrecoverable(&[dep]), Some(&dep_id));
}

#[test]
fn any_dep_irrecoverable_superseded_returns_some() {
    let dep = make_work_with_status(WorkStatus::Superseded);
    let dep_id = dep.id.clone();
    let mut w = Work::new(PlanId::new(), "w".to_string());
    w.dependencies = vec![dep_id.clone()];
    assert_eq!(w.any_dep_irrecoverable(&[dep]), Some(&dep_id));
}

#[test]
fn any_dep_irrecoverable_blocked_returns_none() {
    let dep = make_work_with_status(WorkStatus::Blocked);
    let mut w = Work::new(PlanId::new(), "w".to_string());
    w.dependencies = vec![dep.id.clone()];
    // Blocked is recoverable - must NOT be treated as irrecoverable
    assert!(w.any_dep_irrecoverable(&[dep]).is_none());
}

#[test]
fn any_dep_irrecoverable_done_returns_none() {
    let dep = make_work_with_status(WorkStatus::Done);
    let mut w = Work::new(PlanId::new(), "w".to_string());
    w.dependencies = vec![dep.id.clone()];
    assert!(w.any_dep_irrecoverable(&[dep]).is_none());
}
