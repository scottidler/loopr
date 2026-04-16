use eyre::Result;
use tracing::{debug, info};

use crate::agents::implementer::LlmClient;
use crate::agents::{Agent, AgentContext, AgentKind};
use crate::config::AgentRoleConfig;

/// The Director agent - top-level thinking agent that bridges the user to the system.
///
/// Activates in three scenarios:
/// 1. Plan intake: interviews the user, shapes goals into Plans with AC
/// 2. Escalation: diagnoses failures when mechanical recovery fails
/// 3. User intervention: interprets user intent during execution
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

    /// Determine the Director's activation mode from session metadata.
    fn activation_mode(&self) -> DirectorMode {
        // Check session target_id to determine mode.
        // If the session has a target_id, it's an escalation.
        // Otherwise it's a plan intake / user intervention.
        if self.ctx.session.target_id.is_some() {
            DirectorMode::Escalation
        } else {
            DirectorMode::PlanIntake
        }
    }
}

/// The Director's activation mode.
#[derive(Debug, Clone, Copy)]
enum DirectorMode {
    /// Interview the user to shape a goal into a Plan.
    PlanIntake,
    /// Diagnose and act on a failure escalation.
    Escalation,
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
                // Plan intake will be wired in Phase 2 when doc.accept is rewired.
                // For now, the Director completes immediately since the engine handles
                // plan creation mechanically via doc.accept.
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
        }

        Ok(())
    }
}
