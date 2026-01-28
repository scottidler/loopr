//! Loop hierarchy: Plan → Spec → Phase → Ralph.
//!
//! This module provides the higher-level loop types that produce artifacts
//! and spawn child loops. Each loop type follows the Ralph Wiggum pattern:
//! fresh context per iteration, prompt updated with failure feedback.

use std::path::PathBuf;

use crate::llm::{
    CompletionRequest, ContentBlock, LlmClient, Message, MessageContent, Role, StopReason, ToolContext, ToolDefinition,
    ToolExecutor,
};
use crate::store::{LoopRecord, LoopStatus, LoopType, TaskStore};

use super::artifacts::{
    PhaseDefinition, SpecDefinition, extract_phase_goal, parse_phases_from_spec, parse_specs_from_plan,
    validate_phase_format, validate_plan_format, validate_spec_format,
};
use super::ralph::LoopError;
use super::validation::{ValidationFeedback, ValidationResult};
use super::worktree::{Worktree, WorktreeConfig};

/// System prompt for plan loops (high-level architecture).
const PLAN_SYSTEM_PROMPT: &str = r#"You are an AI architect creating a detailed implementation plan. Your plan will be reviewed through 5 passes (Rule of Five) and then decomposed into specs for implementation.

## Output Format

Create a plan.md artifact following this exact structure:

```markdown
# Plan: <Title>

## Summary
<2-3 sentences describing what will be built and why>

## Goals
- <Goal 1 - measurable outcome>
- <Goal 2 - measurable outcome>

## Non-Goals
- <What is explicitly out of scope>

## Proposed Solution

### Overview
<High-level approach in 1-2 paragraphs>

### Key Components
- **Component A**: <description>
- **Component B**: <description>

## Specs

This plan will be implemented through the following specs:

### Spec 1: <Name>
<Brief description>

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
| <Risk 1> | Medium | High | <How to handle> |

## Open Questions
- <Question 1> (if any remain)
```

## Important
- Keep plans focused and achievable
- Each spec should be independently implementable
- Include 2-5 specs typically
- Address risks proactively"#;

/// System prompt for spec loops (detailed requirements).
const SPEC_SYSTEM_PROMPT: &str = r#"You are an AI requirements engineer creating a detailed specification. Your spec will define concrete requirements that can be decomposed into implementation phases.

## Output Format

Create a spec.md artifact following this exact structure:

```markdown
# Spec: <Title>

**Parent Plan:** <plan-title>
**Spec Number:** N of M

## Overview
<1-2 paragraphs describing what this spec implements>

## Requirements

### Functional Requirements
1. **FR1**: <Concrete, testable requirement>
2. **FR2**: <Concrete, testable requirement>

### Non-Functional Requirements
1. **NFR1**: <Performance/security/etc requirement>

## Acceptance Criteria
- [ ] <AC1: Specific, verifiable condition>
- [ ] <AC2: Specific, verifiable condition>

## Phases

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

## Technical Notes
- <Implementation hint or constraint>

## Dependencies
- <External dependency or prerequisite>
```

## Important
- Include 3-7 phases typically
- Each phase should be independently testable
- Validation commands should be runnable
- Be specific about acceptance criteria"#;

/// System prompt for phase loops (implementation units).
const PHASE_SYSTEM_PROMPT: &str = r#"You are an AI engineer creating a detailed phase implementation plan. This phase will be executed by a Ralph loop that writes actual code.

## Output Format

Create a phase.md artifact following this exact structure:

```markdown
# Phase: <Title>

**Parent Spec:** <spec-title>
**Phase Number:** N of M

## Goal
<One clear sentence describing the outcome of this phase>

## Context
<Relevant background from the spec that this phase needs>

## Tasks
1. <Specific, actionable task>
2. <Specific, actionable task>
3. <Specific, actionable task>

## Files to Modify
- `path/to/file.rs` - Description of changes
- `path/to/new_file.rs` - New file description

## Acceptance Criteria
- [ ] <Specific, verifiable condition>
- [ ] <Specific, verifiable condition>

## Validation Command
```bash
<command to verify completion>
```

## Notes
- <Implementation hint>
- <Edge case to handle>
```

## Important
- Be very specific about what files to modify
- Tasks should be concrete and actionable
- Include a working validation command"#;

/// Configuration for hierarchy loops.
#[derive(Debug, Clone)]
pub struct HierarchyLoopConfig {
    /// Worktree configuration
    pub worktree: WorktreeConfig,
    /// Maximum tokens for LLM response
    pub max_tokens: u32,
}

impl Default for HierarchyLoopConfig {
    fn default() -> Self {
        Self {
            worktree: WorktreeConfig::default(),
            max_tokens: 16384,
        }
    }
}

/// PlanLoop produces plan.md artifacts that spawn SpecLoops.
pub struct PlanLoop {
    /// The loop record from TaskStore
    pub record: LoopRecord,
    /// The worktree for this loop
    worktree: Option<Worktree>,
    /// Configuration
    config: HierarchyLoopConfig,
    /// Iteration history for prompt building
    iteration_history: Vec<IterationFeedback>,
}

/// SpecLoop produces spec.md artifacts that spawn PhaseLoops.
pub struct SpecLoop {
    /// The loop record from TaskStore
    pub record: LoopRecord,
    /// The worktree for this loop
    worktree: Option<Worktree>,
    /// Configuration
    config: HierarchyLoopConfig,
    /// Iteration history for prompt building
    iteration_history: Vec<IterationFeedback>,
}

/// PhaseLoop produces phase.md artifacts that spawn RalphLoops.
pub struct PhaseLoop {
    /// The loop record from TaskStore
    pub record: LoopRecord,
    /// The worktree for this loop
    worktree: Option<Worktree>,
    /// Configuration
    config: HierarchyLoopConfig,
    /// Iteration history for prompt building
    iteration_history: Vec<IterationFeedback>,
}

/// Feedback from a single iteration (for prompt building).
#[derive(Debug, Clone)]
pub struct IterationFeedback {
    /// Iteration number
    pub iteration: u32,
    /// What went wrong
    pub feedback: ValidationFeedback,
}

impl PlanLoop {
    /// Create a new PlanLoop from a LoopRecord.
    pub fn new(record: LoopRecord, config: HierarchyLoopConfig) -> Self {
        debug_assert!(record.loop_type == LoopType::Plan);
        Self {
            record,
            worktree: None,
            config,
            iteration_history: Vec::new(),
        }
    }

    /// Get the task description.
    pub fn task(&self) -> Option<&str> {
        self.record.context.get("task").and_then(|v| v.as_str())
    }

    /// Get the current review pass (1-5 for Rule of Five).
    pub fn review_pass(&self) -> u32 {
        self.record
            .context
            .get("review_pass")
            .and_then(|v| v.as_u64())
            .unwrap_or(1) as u32
    }

    /// Initialize the worktree.
    pub async fn init_worktree(&mut self) -> Result<(), LoopError> {
        if self.worktree.is_some() {
            return Ok(());
        }
        let worktree = Worktree::create(&self.record.id, self.config.worktree.clone()).await?;
        self.worktree = Some(worktree);
        Ok(())
    }

    /// Get the worktree path.
    pub fn worktree_path(&self) -> Option<&PathBuf> {
        self.worktree.as_ref().map(|w| &w.path)
    }

    /// Build the user prompt for the current iteration.
    fn build_user_prompt(&self) -> Result<String, LoopError> {
        let task = self.task().ok_or(LoopError::MissingTask)?;

        let mut prompt = format!("## Task\n\nCreate a detailed implementation plan for:\n\n{}\n\n", task);

        if !self.iteration_history.is_empty() {
            prompt.push_str("## Previous Iterations\n\n");
            for fb in &self.iteration_history {
                prompt.push_str(&format!("### Iteration {}\n", fb.iteration));
                prompt.push_str(&fb.feedback.format_for_prompt());
                prompt.push('\n');
            }
            prompt.push_str("**Fix the issues from the previous iteration(s).**\n\n");
        }

        Ok(prompt)
    }

    /// Build the completion request.
    pub fn build_request(&self, tools: Vec<ToolDefinition>) -> Result<CompletionRequest, LoopError> {
        let user_prompt = self.build_user_prompt()?;

        Ok(CompletionRequest {
            system_prompt: PLAN_SYSTEM_PROMPT.to_string(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text(user_prompt),
            }],
            tools,
            max_tokens: self.config.max_tokens,
        })
    }

    /// Record iteration feedback.
    pub fn record_feedback(&mut self, feedback: ValidationFeedback) {
        self.iteration_history.push(IterationFeedback {
            iteration: self.record.iteration,
            feedback,
        });
    }

    /// Parse specs from the produced plan artifact.
    pub fn parse_specs(&self, plan_content: &str) -> Vec<SpecDefinition> {
        parse_specs_from_plan(plan_content)
    }

    /// Validate the plan artifact.
    pub fn validate_artifact(&self, content: &str) -> Result<(), LoopError> {
        validate_plan_format(content).map_err(|e| LoopError::Store(e.to_string()))
    }

    /// Cleanup the worktree.
    pub async fn cleanup(self) -> Result<(), LoopError> {
        if let Some(worktree) = self.worktree {
            worktree.cleanup().await?;
        }
        Ok(())
    }
}

impl SpecLoop {
    /// Create a new SpecLoop from a LoopRecord.
    pub fn new(record: LoopRecord, config: HierarchyLoopConfig) -> Self {
        debug_assert!(record.loop_type == LoopType::Spec);
        Self {
            record,
            worktree: None,
            config,
            iteration_history: Vec::new(),
        }
    }

    /// Get the plan content that spawned this spec.
    pub fn plan_content(&self) -> Option<&str> {
        self.record.context.get("plan_content").and_then(|v| v.as_str())
    }

    /// Get the spec name (if available).
    pub fn spec_name(&self) -> Option<&str> {
        self.record.context.get("spec_name").and_then(|v| v.as_str())
    }

    /// Initialize the worktree.
    pub async fn init_worktree(&mut self) -> Result<(), LoopError> {
        if self.worktree.is_some() {
            return Ok(());
        }
        let worktree = Worktree::create(&self.record.id, self.config.worktree.clone()).await?;
        self.worktree = Some(worktree);
        Ok(())
    }

    /// Get the worktree path.
    pub fn worktree_path(&self) -> Option<&PathBuf> {
        self.worktree.as_ref().map(|w| &w.path)
    }

    /// Build the user prompt.
    fn build_user_prompt(&self) -> Result<String, LoopError> {
        let plan_content = self.plan_content().ok_or(LoopError::MissingTask)?;

        let mut prompt = format!(
            "## Context\n\nCreate a detailed spec based on this plan:\n\n{}\n\n",
            plan_content
        );

        if let Some(name) = self.spec_name() {
            prompt.push_str(&format!("Focus on the spec: **{}**\n\n", name));
        }

        if !self.iteration_history.is_empty() {
            prompt.push_str("## Previous Iterations\n\n");
            for fb in &self.iteration_history {
                prompt.push_str(&format!("### Iteration {}\n", fb.iteration));
                prompt.push_str(&fb.feedback.format_for_prompt());
                prompt.push('\n');
            }
            prompt.push_str("**Fix the issues from the previous iteration(s).**\n\n");
        }

        Ok(prompt)
    }

    /// Build the completion request.
    pub fn build_request(&self, tools: Vec<ToolDefinition>) -> Result<CompletionRequest, LoopError> {
        let user_prompt = self.build_user_prompt()?;

        Ok(CompletionRequest {
            system_prompt: SPEC_SYSTEM_PROMPT.to_string(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text(user_prompt),
            }],
            tools,
            max_tokens: self.config.max_tokens,
        })
    }

    /// Record iteration feedback.
    pub fn record_feedback(&mut self, feedback: ValidationFeedback) {
        self.iteration_history.push(IterationFeedback {
            iteration: self.record.iteration,
            feedback,
        });
    }

    /// Parse phases from the produced spec artifact.
    pub fn parse_phases(&self, spec_content: &str) -> Vec<PhaseDefinition> {
        parse_phases_from_spec(spec_content)
    }

    /// Validate the spec artifact.
    pub fn validate_artifact(&self, content: &str) -> Result<(), LoopError> {
        validate_spec_format(content).map_err(|e| LoopError::Store(e.to_string()))
    }

    /// Cleanup the worktree.
    pub async fn cleanup(self) -> Result<(), LoopError> {
        if let Some(worktree) = self.worktree {
            worktree.cleanup().await?;
        }
        Ok(())
    }
}

impl PhaseLoop {
    /// Create a new PhaseLoop from a LoopRecord.
    pub fn new(record: LoopRecord, config: HierarchyLoopConfig) -> Self {
        debug_assert!(record.loop_type == LoopType::Phase);
        Self {
            record,
            worktree: None,
            config,
            iteration_history: Vec::new(),
        }
    }

    /// Get the spec content that spawned this phase.
    pub fn spec_content(&self) -> Option<&str> {
        self.record.context.get("spec_content").and_then(|v| v.as_str())
    }

    /// Get the phase name.
    pub fn phase_name(&self) -> Option<&str> {
        self.record.context.get("phase_name").and_then(|v| v.as_str())
    }

    /// Get the phase number.
    pub fn phase_number(&self) -> u32 {
        self.record
            .context
            .get("phase_number")
            .and_then(|v| v.as_u64())
            .unwrap_or(1) as u32
    }

    /// Initialize the worktree.
    pub async fn init_worktree(&mut self) -> Result<(), LoopError> {
        if self.worktree.is_some() {
            return Ok(());
        }
        let worktree = Worktree::create(&self.record.id, self.config.worktree.clone()).await?;
        self.worktree = Some(worktree);
        Ok(())
    }

    /// Get the worktree path.
    pub fn worktree_path(&self) -> Option<&PathBuf> {
        self.worktree.as_ref().map(|w| &w.path)
    }

    /// Build the user prompt.
    fn build_user_prompt(&self) -> Result<String, LoopError> {
        let spec_content = self.spec_content().ok_or(LoopError::MissingTask)?;

        let mut prompt = format!(
            "## Context\n\nCreate a detailed phase.md based on this spec:\n\n{}\n\n",
            spec_content
        );

        if let Some(name) = self.phase_name() {
            prompt.push_str(&format!("Focus on Phase {}: **{}**\n\n", self.phase_number(), name));
        }

        if !self.iteration_history.is_empty() {
            prompt.push_str("## Previous Iterations\n\n");
            for fb in &self.iteration_history {
                prompt.push_str(&format!("### Iteration {}\n", fb.iteration));
                prompt.push_str(&fb.feedback.format_for_prompt());
                prompt.push('\n');
            }
            prompt.push_str("**Fix the issues from the previous iteration(s).**\n\n");
        }

        Ok(prompt)
    }

    /// Build the completion request.
    pub fn build_request(&self, tools: Vec<ToolDefinition>) -> Result<CompletionRequest, LoopError> {
        let user_prompt = self.build_user_prompt()?;

        Ok(CompletionRequest {
            system_prompt: PHASE_SYSTEM_PROMPT.to_string(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text(user_prompt),
            }],
            tools,
            max_tokens: self.config.max_tokens,
        })
    }

    /// Record iteration feedback.
    pub fn record_feedback(&mut self, feedback: ValidationFeedback) {
        self.iteration_history.push(IterationFeedback {
            iteration: self.record.iteration,
            feedback,
        });
    }

    /// Extract the task for a ralph loop from the phase artifact.
    pub fn extract_ralph_task(&self, phase_content: &str) -> String {
        extract_phase_goal(phase_content)
    }

    /// Validate the phase artifact.
    pub fn validate_artifact(&self, content: &str) -> Result<(), LoopError> {
        validate_phase_format(content).map_err(|e| LoopError::Store(e.to_string()))
    }

    /// Cleanup the worktree.
    pub async fn cleanup(self) -> Result<(), LoopError> {
        if let Some(worktree) = self.worktree {
            worktree.cleanup().await?;
        }
        Ok(())
    }
}

/// Spawn child loops from a completed parent loop's artifact.
///
/// This is the core "connective tissue" logic that links parent artifacts
/// to child loops.
pub fn spawn_children_from_artifact(
    parent: &LoopRecord,
    artifact_content: &str,
    artifact_path: &str,
) -> Vec<LoopRecord> {
    match parent.loop_type {
        LoopType::Plan => {
            let specs = parse_specs_from_plan(artifact_content);
            specs
                .into_iter()
                .map(|spec| {
                    let mut record = LoopRecord::new_spec(&parent.id, artifact_content, 10);
                    record.triggered_by = Some(artifact_path.to_string());
                    record.conversation_id = parent.conversation_id.clone();
                    // Add spec name to context
                    if let Some(ctx) = record.context.as_object_mut() {
                        ctx.insert("spec_name".to_string(), serde_json::json!(spec.name));
                        ctx.insert("spec_description".to_string(), serde_json::json!(spec.description));
                    }
                    record
                })
                .collect()
        }
        LoopType::Spec => {
            let phases = parse_phases_from_spec(artifact_content);
            let total = phases.len() as u32;
            phases
                .into_iter()
                .map(|phase| {
                    let mut record = LoopRecord::new_phase(
                        &parent.id,
                        artifact_content,
                        phase.number as u32,
                        &phase.name,
                        total,
                        10,
                    );
                    record.triggered_by = Some(artifact_path.to_string());
                    record.conversation_id = parent.conversation_id.clone();
                    // Add phase goal to context
                    if let Some(ctx) = record.context.as_object_mut() {
                        ctx.insert("phase_goal".to_string(), serde_json::json!(phase.goal));
                    }
                    record
                })
                .collect()
        }
        LoopType::Phase => {
            // Phases spawn exactly one ralph
            let task = extract_phase_goal(artifact_content);
            let mut record = LoopRecord::new_ralph_from_phase(&parent.id, artifact_content, &task, 50);
            record.triggered_by = Some(artifact_path.to_string());
            record.conversation_id = parent.conversation_id.clone();
            vec![record]
        }
        LoopType::Ralph => {
            // Ralphs don't spawn children
            vec![]
        }
    }
}

/// Save spawned children to the TaskStore.
pub fn save_spawned_children(store: &mut TaskStore, children: &[LoopRecord]) -> Result<(), LoopError> {
    for child in children {
        store.save(child).map_err(|e| LoopError::Store(e.to_string()))?;
    }
    Ok(())
}

/// Invalidate all children of a loop (when parent re-iterates).
pub fn invalidate_children(store: &mut TaskStore, parent_id: &str) -> Result<(), LoopError> {
    let children = store
        .list_children(parent_id)
        .map_err(|e| LoopError::Store(e.to_string()))?;

    for mut child in children {
        // Recursively invalidate grandchildren first
        invalidate_children(store, &child.id)?;

        // Then invalidate this child
        child.status = LoopStatus::Invalidated;
        child.touch();
        store.update(&child).map_err(|e| LoopError::Store(e.to_string()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_plan_record() -> LoopRecord {
        LoopRecord::new_plan("Build a REST API", 10)
    }

    fn make_spec_record(parent_id: &str) -> LoopRecord {
        LoopRecord::new_spec(parent_id, "# Plan content", 10)
    }

    fn make_phase_record(parent_id: &str) -> LoopRecord {
        LoopRecord::new_phase(parent_id, "# Spec content", 1, "User Model", 3, 10)
    }

    #[test]
    fn test_plan_loop_new() {
        let record = make_plan_record();
        let config = HierarchyLoopConfig::default();
        let plan_loop = PlanLoop::new(record, config);

        assert_eq!(plan_loop.task(), Some("Build a REST API"));
        assert_eq!(plan_loop.review_pass(), 1);
    }

    #[test]
    fn test_plan_loop_build_prompt() {
        let record = make_plan_record();
        let config = HierarchyLoopConfig::default();
        let plan_loop = PlanLoop::new(record, config);

        let prompt = plan_loop.build_user_prompt().unwrap();
        assert!(prompt.contains("Build a REST API"));
        assert!(!prompt.contains("Previous Iterations"));
    }

    #[test]
    fn test_plan_loop_build_prompt_with_history() {
        let record = make_plan_record();
        let config = HierarchyLoopConfig::default();
        let mut plan_loop = PlanLoop::new(record, config);

        plan_loop.record_feedback(ValidationFeedback::from_command_output(
            Some(1),
            String::new(),
            "Missing specs section".to_string(),
        ));

        let prompt = plan_loop.build_user_prompt().unwrap();
        assert!(prompt.contains("Previous Iterations"));
        assert!(prompt.contains("Missing specs section"));
    }

    #[test]
    fn test_spec_loop_new() {
        let record = make_spec_record("parent123");
        let config = HierarchyLoopConfig::default();
        let spec_loop = SpecLoop::new(record, config);

        assert!(spec_loop.plan_content().is_some());
    }

    #[test]
    fn test_spec_loop_build_prompt() {
        let mut record = make_spec_record("parent123");
        if let Some(ctx) = record.context.as_object_mut() {
            ctx.insert("spec_name".to_string(), serde_json::json!("Auth Module"));
        }
        let config = HierarchyLoopConfig::default();
        let spec_loop = SpecLoop::new(record, config);

        let prompt = spec_loop.build_user_prompt().unwrap();
        assert!(prompt.contains("Auth Module"));
    }

    #[test]
    fn test_phase_loop_new() {
        let record = make_phase_record("parent123");
        let config = HierarchyLoopConfig::default();
        let phase_loop = PhaseLoop::new(record, config);

        assert_eq!(phase_loop.phase_name(), Some("User Model"));
        assert_eq!(phase_loop.phase_number(), 1);
    }

    #[test]
    fn test_phase_loop_build_prompt() {
        let record = make_phase_record("parent123");
        let config = HierarchyLoopConfig::default();
        let phase_loop = PhaseLoop::new(record, config);

        let prompt = phase_loop.build_user_prompt().unwrap();
        assert!(prompt.contains("Phase 1"));
        assert!(prompt.contains("User Model"));
    }

    #[test]
    fn test_spawn_children_from_plan() {
        let parent = make_plan_record();
        let plan_content = r#"# Plan: Test
## Summary
Test plan
## Goals
- Goal
## Specs
### Spec 1: Auth
Auth module
**Scope:**
- User login
### Spec 2: API
API module
**Scope:**
- REST endpoints
"#;

        let children = spawn_children_from_artifact(&parent, plan_content, "plan.md");

        assert_eq!(children.len(), 2);
        assert_eq!(children[0].loop_type, LoopType::Spec);
        assert_eq!(children[0].parent_loop, Some(parent.id.clone()));
        assert_eq!(children[0].triggered_by, Some("plan.md".to_string()));
    }

    #[test]
    fn test_spawn_children_from_spec() {
        let mut parent = make_spec_record("plan123");
        parent.id = "spec123".to_string();

        let spec_content = r#"# Spec: Auth
## Overview
Auth module
## Requirements
### Functional Requirements
1. FR1
## Phases
### Phase 1: Setup
**Goal:** Set up the project
**Tasks:**
- Create files
**Validation:** cargo test
### Phase 2: Implement
**Goal:** Implement the feature
**Tasks:**
- Write code
**Validation:** cargo test
"#;

        let children = spawn_children_from_artifact(&parent, spec_content, "spec.md");

        assert_eq!(children.len(), 2);
        assert_eq!(children[0].loop_type, LoopType::Phase);
        assert_eq!(children[0].parent_loop, Some("spec123".to_string()));
    }

    #[test]
    fn test_spawn_children_from_phase() {
        let mut parent = make_phase_record("spec123");
        parent.id = "phase123".to_string();

        let phase_content = r#"# Phase: Setup
## Goal
Create the project structure
## Tasks
1. Create files
"#;

        let children = spawn_children_from_artifact(&parent, phase_content, "phase.md");

        assert_eq!(children.len(), 1);
        assert_eq!(children[0].loop_type, LoopType::Ralph);
        assert_eq!(children[0].parent_loop, Some("phase123".to_string()));
    }

    #[test]
    fn test_spawn_children_from_ralph() {
        let mut parent = LoopRecord::new_ralph("task", 5);
        parent.id = "ralph123".to_string();

        let children = spawn_children_from_artifact(&parent, "code content", "code.rs");

        assert!(children.is_empty()); // Ralphs don't spawn children
    }

    #[test]
    fn test_invalidate_children() {
        let temp_dir = TempDir::new().unwrap();
        let mut store = TaskStore::open_at(temp_dir.path()).unwrap();

        // Create a hierarchy
        let mut parent = make_plan_record();
        parent.id = "parent".to_string();
        store.save(&parent).unwrap();

        let mut child = make_spec_record("parent");
        child.id = "child".to_string();
        child.status = LoopStatus::Running;
        store.save(&child).unwrap();

        let mut grandchild = make_phase_record("child");
        grandchild.id = "grandchild".to_string();
        grandchild.status = LoopStatus::Pending;
        store.save(&grandchild).unwrap();

        // Invalidate children
        invalidate_children(&mut store, "parent").unwrap();

        // Check statuses
        let child = store.get("child").unwrap().unwrap();
        assert_eq!(child.status, LoopStatus::Invalidated);

        let grandchild = store.get("grandchild").unwrap().unwrap();
        assert_eq!(grandchild.status, LoopStatus::Invalidated);
    }

    #[test]
    fn test_hierarchy_loop_config_default() {
        let config = HierarchyLoopConfig::default();
        assert_eq!(config.max_tokens, 16384);
    }

    #[test]
    fn test_plan_loop_validate_artifact() {
        let record = make_plan_record();
        let config = HierarchyLoopConfig::default();
        let plan_loop = PlanLoop::new(record, config);

        // Valid plan
        let valid_plan = r#"# Plan: Test

## Summary

A test plan for building something useful with enough content to pass validation.

## Goals

- Build something
- Test it

## Non-Goals

- Nothing else

## Proposed Solution

### Overview

We will build it properly with good architecture and design principles applied throughout.

## Specs

### Spec 1: Core

Core functionality implementation

**Scope:**
- Item 1
- Item 2

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Failure | Low | High | Handle it |
"#;

        assert!(plan_loop.validate_artifact(valid_plan).is_ok());

        // Invalid plan (missing sections)
        let invalid_plan = "# Plan: Test\n## Summary\nTest";
        assert!(plan_loop.validate_artifact(invalid_plan).is_err());
    }

    #[test]
    fn test_spec_loop_validate_artifact() {
        let record = make_spec_record("parent");
        let config = HierarchyLoopConfig::default();
        let spec_loop = SpecLoop::new(record, config);

        let valid_spec = r#"# Spec: Test

**Parent Plan:** Test Plan
**Spec Number:** 1 of 1

## Overview

A detailed specification for testing purposes with enough content to pass validation checks.

## Requirements

### Functional Requirements

1. **FR1**: Do something useful

### Non-Functional Requirements

1. **NFR1**: Be fast

## Acceptance Criteria

- [ ] Works correctly

## Phases

### Phase 1: Setup

**Goal:** Set up the project structure and dependencies

**Tasks:**
- Create files
- Configure things

**Validation:** cargo test

## Technical Notes

- Use good practices
"#;

        assert!(spec_loop.validate_artifact(valid_spec).is_ok());
    }

    #[test]
    fn test_phase_loop_validate_artifact() {
        let record = make_phase_record("parent");
        let config = HierarchyLoopConfig::default();
        let phase_loop = PhaseLoop::new(record, config);

        let valid_phase = r#"# Phase: Setup

**Parent Spec:** Test Spec
**Phase Number:** 1 of 3

## Goal

Set up the project structure and configuration files for the implementation.

## Context

This is the first phase.

## Tasks

1. Create the directory structure
2. Add configuration files
3. Set up dependencies

## Files to Modify

- `src/main.rs` - Entry point
- `Cargo.toml` - Dependencies

## Acceptance Criteria

- [ ] Project compiles

## Validation Command

```bash
cargo build
```

## Notes

- Keep it simple
"#;

        assert!(phase_loop.validate_artifact(valid_phase).is_ok());
    }

    #[test]
    fn test_iteration_feedback_recording() {
        let record = make_plan_record();
        let config = HierarchyLoopConfig::default();
        let mut plan_loop = PlanLoop::new(record, config);

        plan_loop.record.iteration = 1;
        plan_loop.record_feedback(ValidationFeedback::timeout());

        plan_loop.record.iteration = 2;
        plan_loop.record_feedback(ValidationFeedback::from_command_output(
            Some(1),
            String::new(),
            "Error".to_string(),
        ));

        assert_eq!(plan_loop.iteration_history.len(), 2);
        assert_eq!(plan_loop.iteration_history[0].iteration, 1);
        assert_eq!(plan_loop.iteration_history[1].iteration, 2);
    }
}
