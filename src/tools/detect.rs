use std::path::Path;

use log::debug;

use crate::config::ToolEntry;

/// Built-in tool presets for JavaScript projects.
fn js_preset() -> Vec<ToolEntry> {
    vec![
        ToolEntry {
            name: "test".into(),
            command: "npm test".into(),
            timeout_secs: 300,
            worktree: true,
        },
        ToolEntry {
            name: "lint".into(),
            command: "npm run lint".into(),
            timeout_secs: 120,
            worktree: true,
        },
        ToolEntry {
            name: "build".into(),
            command: "npm run build".into(),
            timeout_secs: 300,
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
            timeout_secs: 300,
            worktree: true,
        },
        ToolEntry {
            name: "lint".into(),
            command: "ruff check .".into(),
            timeout_secs: 120,
            worktree: true,
        },
        ToolEntry {
            name: "fmt-check".into(),
            command: "ruff format --check .".into(),
            timeout_secs: 30,
            worktree: true,
        },
    ]
}

/// Marker files checked in priority order. First match wins.
const MARKER_ORDER: &[&str] = &["package.json", "pyproject.toml", "Cargo.toml"];

/// Detect project type from marker files and return appropriate tool entries.
/// Falls back to `configured` if no markers found or if Cargo.toml is detected.
pub fn detect_project_tools(worktree: &Path, configured: &[ToolEntry]) -> Vec<ToolEntry> {
    for marker in MARKER_ORDER {
        if worktree.join(marker).exists() {
            let tools = match *marker {
                "package.json" => js_preset(),
                "pyproject.toml" => python_preset(),
                "Cargo.toml" => return configured.to_vec(),
                _ => continue,
            };
            debug!("Detected project marker '{}', using {} tools", marker, tools.len());
            return tools;
        }
    }
    configured.to_vec()
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

        let tools = detect_project_tools(&dir, &[]);
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

        let tools = detect_project_tools(&dir, &[]);
        assert!(tools.iter().any(|t| t.command.contains("pytest")));
    }

    #[test]
    fn test_detect_rust_project_uses_config() {
        let dir = TestDir::new("loopr-detect-rs");
        std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();

        let configured = vec![ToolEntry {
            name: "custom-test".into(),
            command: "cargo test".into(),
            timeout_secs: 300,
            worktree: true,
        }];

        let tools = detect_project_tools(&dir, &configured);
        assert!(tools.iter().any(|t| t.name == "custom-test"));
    }

    #[test]
    fn test_detect_no_markers_uses_config() {
        let dir = TestDir::new("loopr-detect-none");

        let configured = vec![ToolEntry {
            name: "make-test".into(),
            command: "make test".into(),
            timeout_secs: 300,
            worktree: true,
        }];

        let tools = detect_project_tools(&dir, &configured);
        assert!(tools.iter().any(|t| t.name == "make-test"));
    }

    #[test]
    fn test_detect_priority_order_js_over_python() {
        let dir = TestDir::new("loopr-detect-priority");
        std::fs::write(dir.join("package.json"), "{}").unwrap();
        std::fs::write(dir.join("pyproject.toml"), "[project]").unwrap();

        let tools = detect_project_tools(&dir, &[]);
        assert!(tools.iter().any(|t| t.command.contains("npm")));
    }
}
