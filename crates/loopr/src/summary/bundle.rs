//! Bundle summary renderer + atomic writer.
//!
//! Output path: `<target>/.loopr/records/bundles/<bundle-id>/summary.md`.

use std::fmt::Write;
use std::io;
use std::path::Path;

use domain::{Bundle, BundleStatus};

use crate::summary::{atomic_write, records_root};

/// Render a Bundle's summary markdown. Pure: no I/O.
pub fn render_bundle(bundle: &Bundle) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# Bundle {}", bundle.id);
    s.push('\n');
    let _ = writeln!(
        s,
        "**Work:** [{}](../../works/{}/summary.md)",
        bundle.work_id, bundle.work_id
    );
    let _ = writeln!(s, "**Status:** {}", status_str(bundle.status));
    let _ = writeln!(s, "**Branch:** `{}`", bundle.branch_name);
    if let Some(sha) = &bundle.head_commit {
        let _ = writeln!(s, "**Head Commit:** `{sha}`");
    }
    if let Some(loc) = bundle.loc_changed {
        let _ = writeln!(s, "**Lines Changed:** {loc}");
    }
    if bundle.force_proposed {
        let _ = writeln!(s, "**Force-Proposed:** yes (iteration cap reached)");
    }
    s.push('\n');

    if !bundle.claims.is_empty() {
        s.push_str("## Claims\n");
        for claim in &bundle.claims {
            let _ = writeln!(s, "- {claim}");
        }
        s.push('\n');
    }

    if !bundle.paths.is_empty() {
        s.push_str("## Paths\n");
        for p in &bundle.paths {
            let _ = writeln!(s, "- `{p}`");
        }
        s.push('\n');
    }

    if let Some(reason) = &bundle.noop_reason {
        s.push_str("## Noop Reason\n");
        s.push_str(reason);
        s.push('\n');
        s.push('\n');
    }

    if !bundle.verification.is_empty() {
        s.push_str("## Verification\n");
        s.push_str(&bundle.verification);
        s.push('\n');
        s.push('\n');
    }

    s.push_str("## Raw\n");
    let _ = writeln!(s, "- review transcript: `review.md`");
    s
}

fn status_str(status: BundleStatus) -> &'static str {
    match status {
        BundleStatus::Proposed => "Proposed",
        BundleStatus::Triaged => "Triaged",
        BundleStatus::Reviewed => "Reviewed",
        BundleStatus::Accepted => "Accepted",
        BundleStatus::Rejected => "Rejected",
        BundleStatus::Integrating => "Integrating",
        BundleStatus::Merged => "Merged",
        BundleStatus::IntegrationFailed => "IntegrationFailed",
        BundleStatus::Superseded => "Superseded",
    }
}

/// Atomically write the rendered summary to disk under the target.
pub fn write_bundle(target: &Path, bundle: &Bundle) -> io::Result<()> {
    let path = records_root(target)
        .join("bundles")
        .join(bundle.id.as_ref())
        .join("summary.md");
    atomic_write(&path, &render_bundle(bundle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::WorkId;
    use std::str::FromStr;

    fn sample_bundle() -> Bundle {
        let mut b = Bundle::new(
            WorkId::from_str("wk-abc").unwrap(),
            "loopr/wk-abc-1".to_string(),
            vec!["feature added".to_string()],
        );
        b.id = domain::BundleId::from_str("bd-xyz").unwrap();
        b.head_commit = Some("0123abc".to_string());
        b.loc_changed = Some(42);
        b.paths = vec!["src/main.rs".to_string()];
        b.status = BundleStatus::Accepted;
        b
    }

    #[test]
    fn render_bundle_includes_required_sections() {
        let b = sample_bundle();
        let out = render_bundle(&b);
        assert!(out.contains("# Bundle bd-xyz"), "header: {out}");
        assert!(out.contains("**Status:** Accepted"));
        assert!(out.contains("**Branch:** `loopr/wk-abc-1`"));
        assert!(out.contains("**Head Commit:** `0123abc`"));
        assert!(out.contains("**Lines Changed:** 42"));
        assert!(out.contains("## Claims\n- feature added"));
        assert!(out.contains("## Paths\n- `src/main.rs`"));
        assert!(out.contains("## Raw"));
    }

    #[test]
    fn force_proposed_adds_marker_line() {
        let mut b = sample_bundle();
        b.force_proposed = true;
        let out = render_bundle(&b);
        assert!(out.contains("**Force-Proposed:** yes"));
    }

    #[test]
    fn noop_reason_renders_section() {
        let mut b = sample_bundle();
        b.noop_reason = Some("nothing to change".to_string());
        let out = render_bundle(&b);
        assert!(out.contains("## Noop Reason\nnothing to change"));
    }

    #[test]
    fn write_bundle_creates_file() {
        let td = tempfile::TempDir::new().unwrap();
        let b = sample_bundle();
        write_bundle(td.path(), &b).unwrap();
        let path = td.path().join(".loopr/records/bundles/bd-xyz/summary.md");
        assert!(path.is_file(), "summary written at {}", path.display());
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("# Bundle bd-xyz"));
    }
}
