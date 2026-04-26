//! Per-process digest. The daemon (or any loopr CLI process) holds a
//! `ProcessSnapshot` for its lifetime, increments counters during the
//! run, and writes a markdown rollup to
//! `$XDG_DATA_HOME/loopr/sessions/<sid>/targets/<slug>/runs/<pid>/summary.md`
//! at exit.
//!
//! Phase 7 of the Tier-1 cleanup. Phase 8 layers a session-level
//! aggregator on top that walks every per-process digest under one
//! session.
//!
//! The renderer produces YAML frontmatter (machine-parseable for the
//! Phase 8 aggregator) followed by a human-readable markdown body.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::digest::cost::{cost_micros, format_dollars};

/// Counters accumulated during one daemon (or CLI) lifetime. Held
/// inside `Arc<Mutex<_>>` by the daemon's `DaemonContext` so handler
/// tasks can increment from across `await` points.
#[derive(Debug, Clone)]
pub struct ProcessSnapshot {
    pub started_at: SystemTime,
    pub model: String,
    pub plans_created: u32,
    pub works_created: u32,
    pub works_completed: u32,
    pub works_blocked: u32,
    pub bundles_proposed: u32,
    pub bundles_accepted: u32,
    pub bundles_merged: u32,
    pub ticks_created: u32,
    pub llm_calls: u32,
    pub llm_input_tokens: u64,
    pub llm_output_tokens: u64,
    pub llm_cache_write_tokens: u64,
    pub llm_cache_read_tokens: u64,
    /// Cumulative LLM cost in U.S. micro-dollars; 1_000_000 == $1.00.
    pub llm_cost_micros: u64,
    pub escalations: u32,
    /// Corruption count surfaced by the boot-time reconcile sweep.
    /// Phase 2 added the source field on `ReconcileReport`; this
    /// snapshot mirror lets the per-process digest record the boot's
    /// corruption tally even when `--accept-corruption` was used.
    pub corruption_count: u32,
    /// `Some(message)` for an abnormal exit (panic / SIGQUIT / forced
    /// kill); `None` for a graceful drain. Set by the panic hook /
    /// SIGQUIT handler before the digest is rendered.
    pub abnormal_exit: Option<String>,
}

impl ProcessSnapshot {
    /// Construct a fresh snapshot at process start.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            started_at: SystemTime::now(),
            model: model.into(),
            plans_created: 0,
            works_created: 0,
            works_completed: 0,
            works_blocked: 0,
            bundles_proposed: 0,
            bundles_accepted: 0,
            bundles_merged: 0,
            ticks_created: 0,
            llm_calls: 0,
            llm_input_tokens: 0,
            llm_output_tokens: 0,
            llm_cache_write_tokens: 0,
            llm_cache_read_tokens: 0,
            llm_cost_micros: 0,
            escalations: 0,
            corruption_count: 0,
            abnormal_exit: None,
        }
    }

    /// Record one LLM call's usage. Increments token counts and
    /// accumulates the per-call cost via the rate table; unknown
    /// models add 0 and emit no warning (the table is small and
    /// hand-maintained, so omissions are intentional rather than
    /// errors).
    pub fn record_llm_call(
        &mut self,
        input_tokens: u64,
        output_tokens: u64,
        cache_write_tokens: u64,
        cache_read_tokens: u64,
    ) {
        self.llm_calls = self.llm_calls.saturating_add(1);
        self.llm_input_tokens = self.llm_input_tokens.saturating_add(input_tokens);
        self.llm_output_tokens = self.llm_output_tokens.saturating_add(output_tokens);
        self.llm_cache_write_tokens = self.llm_cache_write_tokens.saturating_add(cache_write_tokens);
        self.llm_cache_read_tokens = self.llm_cache_read_tokens.saturating_add(cache_read_tokens);
        let micros = cost_micros(
            &self.model,
            input_tokens,
            output_tokens,
            cache_write_tokens,
            cache_read_tokens,
        );
        self.llm_cost_micros = self.llm_cost_micros.saturating_add(micros);
    }
}

/// Render the per-process digest as YAML frontmatter + markdown body.
pub fn render_process_digest(snapshot: &ProcessSnapshot, ended_at: SystemTime) -> String {
    let started_unix = snapshot
        .started_at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ended_unix = ended_at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let duration_s = ended_unix.saturating_sub(started_unix);

    let exit_kind = match &snapshot.abnormal_exit {
        Some(msg) => format!("abnormal: {msg}"),
        None => "graceful".to_string(),
    };
    let cost = format_dollars(snapshot.llm_cost_micros);

    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("started_at: {started_unix}\n"));
    out.push_str(&format!("ended_at: {ended_unix}\n"));
    out.push_str(&format!("duration_s: {duration_s}\n"));
    out.push_str(&format!("model: {}\n", snapshot.model));
    out.push_str(&format!("exit: \"{exit_kind}\"\n"));
    out.push_str(&format!("plans_created: {}\n", snapshot.plans_created));
    out.push_str(&format!("works_created: {}\n", snapshot.works_created));
    out.push_str(&format!("works_completed: {}\n", snapshot.works_completed));
    out.push_str(&format!("works_blocked: {}\n", snapshot.works_blocked));
    out.push_str(&format!("bundles_proposed: {}\n", snapshot.bundles_proposed));
    out.push_str(&format!("bundles_accepted: {}\n", snapshot.bundles_accepted));
    out.push_str(&format!("bundles_merged: {}\n", snapshot.bundles_merged));
    out.push_str(&format!("ticks_created: {}\n", snapshot.ticks_created));
    out.push_str(&format!("llm_calls: {}\n", snapshot.llm_calls));
    out.push_str(&format!("llm_input_tokens: {}\n", snapshot.llm_input_tokens));
    out.push_str(&format!("llm_output_tokens: {}\n", snapshot.llm_output_tokens));
    out.push_str(&format!(
        "llm_cache_write_tokens: {}\n",
        snapshot.llm_cache_write_tokens
    ));
    out.push_str(&format!("llm_cache_read_tokens: {}\n", snapshot.llm_cache_read_tokens));
    out.push_str(&format!("llm_cost_micros: {}\n", snapshot.llm_cost_micros));
    out.push_str(&format!("escalations: {}\n", snapshot.escalations));
    out.push_str(&format!("corruption_count: {}\n", snapshot.corruption_count));
    out.push_str("---\n\n");

    out.push_str("# Process digest\n\n");
    out.push_str(&format!("Duration: {duration_s}s. Exit: {exit_kind}.\n\n"));
    out.push_str("## Records\n\n");
    out.push_str(&format!("- Plans created: {}\n", snapshot.plans_created));
    out.push_str(&format!(
        "- Works: {} created, {} completed, {} blocked\n",
        snapshot.works_created, snapshot.works_completed, snapshot.works_blocked
    ));
    out.push_str(&format!(
        "- Bundles: {} proposed, {} accepted, {} merged\n",
        snapshot.bundles_proposed, snapshot.bundles_accepted, snapshot.bundles_merged
    ));
    out.push_str(&format!("- Ticks: {} created\n\n", snapshot.ticks_created));
    out.push_str("## LLM\n\n");
    out.push_str(&format!("- Calls: {}\n", snapshot.llm_calls));
    out.push_str(&format!("- Model: {}\n", snapshot.model));
    out.push_str(&format!(
        "- Tokens: {} input, {} output\n",
        snapshot.llm_input_tokens, snapshot.llm_output_tokens
    ));
    out.push_str(&format!(
        "- Cache: {} written, {} read\n",
        snapshot.llm_cache_write_tokens, snapshot.llm_cache_read_tokens
    ));
    out.push_str(&format!("- Cost: {cost} _(rates as of 2026-04-25)_\n\n"));
    if snapshot.escalations > 0 {
        out.push_str(&format!(
            "## Escalations\n\n{} lifeguard escalation(s).\n\n",
            snapshot.escalations
        ));
    }
    if snapshot.corruption_count > 0 {
        out.push_str(&format!(
            "## Corruption\n\n{} corrupt JSONL row(s) skipped during boot reconcile.\n\n",
            snapshot.corruption_count
        ));
    }
    out
}

/// Atomic write of the digest to `<process_run_dir>/summary.md`.
/// `process_run_dir` is the directory this PID owns under the
/// session's run tree; the caller already created it during
/// `telemetry::init`. Failure to write returns `io::Error`; callers
/// (the panic hook, the post-loop tail) wrap with `warn!` and continue.
pub fn write_process_digest(process_run_dir: &Path, snapshot: &ProcessSnapshot) -> io::Result<PathBuf> {
    let body = render_process_digest(snapshot, SystemTime::now());
    let path = process_run_dir.join("summary.md");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("md.tmp");
    fs::write(&tmp, body.as_bytes())?;
    fs::rename(&tmp, &path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap_with_some_data() -> ProcessSnapshot {
        let mut s = ProcessSnapshot::new("claude-sonnet-4-6");
        s.plans_created = 1;
        s.works_created = 5;
        s.works_completed = 4;
        s.works_blocked = 1;
        s.bundles_proposed = 5;
        s.bundles_accepted = 4;
        s.bundles_merged = 4;
        s.ticks_created = 4;
        s.escalations = 1;
        s.corruption_count = 2;
        s
    }

    #[test]
    fn record_llm_call_increments_counters() {
        let mut s = ProcessSnapshot::new("claude-sonnet-4-6");
        s.record_llm_call(100, 50, 0, 0);
        assert_eq!(s.llm_calls, 1);
        assert_eq!(s.llm_input_tokens, 100);
        assert_eq!(s.llm_output_tokens, 50);
        assert!(s.llm_cost_micros > 0);
    }

    #[test]
    fn record_llm_call_unknown_model_zero_cost() {
        let mut s = ProcessSnapshot::new("not-a-model");
        s.record_llm_call(1_000_000, 1_000_000, 0, 0);
        assert_eq!(s.llm_cost_micros, 0);
    }

    #[test]
    fn render_includes_frontmatter_and_body() {
        let s = snap_with_some_data();
        let out = render_process_digest(&s, SystemTime::now());
        assert!(out.starts_with("---\n"));
        assert!(out.contains("# Process digest"));
        assert!(out.contains("plans_created: 1"));
        assert!(out.contains("Plans created: 1"));
        assert!(out.contains("Cache: 0 written, 0 read"));
    }

    #[test]
    fn render_marks_abnormal_exit() {
        let mut s = ProcessSnapshot::new("claude-sonnet-4-6");
        s.abnormal_exit = Some("panic in handler X".to_string());
        let out = render_process_digest(&s, SystemTime::now());
        assert!(out.contains("abnormal: panic in handler X"));
    }

    #[test]
    fn render_emits_corruption_section_only_when_nonzero() {
        let mut s = ProcessSnapshot::new("claude-sonnet-4-6");
        let out = render_process_digest(&s, SystemTime::now());
        assert!(!out.contains("## Corruption"));
        s.corruption_count = 3;
        let out2 = render_process_digest(&s, SystemTime::now());
        assert!(out2.contains("## Corruption"));
        assert!(out2.contains("3 corrupt JSONL row"));
    }

    #[test]
    fn write_process_digest_creates_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = snap_with_some_data();
        let path = write_process_digest(dir.path(), &s).unwrap();
        assert!(path.exists());
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("# Process digest"));
    }
}
