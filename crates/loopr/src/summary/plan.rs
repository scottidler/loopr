//! Plan summary renderer + atomic writer.
//!
//! Output path: `<target>/.loopr/records/plans/<plan-id>/summary.md`.

use std::fmt::Write;
use std::io;
use std::path::Path;

use domain::{Plan, PlanStatus, Work};

use crate::summary::{atomic_write, records_root};

/// Render a Plan's summary markdown. Pure: no I/O.
///
/// `children` are the Works belonging to this plan (caller fetches via
/// `WorksStore::list_by_parent_id`). When the slice is empty, the children
/// section is omitted; the plan summary is still useful at decomposition
/// time before child Works land.
pub fn render_plan(plan: &Plan, children: &[Work]) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# Plan {}", plan.id);
    s.push('\n');
    let _ = writeln!(s, "**Status:** {}", status_str(plan.status));
    let _ = writeln!(s, "**Children:** {}", children.len());
    s.push('\n');

    s.push_str("## Goal\n");
    s.push_str(&plan.goal);
    s.push('\n');
    s.push('\n');

    if !children.is_empty() {
        s.push_str("## Children\n");
        s.push_str("| Work | Status | Title |\n");
        s.push_str("| --- | --- | --- |\n");
        for w in children {
            let _ = writeln!(
                s,
                "| [{}](../../works/{}/summary.md) | {} | {} |",
                w.id,
                w.id,
                work_status_str(w.status),
                w.title
            );
        }
        s.push('\n');
    }

    s
}

fn status_str(status: PlanStatus) -> &'static str {
    match status {
        PlanStatus::Draft => "Draft",
        PlanStatus::Pending => "Pending",
        PlanStatus::Active => "Active",
        PlanStatus::Complete => "Complete",
        PlanStatus::Superseded => "Superseded",
        PlanStatus::Abandoned => "Abandoned",
    }
}

fn work_status_str(status: domain::WorkStatus) -> &'static str {
    match status {
        domain::WorkStatus::Draft => "Draft",
        domain::WorkStatus::Pending => "Pending",
        domain::WorkStatus::Ready => "Ready",
        domain::WorkStatus::InProgress => "InProgress",
        domain::WorkStatus::Blocked => "Blocked",
        domain::WorkStatus::InReview => "InReview",
        domain::WorkStatus::Integrated => "Integrated",
        domain::WorkStatus::Done => "Done",
        domain::WorkStatus::Superseded => "Superseded",
        domain::WorkStatus::Abandoned => "Abandoned",
    }
}

/// Atomically write the rendered summary to disk under the target.
pub fn write_plan(target: &Path, plan: &Plan, children: &[Work]) -> io::Result<()> {
    let path = records_root(target)
        .join("plans")
        .join(plan.id.as_ref())
        .join("summary.md");
    atomic_write(&path, &render_plan(plan, children))
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{AcceptanceCriteria, PlanId};
    use std::str::FromStr;

    fn sample_plan() -> Plan {
        let mut p = Plan::new("ship X".to_string());
        p.id = PlanId::from_str("pl-abc").unwrap();
        p.status = PlanStatus::Active;
        p
    }

    fn sample_work(parent: &PlanId, id: &str, status: domain::WorkStatus) -> Work {
        let mut w = Work::new(parent.clone(), format!("title {id}"));
        w.id = domain::WorkId::from_str(id).unwrap();
        w.status = status;
        w.acceptance_criteria = AcceptanceCriteria(vec!["x".to_string()]);
        w
    }

    #[test]
    fn render_plan_no_children_omits_table() {
        let p = sample_plan();
        let out = render_plan(&p, &[]);
        assert!(out.contains("# Plan pl-abc"));
        assert!(out.contains("**Status:** Active"));
        assert!(out.contains("**Children:** 0"));
        assert!(out.contains("## Goal\nship X"));
        assert!(!out.contains("## Children"));
    }

    #[test]
    fn render_plan_with_children_renders_table() {
        let p = sample_plan();
        let children = vec![
            sample_work(&p.id, "wk-1", domain::WorkStatus::Done),
            sample_work(&p.id, "wk-2", domain::WorkStatus::Blocked),
        ];
        let out = render_plan(&p, &children);
        assert!(out.contains("**Children:** 2"));
        assert!(out.contains("## Children"));
        assert!(out.contains("| [wk-1](../../works/wk-1/summary.md) | Done | title wk-1 |"));
        assert!(out.contains("| [wk-2](../../works/wk-2/summary.md) | Blocked | title wk-2 |"));
    }

    #[test]
    fn write_plan_creates_file() {
        let td = tempfile::TempDir::new().unwrap();
        let p = sample_plan();
        write_plan(td.path(), &p, &[]).unwrap();
        let path = td.path().join(".loopr/records/plans/pl-abc/summary.md");
        assert!(path.is_file());
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("# Plan pl-abc"));
    }
}
