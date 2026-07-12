//! Work summary renderer + atomic writer.
//!
//! Output path: `<target>/.loopr/records/works/<work-id>/summary.md`.

use std::fmt::Write;
use std::io;
use std::path::Path;

use domain::{Work, WorkStatus};

use crate::summary::{atomic_write, records_root};

/// Render a Work's summary markdown. Pure: no I/O.
pub fn render_work(work: &Work) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# Work {}", work.id);
    s.push('\n');
    let _ = writeln!(
        s,
        "**Plan:** [{}](../../plans/{}/summary.md)",
        work.parent_id, work.parent_id
    );
    let _ = writeln!(s, "**Status:** {}", status_str(work.status));
    let _ = writeln!(s, "**Attempts:** {}", work.attempt_count);
    s.push('\n');

    s.push_str("## Title\n");
    s.push_str(&work.title);
    s.push('\n');
    s.push('\n');

    s.push_str("## Acceptance Criteria\n");
    if work.acceptance_criteria.is_empty() {
        s.push_str("(none specified)\n");
    } else {
        for ac in work.acceptance_criteria.iter() {
            let _ = writeln!(s, "{}. {}", ac.id, ac.text);
        }
    }
    s.push('\n');

    if !work.dependencies.is_empty() {
        s.push_str("## Dependencies\n");
        for dep in &work.dependencies {
            let _ = writeln!(s, "- [{dep}](../{dep}/summary.md)");
        }
        s.push('\n');
    }

    s.push_str("## Raw\n");
    s.push_str("- transcript: `transcript.md`\n");
    s
}

fn status_str(status: WorkStatus) -> &'static str {
    match status {
        WorkStatus::Draft => "Draft",
        WorkStatus::Pending => "Pending",
        WorkStatus::Ready => "Ready",
        WorkStatus::InProgress => "InProgress",
        WorkStatus::Blocked => "Blocked",
        WorkStatus::InReview => "InReview",
        WorkStatus::Integrated => "Integrated",
        WorkStatus::Done => "Done",
        WorkStatus::Superseded => "Superseded",
        WorkStatus::Abandoned => "Abandoned",
    }
}

/// Atomically write the rendered summary to disk under the target.
pub fn write_work(target: &Path, work: &Work) -> io::Result<()> {
    let path = records_root(target)
        .join("works")
        .join(work.id.as_ref())
        .join("summary.md");
    atomic_write(&path, &render_work(work))
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{AcceptanceCriteria, PlanId};
    use std::str::FromStr;

    fn sample_work() -> Work {
        let mut w = Work::new(PlanId::from_str("pl-abc").unwrap(), "ship feature X".to_string());
        w.id = domain::WorkId::from_str("wk-xyz").unwrap();
        w.acceptance_criteria = AcceptanceCriteria::from_texts(vec!["X works".to_string(), "tests pass".to_string()]);
        w.attempt_count = 2;
        w.status = WorkStatus::Done;
        w
    }

    #[test]
    fn render_work_includes_required_sections() {
        let w = sample_work();
        let out = render_work(&w);
        assert!(out.contains("# Work wk-xyz"));
        assert!(out.contains("**Plan:** [pl-abc]"));
        assert!(out.contains("**Status:** Done"));
        assert!(out.contains("**Attempts:** 2"));
        assert!(out.contains("## Title\nship feature X"));
        assert!(out.contains("1. X works\n2. tests pass"));
    }

    #[test]
    fn write_work_creates_file() {
        let td = tempfile::TempDir::new().unwrap();
        let w = sample_work();
        write_work(td.path(), &w).unwrap();
        let path = td.path().join(".loopr/records/works/wk-xyz/summary.md");
        assert!(path.is_file());
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("# Work wk-xyz"));
    }
}
