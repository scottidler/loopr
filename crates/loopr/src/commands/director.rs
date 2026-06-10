//! `loopr director ...` CLI body. Phase 8 of
//! `docs/design/2026-05-09-director-phase-2.md` shipped `chat`;
//! Phase 2 follow-ups (Item 3) of
//! `docs/design/2026-05-12-director-phase-2-followups.md` added
//! `status`. Phase 8 of `docs/design/2026-06-09-code-review-remediation.md`
//! routed both through the shared `transport::ipc_call` helper and made
//! `status` honor `--output`.

use std::path::Path;

use crate::cli::DirectorCmd;
use crate::error::LooprError;
use crate::output::{self, Format};
use crate::transport;

#[tracing::instrument(
    name = "client.director",
    level = "info",
    skip_all,
    fields(target = %target.display(), subcommand = "director"),
    err,
)]
pub fn run(target: &Path, cmd: DirectorCmd, output_format: Option<Format>) -> Result<(), LooprError> {
    match cmd {
        DirectorCmd::Chat { plan_id, message } => chat(target, plan_id, message),
        DirectorCmd::Status { plan_id } => status(target, plan_id, output_format),
    }
}

fn chat(target: &Path, plan_id: String, message: String) -> Result<(), LooprError> {
    let params = ipc::DirectorChatParams { plan_id, message };
    let result: ipc::DirectorChatResult = transport::ipc_call(target, ipc::MethodName::DirectorChat, &params)?;
    println!("note: {}", result.note_id);
    Ok(())
}

fn status(target: &Path, plan_id: String, output_format: Option<Format>) -> Result<(), LooprError> {
    let params = ipc::DirectorStatusParams { plan_id };
    let result: ipc::DirectorStatusResult = transport::ipc_call(target, ipc::MethodName::DirectorStatus, &params)?;
    let fmt = Format::resolve(output_format);
    let rendered =
        output::render(&result, fmt).map_err(|e| LooprError::ClientIo(format!("render director.status: {e}")))?;
    println!("{rendered}");
    Ok(())
}
