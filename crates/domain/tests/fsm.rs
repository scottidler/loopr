use domain::{FsmError, FsmErrorKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tiny {
    A,
    B,
    C,
}

const VALID_NORMAL_FROM_A: &[(Tiny, &[&str])] = &[(Tiny::B, &["coordinator"]), (Tiny::C, &[])];

const VALID_OVERRIDE_FROM_A: &[(Tiny, &[&str])] = &[(Tiny::C, &["coordinator"])];

#[test]
fn display_no_transition_renders_header_and_hints() {
    let err: FsmError<Tiny> = FsmError {
        from: Tiny::A,
        to: Tiny::C,
        role: "implementer".to_string(),
        kind: FsmErrorKind::NoTransition,
        valid_normal: VALID_NORMAL_FROM_A,
        valid_override: VALID_OVERRIDE_FROM_A,
        context: None,
    };
    let rendered = err.to_string();
    let expected = "invalid transition: A -> C (role: implementer): no transition exists\n\
                    \x20\x20valid from A (normal): B (coordinator), C (any)\n\
                    \x20\x20valid from A (override): C (coordinator)";
    assert_eq!(rendered, expected);
}

#[test]
fn display_role_not_authorized_uses_correct_reason() {
    let err: FsmError<Tiny> = FsmError {
        from: Tiny::A,
        to: Tiny::B,
        role: "director".to_string(),
        kind: FsmErrorKind::RoleNotAuthorized,
        valid_normal: VALID_NORMAL_FROM_A,
        valid_override: VALID_OVERRIDE_FROM_A,
        context: None,
    };
    assert!(err.to_string().contains("role not authorized"));
}

#[test]
fn display_with_context_chains_normal_path_reason() {
    let normal: FsmError<Tiny> = FsmError {
        from: Tiny::A,
        to: Tiny::C,
        role: "implementer".to_string(),
        kind: FsmErrorKind::NoTransition,
        valid_normal: VALID_NORMAL_FROM_A,
        valid_override: VALID_OVERRIDE_FROM_A,
        context: None,
    };
    let override_err: FsmError<Tiny> = FsmError {
        from: Tiny::A,
        to: Tiny::C,
        role: "implementer".to_string(),
        kind: FsmErrorKind::RoleNotAuthorized,
        valid_normal: VALID_NORMAL_FROM_A,
        valid_override: VALID_OVERRIDE_FROM_A,
        context: Some(Box::new(normal)),
    };
    let rendered = override_err.to_string();
    assert!(rendered.contains("role not authorized"));
    assert!(rendered.contains("(normal path: no transition exists)"));
}

#[test]
fn display_empty_targets_renders_none() {
    let err: FsmError<Tiny> = FsmError {
        from: Tiny::C,
        to: Tiny::A,
        role: "coordinator".to_string(),
        kind: FsmErrorKind::NoTransition,
        valid_normal: &[],
        valid_override: &[],
        context: None,
    };
    let rendered = err.to_string();
    assert!(rendered.contains("valid from C (normal): (none)"));
    assert!(rendered.contains("valid from C (override): (none)"));
}

#[test]
fn role_display_uses_kebab_case() {
    use domain::Role;
    assert_eq!(Role::Coordinator.to_string(), "coordinator");
    assert_eq!(Role::Integrator.to_string(), "integrator");
    assert_eq!(Role::Implementer.to_string(), "implementer");
    assert_eq!(Role::Director.to_string(), "director");
    assert_eq!(Role::Reviewer.to_string(), "reviewer");
    assert_eq!(Role::Researcher.to_string(), "researcher");
    assert_eq!(Role::Decomposer.to_string(), "decomposer");
}
