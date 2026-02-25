# Design Document: /plan Command and Loop Cascade System

**Author:** Scott A. Idler
**Date:** 2026-02-02
**Status:** Ready for Review
**Review Passes Completed:** 5/5

## Summary

Wire the `/plan` slash command in the TUI Chat view to create a PlanLoop that executes the Rule of Five methodology, producing 1+ `plan.md` artifacts. Implement the full cascade system where each loop type produces artifacts that spawn child loops:

```
PlanLoop → 1+ plan.md → SpecLoop(s) → 1+ spec.md → PhaseLoop(s) → 1+ phase.md → CodeLoop(s) → 1+ code/tests/docs/config
```

## Problem Statement

### Background

Loopr is designed as a hierarchical loop-based autonomous development system. The architecture is well-documented:
- `loop-architecture.md` defines the cascade model
- `rule-of-five.md` defines the quality methodology
- `domain-types.md` defines the unified `Loop` struct
- Prompt templates exist for plan, spec, and phase

However, the TUI Chat view currently sends all input directly to the daemon's chat handler. Typing `/plan` just sends that text to the LLM as a regular message - it doesn't trigger the loop creation system.

### Problem

1. **No slash command routing** - TUI input bypasses loop creation entirely
2. **No artifact lineage tracking** - `.md` files have no parent-child relationships
3. **No artifact-specific quality passes** - Rule of Five is generic, not tailored per artifact type
4. **No user activation flow** - Plans should pause for review before cascading

### Goals

- Wire `/plan` command to create a PlanLoop from conversation context
- Store artifacts with parent-child relationships in TaskStore
- Implement artifact-specific Rule of Five variants (plan vs spec vs phase)
- Display loops and artifacts in the Loops view
- Enable user activation of plans to trigger SpecLoop cascade
- Maintain full traceability: plan → spec → phase → code

### Non-Goals

- Changing the unified `Loop` struct design (behavior via `loop_type`, not separate types)
- Implementing auto-merge or worktree coordination (already designed)
- Building a full CI/CD integration
- Real-time streaming of LLM responses to artifacts (future enhancement)

## Proposed Solution

### Overview

1. **Slash Command Router**: Detect `/plan` in TUI input, route to `create_plan` IPC method
2. **Artifact Model**: New `Artifact` record type in TaskStore with parent-child relationships
3. **Artifact-Specific Passes**: Different Rule of Five focus areas per artifact type
4. **Activation Flow**: Plans complete in `AwaitingApproval` status, user activates to cascade

### Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                           TUI                                        │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐                          │
│  │  Chat    │  │  Loops   │  │ Approval │                          │
│  │          │  │          │  │          │                          │
│  │ /plan ───┼──┼──────────┼──┼──► Shows pending artifacts          │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘                          │
└───────┼─────────────┼─────────────┼──────────────────────────────────┘
        │             │             │
        │ IPC         │ IPC         │ IPC
        ▼             ▼             ▼
┌─────────────────────────────────────────────────────────────────────┐
│                         DAEMON                                       │
│  ┌─────────────────────────────────────────────────────────────┐    │
│  │                    LoopManager                               │    │
│  │  ┌──────────┐                                               │    │
│  │  │ PlanLoop │ ─► 1+ plan.md ─► AwaitingApproval            │    │
│  │  └──────────┘       │                                       │    │
│  │                     │ (user activates)                      │    │
│  │                     ▼                                       │    │
│  │  ┌──────────┐                                               │    │
│  │  │ SpecLoop │ ─► 1+ spec.md ─► auto-cascade                │    │
│  │  └──────────┘       │                                       │    │
│  │                     ▼                                       │    │
│  │  ┌───────────┐                                              │    │
│  │  │ PhaseLoop │ ─► 1+ phase.md ─► auto-cascade              │    │
│  │  └───────────┘       │                                      │    │
│  │                      ▼                                      │    │
│  │  ┌──────────┐                                               │    │
│  │  │ CodeLoop │ ─► 1+ code/tests/docs/config ─► validation   │    │
│  │  └──────────┘                                               │    │
│  └─────────────────────────────────────────────────────────────┘    │
│                              │                                       │
│                              ▼                                       │
│  ┌─────────────────────────────────────────────────────────────┐    │
│  │                     TaskStore                                │    │
│  │  loops.jsonl  │  artifacts.jsonl  │  events.jsonl           │    │
│  └─────────────────────────────────────────────────────────────┘    │
│                              │                                       │
│                              ▼                                       │
│  ┌─────────────────────────────────────────────────────────────┐    │
│  │              .loopr/artifacts/ (flat)                        │    │
│  │  plan-*.md  │  spec-*.md  │  phase-*.md  │  (code files)    │    │
│  │  (hierarchy via parent_id in TaskStore)                      │    │
│  └─────────────────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────────────┘
```

### Data Model

#### Artifact Record (New)

**Key: One Loop produces 1+ Artifacts.** A PlanLoop may produce multiple plan.md files (e.g., alternative approaches). Each artifact tracks its parent for hierarchy building.

```rust
/// An artifact produced by a loop (plan.md, spec.md, phase.md, or code file)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    /// Unique identifier (e.g., "plan-oauth-feature", "spec-auth-module")
    pub id: String,

    /// Type of artifact
    pub artifact_type: ArtifactType,

    /// Parent artifact ID (None for root plans, Some for specs/phases/code)
    /// Used to build hierarchy from flat storage
    pub parent_id: Option<String>,

    /// Loop that produced this artifact (many artifacts can share same loop_id)
    pub loop_id: String,

    /// Path to the file (relative to .loopr/artifacts/)
    pub content_path: PathBuf,

    /// Status for activation flow
    pub status: ArtifactStatus,

    /// Timestamps
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactType {
    Plan,
    Spec,
    Phase,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactStatus {
    Draft,              // Being created by loop
    AwaitingApproval,   // Plan complete, waiting for user
    Active,             // User activated, children can spawn
    Complete,           // All children completed
    Superseded,         // Parent re-iterated, this is stale
}
```

#### Artifact Frontmatter

Every `.md` artifact includes frontmatter for human readability. Note: `loop_id` is NOT included in frontmatter - it lives only in TaskStore's Artifact record.

```markdown
---
id: spec-auth-module
parent: plan-oauth-feature
type: spec
status: active
created_at: 2026-02-02T10:00:00Z
---

# Spec: Authentication Module

...
```

This keeps artifacts clean and human-focused. The loop that produced an artifact can be looked up via TaskStore if needed.

### API Design

#### New/Modified IPC Methods

```rust
/// Create a plan from conversation context
/// Method: "loop.create_plan"
#[derive(Debug, Serialize, Deserialize)]
pub struct CreatePlanRequest {
    /// Conversation history to use as context
    pub messages: Vec<ChatMessage>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreatePlanResponse {
    /// ID of the created PlanLoop
    pub loop_id: String,
}

/// Activate a plan to spawn child SpecLoops
/// Method: "artifact.activate"
#[derive(Debug, Serialize, Deserialize)]
pub struct ActivateArtifactRequest {
    /// Artifact ID to activate
    pub artifact_id: String,
}

/// List artifacts with optional filters
/// Method: "artifact.list"
#[derive(Debug, Serialize, Deserialize)]
pub struct ListArtifactsRequest {
    pub artifact_type: Option<ArtifactType>,
    pub status: Option<ArtifactStatus>,
    pub parent_id: Option<String>,
}

/// Get artifact content and metadata
/// Method: "artifact.get"
#[derive(Debug, Serialize, Deserialize)]
pub struct GetArtifactRequest {
    pub artifact_id: String,
}
```

#### Slash Command Router

In `src/main.rs`, modify the chat input handler:

```rust
// In run_event_loop, ActiveView::Chat branch
if key.is_enter() && !app.state.chat_input.is_empty() && !app.state.is_loading {
    let msg = std::mem::take(&mut app.state.chat_input);

    // Check for slash commands
    if msg.starts_with('/') {
        handle_slash_command(&msg, app, &response_tx).await;
    } else {
        // Existing chat send logic
        let pending_idx = app.add_pending_message(msg.clone());
        // ...
    }
}

async fn handle_slash_command(
    input: &str,
    app: &mut App,
    response_tx: &mpsc::Sender<ChatResponse>,
) {
    let parts: Vec<&str> = input.splitn(2, ' ').collect();
    let cmd = parts[0];

    match cmd {
        "/plan" => {
            // Gather conversation history
            let messages: Vec<ChatMessage> = app.state.chat_messages
                .iter()
                .filter(|m| m.sender != MessageSender::System)
                .cloned()
                .collect();

            if messages.is_empty() {
                app.add_chat_message(
                    MessageSender::System,
                    "No conversation to create plan from. Chat first, then /plan.".to_string(),
                );
                return;
            }

            // Call daemon to create plan
            if let Some(client) = app.client() {
                app.add_chat_message(
                    MessageSender::System,
                    "Creating plan from conversation...".to_string(),
                );
                app.start_loading();

                let tx = response_tx.clone();
                tokio::spawn(async move {
                    let result = client.create_plan(&messages).await;
                    // Send response back
                    // ...
                });
            }
        }
        "/help" => {
            app.add_chat_message(
                MessageSender::System,
                "Commands:\n  /plan - Create a plan from conversation\n  /help - Show this help".to_string(),
            );
        }
        _ => {
            app.add_chat_message(
                MessageSender::System,
                format!("Unknown command: {}", cmd),
            );
        }
    }
}
```

### Artifact-Specific Rule of Five

**Key Design Principle:** Each artifact type has a distinct level of abstraction. The Rule of Five passes are tailored to enforce these boundaries:

| Artifact | Abstraction Level | Character | Forbidden |
|----------|------------------|-----------|-----------|
| plan.md | Strategic | WHAT to build | Implementation details, code |
| spec.md | Tactical | HOW to build it | Vague requirements, missing APIs |
| phase.md | Operational | WHAT SPECIFICALLY to change | Ambiguous acceptance criteria |

Each artifact type gets tailored passes:

#### plan.md Passes (Strategic - WHAT)

| Pass | Focus | Validation |
|------|-------|------------|
| 1. Draft | Get high-level shape | Has Overview, Specs to Create |
| 2. Completeness | All user requirements captured? | Requirements traced to specs |
| 3. Scope | NOT too detailed, NOT too vague | No implementation details, no "TBD" |
| 4. Boundaries | Specs are well-bounded? | Each spec is independent |
| 5. Final | Would you approve this plan? | LLM-as-judge |

**Validation criteria for plan.md:**
- Has Overview section (2-3 paragraphs, no code)
- Has "Specs to Create" section with at least one spec
- No implementation details (no function signatures, no code snippets)
- No TODO, TBD, or placeholder text
- Each spec description is one sentence

#### spec.md Passes (Tactical - HOW)

| Pass | Focus | Validation |
|------|-------|------------|
| 1. Draft | Detailed technical approach | Has Phases, Files, Dependencies |
| 2. Completeness | All technical requirements? | Files listed, APIs defined |
| 3. Feasibility | Is this implementable? | No impossible dependencies |
| 4. Boundaries | Phases are well-bounded? | Each phase is atomic |
| 5. Final | Is this spec ready? | LLM-as-judge |

**Validation criteria for spec.md:**
- References parent plan ID
- Has detailed Phases section (3-7 phases)
- Each phase has Name, Description, Files, Validation
- Lists all files to be modified
- Includes testing strategy

#### phase.md Passes (Operational - WHAT SPECIFICALLY)

| Pass | Focus | Validation |
|------|-------|------------|
| 1. Draft | Specific work items | Has Task, Specific Work, Files |
| 2. Acceptance Criteria | Testable assertions | Each criterion is verifiable |
| 3. Dependencies | What must exist first? | Prerequisites listed |
| 4. Validation | How to know it's done? | Test commands, checks |
| 5. Final | Is this phase ready? | LLM-as-judge |

**Validation criteria for phase.md:**
- References parent spec ID
- Has numbered Specific Work items
- Has checkable Success Criteria (- [ ] format)
- Lists exact file paths
- Includes actual function signatures where relevant

### Concrete Example: Full Flow

User has a conversation in Chat view:
```
User: I want to add OAuth authentication to the API
Daemon: OAuth is a good choice. What providers do you need?
User: Google and GitHub. Also need token refresh.
User: /plan
```

**Step 1: TUI routes /plan**
- TUI detects `/plan` prefix
- Collects non-system messages from chat history
- Calls `client.create_plan(messages)`

**Step 2: Daemon creates PlanLoop**
```rust
Loop {
    id: "p-a1b2",  // Plan loop with 4-char suffix
    loop_type: Plan,
    context: { "conversation": [...], "review_pass": 1 },
    status: Pending,
}
```

**Step 3: PlanLoop runs 5 passes**
- Pass 1 (Draft): Produces initial plan.md with Overview and Specs
- Pass 2 (Completeness): Adds missing requirements (token refresh)
- Pass 3 (Scope): Removes implementation details (was getting too specific)
- Pass 4 (Boundaries): Ensures specs are independent
- Pass 5 (Final): LLM-as-judge approves

**Step 4: Artifacts created (1+ plan.md), status = AwaitingApproval**

PlanLoop produces 1+ plan.md artifacts. In this case, one plan covering all requirements:
```rust
Artifact {
    id: "plan-oauth-auth",
    artifact_type: Plan,
    parent_id: None,  // Root artifact
    loop_id: "p-a1b2",  // Links to Loop (stored in TaskStore only, not in .md)
    status: AwaitingApproval,
    content_path: ".loopr/artifacts/plan-oauth-auth.md",
}
```

(If the LLM determined multiple independent plans were needed, there would be multiple plan artifacts with the same `loop_id`.)

**Step 5: User sees plan(s) in Loops view, presses 'A' to activate**

**Step 6: SpecLoops spawn for each active plan**

Each SpecLoop may produce 1+ spec.md artifacts:
```
.loopr/artifacts/
├── plan-oauth-auth.md           (parent_id: None)
├── spec-google-provider.md      (parent_id: plan-oauth-auth)
├── spec-github-provider.md      (parent_id: plan-oauth-auth)
└── spec-token-refresh.md        (parent_id: plan-oauth-auth)
```

Hierarchy is built via `parent_id` queries, not directory structure.

**Step 7: Cascade continues automatically**
- SpecLoops → spec.md → PhaseLoops → phase.md → CodeLoops → code

**Step 8: Traceability**
Any code change can be traced back:
```
src/auth/google.rs:42
  ← phase-google-impl-001
    ← spec-google-provider
      ← plan-oauth-auth
```

### Implementation Plan

#### Phase 1: Slash Command Router

1. Modify `run_event_loop` in `src/main.rs` to detect `/` prefix
2. Implement `handle_slash_command` function
3. Add `/plan` and `/help` commands
4. Route `/plan` to existing `client.create_plan()` method

#### Phase 2: Artifact Model

1. Add `Artifact` struct to `src/types.rs`
2. Add `artifacts.jsonl` collection to TaskStore
3. Implement `ArtifactManager` in daemon
4. Add frontmatter parsing/writing utilities

#### Phase 3: PlanLoop Integration

1. Modify `handle_loop_create_plan` to accept conversation context
2. Create PlanLoop with context containing conversation
3. Implement plan-specific Rule of Five passes
4. Create Artifact record when plan.md is written
5. Set artifact status to `AwaitingApproval` on loop completion

#### Phase 4: Loops View Enhancement

1. Show loops grouped by parent/child hierarchy
2. Show artifact status indicators
3. Add "Activate" action for plans in AwaitingApproval status

#### Phase 5: Cascade Implementation

1. Implement `artifact.activate` handler
2. On activation, spawn SpecLoops for each spec in plan
3. SpecLoop completion auto-spawns PhaseLoops
4. PhaseLoop completion auto-spawns CodeLoops

#### Phase 6: Artifact Lineage Queries

1. Implement `artifact.list` with parent filter
2. Implement `artifact.get` for content retrieval
3. Add "Show Children" / "Show Parent" in Loops view

## Alternatives Considered

### Alternative 1: Separate Loop Types (PlanLoop, SpecLoop structs)

- **Description:** Create distinct Rust structs for each loop type
- **Pros:** Type-safe, clear separation
- **Cons:** Code duplication, violates existing design principle
- **Why not chosen:** `domain-types.md` explicitly says "Don't create PlanLoop, SpecLoop, PhaseLoop, CodeLoop structs"

### Alternative 2: Artifacts as Loop Output Only (No Artifact Record)

- **Description:** Track lineage only through Loop.parent_id and Loop.output_artifacts
- **Pros:** Simpler, no new record type
- **Cons:** Can't query artifacts independently, harder to track status
- **Why not chosen:** Need artifact-specific status (AwaitingApproval, Active) separate from loop status

### Alternative 3: Frontmatter-Only Lineage (No TaskStore)

- **Description:** Store all lineage in .md frontmatter, parse at runtime
- **Pros:** Human-readable, self-contained files
- **Cons:** Slow queries, no indexing, error-prone parsing
- **Why not chosen:** TaskStore already exists for fast queries; use both (frontmatter for backup/human use)

## Technical Considerations

### Dependencies

- **Internal:** TaskStore, LoopManager, IPC infrastructure
- **External:** None new

### Performance

- Artifact queries use SQLite indexes (fast)
- Loop cascade is async, non-blocking
- Frontmatter parsing only on artifact read

### Security

- No new attack surface (same IPC model)
- Artifacts stored in project-local directory

### Testing Strategy

1. **Unit tests:** Slash command parsing, frontmatter parsing
2. **Integration tests:** Create plan → activate → cascade flow
3. **E2E tests:** TUI /plan → Loops view → Approve → verify artifacts

### Rollout Plan

1. Implement slash command router (minimal change)
2. Add Artifact model to TaskStore
3. Wire /plan to create PlanLoop with conversation
4. Test manually with real conversation
5. Implement cascade (SpecLoop spawning)
6. Full E2E testing

## Edge Cases and Failure Modes

### E1: PlanLoop Fails All Passes

**Scenario:** LLM cannot produce a valid plan after max_iterations.

**Behavior:**
- Loop status → `Failed`
- Artifact status → `Draft` (never progressed to AwaitingApproval)
- User sees failure in Loops view with reason
- User can retry with `/plan` (creates new loop)

### E2: User Never Activates Plan

**Scenario:** Plan sits in AwaitingApproval indefinitely.

**Behavior:**
- No timeout (user's choice to delay)
- Loops view shows "Awaiting Approval" badge
- User can activate any time, even days later
- Multiple plans can be awaiting approval simultaneously

### E3: Multiple Active Cascades

**Scenario:** User activates Plan A while Plan B's cascade is still running.

**Behavior:**
- Allowed - cascades run independently
- Each uses separate worktrees (no file conflicts)
- Loops view shows both hierarchies
- User must manually manage if specs conflict

### E4: Chat Continues After /plan

**Scenario:** User types more messages after `/plan` but before activation.

**Behavior:**
- New messages are regular chat (not part of plan)
- Plan was created from snapshot at `/plan` invocation time
- If user wants to include new context, they should:
  - Reject current plan
  - Continue chatting
  - Run `/plan` again

### E5: Daemon Crash During Loop

**Scenario:** Daemon crashes mid-loop execution.

**Behavior:**
- Covered by `execution-model.md` recovery
- Loop marked as `Pending` on daemon restart
- Worktree preserved, loop resumes
- If worktree lost, loop marked `Failed`

### E6: User Cancels Mid-Cascade

**Scenario:** User cancels an active plan while its children are running.

**Behavior:**
- Plan artifact status → `Superseded`
- Signal sent to all descendant loops: `Stop`
- Descendant loops → `Invalidated`
- Descendant artifacts → `Superseded`
- Worktrees cleaned up

### E7: Parent Re-iterates After Children Started

**Scenario:** SpecLoop fails and re-iterates, but PhaseLoops already started.

**Behavior:**
- Per `loop-architecture.md` "Onion Problem" section
- PhaseLoops receive invalidation signal
- PhaseLoop artifacts → `Superseded`
- New spec.md spawns new PhaseLoops

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Cascade creates too many loops | Medium | High | Add max_children config, user can cancel |
| LLM produces invalid artifact format | Medium | Medium | Validation passes, retry on failure |
| Plan approval blocks indefinitely | Low | Medium | No auto-timeout; user choice. Dashboard shows pending. |
| Artifact lineage becomes inconsistent | Low | High | Transaction-like updates, integrity checks |
| Multiple cascades conflict on same files | Medium | Medium | Separate worktrees; user resolves at merge time |
| User confused about what's in a plan | Low | Medium | Show conversation snapshot that created the plan |

## Design Decisions (Resolved)

| Question | Decision | Rationale |
|----------|----------|-----------|
| Artifacts per loop? | **1+ per loop** | PlanLoop → 1+ plan.md, SpecLoop → 1+ spec.md, PhaseLoop → 1+ phase.md, CodeLoop → 1+ code/tests/docs/config |
| Chat after `/plan`? | **Ignored** - plan uses snapshot at invocation time | See Edge Case E4. User can reject and re-run. |
| Storage location | **Flat**: `.loopr/artifacts/{id}.md` | Hierarchy built via `parent_id` references in TaskStore. |
| Artifact ID format | **Human-readable slugs**: `plan-oauth-auth`, `spec-google-provider` | Better UX in Loops view and traceability. |
| Loops view content | **Both loops and artifacts** | Show Loop ID, status, and the artifacts it produced. |

### Multiple plan.md Scenarios

One PlanLoop produces multiple plan.md files when:
- **Multi-repo coordination** - one plan per repo
- **Multiple domains** - frontend plan, backend plan, infra plan
- **Large projects with discrete parts** - independent workstreams

### Loop ID Encoding

Loop IDs use a type prefix for immediate identification:

| Loop Type | Prefix | Example |
|-----------|--------|---------|
| Plan | `p-` | `p-a1b2` |
| Spec | `s-` | `s-c3d4` |
| Phase | `h-` | `h-e5f6` |
| Code | `c-` | `c-7890` |

**Benefits:**
- Immediately know loop type from ID
- Short (6 chars total)
- No collision between types

### Separation of Concerns

| Thing | ID Format | Where Stored |
|-------|-----------|--------------|
| Loop | `p-a1b2` | loops.jsonl |
| Artifact | `plan-oauth-auth` | artifacts.jsonl + .md frontmatter |
| Link | `Artifact.loop_id` → `Loop.id` | artifacts.jsonl only |

The `.md` frontmatter stays clean (human-focused):
```yaml
---
id: plan-oauth-auth
parent: null
type: plan
status: awaiting_approval
created_at: 2026-02-02T10:00:00Z
---
```

The `loop_id` linkage lives **only** in TaskStore's Artifact record, not in the `.md` file. This keeps artifacts readable without internal implementation details.

### Loop → Artifact Relationships

**Key distinction:**
- **Loop PRODUCES artifacts**: 1 Loop → 1+ artifacts (one-to-many)
- **Loop WORKS ON artifact**: 1 Loop → 1 artifact (one-to-one)

When activated, each artifact spawns exactly one child loop. That child loop may produce multiple artifacts.

```
p-a1b2 (produces 2 artifacts)
  ├── plan-oauth-auth.md ──→ s-c3d4 (works on this, produces 3 artifacts)
  │                            ├── spec-google.md ──→ h-1111
  │                            ├── spec-github.md ──→ h-2222
  │                            └── spec-token.md  ──→ h-3333
  │
  └── plan-infra.md ───────→ s-d5e6 (works on this, produces 1 artifact)
                               └── spec-database.md ──→ h-4444
```

## Open Questions

- [ ] How to handle artifact ID collisions if user creates two similar plans?
- [ ] When a PlanLoop produces multiple plan.md, are they alternatives (user picks one) or all activated together?

See [2026-02-02-loops-view-rendering-options.md](2026-02-02-loops-view-rendering-options.md) for Loops view visualization options.

## Appendix: Technical Details

### A. Loop.output_artifacts vs Artifact Record

Both are used for different purposes:

| Aspect | Loop.output_artifacts | Artifact record |
|--------|----------------------|-----------------|
| Storage | Loop record in loops.jsonl | artifacts.jsonl |
| Cardinality | 1 Loop → N paths | 1 Loop → N Artifact records |
| Purpose | File paths for worktree management | Rich metadata for UI and queries |
| Lifecycle | Loop scope | Can outlive loop |
| Status | None (just paths) | Draft → AwaitingApproval → Active → Complete |
| Hierarchy | None | `parent_id` links to parent artifact |

**Relationship:** One Loop can produce multiple artifacts. All artifacts from the same loop share the same `loop_id`. The `parent_id` field links artifacts across loop boundaries (e.g., spec artifact → plan artifact).

When a loop writes artifact files, it creates an Artifact record for each (see Implementation Plan Phase 3).

### B. Structured Output via tool_use

Per `artifact-tools.md`, the LLM outputs structured data via tool calls, not markdown parsing:

```json
{
    "tool": "plan_output",
    "input": {
        "overview": "Add OAuth 2.0 authentication...",
        "specs": [
            { "name": "google-provider", "description": "Google OAuth integration" },
            { "name": "github-provider", "description": "GitHub OAuth integration" }
        ],
        "success_criteria": ["Users can log in with Google", "..."]
    }
}
```

The daemon receives structured data, stores it for spawning children, AND renders to markdown for human reading.

### C. Prompt Structure

**Recommended: Single Prompt with Pass Injection**

Base prompt (`prompts/plan.md`) + pass-specific addendum:

```rust
impl Loop {
    fn build_prompt(&self) -> String {
        let base = fs::read_to_string(&self.prompt_path)?;
        let pass = self.context["review_pass"].as_u64().unwrap_or(1);
        let pass_addendum = get_pass_addendum(self.loop_type, pass);
        format!("{}\n\n{}", base, pass_addendum)
    }
}

fn get_pass_addendum(loop_type: LoopType, pass: u64) -> &'static str {
    match (loop_type, pass) {
        (Plan, 1) => "Focus on breadth. Get the high-level shape right.",
        (Plan, 2) => "Check completeness. Are all user requirements addressed?",
        (Plan, 3) => "Check scope. Remove implementation details. No code snippets.",
        (Plan, 4) => "Check boundaries. Are specs independent and well-bounded?",
        (Plan, 5) => "Final review. Would you approve this plan?",
        // ... spec and phase variants
    }
}
```

## References

- [loop-architecture.md](../loop-architecture.md) - Loop hierarchy design
- [rule-of-five.md](../rule-of-five.md) - Quality methodology
- [domain-types.md](../domain-types.md) - Unified Loop struct
- [persistence.md](../persistence.md) - TaskStore design
- [prompts/plan.md](../prompts/plan.md) - Plan prompt template
- [prompts/spec.md](../prompts/spec.md) - Spec prompt template
- [prompts/phase.md](../prompts/phase.md) - Phase prompt template
- [2026-02-02-loops-view-rendering-options.md](2026-02-02-loops-view-rendering-options.md) - TUI visualization options
