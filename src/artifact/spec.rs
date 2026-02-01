//! Spec artifact parsing.
//!
//! This module extracts PhaseDescriptor entries from spec.md artifacts.

use crate::error::Result;

use super::parser::extract_section;

/// Descriptor for a phase extracted from a spec.
///
/// Specs contain a "## Phases" section with numbered phases like:
/// 1. **Setup**: Initialize the project structure
/// 2. **Implementation**: Build core functionality
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseDescriptor {
    /// Phase number (1-based)
    pub number: u32,
    /// Name of the phase
    pub name: String,
    /// Description of what the phase covers
    pub description: String,
    /// Files to be created/modified in this phase
    pub files: Vec<String>,
}

impl PhaseDescriptor {
    /// Create a new phase descriptor.
    pub fn new(number: u32, name: impl Into<String>, description: impl Into<String>, files: Vec<String>) -> Self {
        Self {
            number,
            name: name.into(),
            description: description.into(),
            files,
        }
    }
}

/// Parse phases from a spec.md artifact.
///
/// Looks for a "## Phases" section and parses numbered items in the format:
/// 1. **Phase Name**: Description
///
/// Or with description on next line:
/// 1. **Phase Name**
///    Description on next line
///
/// Files are extracted from lines starting with "- " after the phase header
/// until the next numbered item or section.
///
/// Returns an empty Vec if no phases are found (but section exists).
/// Returns an error if the section is missing.
pub fn parse_spec_phases(content: &str) -> Result<Vec<PhaseDescriptor>> {
    let section = extract_section(content, "Phases")?;

    let mut phases = Vec::new();
    let mut current_phase: Option<(u32, String, String, Vec<String>)> = None;
    let mut in_files_block = false;
    let mut phase_counter = 0u32;

    for line in section.lines() {
        let trimmed = line.trim();

        // Check for numbered phase: "1. **Name**: Description" or "1. **Name**"
        if let Some((line_number, rest)) = try_parse_numbered_item_with_number(trimmed) {
            // Save previous phase if exists
            if let Some((num, name, desc, files)) = current_phase.take() {
                phases.push(PhaseDescriptor::new(num, name, desc, files));
            }

            // Parse the new phase
            if let Some((_, name, description)) = parse_phase_header(rest) {
                // Use the actual number from the line if available, otherwise use counter
                phase_counter += 1;
                let number = if line_number > 0 { line_number } else { phase_counter };
                current_phase = Some((number, name, description, Vec::new()));
                in_files_block = false;
            }
        } else if let Some(ref mut phase) = current_phase {
            // Look for file list items
            if let Some(file) = trimmed.strip_prefix("- ") {
                let file = file.trim();
                // Check if this looks like a file path (contains / or .)
                if (file.contains('/') || file.contains('.')) && !file.contains(':') {
                    phase.3.push(file.to_string());
                    in_files_block = true;
                }
            } else if trimmed.starts_with("Files:") || trimmed.starts_with("**Files:**") {
                in_files_block = true;
            } else if !in_files_block && !trimmed.is_empty() && phase.2.is_empty() {
                // If no description yet and this is non-empty text, use as description
                phase.2 = trimmed.to_string();
            }
        }
    }

    // Don't forget the last phase
    if let Some((num, name, desc, files)) = current_phase {
        phases.push(PhaseDescriptor::new(num, name, desc, files));
    }

    Ok(phases)
}

/// Try to parse a numbered list item, returning the number and rest (e.g., "1. rest" -> (1, "rest"))
fn try_parse_numbered_item_with_number(line: &str) -> Option<(u32, &str)> {
    let bytes = line.as_bytes();
    let mut i = 0;

    // Must start with digits
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }

    if i == 0 {
        return None;
    }

    // Must be followed by ". "
    if i + 2 > bytes.len() || bytes[i] != b'.' || bytes[i + 1] != b' ' {
        return None;
    }

    // Parse the number
    let number: u32 = line[..i].parse().ok()?;

    // Return the number and rest of the line
    Some((number, &line[i + 2..]))
}

/// Parse a phase header like "**Phase Name**: Description" or "**Phase Name**"
fn parse_phase_header(text: &str) -> Option<(u32, String, String)> {
    // Extract the bold name: **Name**
    let text = text.trim();
    if !text.starts_with("**") {
        // Try without bold markers
        if let Some((name, desc)) = text.split_once(':') {
            return Some((1, name.trim().to_string(), desc.trim().to_string()));
        }
        return None;
    }

    // Find closing **
    let after_open = &text[2..];
    let close_pos = after_open.find("**")?;
    let name = after_open[..close_pos].trim().to_string();

    if name.is_empty() {
        return None;
    }

    // Get description after **Name**
    let after_name = &after_open[close_pos + 2..].trim();
    let description = if let Some(desc) = after_name.strip_prefix(':') {
        desc.trim().to_string()
    } else {
        String::new()
    };

    // We don't have the actual number here, will be set by caller based on position
    // Actually, let's extract from the original parsing context - return 0 as placeholder
    Some((0, name, description))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test helper: Try to parse a numbered list item (e.g., "1. rest" or "2. rest")
    fn try_parse_numbered_item(line: &str) -> Option<&str> {
        try_parse_numbered_item_with_number(line).map(|(_, rest)| rest)
    }

    const SAMPLE_SPEC: &str = r#"# Authentication Spec

## Overview

This spec covers the authentication system.

## Phases

1. **Setup**: Initialize project structure and dependencies
   - src/lib.rs
   - Cargo.toml

2. **Core Types**: Define authentication types
   - src/auth/mod.rs
   - src/auth/types.rs

3. **Implementation**: Implement authentication logic
   - src/auth/service.rs
   - src/auth/middleware.rs

## Success Criteria

- All tests pass
"#;

    #[test]
    fn test_parse_spec_phases_basic() {
        let phases = parse_spec_phases(SAMPLE_SPEC).unwrap();
        assert_eq!(phases.len(), 3);

        assert_eq!(phases[0].name, "Setup");
        assert_eq!(phases[0].description, "Initialize project structure and dependencies");
        assert_eq!(phases[0].files, vec!["src/lib.rs", "Cargo.toml"]);

        assert_eq!(phases[1].name, "Core Types");
        assert_eq!(phases[1].description, "Define authentication types");
        assert_eq!(phases[1].files, vec!["src/auth/mod.rs", "src/auth/types.rs"]);

        assert_eq!(phases[2].name, "Implementation");
        assert_eq!(phases[2].description, "Implement authentication logic");
    }

    #[test]
    fn test_parse_spec_phases_empty_section() {
        let content = r#"# Spec

## Phases

## Next Section
"#;
        let phases = parse_spec_phases(content).unwrap();
        assert!(phases.is_empty());
    }

    #[test]
    fn test_parse_spec_phases_missing_section() {
        let content = r#"# Spec

## Overview

Just an overview.
"#;
        let result = parse_spec_phases(content);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_spec_phases_no_files() {
        let content = r#"# Spec

## Phases

1. **Planning**: Design the system

2. **Review**: Code review and feedback
"#;
        let phases = parse_spec_phases(content).unwrap();
        assert_eq!(phases.len(), 2);
        assert!(phases[0].files.is_empty());
        assert!(phases[1].files.is_empty());
    }

    #[test]
    fn test_parse_spec_phases_description_on_next_line() {
        let content = r#"# Spec

## Phases

1. **Setup**
   Initialize the project

   - src/main.rs
"#;
        let phases = parse_spec_phases(content).unwrap();
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0].name, "Setup");
        assert_eq!(phases[0].description, "Initialize the project");
        assert_eq!(phases[0].files, vec!["src/main.rs"]);
    }

    #[test]
    fn test_parse_spec_phases_files_block() {
        let content = r#"# Spec

## Phases

1. **Build**: Build the project

   Files:
   - src/build.rs
   - src/compile.rs
"#;
        let phases = parse_spec_phases(content).unwrap();
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0].files, vec!["src/build.rs", "src/compile.rs"]);
    }

    #[test]
    fn test_phase_descriptor_new() {
        let phase = PhaseDescriptor::new(1, "Test", "Test description", vec!["file.rs".to_string()]);
        assert_eq!(phase.number, 1);
        assert_eq!(phase.name, "Test");
        assert_eq!(phase.description, "Test description");
        assert_eq!(phase.files, vec!["file.rs"]);
    }

    #[test]
    fn test_phase_descriptor_equality() {
        let phase1 = PhaseDescriptor::new(1, "Setup", "Init", vec![]);
        let phase2 = PhaseDescriptor::new(1, "Setup", "Init", vec![]);
        let phase3 = PhaseDescriptor::new(2, "Setup", "Init", vec![]);

        assert_eq!(phase1, phase2);
        assert_ne!(phase1, phase3);
    }

    #[test]
    fn test_phase_descriptor_clone() {
        let phase = PhaseDescriptor::new(1, "Test", "Desc", vec!["a.rs".to_string()]);
        let cloned = phase.clone();
        assert_eq!(phase, cloned);
    }

    #[test]
    fn test_try_parse_numbered_item() {
        assert_eq!(try_parse_numbered_item("1. Hello"), Some("Hello"));
        assert_eq!(try_parse_numbered_item("12. World"), Some("World"));
        assert_eq!(try_parse_numbered_item("123. Test"), Some("Test"));
        assert_eq!(try_parse_numbered_item("- Hello"), None);
        assert_eq!(try_parse_numbered_item("1.Hello"), None); // No space
        assert_eq!(try_parse_numbered_item("Hello"), None);
    }

    #[test]
    fn test_parse_phase_header() {
        let (_, name, desc) = parse_phase_header("**Setup**: Initialize project").unwrap();
        assert_eq!(name, "Setup");
        assert_eq!(desc, "Initialize project");

        let (_, name, desc) = parse_phase_header("**Build**").unwrap();
        assert_eq!(name, "Build");
        assert_eq!(desc, "");

        assert!(parse_phase_header("****").is_none()); // Empty name
    }

    #[test]
    fn test_parse_spec_phases_mixed_content() {
        let content = r#"# Spec

## Phases

Some intro text.

1. **Phase One**: First phase
   - src/one.rs

Extra text between phases.

2. **Phase Two**: Second phase
   - src/two.rs
"#;
        let phases = parse_spec_phases(content).unwrap();
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[0].name, "Phase One");
        assert_eq!(phases[1].name, "Phase Two");
    }

    #[test]
    fn test_parse_spec_phases_non_file_list_items() {
        let content = r#"# Spec

## Phases

1. **Setup**: Initialize

   - src/lib.rs
   - Not a file: this is a note
   - Just a note
"#;
        let phases = parse_spec_phases(content).unwrap();
        assert_eq!(phases.len(), 1);
        // Only src/lib.rs should be in files (contains . or /)
        assert_eq!(phases[0].files, vec!["src/lib.rs"]);
    }
}
