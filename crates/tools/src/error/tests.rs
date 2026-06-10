use super::*;

#[test]
fn display_unknown_tool() {
    let e = ToolError::UnknownTool("foo".into());
    assert_eq!(format!("{e}"), "unknown tool: foo");
}

#[test]
fn display_bash_denied() {
    let e = ToolError::BashDenied {
        reason: "deletes root filesystem".into(),
    };
    assert_eq!(
        format!("{e}"),
        "bash command rejected by denylist: deletes root filesystem"
    );
}

#[test]
fn display_lane_closed() {
    let e = ToolError::LaneClosed(Lane::Heavy);
    assert_eq!(format!("{e}"), "lane semaphore closed: Heavy");
}

#[test]
fn io_conversion() {
    let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
    let e: ToolError = io.into();
    assert!(matches!(e, ToolError::Io(_)));
}
