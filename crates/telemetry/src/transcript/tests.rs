use super::*;
use crate::transcript::model::TranscriptIteration;
use crate::transcript::render::{redact_paths, render_iteration};

fn sample_iter() -> TranscriptIteration {
    let mut it = TranscriptIteration::new_single_turn("claude-opus-4-7", "2026-04-24T15:03:13");
    it.latency_ms = 4200;
    it.prompt_tokens = 3812;
    it.completion_tokens = 247;
    it.session_id = "20260424-150000".to_string();
    it.process_id = "pc-k3m9f2".to_string();
    it.events_log_path = "$XDG/loopr/sessions/20260424-150000/runs/pc-k3m9f2/events.log".to_string();
    it.system_prompt = "you are an implementer".to_string();
    it.user_prompt = "do the thing".to_string();
    it.response = "[]".to_string();
    it.parsed_actions = vec!["run_tool: bash".to_string()];
    it.dispatcher_outcomes = vec!["ok".to_string()];
    it.lifeguard_decision = Some("continue".to_string());
    it
}

#[test]
fn renders_required_subsections() {
    let it = sample_iter();
    let out = render_iteration(&it);
    assert!(out.contains("## Iteration 1 - 2026-04-24T15:03:13"));
    assert!(out.contains("**Model:** claude-opus-4-7"));
    assert!(out.contains("**Latency:** 4200ms"));
    assert!(out.contains("**Tokens:** prompt=3812, completion=247"));
    assert!(out.contains("**Session:** `20260424-150000`"));
    assert!(out.contains("**Process:** `pc-k3m9f2`"));
    assert!(out.contains("### Prompt (system)\nyou are an implementer"));
    assert!(out.contains("### Prompt (user)\ndo the thing"));
    assert!(out.contains("### Response\n[]"));
    assert!(out.contains("### Parsed Actions\n- run_tool: bash"));
    assert!(out.contains("### Dispatcher Outcome\n- ok\n- Lifeguard: continue"));
    assert!(out.ends_with("---\n"));
}

#[test]
fn empty_actions_render_as_none() {
    let mut it = sample_iter();
    it.parsed_actions = Vec::new();
    it.dispatcher_outcomes = Vec::new();
    it.lifeguard_decision = None;
    let out = render_iteration(&it);
    assert!(out.contains("### Parsed Actions\n(none)"));
    assert!(out.contains("### Dispatcher Outcome\n(none)"));
}

#[test]
fn truncation_marker_is_literal() {
    let mut it = sample_iter();
    // Force the response section over the per-section cap.
    let oversize = "x".repeat(crate::transcript::ITERATION_BYTE_CAP);
    it.response = oversize;
    let out = render_iteration(&it);
    assert!(out.contains(">[truncated:"), "expected truncation marker in {out}");
    assert!(out.contains("KB original; sha="));
    // Marker uses literal `]<` close per design doc spec.
    assert!(out.contains("]<"));
}

#[test]
fn append_iteration_writes_then_appends() {
    let td = tempfile::TempDir::new().unwrap();
    let path = td.path().join("transcript.md");
    let it1 = sample_iter();
    append_iteration(&path, &it1).unwrap();
    let mut it2 = sample_iter();
    it2.iteration = 2;
    it2.user_prompt = "iteration two".to_string();
    append_iteration(&path, &it2).unwrap();
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("## Iteration 1"));
    assert!(body.contains("## Iteration 2"));
    assert!(body.contains("iteration two"));
}

#[test]
fn redact_replaces_lines_matching_pattern() {
    let text = "alpha\nfoo .env bar\nbeta\ncreds = baz\n";
    let patterns = vec![".env".to_string(), "creds".to_string()];
    let out = redact_paths(text, &patterns);
    assert_eq!(out, "alpha\n[redacted: pattern=.env]\nbeta\n[redacted: pattern=creds]");
}

#[test]
fn redact_with_empty_patterns_is_identity() {
    let text = "anything goes here";
    let out = redact_paths(text, &[]);
    assert_eq!(out, text);
}
