use eyre::Result;
use std::path::PathBuf;
use tokio::sync::broadcast;
use tracing::{debug, info};

use crate::agents::llm_client::AgentLlmClient;
use crate::ipc::protocol::DaemonEvent;

/// Create the LLM client. Fails if the API key env var is not set.
///
/// When `conversation_log` is provided as `(dir, name)`, LLM request/response pairs are
/// appended to `{dir}/{name}.log`. The conversations dir is only created by `setup_logging`
/// when the log level is not INFO, so passing the dir is safe at any log level.
pub(super) fn create_llm_client(
    config: &crate::config::AgentRoleConfig,
    session_id: &str,
    event_tx: &broadcast::Sender<DaemonEvent>,
    conversation_log: Option<(PathBuf, String)>,
) -> Result<AgentLlmClient> {
    debug!(
        "create_llm_client(session_id={}, model={})",
        session_id, config.llm.model
    );
    let client = AgentLlmClient::new(config.clone(), session_id.to_string(), event_tx.clone())?;
    let client = if let Some((dir, name)) = conversation_log {
        client.with_conversation_log(dir, name)
    } else {
        client
    };
    info!("Agent {} using LLM client (model: {})", session_id, config.llm.model);
    Ok(client)
}
