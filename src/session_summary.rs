use std::fs;
use std::path::Path;

use log::info;

/// Generate a session summary from the session log file.
/// Parses `[transition]`, `[agent_status]`, `[agent:*] tool_result`, and error-level lines.
/// Writes `summary.md` to the session directory.
pub fn generate_summary(session_dir: &Path, session_id: &str, start_time: &str) {
    let log_path = session_dir.join("loopr.log");
    let content = match fs::read_to_string(&log_path) {
        Ok(c) => c,
        Err(e) => {
            info!("Could not read session log for summary: {}", e);
            return;
        }
    };

    let summary = build_summary(&content, session_id, start_time);

    let summary_path = session_dir.join("summary.md");
    if let Err(e) = fs::write(&summary_path, &summary) {
        info!("Failed to write session summary: {}", e);
    } else {
        info!("Session summary written to {}", summary_path.display());
    }
}

fn build_summary(log_content: &str, session_id: &str, start_time: &str) -> String {
    let mut transitions = Vec::new();
    let mut agent_statuses = Vec::new();
    let mut errors = Vec::new();
    let mut tool_calls_total = 0u64;
    let mut tool_calls_failed = 0u64;

    for line in log_content.lines() {
        if line.contains("[transition]") {
            transitions.push(extract_after(line, "[transition]"));
        } else if line.contains("[agent_status]") {
            agent_statuses.push(extract_after(line, "[agent_status]"));
        } else if line.contains("tool_result:") {
            tool_calls_total += 1;
            if line.contains("is_error=true") {
                tool_calls_failed += 1;
            }
        } else if line.contains("ERROR") {
            errors.push(line.trim().to_string());
        }
    }

    let tool_calls_success = tool_calls_total.saturating_sub(tool_calls_failed);

    let mut out = String::new();
    out.push_str(&format!("# Loopr Session {} (started {})\n\n", session_id, start_time));

    if !transitions.is_empty() {
        out.push_str("## State Changes\n");
        for t in &transitions {
            out.push_str(&format!("- {}\n", t));
        }
        out.push('\n');
    }

    if !agent_statuses.is_empty() {
        out.push_str("## Agent Status Changes\n");
        for s in &agent_statuses {
            out.push_str(&format!("- {}\n", s));
        }
        out.push('\n');
    }

    if !errors.is_empty() {
        out.push_str("## Errors\n");
        for (i, e) in errors.iter().enumerate() {
            let short = if e.len() > 200 { &e[..200] } else { e };
            out.push_str(&format!("{}. {}\n", i + 1, short));
        }
        out.push('\n');
    }

    out.push_str(&format!(
        "## Tool Calls: {} total ({} success, {} failed)\n",
        tool_calls_total, tool_calls_success, tool_calls_failed
    ));

    out
}

fn extract_after(line: &str, marker: &str) -> String {
    if let Some(pos) = line.find(marker) {
        line[pos + marker.len()..].trim().to_string()
    } else {
        line.trim().to_string()
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_summary_empty_log() {
        let summary = build_summary("", "20260305T143200", "2026-03-05T14:32:00");
        assert!(summary.contains("# Loopr Session 20260305T143200"));
        assert!(summary.contains("0 total"));
    }

    #[test]
    fn test_build_summary_with_transitions() {
        let log = "\
[2026-03-05 14:32:01 DEBUG loopr] [transition] work.wi-1: Draft -> InProgress by Coordinator
[2026-03-05 14:32:02 DEBUG loopr] [transition] work.wi-1: InProgress -> Review by Implementer
";
        let summary = build_summary(log, "20260305T143200", "2026-03-05T14:32:00");
        assert!(summary.contains("## State Changes"));
        assert!(summary.contains("work.wi-1: Draft -> InProgress"));
        assert!(summary.contains("work.wi-1: InProgress -> Review"));
    }

    #[test]
    fn test_build_summary_with_tool_calls() {
        let log = "\
[2026-03-05 14:32:01 DEBUG loopr] [agent:ag01] tool_result: tool=shell is_error=false exit=0 duration=100ms content_len=50
[2026-03-05 14:32:02 DEBUG loopr] [agent:ag01] tool_result: tool=read is_error=true exit=1 duration=5ms content_len=30
[2026-03-05 14:32:03 DEBUG loopr] [agent:ag01] tool_result: tool=write is_error=false exit=0 duration=10ms content_len=20
";
        let summary = build_summary(log, "20260305T143200", "2026-03-05T14:32:00");
        assert!(summary.contains("3 total"));
        assert!(summary.contains("2 success"));
        assert!(summary.contains("1 failed"));
    }

    #[test]
    fn test_build_summary_with_errors() {
        let log = "\
[2026-03-05 14:32:01 ERROR loopr] something went wrong
[2026-03-05 14:32:02 DEBUG loopr] [transition] plan.p1: Draft -> Active by Coordinator
[2026-03-05 14:32:03 ERROR loopr] another error occurred
";
        let summary = build_summary(log, "20260305T143200", "2026-03-05T14:32:00");
        assert!(summary.contains("## Errors"));
        assert!(summary.contains("something went wrong"));
        assert!(summary.contains("another error occurred"));
    }

    #[test]
    fn test_build_summary_with_agent_statuses() {
        let log = "\
[2026-03-05 14:32:01 DEBUG loopr] [agent_status] ag01: -> Starting (type=Coordinator)
[2026-03-05 14:32:02 DEBUG loopr] [agent_status] ag01: -> Running
";
        let summary = build_summary(log, "20260305T143200", "2026-03-05T14:32:00");
        assert!(summary.contains("## Agent Status Changes"));
        assert!(summary.contains("ag01: -> Starting"));
        assert!(summary.contains("ag01: -> Running"));
    }

    #[test]
    fn test_extract_after() {
        let line = "[2026 DEBUG loopr] [transition] work.wi-1: Draft -> Active";
        assert_eq!(extract_after(line, "[transition]"), "work.wi-1: Draft -> Active");
    }
}
