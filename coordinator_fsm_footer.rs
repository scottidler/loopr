fn build_fsm_footer(
    stores: &Stores,
    coord_state: &CoordinatorState,
    goal: &str,
    config: &CoordinatorConfig,
    agent_log: &AgentLogger,
) -> String {
    match coord_state.fsm_state {
        CoordinatorFsmState::Interviewing => {
            // In Interviewing state, the Coordinator generates interview questions
            // or proposes a Plan. This is handled by the interview IPC handlers;
            // the FSM footer just signals the state.
            "## Interviewing\n\n\
             You are in the Interviewing state. Generate interview questions to clarify the user's goal, \
             or propose a Plan if you have enough context.\n\n\
             Use InterviewQuestion to ask the user questions, or ProposePlan to propose a Plan draft.\n\n\
             Respond with a JSON array of actions."
                .to_string()
        }
        CoordinatorFsmState::Planning => {
            // Use existing generation footer logic for Plan→Spec→Phase hierarchy
            if let Some(gen_footer) = build_generation_footer(
                stores,
                goal,
                config.max_validation_attempts,
                None,
                agent_log,
                Some(coord_state),
                Some(stores.config.strategy.max_decomposition_attempts),
            ) {
                gen_footer
            } else {
                // All hierarchy levels exist — ready to transition to ActivatePhase
                "All planning artifacts (Plan, Spec, Phases) are created and active. \
                 Respond with: [{\"action\": \"done\", \"summary\": \"Planning complete, ready to activate first phase\"}]"
                    .to_string()
            }
        }
        CoordinatorFsmState::ActivatePhase => {
            // Find the next phase to activate and generate Works for it
            let phase_info = find_next_phase_to_activate(stores, coord_state);
            match phase_info {
                Some((phase_id, phase_title)) => {
                    let phase = {
                        let Ok(phases) = stores.read_phases() else {
                            return "phases lock poisoned".to_string();
                        };
                        phases.get(&phase_id).cloned()
                    };
                    if let Some(phase) = phase {
                        let existing = generation::find_works_for_phase(stores, &phase.id);
                        if existing.is_empty() {
                            let prompt = build_work_prompt(&phase, &existing, &[], &[], None);
                            format!(
                                "## Activating Phase: {} (id: {})\n\n\
                                 Generate Works for this phase. Each Work should have clear \
                                 acceptance criteria and declare dependencies on other Works in this phase \
                                 using their IDs.\n\n{}",
                                phase_title, phase_id, prompt.user_message
                            )
                        } else {
                            format!(
                                "Phase '{}' already has {} Works. \
                                 Respond with: [{{\"action\": \"done\", \"summary\": \"Phase {} Works ready\"}}]",
                                phase_title,
                                existing.len(),
                                phase_title
                            )
                        }
                    } else {
                        "No phase found to activate. Respond with: [{\"action\": \"done\", \"summary\": \"No phases available\"}]".to_string()
                    }
                }
                None => "All phases have been completed. \
                     Respond with: [{\"action\": \"done\", \"summary\": \"All phases complete\"}]"
                    .to_string(),
            }
        }
        CoordinatorFsmState::Executing => {
            // Build executing context — monitor works, assign agents, triage bundles
            let phase_status = build_phase_status(stores, coord_state);
            format!(
                "## Executing Phase\n\n{}\n\n\
                 Monitor Work statuses. Assign implementers to Ready Works whose dependencies are all Done. \
                 Triage proposed Bundles. Accept reviewed Bundles. \
                 If a Work is Blocked or has failed, consider retrying.\n\n\
                 Respond with a JSON array of actions.",
                phase_status
            )
        }
        CoordinatorFsmState::PhaseGate => {
            let phase_status = build_phase_status(stores, coord_state);
            format!(
                "## Phase Gate Check\n\n{}\n\n\
                 All Works in this phase should be in a terminal state (Done, Abandoned, or NeedHelp). \
                 If all are Done, the phase is complete. \
                 Respond with: [{{\"action\": \"done\", \"summary\": \"Phase gate passed\"}}]",
                phase_status
            )
        }
        CoordinatorFsmState::GoalComplete => {
            format!(
                "Goal is complete. {} phases were completed. \
                 Respond with: [{{\"action\": \"done\", \"summary\": \"Goal complete\"}}]",
                coord_state.phases_completed.len()
            )
        }
    }
}

/// Find the next phase that hasn't been completed yet (by order).
fn find_next_phase_to_activate(stores: &Stores, coord_state: &CoordinatorState) -> Option<(String, String)> {
    let plan = generation::find_active_plan(stores)?;
    let specs = generation::find_active_specs_for_plan(stores, &plan.id);
