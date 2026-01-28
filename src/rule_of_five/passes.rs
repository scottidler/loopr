//! Pass-specific prompts for the Rule of Five review process.
//!
//! Each pass focuses on a single quality dimension, allowing for thorough
//! and focused review of Plan documents.

use std::fmt;

/// The five review passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReviewPass {
    /// Pass 1: Is anything missing?
    Completeness = 1,
    /// Pass 2: Is anything wrong?
    Correctness = 2,
    /// Pass 3: What could go wrong?
    EdgeCases = 3,
    /// Pass 4: Does this fit the larger system?
    Architecture = 4,
    /// Pass 5: Can someone implement this unambiguously?
    Clarity = 5,
}

impl ReviewPass {
    /// Get the pass from a 1-indexed number.
    pub fn from_number(n: u32) -> Option<Self> {
        match n {
            1 => Some(Self::Completeness),
            2 => Some(Self::Correctness),
            3 => Some(Self::EdgeCases),
            4 => Some(Self::Architecture),
            5 => Some(Self::Clarity),
            _ => None,
        }
    }

    /// Get the 1-indexed number for this pass.
    pub fn number(&self) -> u32 {
        *self as u32
    }

    /// Get the name of this pass.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Completeness => "Completeness",
            Self::Correctness => "Correctness",
            Self::EdgeCases => "Edge Cases",
            Self::Architecture => "Architecture",
            Self::Clarity => "Clarity",
        }
    }

    /// Get the key question for this pass.
    pub fn key_question(&self) -> &'static str {
        match self {
            Self::Completeness => "Is anything missing?",
            Self::Correctness => "Is anything wrong?",
            Self::EdgeCases => "What could go wrong?",
            Self::Architecture => "Does this fit the larger system?",
            Self::Clarity => "Can someone implement this unambiguously?",
        }
    }

    /// Get the next pass, if any.
    pub fn next(&self) -> Option<Self> {
        Self::from_number(self.number() + 1)
    }

    /// Check if this is the final pass.
    pub fn is_final(&self) -> bool {
        *self == Self::Clarity
    }
}

impl fmt::Display for ReviewPass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pass {}: {}", self.number(), self.name())
    }
}

/// A prompt for a specific review pass.
#[derive(Debug, Clone)]
pub struct PassPrompt {
    /// The pass this prompt is for.
    pub pass: ReviewPass,
    /// The system prompt.
    pub system: String,
    /// The user prompt template (contains {{current-plan}} placeholder).
    pub user_template: String,
}

impl PassPrompt {
    /// Fill in the user template with the current plan content.
    pub fn render_user_prompt(&self, plan_content: &str) -> String {
        self.user_template.replace("{{current-plan}}", plan_content)
    }
}

/// Get the prompt for a specific review pass.
pub fn get_pass_prompt(pass: ReviewPass) -> PassPrompt {
    match pass {
        ReviewPass::Completeness => get_completeness_prompt(),
        ReviewPass::Correctness => get_correctness_prompt(),
        ReviewPass::EdgeCases => get_edge_cases_prompt(),
        ReviewPass::Architecture => get_architecture_prompt(),
        ReviewPass::Clarity => get_clarity_prompt(),
    }
}

fn get_completeness_prompt() -> PassPrompt {
    PassPrompt {
        pass: ReviewPass::Completeness,
        system: r#"You are reviewing a Plan document for COMPLETENESS.

This is Pass 1 of 5 in the Rule of Five review process. Focus ONLY on completeness.
Do not comment on correctness, edge cases, architecture, or clarity - those are for later passes.

Your task is to ensure all required sections are present and filled with substantive content."#
            .to_string(),
        user_template: r#"## Completeness Review (Pass 1 of 5)

**Key Question:** Is anything missing?

For this Plan to pass the completeness check, it MUST have:

1. **Summary** (2-3 sentences)
   - What is being built?
   - Why is it needed?
   - What's the scope boundary?

2. **Goals** (3-7 bullet points)
   - What will be true when this is done?
   - Measurable where possible

3. **Non-Goals** (2-5 bullet points)
   - What is explicitly out of scope?
   - What might readers assume is included but isn't?

4. **Proposed Solution**
   - High-level approach
   - Key components
   - Data flow

5. **Specs Section**
   - List of specs to implement this plan
   - Each spec has name, description, and scope

6. **Risks and Mitigations**
   - What could go wrong?
   - How will you prevent/detect/recover?

## Common Completeness Gaps

Watch for:
- "And other features" hand-waving
- Missing non-functional requirements (performance, security)
- No rollback plan
- Missing error handling strategy
- Undefined dependencies

## Your Task

Review the Plan below. For each missing or incomplete section:
1. Note specifically what's missing
2. Suggest concrete content to add

If the Plan is complete, respond with exactly:
`PASS: All required sections present and filled.`

If improvements are needed, respond with:
`NEEDS_WORK:` followed by a numbered list of specific issues.

---

{{current-plan}}"#
            .to_string(),
    }
}

fn get_correctness_prompt() -> PassPrompt {
    PassPrompt {
        pass: ReviewPass::Correctness,
        system: r#"You are reviewing a Plan document for CORRECTNESS.

This is Pass 2 of 5 in the Rule of Five review process. Focus ONLY on correctness.
Assume the Plan is complete (that was checked in Pass 1).
Do not comment on edge cases, architecture, or clarity - those are for later passes.

Your task is to find logical errors, invalid assumptions, and technical impossibilities."#
            .to_string(),
        user_template: r#"## Correctness Review (Pass 2 of 5)

**Key Question:** Is anything wrong?

Check for:

### Logical Errors
- Contradictions between sections
- Steps that don't follow from previous steps
- Circular reasoning

### Invalid Assumptions
- Assuming APIs exist that don't
- Assuming capabilities the system doesn't have
- Assuming data exists that won't

### Technical Impossibilities
- Race conditions in described flows
- Circular dependencies
- Impossible timing requirements
- Wrong data types or formats

### Incorrect Dependencies
- Depending on components that don't exist
- Wrong version requirements
- Missing transitive dependencies

## Common Correctness Errors

Watch for:
- Using a library incorrectly
- Misunderstanding an existing API
- Assuming synchronous when async is required
- Memory/resource leaks in described approach

## Your Task

Review the Plan for factual and logical correctness. For each error:
1. Quote the problematic statement
2. Explain why it's wrong
3. Suggest the correction

If the Plan is correct, respond with exactly:
`PASS: No logical errors or incorrect assumptions found.`

If corrections are needed, respond with:
`NEEDS_WORK:` followed by a numbered list of specific issues.

---

{{current-plan}}"#
            .to_string(),
    }
}

fn get_edge_cases_prompt() -> PassPrompt {
    PassPrompt {
        pass: ReviewPass::EdgeCases,
        system: r#"You are reviewing a Plan document for EDGE CASES.

This is Pass 3 of 5 in the Rule of Five review process. Focus ONLY on edge cases and error handling.
Assume the Plan is complete and correct (those were checked in Passes 1-2).
Do not comment on architecture or clarity - those are for later passes.

Your task is to identify failure modes and ensure they're addressed."#
            .to_string(),
        user_template: r#"## Edge Cases Review (Pass 3 of 5)

**Key Question:** What could go wrong?

For each component/operation in the Plan, consider:

### Resource Failures
- Network unavailable
- Disk full
- Memory exhausted
- File locked by another process
- Permission denied

### Timing Issues
- Timeout during operation
- Partial completion before failure
- Concurrent access from multiple processes
- Clock skew between systems

### Data Issues
- Empty input
- Malformed input
- Extremely large input
- Unicode/encoding issues
- Null/missing fields

### State Issues
- Operation interrupted mid-way
- Retry after partial failure
- Stale cache/data
- Version mismatch

### External Dependencies
- API rate limiting
- Service unavailable
- Authentication expired
- Schema changes

## Common Edge Case Misses

- Network failures (timeouts, disconnects)
- Disk full during write
- Permission denied
- Invalid user input
- Clock skew between systems

## Your Task

For each operation described in the Plan:
1. Identify potential edge cases
2. Check if the Plan addresses them
3. If not, suggest specific handling

Format issues as:
- **[Component/Operation]**: [Edge case] - [Current handling or "NOT ADDRESSED"]

If all edge cases are adequately addressed, respond with exactly:
`PASS: Edge cases adequately covered.`

If improvements are needed, respond with:
`NEEDS_WORK:` followed by a list of unaddressed edge cases.

---

{{current-plan}}"#
            .to_string(),
    }
}

fn get_architecture_prompt() -> PassPrompt {
    PassPrompt {
        pass: ReviewPass::Architecture,
        system: r#"You are reviewing a Plan document for ARCHITECTURE fit.

This is Pass 4 of 5 in the Rule of Five review process. Focus ONLY on architectural concerns.
Assume the Plan is complete, correct, and handles edge cases (those were checked in Passes 1-3).
Do not comment on clarity - that's for the final pass.

Your task is to ensure the Plan fits well within the larger system."#
            .to_string(),
        user_template: r#"## Architecture Review (Pass 4 of 5)

**Key Question:** Does this fit the larger system?

Check for:

### Consistency with Existing Patterns
- Does this follow established conventions?
- Are similar problems solved similarly?
- Does naming match existing style?

### Integration Points
- How does this connect to existing components?
- Are interfaces well-defined?
- Are there version compatibility concerns?

### Impact on Existing Functionality
- Will this break anything?
- Are there performance implications?
- Does this introduce new dependencies?

### Scalability
- Will this work at 10x scale?
- Are there bottlenecks?
- Is there unnecessary coupling?

### Technical Debt
- Is this creating future problems?
- Are there cleaner alternatives?
- Is complexity justified?

## Common Architecture Issues

- Reinventing existing utilities
- Breaking encapsulation
- Creating circular imports
- Inconsistent naming conventions
- Tight coupling to implementation details

## Your Task

Evaluate the Plan's architectural fit:
1. Note any architectural concerns
2. Suggest specific improvements
3. Highlight good architectural decisions

If the architecture is sound, respond with exactly:
`PASS: Architecture fits well with the larger system.`

If improvements are needed, respond with:
`NEEDS_WORK:` followed by a list of architectural concerns.

---

{{current-plan}}"#
            .to_string(),
    }
}

fn get_clarity_prompt() -> PassPrompt {
    PassPrompt {
        pass: ReviewPass::Clarity,
        system: r#"You are reviewing a Plan document for CLARITY.

This is Pass 5 of 5 (final pass) in the Rule of Five review process. Focus ONLY on clarity.
Assume the Plan is complete, correct, handles edge cases, and has good architecture (all checked in Passes 1-4).

Your task is to ensure someone can implement this unambiguously."#
            .to_string(),
        user_template: r#"## Clarity Review (Pass 5 of 5 - Final)

**Key Question:** Can someone implement this unambiguously?

Check for:

### Precision of Language
- No "should", "might", "could" where certainty is needed
- No weasel words ("approximately", "generally", "usually")
- Specific numbers instead of "a few", "several", "many"

### Concrete Examples
- Abstract concepts illustrated with examples
- Sample inputs and outputs
- Reference implementations where helpful

### Clear Acceptance Criteria
- How do you know when it's done?
- What does "success" look like?
- How is it tested?

### No Undefined Terms
- All jargon explained or linked
- No assumed knowledge
- Acronyms expanded on first use

### Measurable Outcomes
- "Fast" → "<100ms response time"
- "Scalable" → "handles 10k requests/second"
- "Reliable" → "99.9% uptime"

## Common Clarity Problems

- "Make it fast" (fast is not measurable)
- "Handle errors appropriately" (appropriate is undefined)
- Jargon without definition
- Multiple valid interpretations
- Vague descriptions

## Your Task

Review the Plan for implementability:
1. Flag any ambiguous statements
2. Suggest specific rewording
3. Identify missing examples or details

If the Plan is clear enough to implement, respond with exactly:
`PASS: Plan is clear and implementable.`

If improvements are needed, respond with:
`NEEDS_WORK:` followed by a list of clarity issues.

---

{{current-plan}}"#
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_review_pass_from_number() {
        assert_eq!(ReviewPass::from_number(1), Some(ReviewPass::Completeness));
        assert_eq!(ReviewPass::from_number(2), Some(ReviewPass::Correctness));
        assert_eq!(ReviewPass::from_number(3), Some(ReviewPass::EdgeCases));
        assert_eq!(ReviewPass::from_number(4), Some(ReviewPass::Architecture));
        assert_eq!(ReviewPass::from_number(5), Some(ReviewPass::Clarity));
        assert_eq!(ReviewPass::from_number(0), None);
        assert_eq!(ReviewPass::from_number(6), None);
    }

    #[test]
    fn test_review_pass_number() {
        assert_eq!(ReviewPass::Completeness.number(), 1);
        assert_eq!(ReviewPass::Correctness.number(), 2);
        assert_eq!(ReviewPass::EdgeCases.number(), 3);
        assert_eq!(ReviewPass::Architecture.number(), 4);
        assert_eq!(ReviewPass::Clarity.number(), 5);
    }

    #[test]
    fn test_review_pass_name() {
        assert_eq!(ReviewPass::Completeness.name(), "Completeness");
        assert_eq!(ReviewPass::Correctness.name(), "Correctness");
        assert_eq!(ReviewPass::EdgeCases.name(), "Edge Cases");
        assert_eq!(ReviewPass::Architecture.name(), "Architecture");
        assert_eq!(ReviewPass::Clarity.name(), "Clarity");
    }

    #[test]
    fn test_review_pass_next() {
        assert_eq!(ReviewPass::Completeness.next(), Some(ReviewPass::Correctness));
        assert_eq!(ReviewPass::Correctness.next(), Some(ReviewPass::EdgeCases));
        assert_eq!(ReviewPass::EdgeCases.next(), Some(ReviewPass::Architecture));
        assert_eq!(ReviewPass::Architecture.next(), Some(ReviewPass::Clarity));
        assert_eq!(ReviewPass::Clarity.next(), None);
    }

    #[test]
    fn test_review_pass_is_final() {
        assert!(!ReviewPass::Completeness.is_final());
        assert!(!ReviewPass::Correctness.is_final());
        assert!(!ReviewPass::EdgeCases.is_final());
        assert!(!ReviewPass::Architecture.is_final());
        assert!(ReviewPass::Clarity.is_final());
    }

    #[test]
    fn test_review_pass_display() {
        assert_eq!(format!("{}", ReviewPass::Completeness), "Pass 1: Completeness");
        assert_eq!(format!("{}", ReviewPass::Clarity), "Pass 5: Clarity");
    }

    #[test]
    fn test_pass_prompt_render() {
        let prompt = get_pass_prompt(ReviewPass::Completeness);
        let plan_content = "# Plan: Test\n## Summary\nTest plan";
        let rendered = prompt.render_user_prompt(plan_content);

        assert!(rendered.contains("# Plan: Test"));
        assert!(rendered.contains("## Summary"));
        assert!(!rendered.contains("{{current-plan}}"));
    }

    #[test]
    fn test_all_passes_have_prompts() {
        for i in 1..=5 {
            let pass = ReviewPass::from_number(i).unwrap();
            let prompt = get_pass_prompt(pass);

            // Check that system prompt mentions the pass
            assert!(prompt.system.contains(&format!("Pass {} of 5", i)));

            // Check that user template has the placeholder
            assert!(prompt.user_template.contains("{{current-plan}}"));
        }
    }

    #[test]
    fn test_completeness_prompt_content() {
        let prompt = get_pass_prompt(ReviewPass::Completeness);
        assert!(prompt.user_template.contains("Summary"));
        assert!(prompt.user_template.contains("Goals"));
        assert!(prompt.user_template.contains("Non-Goals"));
        assert!(prompt.user_template.contains("Proposed Solution"));
        assert!(prompt.user_template.contains("Specs"));
        assert!(prompt.user_template.contains("Risks"));
    }

    #[test]
    fn test_edge_cases_prompt_content() {
        let prompt = get_pass_prompt(ReviewPass::EdgeCases);
        assert!(prompt.user_template.contains("Network"));
        assert!(prompt.user_template.contains("Disk"));
        assert!(prompt.user_template.contains("Timeout"));
        assert!(prompt.user_template.contains("Empty input"));
    }

    #[test]
    fn test_clarity_prompt_content() {
        let prompt = get_pass_prompt(ReviewPass::Clarity);
        assert!(prompt.user_template.contains("should"));
        assert!(prompt.user_template.contains("might"));
        assert!(prompt.user_template.contains("Measurable"));
        assert!(prompt.user_template.contains("Acceptance Criteria"));
    }
}
