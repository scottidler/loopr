# Design Document: Agent Guidance System

**Author:** Scott Idler
**Date:** 2026-02-28
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Introduce a layered guidance system for Loopr agents modeled after Claude Code's `CLAUDE.md` / `AGENT.md` pattern. Agents receive context from four layers: built-in role prompts (`.pmt`), auto-generated schema docs (transition graphs, valid actions), global user preferences (`~/.config/loopr/LOOPR.md`), and project-specific conventions (`$TARGET_PROJECT/LOOPR.md`). This eliminates the class of bugs where agents hallucinate invalid actions (e.g., the coordinator burning 10+ iterations trying `Blocked → Done`) and gives users a familiar, file-based mechanism to steer agent behavior.

## Problem Statement

### Background

Loopr agents operate via LLM calls with structured JSON action responses. Each agent type has a system prompt (`.pmt` file) that describes its role, capabilities, and rules. The coordinator prompt includes a one-line summary of work status transitions:

```
Works: Ready → InProgress → InReview → Integrated → Done (or Blocked, Abandoned)
```

This is the only guidance the coordinator receives about valid state transitions — it doesn't include which states can reach `Blocked`, how to exit `Blocked`, or which role is required for each transition.

### Problem

1. **Agents hallucinate invalid actions.** The coordinator repeatedly tried `Blocked → Done` because it had no way to know the valid transition graph. It burned 10+ LLM iterations on the same invalid action, recording "learnings" that didn't help because the constraint is structural, not discovered.

2. **No user-authored project guidance.** When loopr runs against a target project (e.g., a Rails app), agents have no way to learn project conventions. An implementer might use `minitest` when the project uses `rspec`, or create files in the wrong directory structure.

3. **No global user preferences.** Users can't express cross-project preferences like "always use ES modules" or "prefer functional style" without editing the built-in `.pmt` files.

4. **Schema knowledge is siloed in Rust code.** Transition rules, valid actions per role, field schemas, and status enums are defined in Rust but never exposed to the agents that need them. The only bridge is hand-written prose in `.pmt` files that can drift from the actual code.

### Goals

- Agents never attempt structurally invalid actions (transitions, malformed payloads, role violations)
- Users can steer agent behavior via `LOOPR.md` files without modifying loopr source
- Schema documentation is auto-generated from Rust code, guaranteed in sync
- Guidance layers compose predictably: built-in < global < project
- Token budget impact is bounded and configurable

### Non-Goals

- Runtime query actions (e.g., `{"action": "query_transitions"}`) — may come later, but static context injection solves 90% of the problem at lower complexity
- Per-agent-type user overrides (e.g., `LOOPR-implementer.md`) — the single `LOOPR.md` per layer is sufficient for MVP
- Prompt override migration — the existing `~/.config/loopr/prompts/*.pmt` override mechanism remains unchanged and independent

## Proposed Solution

### Overview

Four layers of guidance, assembled at context-build time into each agent's prompt:

```
┌─────────────────────────────────────────────┐
│ Layer 4: Auto-Generated Schema              │  ← Derived from Rust code
│   Transition graphs, valid actions, enums   │
├─────────────────────────────────────────────┤
│ Layer 3: Project LOOPR.md                   │  ← $TARGET_PROJECT/LOOPR.md
│   "Use rspec", "services in app/services/"  │
├─────────────────────────────────────────────┤
│ Layer 2: Global LOOPR.md                    │  ← ~/.config/loopr/LOOPR.md
│   "Prefer ES modules", "Use 2-space indent" │
├─────────────────────────────────────────────┤
│ Layer 1: Built-in Role Prompt (.pmt)        │  ← prompts/coordinator.pmt
│   Agent identity, capabilities, rules       │
└─────────────────────────────────────────────┘
```

Higher layers can refine but not contradict lower layers. The built-in `.pmt` defines what the agent *is*; schema docs define what the system *allows*; `LOOPR.md` files define how the user *wants* things done.

### Architecture

#### Layer 1: Built-in Role Prompts (existing, unchanged)

The `.pmt` files in `prompts/` remain the system prompt foundation. They define agent identity, capabilities, and structural rules. Loaded via `prompts::init()` with `~/.config/loopr/prompts/` overrides.

No changes to this layer.

#### Layer 2: Global User Guidance

**File:** `~/.config/loopr/LOOPR.md`

Loaded once at daemon startup. Injected into every agent's context. Contains cross-project user preferences:

```markdown
# Global Preferences

- Always use ES modules (import/export), never CommonJS (require)
- Prefer functional patterns over class hierarchies
- Test file naming: `*.test.{ext}` colocated with source
- Never auto-commit to main/master branches
```

#### Layer 3: Project Guidance

**File:** `$TARGET_PROJECT/LOOPR.md` (where `$TARGET_PROJECT` = `config.project.repo_path`)

Loaded once at daemon startup, re-read if modified (via notify/polling — deferred to later). Injected into every agent's context. Contains project-specific conventions:

```markdown
# Project: MyApp

## Stack
- Ruby on Rails 7.2, Ruby 3.3
- PostgreSQL 16, Redis 7

## Conventions
- Tests: RSpec, not minitest. Run with `bundle exec rspec`.
- Services go in `app/services/`, follow `ApplicationService` base class.
- API controllers inherit from `Api::BaseController`.
- Use Sorbet for type annotations on new code.

## Off Limits
- Do not modify `app/models/legacy_billing.rb` — scheduled for removal.
- Do not add new Sidekiq workers without explicit approval.
```

#### Layer 4: Auto-Generated Schema Docs

Generated at daemon startup from the Rust transition rules and action definitions. Stored in memory (not files). Injected into agent context based on role.

**What gets generated:**

1. **Transition graphs** per collection, filtered by the agent's role:
```
## Work Transitions (as Coordinator)
Blocked → Ready
Blocked → Abandoned
Draft → Ready
Draft → Abandoned
Ready → InProgress
Ready → Abandoned
InProgress → Blocked  (note: any role)
InReview → InProgress
Integrated → Done
InProgress → Abandoned
Blocked → Abandoned
InReview → Abandoned
Integrated → Abandoned
```

2. **Status enums** with terminal markers:
```
## Work Statuses
Draft, Ready, InProgress, Blocked, InReview, Integrated, Done*, Abandoned*
(* = terminal, no outgoing transitions)
```

3. **Valid actions for this role** with required fields:
```
## Your Actions
- transition: {collection, id, target_status} — see transition graphs above
- create_plan: {title, description, acceptance_criteria}
- assign_agent: {agent_type, target_id}
...
```

The coordinator sees all transition graphs (plans, specs, phases, works, bundles). An implementer only sees the subset relevant to its role (propose_bundle, done, need_help). A reviewer sees verdict options and bundle transitions.

### Data Model

New struct for assembled guidance:

```rust
/// Assembled guidance from all layers, ready for context injection.
pub struct AgentGuidance {
    /// Layer 2: Global user preferences (from ~/.config/loopr/LOOPR.md)
    pub global_md: Option<String>,
    /// Layer 3: Project conventions (from $TARGET/LOOPR.md)
    pub project_md: Option<String>,
    /// Layer 4: Auto-generated schema, keyed by role
    pub schema_docs: HashMap<Role, String>,
}
```

Generated once at startup, stored in `DaemonContext`:

```rust
pub struct DaemonContext {
    // ... existing fields ...
    pub guidance: AgentGuidance,
}
```

#### Schema Generation

New module `src/guidance.rs` (or `src/schema_docs.rs`):

```rust
/// Generate role-specific schema documentation from transition rules.
pub fn generate_schema_doc(role: Role) -> String {
    let mut doc = String::new();

    // Work transitions for this role
    doc.push_str("## Work Status Transitions\n");
    for rule in work_transitions() {
        if rule.role.is_none() || rule.role == Some(role) {
            doc.push_str(&format!("{} → {}", rule.from, rule.to));
            if rule.role.is_none() {
                doc.push_str(" (any role)");
            }
            doc.push('\n');
        }
    }

    // Bundle transitions for this role
    // Plan/Spec/Phase transitions for this role
    // Valid actions for this role
    // ...

    doc
}
```

This reads from the same `work_transitions()`, `bundle_transitions()`, `hierarchy_transitions()` functions that the runtime validates against — guaranteed in sync by construction. If a developer adds a new transition rule in Rust, the schema doc updates automatically on the next daemon start. No manual `.pmt` edits, no split-brain.

### Context Assembly Changes

The `ContextBuilder` gains a new builder method following the existing `with_*` pattern:

```rust
impl ContextBuilder {
    /// Inject assembled guidance (schema + global + project LOOPR.md).
    pub fn with_guidance(mut self, guidance: &AgentGuidance, role: Role) -> Self {
        self.guidance_text = Some(assemble_guidance(guidance, role));
        self
    }
}
```

The `build()` method already assembles sections into the user message string in order. The guidance block slots in after the project goal:

```
1. [existing] Project Goal (coordinator_goal)
2. [NEW]      Guidance Block (schema + global + project LOOPR.md)
3. [existing] Hierarchy context (plan → spec → phase → work)
4. [existing] Sibling Works
5. [existing] Bundle under review
6. [existing] Learnings
7. [existing] Available Tools
8. [existing] State Summary
9. [existing] Previous Iteration Summary
10. [existing] Current Iteration + Footer
```

The guidance block is injected early so it establishes the rules before the agent sees the state it needs to act on. This mirrors how Claude Code's `CLAUDE.md` appears in the system context before the user's task.

#### Concrete Example: Coordinator Guidance Block

This is what the coordinator would see in its user message (after the goal, before the state summary):

```
### System Rules

## Work Status Transitions (your role: Coordinator)
Draft → Ready
Ready → InProgress
Blocked → Ready
InReview → InProgress  (send back for rework)
Integrated → Done
Draft → Abandoned
Ready → Abandoned
InProgress → Abandoned
Blocked → Abandoned
InReview → Abandoned
Integrated → Abandoned

Note: InProgress → Blocked is allowed by any role.
Note: InProgress → InReview is Implementer-only.
Note: InReview → Integrated is Integrator-only.

Terminal states: Done, Abandoned (no outgoing transitions)

## Plan/Spec/Phase Status Transitions (your role: Coordinator)
Draft → Active
Active → Complete
Draft → Abandoned
Active → Abandoned

## Bundle Status Transitions (your role: Coordinator)
Proposed → Triaged
Reviewed → Accepted
Proposed → Rejected
Triaged → Rejected
Reviewed → Rejected
Proposed → Superseded
Triaged → Superseded
Reviewed → Superseded
Accepted → Superseded
Integrating → Superseded

### User Preferences

- Always use ES modules (import/export), never CommonJS (require)
- Prefer functional patterns over class hierarchies

### Project Conventions

- Tests: Jest with `--experimental-vm-modules` for ESM support
- Source files in `src/`, tests colocated as `*.test.js`
- Use `crypto.randomUUID()` for ID generation
```

#### Token Budget

Add a `guidance` field to `TokenBudget`:

| Role | guidance tokens | Source |
|------|----------------|--------|
| Coordinator | 1500 | Needs full transition graphs for all collections |
| Implementer | 800 | Smaller: just work conventions and tool list |
| Reviewer | 500 | Minimal: review criteria, bundle statuses |
| Researcher | 500 | Minimal: search conventions |
| Integrator | 800 | Bundle + work transition graphs |

Total token increase per agent: 500–1500. Well within the context limits.

#### Truncation

If combined guidance exceeds budget:
1. Schema docs are never truncated (they're the structural ground truth)
2. Project LOOPR.md truncated first (most likely to be long)
3. Global LOOPR.md truncated last

### File Discovery

```rust
fn load_guidance(config: &Config) -> AgentGuidance {
    let global_md = load_optional_file(
        dirs::config_dir().map(|d| d.join("loopr/LOOPR.md"))
    );

    let project_md = load_optional_file(
        Some(config.project.repo_path.join("LOOPR.md"))
    );

    let schema_docs = [
        Role::Coordinator,
        Role::Implementer,
        Role::Reviewer,
        Role::Researcher,
        Role::Integrator,
    ].into_iter()
     .map(|role| (role, generate_schema_doc(role)))
     .collect();

    AgentGuidance { global_md, project_md, schema_docs }
}
```

### Implementation Plan

**Phase 1: Schema Generation (foundation)**
- Add `src/guidance.rs` module
- Implement `generate_schema_doc(role)` reading from existing `work_transitions()`, `bundle_transitions()`, `hierarchy_transitions()`
- Add `AgentGuidance` struct with `global_md`, `project_md`, `schema_docs` fields
- Tests: verify generated docs contain every rule from each transition function, filtered by role

**Phase 2: Context Integration**
- Add `guidance` field to `TokenBudget` per role (Coordinator: 1500, Implementer/Integrator: 800, Reviewer/Researcher: 500)
- Add `with_guidance()` builder method to `ContextBuilder`
- Add `guidance` field to `DaemonContext`, populated at startup via `load_guidance()`
- Inject guidance block into `ContextBuilder::build()` after goal, before hierarchy
- Inject schema section into `build_plan_prompt()`, `build_spec_prompt()`, `build_phase_prompt()`, `build_work_prompt()` in `generation.rs`
- Remove stale `## Status Transitions` section from `coordinator.pmt`

**Phase 3: LOOPR.md Loading**
- Load `~/.config/loopr/LOOPR.md` at startup (optional, logged if found)
- Load `$TARGET/LOOPR.md` at startup (where `$TARGET` = `config.project.repo_path`, optional)
- Compose into guidance block: schema (never truncated) + global md + project md (truncated to budget)
- Log warning when truncation occurs
- Tests: verify layering, truncation priority, missing files handled gracefully

**Phase 4: Validation**
- Run the todo-app test case again with guidance enabled
- Verify coordinator no longer attempts invalid transitions
- Verify implementers respect project LOOPR.md conventions
- Measure token budget impact (actual vs budgeted)

## Alternatives Considered

### Alternative 1: Runtime Query Actions
- **Description:** Add `{"action": "query", "kind": "valid_transitions", ...}` that agents call at runtime to look up schema information.
- **Pros:** Zero token overhead when not needed. Agents can query exactly what they need. Naturally extends to dynamic state queries (e.g., "what blocks this work?").
- **Cons:** Costs an LLM round-trip per query. Agent must know to query before acting (chicken-and-egg). More complex to implement (new action type, handler, response format). Doesn't solve the "agent doesn't know what it doesn't know" problem.
- **Why not chosen:** Static injection solves the immediate problem at lower complexity. Query actions are a good future addition for dynamic state, but transition graphs are static and belong in context.

### Alternative 2: Enhanced Error Messages
- **Description:** When a transition fails, return the valid transitions in the error message so the agent can self-correct.
- **Pros:** Zero upfront token cost. Agent learns from failures.
- **Cons:** Still wastes iterations on the first failure. Doesn't help with other schema knowledge (action formats, field requirements). Reactive, not proactive.
- **Why not chosen:** Band-aid. The agent shouldn't have to fail to learn the rules. But this is a good complement — we should do this too.

### Alternative 3: Hardcode Everything in .pmt Files
- **Description:** Manually write the full transition graphs into each `.pmt` file.
- **Pros:** Simple. No new code. Works today.
- **Cons:** Drifts from code. Must be manually updated when transition rules change. Duplicates information across multiple `.pmt` files. No user customization mechanism.
- **Why not chosen:** Violates DRY and will drift. Auto-generation from the actual Rust transition rules is the right approach.

### Alternative 4: Per-Agent-Type User Overrides
- **Description:** Support `LOOPR-coordinator.md`, `LOOPR-implementer.md`, etc. for per-role guidance.
- **Pros:** More granular control.
- **Cons:** Over-engineering for MVP. Users rarely need role-specific guidance — project conventions apply to all agents. Adds file discovery complexity.
- **Why not chosen:** Deferred. Single `LOOPR.md` covers the common case. Can add role-specific files later if demand exists.

## Technical Considerations

### Dependencies

- No new external dependencies
- Internal: reads from existing `work_transitions()`, `bundle_transitions()`, `hierarchy_transitions()` functions
- `dirs` crate already used for config/prompt path resolution

### Performance

- Schema generation happens once at startup — no per-iteration cost
- LOOPR.md files read once at startup — cached in `DaemonContext`
- Token budget increase is 500–1500 tokens per agent, well within limits
- No additional LLM calls

### Security

- LOOPR.md files are user-authored and could contain prompt injection attempts
- Mitigation: guidance is injected as a clearly delimited section with framing ("The following are project conventions provided by the user...")
- Same trust model as Claude Code's CLAUDE.md — the user owns the file

### Testing Strategy

- Unit tests: `generate_schema_doc()` output matches expected format for each role
- Unit tests: transition graph completeness (every rule appears in generated doc)
- Unit tests: `ContextBuilder::with_guidance()` respects token budgets
- Unit tests: guidance layering (schema + global + project compose correctly)
- Integration test: coordinator with guidance does not attempt invalid transitions
- Manual test: re-run the todo-app scenario and verify no `invalid target_status` errors

### Rollout Plan

1. Merge Phase 1 (schema generation) — no behavior change, just new module
2. Merge Phase 2 (context integration) — agents now see schema docs
3. Merge Phase 3 (LOOPR.md loading) — users can now author guidance
4. Phase 4 is validation, not a separate deploy

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Schema docs consume too many tokens, crowding out other context | Low | Medium | Dedicated budget field, measured in Phase 4. Schema docs are compact (~200-400 tokens per role). |
| LOOPR.md prompt injection | Low | Medium | Delimit with framing text. Same trust model as CLAUDE.md. |
| Schema generation drifts from actual code | Very Low | High | Generated from the same functions the runtime uses. Cannot drift by construction. |
| Users write conflicting guidance in global vs project LOOPR.md | Medium | Low | Document precedence clearly. Project overrides global. Log when conflict detected. |
| Agents ignore guidance even when present | Medium | Medium | Phase 4 validates empirically. If still ignored, increase token budget or move critical rules to system prompt. |

## Migration Notes

The existing `coordinator.pmt` contains a stale one-liner about transitions:

```
Works: Ready → InProgress → InReview → Integrated → Done (or Blocked, Abandoned)
```

This line must be **removed** when the auto-generated schema is wired in, to avoid conflicting or redundant information. The auto-generated schema supersedes any hand-written transition documentation in `.pmt` files.

Similarly, the `## Status Transitions` section in `coordinator.pmt` (lines 43-47) should be replaced with a pointer: `"See the System Rules section in your context for valid transitions."`

## Edge Cases

1. **Self-referential usage:** When loopr runs against its own repo, the `LOOPR.md` would contain Rust conventions. This is fine and expected — the system doesn't special-case it.

2. **Generation prompts:** The `build_*_prompt()` functions in `generation.rs` assemble context independently of `ContextBuilder`. They need guidance injection too — specifically the schema docs, since the coordinator uses these prompts when creating plans/specs/phases. Add a `guidance_section` parameter to each `build_*_prompt()` function.

3. **No LOOPR.md files exist:** Both layers are `Option<String>`. If neither file exists, the guidance block contains only the auto-generated schema. This is the default for new projects.

4. **Very large LOOPR.md:** A user could write a 5000-word project guide. Truncation handles this, but we should log a warning when truncation occurs so users know to trim.

5. **Encoding:** LOOPR.md files should be UTF-8. Non-UTF-8 files are skipped with a warning (same as existing `.pmt` override loading).

## Open Questions

- [ ] Should we also enhance error messages with valid transitions (Alternative 2) as a complement? (Likely yes — belt and suspenders.)
- [ ] Should LOOPR.md support frontmatter for metadata (e.g., `agent_types: [implementer, reviewer]` to scope sections)?
- [ ] Hot-reload: should the daemon watch LOOPR.md for changes, or require restart? (Restart is simpler for MVP.)
- [ ] Should the schema doc include example JSON for each action, or just field names? (Field names + types keeps it compact.)
- [ ] Should the generation prompts (`build_plan_prompt`, etc.) receive the full guidance block, or just the schema portion?

## References

- Claude Code CLAUDE.md documentation
- Existing prompt system: `src/prompts.rs`, `prompts/*.pmt`
- Context assembly: `src/agents/context.rs`
- Transition rules: `src/domain/work.rs`, `src/domain/plan.rs`, `src/domain/bundle.rs`, `src/domain/transition.rs`
- Config loading: `src/config.rs`
