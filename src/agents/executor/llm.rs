use eyre::Result;
use log::{debug, info};
use tokio::sync::broadcast;

use crate::agents::llm_client::AgentLlmClient;
use crate::ipc::protocol::DaemonEvent;

/// Create the LLM client. Fails if the API key env var is not set.
pub(super) fn create_llm_client(
    config: &crate::config::AgentRoleConfig,
    session_id: &str,
    event_tx: &broadcast::Sender<DaemonEvent>,
) -> Result<AgentLlmClient> {
    debug!("create_llm_client(session_id={}, model={})", session_id, config.model);
    let client = AgentLlmClient::new(config.clone(), session_id.to_string(), event_tx.clone())?;
    info!("Agent {} using LLM client (model: {})", session_id, config.model);
    Ok(client)
}
