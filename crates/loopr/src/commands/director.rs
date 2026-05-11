//! `loopr director ...` CLI body. Phase 8 of
//! `docs/design/2026-05-09-director-phase-2.md` shipped `chat`;
//! Phase 2 follow-ups (Item 3) of
//! `docs/design/2026-05-12-director-phase-2-followups.md` added
//! `status`.

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
            DirectorCmd::Status { plan_id } => status(target, plan_id).await,
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

async fn status(target: &Path, plan_id: String) -> Result<(), LooprError> {
    let mut client = transport::connect_or_wait(target).await?;
    client.handshake(None).await?;
    let params = ipc::DirectorStatusParams { plan_id };
    let params_value = serde_json::to_value(&params)
        .map_err(|e| LooprError::ClientIo(format!("serialize director.status params: {e}")))?;
    let (resp, _events) = client.request(ipc::MethodName::DirectorStatus, params_value).await?;
    if let Some(err) = resp.error {
        return Err(LooprError::Rpc(err));
    }
    let result_value = resp
        .result
        .ok_or_else(|| LooprError::ClientIo("director.status response missing result".into()))?;
    let result: ipc::DirectorStatusResult = serde_json::from_value(result_value)
        .map_err(|e| LooprError::ClientIo(format!("decode director.status: {e}")))?;
    print_status(&result);
    Ok(())
}

fn print_status(result: &ipc::DirectorStatusResult) {
    println!("plan:           {}", result.plan_id);
    println!("status:         {}", result.plan_status);
    match &result.snapshot {
        None => {
            println!("director:       not running (plan is {})", result.plan_status);
        }
        Some(s) => {
            println!("director mode:  {}", s.mode);
            println!("no-progress:    streak={}", s.no_progress_streak);
            println!("same-action:    streak={}", s.same_action_streak);
            match (
                s.last_action_kind.as_deref(),
                s.last_action_target_id.as_deref(),
                s.last_action_ts,
            ) {
                (None, _, _) => println!("last action:    (none this iteration)"),
                (Some(kind), Some(target), Some(ts)) => {
                    println!("last action:    {kind} {target}  (ts={ts}ms)");
                }
                (Some(kind), None, Some(ts)) => {
                    println!("last action:    {kind}  (ts={ts}ms)");
                }
                (Some(kind), Some(target), None) => {
                    println!("last action:    {kind} {target}");
                }
                (Some(kind), None, None) => {
                    println!("last action:    {kind}");
                }
            }
            println!("unread notes:   {}", s.unread_note_count);
            println!("iteration:      {}", s.iteration);
            println!("needs-operator: {} iters", s.needs_operator_iters);
        }
    }
}
