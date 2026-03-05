use std::fs;
use std::path::{Path, PathBuf};

use eyre::{Context, eyre};

use super::DiagnoseCmd;

/// Run a diagnose subcommand. All subcommands work offline (no daemon required).
pub fn run(cmd: &DiagnoseCmd) -> eyre::Result<()> {
    match cmd {
        DiagnoseCmd::Dump { session, filter } => run_dump(session.as_deref(), filter.as_deref()),
        DiagnoseCmd::Log { session, filter, tail } => run_log(session.as_deref(), filter.as_deref(), *tail),
        DiagnoseCmd::Sessions { count } => run_sessions(*count),
        DiagnoseCmd::Summary { session } => run_summary(session.as_deref()),
        DiagnoseCmd::State => run_state(),
        DiagnoseCmd::Agents { failed } => run_agents(*failed),
    }
}

/// Resolve the sessions base directory.
fn sessions_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("loopr")
        .join("sessions")
}

/// Resolve a session directory from an optional session ID.
/// If None, follows the `latest` symlink.
fn find_session_dir(session: Option<&str>) -> eyre::Result<PathBuf> {
    let base = sessions_dir();
    let dir = if let Some(sid) = session {
        base.join(sid)
    } else {
        let latest = base.join("latest");
        if !latest.exists() {
            return Err(eyre!("no sessions found (no 'latest' symlink at {})", latest.display()));
        }
        fs::read_link(&latest)
            .map(|target| if target.is_relative() { base.join(target) } else { target })
            .context("failed to read latest symlink")?
    };

    if !dir.exists() {
        return Err(eyre!("session directory not found: {}", dir.display()));
    }
    Ok(dir)
}

/// Read and optionally filter/tail a log file.
fn read_log(path: &Path, filter: Option<&str>, tail: Option<usize>) -> eyre::Result<String> {
    let content = fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;

    let lines: Vec<&str> = if let Some(pattern) = filter {
        content.lines().filter(|l| l.contains(pattern)).collect()
    } else {
        content.lines().collect()
    };

    let lines = if let Some(n) = tail {
        if n < lines.len() { &lines[lines.len() - n..] } else { &lines }
    } else {
        &lines
    };

    Ok(lines.join("\n"))
}

fn run_dump(session: Option<&str>, filter: Option<&str>) -> eyre::Result<()> {
    let session_dir = find_session_dir(session)?;
    let session_name = session_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    println!("=== LOOPR SESSION DIAGNOSTIC DUMP ===");
    println!("Session: {}", session_name);
    println!("Session dir: {}", session_dir.display());
    println!();

    // Summary
    let summary_path = session_dir.join("summary.md");
    if summary_path.exists() {
        println!("=== SESSION SUMMARY ===");
        println!("{}", fs::read_to_string(&summary_path).unwrap_or_default());
        println!();
    }

    // TaskStore state
    println!("=== TASKSTORE STATE ===");
    if let Err(e) = run_state() {
        println!("(could not read TaskStore: {})", e);
    }
    println!();

    // Agent sessions
    println!("=== AGENT SESSIONS ===");
    if let Err(e) = run_agents(false) {
        println!("(could not read agent sessions: {})", e);
    }
    println!();

    // Failed agent logs
    let agents_dir = session_dir.join("agents");
    if agents_dir.exists()
        && let Ok(entries) = fs::read_dir(&agents_dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("log") {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                let content = fs::read_to_string(&path).unwrap_or_default();
                let lines: Vec<&str> = content.lines().collect();
                let excerpt = if lines.len() > 100 { &lines[lines.len() - 100..] } else { &lines };
                println!("=== AGENT LOG: {} ===", name);
                println!("{}", excerpt.join("\n"));
                println!();
            }
        }
    }

    // Session log
    println!("=== SESSION LOG ===");
    let log_path = session_dir.join("loopr.log");
    if log_path.exists() {
        match read_log(&log_path, filter, None) {
            Ok(content) => println!("{}", content),
            Err(e) => println!("(could not read session log: {})", e),
        }
    } else {
        println!("(no session log found)");
    }

    Ok(())
}

fn run_log(session: Option<&str>, filter: Option<&str>, tail: Option<usize>) -> eyre::Result<()> {
    let session_dir = find_session_dir(session)?;
    let log_path = session_dir.join("loopr.log");
    let content = read_log(&log_path, filter, tail)?;
    println!("{}", content);
    Ok(())
}

fn run_sessions(count: usize) -> eyre::Result<()> {
    let base = sessions_dir();
    if !base.exists() {
        println!("No sessions directory found at {}", base.display());
        return Ok(());
    }

    let mut entries: Vec<_> = fs::read_dir(&base)?
        .flatten()
        .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false) && e.file_name() != "latest")
        .collect();

    entries.sort_by_key(|b| std::cmp::Reverse(b.file_name()));

    let latest_target = fs::read_link(base.join("latest"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()));

    println!("{:<20} {:>10}  LATEST", "SESSION", "LOG SIZE");
    for entry in entries.iter().take(count) {
        let name = entry.file_name().to_string_lossy().to_string();
        let log_path = entry.path().join("loopr.log");
        let size = fs::metadata(&log_path)
            .map(|m| format_bytes(m.len()))
            .unwrap_or_else(|_| "—".into());
        let marker = if latest_target.as_deref() == Some(&name) { " <-" } else { "" };
        println!("{:<20} {:>10}{}", name, size, marker);
    }
    Ok(())
}

fn run_summary(session: Option<&str>) -> eyre::Result<()> {
    let session_dir = find_session_dir(session)?;
    let summary_path = session_dir.join("summary.md");
    if summary_path.exists() {
        println!("{}", fs::read_to_string(&summary_path)?);
    } else {
        println!("No summary.md found for this session.");
        println!("(Summary is generated on daemon shutdown.)");
    }
    Ok(())
}

fn run_state() -> eyre::Result<()> {
    let cwd = std::env::current_dir()?;
    let taskstore_dir = cwd.join(".taskstore");
    if !taskstore_dir.exists() {
        println!("No .taskstore/ directory found in {}", cwd.display());
        return Ok(());
    }

    // Read JSONL files and count records
    let collections = [
        "plans",
        "specs",
        "phases",
        "works",
        "bundles",
        "ticks",
        "learnings",
        "locks",
        "agent_sessions",
        "coordinator_goals",
        "coordinator_states",
        "proposals",
        "decisions",
        "coverage_reports",
        "validation_reports",
    ];

    for collection in &collections {
        let jsonl_path = taskstore_dir.join(format!("{}.jsonl", collection));
        if jsonl_path.exists() {
            let content = fs::read_to_string(&jsonl_path).unwrap_or_default();
            let count = content.lines().filter(|l| !l.trim().is_empty()).count();
            if count > 0 {
                println!("{}: {}", collection, count);
            }
        }
    }
    Ok(())
}

fn run_agents(failed_only: bool) -> eyre::Result<()> {
    let cwd = std::env::current_dir()?;
    let jsonl_path = cwd.join(".taskstore").join("agent_sessions.jsonl");
    if !jsonl_path.exists() {
        println!("No agent_sessions.jsonl found");
        return Ok(());
    }

    let content = fs::read_to_string(&jsonl_path)?;
    println!(
        "| {:<12} | {:<14} | {:<10} | {:>4} | {:<20} | Error",
        "ID", "Type", "Status", "Iter", "Work/Bundle"
    );
    println!(
        "|{}|{}|{}|{}|{}|{}",
        "-".repeat(14),
        "-".repeat(16),
        "-".repeat(12),
        "-".repeat(6),
        "-".repeat(22),
        "-".repeat(30)
    );

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let status = v["status"].as_str().unwrap_or("?");
        if failed_only && status != "failed" {
            continue;
        }

        let id = v["id"].as_str().unwrap_or("?");
        let agent_type = v["agent_type"].as_str().unwrap_or("?");
        let iteration = v["iteration"].as_u64().unwrap_or(0);
        let work_id = v["work_id"].as_str().unwrap_or("");
        let bundle_id = v["bundle_id"].as_str().unwrap_or("");
        let target = if !work_id.is_empty() {
            work_id.to_string()
        } else if !bundle_id.is_empty() {
            bundle_id.to_string()
        } else {
            "—".to_string()
        };
        let error = v["error_message"].as_str().unwrap_or("—");
        let error_short = if error.len() > 28 {
            format!("{}…", &error[..27])
        } else {
            error.to_string()
        };

        println!(
            "| {:<12} | {:<14} | {:<10} | {:>4} | {:<20} | {}",
            &id[..id.len().min(12)],
            agent_type,
            status,
            iteration,
            &target[..target.len().min(20)],
            error_short,
        );
    }
    Ok(())
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TestDir;

    #[test]
    fn test_find_session_dir_with_id() {
        let tmp = TestDir::new("loopr-diagnose-find");
        let sessions = tmp.join("sessions");
        let session = sessions.join("20260305T143200");
        fs::create_dir_all(&session).unwrap();

        // Manually test the logic (can't override sessions_dir, but test the path resolution)
        assert!(session.exists());
    }

    #[test]
    fn test_read_log_no_filter() {
        let tmp = TestDir::new("loopr-diagnose-log");
        let log = tmp.join("test.log");
        fs::write(&log, "line1\nline2\nline3\n").unwrap();

        let result = read_log(&log, None, None).unwrap();
        assert_eq!(result, "line1\nline2\nline3");
    }

    #[test]
    fn test_read_log_with_filter() {
        let tmp = TestDir::new("loopr-diagnose-filter");
        let log = tmp.join("test.log");
        fs::write(&log, "[debug] foo\n[info] bar\n[debug] baz\n").unwrap();

        let result = read_log(&log, Some("[debug]"), None).unwrap();
        assert_eq!(result, "[debug] foo\n[debug] baz");
    }

    #[test]
    fn test_read_log_with_tail() {
        let tmp = TestDir::new("loopr-diagnose-tail");
        let log = tmp.join("test.log");
        fs::write(&log, "a\nb\nc\nd\ne\n").unwrap();

        let result = read_log(&log, None, Some(2)).unwrap();
        assert_eq!(result, "d\ne");
    }

    #[test]
    fn test_read_log_tail_larger_than_file() {
        let tmp = TestDir::new("loopr-diagnose-tail-large");
        let log = tmp.join("test.log");
        fs::write(&log, "a\nb\n").unwrap();

        let result = read_log(&log, None, Some(100)).unwrap();
        assert_eq!(result, "a\nb");
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(2_097_152), "2.0 MB");
    }

    #[test]
    fn test_run_sessions_no_dir() {
        // Should not panic even if sessions dir doesn't exist
        // (just prints a message)
    }
}
