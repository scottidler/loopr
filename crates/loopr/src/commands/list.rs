//! `loopr plans|works|bundles|ticks` bodies.
//!
//! Each verb connects to the daemon, issues `record.list` with its
//! `RecordKind`, and renders the returned `RecordsResult` as YAML or
//! JSON per `Format::resolve`. The daemon returns summary projections
//! (not full records) so responses stay well under the IPC frame cap
//! at mature-repo scale.

use std::path::Path;

use ipc::{MethodName, RecordKind, RecordListParams, RecordsResult};

use crate::error::LooprError;
use crate::output::{self, Format};
use crate::transport;

/// `loopr plans` body.
#[tracing::instrument(
    name = "client.list",
    level = "info",
    skip_all,
    fields(target = %target.display(), kind = ?kind, subcommand = "list"),
    err,
)]
pub fn run(target: &Path, explicit_format: Option<Format>, kind: RecordKind) -> Result<(), LooprError> {
    // Phase 16 of `docs/design/2026-07-11-verified-swarm.md`: a read verb
    // must not auto-fork a daemon. Report "no daemon running" and return
    // rather than let `connect_or_wait` poll the full daemon-startup
    // budget waiting for a socket that will never appear.
    if !crate::daemon::is_running(target)? {
        println!("no daemon running");
        return Ok(());
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| LooprError::ClientIo(format!("runtime build: {e}")))?;
    rt.block_on(async {
        let mut client = transport::connect_or_wait(target).await?;
        client.handshake(None).await?;
        let params = RecordListParams { kind };
        let params_value = serde_json::to_value(&params)
            .map_err(|e| LooprError::ClientIo(format!("serialize record.list params: {e}")))?;
        let (resp, _events) = client.request(MethodName::RecordList, params_value).await?;
        if let Some(err) = resp.error {
            return Err(LooprError::Rpc(err));
        }
        let result_value = resp
            .result
            .ok_or_else(|| LooprError::ClientIo("record.list response missing result".into()))?;
        let result: RecordsResult = serde_json::from_value(result_value)
            .map_err(|e| LooprError::ClientIo(format!("decode record.list: {e}")))?;
        validate_kind_match(&result, kind)?;
        let fmt = Format::resolve(explicit_format);
        let rendered =
            output::render(&result, fmt).map_err(|e| LooprError::ClientIo(format!("render record.list: {e}")))?;
        println!("{rendered}");
        Ok(())
    })
}

/// Defensive check: the daemon returned the kind the CLI asked for.
/// A mismatch is a daemon bug; we surface it as a client-side error
/// rather than silently rendering the wrong sum-type arm.
fn validate_kind_match(result: &RecordsResult, requested: RecordKind) -> Result<(), LooprError> {
    let got = match result {
        RecordsResult::Plans(_) => RecordKind::Plan,
        RecordsResult::Works(_) => RecordKind::Work,
        RecordsResult::Bundles(_) => RecordKind::Bundle,
        RecordsResult::Ticks(_) => RecordKind::Tick,
    };
    if got != requested {
        return Err(LooprError::ClientIo(format!(
            "daemon returned {got:?} for a RecordList(kind={requested:?}) request (protocol mismatch)"
        )));
    }
    Ok(())
}
