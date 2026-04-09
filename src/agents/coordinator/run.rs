use tokio::sync::broadcast;

use super::*;

/// Events that indicate meaningful state change the coordinator must respond to.
/// Work and Bundle status transitions, new Work records, published ticks, and
/// decomposition completion/failure all require coordinator attention.
/// Agent status events are excluded - too noisy.
fn is_coordinator_wakeup(ev: &DaemonEvent) -> bool {
    matches!(
        ev.event.as_str(),
        "transition.completed"
            | "record.created"
            | "tick.published"
            | "decomposition.completed"
            | "decomposition.failed"
    )
}

impl<L: LlmClient> CoordinatorAgent<L> {
    pub fn new(ctx: AgentContext, llm: L, config: CoordinatorConfig) -> Self {
        Self {
            ctx,
            llm,
            config,
            iteration: 0,
            previous_summary: None,
        }
    }

    /// Run the main FSM-driven coordinator loop.
    pub(super) async fn run_fsm_loop(&mut self, mut coord_state: CoordinatorState) -> Result<()> {
        let mut guard = Lifeguard::new();
        let mut event_rx = self.ctx.event_tx.subscribe();
        loop {
            // Check cancellation
            if self.ctx.is_cancelled() {
                self.ctx.info("cancelled, exiting loop");
                return Ok(());
            }

            // Check if goal is complete
            if coord_state.fsm_state.is_terminal() {
                self.ctx.info("goal complete, exiting loop");
                return Ok(());
            }

            self.iteration = self.iteration.saturating_add(1);
            self.ctx.session.iteration = self.iteration;
            self.ctx.persist_iteration();
            self.ctx.info(&format!(
                "iteration {} (FSM: {})",
                self.iteration, coord_state.fsm_state
            ));

            let outcome = self.run_iteration(&mut coord_state, &mut guard).await;

            let interval = match &outcome {
                Ok(IterationOutcome::Done(summary)) => {
                    self.ctx.emit_iteration_completed(self.iteration, summary);
                    self.ctx
                        .info(&format!("idle (FSM: {}): {}", coord_state.fsm_state, summary));
                    self.previous_summary = Some(summary.clone());
                    // Use active interval for FSM states that need quick transitions
                    match coord_state.fsm_state {
                        CoordinatorFsmState::Planning => self.config.active_interval_secs,
                        _ => self.config.idle_interval_secs,
                    }
                }
                Ok(IterationOutcome::Continue(summary)) => {
                    self.ctx.emit_iteration_completed(self.iteration, summary);
                    self.ctx
                        .info(&format!("continue (FSM: {}): {}", coord_state.fsm_state, summary));
                    self.previous_summary = Some(summary.clone());
                    self.config.active_interval_secs
                }
                Ok(IterationOutcome::NeedHelp(reason)) => {
                    self.ctx.emit_iteration_completed(self.iteration, reason);
                    self.ctx.warn(&format!("needs help: {}", reason));
                    return Err(eyre!("coordinator needs help: {}", reason));
                }
                Err(e) => {
                    // Lifeguard: track parse failures
                    if let Verdict::Escalate(reason) = guard.record_parse_failure() {
                        self.ctx.trace(&format!("lifeguard: {}", reason));
                        return Err(eyre!("lifeguard: {}", reason));
                    }
                    self.ctx
                        .warn(&format!("iteration {} failed (will retry): {}", self.iteration, e));
                    self.previous_summary = Some(format!(
                        "ERROR: Your previous response could not be parsed. \
                         You MUST respond with ONLY a JSON array of action objects. \
                         No prose, no markdown, no explanation.\n\
                         Parse error: {}",
                        e
                    ));
                    self.config.active_interval_secs
                }
            };

            // In idle-waiting FSM states, wake early on relevant state changes
            // rather than burning the full polling interval on a no-op LLM call.
            let is_idle_waiting = matches!(&outcome, Ok(IterationOutcome::Done(_)))
                && matches!(
                    coord_state.fsm_state,
                    CoordinatorFsmState::Decomposing | CoordinatorFsmState::Planning
                );

            if is_idle_waiting {
                // Use a timeout ceiling so Lagged events cannot cancel the sleep.
                // Lagged drains the channel and continues the inner loop while the
                // timeout counts down. An early wakeup on a relevant event breaks
                // the inner loop, which resolves the future and the timeout returns Ok(()).
                let _ = tokio::time::timeout(Duration::from_secs(interval), async {
                    loop {
                        match event_rx.recv().await {
                            Ok(ev) if is_coordinator_wakeup(&ev) => {
                                self.ctx.info(&format!("early wake on: {}", ev.event));
                                break;
                            }
                            Err(broadcast::error::RecvError::Closed) => {
                                // Daemon shutting down; let cancellation check on next iteration handle it.
                                break;
                            }
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                // Channel lagged: drain and continue — timeout ceiling is still running.
                                tracing::debug!("event_rx lagged {} events during idle wait", n);
                            }
                            _ => {} // Irrelevant event; drain and continue.
                        }
                    }
                })
                .await
                .ok();
            } else {
                tokio::time::sleep(Duration::from_secs(interval)).await;
            }
        }
    }

    /// Run a single coordinator iteration: load context -> call LLM -> parse -> execute actions.
    /// Now dispatches based on FSM state.
    pub(super) async fn run_iteration(
        &self,
        coord_state: &mut CoordinatorState,
        guard: &mut Lifeguard,
    ) -> Result<IterationOutcome> {
        let stores = &self.ctx.stores;
        let config = &self.config;
        let bridge = &self.ctx.bridge;
        let iteration = self.iteration;
        let prefix = self.ctx.log_prefix();
        // Check for FSM state transitions before the iteration
        if let Some(new_state) = check_fsm_transition(stores, coord_state, config) {
            self.ctx
                .info(&format!("FSM transition: {} -> {}", coord_state.fsm_state, new_state));
            if let Some(outcome) = apply_fsm_transition(new_state, coord_state, stores, &prefix) {
                return Ok(outcome);
            }
        }

        // Deterministic: transition Integrated Works to Done before consulting the LLM.
        sweep_integrated_to_done(stores, coord_state, bridge, &prefix);

        // Re-check FSM after sweep — if all Works are now Done, advance immediately.
        if let Some(new_state) = check_fsm_transition(stores, coord_state, config) {
            self.ctx.info(&format!(
                "FSM transition (post-sweep): {} -> {}",
                coord_state.fsm_state, new_state
            ));
            if let Some(outcome) = apply_fsm_transition(new_state, coord_state, stores, &prefix) {
                return Ok(outcome);
            }
        }

        // Reconciliation: promote Pending records and detect completions.
        // Runs during Executing state, before the LLM call, so the LLM sees post-reconciliation state.
        if coord_state.fsm_state == CoordinatorFsmState::Executing {
            let outcome = super::reconcile::reconcile(stores);
            if outcome.promoted > 0 || outcome.completed > 0 {
                self.ctx.info(&format!(
                    "reconcile: promoted={} completed={} goal_complete={}",
                    outcome.promoted, outcome.completed, outcome.goal_complete,
                ));
            }
            if outcome.goal_complete {
                // All Specs (Full) or all Works (Brief) are terminal - trigger GoalComplete.
                if let Some(fsm_outcome) =
                    apply_fsm_transition(CoordinatorFsmState::GoalComplete, coord_state, stores, &prefix)
                {
                    return Ok(fsm_outcome);
                }
            }
        }

        // Decomposing: background task is still running; do not call the LLM.
        // The event-driven wake in run_fsm_loop will re-enter this iteration when
        // decomposition.completed or decomposition.failed is emitted.
        if coord_state.fsm_state == CoordinatorFsmState::Decomposing {
            if let Some(err) = &coord_state.decomposition_error {
                return Ok(IterationOutcome::NeedHelp(format!(
                    "Background decomposition failed: {}",
                    err
                )));
            }
            return Ok(IterationOutcome::Done(
                "waiting for decomposition to complete".to_string(),
            ));
        }

        // Check if any phases have completed (all Works Done) — legacy helper
        let completed_phases = check_phase_completion(stores);
        for cp in &completed_phases {
            self.ctx.info(&format!("detected: {}", cp));
        }

        let state_summary = build_state_summary_with_sla(
            stores,
            &prefix,
            Some(coord_state),
            Some(&stores.config.strategy.work_sla),
        );

        let goal = {
            let goals = stores.read_coordinator_goals()?;
            goals
                .values()
                .find(|g| g.active)
                .map(|g| g.goal.clone())
                .unwrap_or_else(|| "No goal set.".to_string())
        };

        // Build FSM-aware footer
        let footer = build_fsm_footer(stores, coord_state, &goal, config, &prefix);

        // Add FSM state context to state summary
        let fsm_context = format!("## Coordinator FSM State: {}\n\n", coord_state.fsm_state,);

        let builder = ContextBuilder::new(stores, Role::Coordinator)
            .with_guidance(&stores.guidance)
            .with_state_summary(format!("{}{}", fsm_context, state_summary))
            .with_previous_summary(self.previous_summary.clone())
            .with_iteration(iteration)
            .with_footer(footer);

        let assembled = builder.build(&crate::prompts::store().coordinator)?;

        self.ctx.info(&format!(
            "iteration {} (FSM: {}) context: ~{} tokens",
            iteration, coord_state.fsm_state, assembled.token_estimate
        ));

        tracing::debug!("[agent_status] {}: -> WaitingForLlm", self.ctx.session.id);
        let _ = self.ctx.event_tx.send(DaemonEvent::agent_status_changed(
            &self.ctx.session.id,
            AgentStatus::WaitingForLlm,
        ));

        // Self-correction loop: re-prompt on parse failure up to max_requeries times
        let mut messages = vec![ChatMessage::user(&assembled.user_message)];
        let mut requeries = 0u32;
        let max_requeries = self.config.role.max_requeries;

        let mut actions = loop {
            let response = self.llm.call_with_history(&assembled.system_prompt, &messages).await?;
            self.ctx.info(&format!(
                "raw LLM response ({} chars): {}",
                response.len(),
                &response[..response.len().min(800)]
            ));

            match implementer::parse_actions(&response, &prefix) {
                Ok(actions) => break actions,
                Err(parse_err) => {
                    requeries += 1;
                    if requeries > max_requeries {
                        return Err(parse_err);
                    }
                    self.ctx.info(&format!(
                        "parse failed (requery {}/{}): {}",
                        requeries, max_requeries, parse_err
                    ));
                    messages.push(ChatMessage::assistant(&response));
                    messages.push(ChatMessage::user(&format!(
                        "Your response could not be parsed as a valid JSON action array.\n\
                         Error: {}\n\n\
                         Please respond with ONLY a valid JSON array of actions. \
                         Do not include any text before or after the JSON.",
                        parse_err
                    )));
                }
            }
        };
        guard.reset_parse_failures();

        tracing::debug!("[agent_status] {}: -> Running (LLM complete)", self.ctx.session.id);
        let _ = self.ctx.event_tx.send(DaemonEvent::agent_status_changed(
            &self.ctx.session.id,
            AgentStatus::Running,
        ));

        if actions.is_empty() {
            return Ok(IterationOutcome::Done("No actions needed".to_string()));
        }

        // Phase 2: Action coherence validation - warn when coordinator promises
        // to create replacement works but doesn't emit the create_work action.
        let coherence_warnings = super::validate_action_coherence(&actions, &prefix);
        for warning in &coherence_warnings {
            self.ctx.warn(warning);
        }

        // Gap #28: One-level-per-iteration guard — filter mixed-level actions
        let levels: std::collections::HashSet<_> = actions.iter().filter_map(infer_action_level).collect();
        if levels.len() > 1 {
            let first_level = infer_action_level(&actions[0]);
            self.ctx.warn(&format!(
                "attempted multi-level actions: {:?}. Executing only first level.",
                levels
            ));
            actions.retain(|a| infer_action_level(a) == first_level || infer_action_level(a).is_none());
        }

        // Use repo root as the "worktree" path for Coordinator (thinking plane — no actual worktree)
        let repo_root = &stores.config.project.repo_path;

        // Fix #2: Track batch-created Work IDs for batch:N dependency resolution
        let mut batch_created_ids: Vec<String> = Vec::new();
        let mut last_summary = String::new();
        for action in &actions {
            // Lifeguard: check for repeated identical actions
            let action_hash = lifeguard::hash_action(action);
            if let Verdict::Escalate(reason) = guard.check_action(action_hash) {
                self.ctx.trace(&format!("lifeguard: {}", reason));
                return Ok(IterationOutcome::NeedHelp(format!("lifeguard: {}", reason)));
            }

            // Fix #2: Resolve batch:N dependencies before executing CreateWork
            let resolved_action = resolve_batch_dependencies(action, &batch_created_ids, &prefix);
            let action_ref = resolved_action.as_ref().unwrap_or(action);

            // Error classification: check if the most recent failed session for this
            // work has a structural error_kind. ContextOverflow and ParseExhausted are
            // not worth retrying - abandon immediately with an explanatory learning.
            if let AgentAction::AssignAgent { agent_type, target_id } = action_ref
                && agent_type == "implementer"
                && let Some(error_kind) = last_error_kind_for_work(&self.ctx.stores, target_id)
                && matches!(
                    error_kind,
                    AgentErrorKind::ContextOverflow | AgentErrorKind::ParseExhausted
                )
            {
                let reason = match error_kind {
                    AgentErrorKind::ContextOverflow => {
                        format!(
                            "Work '{}' failed due to context overflow - \
                             reduce scope or split the work",
                            target_id
                        )
                    }
                    AgentErrorKind::ParseExhausted => {
                        format!(
                            "Work '{}' failed after exhausting parse retries - \
                             the LLM cannot produce valid output for this prompt",
                            target_id
                        )
                    }
                    _ => unreachable!(),
                };
                self.ctx.warn(&reason);
                let abandon_resp = bridge.request(
                    "work.transition",
                    serde_json::json!({
                        "id": target_id,
                        "target_status": "Abandoned",
                        "role": "coordinator"
                    }),
                );
                if abandon_resp.is_error() {
                    self.ctx.error(&format!(
                        "failed to abandon work {}: {:?}",
                        target_id, abandon_resp.error
                    ));
                }
                let learn_resp = bridge.request(
                    "learning.create",
                    serde_json::json!({
                        "content": reason,
                        "scope": "phase",
                        "source_id": target_id,
                    }),
                );
                if learn_resp.is_error() {
                    self.ctx
                        .warn(&format!("failed to create learning for work {}", target_id));
                }
                let summary = format!("Work {} abandoned (structural failure: {:?})", target_id, error_kind);
                let _ = self
                    .ctx
                    .event_tx
                    .send(DaemonEvent::agent_action_completed(&self.ctx.session.id, &summary));
                last_summary = summary;
                continue;
            }

            // Fix #4: Enforce max_work_retries for implementer assignments
            if let AgentAction::AssignAgent { agent_type, target_id } = action_ref
                && agent_type == "implementer"
            {
                let attempts = coord_state.increment_attempts(target_id);
                let max_retries = config.max_work_retries;
                if attempts > max_retries {
                    self.ctx.warn(&format!(
                        "Work {} exceeded max retries ({}/{}), transitioning to Abandoned",
                        target_id, attempts, max_retries
                    ));
                    // Transition to Abandoned via bridge
                    let abandon_resp = bridge.request(
                        "work.transition",
                        serde_json::json!({
                            "id": target_id,
                            "target_status": "Abandoned",
                            "role": "coordinator"
                        }),
                    );
                    if abandon_resp.is_error() {
                        self.ctx.error(&format!(
                            "failed to abandon work {}: {:?}",
                            target_id, abandon_resp.error
                        ));
                    }
                    // Create a Learning about the retry exhaustion
                    // M2: Use lowercase scope to match LearningScope serde format
                    let learn_resp = bridge.request(
                        "learning.create",
                        serde_json::json!({
                            "content": format!("Work '{}' abandoned after {} failed attempts", target_id, attempts),
                            "scope": "phase",
                            "source_id": target_id,
                        }),
                    );
                    if learn_resp.is_error() {
                        self.ctx
                            .warn(&format!("failed to create learning for work {}", target_id));
                    }
                    let summary = format!("Work {} abandoned (max retries exceeded)", target_id);
                    let _ = self
                        .ctx
                        .event_tx
                        .send(DaemonEvent::agent_action_completed(&self.ctx.session.id, &summary));
                    last_summary = summary;
                    continue;
                }
            }

            // Pre-validation: catch AssignAgent targeting terminal work before executing.
            // This avoids wasting a bridge round-trip and gives a hard, directive error.
            if let AgentAction::AssignAgent { target_id, .. } = action_ref
                && let Ok(works) = stores.read_works()
                && let Some(wi) = works.get(target_id)
                && matches!(wi.status(), WorkStatus::Done | WorkStatus::Abandoned)
            {
                let err_msg = format!(
                    "INVALID: Work '{}' ({}) is already '{}'. \
                     You MUST NOT assign agents to completed or abandoned work. \
                     Review the Actionable Works list and assign agents to Ready tasks instead.",
                    target_id,
                    wi.title,
                    wi.status()
                );
                self.ctx.warn(&err_msg);
                let (verdict, warning) = guard.record_error(&err_msg);
                if let Some(w) = warning {
                    self.ctx.warn(&w);
                }
                if let Verdict::Escalate(reason) = verdict {
                    self.ctx.trace(&format!("lifeguard: {}", reason));
                    return Ok(IterationOutcome::NeedHelp(format!(
                        "lifeguard: repeated assignment to terminal work: {}",
                        reason
                    )));
                }
                let _ = self
                    .ctx
                    .event_tx
                    .send(DaemonEvent::agent_action_completed(&self.ctx.session.id, &err_msg));
                last_summary = err_msg;
                continue;
            }

            // Pre-execution guard: researcher spawn limit per scope
            if let AgentAction::SpawnResearcher { scope_id, .. } = action_ref {
                let count = coord_state.researcher_spawn_count(scope_id);
                if count >= config.max_researcher_spawns {
                    self.ctx.warn(&format!(
                        "researcher spawn limit reached ({}/{}) for scope '{}'",
                        count, config.max_researcher_spawns, scope_id
                    ));
                    last_summary = format!(
                        "Researcher spawn limit reached for scope '{}'. \
                         You MUST escalate via need_help.",
                        scope_id
                    );
                    continue;
                }
            }

            let result = match execute_action(action_ref, &self.ctx, repo_root, None).await {
                Ok(r) => r,
                Err(e) => {
                    let err_msg = e.to_string();
                    self.ctx.warn(&format!("action failed (non-fatal): {err_msg}"));
                    // Lifeguard: check for repeated errors (config errors don't escalate)
                    let (verdict, warning) = guard.record_error(&err_msg);
                    if let Some(w) = warning {
                        self.ctx.warn(&w);
                    }
                    if let Verdict::Escalate(reason) = verdict {
                        self.ctx.trace(&format!("lifeguard: {}", reason));
                        return Ok(IterationOutcome::NeedHelp(format!("lifeguard: {}", reason)));
                    }
                    ActionResult::ActionError(err_msg)
                }
            };

            // Lifeguard: also catch ActionError results (validation failures from executor
            // that return Ok(ActionError) rather than Err). Without this, the Lifeguard
            // only monitors hard errors and misses tool validation loops.
            if let ActionResult::ActionError(ref err_msg) = result {
                let (verdict, warning) = guard.record_error(err_msg);
                if let Some(w) = warning {
                    self.ctx.warn(&w);
                }
                if let Verdict::Escalate(reason) = verdict {
                    self.ctx.trace(&format!("lifeguard: {}", reason));
                    return Ok(IterationOutcome::NeedHelp(format!(
                        "lifeguard: tool validation loop (not a system failure): {}",
                        reason
                    )));
                }
            }

            // B4: Only count successful AgentSpawned as actual attempts.
            // DependencyNotMet and ActionError/other non-spawn results should not burn retry slots.
            if let AgentAction::AssignAgent {
                target_id,
                agent_type: at,
            } = action_ref
                && at == "implementer"
            {
                match &result {
                    ActionResult::AgentSpawned { .. } => {
                        // Successful spawn — attempt counts (already incremented above)
                        // Track SLA: record first assignment time
                        coord_state.record_first_assignment(target_id);
                    }
                    ActionResult::DependencyNotMet { work_id, .. } => {
                        if let Some(count) = coord_state.work_attempts.get_mut(work_id) {
                            *count = count.saturating_sub(1);
                        }
                    }
                    _ => {
                        // Action failed before agent spawned — don't count
                        if let Some(count) = coord_state.work_attempts.get_mut(target_id) {
                            *count = count.saturating_sub(1);
                        }
                    }
                }
            }

            // Post-execution: track successful researcher spawns
            if let AgentAction::SpawnResearcher { scope_id, .. } = action_ref
                && matches!(result, ActionResult::AgentSpawned { .. })
            {
                coord_state.increment_researcher_spawns(scope_id);
            }

            // Track created Work IDs for batch dependency resolution
            if let ActionResult::RecordCreated { ref collection, ref id } = result
                && collection == "works"
            {
                batch_created_ids.push(id.clone());
            }

            let summary = format_action_summary(&result);
            let _ = self
                .ctx
                .event_tx
                .send(DaemonEvent::agent_action_completed(&self.ctx.session.id, &summary));

            match &result {
                ActionResult::ActionError(_) => {
                    last_summary = summary;
                    break;
                }
                ActionResult::Done(s) => return Ok(IterationOutcome::Done(s.clone())),
                ActionResult::NeedHelp(reason) => return Ok(IterationOutcome::NeedHelp(reason.clone())),
                ActionResult::CoverageEvaluated { verdict, .. } => {
                    if let AgentAction::EvaluateCoverage { parent_id, .. } = action_ref {
                        if verdict == "incomplete" {
                            let count = coord_state.increment_decomposition_attempts(parent_id);
                            self.ctx.info(&format!(
                                "coverage incomplete for {}, decomposition attempt {}",
                                parent_id, count
                            ));
                        } else {
                            coord_state.reset_decomposition_attempts(parent_id);
                            self.ctx.info(&format!("coverage complete for {}", parent_id));
                        }
                    }
                }
                ActionResult::Transitioned(_) => {
                    // After ReviseParent succeeds: increment bubble-up count and reset
                    // decomposition attempts so the revised parent gets fresh retries.
                    if let AgentAction::ReviseParent { id, .. } = action_ref {
                        let count = coord_state.increment_bubble_up();
                        coord_state.reset_decomposition_attempts(id);
                        self.ctx.info(&format!(
                            "bubble-up complete for {}, reset decomposition attempts (bubble_up_count={})",
                            id, count
                        ));
                    }
                }
                _ => {}
            }
            last_summary = summary;
        }

        // Fix #6: Inject same-file deps first, then prune independent ones.
        // inject_overlap_deps ensures works sharing a file are serialized (cycle-safe).
        // prune_independent_deps removes false chain deps between non-overlapping works.
        if !batch_created_ids.is_empty() {
            inject_overlap_deps(stores, &batch_created_ids, &prefix);
            prune_independent_deps(stores, &batch_created_ids, &prefix);
        }

        // Persist state after each iteration
        coord_state.updated_at = crate::id::now_millis();
        persist_coordinator_state(stores, coord_state);

        Ok(IterationOutcome::Continue(last_summary))
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use crate::agents::coordinator::tests::{insert_test_goal, test_coordinator, test_stores};
    use crate::agents::implementer::IterationOutcome;
    use crate::agents::lifeguard::Lifeguard;
    use crate::agents::{Agent, AgentStatus};
    use crate::config::{CoordinatorConfig, InterviewMode};
    use crate::domain::coordinator_state::CoordinatorState;
    use crate::test_util::TestDir;

    // --- is_cancelled tests (via AgentContext) ---

    #[tokio::test(flavor = "multi_thread")]
    async fn test_is_cancelled_false() {
        let dir = TestDir::new("loopr-coord-canc1");
        let stores = test_stores(&dir);
        let agent = test_coordinator(&dir, &stores, vec![], CoordinatorConfig::default());

        // Insert the agent's session as Running
        let mut session = agent.ctx.session.clone();
        let _ = session.transition_to(AgentStatus::Running);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        assert!(!agent.ctx.is_cancelled());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_is_cancelled_true() {
        let dir = TestDir::new("loopr-coord-canc2");
        let stores = test_stores(&dir);
        let agent = test_coordinator(&dir, &stores, vec![], CoordinatorConfig::default());

        // Insert the agent's session as Cancelled
        let mut session = agent.ctx.session.clone();
        let _ = session.transition_to(AgentStatus::Running);
        let _ = session.transition_to(AgentStatus::Cancelled);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        assert!(agent.ctx.is_cancelled());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_is_cancelled_missing() {
        let dir = TestDir::new("loopr-coord-canc3");
        let stores = test_stores(&dir);
        // Agent session not inserted into stores — should treat as cancelled
        let agent = test_coordinator(&dir, &stores, vec![], CoordinatorConfig::default());

        assert!(agent.ctx.is_cancelled());
    }

    // --- run_iteration tests ---

    #[tokio::test(flavor = "multi_thread")]
    async fn test_coordinator_iteration_done() {
        let dir = TestDir::new("loopr-coord-itdone");
        let stores = test_stores(&dir);

        let agent = test_coordinator(
            &dir,
            &stores,
            vec![r#"[{"action": "done", "summary": "Nothing to do"}]"#.to_string()],
            CoordinatorConfig::default(),
        );

        let outcome = agent
            .run_iteration(
                &mut CoordinatorState::new("test-goal".to_string(), InterviewMode::Interactive),
                &mut Lifeguard::new(),
            )
            .await
            .unwrap();

        assert!(matches!(outcome, IterationOutcome::Done(ref s) if s.contains("Nothing to do")));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_coordinator_iteration_need_help() {
        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-coord-ithelp");
        let stores = test_stores(&dir);

        let agent = test_coordinator(
            &dir,
            &stores,
            vec![r#"[{"action": "need_help", "reason": "Unclear requirements"}]"#.to_string()],
            CoordinatorConfig::default(),
        );

        let outcome = agent
            .run_iteration(
                &mut CoordinatorState::new("test-goal".to_string(), InterviewMode::Interactive),
                &mut Lifeguard::new(),
            )
            .await
            .unwrap();

        assert!(matches!(outcome, IterationOutcome::NeedHelp(ref s) if s.contains("Unclear")));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_coordinator_iteration_continue_with_stub_actions() {
        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-coord-itstub");
        let stores = test_stores(&dir);

        let agent = test_coordinator(
            &dir,
            &stores,
            vec![
                r#"[{"action": "create_learning", "content": "auth design", "scope": "global", "source_id": "test"}]"#
                    .to_string(),
            ],
            CoordinatorConfig::default(),
        );

        let outcome = agent
            .run_iteration(
                &mut CoordinatorState::new("test-goal".to_string(), InterviewMode::Interactive),
                &mut Lifeguard::new(),
            )
            .await
            .unwrap();

        // create_learning is a live action — executes and returns Continue
        assert!(matches!(outcome, IterationOutcome::Continue(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_coordinator_iteration_empty_actions_is_done() {
        let dir = TestDir::new("loopr-coord-itempty");
        let stores = test_stores(&dir);

        let agent = test_coordinator(&dir, &stores, vec!["[]".to_string()], CoordinatorConfig::default());

        let outcome = agent
            .run_iteration(
                &mut CoordinatorState::new("test-goal".to_string(), InterviewMode::Interactive),
                &mut Lifeguard::new(),
            )
            .await
            .unwrap();

        assert!(matches!(outcome, IterationOutcome::Done(_)));
    }

    // --- Agent::run tests ---

    #[tokio::test(flavor = "multi_thread")]
    async fn test_coordinator_exits_on_need_help() {
        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-coord-runhelp");
        let stores = test_stores(&dir);
        insert_test_goal(&stores);

        let mut agent = test_coordinator(
            &dir,
            &stores,
            vec![r#"[{"action": "need_help", "reason": "I'm stuck"}]"#.to_string()],
            CoordinatorConfig::default(),
        );

        // Insert the session as Running
        let mut session = agent.ctx.session.clone();
        let _ = session.transition_to(AgentStatus::Running);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        let result = agent.run().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("needs help"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_coordinator_exits_on_cancellation() {
        let dir = TestDir::new("loopr-coord-runcanc");
        let stores = test_stores(&dir);
        insert_test_goal(&stores);

        let mut agent = test_coordinator(&dir, &stores, vec![], CoordinatorConfig::default());

        // Insert the session as Cancelled
        let mut session = agent.ctx.session.clone();
        let _ = session.transition_to(AgentStatus::Running);
        let _ = session.transition_to(AgentStatus::Cancelled);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        let result = agent.run().await;
        assert!(result.is_ok()); // Cancelled = graceful exit
    }

    // --- test_coordinator_iteration_persists ---

    #[tokio::test(flavor = "multi_thread")]
    async fn test_coordinator_iteration_persists() {
        let dir = TestDir::new("loopr-coord-itpersist");
        let stores = test_stores(&dir);
        insert_test_goal(&stores);

        let config = CoordinatorConfig {
            active_interval_secs: 0,
            idle_interval_secs: 0,
            ..CoordinatorConfig::default()
        };

        // MockLlm: iterations 1,2 return Continue, iteration 3 returns NeedHelp to exit loop
        let mut agent = test_coordinator(
            &dir,
            &stores,
            vec![
                r#"[{"action": "create_learning", "content": "iter 1", "scope": "global", "source_id": "test"}]"#
                    .to_string(),
                r#"[{"action": "create_learning", "content": "iter 2", "scope": "global", "source_id": "test"}]"#
                    .to_string(),
                r#"[{"action": "need_help", "reason": "done testing"}]"#.to_string(),
            ],
            config,
        );

        // Insert the session as Running
        let mut session = agent.ctx.session.clone();
        let _ = session.transition_to(AgentStatus::Running);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        let _ = agent.run().await;

        // Session iteration should be 3 (need_help on iteration 3)
        assert_eq!(agent.ctx.session.iteration, 3);

        // The iteration should also be persisted in stores
        let stored_iteration = stores
            .agent_sessions
            .read()
            .unwrap()
            .get(&agent.ctx.session.id)
            .map(|s| s.iteration)
            .unwrap_or(0);
        assert_eq!(stored_iteration, 3, "iteration should be persisted in stores");
    }

    // --- multi-level action filter tests ---

    #[tokio::test(flavor = "multi_thread")]
    async fn test_coordinator_iteration_filters_multi_level_actions() {
        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-coord-multilevel");
        let stores = test_stores(&dir);

        // Two create_learning actions — neither has a hierarchy level, so no multi-level filtering
        let agent = test_coordinator(
            &dir,
            &stores,
            vec![
                r#"[
                {"action": "create_learning", "content": "first learning", "scope": "global", "source_id": "t1"},
                {"action": "create_learning", "content": "second learning", "scope": "global", "source_id": "t2"}
            ]"#
                .to_string(),
            ],
            CoordinatorConfig::default(),
        );

        let outcome = agent
            .run_iteration(
                &mut CoordinatorState::new("test-goal".to_string(), InterviewMode::Interactive),
                &mut Lifeguard::new(),
            )
            .await
            .unwrap();

        // Both actions have no level — no filtering applied, both execute, returns Continue
        assert!(matches!(outcome, IterationOutcome::Continue(_)));

        // Both learnings should have been created
        let learnings = stores.learnings.read().unwrap();
        assert_eq!(learnings.len(), 2, "both learning actions should have executed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_coordinator_iteration_empty_after_filter() {
        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-coord-emptyfilter");
        let stores = test_stores(&dir);

        let agent = test_coordinator(
            &dir,
            &stores,
            vec![r#"[{"action": "done", "summary": "Finished planning"}]"#.to_string()],
            CoordinatorConfig::default(),
        );

        let outcome = agent
            .run_iteration(
                &mut CoordinatorState::new("test-goal".to_string(), InterviewMode::Interactive),
                &mut Lifeguard::new(),
            )
            .await
            .unwrap();

        assert!(
            matches!(outcome, IterationOutcome::Done(ref s) if s.contains("Finished planning")),
            "done action should yield Done outcome"
        );
    }

    // --- Self-correction loop tests for Coordinator ---

    #[tokio::test(flavor = "multi_thread")]
    async fn test_coordinator_self_correction_parse_failure_then_success() {
        // First LLM response is malformed, second is valid JSON.
        // Self-correction loop should re-prompt and succeed within the same iteration.
        let dir = TestDir::new("loopr-coord-selfcorr1");
        let stores = test_stores(&dir);

        let agent = test_coordinator(
            &dir,
            &stores,
            vec![
                "Let me think about the plan first.".to_string(), // malformed
                r#"[{"action": "done", "summary": "Self-corrected coordinator"}]"#.to_string(), // valid
            ],
            CoordinatorConfig::default(),
        );

        let outcome = agent
            .run_iteration(
                &mut CoordinatorState::new("test-goal".to_string(), InterviewMode::Interactive),
                &mut Lifeguard::new(),
            )
            .await
            .unwrap();

        assert!(
            matches!(outcome, IterationOutcome::Done(ref s) if s.contains("Self-corrected")),
            "expected Done after self-correction, got: {:?}",
            outcome
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_coordinator_self_correction_max_requeries_exceeded() {
        // All responses are malformed. After max_requeries retries, should return Err.
        let dir = TestDir::new("loopr-coord-selfcorr2");
        let stores = test_stores(&dir);

        let agent = test_coordinator(
            &dir,
            &stores,
            vec![
                "bad 1".to_string(),
                "bad 2".to_string(),
                "bad 3".to_string(),
                "bad 4".to_string(), // max_requeries=3: initial + 3 retries
            ],
            CoordinatorConfig::default(),
        );

        let result = agent
            .run_iteration(
                &mut CoordinatorState::new("test-goal".to_string(), InterviewMode::Interactive),
                &mut Lifeguard::new(),
            )
            .await;

        assert!(result.is_err(), "expected error when max_requeries exceeded");
        assert!(
            result.unwrap_err().to_string().contains("failed to parse"),
            "error should be a parse error"
        );
    }

    // --- is_coordinator_wakeup tests ---

    #[test]
    fn test_wakeup_on_transition_completed() {
        let ev = crate::ipc::protocol::DaemonEvent::new("transition.completed", serde_json::json!({}));
        assert!(super::is_coordinator_wakeup(&ev));
    }

    #[test]
    fn test_wakeup_on_record_created() {
        let ev = crate::ipc::protocol::DaemonEvent::new("record.created", serde_json::json!({}));
        assert!(super::is_coordinator_wakeup(&ev));
    }

    #[test]
    fn test_wakeup_on_tick_published() {
        let ev = crate::ipc::protocol::DaemonEvent::new("tick.published", serde_json::json!({}));
        assert!(super::is_coordinator_wakeup(&ev));
    }

    #[test]
    fn test_no_wakeup_on_agent_status_changed() {
        let ev = crate::ipc::protocol::DaemonEvent::new("agent.status_changed", serde_json::json!({}));
        assert!(!super::is_coordinator_wakeup(&ev));
    }

    #[test]
    fn test_no_wakeup_on_agent_timing_info() {
        let ev = crate::ipc::protocol::DaemonEvent::new("agent.timing_info", serde_json::json!({}));
        assert!(!super::is_coordinator_wakeup(&ev));
    }

    // --- idle wait robustness (Phase 3) ---

    /// Verify that Lagged events do NOT shorten the idle wait timeout.
    /// Under the old tokio::select! pattern, a Lagged result would cancel the sleep.
    /// Under the new tokio::time::timeout pattern, Lagged drains the channel and
    /// the inner loop continues while the outer ceiling counts down.
    #[tokio::test]
    async fn test_idle_wait_lagged_does_not_shorten_wait() {
        use std::time::{Duration, Instant};
        use tokio::sync::broadcast;

        const INTERVAL_MS: u64 = 80;

        // Capacity 1: sends beyond 1 are dropped, causing the subscriber to get Lagged.
        let (tx, _) = broadcast::channel::<i32>(1);
        let mut rx = tx.subscribe();

        // Flood the channel to guarantee Lagged on the first recv()
        let _ = tx.send(1);
        let _ = tx.send(2);
        let _ = tx.send(3);

        let start = Instant::now();

        // Run the same timeout pattern used in run_fsm_loop.
        // Irrelevant events (including the flood above) do not break; only the
        // timeout ceiling ends the wait. This mirrors the coordinator: only
        // is_coordinator_wakeup events cause an early break.
        let _ = tokio::time::timeout(Duration::from_millis(INTERVAL_MS), async {
            loop {
                match rx.recv().await {
                    Ok(_) => {} // irrelevant event — drain and continue
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Drain and continue — must not break here.
                    }
                }
            }
        })
        .await
        .ok();

        let elapsed = start.elapsed();
        // Allow a small margin for scheduling jitter.
        assert!(
            elapsed >= Duration::from_millis(INTERVAL_MS - 10),
            "Lagged events must not shorten idle wait below interval (elapsed: {:?})",
            elapsed
        );
    }

    /// Verify that a relevant wakeup event causes the idle wait to exit early.
    #[tokio::test]
    async fn test_idle_wait_exits_early_on_wakeup_event() {
        use std::time::{Duration, Instant};
        use tokio::sync::broadcast;

        const LONG_INTERVAL_MS: u64 = 5_000;
        const WAKEUP_DELAY_MS: u64 = 30;

        let (tx, _) = broadcast::channel::<crate::ipc::protocol::DaemonEvent>(16);
        let mut rx = tx.subscribe();

        // Send a wakeup event after a short delay from a background task.
        let tx2 = tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(WAKEUP_DELAY_MS)).await;
            let _ = tx2.send(crate::ipc::protocol::DaemonEvent::new(
                "decomposition.completed",
                serde_json::json!({}),
            ));
        });

        let start = Instant::now();

        let result = tokio::time::timeout(Duration::from_millis(LONG_INTERVAL_MS), async {
            loop {
                match rx.recv().await {
                    Ok(ev) if super::is_coordinator_wakeup(&ev) => break,
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    _ => {}
                }
            }
        })
        .await;

        let elapsed = start.elapsed();
        assert!(result.is_ok(), "timeout should not have fired");
        assert!(
            elapsed < Duration::from_millis(LONG_INTERVAL_MS / 2),
            "should have woken early (elapsed: {:?})",
            elapsed
        );
    }
}
