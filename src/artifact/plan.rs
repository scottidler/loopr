//! Plan artifact parsing.
//!
//! This module extracts SpecDescriptor entries from plan.md artifacts.

use crate::error::Result;

use super::parser::extract_section;

/// Descriptor for a spec extracted from a plan.
///
/// Plans contain a "## Specs to Create" section with list items like:
/// - spec-auth: Authentication and authorization system
/// - spec-api: REST API endpoints
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecDescriptor {
    /// The name of the spec (without "spec-" prefix)
    pub name: String,
    /// Description of what the spec covers
    pub description: String,
    /// Index (0-based) of this spec in the list
    pub index: u32,
}

impl SpecDescriptor {
    /// Create a new spec descriptor.
    pub fn new(name: impl Into<String>, description: impl Into<String>, index: u32) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            index,
        }
    }
}

/// Parse specs from a plan.md artifact.
///
/// Looks for a "## Specs to Create" section and parses list items in the format:
/// - spec-\<name\>: \<description\>
///
/// Or without the `spec-` prefix:
/// - \<name\>: \<description\>
///
/// Returns an empty Vec if no specs are found (but section exists).
/// Returns an error if the section is missing.
pub fn parse_plan_specs(content: &str) -> Result<Vec<SpecDescriptor>> {
    let section = extract_section(content, "Specs to Create")?;

    let mut specs = Vec::new();
    let mut index = 0u32;

    for line in section.lines() {
        let trimmed = line.trim();

        // Look for list items: "- spec-name: description" or "- name: description"
        if let Some(item) = trimmed.strip_prefix("- ")
            && let Some((name_part, description)) = item.split_once(':')
        {
            let name = name_part
                .trim()
                .strip_prefix("spec-")
                .unwrap_or(name_part.trim())
                .to_string();

            if !name.is_empty() {
                specs.push(SpecDescriptor::new(name, description.trim(), index));
                index += 1;
            }
        }
    }

    Ok(specs)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_PLAN: &str = r#"# Implementation Plan

## Overview

This plan describes the implementation of a user authentication system.

## Specs to Create

- spec-auth: Authentication and authorization system
- spec-api: REST API endpoints for user management
- spec-db: Database schema and migrations

## Success Criteria

- All tests pass
- Documentation complete
"#;

    #[test]
    fn test_parse_plan_specs_basic() {
        let specs = parse_plan_specs(SAMPLE_PLAN).unwrap();
        assert_eq!(specs.len(), 3);

        assert_eq!(specs[0].name, "auth");
        assert_eq!(specs[0].description, "Authentication and authorization system");
        assert_eq!(specs[0].index, 0);

        assert_eq!(specs[1].name, "api");
        assert_eq!(specs[1].description, "REST API endpoints for user management");
        assert_eq!(specs[1].index, 1);

        assert_eq!(specs[2].name, "db");
        assert_eq!(specs[2].description, "Database schema and migrations");
        assert_eq!(specs[2].index, 2);
    }

    #[test]
    fn test_parse_plan_specs_without_prefix() {
        let content = r#"# Plan

## Specs to Create

- auth: Authentication system
- api: API endpoints
"#;
        let specs = parse_plan_specs(content).unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "auth");
        assert_eq!(specs[1].name, "api");
    }

    #[test]
    fn test_parse_plan_specs_empty_section() {
        let content = r#"# Plan

## Specs to Create

## Next Section
"#;
        let specs = parse_plan_specs(content).unwrap();
        assert!(specs.is_empty());
    }

    #[test]
    fn test_parse_plan_specs_missing_section() {
        let content = r#"# Plan

## Overview

Just an overview.
"#;
        let result = parse_plan_specs(content);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_plan_specs_mixed_content() {
        let content = r#"# Plan

## Specs to Create

Some introductory text about specs.

- spec-core: Core functionality
- spec-ui: User interface components

Additional notes after the list.
"#;
        let specs = parse_plan_specs(content).unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "core");
        assert_eq!(specs[1].name, "ui");
    }

    #[test]
    fn test_parse_plan_specs_whitespace_handling() {
        let content = r#"# Plan

## Specs to Create

-   spec-auth:   Authentication
- spec-api:API endpoints
"#;
        let specs = parse_plan_specs(content).unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "auth");
        assert_eq!(specs[0].description, "Authentication");
        assert_eq!(specs[1].name, "api");
        assert_eq!(specs[1].description, "API endpoints");
    }

    #[test]
    fn test_parse_plan_specs_invalid_format() {
        let content = r#"# Plan

## Specs to Create

- spec without colon
- : no name
- spec-valid: This one is valid
"#;
        let specs = parse_plan_specs(content).unwrap();
        // Only the valid one should be parsed
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "valid");
    }

    #[test]
    fn test_spec_descriptor_new() {
        let spec = SpecDescriptor::new("test", "Test description", 5);
        assert_eq!(spec.name, "test");
        assert_eq!(spec.description, "Test description");
        assert_eq!(spec.index, 5);
    }

    #[test]
    fn test_spec_descriptor_equality() {
        let spec1 = SpecDescriptor::new("auth", "Auth system", 0);
        let spec2 = SpecDescriptor::new("auth", "Auth system", 0);
        let spec3 = SpecDescriptor::new("api", "Auth system", 0);

        assert_eq!(spec1, spec2);
        assert_ne!(spec1, spec3);
    }

    #[test]
    fn test_spec_descriptor_clone() {
        let spec = SpecDescriptor::new("test", "Description", 1);
        let cloned = spec.clone();
        assert_eq!(spec, cloned);
    }

    #[test]
    fn test_parse_plan_specs_numbered_list() {
        // Should not parse numbered lists
        let content = r#"# Plan

## Specs to Create

1. spec-auth: Authentication
2. spec-api: API endpoints
"#;
        let specs = parse_plan_specs(content).unwrap();
        assert!(specs.is_empty());
    }

    #[test]
    fn test_parse_plan_specs_nested_list() {
        let content = r#"# Plan

## Specs to Create

- spec-auth: Authentication
  - sub-item (ignored)
- spec-api: API endpoints
"#;
        let specs = parse_plan_specs(content).unwrap();
        assert_eq!(specs.len(), 2);
    }
}
