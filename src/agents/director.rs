use eyre::Result;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::agents::implementer::LlmClient;
use crate::agents::{Agent, AgentContext, AgentKind};
use crate::config::AgentRoleConfig;

/// The Director agent - top-level thinking agent that bridges the user to the system.
///
/// Four operating modes (state machine):
/// - PlanIntake: interviews the user, shapes goals into Plans with AC
/// - Monitoring: watches broadcast events for the active plan; maintains pattern tracker
/// - Escalation: diagnoses and acts when mechanical recovery fails
/// - UserIntervention: interprets user intent during execution
///
/// The Director does NOT schedule agents or drive the mechanical loop - that's
/// the engine's job. The Director provides judgment for cases the engine can't handle.
pub struct DirectorAgent<L: LlmClient> {
    pub ctx: AgentContext,
    llm: L,
    config: AgentRoleConfig,
}

impl<L: LlmClient> DirectorAgent<L> {
    pub fn new(ctx: AgentContext, llm: L, config: AgentRoleConfig) -> Self {
        Self { ctx, llm, config }
    }

    /// Determine the Director's initial activation mode from session metadata.
    ///
    /// Phase 1 retains the v3 stub's target_id-based dispatch so existing escalation
    /// spawns still work until the full event-driven run loop lands in Phase 2.
    fn activation_mode(&self) -> DirectorMode {
        if self.ctx.session.target_id.is_some() {
            DirectorMode::Escalation
        } else {
            DirectorMode::PlanIntake
        }
    }
}

/// The Director's operating mode.
///
/// Persisted on `AgentSession.director_mode` for observability. Expansion over the
/// v3 stub (`PlanIntake` + `Escalation`) to accommodate the long-lived monitoring
/// loop (`Monitoring`) and synchronous user-in-the-loop path (`UserIntervention`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DirectorMode {
    /// Interview the user to shape a goal into a Plan.
    PlanIntake,
    /// Long-lived observer: event-driven broadcast loop over a plan's lifetime.
    Monitoring,
    /// Diagnose and act on a failure escalation.
    Escalation,
    /// Translate a user chat message into plan modifications during execution.
    UserIntervention,
}

impl<L: LlmClient> Agent for DirectorAgent<L> {
    fn agent_type(&self) -> AgentKind {
        AgentKind::Director
    }

    async fn run(&mut self) -> Result<()> {
        let mode = self.activation_mode();
        debug!(
            "director: session={} mode={:?} target={:?} model={}",
            self.ctx.session.id, mode, self.ctx.session.target_id, self.config.llm.model
        );
        self.ctx.session.iteration = 1;

        match mode {
            DirectorMode::PlanIntake => {
                info!("director: plan intake mode (max_tokens={})", self.config.llm.max_tokens);
                // PlanIntake conversation loop lands in Phase 3 (director.start_plan_intake handler).
                // Phase 1 keeps this a no-op so supervision strategies can spawn the Director
                // without triggering an LLM call or infinite loop.
            }
            DirectorMode::Escalation => {
                info!("director: escalation mode for target {:?}", self.ctx.session.target_id);
                // Build escalation context and call LLM for diagnosis.
                let system_prompt = crate::prompts::store().director.clone();
                if system_prompt.is_empty() {
                    info!("director: no prompt configured, completing without LLM call");
                    return Ok(());
                }
                let user_message = format!(
                    "Escalation: mechanical recovery failed for target {:?}. Diagnose and recommend action.",
                    self.ctx.session.target_id
                );
                match self.llm.call(&system_prompt, &user_message).await {
                    Ok(response) => {
                        info!("director: escalation diagnosis complete ({}B)", response.len());
                    }
                    Err(e) => {
                        info!("director: LLM call failed (expected in test): {}", e);
                    }
                }
            }
            DirectorMode::Monitoring | DirectorMode::UserIntervention => {
                // Monitoring and UserIntervention are entered via the event-driven run loop
                // in Phase 2+, not via activation_mode(). This arm is unreachable in Phase 1
                // but exhaustiveness checking needs it.
                debug!("director: mode {:?} not yet implemented (Phase 2+)", mode);
            }
        }

        Ok(())
    }
}
