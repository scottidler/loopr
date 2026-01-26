# Domain Types Specification

**Author:** Scott A. Idler
**Date:** 2026-01-25
**Status:** Implementation Spec
**Parent Doc:** [loop-architecture.md](loop-architecture.md)

---

## Summary

Loopr uses a single `Loop` record type in TaskStore for all loop types (plan, spec, phase, ralph). In Rust code, distinct structs (`PlanLoop`, `SpecLoop`, `PhaseLoop`, `RalphLoop`) provide type-safe APIs for each loop's specific behavior. This gives us unified storage with type-safe code.

---

## Design Principle

**Storage:** One record type, one JSONL file, one table
**Code:** Four distinct structs implementing a common `Loop` trait

```
TaskStore (loops.jsonl)
    └── Loop record (loop_type = "plan" | "spec" | "phase" | "ralph")

Rust Code
    ├── PlanLoop   ─┐
    ├── SpecLoop    │── all implement Loop trait
    ├── PhaseLoop   │── all serialize to/from Loop record
    └── RalphLoop  ─┘
```

---

## Loop Record (TaskStore)

Single record type stored in `loops.jsonl`:

```rust
/// The unified loop record stored in TaskStore
///
/// All loop types (plan, spec, phase, ralph) use this same record.
/// The `loop_type` field discriminates between them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopRecord {
    // Identity
    pub id: String,                    // Timestamp-based: "1737802800"
    pub loop_type: LoopType,           // plan | spec | phase | ralph

    // Hierarchy (the connective tissue)
    pub parent_loop: Option<String>,   // Parent loop ID (None for top-level plan loops)
    pub triggered_by: Option<String>,  // Path to artifact that spawned this loop
    pub conversation_id: Option<String>, // TUI conversation reference

    // State
    pub status: LoopStatus,
    pub iteration: u32,                // Current iteration (1-indexed)
    pub max_iterations: u32,           // Limit before failure

    // Progress (see progress-strategy.md)
    pub progress: String,              // Accumulated iteration feedback

    // Context (loop-type-specific data)
    pub context: serde_json::Value,    // Template variables, task description, etc.

    // Timestamps
    pub created_at: i64,               // Unix ms
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LoopType {
    Plan,
    Spec,
    Phase,
    Ralph,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LoopStatus {
    Pending,      // Waiting to start
    Running,      // Actively iterating
    Paused,       // User paused or rate limited
    Complete,     // Validation passed
    Failed,       // Max iterations or unrecoverable error
    Invalidated,  // Parent re-iterated, this loop is stale
}
```

### Context Field by Loop Type

The `context` JSON varies by loop type:

```rust
// PlanLoop context
{
    "task": "Add OAuth authentication",
    "review_pass": 3,  // Rule of Five pass (1-5)
}

// SpecLoop context
{
    "plan_content": "...",      // Content of triggering plan.md
    "plan_id": "1737800000",
}

// PhaseLoop context
{
    "spec_content": "...",      // Content of triggering spec.md
    "spec_id": "1737801000",
    "phase_number": 2,
    "phase_name": "Implement token validation",
    "phases_total": 5,
}

// RalphLoop context
{
    "task": "Fix the bug in auth.rs:42",
    "phase_content": "...",     // Optional: content of triggering phase.md
    "phase_id": "1737802000",   // Optional: parent phase
}
```

---

## Rust Structs

Each loop type has a distinct struct with type-safe methods.

### Common Trait

```rust
/// Common interface for all loop types
pub trait Loop: Send + Sync {
    /// Get the loop's ID
    fn id(&self) -> &str;

    /// Get the loop type
    fn loop_type(&self) -> LoopType;

    /// Get current status
    fn status(&self) -> LoopStatus;

    /// Get current iteration number
    fn iteration(&self) -> u32;

    /// Convert to storage record
    fn to_record(&self) -> LoopRecord;

    /// Build the prompt for the current iteration
    fn build_prompt(&self, config: &LoopConfig) -> Result<String>;

    /// Get validation command for this loop type
    fn validation_command(&self, config: &LoopConfig) -> &str;

    /// Process validation result and update state
    fn handle_validation(&mut self, result: ValidationResult) -> LoopAction;

    /// Get artifacts produced by this loop (if complete)
    fn artifacts(&self) -> &[Artifact];
}

pub enum LoopAction {
    Continue,           // Keep iterating
    Complete,           // Validation passed, loop done
    SpawnChildren(Vec<LoopRecord>),  // Spawn child loops from artifacts
    Fail(String),       // Give up with reason
}
```

### PlanLoop

```rust
/// Creates high-level plans from user tasks
///
/// Implements Rule of Five: 5 review passes for plan quality.
/// Produces: plan.md artifacts that spawn SpecLoops.
pub struct PlanLoop {
    pub id: String,
    pub status: LoopStatus,
    pub iteration: u32,
    pub max_iterations: u32,
    pub progress: String,

    // Plan-specific
    pub task: String,           // Original user task
    pub review_pass: u32,       // Current Rule of Five pass (1-5)
    pub plan_content: String,   // Current plan draft

    pub created_at: i64,
    pub updated_at: i64,
}

impl PlanLoop {
    pub fn new(task: String, max_iterations: u32) -> Self {
        Self {
            id: generate_loop_id(),
            status: LoopStatus::Pending,
            iteration: 0,
            max_iterations,
            progress: String::new(),
            task,
            review_pass: 1,
            plan_content: String::new(),
            created_at: now_ms(),
            updated_at: now_ms(),
        }
    }

    pub fn from_record(record: LoopRecord) -> Result<Self> {
        ensure!(record.loop_type == LoopType::Plan, "Expected plan loop");
        Ok(Self {
            id: record.id,
            status: record.status,
            iteration: record.iteration,
            max_iterations: record.max_iterations,
            progress: record.progress,
            task: record.context["task"].as_str().unwrap_or("").to_string(),
            review_pass: record.context["review_pass"].as_u64().unwrap_or(1) as u32,
            plan_content: String::new(), // Loaded from artifact file
            created_at: record.created_at,
            updated_at: record.updated_at,
        })
    }

    /// Advance to next Rule of Five pass
    pub fn advance_pass(&mut self) {
        if self.review_pass < 5 {
            self.review_pass += 1;
        }
    }
}

impl Loop for PlanLoop {
    fn loop_type(&self) -> LoopType { LoopType::Plan }

    fn build_prompt(&self, config: &LoopConfig) -> Result<String> {
        let mut ctx = TemplateContext::new();
        ctx.insert("task", &self.task);
        ctx.insert("review_pass", &self.review_pass);
        ctx.insert("current_plan", &self.plan_content);
        ctx.insert("progress", &self.progress);

        render_template(&config.prompt_template, &ctx)
    }

    fn handle_validation(&mut self, result: ValidationResult) -> LoopAction {
        if result.passed {
            if self.review_pass < 5 {
                self.advance_pass();
                LoopAction::Continue
            } else {
                // All 5 passes complete
                LoopAction::Complete
            }
        } else {
            self.iteration += 1;
            if self.iteration >= self.max_iterations {
                LoopAction::Fail("Max iterations reached".into())
            } else {
                LoopAction::Continue
            }
        }
    }

    // ... other trait methods
}
```

### SpecLoop

```rust
/// Creates detailed specifications from plans
///
/// Produces: spec.md artifacts that spawn PhaseLoops.
pub struct SpecLoop {
    pub id: String,
    pub status: LoopStatus,
    pub iteration: u32,
    pub max_iterations: u32,
    pub progress: String,

    // Hierarchy
    pub parent_loop: String,     // PlanLoop ID
    pub triggered_by: String,    // Path to plan.md artifact

    // Spec-specific
    pub plan_content: String,    // Content of parent plan
    pub spec_content: String,    // Current spec draft

    pub created_at: i64,
    pub updated_at: i64,
}

impl SpecLoop {
    pub fn from_plan_artifact(
        parent_loop: &str,
        artifact_path: &str,
        plan_content: String,
        max_iterations: u32,
    ) -> Self {
        Self {
            id: generate_loop_id(),
            status: LoopStatus::Pending,
            iteration: 0,
            max_iterations,
            progress: String::new(),
            parent_loop: parent_loop.to_string(),
            triggered_by: artifact_path.to_string(),
            plan_content,
            spec_content: String::new(),
            created_at: now_ms(),
            updated_at: now_ms(),
        }
    }
}

impl Loop for SpecLoop {
    fn loop_type(&self) -> LoopType { LoopType::Spec }

    fn build_prompt(&self, config: &LoopConfig) -> Result<String> {
        let mut ctx = TemplateContext::new();
        ctx.insert("plan_content", &self.plan_content);
        ctx.insert("current_spec", &self.spec_content);
        ctx.insert("progress", &self.progress);

        render_template(&config.prompt_template, &ctx)
    }

    // ... other trait methods
}
```

### PhaseLoop

```rust
/// Implements individual phases from specs
///
/// Produces: code files + phase.md artifacts that spawn RalphLoops.
pub struct PhaseLoop {
    pub id: String,
    pub status: LoopStatus,
    pub iteration: u32,
    pub max_iterations: u32,
    pub progress: String,

    // Hierarchy
    pub parent_loop: String,     // SpecLoop ID
    pub triggered_by: String,    // Path to spec.md artifact

    // Phase-specific
    pub spec_content: String,    // Content of parent spec
    pub phase_number: u32,       // Which phase (1-indexed)
    pub phase_name: String,      // "Implement token validation"
    pub phases_total: u32,       // Total phases in spec

    pub created_at: i64,
    pub updated_at: i64,
}

impl Loop for PhaseLoop {
    fn loop_type(&self) -> LoopType { LoopType::Phase }

    fn build_prompt(&self, config: &LoopConfig) -> Result<String> {
        let mut ctx = TemplateContext::new();
        ctx.insert("spec_content", &self.spec_content);
        ctx.insert("phase_number", &self.phase_number);
        ctx.insert("phase_name", &self.phase_name);
        ctx.insert("phases_total", &self.phases_total);
        ctx.insert("progress", &self.progress);

        render_template(&config.prompt_template, &ctx)
    }

    // ... other trait methods
}
```

### RalphLoop

```rust
/// General-purpose implementation loop
///
/// The workhorse. Executes tasks with full tool access.
/// Produces: code files, documentation, tests.
pub struct RalphLoop {
    pub id: String,
    pub status: LoopStatus,
    pub iteration: u32,
    pub max_iterations: u32,
    pub progress: String,

    // Hierarchy (optional - ralph loops can be standalone)
    pub parent_loop: Option<String>,   // PhaseLoop ID (if spawned from phase)
    pub triggered_by: Option<String>,  // Path to phase.md artifact

    // Ralph-specific
    pub task: String,                  // Task description
    pub phase_content: Option<String>, // Content of parent phase (if any)

    pub created_at: i64,
    pub updated_at: i64,
}

impl RalphLoop {
    /// Create standalone ralph loop (no parent)
    pub fn standalone(task: String, max_iterations: u32) -> Self {
        Self {
            id: generate_loop_id(),
            status: LoopStatus::Pending,
            iteration: 0,
            max_iterations,
            progress: String::new(),
            parent_loop: None,
            triggered_by: None,
            task,
            phase_content: None,
            created_at: now_ms(),
            updated_at: now_ms(),
        }
    }

    /// Create ralph loop from phase artifact
    pub fn from_phase_artifact(
        parent_loop: &str,
        artifact_path: &str,
        phase_content: String,
        task: String,
        max_iterations: u32,
    ) -> Self {
        Self {
            id: generate_loop_id(),
            status: LoopStatus::Pending,
            iteration: 0,
            max_iterations,
            progress: String::new(),
            parent_loop: Some(parent_loop.to_string()),
            triggered_by: Some(artifact_path.to_string()),
            task,
            phase_content: Some(phase_content),
            created_at: now_ms(),
            updated_at: now_ms(),
        }
    }
}

impl Loop for RalphLoop {
    fn loop_type(&self) -> LoopType { LoopType::Ralph }

    fn build_prompt(&self, config: &LoopConfig) -> Result<String> {
        let mut ctx = TemplateContext::new();
        ctx.insert("task", &self.task);
        if let Some(ref content) = self.phase_content {
            ctx.insert("phase_content", content);
        }
        ctx.insert("progress", &self.progress);

        render_template(&config.prompt_template, &ctx)
    }

    // ... other trait methods
}
```

---

## Conversion Functions

```rust
/// Convert any Loop impl to a LoopRecord for storage
impl<T: Loop> From<&T> for LoopRecord {
    fn from(loop_impl: &T) -> Self {
        loop_impl.to_record()
    }
}

/// Load the appropriate struct from a record
pub fn load_loop(record: LoopRecord) -> Result<Box<dyn Loop>> {
    match record.loop_type {
        LoopType::Plan => Ok(Box::new(PlanLoop::from_record(record)?)),
        LoopType::Spec => Ok(Box::new(SpecLoop::from_record(record)?)),
        LoopType::Phase => Ok(Box::new(PhaseLoop::from_record(record)?)),
        LoopType::Ralph => Ok(Box::new(RalphLoop::from_record(record)?)),
    }
}
```

---

## Storage Layout

```
~/.loopr/<project-hash>/
├── .taskstore/
│   ├── loops.jsonl           # All loop records (unified)
│   └── taskstore.db          # SQLite index cache
├── loops/
│   └── <loop-id>/
│       ├── iterations/
│       │   └── 001/
│       │       ├── prompt.md
│       │       ├── conversation.jsonl
│       │       ├── validation.log
│       │       └── artifacts/
│       │           ├── plan.md      # PlanLoop artifact
│       │           ├── spec.md      # SpecLoop artifact
│       │           └── phase.md     # PhaseLoop artifact
│       └── current -> iterations/NNN/
└── archive/
    └── <loop-id>/            # Invalidated loops
```

---

## ID Format

Loop IDs are timestamp-based for natural sorting:

```
1737802800     # Unix timestamp (seconds)
```

This ensures:
- Globally unique (one loop per second max)
- Sortable by creation time
- Simple to generate
- Easy to reference in CLI

---

## Status Transitions

All loop types follow the same state machine:

```
pending → running        (loop execution starts)
running → paused         (user action, rate limit)
running → complete       (validation passes, work done)
running → failed         (max iterations, error)
running → invalidated    (parent re-iterated)
paused → running         (resume)
paused → invalidated     (parent re-iterated while paused)
```

---

## Indexed Fields

For fast TaskStore queries:

```rust
impl Record for LoopRecord {
    fn indexed_fields(&self) -> HashMap<String, IndexValue> {
        let mut fields = HashMap::new();
        fields.insert("loop_type".into(), IndexValue::String(self.loop_type.to_string()));
        fields.insert("status".into(), IndexValue::String(self.status.to_string()));
        if let Some(ref parent) = self.parent_loop {
            fields.insert("parent_loop".into(), IndexValue::String(parent.clone()));
        }
        fields
    }
}
```

Common queries:
- `SELECT * FROM loops WHERE status = 'running'`
- `SELECT * FROM loops WHERE parent_loop = '1737800000'`
- `SELECT * FROM loops WHERE loop_type = 'ralph' AND status = 'pending'`

---

## References

- [loop-architecture.md](loop-architecture.md) - Loop hierarchy and storage layout
- [loop-coordination.md](loop-coordination.md) - How loops coordinate via TaskStore
- [loop-config.md](loop-config.md) - Per-loop-type configuration
- [progress-strategy.md](progress-strategy.md) - Progress field management
