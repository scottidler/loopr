//! Per-session digest. Aggregates every per-process `summary.md`
//! under one session into a single rolled-up markdown file.
//!
//! Phase 8 of the Tier-1 cleanup. The aggregator walks
//! `$XDG_DATA_HOME/loopr/sessions/<sid>/targets/*/runs/*/summary.md`,
//! parses the YAML frontmatter from each (Phase 7's renderer
//! produces it), sums the counters, and writes
//! `$XDG_DATA_HOME/loopr/sessions/<sid>/summary.md`.
//!
//! Frontmatter parsing is deliberately tiny — `key: value` per line
//! between two `---` markers — to avoid pulling a YAML crate just for
//! this rollup. Per-process digests that fail to parse emit a
//! `warn!`-level body line and skip; the rest of the walk continues.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::digest::cost::format_dollars;
use crate::session::SessionId;
use crate::xdg::{XdgError, session_dir};

/// Rolled-up counters for one session. Mirrors the `ProcessSnapshot`
/// fields, summed across every per-process digest. `process_count`
/// records how many digests successfully parsed; `skipped_count` is
/// how many were unreadable / malformed.
#[derive(Debug, Default, Clone)]
pub struct SessionAggregate {
    pub process_count: u32,
    pub skipped_count: u32,
    pub plans_created: u64,
    pub works_created: u64,
    pub works_completed: u64,
    pub works_blocked: u64,
    pub bundles_proposed: u64,
    pub bundles_accepted: u64,
    pub bundles_merged: u64,
    pub ticks_created: u64,
    pub llm_calls: u64,
    pub llm_input_tokens: u64,
    pub llm_output_tokens: u64,
    pub llm_cache_write_tokens: u64,
    pub llm_cache_read_tokens: u64,
    pub llm_cost_micros: u64,
    pub escalations: u64,
    pub corruption_count: u64,
    pub abnormal_exits: u32,
}

/// Find every `summary.md` under
/// `<sessions_root>/<sid>/targets/*/runs/*/`. Robust to missing
/// directories: returns an empty `Vec` if the session dir doesn't
/// exist (the digest writer renders an empty rollup).
pub fn discover_process_digests(session_dir_path: &Path) -> Vec<PathBuf> {
    let targets_dir = session_dir_path.join("targets");
    if !targets_dir.is_dir() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let Ok(targets) = fs::read_dir(&targets_dir) else {
        return out;
    };
    for target_entry in targets.flatten() {
        let runs_dir = target_entry.path().join("runs");
        let Ok(runs) = fs::read_dir(&runs_dir) else { continue };
        for run_entry in runs.flatten() {
            let summary = run_entry.path().join("summary.md");
            if summary.is_file() {
                out.push(summary);
            }
        }
    }
    out
}

/// Parse a Phase-7 frontmatter block from one digest file. Returns
/// `(parsed_value_map, exit_was_abnormal)` so the aggregator can
/// count both kinds of exits without re-reading the file. A failure
/// here returns `None`; the caller increments `skipped_count` and
/// moves on.
fn parse_frontmatter(path: &Path) -> Option<(std::collections::HashMap<String, String>, bool)> {
    let body = fs::read_to_string(path).ok()?;
    let mut lines = body.lines();
    if lines.next()? != "---" {
        return None;
    }
    let mut kv = std::collections::HashMap::new();
    for line in lines.by_ref() {
        if line == "---" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            kv.insert(k.trim().to_string(), v.trim().trim_matches('"').to_string());
        }
    }
    let exit = kv.get("exit").cloned().unwrap_or_default();
    let abnormal = exit.starts_with("abnormal");
    Some((kv, abnormal))
}

fn add_u64(agg: &mut u64, kv: &std::collections::HashMap<String, String>, key: &str) {
    if let Some(v) = kv.get(key).and_then(|s| s.parse::<u64>().ok()) {
        *agg = agg.saturating_add(v);
    }
}

/// Aggregate every per-process digest under a session dir.
pub fn aggregate_session(session_dir_path: &Path) -> SessionAggregate {
    let mut agg = SessionAggregate::default();
    for path in discover_process_digests(session_dir_path) {
        match parse_frontmatter(&path) {
            Some((kv, abnormal)) => {
                agg.process_count = agg.process_count.saturating_add(1);
                if abnormal {
                    agg.abnormal_exits = agg.abnormal_exits.saturating_add(1);
                }
                add_u64(&mut agg.plans_created, &kv, "plans_created");
                add_u64(&mut agg.works_created, &kv, "works_created");
                add_u64(&mut agg.works_completed, &kv, "works_completed");
                add_u64(&mut agg.works_blocked, &kv, "works_blocked");
                add_u64(&mut agg.bundles_proposed, &kv, "bundles_proposed");
                add_u64(&mut agg.bundles_accepted, &kv, "bundles_accepted");
                add_u64(&mut agg.bundles_merged, &kv, "bundles_merged");
                add_u64(&mut agg.ticks_created, &kv, "ticks_created");
                add_u64(&mut agg.llm_calls, &kv, "llm_calls");
                add_u64(&mut agg.llm_input_tokens, &kv, "llm_input_tokens");
                add_u64(&mut agg.llm_output_tokens, &kv, "llm_output_tokens");
                add_u64(&mut agg.llm_cache_write_tokens, &kv, "llm_cache_write_tokens");
                add_u64(&mut agg.llm_cache_read_tokens, &kv, "llm_cache_read_tokens");
                add_u64(&mut agg.llm_cost_micros, &kv, "llm_cost_micros");
                add_u64(&mut agg.escalations, &kv, "escalations");
                add_u64(&mut agg.corruption_count, &kv, "corruption_count");
            }
            None => {
                tracing::warn!(
                    path = %path.display(),
                    "session digest: failed to parse per-process frontmatter; skipping"
                );
                agg.skipped_count = agg.skipped_count.saturating_add(1);
            }
        }
    }
    agg
}

/// Render a session digest as YAML frontmatter + markdown body.
pub fn render_session_digest(session_id: &SessionId, agg: &SessionAggregate) -> String {
    let cost = format_dollars(agg.llm_cost_micros);
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("session_id: {session_id}\n"));
    out.push_str(&format!("process_count: {}\n", agg.process_count));
    out.push_str(&format!("skipped_count: {}\n", agg.skipped_count));
    out.push_str(&format!("abnormal_exits: {}\n", agg.abnormal_exits));
    out.push_str(&format!("plans_created: {}\n", agg.plans_created));
    out.push_str(&format!("works_created: {}\n", agg.works_created));
    out.push_str(&format!("works_completed: {}\n", agg.works_completed));
    out.push_str(&format!("works_blocked: {}\n", agg.works_blocked));
    out.push_str(&format!("bundles_proposed: {}\n", agg.bundles_proposed));
    out.push_str(&format!("bundles_accepted: {}\n", agg.bundles_accepted));
    out.push_str(&format!("bundles_merged: {}\n", agg.bundles_merged));
    out.push_str(&format!("ticks_created: {}\n", agg.ticks_created));
    out.push_str(&format!("llm_calls: {}\n", agg.llm_calls));
    out.push_str(&format!("llm_cost_micros: {}\n", agg.llm_cost_micros));
    out.push_str(&format!("escalations: {}\n", agg.escalations));
    out.push_str(&format!("corruption_count: {}\n", agg.corruption_count));
    out.push_str("---\n\n");
    out.push_str(&format!("# Session {session_id} digest\n\n"));
    out.push_str(&format!(
        "Aggregated {} process digest(s) ({} skipped, {} abnormal exit{}).\n\n",
        agg.process_count,
        agg.skipped_count,
        agg.abnormal_exits,
        if agg.abnormal_exits == 1 { "" } else { "s" }
    ));
    out.push_str("## Records\n\n");
    out.push_str(&format!("- Plans created: {}\n", agg.plans_created));
    out.push_str(&format!(
        "- Works: {} created, {} completed, {} blocked\n",
        agg.works_created, agg.works_completed, agg.works_blocked
    ));
    out.push_str(&format!(
        "- Bundles: {} proposed, {} accepted, {} merged\n",
        agg.bundles_proposed, agg.bundles_accepted, agg.bundles_merged
    ));
    out.push_str(&format!("- Ticks: {} created\n\n", agg.ticks_created));
    out.push_str("## LLM\n\n");
    out.push_str(&format!("- Calls: {}\n", agg.llm_calls));
    out.push_str(&format!(
        "- Tokens: {} input, {} output\n",
        agg.llm_input_tokens, agg.llm_output_tokens
    ));
    out.push_str(&format!(
        "- Cache: {} written, {} read\n",
        agg.llm_cache_write_tokens, agg.llm_cache_read_tokens
    ));
    out.push_str(&format!("- Cost: {cost} _(rates as of 2026-04-25)_\n\n"));
    if agg.escalations > 0 {
        out.push_str(&format!(
            "## Escalations\n\n{} lifeguard escalation(s).\n\n",
            agg.escalations
        ));
    }
    if agg.corruption_count > 0 {
        out.push_str(&format!(
            "## Corruption\n\n{} corrupt JSONL row(s) skipped during boot reconciles.\n\n",
            agg.corruption_count
        ));
    }
    out
}

/// Atomic write of the session digest to
/// `<xdg>/loopr/sessions/<sid>/summary.md`. Resolves the session dir
/// via `xdg::session_dir` so the path is consistent with where the
/// per-process digests already live.
pub fn write_session_digest(session_id: &SessionId) -> Result<PathBuf, SessionDigestError> {
    let dir = session_dir(session_id).map_err(SessionDigestError::Xdg)?;
    let agg = aggregate_session(&dir);
    let body = render_session_digest(session_id, &agg);
    let path = dir.join("summary.md");
    fs::create_dir_all(&dir).map_err(SessionDigestError::Io)?;
    let tmp = path.with_extension("md.tmp");
    fs::write(&tmp, body.as_bytes()).map_err(SessionDigestError::Io)?;
    fs::rename(&tmp, &path).map_err(SessionDigestError::Io)?;
    Ok(path)
}

#[derive(Debug, thiserror::Error)]
pub enum SessionDigestError {
    #[error("XDG resolve failed: {0}")]
    Xdg(XdgError),
    #[error("session digest I/O failed: {0}")]
    Io(io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fake_process_digest(run_dir: &Path, plans: u32, works_done: u32, llm_calls: u32, abnormal: bool) {
        fs::create_dir_all(run_dir).unwrap();
        let path = run_dir.join("summary.md");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "---").unwrap();
        writeln!(f, "started_at: 0").unwrap();
        writeln!(f, "ended_at: 60").unwrap();
        writeln!(f, "duration_s: 60").unwrap();
        writeln!(f, "model: claude-sonnet-4-6").unwrap();
        if abnormal {
            writeln!(f, "exit: \"abnormal: panic\"").unwrap();
        } else {
            writeln!(f, "exit: \"graceful\"").unwrap();
        }
        writeln!(f, "plans_created: {plans}").unwrap();
        writeln!(f, "works_created: {works_done}").unwrap();
        writeln!(f, "works_completed: {works_done}").unwrap();
        writeln!(f, "works_blocked: 0").unwrap();
        writeln!(f, "bundles_proposed: 0").unwrap();
        writeln!(f, "bundles_accepted: 0").unwrap();
        writeln!(f, "bundles_merged: 0").unwrap();
        writeln!(f, "ticks_created: 0").unwrap();
        writeln!(f, "llm_calls: {llm_calls}").unwrap();
        writeln!(f, "llm_input_tokens: 0").unwrap();
        writeln!(f, "llm_output_tokens: 0").unwrap();
        writeln!(f, "llm_cache_write_tokens: 0").unwrap();
        writeln!(f, "llm_cache_read_tokens: 0").unwrap();
        writeln!(f, "llm_cost_micros: 0").unwrap();
        writeln!(f, "escalations: 0").unwrap();
        writeln!(f, "corruption_count: 0").unwrap();
        writeln!(f, "---").unwrap();
        writeln!(f, "# body").unwrap();
    }

    #[test]
    fn discover_finds_summaries_under_targets_runs() {
        let td = tempfile::TempDir::new().unwrap();
        let session_dir = td.path().join("sessions").join("S1");
        let r1 = session_dir.join("targets").join("a").join("runs").join("p1");
        let r2 = session_dir.join("targets").join("a").join("runs").join("p2");
        let r3 = session_dir.join("targets").join("b").join("runs").join("p3");
        fake_process_digest(&r1, 1, 1, 1, false);
        fake_process_digest(&r2, 1, 1, 1, false);
        fake_process_digest(&r3, 0, 0, 1, false);

        let found = discover_process_digests(&session_dir);
        assert_eq!(found.len(), 3);
    }

    #[test]
    fn aggregate_sums_counters_across_processes() {
        let td = tempfile::TempDir::new().unwrap();
        let sd = td.path().join("sessions").join("S1");
        fake_process_digest(&sd.join("targets").join("a").join("runs").join("p1"), 1, 2, 3, false);
        fake_process_digest(&sd.join("targets").join("a").join("runs").join("p2"), 2, 1, 5, true);

        let agg = aggregate_session(&sd);
        assert_eq!(agg.process_count, 2);
        assert_eq!(agg.abnormal_exits, 1);
        assert_eq!(agg.plans_created, 3);
        assert_eq!(agg.works_completed, 3);
        assert_eq!(agg.llm_calls, 8);
    }

    #[test]
    fn aggregate_handles_unreadable_frontmatter() {
        let td = tempfile::TempDir::new().unwrap();
        let sd = td.path().join("sessions").join("S1");
        let bad = sd.join("targets").join("a").join("runs").join("p1");
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join("summary.md"), "this is not valid frontmatter").unwrap();

        let agg = aggregate_session(&sd);
        assert_eq!(agg.process_count, 0);
        assert_eq!(agg.skipped_count, 1);
    }

    #[test]
    fn aggregate_empty_session_returns_zero() {
        let td = tempfile::TempDir::new().unwrap();
        let sd = td.path().join("sessions").join("S1");
        let agg = aggregate_session(&sd);
        assert_eq!(agg.process_count, 0);
    }

    #[test]
    fn render_session_digest_includes_frontmatter_and_body() {
        let sid = SessionId::parse("20260422-000000").unwrap();
        let agg = SessionAggregate {
            process_count: 3,
            plans_created: 2,
            llm_cost_micros: 5_500_000,
            ..SessionAggregate::default()
        };
        let out = render_session_digest(&sid, &agg);
        assert!(out.contains("session_id: 20260422-000000"));
        assert!(out.contains("# Session 20260422-000000 digest"));
        assert!(out.contains("Plans created: 2"));
        assert!(out.contains("$5.50"));
    }
}
