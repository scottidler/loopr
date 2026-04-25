//! Iteration-block renderer.
//!
//! Output format mirrors the design doc Q5 example: five subsections per
//! iteration (system prompt, user prompt, response, parsed actions,
//! dispatcher outcomes), preceded by header metadata.
//!
//! Per-iteration cap enforced here (not at append time): a single field
//! whose rendered length exceeds `ITERATION_BYTE_CAP / 4` is truncated
//! with the literal marker
//! `>[truncated: N KB original; sha=<8-char>]<`. Acceptance test asserts
//! that exact phrasing.

use std::fmt::Write;

use crate::transcript::ITERATION_BYTE_CAP;
use crate::transcript::model::TranscriptIteration;

/// Per-section cap. Splits the iteration's hard cap across the four
/// large fields (system, user, response, plus a slack for headers).
const PER_SECTION_CAP: usize = ITERATION_BYTE_CAP / 4;

/// Render one iteration block. Caller appends the result to a
/// transcript file. Trailing `---\n` separator delimits sequential
/// blocks.
pub fn render_iteration(iter: &TranscriptIteration) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "## Iteration {} - {}", iter.iteration, iter.started_at);
    s.push('\n');
    let _ = writeln!(s, "**Model:** {}", iter.model);
    let _ = writeln!(s, "**Latency:** {}ms", iter.latency_ms);
    let _ = writeln!(
        s,
        "**Tokens:** prompt={}, completion={}",
        iter.prompt_tokens, iter.completion_tokens
    );
    if !iter.session_id.is_empty() {
        let _ = writeln!(s, "**Session:** `{}`", iter.session_id);
    }
    if !iter.process_id.is_empty() {
        let _ = writeln!(s, "**Process:** `{}`", iter.process_id);
    }
    if !iter.events_log_path.is_empty() {
        let _ = writeln!(s, "**Span:** events.log at `{}`", iter.events_log_path);
    }
    s.push('\n');

    s.push_str("### Prompt (system)\n");
    s.push_str(&truncate(&iter.system_prompt, PER_SECTION_CAP));
    s.push('\n');
    s.push('\n');

    s.push_str("### Prompt (user)\n");
    s.push_str(&truncate(&iter.user_prompt, PER_SECTION_CAP));
    s.push('\n');
    s.push('\n');

    s.push_str("### Response\n");
    s.push_str(&truncate(&iter.response, PER_SECTION_CAP));
    s.push('\n');
    s.push('\n');

    s.push_str("### Parsed Actions\n");
    if iter.parsed_actions.is_empty() {
        s.push_str("(none)\n");
    } else {
        for action in &iter.parsed_actions {
            let _ = writeln!(s, "- {action}");
        }
    }
    s.push('\n');

    s.push_str("### Dispatcher Outcome\n");
    if iter.dispatcher_outcomes.is_empty() {
        s.push_str("(none)\n");
    } else {
        for outcome in &iter.dispatcher_outcomes {
            let _ = writeln!(s, "- {outcome}");
        }
    }
    if let Some(d) = &iter.lifeguard_decision {
        let _ = writeln!(s, "- Lifeguard: {d}");
    }
    s.push('\n');

    s.push_str("---\n");
    s
}

/// UTF-8 boundary-safe truncation. When `s.len() > cap`, replace the
/// elided region with the literal marker `>[truncated: N KB original;
/// sha=<8>]<`. The `sha` is the first 8 hex chars of a deterministic FNV
/// hash of the full input; recovery tools can match against re-rendered
/// content.
fn truncate(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut cut = cap;
    while !s.is_char_boundary(cut) && cut > 0 {
        cut -= 1;
    }
    let kb = s.len().div_ceil(1024);
    let sha = fnv8(s.as_bytes());
    format!("{}\n>[truncated: {kb} KB original; sha={sha}]<\n", &s[..cut])
}

/// FNV-1a 64-bit hash, hex-formatted to 8 characters. Stable across
/// platforms and Rust versions; cheap; sufficient for "did this byte
/// stream change?"
fn fnv8(bytes: &[u8]) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x100_0000_01b3;
    let mut h = FNV_OFFSET;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    format!("{:08x}", h & 0xffff_ffff)
}

/// Redact `text` against a list of path-deny patterns. A line containing
/// any pattern as a substring is replaced with
/// `[redacted: pattern=<p>]`. Used by agents before populating
/// `system_prompt` / `user_prompt` / `response` for transcripts on
/// targets where sensitive paths could appear in tool outputs (e.g. a
/// `cat .env` slipping into Implementer history).
pub fn redact_paths(text: &str, patterns: &[String]) -> String {
    if patterns.is_empty() {
        return text.to_string();
    }
    text.lines()
        .map(|line| match patterns.iter().find(|p| line.contains(p.as_str())) {
            Some(p) => format!("[redacted: pattern={p}]"),
            None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}
