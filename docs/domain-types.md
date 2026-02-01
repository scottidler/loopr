# Domain Types

**Author:** Scott A. Idler
**Date:** 2026-01-31
**Status:** Implementation Spec

---

## Summary

Loopr uses a **unified `Loop` struct** for all loop types. Behavior differences come from configuration (`LoopConfig`), not from separate Rust types. This document defines the core data types.

**See [loop.md](loop.md) for the complete Loop specification.**

---

## Design Principle: One Loop, Configuration-Driven

**Don't create PlanLoop, SpecLoop, PhaseLoop, CodeLoop structs.** The differences between loop types don't justify separate types - they're all configuration.

| Aspect | PlanLoop | SpecLoop | PhaseLoop | CodeLoop |
|--------|----------|----------|-----------|----------|
| **Core iteration logic** | Same | Same | Same | Same |
| **Prompt template** | Different | Different | Different | Different |
| **Validation command** | Different | Different | Different | Different |
| **Input artifact** | None | plan.md | spec.md | phase.md |
| **Output artifact** | plan.md | spec.md | phase.md | code/docs |
| **Spawns children?** | Yes (specs) | Yes (phases) | Yes (code) | No |

The **behavior** (Ralph Wiggum iteration pattern) is identical. The **differences** are all **data**: prompts, validators, parsers, artifact paths.

**Make data different, not code different.**

---

## The Two Types

### 1. LoopConfig (Template, Loaded Once)

Used **once** when creating a new Loop. Defines default behavior for each loop type. Loaded from YAML at daemon startup, shared across all loops of that type.

```rust
pub struct LoopConfig {
    pub prompt_template: PathBuf,
    pub validation_command: String,
    pub max_iterations: u32,
    pub child_type: Option<LoopType>,  // What to spawn (None for Code)
    pub artifact_parser: Option<ParserType>,
}
```

### 2. Loop (Self-Contained Instance)

The `Loop` struct is what gets created, persisted to TaskStore, and tracked. There is no separate "LoopRecord" - `Loop` IS the record. When we serialize to JSONL, we serialize `Loop`. When we deserialize, we get `Loop` back.

**Loop is self-contained.** It has the prompt path, validation command, max iterations, and worktree path. It can run itself - no separate "runner" needed.

---

## Why Two Types (Not Three)

The previous design had three types: `LoopConfig` → `Loop` → `LoopRunner`. But `LoopRunner` was unnecessary:

- Config values are **already copied** to Loop at creation
- Worktree path is **already stored** in Loop

`LoopRunner` added nothing. It was indirection without value.

**Two types are sufficient:**
1. **LoopConfig** - Template for creating loops
2. **Loop** - Self-contained instance with `impl Loop { fn run() }`

The separation between "what a loop is" and "how to run it" is unnecessary when the loop already contains everything needed to run.

---

## Design Principle

**One struct, four types via enum.**

Behavior is determined by:
- `loop_type` field (Plan, Spec, Phase, Code)
- `prompt_path` (different prompt template per type)
- `validation_command` (different validation per type)
- `context` (type-specific data as JSON)

NOT by separate Rust structs with different `impl` blocks.

```
┌─────────────────────────────────────────────┐
│                  Loop                        │
│                                              │
│  loop_type: Plan | Spec | Phase | Code      │
│  prompt_path: prompts/{type}.md             │
│  validation_command: type-specific          │
│  context: { type-specific JSON }            │
│                                              │
│  All use the same iteration logic:          │
│  prompt → LLM → tools → validate → repeat   │
└─────────────────────────────────────────────┘
```

---

## The Loop Struct

```rust
/// The core abstraction in Loopr.
/// Runs in a tokio task, iterates with fresh context until validation passes.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub input_artifact: Option<PathBuf>,

    /// The artifact(s) this loop produces
    pub output_artifacts: Vec<PathBuf>,

    //=== Behavior Configuration ===

    /// Path to the prompt template for this loop type
    pub prompt_path: PathBuf,

    /// Command to validate this loop's output
    pub validation_command: String,

    /// Maximum iterations before failure
    pub max_iterations: u32,

    //=== Workspace ===

    /// Git worktree path for this loop's work
    pub worktree: PathBuf,

    //=== Runtime State ===

    /// Current iteration number (0-indexed)
    pub iteration: u32,

    /// Current status
    pub status: LoopStatus,

    /// Accumulated feedback from failed iterations
    /// Injected into prompt, NOT into conversation history
    pub progress: String,

    /// Loop-type-specific context data (JSON)
    pub context: serde_json::Value,

    //=== Timestamps ===

    pub created_at: i64,  // Unix ms
    pub updated_at: i64,
}

impl Loop {
    /// Create a new Loop from type config
    pub fn new(loop_type: LoopType, config: &LoopConfig, context: Value) -> Self {
        Self {
            id: generate_loop_id(),
            loop_type,
            parent_id: None,
            input_artifact: None,
            output_artifacts: vec![],
            prompt_path: config.prompt_template.clone(),
            validation_command: config.validation_command.clone(),
            max_iterations: config.max_iterations,
            worktree: PathBuf::new(), // Set by LoopManager
            iteration: 0,
            status: LoopStatus::Pending,
            progress: String::new(),
            context,
            created_at: now_ms(),
            updated_at: now_ms(),
        }
    }

    /// Run the loop - self-contained, no separate runner needed
    pub async fn run(
        &mut self,
        llm: &dyn LlmClient,
        tools: &ToolRouter,
        state: &StateManager,
    ) -> LoopOutcome {
        while self.iteration < self.max_iterations {
            // Build prompt with accumulated feedback
            let prompt = self.build_prompt()?;

            // Call LLM with fresh context
            let response = llm.complete(CompletionRequest {
                system: prompt,
                messages: vec![Message::user(&self.context["task"])],
                tools: tools.definitions_for(self.loop_type),
                max_tokens: 8192,
            }).await?;

            // Execute tools
            for call in response.tool_calls {
                tools.execute(call, &self.worktree).await?;
            }

            // Validate
            let result = self.validate().await?;

            if result.passed {
                self.status = LoopStatus::Complete;
                state.update(self).await?;
                return LoopOutcome::Complete;
            }

            // Accumulate feedback for next iteration
            self.progress.push_str(&format!(
                "\n---\nIteration {} failed: {}\n",
                self.iteration + 1,
                result.feedback
            ));
            self.iteration += 1;
            state.update(self).await?;
        }

        self.status = LoopStatus::Failed;
        LoopOutcome::Failed
    }

    fn build_prompt(&self) -> Result<String> {
        let template = fs::read_to_string(&self.prompt_path)?;
        let mut prompt = render_template(&template, &self.context)?;

        if !self.progress.is_empty() {
            prompt.push_str(&format!(
                "\n\n## Previous Attempts\n{}\n",
                self.progress
            ));
        }

        Ok(prompt)
    }

    async fn validate(&self) -> Result<ValidationResult> {
        let output = Command::new("sh")
            .arg("-c")
            .arg(&self.validation_command)
            .current_dir(&self.worktree)
            .output()
            .await?;

        if output.status.success() {
            Ok(ValidationResult::passed())
        } else {
            Ok(ValidationResult::failed(
                String::from_utf8_lossy(&output.stderr)
            ))
        }
    }
}
```

---

## Enums

### LoopType

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LoopType {
    Plan,   // Produces plan.md
    Spec,   // Produces spec.md
    Phase,  // Produces phase.md
    Code,   // Produces code/docs in worktree
}
```

### LoopStatus

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LoopStatus {
    Pending,      // Waiting to start
    Running,      // Actively iterating
    Paused,       // User-initiated pause (resumable)
    Rebasing,     // Stopped for rebase after sibling merge (see worktree-coordination.md)
    Complete,     // Validation passed, artifacts produced
    Failed,       // Max iterations exhausted
    Invalidated,  // Parent re-iterated, work is stale
}

impl LoopStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Complete | Self::Failed | Self::Invalidated)
    }

    pub fn is_resumable(&self) -> bool {
        matches!(self, Self::Paused)
    }

    pub fn is_rebasing(&self) -> bool {
        matches!(self, Self::Rebasing)
    }
}
```

---

## Context Field by Loop Type

The `context` field holds type-specific data as JSON:

```rust
// PlanLoop context
{
    "task": "Add OAuth authentication"
}

// SpecLoop context
{
    "plan_id": "001",
    "plan_content": "..."  // Or read from input_artifact
}

// PhaseLoop context
{
    "spec_id": "001-001",
    "phase_number": 2,
    "phase_name": "Implement token validation",
    "phases_total": 5
}

// CodeLoop context
{
    "phase_id": "001-001-002",
    "task": "Create migration file for OAuth tokens"
}
```

---

## Configuration by Loop Type

| Field | Plan | Spec | Phase | Code |
|-------|------|------|-------|------|
| `prompt_path` | `prompts/plan.md` | `prompts/spec.md` | `prompts/phase.md` | `prompts/code.md` |
| `validation_command` | `loopr validate plan` | `loopr validate spec` | `loopr validate phase` | `cargo test` |
| `max_iterations` | 50 | 30 | 20 | 100 |
| `input_artifact` | None | plan.md | spec.md | phase.md |
| `output_artifacts` | [plan.md] | [spec.md, ...] | [phase.md] | [] |

---

## Other Records

### SignalRecord

Used for loop-to-loop coordination (stop, pause, invalidate).

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalRecord {
    pub id: String,
    pub signal_type: SignalType,
    pub source_loop: Option<String>,
    pub target_loop: Option<String>,
    pub target_selector: Option<String>,  // e.g., "descendants:001"
    pub reason: String,
    pub payload: Option<Value>,
    pub created_at: i64,
    pub acknowledged_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SignalType {
    Stop,        // Terminate immediately
    Pause,       // Suspend execution (resumable)
    Resume,      // Continue paused loop
    Rebase,      // Stop, rebase worktree, continue (see worktree-coordination.md)
    Error,       // Report problem upstream
    Info,        // Advisory message
    Invalidate,  // Parent re-iterated, work is stale
}
```

### ToolJobRecord

Audit trail for tool executions.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolJobRecord {
    pub id: String,
    pub loop_id: String,
    pub iteration: u32,
    pub tool_name: String,
    pub lane: String,            // "no-net", "net", "heavy"
    pub input_summary: String,   // Truncated for storage
    pub output_summary: String,  // Truncated for storage
    pub status: ToolJobStatus,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolJobStatus {
    Success,
    Failed,
    Timeout,
    Cancelled,
}
```

### EventRecord

General-purpose event log for observability. **See [observability.md](observability.md) for the complete event system.**

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    pub id: String,
    pub event_type: String,      // "loop.started", "iteration.complete", etc.
    pub loop_id: Option<String>,
    pub payload: Value,
    pub created_at: i64,
}
```

---

## ID Generation

```rust
use std::time::{SystemTime, UNIX_EPOCH};
use rand::Rng;

/// Generate a loop ID (timestamp + random suffix to prevent collisions)
/// Format: "1738300800123-a1b2" (milliseconds since epoch + 4-char hex suffix)
pub fn generate_loop_id() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let suffix: u16 = rand::thread_rng().gen();
    format!("{}-{:04x}", ts, suffix)
}

/// Generate a hierarchical loop ID
/// e.g., "001-002" for second spec under plan 001
pub fn generate_child_id(parent_id: &str, child_index: u32) -> String {
    format!("{}-{:03}", parent_id, child_index)
}

/// Generate a signal ID
pub fn generate_signal_id() -> String {
    format!("sig-{}", generate_loop_id())
}

/// Generate a tool job ID
pub fn generate_job_id(loop_id: &str, iteration: u32) -> String {
    format!("job-{}-{}-{}", loop_id, iteration, rand::random::<u16>())
}
```

---

## Loop Constructors

```rust
impl Loop {
    /// Create a new PlanLoop from a user task
    pub fn new_plan(task: &str) -> Self {
        Self {
            id: generate_loop_id(),
            loop_type: LoopType::Plan,
            parent_id: None,
            input_artifact: None,
            output_artifacts: vec![],
            prompt_path: PathBuf::from("prompts/plan.md"),
            validation_command: "loopr validate plan".into(),
            max_iterations: 50,
            worktree: PathBuf::new(),  // Set by LoopManager
            iteration: 0,
            status: LoopStatus::Pending,
            progress: String::new(),
            context: json!({ "task": task }),
            created_at: now_ms(),
            updated_at: now_ms(),
        }
    }

    /// Create a SpecLoop from a parent PlanLoop
    pub fn new_spec(parent: &Loop, spec_index: u32) -> Self {
        Self {
            id: generate_child_id(&parent.id, spec_index),
            loop_type: LoopType::Spec,
            parent_id: Some(parent.id.clone()),
            input_artifact: parent.output_artifacts.first().cloned(),
            output_artifacts: vec![],
            prompt_path: PathBuf::from("prompts/spec.md"),
            validation_command: "loopr validate spec".into(),
            max_iterations: 30,
            worktree: PathBuf::new(),
            iteration: 0,
            status: LoopStatus::Pending,
            progress: String::new(),
            context: json!({ "plan_id": parent.id }),
            created_at: now_ms(),
            updated_at: now_ms(),
        }
    }

    /// Create a PhaseLoop from a parent SpecLoop
    pub fn new_phase(parent: &Loop, phase_index: u32, phase_name: &str, phases_total: u32) -> Self {
        Self {
            id: generate_child_id(&parent.id, phase_index),
            loop_type: LoopType::Phase,
            parent_id: Some(parent.id.clone()),
            input_artifact: parent.output_artifacts.first().cloned(),
            output_artifacts: vec![],
            prompt_path: PathBuf::from("prompts/phase.md"),
            validation_command: "loopr validate phase".into(),
            max_iterations: 20,
            worktree: PathBuf::new(),
            iteration: 0,
            status: LoopStatus::Pending,
            progress: String::new(),
            context: json!({
                "spec_id": parent.id,
                "phase_number": phase_index,
                "phase_name": phase_name,
                "phases_total": phases_total
            }),
            created_at: now_ms(),
            updated_at: now_ms(),
        }
    }

    /// Create a CodeLoop from a parent PhaseLoop
    pub fn new_code(parent: &Loop) -> Self {
        Self {
            id: generate_child_id(&parent.id, 1),
            loop_type: LoopType::Code,
            parent_id: Some(parent.id.clone()),
            input_artifact: parent.output_artifacts.first().cloned(),
            output_artifacts: vec![],  // Code produces files in worktree, not artifact files
            prompt_path: PathBuf::from("prompts/code.md"),
            validation_command: "cargo test".into(),  // Or from config
            max_iterations: 100,
            worktree: PathBuf::new(),
            iteration: 0,
            status: LoopStatus::Pending,
            progress: String::new(),
            context: json!({ "phase_id": parent.id }),
            created_at: now_ms(),
            updated_at: now_ms(),
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}
```

---

## TaskStore Collections

| Collection | Record Type | File |
|------------|-------------|------|
| loops | Loop | `loops.jsonl` |
| signals | SignalRecord | `signals.jsonl` |
| tool_jobs | ToolJobRecord | `tool_jobs.jsonl` |
| events | EventRecord | `events.jsonl` |

---

## References

- **[loop.md](loop.md)** - Core Loop specification (the essential document)
- [persistence.md](persistence.md) - TaskStore design
- [loop-coordination.md](loop-coordination.md) - Signal-based coordination
- [observability.md](observability.md) - Event system and logging
