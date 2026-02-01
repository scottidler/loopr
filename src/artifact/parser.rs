//! Common parsing utilities for artifact extraction.
//!
//! This module provides helpers for extracting markdown sections from artifacts.

use crate::error::{LooprError, Result};

/// Extract content between `## Heading` and the next `##` or end of file.
///
/// Returns the content between the heading and the next heading (or end of file),
/// excluding the heading line itself.
pub fn extract_section(content: &str, heading: &str) -> Result<String> {
    let target = format!("## {}", heading);

    // Find the start of the section
    let start_pos = content
        .find(&target)
        .ok_or_else(|| LooprError::ValidationFailed(format!("Missing required section: ## {}", heading)))?;

    // Skip past the heading line
    let after_heading = &content[start_pos + target.len()..];
    let content_start = after_heading
        .find('\n')
        .map(|pos| start_pos + target.len() + pos + 1)
        .unwrap_or(content.len());

    // Find the end (next ## heading or end of file)
    let remaining = &content[content_start..];
    let end_pos = remaining
        .find("\n## ")
        .map(|pos| content_start + pos)
        .unwrap_or(content.len());

    let section_content = content[content_start..end_pos].trim().to_string();
    Ok(section_content)
}

/// Check if a section exists in the content.
pub fn has_section(content: &str, heading: &str) -> bool {
    let target = format!("## {}", heading);
    content.contains(&target)
}

/// Extract all section headings from the content.
pub fn list_sections(content: &str) -> Vec<String> {
    let mut sections = Vec::new();
    for line in content.lines() {
        if line.starts_with("## ") {
            sections.push(line[3..].trim().to_string());
        }
    }
    sections
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_DOC: &str = r#"# Document Title

Some intro text.

## Overview

This is the overview section.
It has multiple lines.

## Phases

### Phase 1

First phase details.

### Phase 2

Second phase details.

## Summary

Final summary here.
"#;

    #[test]
    fn test_extract_section_overview() {
        let result = extract_section(SAMPLE_DOC, "Overview").unwrap();
        assert!(result.contains("This is the overview section."));
        assert!(result.contains("It has multiple lines."));
        assert!(!result.contains("## Phases"));
    }

    #[test]
    fn test_extract_section_phases() {
        let result = extract_section(SAMPLE_DOC, "Phases").unwrap();
        assert!(result.contains("### Phase 1"));
        assert!(result.contains("### Phase 2"));
        assert!(!result.contains("## Summary"));
    }

    #[test]
    fn test_extract_section_last() {
        let result = extract_section(SAMPLE_DOC, "Summary").unwrap();
        assert!(result.contains("Final summary here."));
    }

    #[test]
    fn test_extract_section_missing() {
        let result = extract_section(SAMPLE_DOC, "NonExistent");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Missing required section"));
    }

    #[test]
    fn test_has_section_true() {
        assert!(has_section(SAMPLE_DOC, "Overview"));
        assert!(has_section(SAMPLE_DOC, "Phases"));
        assert!(has_section(SAMPLE_DOC, "Summary"));
    }

    #[test]
    fn test_has_section_false() {
        assert!(!has_section(SAMPLE_DOC, "NonExistent"));
        assert!(!has_section(SAMPLE_DOC, "overview")); // Case sensitive
    }

    #[test]
    fn test_list_sections() {
        let sections = list_sections(SAMPLE_DOC);
        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0], "Overview");
        assert_eq!(sections[1], "Phases");
        assert_eq!(sections[2], "Summary");
    }

    #[test]
    fn test_list_sections_empty() {
        let sections = list_sections("# Just a title\n\nSome content.");
        assert!(sections.is_empty());
    }

    #[test]
    fn test_extract_section_empty_content() {
        let doc = "## Empty Section\n\n## Next Section\n\nContent here.";
        let result = extract_section(doc, "Empty Section").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_extract_section_with_trailing_whitespace() {
        let doc = "## Test   \n\n  Content with spaces  \n\n## Next";
        let result = extract_section(doc, "Test").unwrap();
        assert_eq!(result, "Content with spaces");
    }

    #[test]
    fn test_has_section_partial_match() {
        // Should not match partial heading names
        let doc = "## Overview\n\nContent\n## Overview Extended\n\nMore";
        assert!(has_section(doc, "Overview"));
        assert!(has_section(doc, "Overview Extended"));
    }
}
