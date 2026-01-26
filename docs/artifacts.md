# Artifact Specification: plan.md, spec.md, phase.md

**Author:** Scott A. Idler
**Date:** 2026-01-25
**Status:** Implementation Spec
**Parent Doc:** [loop-architecture.md](loop-architecture.md)

---

## Summary

Artifacts are the connective tissue between loop levels. Each loop type produces a specific artifact format that defines work for child loops:
- **PlanLoop** produces `plan.md` → spawns SpecLoops
- **SpecLoop** produces `spec.md` → spawns PhaseLoops
- **PhaseLoop** produces `phase.md` → spawns RalphLoops
- **RalphLoop** produces code files (no child spawning)

This document specifies the required format for each artifact type.

---

## Artifact Storage

Artifacts live in the producing loop's iteration directory:

```
~/.loopr/<project>/loops/<loop-id>/
├── iterations/
│   ├── 001/
│   │   └── artifacts/
│   │       └── plan.md      ← Artifact from iteration 1
│   ├── 002/
│   │   └── artifacts/
│   │       └── plan.md      ← Artifact from iteration 2 (revised)
│   └── 003/
│       └── artifacts/
│           └── plan.md      ← Final artifact (validation passed)
└── current -> iterations/003/
```

The `triggered_by` field in child loops references the specific iteration:
```
"triggered_by": "iterations/003/artifacts/plan.md"
```

---

## plan.md Format

### Purpose

Defines the high-level approach for a task. Reviewed through 5 passes (Rule of Five) before spawning specs.

### Required Sections

```markdown
# Plan: <Title>

## Summary

<2-3 sentences describing what will be built and why>

## Goals

- <Goal 1 - measurable outcome>
- <Goal 2 - measurable outcome>
- <Goal 3 - measurable outcome>

## Non-Goals

- <What is explicitly out of scope>
- <What might be assumed but isn't included>

## Proposed Solution

### Overview

<High-level approach in 1-2 paragraphs>

### Key Components

- **Component A**: <description>
- **Component B**: <description>

## Specs

This plan will be implemented through the following specs:

### Spec 1: <Name>

<Brief description of what this spec covers>

**Scope:**
- <Item 1>
- <Item 2>

### Spec 2: <Name>

<Brief description>

**Scope:**
- <Item 1>

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| <Risk 1> | Medium | High | <How to prevent/handle> |

## Open Questions

- <Question 1> (if any remain)

---
*Review Pass: 5/5 - Ready for implementation*
```

### Parsing Rules

To extract specs from a plan:

```rust
fn parse_specs_from_plan(plan_content: &str) -> Vec<SpecDefinition> {
    let mut specs = Vec::new();
    let mut current_spec: Option<SpecDefinition> = None;
    let mut in_specs_section = false;

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
        }

        // Accumulate description and scope
        if let Some(ref mut spec) = current_spec {
            if line.starts_with("- ") {
                spec.scope.push(line[2..].to_string());
            } else if !line.is_empty() && !line.starts_with("**Scope:**") {
                if !spec.description.is_empty() {
                    spec.description.push('\n');
                }
                spec.description.push_str(line);
            }
        }
    }

    if let Some(spec) = current_spec {
        specs.push(spec);
    }

    specs
}
```

### Validation

Plans are validated by the Rule of Five (see [rule-of-five.md](rule-of-five.md)):

1. **Completeness** - All sections present
2. **Correctness** - No logical errors
3. **Edge Cases** - Failure modes addressed
4. **Architecture** - Fits existing system
5. **Clarity** - Unambiguous, implementable

---

## spec.md Format

### Purpose

Detailed specification for a subset of the plan. Defines concrete requirements and acceptance criteria that can be decomposed into phases.

### Required Sections

```markdown
# Spec: <Title>

**Parent Plan:** <plan-title>
**Spec Number:** 1 of 2

## Overview

<1-2 paragraphs describing what this spec implements>

## Requirements

### Functional Requirements

1. **FR1**: <Concrete, testable requirement>
2. **FR2**: <Concrete, testable requirement>
3. **FR3**: <Concrete, testable requirement>

### Non-Functional Requirements

1. **NFR1**: <Performance/security/etc requirement>

## Acceptance Criteria

- [ ] <AC1: Specific, verifiable condition>
- [ ] <AC2: Specific, verifiable condition>
- [ ] <AC3: Specific, verifiable condition>

## Phases

This spec will be implemented in the following phases:

### Phase 1: <Name>

**Goal:** <One sentence describing the phase outcome>

**Tasks:**
- <Task 1>
- <Task 2>

**Validation:** <How to verify this phase is complete>

### Phase 2: <Name>

**Goal:** <One sentence>

**Tasks:**
- <Task 1>

**Validation:** <Verification method>

### Phase 3: <Name>

**Goal:** <One sentence>

**Tasks:**
- <Task 1>

**Validation:** <Verification method>

## Technical Notes

- <Implementation hint or constraint>
- <API or library to use>

## Dependencies

- <External dependency or prerequisite>

---
*Spec validated and ready for phase execution*
```

### Parsing Rules

To extract phases from a spec:

```rust
fn parse_phases_from_spec(spec_content: &str) -> Vec<PhaseDefinition> {
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

            let parts: Vec<&str> = line
                .trim_start_matches("### Phase ")
                .splitn(2, ':')
                .collect();

            let number = parts[0].trim().parse().unwrap_or(phases.len() + 1);
            let name = parts.get(1).map(|s| s.trim().to_string())
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
                phase.tasks.push(line[2..].to_string());
            }
        }
    }

    if let Some(phase) = current_phase {
        phases.push(phase);
    }

    phases
}
```

### Validation

Specs are validated with:

1. **Structure check** - Required sections present
2. **LLM-as-judge** - Is this implementable? Are acceptance criteria testable?

---

## phase.md Format

### Purpose

Defines a single implementation phase. Contains enough detail for a Ralph loop to execute the work.

### Required Sections

```markdown
# Phase: <Title>

**Parent Spec:** <spec-title>
**Phase Number:** 2 of 5

## Goal

<One clear sentence describing the outcome of this phase>

## Context

<Relevant background from the spec that this phase needs>

## Tasks

1. <Specific, actionable task>
2. <Specific, actionable task>
3. <Specific, actionable task>

## Files to Modify

- `src/api/users.rs` - Add validation logic
- `src/models/user.rs` - Add new fields
- `tests/api_tests.rs` - Add test cases

## Acceptance Criteria

- [ ] <Specific, verifiable condition>
- [ ] <Specific, verifiable condition>

## Validation Command

```bash
cargo test --package api -- users
```

## Notes

- <Implementation hint>
- <Edge case to handle>

---
*Phase ready for implementation*
```

### Parsing Rules

Phases are leaf-level spawners. The Ralph loop reads the entire phase.md as context rather than parsing specific sections.

```rust
fn create_ralph_from_phase(phase_path: &Path, phase_loop_id: &str) -> Result<RalphLoop> {
    let content = std::fs::read_to_string(phase_path)?;

    // Extract the goal as the task description
    let goal = content
        .lines()
        .find(|line| line.starts_with("## Goal"))
        .and_then(|_| content.lines().skip_while(|l| !l.starts_with("## Goal")).nth(2))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "Implement phase".to_string());

    Ok(RalphLoop::from_phase_artifact(
        phase_loop_id,
        phase_path.to_str().unwrap(),
        content,
        goal,
        100, // max_iterations
    ))
}
```

### Validation

Phases are validated by running the validation command (typically `otto ci` or similar).

---

## Artifact Lifecycle

### Creation

```
1. Loop produces output (LLM generates content)
2. Tool writes to iterations/NNN/artifacts/<name>.md
3. Loop validation checks artifact format
4. If valid: loop completes, children can spawn
5. If invalid: loop re-iterates with feedback
```

### Spawning Children

When a loop completes with a valid artifact:

```rust
async fn spawn_children_from_artifact(
    parent: &LoopRecord,
    artifact_path: &Path,
    store: &TaskStore,
) -> Result<Vec<LoopRecord>> {
    let content = tokio::fs::read_to_string(artifact_path).await?;

    let children = match parent.loop_type {
        LoopType::Plan => {
            let specs = parse_specs_from_plan(&content);
            specs.into_iter().map(|spec| {
                LoopRecord::new_spec_from_plan(
                    &parent.id,
                    artifact_path.to_str().unwrap(),
                    &spec.name,
                    &spec.description,
                )
            }).collect()
        }
        LoopType::Spec => {
            let phases = parse_phases_from_spec(&content);
            phases.into_iter().map(|phase| {
                LoopRecord::new_phase_from_spec(
                    &parent.id,
                    artifact_path.to_str().unwrap(),
                    phase.number,
                    &phase.name,
                    &phase.goal,
                )
            }).collect()
        }
        LoopType::Phase => {
            // Phases spawn exactly one ralph
            vec![LoopRecord::new_ralph_from_phase(
                &parent.id,
                artifact_path.to_str().unwrap(),
            )]
        }
        LoopType::Ralph => {
            // Ralphs don't spawn children
            vec![]
        }
    };

    // Create all children in TaskStore
    for child in &children {
        store.create(child)?;
    }

    Ok(children)
}
```

### Invalidation

When a parent re-iterates, the old artifact becomes stale:

```
Parent: iterations/001/artifacts/plan.md  ← Old
        iterations/002/artifacts/plan.md  ← New (after re-iteration)

Children triggered by iterations/001/artifacts/plan.md are invalidated
```

---

## Artifact Validation

### Empty Artifact Detection

Artifacts must contain meaningful content to spawn children. Empty or near-empty artifacts are rejected.

```rust
/// Minimum content thresholds (characters, excluding whitespace and headers)
const MIN_PLAN_CONTENT: usize = 200;   // Plans need substantial detail
const MIN_SPEC_CONTENT: usize = 300;   // Specs need requirements + phases
const MIN_PHASE_CONTENT: usize = 100;  // Phases need tasks + validation

fn validate_artifact_not_empty(content: &str, artifact_type: &str) -> Result<()> {
    // Remove markdown headers and whitespace to count actual content
    let content_chars: usize = content
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| line.len())
        .sum();

    let min_chars = match artifact_type {
        "plan" => MIN_PLAN_CONTENT,
        "spec" => MIN_SPEC_CONTENT,
        "phase" => MIN_PHASE_CONTENT,
        _ => 50, // Default fallback
    };

    if content_chars < min_chars {
        return Err(eyre!(
            "Artifact too sparse: {} has {} content characters, minimum is {}. \
             Add more detail to goals, requirements, or tasks.",
            artifact_type,
            content_chars,
            min_chars
        ));
    }

    Ok(())
}
```

### Format Validation

Check that required sections exist and contain content:

```rust
fn validate_plan_format(content: &str) -> Result<()> {
    // Check minimum content
    validate_artifact_not_empty(content, "plan")?;

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

fn validate_spec_format(content: &str) -> Result<()> {
    // Check minimum content
    validate_artifact_not_empty(content, "spec")?;

    let required = ["# Spec:", "## Overview", "## Requirements", "## Phases"];
    for section in required {
        if !content.contains(section) {
            return Err(eyre!("Missing required section: {}", section));
        }
    }

    // Check phases section has 3-7 phases
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

fn validate_phase_format(content: &str) -> Result<()> {
    // Check minimum content
    validate_artifact_not_empty(content, "phase")?;

    let required = ["# Phase:", "## Goal", "## Tasks", "## Validation Command"];
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
        .filter(|line| line.trim().starts_with("- ") || line.trim().starts_with("1."))
        .count();

    if task_count == 0 {
        return Err(eyre!("Phase must have at least one task in Tasks section"));
    }

    Ok(())
}
```

### Content Validation (LLM-as-Judge)

For subjective quality (see [loop-validation.md](loop-validation.md)):

```rust
async fn validate_plan_quality(content: &str, llm: &dyn LlmClient) -> Result<bool> {
    let prompt = format!(
        "Review this plan for implementability:\n\n{}\n\n\
         Check:\n\
         1. Is the summary clear and complete?\n\
         2. Are goals measurable?\n\
         3. Are specs concrete enough to implement?\n\
         4. Are risks realistic?\n\n\
         Respond with PASS or FAIL: <reason>",
        content
    );

    let response = llm.complete(&prompt).await?;
    Ok(response.trim().starts_with("PASS"))
}
```

---

## Example Artifacts

### Example plan.md

```markdown
# Plan: Add User Authentication

## Summary

Add JWT-based authentication to the REST API. Users will be able to register, log in, and access protected endpoints with bearer tokens.

## Goals

- Users can register with email/password
- Users can log in and receive a JWT token
- Protected endpoints require valid JWT
- Tokens expire after 24 hours

## Non-Goals

- OAuth/social login (future work)
- Password reset flow (future work)
- Multi-factor authentication

## Proposed Solution

### Overview

Implement a standard JWT authentication flow using the `jsonwebtoken` crate. Passwords will be hashed with bcrypt. A middleware will validate tokens on protected routes.

### Key Components

- **AuthService**: Handles registration, login, token generation
- **AuthMiddleware**: Validates JWT on protected routes
- **UserRepository**: Database operations for users

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
- Handle expired/invalid tokens

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Token leakage | Low | High | Use short expiry, HTTPS only |
| Brute force attacks | Medium | Medium | Rate limiting on login |

## Open Questions

None remaining.

---
*Review Pass: 5/5 - Ready for implementation*
```

### Example spec.md

```markdown
# Spec: Core Authentication

**Parent Plan:** Add User Authentication
**Spec Number:** 1 of 2

## Overview

Implement the core authentication service including user registration and login with JWT token generation. This creates the foundation for protected routes.

## Requirements

### Functional Requirements

1. **FR1**: Users can register with email and password
2. **FR2**: Passwords are hashed before storage
3. **FR3**: Users can log in with email/password
4. **FR4**: Successful login returns a JWT token
5. **FR5**: JWT contains user ID and expiration

### Non-Functional Requirements

1. **NFR1**: Password hashing uses bcrypt with cost 12
2. **NFR2**: JWT tokens expire after 24 hours

## Acceptance Criteria

- [ ] POST /api/register creates a new user
- [ ] POST /api/login returns a JWT on success
- [ ] Invalid login returns 401
- [ ] Duplicate email registration returns 409
- [ ] All passwords in DB are hashed

## Phases

### Phase 1: User Model and Repository

**Goal:** Create the User model with password hashing support

**Tasks:**
- Add User struct with id, email, password_hash, created_at
- Implement UserRepository with create and find_by_email
- Add bcrypt password hashing utility

**Validation:** `cargo test --package auth -- user`

### Phase 2: Registration Endpoint

**Goal:** Implement user registration

**Tasks:**
- Add POST /api/register endpoint
- Validate email format and password strength
- Hash password and store user
- Return 201 on success, 409 on duplicate

**Validation:** `cargo test --package auth -- register`

### Phase 3: Login and JWT

**Goal:** Implement login with JWT generation

**Tasks:**
- Add POST /api/login endpoint
- Verify password against hash
- Generate JWT with user_id claim
- Return token on success, 401 on failure

**Validation:** `cargo test --package auth -- login`

## Technical Notes

- Use `jsonwebtoken` crate for JWT
- Use `bcrypt` crate for password hashing
- Store JWT secret in environment variable

## Dependencies

- `jsonwebtoken = "9"`
- `bcrypt = "0.15"`

---
*Spec validated and ready for phase execution*
```

### Example phase.md

```markdown
# Phase: User Model and Repository

**Parent Spec:** Core Authentication
**Phase Number:** 1 of 3

## Goal

Create the User model with password hashing support and repository for database operations.

## Context

This is the foundation phase for authentication. The User model will store credentials, and the repository will handle database operations. Password hashing must be done before storage.

## Tasks

1. Create User struct in `src/models/user.rs`
2. Implement password hashing utility in `src/auth/password.rs`
3. Create UserRepository in `src/repositories/user.rs`
4. Add database migration for users table
5. Write unit tests for password hashing
6. Write integration tests for repository

## Files to Modify

- `src/models/mod.rs` - Add user module
- `src/models/user.rs` - New file: User struct
- `src/auth/mod.rs` - Add password module
- `src/auth/password.rs` - New file: hashing utilities
- `src/repositories/mod.rs` - Add user module
- `src/repositories/user.rs` - New file: UserRepository
- `migrations/` - New migration file

## Acceptance Criteria

- [ ] User struct has id, email, password_hash, created_at fields
- [ ] password::hash() returns bcrypt hash
- [ ] password::verify() validates password against hash
- [ ] UserRepository::create() stores user in DB
- [ ] UserRepository::find_by_email() retrieves user
- [ ] All tests pass

## Validation Command

```bash
cargo test --package api -- user password
```

## Notes

- Use bcrypt cost factor of 12 (balance of security and speed)
- User.id should be UUID
- created_at uses UTC timestamp

---
*Phase ready for implementation*
```

---

## References

- [loop-architecture.md](loop-architecture.md) - Loop hierarchy and artifact storage
- [loop-validation.md](loop-validation.md) - Validation strategies
- [rule-of-five.md](rule-of-five.md) - Plan review methodology
- [domain-types.md](domain-types.md) - LoopRecord schema
