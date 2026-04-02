use crate::agents::AgentStatus;

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
