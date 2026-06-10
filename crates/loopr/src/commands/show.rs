//! `loopr show <id>` body.
//!
//! Uses the 2-char id prefix to fail fast on an obviously-malformed id
//! before touching the network, then issues one `record.get` IPC call.
//! The prefix literals mirror the `$prefix` arguments to the `id_type!`
//! macro invocations in `crates/domain/src/id.rs`; changing one without
//! the other breaks this match silently, so keep them in lockstep.
//!
//! Exact id match only. Fuzzy/partial matching is a TUI behavior, not a
//! plumbing concern.

use std::path::Path;

use ipc::{MethodName, RecordGetParams, RecordKind, RecordResult};

use crate::error::LooprError;
use crate::output::{self, Format};
use crate::transport;

#[tracing::instrument(
    name = "client.show",
    level = "info",
    skip_all,
    fields(target = %target.display(), record_id = %id, subcommand = "show"),
    err,
)]
pub fn run(target: &Path, explicit_format: Option<Format>, id: String) -> Result<(), LooprError> {
    // Local prefix check so a bad id doesn't trigger a pointless IPC
    // round-trip; the prefix-derived kind is also cross-checked against the
    // daemon's returned record below (the defensive check list.rs already
    // does), so a routing/serde mismatch surfaces as a clear client error
    // rather than rendering the wrong sum-type arm.
    let kind = kind_from_prefix(&id)?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| LooprError::ClientIo(format!("runtime build: {e}")))?;
    rt.block_on(async {
        let mut client = transport::connect_or_wait(target).await?;
        client.handshake(None).await?;
        let params = RecordGetParams { id: id.clone() };
        let params_value = serde_json::to_value(&params)
            .map_err(|e| LooprError::ClientIo(format!("serialize record.get params: {e}")))?;
        let (resp, _events) = client.request(MethodName::RecordGet, params_value).await?;
        if let Some(err) = resp.error {
            return Err(LooprError::Rpc(err));
        }
        let result_value = resp
            .result
            .ok_or_else(|| LooprError::ClientIo("record.get response missing result".into()))?;
        let result: RecordResult = serde_json::from_value(result_value)
            .map_err(|e| LooprError::ClientIo(format!("decode record.get: {e}")))?;
        validate_kind_match(&result, kind)?;
        let fmt = Format::resolve(explicit_format);
        let rendered =
            output::render(&result, fmt).map_err(|e| LooprError::ClientIo(format!("render record.get: {e}")))?;
        println!("{rendered}");
        Ok(())
    })
}

/// Defensive check: the record the daemon returned matches the kind the
/// id prefix implied. A mismatch is a daemon routing or serde bug; surface
/// it as a client-side error rather than silently rendering the wrong arm.
/// Mirrors `list::validate_kind_match`.
fn validate_kind_match(result: &RecordResult, requested: RecordKind) -> Result<(), LooprError> {
    let got = match result {
        RecordResult::Plan(_) => RecordKind::Plan,
        RecordResult::Work(_) => RecordKind::Work,
        RecordResult::Bundle(_) => RecordKind::Bundle,
        RecordResult::Tick(_) => RecordKind::Tick,
    };
    if got != requested {
        return Err(LooprError::ClientIo(format!(
            "daemon returned {got:?} for a RecordGet(prefix-kind={requested:?}) request (protocol mismatch)"
        )));
    }
    Ok(())
}

fn kind_from_prefix(id: &str) -> Result<RecordKind, LooprError> {
    let prefix = id.split('-').next().unwrap_or("");
    match prefix {
        "pl" => Ok(RecordKind::Plan),
        "wk" => Ok(RecordKind::Work),
        "bd" => Ok(RecordKind::Bundle),
        "tk" => Ok(RecordKind::Tick),
        _ => Err(LooprError::UnknownIdPrefix { id: id.into() }),
    }
}

#[cfg(test)]
mod tests;
