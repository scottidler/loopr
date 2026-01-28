//! Artifact parsing for extracting child loop definitions.
//!
//! Artifacts (plan.md, spec.md, phase.md) are the connective tissue between
//! loop levels. This module parses these artifacts to extract definitions
//! for child loops.

use eyre::{Result, eyre};

/// Definition of a spec extracted from a plan.md artifact.
#[derive(Debug, Clone, PartialEq)]
pub struct SpecDefinition {
    /// Spec name (e.g., "Core Authentication")
    pub name: String,
    /// Brief description of the spec
    pub description: String,
    /// Scope items from the spec section
    pub scope: Vec<String>,
}

/// Definition of a phase extracted from a spec.md artifact.
#[derive(Debug, Clone, PartialEq)]
pub struct PhaseDefinition {
    /// Phase number (1-indexed)
    pub number: usize,
    /// Phase name (e.g., "User Model and Repository")
    pub name: String,
    /// One-sentence goal
    pub goal: String,
    /// List of tasks to complete
    pub tasks: Vec<String>,
    /// Validation method/command
    pub validation: String,
}

/// Minimum content thresholds (characters, excluding whitespace and headers)
const MIN_PLAN_CONTENT: usize = 200;
const MIN_SPEC_CONTENT: usize = 300;
const MIN_PHASE_CONTENT: usize = 100;

/// Parse specs from a plan.md artifact.
///
/// Specs are defined in the "## Specs" section with headers like:
/// "### Spec 1: Core Authentication"
pub fn parse_specs_from_plan(plan_content: &str) -> Vec<SpecDefinition> {
    let mut specs = Vec::new();
    let mut current_spec: Option<SpecDefinition> = None;
    let mut in_specs_section = false;
    let mut in_scope = false;

    for line in plan_content.lines() {
        // Detect "## Specs" section
        if line.starts_with("## Specs") {
            in_specs_section = true;
            continue;
        }

        // Detect next top-level section (end of specs)
        if in_specs_section && line.starts_with("## ") && !line.starts_with("## Specs") {
            if let Some(spec) = current_spec.take() {
                specs.push(spec);
            }
            in_specs_section = false;
            continue;
        }

        // Parse spec headers: "### Spec N: Name"
        if in_specs_section && line.starts_with("### Spec ") {
            if let Some(spec) = current_spec.take() {
                specs.push(spec);
            }

            let name = line
                .trim_start_matches("### Spec ")
                .split(':')
                .nth(1)
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| "Unnamed Spec".to_string());

            current_spec = Some(SpecDefinition {
                name,
                description: String::new(),
                scope: Vec::new(),
            });
            in_scope = false;
            continue;
        }

        // Accumulate description and scope
        if let Some(ref mut spec) = current_spec {
            if line.starts_with("**Scope:**") {
                in_scope = true;
            } else if line.starts_with("- ") && in_scope {
                spec.scope.push(line[2..].trim().to_string());
            } else if !line.is_empty() && !in_scope && !line.starts_with('#') {
                if !spec.description.is_empty() {
                    spec.description.push(' ');
                }
                spec.description.push_str(line.trim());
            }
        }
    }

    // Don't forget the last spec
    if let Some(spec) = current_spec {
        specs.push(spec);
    }

    specs
}

/// Parse phases from a spec.md artifact.
///
/// Phases are defined in the "## Phases" section with headers like:
/// "### Phase 1: User Model and Repository"
pub fn parse_phases_from_spec(spec_content: &str) -> Vec<PhaseDefinition> {
    let mut phases = Vec::new();
    let mut current_phase: Option<PhaseDefinition> = None;
    let mut in_phases_section = false;
    let mut current_field = "";

    for line in spec_content.lines() {
        if line.starts_with("## Phases") {
            in_phases_section = true;
            continue;
        }

        if in_phases_section && line.starts_with("## ") && !line.starts_with("## Phases") {
            if let Some(phase) = current_phase.take() {
                phases.push(phase);
            }
            in_phases_section = false;
            continue;
        }

        // Parse phase headers: "### Phase N: Name"
        if in_phases_section && line.starts_with("### Phase ") {
            if let Some(phase) = current_phase.take() {
                phases.push(phase);
            }

            let rest = line.trim_start_matches("### Phase ");
            let parts: Vec<&str> = rest.splitn(2, ':').collect();

            let number = parts[0].trim().parse().unwrap_or(phases.len() + 1);
            let name = parts
                .get(1)
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| format!("Phase {}", number));

            current_phase = Some(PhaseDefinition {
                number,
                name,
                goal: String::new(),
                tasks: Vec::new(),
                validation: String::new(),
            });
            current_field = "";
        }

        if let Some(ref mut phase) = current_phase {
            if line.starts_with("**Goal:**") {
                phase.goal = line.trim_start_matches("**Goal:**").trim().to_string();
            } else if line.starts_with("**Tasks:**") {
                current_field = "tasks";
            } else if line.starts_with("**Validation:**") {
                phase.validation = line.trim_start_matches("**Validation:**").trim().to_string();
                current_field = "";
            } else if current_field == "tasks" && line.starts_with("- ") {
                phase.tasks.push(line[2..].trim().to_string());
            }
        }
    }

    if let Some(phase) = current_phase {
        phases.push(phase);
    }

    phases
}

/// Extract the goal from a phase.md artifact.
pub fn extract_phase_goal(phase_content: &str) -> String {
    let mut in_goal = false;

    for line in phase_content.lines() {
        if line.starts_with("## Goal") {
            in_goal = true;
            continue;
        }

        if in_goal {
            let trimmed = line.trim();
            if trimmed.starts_with("##") {
                break;
            }
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }

    "Implement phase".to_string()
}

/// Count content characters (excluding headers and whitespace).
fn count_content_chars(content: &str) -> usize {
    content
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| line.len())
        .sum()
}

/// Validate that a plan artifact has sufficient content and structure.
pub fn validate_plan_format(content: &str) -> Result<()> {
    // Check minimum content
    let content_chars = count_content_chars(content);
    if content_chars < MIN_PLAN_CONTENT {
        return Err(eyre!(
            "Plan too sparse: {} content characters, minimum is {}. Add more detail.",
            content_chars,
            MIN_PLAN_CONTENT
        ));
    }

    let required = ["# Plan:", "## Summary", "## Goals", "## Specs"];
    for section in required {
        if !content.contains(section) {
            return Err(eyre!("Missing required section: {}", section));
        }
    }

    // Check specs section has at least one spec
    let specs = parse_specs_from_plan(content);
    if specs.is_empty() {
        return Err(eyre!("Plan must define at least one spec"));
    }

    // Validate each spec has meaningful content
    for spec in &specs {
        if spec.name.trim().is_empty() {
            return Err(eyre!("Spec name cannot be empty"));
        }
        if spec.scope.is_empty() {
            return Err(eyre!("Spec '{}' must have at least one scope item", spec.name));
        }
    }

    Ok(())
}

/// Validate that a spec artifact has sufficient content and structure.
pub fn validate_spec_format(content: &str) -> Result<()> {
    // Check minimum content
    let content_chars = count_content_chars(content);
    if content_chars < MIN_SPEC_CONTENT {
        return Err(eyre!(
            "Spec too sparse: {} content characters, minimum is {}. Add more detail.",
            content_chars,
            MIN_SPEC_CONTENT
        ));
    }

    let required = ["# Spec:", "## Overview", "## Requirements", "## Phases"];
    for section in required {
        if !content.contains(section) {
            return Err(eyre!("Missing required section: {}", section));
        }
    }

    // Check phases section has 1-7 phases
    let phases = parse_phases_from_spec(content);
    if phases.is_empty() {
        return Err(eyre!("Spec must define at least one phase"));
    }
    if phases.len() > 7 {
        return Err(eyre!("Spec should have at most 7 phases, got {}", phases.len()));
    }

    // Validate each phase has meaningful content
    for phase in &phases {
        if phase.name.trim().is_empty() {
            return Err(eyre!("Phase {} name cannot be empty", phase.number));
        }
        if phase.goal.trim().is_empty() {
            return Err(eyre!("Phase '{}' must have a goal", phase.name));
        }
        if phase.tasks.is_empty() {
            return Err(eyre!("Phase '{}' must have at least one task", phase.name));
        }
    }

    Ok(())
}

/// Validate that a phase artifact has sufficient content and structure.
pub fn validate_phase_format(content: &str) -> Result<()> {
    // Check minimum content
    let content_chars = count_content_chars(content);
    if content_chars < MIN_PHASE_CONTENT {
        return Err(eyre!(
            "Phase too sparse: {} content characters, minimum is {}. Add more detail.",
            content_chars,
            MIN_PHASE_CONTENT
        ));
    }

    let required = ["# Phase:", "## Goal", "## Tasks"];
    for section in required {
        if !content.contains(section) {
            return Err(eyre!("Missing required section: {}", section));
        }
    }

    // Extract and validate tasks section has actual tasks
    let tasks_section = content
        .split("## Tasks")
        .nth(1)
        .and_then(|s| s.split("##").next())
        .unwrap_or("");

    let task_count = tasks_section
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            trimmed.starts_with("- ") || trimmed.starts_with("1.")
        })
        .count();

    if task_count == 0 {
        return Err(eyre!("Phase must have at least one task in Tasks section"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE_PLAN: &str = r#"# Plan: Add User Authentication

## Summary

Add JWT-based authentication to the REST API. Users will be able to register, log in, and access protected endpoints with bearer tokens.

## Goals

- Users can register with email/password
- Users can log in and receive a JWT token
- Protected endpoints require valid JWT

## Non-Goals

- OAuth/social login (future work)

## Proposed Solution

### Overview

Implement a standard JWT authentication flow.

## Specs

### Spec 1: Core Authentication

Implement the auth service with registration and login.

**Scope:**
- User model with password hashing
- Registration endpoint
- Login endpoint with JWT generation

### Spec 2: Protected Routes

Add middleware and protect existing endpoints.

**Scope:**
- JWT validation middleware
- Apply to all /api/* routes

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Token leakage | Low | High | Use short expiry |
"#;

    const EXAMPLE_SPEC: &str = r#"# Spec: Core Authentication

**Parent Plan:** Add User Authentication
**Spec Number:** 1 of 2

## Overview

Implement the core authentication service including user registration and login.

## Requirements

### Functional Requirements

1. **FR1**: Users can register with email and password
2. **FR2**: Passwords are hashed before storage

## Acceptance Criteria

- [ ] POST /api/register creates a new user
- [ ] POST /api/login returns a JWT on success

## Phases

### Phase 1: User Model and Repository

**Goal:** Create the User model with password hashing support

**Tasks:**
- Add User struct with id, email, password_hash
- Implement UserRepository with create and find_by_email
- Add bcrypt password hashing utility

**Validation:** `cargo test --package auth -- user`

### Phase 2: Registration Endpoint

**Goal:** Implement user registration

**Tasks:**
- Add POST /api/register endpoint
- Validate email format and password strength
- Hash password and store user

**Validation:** `cargo test --package auth -- register`

### Phase 3: Login and JWT

**Goal:** Implement login with JWT generation

**Tasks:**
- Add POST /api/login endpoint
- Verify password against hash
- Generate JWT with user_id claim

**Validation:** `cargo test --package auth -- login`

## Technical Notes

- Use jsonwebtoken crate for JWT
"#;

    const EXAMPLE_PHASE: &str = r#"# Phase: User Model and Repository

**Parent Spec:** Core Authentication
**Phase Number:** 1 of 3

## Goal

Create the User model with password hashing support and repository for database operations.

## Context

This is the foundation phase for authentication.

## Tasks

1. Create User struct in `src/models/user.rs`
2. Implement password hashing utility
3. Create UserRepository
4. Add database migration
5. Write tests

## Files to Modify

- `src/models/user.rs` - New file
- `src/auth/password.rs` - New file

## Acceptance Criteria

- [ ] User struct has required fields
- [ ] password::hash() returns bcrypt hash

## Validation Command

```bash
cargo test --package api -- user
```
"#;

    #[test]
    fn test_parse_specs_from_plan() {
        let specs = parse_specs_from_plan(EXAMPLE_PLAN);

        assert_eq!(specs.len(), 2);

        assert_eq!(specs[0].name, "Core Authentication");
        assert!(specs[0].description.contains("auth service with registration"));
        assert_eq!(specs[0].scope.len(), 3);
        assert!(specs[0].scope[0].contains("User model"));

        assert_eq!(specs[1].name, "Protected Routes");
        assert_eq!(specs[1].scope.len(), 2);
    }

    #[test]
    fn test_parse_specs_empty_plan() {
        let specs = parse_specs_from_plan("# Plan: Empty\n\n## Summary\n\nNothing here");
        assert!(specs.is_empty());
    }

    #[test]
    fn test_parse_phases_from_spec() {
        let phases = parse_phases_from_spec(EXAMPLE_SPEC);

        assert_eq!(phases.len(), 3);

        assert_eq!(phases[0].number, 1);
        assert_eq!(phases[0].name, "User Model and Repository");
        assert!(phases[0].goal.contains("User model"));
        assert_eq!(phases[0].tasks.len(), 3);
        assert!(phases[0].validation.contains("cargo test"));

        assert_eq!(phases[1].number, 2);
        assert_eq!(phases[1].name, "Registration Endpoint");

        assert_eq!(phases[2].number, 3);
        assert_eq!(phases[2].name, "Login and JWT");
    }

    #[test]
    fn test_parse_phases_empty_spec() {
        let phases = parse_phases_from_spec("# Spec: Empty\n\n## Overview\n\nNothing");
        assert!(phases.is_empty());
    }

    #[test]
    fn test_extract_phase_goal() {
        let goal = extract_phase_goal(EXAMPLE_PHASE);
        assert!(goal.contains("User model"));
    }

    #[test]
    fn test_extract_phase_goal_missing() {
        let goal = extract_phase_goal("# Phase: Test\n\n## Tasks\n- Do stuff");
        assert_eq!(goal, "Implement phase");
    }

    #[test]
    fn test_validate_plan_format_valid() {
        assert!(validate_plan_format(EXAMPLE_PLAN).is_ok());
    }

    #[test]
    fn test_validate_plan_format_missing_section() {
        // Plan with enough content but missing Goals section
        let bad_plan = r#"# Plan: Test

## Summary

This is a detailed summary that explains what we are building. It provides enough context about the project goals and overall approach to implementation. We want to create something useful and valuable for users.

## Non-Goals

- We are not doing X
- We are not doing Y

## Proposed Solution

### Overview

We will implement this with a careful approach that considers all the requirements and constraints. The architecture will be modular and extensible.

## Specs

### Spec 1: Core

Basic implementation

**Scope:**
- Item 1
- Item 2
"#;
        let result = validate_plan_format(bad_plan);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Missing required"));
    }

    #[test]
    fn test_validate_plan_format_no_specs() {
        let bad_plan = r#"# Plan: Test

## Summary

This is a comprehensive summary explaining what we are building for this project. It contains enough detail to pass the minimum content threshold and provides good context for understanding the overall goals and approach.

## Goals

- Goal 1: Build something useful
- Goal 2: Make it work correctly

## Non-Goals

- We are not doing optional feature X
- We are not doing optional feature Y

## Proposed Solution

### Overview

This is our proposed approach to solving the problem with careful consideration of all requirements and constraints in the project scope.

## Specs

This section contains descriptive text but no actual spec definitions with the required ### Spec N: Name format.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Something | Low | Medium | Handle it |
"#;
        let result = validate_plan_format(bad_plan);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("at least one spec"));
    }

    #[test]
    fn test_validate_spec_format_valid() {
        assert!(validate_spec_format(EXAMPLE_SPEC).is_ok());
    }

    #[test]
    fn test_validate_spec_format_too_many_phases() {
        // Create a spec with 8 phases (exceeds the limit of 7)
        let spec_with_many_phases = r#"# Spec: Too Many Phases

**Parent Plan:** Test Plan
**Spec Number:** 1 of 1

## Overview

A specification with too many phases to test the validation limit. This needs enough content to pass the minimum threshold.

## Requirements

### Functional Requirements

1. **FR1**: Do something useful and testable

## Acceptance Criteria

- [ ] Everything works correctly

## Phases

### Phase 1: First

**Goal:** Set up the project structure

**Tasks:**
- Create files
- Configure settings

**Validation:** cargo test

### Phase 2: Second

**Goal:** Implement feature A

**Tasks:**
- Write code
- Add tests

**Validation:** cargo test

### Phase 3: Third

**Goal:** Implement feature B

**Tasks:**
- Write more code

**Validation:** cargo test

### Phase 4: Fourth

**Goal:** Implement feature C

**Tasks:**
- Continue coding

**Validation:** cargo test

### Phase 5: Fifth

**Goal:** Implement feature D

**Tasks:**
- More implementation

**Validation:** cargo test

### Phase 6: Sixth

**Goal:** Implement feature E

**Tasks:**
- Additional work

**Validation:** cargo test

### Phase 7: Seventh

**Goal:** Implement feature F

**Tasks:**
- Final feature

**Validation:** cargo test

### Phase 8: Eighth

**Goal:** Implement feature G (this exceeds the limit)

**Tasks:**
- Too many phases

**Validation:** cargo test

## Technical Notes

- Use good practices
"#;
        let result = validate_spec_format(spec_with_many_phases);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("at most 7 phases"),
            "Expected 'at most 7 phases' error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_validate_phase_format_valid() {
        assert!(validate_phase_format(EXAMPLE_PHASE).is_ok());
    }

    #[test]
    fn test_validate_phase_format_no_tasks() {
        // Phase with enough content but no actual tasks (no - or 1. items)
        let bad_phase = r#"# Phase: Test Phase

**Parent Spec:** Test Spec
**Phase Number:** 1 of 3

## Goal

Create the project structure and configuration files for the implementation phase.

## Context

This phase sets up the foundation for subsequent implementation work. It is important to get this right.

## Tasks

This section has text but no actual task items with bullet points or numbered lists. The content here describes what should be done but doesn't use the proper format.

## Files to Modify

Some files here that will be modified.

## Acceptance Criteria

Testing criteria that need to pass.
"#;
        let result = validate_phase_format(bad_phase);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("at least one task"));
    }

    #[test]
    fn test_spec_definition_equality() {
        let spec1 = SpecDefinition {
            name: "Test".to_string(),
            description: "Desc".to_string(),
            scope: vec!["Item".to_string()],
        };
        let spec2 = spec1.clone();
        assert_eq!(spec1, spec2);
    }

    #[test]
    fn test_phase_definition_equality() {
        let phase1 = PhaseDefinition {
            number: 1,
            name: "Test".to_string(),
            goal: "Goal".to_string(),
            tasks: vec!["Task".to_string()],
            validation: "test".to_string(),
        };
        let phase2 = phase1.clone();
        assert_eq!(phase1, phase2);
    }
}
