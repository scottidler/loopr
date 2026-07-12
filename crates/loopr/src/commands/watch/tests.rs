#![allow(clippy::unwrap_used)]

use ipc::{DaemonEvent, WatchFrame};

use super::render_watch_line;

fn work_event(plan_id: &str, work_id: &str, status: &str) -> WatchFrame {
    WatchFrame::Event(DaemonEvent {
        event: "work.terminal".into(),
        data: serde_json::json!({ "work_id": work_id, "plan_id": plan_id, "status": status }),
    })
}

#[test]
fn heartbeat_renders_nothing() {
    assert_eq!(render_watch_line(&WatchFrame::Heartbeat, None), None);
}

#[test]
fn gap_renders_visible_marker_with_count() {
    let line = render_watch_line(&WatchFrame::Gap { dropped: 12 }, None).expect("gap must render a visible line");
    assert!(line.contains("gap"), "gap marker must be visible: {line}");
    assert!(line.contains("12"), "gap marker must carry the dropped count: {line}");
}

#[test]
fn gap_is_never_filtered_by_plan() {
    // A gap is a stream-wide fact; a --plan filter must NOT suppress it.
    let line = render_watch_line(&WatchFrame::Gap { dropped: 1 }, Some("pl-other"));
    assert!(line.is_some(), "gap marker must survive a plan filter");
}

#[test]
fn event_renders_one_line_with_name_and_data() {
    let frame = work_event("pl-abc12", "wk-abc12", "Done");
    let line = render_watch_line(&frame, None).unwrap();
    assert!(line.starts_with("work.terminal"), "line leads with event name: {line}");
    assert!(line.contains("wk-abc12"), "line carries the work id: {line}");
    // One line only — no embedded newlines.
    assert!(!line.contains('\n'), "event must render as exactly one line: {line}");
}

#[test]
fn plan_filter_keeps_matching_event() {
    let frame = work_event("pl-abc12", "wk-abc12", "Done");
    assert!(render_watch_line(&frame, Some("pl-abc12")).is_some());
}

#[test]
fn plan_filter_drops_nonmatching_event() {
    let frame = work_event("pl-abc12", "wk-abc12", "Done");
    assert_eq!(render_watch_line(&frame, Some("pl-zzz99")), None);
}

#[test]
fn plan_filter_passes_event_without_plan_scope() {
    // budget.exceeded has no plan_id — a process-wide event must still show
    // under a --plan filter (it affects every Plan).
    let frame = WatchFrame::Event(DaemonEvent {
        event: "budget.exceeded".into(),
        data: serde_json::json!({ "scope": "per-run", "cost_usd": 1.0, "cap_usd": 0.5 }),
    });
    assert!(
        render_watch_line(&frame, Some("pl-abc12")).is_some(),
        "process-wide budget event must pass a plan filter"
    );
}

#[test]
fn full_plan_lifecycle_renders_in_order() {
    // The renderer is order-preserving: a lifecycle sequence maps 1:1 onto
    // output lines in the same order.
    let frames = [
        work_event("pl-abc12", "wk-00001", "Done"),
        work_event("pl-abc12", "wk-00002", "Done"),
        WatchFrame::Event(DaemonEvent {
            event: "plan.terminal".into(),
            data: serde_json::json!({ "plan_id": "pl-abc12", "status": "Complete" }),
        }),
    ];
    let lines: Vec<String> = frames
        .iter()
        .filter_map(|f| render_watch_line(f, Some("pl-abc12")))
        .collect();
    assert_eq!(lines.len(), 3);
    assert!(lines[0].contains("wk-00001"));
    assert!(lines[1].contains("wk-00002"));
    assert!(lines[2].starts_with("plan.terminal"));
}
