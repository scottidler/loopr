# Design Document: Loop Validation

**Author:** Scott Idler, Claude
**Date:** 2026-01-25
**Status:** Implementation Spec
**Parent Doc:** [loop-architecture.md](loop-architecture.md)

## Summary

Validation determines whether a loop iteration succeeded or needs retry. Loopr uses three complementary backpressure mechanisms: downstream gates (tests, linting, type-checking), upstream steering (code patterns guide the LLM), and LLM-as-judge (for subjective criteria). When validation fails, feedback is incorporated into the next iteration's prompt—the core Ralph Wiggum insight.

## Problem Statement

### Background

The Ralph Wiggum technique iterates until validation passes:

```bash
while :; do cat PROMPT.md | claude ; done
```

But what determines "pass"? Geoffrey Huntley identifies three backpressure layers:

1. **Downstream gates** - Tests, type-checking, linting fail automatically
2. **Upstream steering** - Existing code patterns guide behavior through discovery
3. **LLM-as-judge** - For subjective criteria, use binary pass/fail LLM reviews

The key insight: "Human roles shift from 'telling the agent what to do' to 'engineering conditions where good outcomes emerge naturally through iteration.'"

### Problem

1. **What validates?** Each loop type needs different validation logic—Plan validation differs from Ralph validation.

2. **How to fail constructively?** A bare "failed" doesn't help the next iteration. Validation must produce actionable feedback.

3. **Subjective criteria?** How do you validate "good documentation" or "clean architecture" when there's no test to run?

4. **Feedback incorporation?** How does validation output become part of the next prompt?

### Goals

1. **Define validation per loop type** - What passes/fails at each hierarchy level
2. **Three-layer backpressure** - Support downstream gates, upstream steering, LLM-as-judge
3. **Structured feedback** - Validation produces feedback the LLM can act on
4. **Prompt evolution** - Failed validation feedback automatically incorporated into next iteration

### Non-Goals

1. **Validation authoring** - How to write good tests/prompts (that's prompt engineering)
2. **Custom validators** - Plugin architecture for validation (future work)
3. **Continuous validation** - Mid-iteration validation checkpoints

## Proposed Solution

### Overview

Each iteration ends with validation:

```
1. LLM completes its work (tools exhausted or explicit "done" signal)
2. Validation runs (command execution and/or LLM-as-judge)
3. If pass → iteration complete, loop may complete
4. If fail → capture feedback, increment iteration, update prompt
```

Validation is the critical handoff between iterations.

### The Three Backpressure Layers

#### Layer 1: Downstream Gates

Automated checks that fail fast on objective criteria:

| Gate | What It Catches | Exit Code |
|------|-----------------|-----------|
| `cargo test` | Broken functionality | Non-zero |
| `cargo clippy` | Code quality issues | Non-zero |
| `cargo fmt --check` | Style violations | Non-zero |
| `tsc --noEmit` | Type errors | Non-zero |
| `otto ci` | All of the above | Non-zero |

**Implementation:** Configured via `validation_command` in loop config.

```yaml
# loop-config.yml
validation:
  command: "otto ci"
  success_exit_code: 0
```

**Feedback extraction:** Capture stdout/stderr as validation feedback.

```rust
fn run_downstream_gate(&self) -> ValidationResult {
    let output = Command::new("sh")
        .args(["-c", &self.config.validation_command])
        .output()?;

    if output.status.code() == Some(self.config.success_exit_code) {
        ValidationResult::Pass
    } else {
        ValidationResult::Fail {
            feedback: String::from_utf8_lossy(&output.stderr).to_string(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        }
    }
}
```

#### Layer 2: Upstream Steering

Existing code patterns guide LLM behavior through discovery. This isn't a validation step—it's how the prompt is constructed to reduce validation failures.

**Examples:**
- "Follow the existing auth pattern in `src/auth/`"
- "Use the error handling convention from `src/errors.rs`"
- "Match the test structure in `tests/integration/`"

**Implementation:** The prompt template references existing code:

```handlebars
## Existing Patterns

{{#each discovered_patterns}}
- {{this.description}}: see `{{this.file}}`
{{/each}}

Follow these patterns in your implementation.
```

Upstream steering reduces iteration count by guiding the LLM toward patterns that will pass downstream gates.

#### Layer 3: LLM-as-Judge

For subjective criteria that can't be tested programmatically:

| Criteria | Why LLM-as-Judge |
|----------|------------------|
| Documentation quality | "Is this clear?" has no test |
| API design | "Is this ergonomic?" is subjective |
| Error messages | "Is this helpful?" needs judgment |
| Code organization | "Is this well-structured?" is fuzzy |

**Implementation:** A separate LLM call with a judge prompt:

```rust
fn run_llm_judge(&self, artifacts: &[Artifact]) -> ValidationResult {
    let judge_prompt = format!(
        "You are a code reviewer. Evaluate the following for: {}\n\n{}\n\n\
         Respond with exactly 'PASS' or 'FAIL: <reason>'",
        self.config.judge_criteria,
        artifacts.iter().map(|a| a.content.clone()).collect::<Vec<_>>().join("\n---\n")
    );

    let response = self.llm.complete(&judge_prompt)?;

    if response.trim().starts_with("PASS") {
        ValidationResult::Pass
    } else {
        ValidationResult::Fail {
            feedback: response.trim_start_matches("FAIL:").trim().to_string(),
            stdout: String::new(),
        }
    }
}
```

**Key principle:** Binary pass/fail only. No "mostly good" or scores—the LLM must make a decision.

**Judge prompt engineering:**
- Be specific about criteria
- Provide examples of pass/fail
- Request actionable feedback on failure

### Per-Loop-Type Validation

Each loop type has different validation needs:

#### PlanLoop Validation

**What it produces:** `plan.md` artifacts (high-level roadmap)

**Validation criteria:**
1. Plan addresses all requirements from the original request
2. Plan is decomposable into specs
3. No obvious technical impossibilities

**Primary validator:** LLM-as-judge (plans are subjective)

```yaml
# plan loop type config
validation:
  type: llm-judge
  criteria: |
    Evaluate this plan for completeness:
    1. Does it address all stated requirements?
    2. Can it be decomposed into 1-2 concrete specs?
    3. Are there any obvious blockers or impossibilities?
```

#### SpecLoop Validation

**What it produces:** `spec.md` artifacts (detailed requirements)

**Validation criteria:**
1. Spec is implementable (concrete, not vague)
2. Spec has clear acceptance criteria
3. Spec can be decomposed into 3-7 phases

**Primary validator:** LLM-as-judge with structure checks

```yaml
validation:
  type: composite
  gates:
    - type: structure
      required_sections: [Overview, Requirements, Acceptance Criteria, Phases]
    - type: llm-judge
      criteria: |
        Is this spec implementable?
        - Are requirements concrete (not "make it good")?
        - Are acceptance criteria testable?
        - Is the phase breakdown reasonable?
```

#### PhaseLoop Validation

**What it produces:** `phase.md` + code files

**Validation criteria:**
1. All acceptance criteria from parent spec are met (for this phase)
2. Code compiles and passes tests
3. Changes are coherent (not scattered unrelated modifications)

**Primary validator:** Downstream gates + LLM-as-judge

```yaml
validation:
  type: composite
  gates:
    - type: command
      command: "otto ci"
      success_exit_code: 0
    - type: llm-judge
      criteria: |
        Does this implementation satisfy the phase requirements?
        Review against: {{phase_requirements}}
```

#### RalphLoop Validation

**What it produces:** Code files (implementation work)

**Validation criteria:**
1. Tests pass
2. Lint clean
3. Type check passes
4. Specific task criteria met

**Primary validator:** Downstream gates (tests/lint/types)

```yaml
validation:
  type: command
  command: "otto ci"
  success_exit_code: 0
```

RalphLoops are the workhorses—they have the tightest validation because they do the actual implementation.

### Feedback Incorporation

When validation fails, the feedback becomes part of the next iteration's prompt.

#### Prompt Evolution

```
Iteration 1 prompt:
  "Implement the JWT token service..."

Iteration 2 prompt:
  "Implement the JWT token service...

   PREVIOUS ITERATION FAILED:
   - Test failure: test_token_expiry expected 3600s, got 0s
   - Clippy: unused variable `claims` in jwt.rs:42

   Address these issues in your implementation."

Iteration 3 prompt:
  "Implement the JWT token service...

   ITERATION HISTORY:
   - Iteration 1: test_token_expiry failed, unused variable
   - Iteration 2: test_token_expiry fixed, new failure in test_refresh_token

   Current failure:
   - Test failure: test_refresh_token - refresh token not persisted

   Address this issue."
```

#### Feedback Structure

```rust
pub struct IterationFeedback {
    pub iteration: u32,
    pub validation_type: String,  // "command", "llm-judge", "composite"
    pub passed: bool,
    pub failures: Vec<FailureDetail>,
    pub timestamp: DateTime<Utc>,
}

pub struct FailureDetail {
    pub category: String,      // "test", "lint", "type", "judge"
    pub message: String,       // Human-readable failure
    pub file: Option<String>,  // File involved, if applicable
    pub line: Option<u32>,     // Line number, if applicable
}
```

#### Feedback in Prompt Template

```handlebars
{{#if iteration_history}}
## Previous Iteration Results

{{#each iteration_history}}
### Iteration {{this.iteration}}
{{#each this.failures}}
- **{{this.category}}**: {{this.message}}
  {{#if this.file}}({{this.file}}{{#if this.line}}:{{this.line}}{{/if}}){{/if}}
{{/each}}
{{/each}}

**Focus on fixing the most recent failures first.**
{{/if}}
```

### Validation Pipeline

Composite validation runs gates in order, failing fast:

```rust
fn validate(&self, iteration: &Iteration) -> ValidationResult {
    let mut all_feedback = Vec::new();

    for gate in &self.config.gates {
        let result = match gate {
            Gate::Command { command, exit_code } => {
                self.run_command_gate(command, *exit_code)
            }
            Gate::LlmJudge { criteria } => {
                self.run_llm_judge(criteria, &iteration.artifacts)
            }
            Gate::Structure { required_sections } => {
                self.check_structure(required_sections, &iteration.artifacts)
            }
        };

        match result {
            ValidationResult::Pass => continue,
            ValidationResult::Fail { feedback, .. } => {
                // Fail fast: return on first failure
                return ValidationResult::Fail {
                    feedback,
                    gate: gate.name(),
                };
            }
        }
    }

    ValidationResult::Pass
}
```

**Fail-fast rationale:** If tests fail, no point running LLM-as-judge on broken code.

### Validation Timeouts

Each validation step has a timeout:

```yaml
validation:
  command: "otto ci"
  timeout_ms: 300000  # 5 minutes for full CI

  # Per-gate timeouts in composite
  gates:
    - type: command
      command: "cargo test"
      timeout_ms: 120000  # 2 minutes
    - type: llm-judge
      timeout_ms: 60000   # 1 minute
```

Timeout exceeded → validation fails with "timeout" feedback.

### Validation Signals

On validation failure, the loop emits a signal for coordination:

```rust
if !validation.passed {
    self.emit_signal(Signal::ValidationFailed {
        loop_id: self.id.clone(),
        iteration: self.iteration,
        feedback: validation.feedback.clone(),
    });
}
```

Parent loops can observe these signals to make decisions (e.g., "child has failed 5 times, maybe re-iterate the spec").

## Alternatives Considered

### Alternative 1: Continuous Validation (During Iteration)

**Description:** Run validation after each tool call, not just at iteration end.

**Pros:**
- Catch failures earlier
- Shorter feedback loops

**Cons:**
- Interrupts LLM flow
- Many partial states won't validate
- Adds latency to every tool call

**Why not chosen:** LLMs work better when allowed to complete a coherent chunk of work. Mid-iteration validation creates thrash.

### Alternative 2: Scoring Instead of Pass/Fail

**Description:** LLM-as-judge returns a score (1-10) instead of binary.

**Pros:**
- More nuanced feedback
- Can set threshold per loop type

**Cons:**
- Scores are inconsistent across runs
- "7/10" doesn't tell you what to fix
- Threshold tuning is arbitrary

**Why not chosen:** Binary decisions force actionable feedback. "FAIL: missing error handling in auth.rs" is better than "6/10".

### Alternative 3: Human-in-the-Loop Validation

**Description:** Pause for human approval at certain loop types.

**Pros:**
- Human judgment for critical decisions
- Catches issues LLM misses

**Cons:**
- Breaks autonomous operation
- Latency of human availability
- Doesn't scale

**Why not chosen:** Goal is autonomous operation. Human involvement is at conversation start (requirements) and end (review), not mid-loop.

## Technical Considerations

### Dependencies

- **Loop config** - Validation settings from [loop-config.md](loop-config.md)
- **Loop coordination** - Validation signals per [loop-coordination.md](loop-coordination.md)
- **LLM client** - For LLM-as-judge calls

### Performance

- Command validation: depends on test suite (typically 30s-5min)
- LLM-as-judge: single API call (~5-30s)
- Structure validation: O(artifact size), typically <100ms

### Testing Strategy

- Unit tests for feedback parsing
- Integration tests for command execution
- Mock LLM tests for judge validation
- End-to-end tests for prompt evolution

### Observability

Log validation results for debugging:

```rust
info!(
    loop_id = %self.id,
    iteration = self.iteration,
    validation_type = %result.validation_type,
    passed = result.passed,
    duration_ms = elapsed.as_millis(),
    "Validation completed"
);
```

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| LLM-as-judge inconsistent | Medium | Medium | Clear criteria, binary output, retry on ambiguous |
| Test flakiness causes spurious failures | Medium | High | Retry flaky tests, track flake rate, exclude known flakes |
| Feedback too long for context | Low | Medium | Truncate with "see full log at..." |
| Validation command hangs | Low | High | Strict timeouts, kill after timeout |
| Judge prompt injection | Low | Medium | Sanitize artifacts before judge prompt |

## Open Questions

1. **Feedback summarization?** - Should long feedback be summarized by an LLM before prompt inclusion?
2. **Retry budget for flaky tests?** - How many retries before treating flake as real failure?

## Future Work

1. **Custom validators** - Plugin architecture for domain-specific validation
2. **Validation caching** - Skip re-running tests if relevant files unchanged
3. **Feedback deduplication** - Don't repeat same failure across iterations
4. **Validation analytics** - Track which validators fail most, iteration counts by loop type

## References

- [loop-architecture.md](loop-architecture.md) - Parent design document
- [loop-config.md](loop-config.md) - Validation configuration
- [loop-coordination.md](loop-coordination.md) - Validation signals
- [Ralph Wiggum technique](https://ghuntley.com/ralph/) - Backpressure mechanisms
