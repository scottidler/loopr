//! `loopr director ...` CLI body. Phase 8 of
//! `docs/design/2026-05-09-director-phase-2.md`.
//!
//! Currently exposes `chat`; future `director` subverbs (e.g.
//! `status`, `clear-stalled`) will land here too.

use std::path::Path;

use crate::cli::DirectorCmd;
use crate::error::LooprError;
use crate::transport;

pub fn run(target: &Path, cmd: DirectorCmd) -> Result<(), LooprError> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| LooprError::ClientIo(format!("runtime build: {e}")))?;
    rt.block_on(async {
        match cmd {
            DirectorCmd::Chat { plan_id, message } => chat(target, plan_id, message).await,
        }
    })
}

async fn chat(target: &Path, plan_id: String, message: String) -> Result<(), LooprError> {
    let mut client = transport::connect_or_wait(target).await?;
    client.handshake(None).await?;
    let params = ipc::DirectorChatParams { plan_id, message };
    let params_value = serde_json::to_value(&params)
        .map_err(|e| LooprError::ClientIo(format!("serialize director.chat params: {e}")))?;
    let (resp, _events) = client.request(ipc::MethodName::DirectorChat, params_value).await?;
    if let Some(err) = resp.error {
        return Err(LooprError::Rpc(err));
    }
    let result_value = resp
        .result
        .ok_or_else(|| LooprError::ClientIo("director.chat response missing result".into()))?;
    let result: ipc::DirectorChatResult =
        serde_json::from_value(result_value).map_err(|e| LooprError::ClientIo(format!("decode director.chat: {e}")))?;
    println!("note: {}", result.note_id);
    Ok(())
}
