# Loop

**Author:** Scott A. Idler
**Date:** 2026-01-31
**Status:** Core Specification

---

## Summary

The **Loop** is the core abstraction in Loopr. It implements the Ralph Wiggum pattern: an iterative loop that calls an LLM with fresh context on each iteration until validation passes. This document specifies the Loop concept, its four types, and how they compose into a hierarchy that produces software.

---

## The Ralph Wiggum Pattern

### Origin

The Ralph Wiggum technique, created by Geoffrey Huntley, is deceptively simple:

```bash
while :; do cat PROMPT.md | claude ; done
```

Each iteration:
1. Starts with **fresh context** (no conversation history)
2. Reads current state from files
3. Does work
4. Writes results to files
5. Exits (loop continues)

The insight: **fresh context prevents context rot**. Long conversations degrade LLM performance. By restarting fresh each iteration, the LLM operates at peak capability indefinitely.

### What We're Building

We productionalize this pattern:

| Original Ralph | Loopr |
|----------------|-------|
| Bash while loop | Tokio async task |
| Spawn `claude` process | Async HTTP to Anthropic API |
| One loop | Four loop types in hierarchy |
| Manual orchestration | Daemon with LoopManager |
| Files on disk | TaskStore + git worktrees |

**We are NOT Gas Town.** We don't spawn hundreds of OS processes (~200MB each). We run tokio tasks (~2MB each) making async HTTP calls. Fresh context means fresh `messages` array in the API call, not fresh process.

---

## The Loop Struct

```rust
pub struct Loop {
    //=== Identity ===

    /// Unique identifier (timestamp + random suffix: "1738300800123-a1b2")
    pub id: String,

    /// What kind of loop: Plan, Spec, Phase, or Code
    pub loop_type: LoopType,

    /// Parent loop that spawned this one (None for root PlanLoop)
    pub parent_id: Option<String>,

    //=== Artifacts ===

    /// The artifact this loop consumes (parent's output)
    /// - PlanLoop: None (user request is the input)
    /// - SpecLoop: path to plan.md
    /// - PhaseLoop: path to spec.md
    /// - CodeLoop: path to phase.md
    pub input_artifact: Option<PathBuf>,

    /// The artifact(s) this loop produces
    /// - PlanLoop: ["plans/001.plan.md"]
    /// - SpecLoop: ["specs/001-001.spec.md", "specs/001-002.spec.md"]
    /// - PhaseLoop: ["phases/001-001-001.phase.md"]
    /// - CodeLoop: [] (produces code in worktree, not artifact files)
    pub output_artifacts: Vec<PathBuf>,

    //=== Behavior Configuration ===

    /// Path to the prompt template for this loop type
    /// e.g., "prompts/plan.md", "prompts/code.md"
    pub prompt_path: PathBuf,

    /// Command to validate this loop's output
    /// e.g., "loopr validate plan", "cargo test"
    pub validation_command: String,

    /// Maximum iterations before failure
    pub max_iterations: u32,

    //=== Workspace ===

    /// Git worktree path for this loop's work
    pub worktree: PathBuf,

    //=== Runtime State ===

    /// Current iteration number (0-indexed, increments on failure)
    pub iteration: u32,

    /// Current status
    pub status: LoopStatus,

    /// Accumulated feedback from failed iterations
    /// Injected into prompt, NOT into conversation history
    pub progress: String,

    /// Loop-type-specific context data
    /// - PlanLoop: { "task": "Add OAuth authentication" }
    /// - SpecLoop: { "plan_content": "..." }
    /// - PhaseLoop: { "spec_content": "...", "phase_number": 2 }
    /// - CodeLoop: { "phase_content": "...", "task": "Implement token validation" }
    pub context: serde_json::Value,

    //=== Timestamps ===

    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopType {
    Plan,
    Spec,
    Phase,
    Code,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopStatus {
    Pending,      // Waiting to start
    Running,      // Actively iterating
    Paused,       // User-initiated pause (resumable)
    Rebasing,     // Stopped for rebase after sibling merge (see worktree-coordination.md)
    Complete,     // Validation passed, artifacts produced
    Failed,       // Max iterations exhausted
    Invalidated,  // Parent re-iterated, this loop's work is stale
}
```

---

## The Four Loop Types

### Each Loop Has ONE Job: Produce Its Artifact(s)

A loop does not know or care what happens downstream. It consumes an input artifact, iterates until validation passes, and produces output artifact(s). That's it.

### PlanLoop

**Job:** Produce `plan.md` file(s) from a user request.

```
Input:  User's task description ("Add OAuth authentication")
Output: plans/001-add-oauth.plan.md
```

**Validation:**
- Format check: Required sections present (## Overview, ## Phases, ## Success Criteria)
- LLM-as-Judge: "Is this plan complete and feasible?"
- **User gate:** User must approve before Specs spawn

**What it produces:**

```markdown
# Plan: Add OAuth Authentication

## Overview
Add OAuth 2.0 authentication to the API...

## Phases
1. Database schema for tokens
2. OAuth endpoints
3. Token validation middleware
4. Integration tests

## Success Criteria
- Users can authenticate via OAuth
- Tokens are securely stored
- All endpoints protected

## Specs to Create
- spec-db-schema: Database tables for OAuth
- spec-endpoints: OAuth API endpoints
```

### SpecLoop

**Job:** Produce `spec.md` file(s) from a plan.

```
Input:  plans/001-add-oauth.plan.md
Output: specs/001-001-db-schema.spec.md
```

**Validation:**
- Format check: Required sections present
- LLM-as-Judge: "Does this spec correctly decompose the plan?"
- Auto-approved (no user gate)

**What it produces:**

```markdown
# Spec: OAuth Database Schema

## Parent Plan
001-add-oauth

## Overview
Create database tables for OAuth token storage...

## Phases
1. Create migrations
2. Create models
3. Create repository layer

## Files to Modify
- src/db/migrations/
- src/models/oauth.rs
- src/repositories/token.rs
```

### PhaseLoop

**Job:** Produce `phase.md` file(s) from a spec.

```
Input:  specs/001-001-db-schema.spec.md
Output: phases/001-001-001-migrations.phase.md
```

**Validation:**
- Format check: Required sections present
- LLM-as-Judge: "Is this phase clearly defined and atomic?"
- Auto-approved (no user gate)

**What it produces:**

```markdown
# Phase: Create OAuth Migrations

## Parent Spec
001-001-db-schema

## Task
Create database migrations for OAuth token tables.

## Specific Work
1. Create migration file: 20260131_create_oauth_tokens.sql
2. Add columns: id, user_id, access_token, refresh_token, expires_at
3. Add indexes on user_id and access_token

## Success Criteria
- Migration runs without errors
- Tables are created with correct schema
```

### CodeLoop

**Job:** Produce code/docs from a phase.

```
Input:  phases/001-001-001-migrations.phase.md
Output: (code changes in worktree)
```

**Validation:**
- Real validation: `cargo test`, `cargo clippy`, `otto ci`
- Tests must pass
- No user gate

**What it produces:**

```
src/db/migrations/20260131_create_oauth_tokens.sql  (new)
src/db/migrations/mod.rs                            (modified)
```

---

## The Artifact Hierarchy

```
.loopr/
├── plans/
│   └── 001-add-oauth.plan.md              ← PlanLoop output
│
├── specs/
│   ├── 001-001-db-schema.spec.md          ← SpecLoop outputs
│   └── 001-002-endpoints.spec.md
│
├── phases/
│   ├── 001-001-001-migrations.phase.md    ← PhaseLoop outputs
│   ├── 001-001-002-models.phase.md
│   ├── 001-002-001-auth-routes.phase.md
│   └── 001-002-002-token-handler.phase.md
│
└── worktrees/
    ├── code-001-001-001/                  ← CodeLoop workspaces
    ├── code-001-001-002/
    └── ...
```

**Artifacts are first-class outputs.** They are:
- Git-tracked (audit trail)
- Human-reviewable (transparency)
- The contract between layers (interface)
- What triggers child loop spawning (coordination)

---

## The Iteration Model

### Fresh Context, Every Time

```rust
impl Loop {
    /// Runs in a tokio task. NOT a separate OS process.
    pub async fn run(
        &mut self,
        llm: Arc<dyn LlmClient>,
        tools: Arc<ToolRouter>,
        state: Arc<StateManager>,
    ) -> LoopOutcome {

        while self.iteration < self.max_iterations {
            // ============================================
            // FRESH CONTEXT: New messages array each time
            // ============================================

            // 1. Build prompt with accumulated feedback
            let system = render_template(&self.prompt_path, &self.context)?;
            let user_message = format!(
                "{}\n\n## Previous Iteration Feedback\n{}",
                self.context["task"],
                self.progress
            );

            // 2. Fresh API call - NO conversation history
            let response = llm.complete(CompletionRequest {
                system,
                messages: vec![Message::user(user_message)],  // Just ONE message
                tools: tools.definitions_for(self.loop_type),
                max_tokens: 8192,
            }).await?;

            // 3. Execute tool calls (routed to runner subprocesses)
            for call in response.tool_calls {
                let result = tools.execute(call, &self.worktree).await?;
                // Tool results go back to LLM in same iteration if needed
            }

            // 4. Validate output
            let validation = self.validate(&tools).await?;

            if validation.passed {
                self.status = LoopStatus::Complete;
                self.checkpoint(&state).await?;
                return LoopOutcome::Complete {
                    iterations: self.iteration + 1,
                    artifacts: self.output_artifacts.clone(),
                };
            }

            // 5. Accumulate feedback for next iteration
            self.progress.push_str(&format!(
                "\n\n---\n## Iteration {} Failed\n{}\n",
                self.iteration + 1,
                validation.output
            ));
            self.iteration += 1;
            self.checkpoint(&state).await?;

            // ============================================
            // ITERATION ENDS - Context is discarded
            // Next iteration starts completely fresh
            // ============================================
        }

        self.status = LoopStatus::Failed;
        self.checkpoint(&state).await?;
        LoopOutcome::Failed {
            reason: "Max iterations exhausted".into(),
            iterations: self.iteration,
        }
    }
}
```

### What Persists vs What's Fresh

| Persists Across Iterations | Fresh Each Iteration |
|---------------------------|---------------------|
| Loop identity (id, type, parent) | LLM conversation (messages array) |
| Iteration count | Tool execution context |
| Accumulated `progress` feedback | API request/response |
| Worktree files | In-memory processing state |
| Output artifacts | |
| Status | |

### Why Fresh Context Matters

**Without fresh context (naive approach):**
```
Iteration 1: messages = [user1, assistant1]
Iteration 2: messages = [user1, assistant1, user2, assistant2]
Iteration 3: messages = [user1, assistant1, user2, assistant2, user3, assistant3]
...
Iteration 50: messages = [...100+ messages, context window full, LLM confused]
```

**With fresh context (Ralph Wiggum):**
```
Iteration 1: messages = [user_with_no_feedback]
Iteration 2: messages = [user_with_feedback_from_iter1]
Iteration 3: messages = [user_with_feedback_from_iter1_and_2]
...
Iteration 50: messages = [user_with_accumulated_feedback]  // Still just ONE message
```

The feedback is **in the prompt**, not in conversation history. This keeps context small and focused.

---

## Validation Per Loop Type

### PlanLoop Validation

```rust
fn validate_plan(artifact: &Path) -> ValidationResult {
    let content = fs::read_to_string(artifact)?;

    // Format checks
    let has_overview = content.contains("## Overview");
    let has_phases = content.contains("## Phases");
    let has_criteria = content.contains("## Success Criteria");

    if !has_overview || !has_phases || !has_criteria {
        return ValidationResult::failed("Missing required sections");
    }

    // LLM-as-Judge (optional, configurable)
    if config.use_llm_judge {
        let judgment = llm.complete(format!(
            "Review this plan for completeness and feasibility:\n\n{}\n\n\
             Answer only PASS or FAIL with brief reason.",
            content
        )).await?;

        if !judgment.contains("PASS") {
            return ValidationResult::failed(judgment);
        }
    }

    ValidationResult::passed()
}
```

### SpecLoop Validation

```rust
fn validate_spec(artifact: &Path, parent_plan: &Path) -> ValidationResult {
    let content = fs::read_to_string(artifact)?;
    let plan = fs::read_to_string(parent_plan)?;

    // Format checks
    let has_parent_ref = content.contains("## Parent Plan");
    let has_phases = content.contains("## Phases");

    if !has_parent_ref || !has_phases {
        return ValidationResult::failed("Missing required sections");
    }

    // LLM-as-Judge: Does spec align with plan?
    if config.use_llm_judge {
        let judgment = llm.complete(format!(
            "Does this spec correctly decompose part of the plan?\n\n\
             PLAN:\n{}\n\nSPEC:\n{}\n\n\
             Answer only PASS or FAIL with brief reason.",
            plan, content
        )).await?;

        if !judgment.contains("PASS") {
            return ValidationResult::failed(judgment);
        }
    }

    ValidationResult::passed()
}
```

### PhaseLoop Validation

```rust
fn validate_phase(artifact: &Path) -> ValidationResult {
    let content = fs::read_to_string(artifact)?;

    // Format checks
    let has_task = content.contains("## Task");
    let has_work = content.contains("## Specific Work");
    let has_criteria = content.contains("## Success Criteria");

    if !has_task || !has_work || !has_criteria {
        return ValidationResult::failed("Missing required sections");
    }

    // LLM-as-Judge: Is task clear and atomic?
    if config.use_llm_judge {
        let judgment = llm.complete(format!(
            "Is this phase clearly defined and appropriately scoped?\n\n{}\n\n\
             Answer only PASS or FAIL with brief reason.",
            content
        )).await?;

        if !judgment.contains("PASS") {
            return ValidationResult::failed(judgment);
        }
    }

    ValidationResult::passed()
}
```

### CodeLoop Validation

```rust
fn validate_code(worktree: &Path, command: &str) -> ValidationResult {
    // Real validation - run tests
    let output = Command::new("sh")
        .arg("-c")
        .arg(command)  // e.g., "cargo test && cargo clippy"
        .current_dir(worktree)
        .output()?;

    if output.status.success() {
        ValidationResult::passed()
    } else {
        ValidationResult::failed(String::from_utf8_lossy(&output.stderr))
    }
}
```

---

## The Onion: Loop Hierarchy

### How Loops Spawn Children

```
User: "Add OAuth authentication"
              │
              ▼
┌────────────────────────────────────────────────────────────────┐
│ PlanLoop (id: 001)                                             │
│   iteration 1: produces plan.md (fails validation)             │
│   iteration 2: produces plan.md (fails validation)             │
│   iteration 3: produces plan.md (passes!) ──────────┐          │
│                                                     │          │
│   [USER APPROVES]                                   │          │
└─────────────────────────────────────────────────────│──────────┘
                                                      │
              ┌───────────────────────────────────────┘
              │ plan.md defines 2 specs
              ▼
┌─────────────────────────────┐  ┌─────────────────────────────┐
│ SpecLoop (id: 001-001)      │  │ SpecLoop (id: 001-002)      │
│   parent: 001               │  │   parent: 001               │
│   input: plan.md            │  │   input: plan.md            │
│   produces: spec-001.md     │  │   produces: spec-002.md     │
└──────────────┬──────────────┘  └──────────────┬──────────────┘
               │                                │
               │ spec defines 2 phases          │ spec defines 2 phases
               ▼                                ▼
┌──────────────────┐ ┌──────────────────┐ ┌──────────────────┐ ...
│ PhaseLoop        │ │ PhaseLoop        │ │ PhaseLoop        │
│ (id: 001-001-001)│ │ (id: 001-001-002)│ │ (id: 001-002-001)│
│ produces:        │ │ produces:        │ │ produces:        │
│ phase-001.md     │ │ phase-002.md     │ │ phase-003.md     │
└────────┬─────────┘ └────────┬─────────┘ └────────┬─────────┘
         │                    │                    │
         ▼                    ▼                    ▼
┌──────────────────┐ ┌──────────────────┐ ┌──────────────────┐
│ CodeLoop         │ │ CodeLoop         │ │ CodeLoop         │
│ (id: ...-001)    │ │ (id: ...-002)    │ │ (id: ...-003)    │
│ produces: code   │ │ produces: code   │ │ produces: code   │
└──────────────────┘ └──────────────────┘ └──────────────────┘
```

### Spawning Logic

When a loop completes, the LoopManager reads its output artifacts and spawns children:

```rust
impl LoopManager {
    async fn on_loop_complete(&mut self, loop_id: &str) -> Result<()> {
        let parent = self.state.get_loop(loop_id).await?;

        match parent.loop_type {
            LoopType::Plan => {
                // User gate: wait for approval
                if !self.wait_for_user_approval(&parent).await? {
                    return Ok(());  // User rejected, don't spawn
                }

                // Parse plan.md to find specs to create
                let specs = parse_plan_specs(&parent.output_artifacts[0])?;
                for spec in specs {
                    self.spawn_loop(Loop::new_spec(&parent, spec)).await?;
                }
            }

            LoopType::Spec => {
                // Parse spec.md to find phases
                let phases = parse_spec_phases(&parent.output_artifacts[0])?;
                for phase in phases {
                    self.spawn_loop(Loop::new_phase(&parent, phase)).await?;
                }
            }

            LoopType::Phase => {
                // Spawn CodeLoop for this phase
                self.spawn_loop(Loop::new_code(&parent)).await?;
            }

            LoopType::Code => {
                // Leaf node, nothing to spawn
                // Check if all siblings complete for merge
                self.check_merge_ready(&parent).await?;
            }
        }

        Ok(())
    }
}
```

### Artifact Parsing

Artifact parsing extracts structured data from markdown files to spawn child loops.

#### SpecDescriptor (from Plan)

```rust
/// Extracted from plan.md's "## Specs to Create" section
#[derive(Debug, Clone)]
pub struct SpecDescriptor {
    pub name: String,        // e.g., "db-schema"
    pub description: String, // e.g., "Database tables for OAuth"
    pub index: u32,          // Position in list (1-indexed)
}

/// Parse plan.md to extract specs to create
pub fn parse_plan_specs(plan_path: &Path) -> Result<Vec<SpecDescriptor>> {
    let content = fs::read_to_string(plan_path)?;

    // Find "## Specs to Create" section
    let specs_section = extract_section(&content, "## Specs to Create")
        .ok_or_else(|| eyre!("Plan missing '## Specs to Create' section"))?;

    // Parse list items: "- spec-<name>: <description>"
    let mut specs = Vec::new();
    let re = Regex::new(r"^-\s+spec-([a-z0-9-]+):\s*(.+)$")?;

    for (index, line) in specs_section.lines().enumerate() {
        if let Some(caps) = re.captures(line.trim()) {
            specs.push(SpecDescriptor {
                name: caps[1].to_string(),
                description: caps[2].to_string(),
                index: (index + 1) as u32,
            });
        }
    }

    if specs.is_empty() {
        return Err(eyre!("No specs found in plan"));
    }

    Ok(specs)
}
```

#### PhaseDescriptor (from Spec)

```rust
/// Extracted from spec.md's "## Phases" section
#[derive(Debug, Clone)]
pub struct PhaseDescriptor {
    pub number: u32,         // Phase number (1-indexed)
    pub name: String,        // e.g., "Create migrations"
    pub description: String, // Full phase description
    pub files: Vec<String>,  // Files to modify (if specified)
}

/// Parse spec.md to extract phases
pub fn parse_spec_phases(spec_path: &Path) -> Result<Vec<PhaseDescriptor>> {
    let content = fs::read_to_string(spec_path)?;

    // Find "## Phases" section
    let phases_section = extract_section(&content, "## Phases")
        .ok_or_else(|| eyre!("Spec missing '## Phases' section"))?;

    // Parse numbered phases
    // Format:
    // 1. **Phase Name**
    //    Description text
    //    - Files: file1.rs, file2.rs

    let mut phases = Vec::new();
    let mut current_phase: Option<PhaseDescriptor> = None;
    let phase_header_re = Regex::new(r"^(\d+)\.\s+\*\*(.+?)\*\*")?;
    let files_re = Regex::new(r"^\s*-?\s*Files?:\s*(.+)$")?;

    for line in phases_section.lines() {
        if let Some(caps) = phase_header_re.captures(line) {
            // Save previous phase
            if let Some(phase) = current_phase.take() {
                phases.push(phase);
            }
            // Start new phase
            current_phase = Some(PhaseDescriptor {
                number: caps[1].parse()?,
                name: caps[2].to_string(),
                description: String::new(),
                files: Vec::new(),
            });
        } else if let Some(ref mut phase) = current_phase {
            // Add to current phase description
            if let Some(caps) = files_re.captures(line) {
                phase.files = caps[1]
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .collect();
            } else if !line.trim().is_empty() {
                if !phase.description.is_empty() {
                    phase.description.push('\n');
                }
                phase.description.push_str(line.trim());
            }
        }
    }

    // Don't forget the last phase
    if let Some(phase) = current_phase {
        phases.push(phase);
    }

    if phases.is_empty() {
        return Err(eyre!("No phases found in spec"));
    }

    Ok(phases)
}
```

#### Section Extraction Helper

```rust
/// Extract content from a markdown section until the next ## heading
fn extract_section(content: &str, heading: &str) -> Option<String> {
    let start = content.find(heading)?;
    let after_heading = &content[start + heading.len()..];

    // Find next ## heading or end of content
    let end = after_heading
        .find("\n## ")
        .unwrap_or(after_heading.len());

    Some(after_heading[..end].trim().to_string())
}
```

#### Expected Artifact Formats

**Plan.md - Specs to Create section:**
```markdown
## Specs to Create
- spec-db-schema: Database tables for OAuth tokens
- spec-endpoints: OAuth API endpoints (/auth, /token, /refresh)
- spec-middleware: Token validation middleware
```

**Spec.md - Phases section:**
```markdown
## Phases

1. **Create migration file**
   Add SQL migration for oauth_tokens table.
   - Files: src/db/migrations/20260131_oauth_tokens.sql

2. **Create model structs**
   Define Rust structs for OAuth tokens.
   - Files: src/models/oauth.rs, src/models/mod.rs

3. **Add repository layer**
   Implement CRUD operations for tokens.
   - Files: src/repositories/token.rs
```

---

## User Gate: Plan Approval

The only user intervention point is after PlanLoop completes:

```
PlanLoop produces plan.md
         │
         ▼
┌─────────────────────────────────────────┐
│         AWAITING USER APPROVAL          │
│                                         │
│  Plan: Add OAuth Authentication         │
│                                         │
│  Overview:                              │
│    Add OAuth 2.0 authentication...      │
│                                         │
│  Phases:                                │
│    1. Database schema                   │
│    2. OAuth endpoints                   │
│    3. Token validation                  │
│                                         │
│  [Approve]  [Reject]  [Iterate]         │
└─────────────────────────────────────────┘
```

- **Approve:** Spawn SpecLoops, execution continues autonomously
- **Reject:** Mark PlanLoop as failed, stop
- **Iterate:** Force another PlanLoop iteration with feedback

Everything below Plan level is autonomous. No user gates for Spec, Phase, or Code.

---

## What Happens When Loops Fail

### Iteration Failure (Recoverable)

If validation fails, the loop iterates again with accumulated feedback:

```
Iteration 1: Write code → Tests fail → Capture error output
Iteration 2: Prompt includes error → Write better code → Tests fail
Iteration 3: Prompt includes both errors → Write code → Tests pass!
```

### Max Iterations Exhausted (Loop Failure)

If a loop hits `max_iterations`, it's marked `Failed`:

```rust
// CodeLoop fails after 100 iterations
loop.status = LoopStatus::Failed;

// Parent PhaseLoop is notified
// PhaseLoop can: retry (spawn new CodeLoop) or fail itself
```

### Parent Re-iteration (Invalidation Cascade)

If a parent loop re-iterates after children have started:

```
PlanLoop (iteration 3) → spawned SpecLoops
    │
    │ User requests changes to plan
    │
    ▼
PlanLoop (iteration 4) → NEW plan.md
    │
    │ Old SpecLoops are now stale
    ▼
Signal::Invalidate sent to all descendants
    │
    ▼
All child loops mark status = Invalidated
Their worktrees are archived (not deleted)
Their artifacts are kept (for reference)
    │
    ▼
New SpecLoops spawned from new plan.md
```

**Nothing merges to main until the entire hierarchy completes successfully.**

---

## Concurrency Model

### Many Loops, One Daemon

```
Daemon Process
    │
    ├── LoopManager
    │       │
    │       ├── Loop (tokio task) ──async──→ Anthropic API
    │       ├── Loop (tokio task) ──async──→ Anthropic API
    │       ├── Loop (tokio task) ──async──→ Anthropic API
    │       └── ... (50+ concurrent loops, ~2MB each)
    │
    └── Runners (subprocesses for tool execution)
            ├── runner-no-net (10 slots, sandboxed)
            ├── runner-net (5 slots, network allowed)
            └── runner-heavy (1 slot, builds/tests)
```

### Why This Works

- **Tokio tasks are cheap:** ~2MB per loop vs ~200MB per OS process (Gas Town)
- **HTTP is async:** LLM API calls don't block, we can have many in flight
- **Tools are isolated:** Runner subprocesses handle sandboxing
- **State is centralized:** TaskStore is the single source of truth

---

## Summary

1. **Loop** is the core abstraction - an iterative executor with fresh context per iteration
2. **Four types:** Plan, Spec, Phase, Code - each produces its artifact(s)
3. **Fresh context:** New `messages` array each API call, feedback in prompt not history
4. **Artifacts are first-class:** plan.md, spec.md, phase.md are versioned outputs
5. **Hierarchy:** Plan → Spec → Phase → Code, each layer spawns the next
6. **User gate:** Only at Plan level, everything else is autonomous
7. **Validation:** Format + LLM-as-Judge for docs, real tests for code
8. **Tokio, not processes:** Efficient async execution, not Gas Town

This is the productionalized Ralph Wiggum pattern.

---

## References

- [Ralph Wiggum Loop](https://ghuntley.com/ralph/) - Original concept by Geoffrey Huntley
- [Gas Town](https://steve-yegge.medium.com/welcome-to-gas-town-4f25ee16dd04) - What we're NOT doing
- [architecture.md](architecture.md) - System architecture
- [persistence.md](persistence.md) - TaskStore design
- [runners.md](runners.md) - Tool execution sandboxing
