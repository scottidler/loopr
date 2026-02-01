# Loop Validation: 3-Layer Backpressure

**Author:** Scott A. Idler
**Date:** 2026-01-30
**Status:** Implementation Spec
**Based on:** loopr/docs/loop-validation.md

---

## Summary

Validation determines whether a loop iteration succeeded. Three layers provide increasing scrutiny: format checks, test execution, and LLM-as-judge review. Failed validation triggers re-iteration with feedback.

---

## Design Principle

**Backpressure:** When validation fails, the loop doesn't proceed. Instead, it re-iterates with the failure feedback incorporated into the prompt. This creates pressure toward quality.

**Fresh Context:** Each iteration starts with a fresh LLM context. The prompt carries forward what failed and why, not the full conversation history.

---

## Three Validation Layers

### Layer 1: Format/Syntax Checks

Fast, cheap checks that catch obvious problems.

| Check | Applies To | How |
|-------|------------|-----|
| File exists | All artifacts | `path.exists()` |
| Valid markdown | plan.md, spec.md | Parse headers present |
| Valid JSON | config files | `serde_json::from_str` |
| Code compiles | Rust/Go code | `cargo check` / `go build` |
| Lint passes | All code | `cargo clippy` / `eslint` |

```rust
async fn layer1_validate(loop_type: LoopType, worktree: &Path) -> ValidationResult {
    match loop_type {
        LoopType::Plan => {
            // Check plan.md exists and has required sections
            let plan_path = worktree.join("plan.md");
            if !plan_path.exists() {
                return ValidationResult::fail("plan.md not found");
            }
            let content = fs::read_to_string(&plan_path)?;
            if !content.contains("## Summary") {
                return ValidationResult::fail("plan.md missing ## Summary");
            }
            if !content.contains("## Specs") {
                return ValidationResult::fail("plan.md missing ## Specs");
            }
            ValidationResult::pass()
        }
        LoopType::Code => {
            // Check code compiles
            let output = Command::new("cargo")
                .args(["check"])
                .current_dir(worktree)
                .output()
                .await?;
            if !output.status.success() {
                return ValidationResult::fail(format!(
                    "Compile error:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
            ValidationResult::pass()
        }
        _ => ValidationResult::pass(),
    }
}
```

### Layer 2: Test Execution

Run tests to verify behavior.

```rust
async fn layer2_validate(config: &LoopConfig, worktree: &Path) -> ValidationResult {
    let command = &config.validation_command; // e.g., "cargo test" or "otto ci"

    let output = Command::new("sh")
        .args(["-c", command])
        .current_dir(worktree)
        .output()
        .await?;

    if output.status.success() {
        ValidationResult::pass()
    } else {
        ValidationResult::fail(format!(
            "Tests failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}
```

### Layer 3: LLM-as-Judge

Use a separate LLM call to review the work.

```rust
async fn layer3_validate(
    loop_impl: &dyn Loop,
    worktree: &Path,
    llm_client: &dyn LlmClient,
) -> ValidationResult {
    // Build review prompt
    let artifacts = loop_impl.artifacts();
    let artifact_content = read_artifacts(&artifacts, worktree)?;

    let prompt = format!(
        r#"Review this work and determine if it meets the requirements.

TASK:
{}

ARTIFACTS PRODUCED:
{}

Does this work:
1. Address the stated requirements?
2. Follow best practices?
3. Have any obvious issues?

Respond with either:
APPROVED: <brief explanation>
REJECTED: <what needs to be fixed>
"#,
        loop_impl.task(),
        artifact_content
    );

    let response = llm_client.complete(&prompt).await?;

    if response.contains("APPROVED:") {
        ValidationResult::pass()
    } else if response.contains("REJECTED:") {
        let feedback = response.split("REJECTED:").nth(1).unwrap_or("");
        ValidationResult::fail(feedback.trim().to_string())
    } else {
        // Unclear response, treat as needs review
        ValidationResult::fail("Review inconclusive, please improve")
    }
}
```

---

## Validation Pipeline

```rust
async fn validate_iteration(
    loop_impl: &dyn Loop,
    worktree: &Path,
    config: &LoopConfig,
    llm_client: &dyn LlmClient,
) -> ValidationResult {
    // Layer 1: Format checks (always run)
    let result = layer1_validate(loop_impl.loop_type(), worktree).await?;
    if !result.passed {
        return result;
    }

    // Layer 2: Tests (if configured)
    if config.run_tests {
        let result = layer2_validate(config, worktree).await?;
        if !result.passed {
            return result;
        }
    }

    // Layer 3: LLM review (for higher-level loops)
    if matches!(loop_impl.loop_type(), LoopType::Plan | LoopType::Spec) {
        let result = layer3_validate(loop_impl, worktree, llm_client).await?;
        if !result.passed {
            return result;
        }
    }

    ValidationResult::pass()
}
```

---

## Feedback Incorporation

When validation fails, the feedback is incorporated into the next iteration's prompt. There is no separate `CodeLoop` struct - all loops use the unified `Loop` struct with behavior determined by `LoopConfig`. The Loop is self-contained and runs itself.

```rust
impl Loop {
    fn build_prompt(&self) -> Result<String> {
        let template = fs::read_to_string(&self.prompt_path)?;
        let mut prompt = render_template(&template, &self.context)?;

        if self.iteration > 0 && !self.progress.is_empty() {
            prompt.push_str(&format!(
                r#"

## Previous Attempts

{}

Please address these issues in this attempt.
"#,
                self.progress
            ));
        }

        Ok(prompt)
    }

    fn update_progress(&mut self, result: &ValidationResult) {
        let entry = format!(
            "Iteration {}: {}",
            self.iteration,
            result.feedback
        );
        self.progress.push_str(&entry);
        self.progress.push('\n');
    }
}
```

---

## Configuration

```yaml
# loopr.yml
validation:
  # Layer 2 test command
  command: "cargo test"

  # Which layers to run
  layers:
    format: true      # Layer 1
    tests: true       # Layer 2
    llm_review: true  # Layer 3

  # Skip Layer 3 for code loops (too slow)
  llm_review_types:
    - plan
    - spec

  # Retry settings
  max_iterations: 50
```

---

## Loop-Type Specific Validation

| Loop Type | Layer 1 | Layer 2 | Layer 3 |
|-----------|---------|---------|---------|
| Plan | Markdown structure | N/A | LLM review |
| Spec | Markdown structure | N/A | LLM review |
| Phase | Code compiles | Tests pass | Optional |
| Code | Code compiles | Tests pass | N/A |

---

## ValidationResult

```rust
pub struct ValidationResult {
    pub passed: bool,
    pub feedback: String,
    pub layer: ValidationLayer,
}

pub enum ValidationLayer {
    Format,
    Tests,
    LlmReview,
}

impl ValidationResult {
    pub fn pass() -> Self {
        Self {
            passed: true,
            feedback: String::new(),
            layer: ValidationLayer::Format,
        }
    }

    pub fn fail(feedback: impl Into<String>) -> Self {
        Self {
            passed: false,
            feedback: feedback.into(),
            layer: ValidationLayer::Format,
        }
    }
}
```

---

## References

- [loop-architecture.md](loop-architecture.md) - Loop hierarchy
- [rule-of-five.md](rule-of-five.md) - Plan validation
- [execution-model.md](execution-model.md) - Iteration flow
