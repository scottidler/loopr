use std::path::Path;

use log::debug;

use crate::config::ToolEntry;

const TOOL_TEST_TIMEOUT_SECS: u64 = 300;
const TOOL_LINT_TIMEOUT_SECS: u64 = 120;
const TOOL_FORMAT_TIMEOUT_SECS: u64 = 30;

/// Built-in tool presets for JavaScript projects.
fn js_preset() -> Vec<ToolEntry> {
    vec![
        ToolEntry {
            name: "test".into(),
            command: "npm test".into(),
            timeout_secs: TOOL_TEST_TIMEOUT_SECS,
            worktree: true,
        },
        ToolEntry {
            name: "lint".into(),
            command: "npm run lint".into(),
            timeout_secs: TOOL_LINT_TIMEOUT_SECS,
            worktree: true,
        },
        ToolEntry {
            name: "build".into(),
            command: "npm run build".into(),
            timeout_secs: TOOL_TEST_TIMEOUT_SECS,
            worktree: true,
        },
    ]
}

/// Built-in tool presets for Python projects.
fn python_preset() -> Vec<ToolEntry> {
    vec![
        ToolEntry {
            name: "test".into(),
            command: "pytest".into(),
            timeout_secs: TOOL_TEST_TIMEOUT_SECS,
            worktree: true,
        },
        ToolEntry {
            name: "lint".into(),
            command: "ruff check .".into(),
            timeout_secs: TOOL_LINT_TIMEOUT_SECS,
            worktree: true,
        },
        ToolEntry {
            name: "fmt-check".into(),
            command: "ruff format --check .".into(),
            timeout_secs: TOOL_FORMAT_TIMEOUT_SECS,
            worktree: true,
        },
    ]
}

/// Marker files checked in priority order. First match wins.
const MARKER_ORDER: &[&str] = &["package.json", "pyproject.toml", "Cargo.toml"];

/// Detect project type from marker files and return appropriate tool entries.
/// Returns empty if no markers found - fallback logic is in `resolve_tools()`.
pub fn detect_project_tools(worktree: &Path) -> Vec<ToolEntry> {
    for marker in MARKER_ORDER {
        if worktree.join(marker).exists() {
            let tools = match *marker {
                "package.json" => js_preset(),
                "pyproject.toml" => python_preset(),
                "Cargo.toml" => return Vec::new(),
                _ => continue,
            };
            debug!("Detected project marker '{}', using {} tools", marker, tools.len());
            return tools;
        }
    }
    Vec::new()
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TestDir;

    #[test]
    fn test_detect_js_project() {
        let dir = TestDir::new("loopr-detect-js");
        std::fs::write(dir.join("package.json"), "{}").unwrap();

        let tools = detect_project_tools(&dir);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"test"));
        assert!(names.contains(&"lint"));
        assert!(names.contains(&"build"));
        assert!(tools.iter().any(|t| t.command.contains("npm")));
    }

    #[test]
    fn test_detect_python_project() {
        let dir = TestDir::new("loopr-detect-py");
        std::fs::write(dir.join("pyproject.toml"), "[project]").unwrap();

        let tools = detect_project_tools(&dir);
        assert!(tools.iter().any(|t| t.command.contains("pytest")));
    }

    #[test]
    fn test_detect_rust_project_returns_empty() {
        let dir = TestDir::new("loopr-detect-rs");
        std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();

        let tools = detect_project_tools(&dir);
        assert!(
            tools.is_empty(),
            "Cargo.toml detection should return empty, not fallback"
        );
    }

    #[test]
    fn test_detect_no_markers_returns_empty() {
        let dir = TestDir::new("loopr-detect-none");

        let tools = detect_project_tools(&dir);
        assert!(tools.is_empty(), "No markers should return empty");
    }

    #[test]
    fn test_detect_priority_order_js_over_python() {
        let dir = TestDir::new("loopr-detect-priority");
        std::fs::write(dir.join("package.json"), "{}").unwrap();
        std::fs::write(dir.join("pyproject.toml"), "[project]").unwrap();

        let tools = detect_project_tools(&dir);
        assert!(tools.iter().any(|t| t.command.contains("npm")));
    }
}
