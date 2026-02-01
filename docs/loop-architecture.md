# Loop Architecture

**Author:** Scott A. Idler
**Date:** 2026-01-30
**Status:** Implementation Spec
**Based on:** loopr/docs/loop-architecture.md (adapted for daemon substrate)

---

## Summary

Loopr orchestrates hierarchical loops for autonomous software development. Four loop types (Plan, Spec, Phase, Code) form a hierarchy where each level produces artifacts that spawn child loops. Every loop follows the Ralph Wiggum pattern: fresh context per iteration, prompt updated with failure feedback, iterate until validation passes.

**See [loop.md](loop.md) for the complete Loop specification.**

In v2, loops are managed by the **Daemon** process, with tool execution delegated to **Runners**.

---

## The Ralph Wiggum Pattern

Key insight from Geoffrey Huntley: fresh context each iteration prevents "context rot" - the LLM doesn't accumulate confusion from failed attempts.

```bash
# Original technique
while :; do cat PROMPT.md | claude ; done
```

**Loopr extends this to hierarchical loops:**

1. **Fresh context window** each iteration (no LLM memory)
2. **Prompt** = base prompt + feedback from previous iteration
3. **Validation** determines if iteration succeeded or needs retry
4. **Iterate** until validation passes or max iterations reached
5. **Produce artifacts** that spawn child loops

---

## Loop Hierarchy

```
User request
└── PlanLoop (1)
    └── produces plan.md
        └── spawns SpecLoop (1 per spec in plan)
            └── produces spec.md
                └── spawns PhaseLoop (3-7 per spec)
                    └── produces phase.md
                        └── spawns CodeLoop (1 per phase)
                            └── produces code/docs/tests
```

### Artifacts as Connective Tissue

| Parent Loop | Produces Artifact | Spawns Child |
|-------------|-------------------|--------------|
| PlanLoop | `plan.md` | SpecLoop |
| SpecLoop | `spec.md` | PhaseLoop |
| PhaseLoop | `phase.md` | CodeLoop |
| CodeLoop | code files | (none - leaf node) |

**A child loop is always spawned *from* a specific artifact produced by its parent.** The artifact is the contract between layers.

---

## Loop Lifecycle

```
pending → running → [iterating] → complete
                 ↘             ↗
                   failed/paused
                        ↓
                   invalidated (if parent re-iterates)
```

### State Transitions

| From | To | Trigger |
|------|----|---------|
| `pending` | `running` | Scheduler picks loop |
| `running` | `complete` | Validation passed |
| `running` | `failed` | Max iterations or unrecoverable error |
| `running` | `paused` | User action or rate limit |
| `paused` | `running` | User resume |
| `*` | `invalidated` | Parent re-iterated |

---

## Loop Execution (Daemon-Managed)

In v2, the **Daemon's LoopManager** executes loops. The `Loop` struct is self-contained - it has everything needed to run itself:

```rust
// In daemon process
impl LoopManager {
    pub async fn run_loop(&self, mut loop_instance: Loop) -> Result<()> {
        // Create worktree and set it on the loop
        loop_instance.worktree = self.create_worktree(&loop_instance).await?;

        loop {
            // Check for signals (stop, pause)
            if let Some(signal) = self.check_signals(&loop_instance.id).await? {
                match signal.signal_type {
                    SignalType::Stop => {
                        self.mark_invalidated(&loop_instance.id).await?;
                        break;
                    }
                    SignalType::Pause => {
                        self.wait_for_resume(&loop_instance.id).await?;
                    }
                }
            }

            // Build prompt for this iteration (Loop has its own prompt_path)
            let prompt = loop_instance.build_prompt()?;

            // Call LLM (daemon handles API call)
            let response = self.llm_client.chat(&prompt, &self.tools_for(loop_instance.loop_type)).await?;

            // Execute tool calls via runners
            for tool_call in response.tool_calls {
                let result = self.execute_tool(&loop_instance, tool_call).await?;
                // Feed result back to LLM...
            }

            // Run validation (Loop has its own validation_command)
            let validation = loop_instance.validate().await?;

            // Update state based on result
            match loop_instance.handle_validation(validation) {
                LoopAction::Continue => {
                    self.update_iteration(&loop_instance.id).await?;
                    continue;
                }
                LoopAction::Complete => {
                    self.mark_complete(&loop_instance.id).await?;
                    self.spawn_children(&loop_instance).await?;
                    break;
                }
                LoopAction::Fail(reason) => {
                    self.mark_failed(&loop_instance.id, &reason).await?;
                    break;
                }
            }
        }

        // Cleanup worktree
        self.cleanup_worktree(&loop_instance.id).await?;
        Ok(())
    }

    /// Execute tool via appropriate runner subprocess
    async fn execute_tool(
        &self,
        loop_instance: &Loop,
        tool_call: ToolCall,
    ) -> Result<ToolResult> {
        // Determine lane from tool catalog
        let lane = self.tool_catalog.get_lane(&tool_call.name)?;

        // Route to runner
        let job = ToolJob {
            job_id: generate_job_id(),
            agent_id: loop_instance.id.clone(),
            tool_name: tool_call.name,
            command: self.tool_catalog.build_command(&tool_call)?,
            cwd: loop_instance.worktree.clone(),
            worktree_dir: loop_instance.worktree.clone(),
            timeout_ms: self.tool_catalog.get_timeout(&tool_call.name),
            max_output_bytes: 100_000,
            ..Default::default()
        };

        self.tool_router.submit(lane, job).await
    }
}
```

---

## User Gate: Plan Approval Protocol

The only user intervention point in the loop hierarchy is after a PlanLoop completes validation. This "user gate" prevents autonomous execution of code without human review.

### Approval States

```
PlanLoop
   │
   │ (iterates until validation passes)
   │
   ▼
AwaitingApproval ──────────────────────────────────┐
   │                                               │
   ├── [User Approves] ───► spawn SpecLoops        │
   │                                               │
   ├── [User Rejects] ────► mark Failed            │
   │                                               │
   └── [User Iterates] ───► add feedback,          │
                            run another iteration──┘
```

### IPC Protocol

**TUI → Daemon Messages:**

```rust
/// Approve the plan and spawn child loops
#[derive(Debug, Serialize, Deserialize)]
struct PlanApprove {
    id: String,  // Plan loop ID
}

/// Reject the plan
#[derive(Debug, Serialize, Deserialize)]
struct PlanReject {
    id: String,
    reason: Option<String>,  // Optional rejection reason
}

/// Request another iteration with feedback
#[derive(Debug, Serialize, Deserialize)]
struct PlanIterate {
    id: String,
    feedback: String,  // User's feedback to incorporate
}

/// Get plan content for display
#[derive(Debug, Serialize, Deserialize)]
struct PlanGetPreview {
    id: String,
}
```

**Daemon → TUI Events:**

```rust
/// Plan is ready for user review
#[derive(Debug, Serialize, Deserialize)]
struct PlanAwaitingApproval {
    id: String,
    content: String,      // Full plan.md content
    specs: Vec<SpecInfo>, // Parsed spec list from plan
}

#[derive(Debug, Serialize, Deserialize)]
struct SpecInfo {
    name: String,
    description: String,
}

/// Plan was approved, specs spawning
#[derive(Debug, Serialize, Deserialize)]
struct PlanApproved {
    id: String,
    specs_spawned: u32,
}

/// Plan was rejected
#[derive(Debug, Serialize, Deserialize)]
struct PlanRejected {
    id: String,
    reason: Option<String>,
}
```

### Daemon Handling

```rust
impl LoopManager {
    /// Handle plan approval from TUI
    async fn handle_plan_approve(&mut self, plan_id: &str) -> Result<u32> {
        let plan = self.state.get_loop(plan_id).await?;

        // Validate state
        if plan.status != LoopStatus::Complete {
            return Err(eyre!("Plan {} not in Complete state", plan_id));
        }
        if plan.loop_type != LoopType::Plan {
            return Err(eyre!("Loop {} is not a Plan", plan_id));
        }

        // Get structured artifact data (NOT parsed from markdown - see artifact-tools.md)
        let plan_artifact: PlanArtifact = self.get_plan_artifact(plan_id).await?;

        // Spawn spec loops from structured data
        for (index, spec) in plan_artifact.specs.iter().enumerate() {
            let spec_loop = Loop::new_spec(&plan, spec, (index + 1) as u32);
            self.spawn_loop(spec_loop).await?;
        }

        // Emit approval event
        self.emit(Event::PlanApproved {
            id: plan_id.to_string(),
            specs_spawned: specs.len() as u32,
        });

        Ok(specs.len() as u32)
    }

    /// Handle plan rejection from TUI
    async fn handle_plan_reject(&mut self, plan_id: &str, reason: Option<String>) -> Result<()> {
        let mut plan = self.state.get_loop(plan_id).await?;

        plan.status = LoopStatus::Failed;
        plan.progress.push_str(&format!(
            "\n\n## User Rejected\n{}",
            reason.as_deref().unwrap_or("No reason provided")
        ));
        self.state.update_loop(&plan).await?;

        self.emit(Event::PlanRejected {
            id: plan_id.to_string(),
            reason,
        });

        Ok(())
    }

    /// Handle iterate request from TUI
    async fn handle_plan_iterate(&mut self, plan_id: &str, feedback: &str) -> Result<()> {
        let mut plan = self.state.get_loop(plan_id).await?;

        // Add feedback to progress
        plan.progress.push_str(&format!(
            "\n\n---\n## User Feedback (Iteration {})\n{}",
            plan.iteration + 1,
            feedback
        ));
        plan.status = LoopStatus::Running;
        plan.iteration += 1;
        self.state.update_loop(&plan).await?;

        // Restart the loop
        self.run_loop(plan).await?;

        Ok(())
    }

    /// Called when PlanLoop validation passes
    async fn on_plan_complete(&mut self, plan_id: &str) -> Result<()> {
        let plan = self.state.get_loop(plan_id).await?;

        // Get structured artifact data (NOT parsed from markdown - see artifact-tools.md)
        let plan_artifact: PlanArtifact = self.get_plan_artifact(plan_id).await?;

        // Read markdown for human display
        let artifact_path = plan.output_artifacts.first()
            .ok_or_else(|| eyre!("Plan has no output artifact"))?;
        let content = fs::read_to_string(artifact_path)?;

        // Emit awaiting approval event to TUI
        self.emit(Event::PlanAwaitingApproval {
            id: plan_id.to_string(),
            content,
            specs: plan_artifact.specs.iter().map(|s| SpecInfo {
                name: s.name.clone(),
                description: s.description.clone(),
            }).collect(),
        });

        // Loop is now blocked waiting for user action
        // TUI will call approve/reject/iterate
        Ok(())
    }
}
```

### TUI Display

When `plan.awaiting_approval` event is received, TUI shows:

```
┌─────────────────────────────────────────────────────────────────┐
│                     PLAN AWAITING APPROVAL                       │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  Plan: Add OAuth Authentication                                  │
│  Loop ID: 1738300800123-a1b2                                     │
│  Iterations: 3                                                   │
│                                                                  │
│  ## Overview                                                     │
│  Add OAuth 2.0 authentication to the API with token refresh...   │
│                                                                  │
│  ## Phases                                                       │
│  1. Database schema for tokens                                   │
│  2. OAuth endpoints                                              │
│  3. Token validation middleware                                  │
│                                                                  │
│  ## Specs to Create (3)                                          │
│  • spec-db-schema: Database tables for OAuth                     │
│  • spec-endpoints: OAuth API endpoints                           │
│  • spec-middleware: Token validation                             │
│                                                                  │
├─────────────────────────────────────────────────────────────────┤
│  [A] Approve    [R] Reject    [I] Iterate with feedback          │
└─────────────────────────────────────────────────────────────────┘
```

### Important Notes

1. **Only PlanLoops have user gates** - Spec, Phase, and Code loops execute autonomously
2. **Blocking is explicit** - The daemon waits for TUI response, does not auto-approve
3. **Timeout behavior** - If TUI disconnects, plan remains in Complete state until reconnect
4. **Multiple TUIs** - First to approve/reject wins, others see updated state

---

## The Onion Problem: Invalidation

When an outer layer re-iterates, inner layers become stale.

### Example: Deep Cascade

```
PlanLoop iter 1 → plan-v1.md
  └── SpecLoop iter 1-3 → spec-v1.md
        └── PhaseLoop → phase-v1.md, phase-v2.md, phase-v3.md
              └── CodeLoop-A → 200 lines of auth code
              └── CodeLoop-B → 300 lines of API code
              └── CodeLoop-C → 150 lines of tests

NOW: SpecLoop validation fails, needs iter 4
     SpecLoop iter 4 → spec-v2.md

QUESTION: What happens to PhaseLoop's work? CodeLoop's code?
```

### Solution: Signal Cascade + Archive

1. **Parent signals descendants** - Write stop signal with `target_selector: "descendants:<parent-id>"`
2. **Children detect signal** - On next iteration boundary, see stop signal
3. **Archive invalidated loops** - Move loop directories to `archive/`
4. **Git branches preserved** - Each loop's work remains on its branch for reference
5. **New children spawned** - Parent's new artifact spawns fresh children

```rust
impl LoopManager {
    async fn invalidate_descendants(&self, parent_id: &str) -> Result<()> {
        // Write stop signal for all descendants
        let signal = SignalRecord {
            id: generate_signal_id(),
            signal_type: SignalType::Stop,
            source_loop: parent_id.to_string(),
            target_selector: Some(format!("descendants:{}", parent_id)),
            reason: "Parent re-iterating".to_string(),
            created_at: now_ms(),
            acknowledged_at: None,
        };
        self.store.create(&signal)?;

        // Wait for children to acknowledge (with timeout)
        self.wait_for_acknowledgments(parent_id, Duration::from_secs(30)).await?;

        // Archive invalidated loops
        let descendants = self.find_descendants(parent_id).await?;
        for child in descendants {
            self.archive_loop(&child.id).await?;
        }

        Ok(())
    }
}
```

---

## Storage Layout

```
~/.loopr/<project-hash>/
├── .taskstore/
│   ├── loops.jsonl           # All loop records
│   ├── signals.jsonl         # Coordination signals
│   └── taskstore.db          # SQLite index cache
├── loops/
│   └── <loop-id>/
│       ├── iterations/
│       │   └── 001/
│       │       ├── prompt.md           # What we sent to LLM
│       │       ├── conversation.jsonl  # LLM responses + tool calls
│       │       ├── validation.log      # Why it failed (if it did)
│       │       └── artifacts/          # What this iteration produced
│       │           ├── plan.md
│       │           ├── spec.md
│       │           └── phase.md
│       ├── stdout.log                  # Aggregate stdout
│       ├── stderr.log                  # Aggregate stderr
│       └── current -> iterations/NNN/  # Symlink to latest
└── archive/                            # Invalidated loops
    └── <loop-id>/
```

---

## Loop Struct

There is no separate "Loop" - `Loop` IS the record. When we serialize to JSONL, we serialize `Loop`. When we deserialize, we get `Loop` back.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Loop {
    // Identity
    pub id: String,                    // Timestamp + random suffix: "1738300800123-a1b2"
    pub loop_type: LoopType,           // plan | spec | phase | code

    // Hierarchy
    pub parent_id: Option<String>,     // Parent loop ID
    pub input_artifact: Option<PathBuf>, // Parent's output artifact
    pub output_artifacts: Vec<PathBuf>,  // This loop's outputs

    // Behavior Configuration
    pub prompt_path: PathBuf,
    pub validation_command: String,
    pub max_iterations: u32,

    // Workspace
    pub worktree: PathBuf,

    // State
    pub status: LoopStatus,
    pub iteration: u32,
    pub progress: String,              // Accumulated feedback
    pub context: serde_json::Value,    // Loop-type-specific data

    // Timestamps
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoopType {
    Plan,
    Spec,
    Phase,
    Code,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoopStatus {
    Pending,
    Running,
    Paused,
    Rebasing,      // Stopped for rebase after sibling merge
    Complete,
    Failed,
    Invalidated,
}
```

---

## How Fresh Context + Learning Works

Since each iteration starts with a fresh LLM context, the **prompt itself must be updated**:

```
Iteration 1 prompt:
  "Create a plan for feature X..."

Iteration 2 prompt:
  "Create a plan for feature X...

   PREVIOUS ATTEMPT FAILED:
   - Missing security section
   - No migration strategy

   Please address these issues."

Iteration 3 prompt:
  "Create a plan for feature X...

   PREVIOUS ATTEMPTS:
   - Iteration 1: Missing security section, no migration strategy
   - Iteration 2: Security added but migration still incomplete

   Please complete the migration strategy."
```

The `progress` field in Loop accumulates this feedback.

---

## Key Operations

| Operation | Implementation |
|-----------|----------------|
| List running loops | `SELECT * FROM loops WHERE status='running'` |
| Find artifacts | `ls <loop-id>/current/artifacts/` |
| Stream logs | TUI subscribes to daemon events |
| Resume failed loop | Daemon creates new iteration |
| Find children | `SELECT * FROM loops WHERE parent_id='...'` |
| Detect stale children | Compare `input_artifact` iteration to parent's current |

---

## Integration with Daemon

### Loop Manager Tick

```rust
impl LoopManager {
    /// Called periodically by daemon main loop
    pub async fn tick(&mut self) -> Result<()> {
        // Find pending loops ready to run
        let pending = self.store.query::<Loop>(&[
            Filter::eq("status", "pending"),
        ])?;

        // Check capacity
        let running = self.running_loops.len();
        let slots = self.config.max_concurrent_loops - running;

        // Prioritize and spawn
        let to_run = self.scheduler.select(pending, slots);
        for record in to_run {
            self.spawn_loop(record).await?;
        }

        // Reap completed
        self.reap_completed().await?;

        Ok(())
    }
}
```

### Tool Execution via Runners

When a loop needs to execute a tool:

1. LoopManager builds ToolJob
2. ToolRouter determines lane from catalog
3. Job sent to appropriate runner via IPC
4. Runner executes in sandbox, returns result
5. Result fed back to LLM

---

## References

- [artifact-tools.md](artifact-tools.md) - Structured output via tool_use (no markdown parsing)
- [loop-coordination.md](loop-coordination.md) - Signal-based coordination
- [scheduler.md](scheduler.md) - Priority model
- [execution-model.md](execution-model.md) - Worktree lifecycle
- [domain-types.md](domain-types.md) - Full type definitions
- [runners.md](runners.md) - Tool execution substrate
