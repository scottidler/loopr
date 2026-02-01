# Rule of Five: Plan Quality Methodology

**Author:** Scott A. Idler
**Date:** 2026-01-30
**Status:** Implementation Spec
**Based on:** loopr/docs/rule-of-five.md

---

## Summary

The Rule of Five is a 5-pass review process for plan creation. Each pass focuses on different aspects, ensuring plans are complete, correct, and actionable before spawning specs.

---

## Why Five Passes?

Single-pass planning often produces:
- Missing sections
- Vague requirements
- Unrealistic scope
- Unaddressed edge cases
- Poor decomposition

Five passes with distinct focuses catch these issues before they cascade into implementation.

---

## The Five Passes

### Pass 1: Initial Draft

**Focus:** Get something down. Don't overthink.

**Prompt addition:**
```
Create an initial plan for this task. Include:
- Summary of what we're building
- High-level approach
- List of specs (major components)

Don't worry about being perfect. We'll refine in subsequent passes.
```

**Validation:** Plan.md exists with Summary and Specs sections.

### Pass 2: Completeness Review

**Focus:** What's missing?

**Prompt addition:**
```
Review the plan for completeness:
- Are all user requirements addressed?
- Are there missing specs?
- Are there implicit requirements not captured?
- What about error handling, edge cases?

Add any missing sections or specs.
```

**Validation:** All requirements from original task are addressed.

### Pass 3: Feasibility Check

**Focus:** Can we actually build this?

**Prompt addition:**
```
Review the plan for feasibility:
- Are any specs too large? (Should be split)
- Are any specs too vague? (Need more detail)
- Are there technical blockers?
- Is the scope realistic?

Adjust specs as needed.
```

**Validation:** Each spec is implementable in a reasonable number of phases.

### Pass 4: Ordering and Dependencies

**Focus:** What order should things happen?

**Prompt addition:**
```
Review spec ordering and dependencies:
- Which specs must complete before others?
- Are there opportunities for parallelism?
- Is the critical path clear?

Reorder specs if needed and note dependencies.
```

**Validation:** Dependencies are explicit. No circular dependencies.

### Pass 5: Final Review

**Focus:** Would you approve this plan?

**Prompt addition:**
```
Final review. This plan will spawn specs that will be implemented.
- Is this plan clear enough for another developer to follow?
- Are there any remaining concerns?
- If you had to implement this, would you feel confident?

Make any final adjustments.
```

**Validation:** LLM-as-judge approval (Layer 3 validation).

---

## Implementation

The Rule of Five is implemented in `Loop` for Plan-type loops. There is no separate `PlanLoop` struct - all loops use the unified `Loop` struct with behavior determined by `LoopConfig`. The Loop is self-contained and runs itself.

```rust
impl Loop {
    /// Build prompt for Plan loop with Rule of Five pass injection
    fn build_plan_prompt(&self) -> Result<String> {
        let task = self.context["task"].as_str().unwrap_or("");
        let plan_content = self.read_current_artifact()?;
        let review_pass = self.context["review_pass"].as_u64().unwrap_or(1);

        let base = format!(
            r#"# Task
{}

# Current Plan
{}
"#,
            task,
            plan_content
        );

        let pass_prompt = match review_pass {
            1 => PASS_1_PROMPT,
            2 => PASS_2_PROMPT,
            3 => PASS_3_PROMPT,
            4 => PASS_4_PROMPT,
            5 => PASS_5_PROMPT,
            _ => PASS_5_PROMPT,
        };

        Ok(format!("{}\n\n{}", base, pass_prompt))
    }

    fn handle_plan_validation(&mut self, result: ValidationResult) -> LoopAction {
        let review_pass = self.context["review_pass"].as_u64().unwrap_or(1);

        if result.passed {
            if review_pass < 5 {
                // Move to next pass
                self.context["review_pass"] = json!(review_pass + 1);
                LoopAction::Continue
            } else {
                // All 5 passes complete
                LoopAction::Complete
            }
        } else {
            // Validation failed - retry current pass
            self.iteration += 1;
            if self.iteration >= self.max_iterations {
                LoopAction::Fail("Max iterations on plan review".into())
            } else {
                // Add feedback to progress
                self.progress.push_str(&format!(
                    "\nPass {} failed: {}",
                    review_pass,
                    result.feedback
                ));
                LoopAction::Continue
            }
        }
    }
}
```

---

## Plan Artifact Format

```markdown
# Plan: <task summary>

## Summary
<1-2 paragraph overview of what we're building>

## Requirements
- <explicit requirement from user>
- <inferred requirement>

## Approach
<High-level technical approach>

## Specs

### Spec 1: <name>
**Description:** <what this spec accomplishes>
**Dependencies:** <none or list of other specs>

### Spec 2: <name>
**Description:** <what this spec accomplishes>
**Dependencies:** Spec 1

...

## Notes
<Any additional context, concerns, or decisions>
```

---

## Configuration

```yaml
# loopr.yml
plan:
  max_iterations_per_pass: 10
  total_max_iterations: 50

  # Enable/disable passes
  passes:
    - draft
    - completeness
    - feasibility
    - ordering
    - review

  # LLM for review (can use cheaper model)
  review_model: "claude-3-haiku"
```

---

## Skipping Passes

For simple tasks, passes can be collapsed:

```rust
fn should_skip_pass(task: &str, pass: u32) -> bool {
    let task_lower = task.to_lowercase();

    // Simple bug fixes can skip feasibility/ordering
    if task_lower.contains("fix") && task_lower.contains("bug") {
        return pass == 3 || pass == 4;
    }

    // Single-file changes can skip most passes
    if task_lower.contains("update") && task_lower.contains("file") {
        return pass > 1 && pass < 5;
    }

    false
}
```

---

## References

- [loop-architecture.md](loop-architecture.md) - Loop hierarchy
- [loop-validation.md](loop-validation.md) - Validation layers
- [domain-types.md](domain-types.md) - Loop struct and LoopConfig
