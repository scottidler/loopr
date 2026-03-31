# Design Document: Loopr v3 MVP9 — Semantic Decomposition Evaluation & Collaborative Plan Creation

**Author:** Scott Idler + Claude
**Date:** 2026-03-03
**Status:** Superseded by [2026-03-30-semantic-bubble-up-wiring.md](2026-03-30-semantic-bubble-up-wiring.md)
**Review Passes Completed:** 5/5

## Summary

MVP9 closes the quality gap between Loopr's tight Work-level feedback loop (code → compile → test → lint → review → iterate) and its loose upper-level decomposition pipeline (generate doc → structural validation → activate). It adds three capabilities: (1) a Coverage Evaluator that semantically checks parent→children relationships at every decomposition boundary, (2) upward feedback so bad children can trigger parent revision instead of infinite retry, and (3) collaborative Plan creation through a user-AI interview loop that sharpens the Plan before the autonomous machinery starts.

## Problem Statement

### Background

MVPs 1–8 built and hardened the full orchestration pipeline. The Work-level RWL is proven: Implementer writes code in an isolated worktree, creates a Bundle (proof of work), Reviewer reviews it, Integrator merges deterministically, and the Coordinator manages retries with SLA enforcement. The signal at the Work level is concrete — compiler errors, test failures, lint violations, review rejections — creating a tight feedback loop that lets agents zero in on correct implementations iteratively.

The upper levels (Plan→Spec, Spec→Phase, Phase→Work) also have a loop, but it's structurally weak:

```
Coordinator LLM generates children (build_*_prompt in generation.rs)
  → Doc Validator validates each child individually (validate_plan/spec/phase)
    → Pass → Coordinator activates child (Draft → Active)
    → Fail → accumulated failures fed into regeneration prompt → retry
```

The Doc Validator checks individual document quality: "Is this Spec clear enough? Does it have sufficient detail?" But it cannot check the **parent→children relationship**: "Do these three Specs, taken together, cover the Plan's acceptance criteria? Is there a gap where nothing implements the auth flow? Is the third Spec out of scope?"

This is a structural lint, not a semantic evaluation. The equivalent at the Work level would be: checking that each source file compiles individually, but never running the test suite to see if the files work together to satisfy the requirements. That would never produce correct software — and the current decomposition pipeline cannot reliably produce correct decompositions for the same reason.

### Problem

Three specific problems:

**1. No semantic coverage check at decomposition boundaries.**

`build_spec_prompt()` (generation.rs:118) includes the parent Plan's acceptance_criteria in the prompt, and `validate_spec()` (validator/mod.rs:62) checks each Spec individually — but nothing verifies that the *set* of Specs covers the Plan. The Coordinator activates Specs one by one as they pass individual validation, with no aggregate check. The same gap exists at Spec→Phase and Phase→Work boundaries.

**2. No upward feedback when decomposition fails.**

`find_draft_needing_regeneration()` (generation.rs:558) retries failed children up to `max_validation_attempts`, then `is_validation_cap_reached()` (generation.rs:656) signals `NeedHelp`. But the failure diagnosis is always "the child doc was bad" — never "the parent doc was ambiguous." If a Plan's acceptance criteria are vague, generating better Specs won't help; the Plan itself needs revision. Jim West's principle: a focused agent that produces bad output means the *input* was bad.

**3. Plan creation is one-shot, not collaborative.**

`build_plan_prompt()` (generation.rs:57) generates a Plan from a goal string in a single LLM call. The user provides `coordinator.set_goal("Build feature X")` and the Coordinator generates everything autonomously. But the Plan determines the quality ceiling of the entire pipeline (Nate's specification engineering principle). A one-shot generation from a terse goal string produces Plans with hidden assumptions, missing acceptance criteria, and implicit constraints — exactly the ambiguity that causes downstream decomposition failures.

Stripe solves this by having humans write the specs (the Slack message + thread context + linked tickets are the spec). Loopr's ambition — "dev team in a box" — requires automating the decomposition, which means the Plan must be richer and more precise than a one-shot generation can produce. The solution: a collaborative interview where the AI surfaces assumptions and the user sharpens intent before the autonomous machinery starts.

### Goals

- G1: Coverage Evaluator checks parent→children semantic completeness at Plan→Spec, Spec→Phase, and Phase→Work boundaries
- G2: Upward feedback allows decomposition failures to trigger parent revision (bubble-up) with configurable depth limits
- G3: Collaborative Plan creation through user-AI interview loop, with explicit user approval before autonomous decomposition begins
- G4: Decomposition attempt tracking in CoordinatorState with configurable max attempts per level
- G5: All changes pass `otto ci` with no regressions

### Non-Goals

- Changing the Work-level pipeline (Implementer → Reviewer → Integrator) — it already works
- Replacing the Doc Validator — it remains responsible for individual document structural quality
- Automated Plan creation without user involvement — the Plan is the one level where the user is always in the loop
- TUI redesign — minimal TUI changes, focused on the interview flow

## Proposed Solution

### Overview

Three new capabilities, layered:

1. **Coverage Evaluator** — a new module (`src/evaluator/`) that takes a parent doc + its children and returns a `CoverageReport` with verdict (Complete/Incomplete), identified gaps, and out-of-scope items. Uses an LLM call with boundary-specific prompts. Persisted in TaskStore.

2. **Upward Feedback** — extends the Coordinator's regeneration logic so that repeated coverage failures at level N trigger parent revision at level N-1. CoordinatorState tracks `decomposition_attempts` per parent. After `max_decomposition_attempts`, the Coordinator transitions the parent back to Draft with diagnostic context and regenerates it. Maximum bubble-up depth prevents infinite recursion. If bubble-up reaches the Plan level, the Coordinator signals NeedHelp (escalate to user).

3. **Collaborative Plan Creation** — a new `Interviewing` state in the Coordinator FSM that precedes `Planning`. In this state, the Coordinator generates interview questions about the goal, presents them to the user via IPC, incorporates answers, and iterates until the Plan is sharp. The user explicitly approves the Plan to transition from `Interviewing` → `Planning`. Below the Plan level, everything is autonomous.

### Architecture

#### New Module: `src/evaluator/`

```
src/evaluator/
├── mod.rs          # CoverageEvaluator struct + public API
└── prompts.rs      # Boundary-specific coverage evaluation prompts
```

The Coverage Evaluator reuses the existing `LlmClient` from `validator/client.rs` — no new HTTP client module needed.

The Coverage Evaluator is architecturally parallel to the Doc Validator:
- Doc Validator: validates **one document** against quality criteria → `ValidationReport`
- Coverage Evaluator: validates **parent + children set** against coverage criteria → `CoverageReport`

Both use synchronous LLM calls (ureq via `LlmClient`), both produce persisted reports, both gate transitions.

#### Modified Flow at Each Decomposition Boundary

**Before (current):**
```
Generate children as Draft
  → Doc Validator validates each child
    → Pass each → activate each (Draft → Active)
    → Fail any → regenerate with accumulated failures
```

**After (MVP9):**
```
Generate all children as Draft (one LLM batch call)
  → Doc Validator validates each child individually (structural quality)
    → Any child fails → regenerate that child with accumulated failures
    → Children continue to accumulate as validated Drafts
  → All children for parent are validated Drafts
    → Coverage Evaluator checks full set against parent (semantic completeness)
      → Complete → activate all children together (Draft → Active)
      → Incomplete → feed coverage gaps into regeneration prompt
        → Abandon current children, regenerate full set with gap context
        → If attempts >= max_decomposition_attempts → bubble up to parent
```

The key change: activation is deferred until *all* children for a parent pass *both* individual validation *and* collective coverage evaluation. This prevents the current behavior where children are activated one by one without an aggregate check.

**New helper function required:** `are_all_children_validated(stores, parent_collection, parent_id) -> bool` — returns true when every Draft child of the given parent has a passing `ValidationReport`. This is the trigger for coverage evaluation. Currently, `find_pending_draft_for_validation()` returns the first unvalidated Draft; the new function checks if *none* remain unvalidated.

#### Coordinator FSM Changes

```
Current:  Planning → ActivatePhase → Executing → PhaseGate → GoalComplete
                                                      ↓
                                                 (next phase)

MVP9:     Interviewing → Planning → ActivatePhase → Executing → PhaseGate → GoalComplete
               ↑                                                     ↓
          (user sets goal)                                      (next phase)
```

New state: `Interviewing`
- Entered when `coordinator.set_goal()` is called
- Coordinator generates interview questions about the goal
- User answers via TUI/IPC
- Coordinator refines understanding, asks follow-ups
- User approves the Plan → transitions to `Planning`
- `Planning` now generates Specs (Plan already exists and is Active)

The `Planning` state semantics shift: it no longer generates the Plan (that happened during `Interviewing`). It generates Specs, Phases, and Works — the autonomous decomposition pipeline. The name `Planning` is retained for backward compatibility with existing CoordinatorState records, but its role is now "Decomposing."

#### Interview Flow — End-to-End Sequence

```
1. User calls coordinator.set_goal("Add user authentication")
2. Coordinator enters Interviewing state
3. Coordinator LLM generates initial questions based on goal:
   → "What authentication method? (JWT, session, OAuth?)"
   → "What user storage backend? (DB, LDAP, external provider?)"
4. Questions sent to TUI via DaemonEvent::InterviewQuestion
5. User answers via coordinator.interview_respond:
   → "JWT with refresh tokens, PostgreSQL for user storage"
6. Coordinator LLM incorporates answer, asks follow-ups:
   → "What token expiration policy?"
   → "Do you need role-based access control?"
7. User answers again (rounds continue, max 5 by default)
8. Coordinator LLM has enough context → generates Plan draft via ProposePlan action
9. Plan created as Draft, presented to user in TUI
10. User reviews Plan draft:
    → Approves → coordinator.approve_plan(plan_id)
    → Requests changes → coordinator.interview_respond with feedback → back to step 6
11. On approval: Plan transitions Draft → Active, FSM transitions Interviewing → Planning
12. Autonomous decomposition begins (no more user interaction unless bubble-up)
```

#### Coverage Gate — Integration Point

The coverage check integrates into the Coordinator's `build_generation_footer()` function (coordinator.rs:287), which is called every iteration to determine what the Coordinator should do next. Currently this function has a decision tree:

1. `determine_generation_level()` → needs generation? → build prompt
2. `find_draft_needing_regeneration()` → failed validation? → regenerate
3. `find_pending_draft_for_validation()` → unvalidated draft? → validate
4. Otherwise → done

MVP9 inserts a new step between 3 and 4:

```
3b. are_all_children_validated(parent_id)?
    → YES: run coverage evaluation → Coverage pass? activate all. Coverage fail? regenerate set.
    → NO: continue validating individual drafts (step 3)
```

Concretely, this is a new function `find_children_needing_coverage_evaluation(stores) -> Option<(parent_id, children_ids)>` that returns the parent whose children are all individually validated but haven't been coverage-checked yet.

#### Bubble-Up — Concrete Walkthrough

Example: Plan says "Add auth with JWT, RBAC, and audit logging." Coordinator generates 3 Specs. Coverage Evaluator says "Incomplete: no Spec covers audit logging." Regeneration cycle:

```
Attempt 1: Generate Specs → Coverage: Incomplete (gap: audit logging)
  → Abandon 3 Draft Specs, regenerate with gap context:
    "Previous attempt missing coverage for: audit logging"
Attempt 2: Generate Specs → Coverage: Incomplete (gap: audit logging still missing)
  → Abandon, regenerate with accumulated gaps
Attempt 3: Generate Specs → Coverage: Incomplete again
  → decomposition_attempts["pl-abc123"] = 3 >= max_decomposition_attempts
  → BUBBLE UP: diagnose root cause

Bubble-up diagnosis:
  → Coordinator examines Plan: "audit logging" is mentioned in description
    but not in acceptance_criteria (ambiguous)
  → Coordinator transitions Plan back to Draft
  → Creates Learning: "Plan pl-abc123 acceptance_criteria did not explicitly
    include audit logging, causing Spec generation to omit it"
  → Regenerates Plan with diagnostic:
    "Previous Plan was ambiguous about audit logging. Ensure acceptance_criteria
    explicitly lists all required capabilities including audit logging."
  → New Plan Draft created, validated, re-activated
  → Spec generation restarts with attempt counter reset

If bubble-up reaches Plan level AND Plan regeneration fails:
  → Re-enter Interviewing state with diagnostic context
  → User sees: "The decomposition couldn't cover: audit logging.
    The Plan's acceptance criteria may need revision."
  → Interview resumes with diagnostic as seed context
  → User sharpens Plan → re-approve → decomposition restarts
```

#### Edge Cases

**Single-child decomposition:** A Plan may legitimately decompose into exactly 1 Spec. Coverage evaluation runs normally — the question is "does this one Spec cover everything?" not "are there enough Specs?"

**Re-coverage after child revision:** If a child is individually re-validated (e.g., regenerated after Doc Validation failure), the previous CoverageReport for that parent is invalidated. The `are_all_children_validated()` check re-triggers coverage evaluation with the updated set.

**Phase→Work coverage without Doc Validation:** Works skip the Doc Validator (MVP4 design — quality verified by compile/test). Coverage evaluation still runs at Phase→Work because it answers a different question: "do these Works cover the Phase's deliverables?" vs. "is this Work well-formed?" Phase→Work coverage goes directly from generation to coverage check, skipping the Doc Validation step.

**Graceful degradation:** All MVP9 features are independently gatable via config:
- `coverage_enabled=false`: skip coverage evaluation, activate children individually after Doc Validation (MVP4-8 behavior)
- `plan_interview_enabled=false`: skip Interviewing state, generate Plan from goal string (MVP4 behavior)
- `plan_approval_required=false`: auto-approve Plan after generation (useful for CI/testing)
- When all MVP9 features are disabled, behavior is identical to MVP4-8
- `plan_interview_enabled=false` + bubble-up reaches Plan level: signals NeedHelp directly (cannot re-interview, user must manually revise via TUI)

#### CoordinatorState Extensions

```rust
pub struct CoordinatorState {
    // ... existing fields ...

    /// Decomposition attempt count per parent ID.
    /// Tracks how many times we've tried to generate children for a given parent.
    /// Keyed by parent doc ID (plan_id, spec_id, or phase_id).
    pub decomposition_attempts: HashMap<String, u32>,

    /// Interview context accumulated during the Interviewing state.
    /// Each entry is a (question, answer) pair from the user interview.
    pub interview_context: Vec<InterviewExchange>,

    /// Whether the user has approved the Plan.
    /// Set to true when user explicitly approves during Interviewing state.
    pub plan_approved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterviewExchange {
    pub questions: Vec<String>,
    pub answer: String,
    pub timestamp: i64,
}
```

### Data Model

#### CoverageReport

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageReport {
    pub id: String,
    pub parent_collection: String,      // "plans", "specs", "phases"
    pub parent_id: String,
    pub children_collection: String,    // "specs", "phases", "works"
    pub children_ids: Vec<String>,
    pub verdict: CoverageVerdict,
    pub gaps: Vec<CoverageGap>,
    pub out_of_scope: Vec<OutOfScopeItem>,
    pub summary: String,
    pub model_used: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CoverageVerdict {
    /// Children fully cover the parent's requirements, nothing out of scope
    Complete,
    /// Gaps and/or out-of-scope items exist — needs regeneration
    /// (gaps and out_of_scope fields in CoverageReport provide details)
    Incomplete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageGap {
    /// The specific parent requirement or criterion that is not covered
    pub parent_criterion: String,
    /// Description of what's missing
    pub description: String,
    /// Whether this gap is critical (blocks progress) or minor (can be deferred)
    pub severity: GapSeverity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GapSeverity {
    Critical,
    Minor,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutOfScopeItem {
    /// Which child doc contains out-of-scope work
    pub child_id: String,
    /// Description of what's out of scope
    pub description: String,
}
```

CoverageReport implements `Record` for TaskStore persistence, indexed on `parent_id` and `parent_collection`.

#### InterviewExchange

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterviewExchange {
    /// Questions asked in this round (one or more per round)
    pub questions: Vec<String>,
    /// User's response to this round's questions
    pub answer: String,
    pub timestamp: i64,
}
```

Stored inline in `CoordinatorState.interview_context`, not as a separate TaskStore collection. The interview context is consumed during Plan generation. It is preserved after Plan approval so that downstream decomposition can reference the user's original intent and trade-off decisions if needed during bubble-up.

### API Design

#### New IPC Methods

**`coordinator.interview_respond`** — user answers an interview question

```json
{
    "method": "coordinator.interview_respond",
    "params": {
        "answer": "The auth system should use JWT with refresh tokens..."
    }
}
```

Response: next question(s) from the Coordinator, or a Plan draft for approval.

```json
{
    "result": {
        "type": "question",
        "questions": ["What token expiration policy do you want?", "Should we support OAuth providers?"]
    }
}
```

Or:

```json
{
    "result": {
        "type": "plan_draft",
        "plan": { "title": "...", "description": "...", "acceptance_criteria": "..." }
    }
}
```

**`coordinator.approve_plan`** — user approves the Plan draft

```json
{
    "method": "coordinator.approve_plan",
    "params": {
        "plan_id": "pl-abc123"
    }
}
```

Transitions Plan to Active, Coordinator FSM to Planning. Autonomous decomposition begins.

**`coverage.evaluate`** — trigger coverage evaluation (called by Coordinator internally, also available via IPC for debugging)

```json
{
    "method": "coverage.evaluate",
    "params": {
        "parent_collection": "plans",
        "parent_id": "pl-abc123"
    }
}
```

Returns CoverageReport.

#### New Coordinator Actions

The Coordinator's action schema (parsed from LLM responses) gains:

```rust
pub enum AgentAction {
    // ... existing variants ...

    /// Ask the user an interview question during Plan creation
    InterviewQuestion {
        questions: Vec<String>,
    },

    /// Propose a Plan draft for user approval
    ProposePlan {
        title: String,
        description: String,
        acceptance_criteria: String,
    },

    /// Request coverage evaluation of children against parent
    EvaluateCoverage {
        parent_collection: String,
        parent_id: String,
    },

    /// Revise a parent document (bubble-up from failed decomposition)
    ReviseParent {
        collection: String,
        id: String,
        reason: String,
        diagnostic: String,
    },
}
```

### Interview Prompt

The Coordinator uses a dedicated interview prompt (`prompts/interview.pmt`) during the `Interviewing` state. The prompt is structured around Nate's five specification primitives:

```
You are a technical interviewer helping a user sharpen a project plan.
Your goal is to surface hidden assumptions, clarify edge cases, and
produce a Plan with zero ambiguity.

## User's Goal
{goal}

## Interview History
{interview_exchanges}

## Your Task
Ask 2-3 focused questions to sharpen the Plan. Target the weakest areas:

1. **Self-contained problem statement**: Can the goal be understood without
   external context? What's assumed but unstated?
2. **Acceptance criteria**: What does "done" look like? How would someone
   verify the output without asking the user?
3. **Constraint architecture**: What must happen? What must NOT happen?
   What should be preferred when multiple approaches exist?
4. **Decomposition hints**: Are there natural boundaries where the work
   splits into independent pieces?
5. **Evaluation design**: How will we know the result is correct?

Focus on the HARD parts — trade-offs, edge cases, implicit constraints.
Do not ask obvious questions.

{if has_diagnostic}
## Diagnostic Context (from previous decomposition failure)
{diagnostic}
The previous Plan was insufficient. Focus your questions on the areas
identified in the diagnostic.
{endif}

When you have enough information to write a complete Plan (typically after
3-5 rounds), respond with a plan_draft action instead of more questions.

Respond with JSON:
{schema}
```

The interview prompt grows with each exchange — previous Q&A pairs are included in `{interview_exchanges}` so the LLM has full context. The `{diagnostic}` section is populated only during bubble-up re-interviews.

### Coverage Evaluation Prompts

Each boundary gets a specific prompt template. The prompts follow the existing pattern in `src/prompts.rs` with files in `prompts/`.

#### Plan → Specs Coverage Prompt

```
You are evaluating whether a set of Specs fully covers a Plan.

## Parent Plan
- Title: {plan_title}
- Description: {plan_description}
- Acceptance Criteria: {plan_acceptance_criteria}

## Generated Specs
{specs_list}

## Task
Evaluate whether these Specs, taken together, fully address the Plan's acceptance criteria and description.

Check for:
1. **Gaps**: Are there acceptance criteria or requirements in the Plan that no Spec addresses?
2. **Out-of-scope**: Do any Specs include work that goes beyond the Plan's stated goals?
3. **Coherence**: Do the Specs work together without contradictions or overlaps?

Respond with JSON:
{schema}
```

#### Spec → Phases Coverage Prompt

```
You are evaluating whether a set of Phases fully implements a Spec.

## Parent Spec
- Title: {spec_title}
- Description: {spec_description}
- Plan context: {plan_title}

## Generated Phases (in order)
{phases_list}

## Task
Evaluate whether these Phases, executed in order, fully implement the Spec.

Check for:
1. **Gaps**: Are there requirements in the Spec that no Phase addresses?
2. **Ordering**: Are dependencies between Phases correct? Can each Phase be executed after the previous ones complete?
3. **Out-of-scope**: Do any Phases include work beyond the Spec?
4. **Completeness**: After all Phases complete, would the Spec's goals be fully achieved?

Respond with JSON:
{schema}
```

#### Phase → Works Coverage Prompt

```
You are evaluating whether a set of Works fully implements a Phase.

## Parent Phase
- Title: {phase_title}
- Description: {phase_description}
- Order: {phase_order}
- Spec context: {spec_title}

## Generated Works
{works_list}

## Task
Evaluate whether these Works, with their dependencies, fully implement the Phase.

Check for:
1. **Gaps**: Are there deliverables in the Phase that no Work addresses?
2. **Granularity**: Are Works small enough for a single agent to implement in one session?
3. **Dependencies**: Are Work dependencies correct and complete? Are there missing dependencies?
4. **Resource tags**: Do resource_tags accurately reflect which files each Work will touch?
5. **Acceptance criteria**: Does each Work have testable acceptance criteria?

Respond with JSON:
{schema}
```

### Implementation Plan

#### Phase 1: Coverage Evaluator Foundation

- Create `src/evaluator/mod.rs` with `CoverageEvaluator` struct (reuses `LlmClient` from `validator/client.rs`)
- Create `src/evaluator/prompts.rs` with boundary-specific prompt templates
- Add `CoverageReport`, `CoverageVerdict`, `CoverageGap`, `OutOfScopeItem`, `GapSeverity` to `src/domain/`
- Implement `Record` for `CoverageReport` for TaskStore persistence
- Add `coverage_reports` store to `Stores`
- Add prompt template files: `prompts/coverage-plan-specs.pmt`, `prompts/coverage-spec-phases.pmt`, `prompts/coverage-phase-works.pmt`
- Add coverage evaluator config to `EvaluatorConfig` (model, max_tokens, etc.)
- Tests: unit tests for prompt building, mock LLM responses, report parsing

#### Phase 2: Coordinator Integration — Coverage Gate

- Modify `build_generation_footer()` to check coverage after all children pass Doc Validation
- Add `EvaluateCoverage` action to `AgentAction` enum
- Add `coverage.evaluate` IPC handler in `handlers.rs`
- Wire coverage evaluation into the Coordinator's iteration loop:
  - After all Draft children for a parent pass Doc Validation
  - Before activating any of them
  - Coverage pass → activate all children
  - Coverage fail → feed gaps into regeneration context
- Add `decomposition_attempts` to `CoordinatorState`
- Extend `find_draft_needing_regeneration()` to handle coverage failures
- Modify generation prompts to include coverage gap context when regenerating
- Tests: integration tests for coverage gate blocking activation, regeneration with gap context

#### Phase 3: Upward Feedback — Bubble-Up Logic

- Add `ReviseParent` action to `AgentAction` enum
- Implement bubble-up in Coordinator: if `decomposition_attempts[parent_id] >= max_decomposition_attempts`:
  - Transition parent back to Draft
  - Create a diagnostic Learning explaining why children couldn't be generated
  - Regenerate parent with diagnostic context
  - If parent is a Plan → signal NeedHelp (user must intervene)
- Add `max_decomposition_attempts` config (default: 3)
- Add `max_bubble_up_depth` config (default: 2) — prevents Plan→Spec failure from cascading infinitely
- Track bubble-up depth in CoordinatorState
- Tests: bubble-up from Spec→Phase failure to Spec revision, Plan-level NeedHelp escalation

#### Phase 4: Collaborative Plan Creation — Interview Loop

- Add `Interviewing` state to `CoordinatorFsmState`
- Modify `coordinator.set_goal()` to enter `Interviewing` instead of `Planning`
- Add `InterviewExchange` struct, `interview_context` and `plan_approved` to `CoordinatorState`
- Add `InterviewQuestion` and `ProposePlan` actions to `AgentAction`
- Add `coordinator.interview_respond` and `coordinator.approve_plan` IPC handlers
- Create interview prompt template (`prompts/interview.pmt`) that generates questions based on:
  - The goal text
  - Previous interview exchanges
  - Nate's five primitives: problem statement, acceptance criteria, constraints, decomposition hints, evaluation design
- Modify `build_plan_prompt()` to incorporate interview context when generating the Plan
- TUI: add interview panel that shows questions and accepts answers
- Plan draft is presented to user; user can request changes or approve
- On approval: Plan transitions to Active, FSM transitions to Planning
- Tests: interview flow state machine, context accumulation, plan generation with interview context

#### Phase 5: Configuration & Strategy Knobs

- Add to `StrategyConfig`:
  - `coverage_enabled: bool` (default: true)
  - `coverage_strictness: CoverageStrictness` — `RequireComplete` (default) | `AllowMinorGaps` | `SuggestOnly`
  - `max_decomposition_attempts: u32` (default: 3)
  - `max_bubble_up_depth: u32` (default: 2)
  - `plan_interview_enabled: bool` (default: true)
  - `plan_approval_required: bool` (default: true)
- Add `coverage_model` to `EvaluatorConfig` (can use different model than validator)
- Wire all knobs into Coordinator decision logic
- Tests: config variations, disabled coverage, disabled interview

## Alternatives Considered

### Alternative 1: Extend Doc Validator Instead of New Module

- **Description:** Add `validate_decomposition()` methods to the existing `DocValidator` rather than creating a separate `CoverageEvaluator`.
- **Pros:** Less new code, reuses existing LLM client and report infrastructure.
- **Cons:** Conflates two different concerns (individual doc quality vs. set coverage). The Doc Validator's prompt templates and report schema are designed for single-doc validation. Adding multi-doc coverage evaluation would make the module incoherent. The two evaluations may also want different models or temperature settings.
- **Why not chosen:** Separation of concerns. The Doc Validator answers "is this document well-formed?" The Coverage Evaluator answers "do these children faithfully implement the parent?" Different questions, different prompts, different report schemas.

### Alternative 2: Deterministic Coverage Check (No LLM)

- **Description:** Use rule-based heuristics instead of LLM for coverage evaluation — e.g., keyword matching between parent acceptance criteria and children descriptions, or checking that every parent criterion is "mentioned" in at least one child.
- **Pros:** Faster, cheaper, deterministic, no LLM token cost.
- **Cons:** Cannot understand semantic equivalence. "Implement user authentication" in the Plan might be covered by a Spec titled "JWT token management and session handling" — a keyword matcher would miss this. The whole point of semantic evaluation is that decomposition uses different language than the parent.
- **Why not chosen:** The problem is inherently semantic. Structural checks (Doc Validator) already handle the deterministic cases. Coverage evaluation requires understanding whether meanings align, not just whether words match.

### Alternative 3: Skip Interview, Use Richer Goal Input

- **Description:** Instead of an interactive interview, require the user to provide a structured goal with pre-defined sections (acceptance criteria, constraints, scope, etc.) via the TUI.
- **Pros:** Simpler implementation, no new FSM state, no IPC round-trips.
- **Cons:** Shifts the specification engineering burden entirely to the user. Users often don't know what they don't know — the interview surfaces hidden assumptions that a form can't. Nate's key insight: the AI should ask about "the hard parts" — edge cases, trade-offs, constraints the user hasn't considered.
- **Why not chosen:** The interview is the mechanism that produces high-quality Plans. A form collects what the user already knows; an interview discovers what they haven't thought about.

## Technical Considerations

### Dependencies

- **Internal:** Reuses `validator/client.rs` LLM client pattern (ureq, sync). Depends on existing TaskStore, Stores, PromptStore infrastructure.
- **External:** No new external dependencies. Same LLM API (Anthropic) used by Doc Validator.

### Performance

- Coverage evaluation adds one LLM call per decomposition boundary per attempt. At three boundaries (Plan→Spec, Spec→Phase, Phase→Work), this is 3 additional LLM calls for a clean decomposition — comparable to the 3 Doc Validator calls already made.
- Interview adds N LLM calls where N is the number of interview rounds (typically 3-5 exchanges). This is a one-time cost at the start of a goal.
- Bubble-up adds LLM calls only on failure (regeneration + re-evaluation). Capped by `max_decomposition_attempts × max_bubble_up_depth`.

### Testing Strategy

- Unit tests: prompt building, report parsing, CoordinatorState extensions, FSM transition validation
- Integration tests: coverage gate blocking activation, regeneration with gap context, bubble-up chain, interview flow
- Mock LLM responses for deterministic testing (same pattern as existing validator tests)
- End-to-end: full goal → interview → Plan → Spec → Phase → Work pipeline with coverage gates

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Coverage Evaluator LLM produces inconsistent verdicts across runs | Medium | Medium | Use low temperature (0.0-0.2), specific prompts with concrete criteria, and accept minor gaps via `AllowMinorGaps` strictness |
| Bubble-up creates oscillation (parent revised → children fail → parent revised again) | Low | High | `max_bubble_up_depth` config caps recursion. Each bubble-up includes diagnostic from previous attempt to prevent repeating the same revision |
| Interview loop never converges (AI keeps asking questions) | Low | Medium | Cap interview rounds (configurable, default 5). Coordinator must eventually `ProposePlan` or signal NeedHelp |
| Coverage evaluation at Phase→Work is too strict (rejects valid decompositions where Works are intentionally granular) | Medium | Low | Phase→Work coverage prompt emphasizes "deliverables" not "lines of code." `AllowMinorGaps` strictness lets minor gaps pass |
| Activation deferral (wait for all children to pass) slows the pipeline | Low | Low | Only defers activation, not generation. Children are generated in one batch. The delay is one additional LLM call |
| Bubble-up re-enters Interviewing for a Plan the user already approved | Low | Medium | Show the user the diagnostic context explaining *why* the Plan needs revision. The user chose the wrong trade-off or missed a constraint — the interview resumes from where they left off, not from scratch |

## Open Questions

- [ ] Should the Coverage Evaluator use the same model as the Doc Validator, or a separate (potentially stronger) model? Coverage evaluation is harder than structural validation.
- [ ] When bubbling up from Spec→Phase to revise the Spec, should existing Phase drafts be abandoned or preserved as context for the regenerated Spec?
- [ ] Should the Coverage Evaluator check for overlaps between children (duplicate coverage), or only gaps and out-of-scope?

## References

- `docs/design/2026-02-26-multi-level-rwl.md` — Multi-level RWL architecture (source of truth for current decomposition flow)
- `docs/design/2026-02-28-coordinator-sequencing.md` — Coordinator FSM and phase gating
- `docs/how-top-engineers-stop-ai-agents-from-writing-slop.txt` — Jim West's anti-slop patterns (quality gates, never fix bad output, pit of success)
- `docs/prompting-split-into-four-skills.txt` — Nate's four prompting disciplines (specification engineering, five primitives, evaluation design)
- `docs/minions-stripes-one-shot-end-to-end-coding-agents.md` — Stripe Minions (interleaved agent + deterministic, shift feedback left, at most two CI rounds)
