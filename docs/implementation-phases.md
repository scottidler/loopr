# Implementation Phases

**Author:** Scott A. Idler
**Date:** 2026-01-31
**Status:** Implementation Roadmap

---

## Overview

This document defines the 16 implementation phases for building Loopr v2. Each phase:

- **Builds on previous phases** - progressive, incremental development
- **Produces working code** - the codebase compiles and tests pass after each phase
- **Is self-contained** - clear deliverables and validation
- **Links to specs** - reference docs for implementation details

**Validation for all phases:** `otto ci` (cargo check, clippy, fmt --check, test)

---

## Phase Summary

| # | Phase | Description | Est. Iterations |
|---|-------|-------------|-----------------|
| 1 | Project Foundation | Scaffold, errors, IDs, utilities | 2-3 |
| 2 | Domain Types | Loop, Signal, ToolJob, Event records | 2-3 |
| 3 | Storage Layer | JSONL persistence with queries | 2-3 |
| 4 | LLM Client | Anthropic API with streaming and tool parsing | 3-4 |
| 5 | Tool System | Definitions, catalog, router trait | 2-3 |
| 6 | Prompt System | Template loading and rendering | 2-3 |
| 7 | Validation System | Format and command validators | 2-3 |
| 8 | Single Loop Execution | Loop::run() with fresh context pattern | 3-4 |
| 9 | Artifact Parsing | Extract specs/phases from markdown artifacts | 2-3 |
| 10 | Worktree Management | Git worktree create/cleanup/branch ops | 2-3 |
| 11 | Loop Coordination | Signals, acknowledgment, invalidation cascade | 2-3 |
| 12 | Loop Manager | Orchestrates loops, spawns children | 3-4 |
| 13 | Daemon Core | Scheduler, tick loop, crash recovery | 3-4 |
| 14 | IPC Layer | Unix socket server, messages, event broadcast | 3-4 |
| 15 | TUI Client | Chat view, loops view, plan approval | 4-5 |
| 16 | CLI & Integration | Entry point, subcommands, end-to-end test | 3-4 |

**Total estimated iterations: 45-55**

**Recommended MAX_ITERATIONS: 100** (with buffer for retries)

---

## Phase 1: Project Foundation

**Goal:** Create the Cargo project with error handling, ID generation, and utility functions.

**Docs:**
- [README.md](README.md) - Project overview
- [domain-types.md](domain-types.md) - ID generation section

**Dependencies:**
```bash
cargo add thiserror eyre
cargo add serde --features derive
cargo add serde_json
cargo add rand
cargo add tokio --features full
```

**Files to create:**
```
Cargo.toml
src/lib.rs
src/main.rs
src/error.rs
src/id.rs
```

**Deliverables:**

1. **Cargo.toml** - Package with dependencies
2. **src/error.rs** - Error types using thiserror
   ```rust
   #[derive(Debug, Error)]
   pub enum LooprError {
       #[error("Loop not found: {0}")]
       LoopNotFound(String),
       #[error("Invalid state: {0}")]
       InvalidState(String),
       #[error("Validation failed: {0}")]
       ValidationFailed(String),
       #[error("Storage error: {0}")]
       Storage(String),
       #[error("LLM error: {0}")]
       Llm(String),
       #[error("Tool error: {0}")]
       Tool(String),
       #[error("IO error: {0}")]
       Io(#[from] std::io::Error),
       #[error("JSON error: {0}")]
       Json(#[from] serde_json::Error),
   }
   pub type Result<T> = std::result::Result<T, LooprError>;
   ```

3. **src/id.rs** - ID generation utilities
   - `generate_loop_id()` → "1738300800123-a1b2"
   - `generate_child_id(parent, index)` → "001-002"
   - `generate_signal_id()` → "sig-..."
   - `generate_job_id(loop_id, iteration)` → "job-..."
   - `now_ms()` → current timestamp in milliseconds

4. **src/main.rs** - Stub that prints version
5. **src/lib.rs** - Exports error and id modules

**Validation:**
- `cargo build` produces binary
- `cargo test` passes (ID format tests)
- `./target/debug/loopr` prints version

---

## Phase 2: Domain Types

**Goal:** Define all core domain types: Loop, LoopType, LoopStatus, SignalRecord, ToolJobRecord, EventRecord.

**Docs:**
- [loop.md](loop.md) - Loop struct specification
- [domain-types.md](domain-types.md) - All type definitions

**Files to create:**
```
src/domain/mod.rs
src/domain/loop_record.rs
src/domain/signal.rs
src/domain/tool_job.rs
src/domain/event.rs
```

**Deliverables:**

1. **Loop struct** with all fields from [loop.md](loop.md):
   - Identity: id, loop_type, parent_id
   - Artifacts: input_artifact, output_artifacts
   - Config: prompt_path, validation_command, max_iterations
   - Workspace: worktree
   - State: iteration, status, progress, context
   - Timestamps: created_at, updated_at

2. **LoopType enum:** Plan, Spec, Phase, Code

3. **LoopStatus enum:** Pending, Running, Paused, Rebasing, Complete, Failed, Invalidated
   - `is_terminal()` method
   - `is_resumable()` method

4. **SignalRecord** with SignalType enum (Stop, Pause, Resume, Rebase, Error, Info, Invalidate)

5. **ToolJobRecord** with ToolJobStatus enum (Pending, Running, Success, Failed, Timeout, Cancelled)

6. **EventRecord** with common event type constants

7. **Loop constructors:**
   - `Loop::new_plan(task)`
   - `Loop::new_spec(parent, index)`
   - `Loop::new_phase(parent, index, name, total)`
   - `Loop::new_code(parent)`

**Validation:**
- All types serialize/deserialize to JSON
- Constructor tests verify field initialization
- Status helper methods work correctly

---

## Phase 3: Storage Layer

**Goal:** Implement JSONL-based persistence with in-memory caching and query support.

**Docs:**
- [persistence.md](persistence.md) - TaskStore design

**Dependencies:**
```bash
cargo add tempfile --dev
```

**Files to create:**
```
src/storage/mod.rs
src/storage/traits.rs
src/storage/jsonl.rs
src/storage/loops.rs
```

**Deliverables:**

1. **Storage trait:**
   ```rust
   pub trait Storage: Send + Sync {
       fn create<T: Serialize + DeserializeOwned>(&self, collection: &str, record: &T) -> Result<()>;
       fn get<T: DeserializeOwned>(&self, collection: &str, id: &str) -> Result<Option<T>>;
       fn update<T: Serialize + DeserializeOwned>(&self, collection: &str, id: &str, record: &T) -> Result<()>;
       fn delete(&self, collection: &str, id: &str) -> Result<()>;
       fn query<T: DeserializeOwned>(&self, collection: &str, filters: &[Filter]) -> Result<Vec<T>>;
       fn list<T: DeserializeOwned>(&self, collection: &str) -> Result<Vec<T>>;
   }
   ```

2. **Filter struct** with Eq, Ne, Contains operations

3. **JsonlStorage implementation:**
   - File per collection: `{collection}.jsonl`
   - In-memory cache with RwLock
   - Load on first access, save on write
   - Filter matching logic

4. **LoopStore helper:**
   - `find_by_status(status)`
   - `find_by_parent(parent_id)`
   - `find_pending()`
   - `find_running()`

**Validation:**
- CRUD operations work correctly
- Query filtering returns expected results
- Data persists across JsonlStorage instances
- Concurrent access is safe (RwLock)

---

## Phase 4: LLM Client

**Goal:** Implement Anthropic API client with streaming support and tool call parsing.

**Docs:**
- [llm-client.md](llm-client.md) - Client specification

**Dependencies:**
```bash
cargo add reqwest --features json,rustls-tls
cargo add async-trait
cargo add futures
cargo add tokio-stream
```

**Files to create:**
```
src/llm/mod.rs
src/llm/types.rs
src/llm/client.rs
src/llm/anthropic.rs
src/llm/streaming.rs
src/llm/tool_parser.rs
```

**Deliverables:**

1. **Message types:**
   - `Role` enum (User, Assistant)
   - `Message` struct with `user()` and `assistant()` constructors
   - `ToolDefinition`, `ToolCall`, `ToolResult` structs
   - `CompletionRequest` and `CompletionResponse`
   - `StopReason` enum (EndTurn, ToolUse, MaxTokens, StopSequence)
   - `Usage` struct (input_tokens, output_tokens)

2. **LlmClient trait:**
   ```rust
   #[async_trait]
   pub trait LlmClient: Send + Sync {
       async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse>;
       async fn continue_with_tool_results(&self, request: CompletionRequest, results: Vec<ToolResult>) -> Result<CompletionResponse>;
   }
   ```

3. **AnthropicClient:**
   - Constructor with API key and model selection
   - Request building (system, messages, tools)
   - Response parsing (text blocks, tool_use blocks)
   - Error handling for API failures

4. **Streaming support:**
   - `StreamEvent` enum (TextDelta, ToolUseStart, ToolInputDelta, Done, Error)
   - `StreamHandle` with receiver channel
   - `complete_stream()` method

5. **Tool parser:**
   - Parse tool calls from content blocks
   - Validate input against schema (required fields)

6. **Mock client for testing**

**Validation:**
- Request building produces correct JSON structure
- Response parsing handles text and tool_use blocks
- Mock client enables unit testing
- Tool parser extracts calls correctly

---

## Phase 5: Tool System

**Goal:** Define tool types, load catalog from TOML, implement router trait.

**Docs:**
- [tools.md](tools.md) - Tool definitions
- [tool-catalog.md](tool-catalog.md) - Catalog format
- [runners.md](runners.md) - Runner lanes

**Dependencies:**
```bash
cargo add toml
```

**Files to create:**
```
src/tools/mod.rs
src/tools/definition.rs
src/tools/catalog.rs
src/tools/router.rs
catalog.toml
```

**Deliverables:**

1. **ToolLane enum:** NoNet, Net, Heavy

2. **Tool struct:**
   - name, description, input_schema
   - lane, timeout_ms, requires_worktree
   - `to_llm_definition()` converter

3. **ToolCatalog:**
   - Load from TOML file
   - `get(name)`, `list()`, `get_lane(name)`
   - TOML → JSON schema conversion

4. **catalog.toml** with basic tools:
   ```toml
   [[tool]]
   name = "read_file"
   description = "Read file contents"
   lane = "no-net"

   [[tool]]
   name = "write_file"
   description = "Write file contents"
   lane = "no-net"

   [[tool]]
   name = "bash"
   description = "Execute bash command"
   lane = "no-net"
   timeout_ms = 60000
   ```

5. **ToolRouter trait:**
   ```rust
   #[async_trait]
   pub trait ToolRouter: Send + Sync {
       async fn execute(&self, call: ToolCall, worktree: &Path) -> Result<ToolResult>;
       fn available_tools(&self) -> Vec<String>;
   }
   ```

6. **LocalToolRouter** - Simple in-process router for development

**Validation:**
- Catalog loads from TOML correctly
- Tool definitions convert to LLM format
- Router executes tools and returns results
- Lane assignment works

---

## Phase 6: Prompt System

**Goal:** Load prompt templates from files and render with context variables.

**Docs:**
- [loop.md](loop.md) - Prompt building section

**Dependencies:**
```bash
cargo add handlebars
```

**Files to create:**
```
src/prompt/mod.rs
src/prompt/loader.rs
src/prompt/render.rs
prompts/plan.md
prompts/spec.md
prompts/phase.md
prompts/code.md
```

**Deliverables:**

1. **PromptLoader:**
   - Load templates from directory
   - Cache loaded templates
   - `load(name)`, `get(name)`, `exists(name)`

2. **PromptRenderer:**
   - Handlebars-based rendering
   - `render(template, context)`
   - `render_with_progress(template, context, progress)` - appends feedback section

3. **Prompt templates** (basic versions):
   - `prompts/plan.md` - Plan loop system prompt
   - `prompts/spec.md` - Spec loop system prompt
   - `prompts/phase.md` - Phase loop system prompt
   - `prompts/code.md` - Code loop system prompt

Each template should have:
- Role description
- Task placeholder: `{{task}}`
- Context placeholders as needed
- Output format instructions

**Validation:**
- Templates load from files
- Variables render correctly
- Missing variables don't crash (empty string)
- Progress appends properly

---

## Phase 7: Validation System

**Goal:** Implement validators for loop outputs - format checking and command execution.

**Docs:**
- [loop-validation.md](loop-validation.md) - Validation per loop type
- [loop.md](loop.md) - Validation section

**Files to create:**
```
src/validation/mod.rs
src/validation/traits.rs
src/validation/format.rs
src/validation/command.rs
src/validation/composite.rs
```

**Deliverables:**

1. **ValidationResult struct:**
   ```rust
   pub struct ValidationResult {
       pub passed: bool,
       pub output: String,
       pub errors: Vec<String>,
   }
   ```

2. **Validator trait:**
   ```rust
   #[async_trait]
   pub trait Validator: Send + Sync {
       async fn validate(&self, artifact: &Path, worktree: &Path) -> Result<ValidationResult>;
   }
   ```

3. **FormatValidator:**
   - Check required markdown sections exist
   - Configurable per loop type:
     - Plan: ## Overview, ## Phases, ## Success Criteria, ## Specs to Create
     - Spec: ## Parent Plan, ## Overview, ## Phases
     - Phase: ## Task, ## Specific Work, ## Success Criteria

4. **CommandValidator:**
   - Execute shell command in worktree
   - Capture stdout/stderr
   - Return pass/fail based on exit code
   - Used for `cargo test`, `otto ci`, etc.

5. **CompositeValidator:**
   - Chain multiple validators
   - All must pass for overall pass
   - Collect all errors

**Validation:**
- Format validator detects missing sections
- Command validator runs and captures output
- Composite chains validators correctly

---

## Phase 8: Single Loop Execution

**Goal:** Implement `Loop::run()` - the core Ralph Wiggum iteration pattern.

**Docs:**
- [loop.md](loop.md) - The essential document, especially "The Iteration Model" section
- [execution-model.md](execution-model.md) - Loop execution flow

**Files to modify:**
```
src/domain/loop_record.rs   # Add run() method
src/runner/mod.rs           # Re-export LoopOutcome only
```

> **NOTE:** Per domain-types.md, `Loop` is self-contained with its own `run()` method.
> There is no separate `LoopRunner` struct - that was deemed unnecessary indirection.

**Deliverables:**

1. **`Loop::run()` method implementing Ralph Wiggum pattern:**
   ```rust
   impl Loop {
       pub async fn run<L, T, V>(
           &mut self,
           llm: Arc<L>,
           tool_router: Arc<T>,
           validator: Arc<V>,
       ) -> Result<LoopOutcome>
       where
           L: LlmClient,
           T: ToolRouter,
           V: Validator,
       {
           while self.iteration < self.max_iterations {
               // 1. Build prompt with accumulated feedback (FRESH CONTEXT)
               let prompt = self.build_system_prompt(&prompt_renderer)?;

               // 2. Call LLM - NEW messages array each time
               let response = llm.complete(CompletionRequest {
                   system: prompt,
                   messages: vec![Message::user(&self.context["task"])],
                   tools: self.get_tools_for_loop_type(&*tool_router),
                   ..Default::default()
               }).await?;

               // 3. Execute tool calls
               for call in response.tool_calls {
                   tool_router.execute(call, &self.worktree).await?;
               }

               // 4. Validate output
               let result = validator.validate(&artifact_path, &self.worktree).await?;

               if result.passed {
                   self.status = LoopStatus::Complete;
                   return Ok(LoopOutcome::Complete);
               }

               // 5. Accumulate feedback for next iteration
               self.progress.push_str(&format!(
                   "\n---\nIteration {} failed:\n{}\n",
                   self.iteration + 1,
                   result.output
               ));
               self.iteration += 1;
           }

           self.status = LoopStatus::Failed;
           Ok(LoopOutcome::Failed("Max iterations".into()))
       }
   }
   ```

2. **LoopOutcome enum:** Complete, Failed(String), Invalidated

3. **Private helper methods on Loop:**
   - `build_system_prompt()` - render template with context and progress
   - `get_tools_for_loop_type()` - get appropriate tools based on loop type
   - `get_artifact_path()` - get path to output artifact for validation

**Validation:**
- Loop iterates until validation passes
- Progress accumulates across iterations
- Fresh context each iteration (no message history)
- Max iterations triggers failure

---

## Phase 9: Artifact Parsing

**Goal:** Parse plan.md and spec.md to extract child loop definitions.

**Docs:**
- [loop.md](loop.md) - Artifact Parsing section with SpecDescriptor and PhaseDescriptor
- [artifact-tools.md](artifact-tools.md) - Structured output

**Files to create:**
```
src/artifact/mod.rs
src/artifact/parser.rs
src/artifact/plan.rs
src/artifact/spec.rs
```

**Deliverables:**

1. **SpecDescriptor** (extracted from plan.md):
   ```rust
   pub struct SpecDescriptor {
       pub name: String,
       pub description: String,
       pub index: u32,
   }
   ```

2. **PhaseDescriptor** (extracted from spec.md):
   ```rust
   pub struct PhaseDescriptor {
       pub number: u32,
       pub name: String,
       pub description: String,
       pub files: Vec<String>,
   }
   ```

3. **parse_plan_specs(path):**
   - Find "## Specs to Create" section
   - Parse list items: `- spec-<name>: <description>`
   - Return `Vec<SpecDescriptor>`

4. **parse_spec_phases(path):**
   - Find "## Phases" section
   - Parse numbered phases: `1. **Phase Name**`
   - Extract description and files
   - Return `Vec<PhaseDescriptor>`

5. **Section extraction helper:**
   - Extract content between `## Heading` and next `## `

**Validation:**
- Plan parsing extracts specs correctly
- Spec parsing extracts phases correctly
- Missing sections return errors
- Empty sections return empty lists

---

## Phase 10: Worktree Management

**Goal:** Create, manage, and cleanup git worktrees for loop isolation.

**Docs:**
- [execution-model.md](execution-model.md) - Worktree Lifecycle section
- [worktree-coordination.md](worktree-coordination.md) - Rebase protocol

**Files to create:**
```
src/worktree/mod.rs
src/worktree/manager.rs
```

**Deliverables:**

1. **WorktreeManager struct:**
   - `base_path` - where worktrees are created
   - `repo_root` - main repository

2. **Core operations:**
   ```rust
   impl WorktreeManager {
       /// Create worktree with new branch from main
       pub async fn create(&self, loop_id: &str) -> Result<PathBuf>;

       /// Remove worktree and optionally delete branch
       pub async fn cleanup(&self, loop_id: &str, preserve_branch: bool) -> Result<()>;

       /// Check if worktree exists
       pub fn exists(&self, loop_id: &str) -> bool;

       /// Get worktree path for loop
       pub fn path(&self, loop_id: &str) -> PathBuf;

       /// List all worktrees
       pub async fn list(&self) -> Result<Vec<String>>;

       /// Check if worktree has uncommitted changes
       pub async fn is_clean(&self, loop_id: &str) -> Result<bool>;

       /// Auto-commit any changes
       pub async fn auto_commit(&self, loop_id: &str, message: &str) -> Result<()>;
   }
   ```

3. **Git command execution:**
   - `git worktree add <path> -b <branch> main`
   - `git worktree remove <path> --force`
   - `git branch -D <branch>`
   - `git status --porcelain`
   - `git add -A && git commit -m "..."`

**Validation:**
- Worktree creates with correct branch
- Cleanup removes worktree and branch
- is_clean() detects uncommitted changes
- auto_commit() stages and commits

---

## Phase 11: Loop Coordination

**Goal:** Implement signal-based coordination between loops (stop, pause, invalidate).

**Docs:**
- [loop-coordination.md](loop-coordination.md) - Signal-based coordination
- [loop-architecture.md](loop-architecture.md) - Invalidation section

**Files to create:**
```
src/coordination/mod.rs
src/coordination/signals.rs
src/coordination/invalidate.rs
```

**Deliverables:**

1. **SignalManager:**
   ```rust
   impl SignalManager {
       /// Write a signal to storage
       pub fn send(&self, signal: SignalRecord) -> Result<()>;

       /// Check for signals targeting a loop
       pub fn check(&self, loop_id: &str) -> Result<Option<SignalRecord>>;

       /// Check for signals matching a selector (e.g., "descendants:001")
       pub fn check_selector(&self, selector: &str) -> Result<Vec<SignalRecord>>;

       /// Acknowledge a signal
       pub fn acknowledge(&self, signal_id: &str) -> Result<()>;

       /// Get unacknowledged signals
       pub fn pending(&self) -> Result<Vec<SignalRecord>>;
   }
   ```

2. **Invalidation cascade:**
   ```rust
   impl SignalManager {
       /// Invalidate all descendants of a loop
       pub async fn invalidate_descendants(&self, parent_id: &str) -> Result<u32>;
   }
   ```
   - Find all loops with parent_id chain leading to parent
   - Send Stop signal to each
   - Return count of invalidated loops

3. **Signal checking in loop execution:**
   - Check for signals at iteration boundary
   - Handle Stop → mark Invalidated, exit
   - Handle Pause → wait for Resume

**Validation:**
- Signals persist to storage
- Check finds signals for target loop
- Acknowledge marks signal as handled
- Invalidation cascades to all descendants

---

## Phase 12: Loop Manager

**Goal:** LoopManager orchestrates loop lifecycle - creation, execution, child spawning.

**Docs:**
- [loop-architecture.md](loop-architecture.md) - LoopManager section
- [execution-model.md](execution-model.md) - Loop Execution Flow

**Files to create:**
```
src/manager/mod.rs
src/manager/loop_manager.rs
src/manager/spawner.rs
```

**Deliverables:**

1. **LoopManager struct:**
   - Owns: Storage, LlmClient, ToolRouter, WorktreeManager, SignalManager
   - Tracks: running loops (HashMap<String, JoinHandle>)

2. **Core methods:**
   ```rust
   impl LoopManager {
       /// Create and persist a new loop
       pub async fn create_loop(&self, loop_type: LoopType, task: &str) -> Result<Loop>;

       /// Start executing a loop (spawns tokio task)
       pub async fn start_loop(&self, loop_id: &str) -> Result<()>;

       /// Stop a running loop
       pub async fn stop_loop(&self, loop_id: &str) -> Result<()>;

       /// Pause a running loop
       pub async fn pause_loop(&self, loop_id: &str) -> Result<()>;

       /// Resume a paused loop
       pub async fn resume_loop(&self, loop_id: &str) -> Result<()>;

       /// Handle loop completion - spawn children if needed
       pub async fn on_loop_complete(&self, loop_id: &str) -> Result<()>;
   }
   ```

3. **Child spawning logic:**
   - PlanLoop complete → wait for user approval → spawn SpecLoops
   - SpecLoop complete → parse phases → spawn PhaseLoops
   - PhaseLoop complete → spawn CodeLoop
   - CodeLoop complete → check merge ready

4. **Loop execution wrapper:**
   - Create worktree
   - Call `loop_instance.run(llm, tools, validator)`
   - Handle outcome (complete/failed/invalidated)
   - Cleanup worktree
   - Persist final state

**Validation:**
- Loops create and persist correctly
- Start spawns tokio task
- Stop/pause signals are sent
- Children spawn from completed parents

---

## Phase 13: Daemon Core

**Goal:** Daemon process with scheduler, tick loop, and crash recovery.

**Docs:**
- [process-model.md](process-model.md) - Daemon lifecycle
- [scheduler.md](scheduler.md) - Priority model
- [execution-model.md](execution-model.md) - Crash Recovery section

**Files to create:**
```
src/daemon/mod.rs
src/daemon/scheduler.rs
src/daemon/tick.rs
src/daemon/recovery.rs
```

**Deliverables:**

1. **Scheduler:**
   ```rust
   impl Scheduler {
       /// Select loops to run given available slots
       pub fn select(&self, pending: Vec<Loop>, slots: usize) -> Vec<Loop>;
   }
   ```
   - Priority: Code > Phase > Spec > Plan (depth-first)
   - Respect max_concurrent_loops limit
   - Consider loop dependencies

2. **Daemon tick loop:**
   ```rust
   impl Daemon {
       pub async fn run(&mut self) -> Result<()> {
           loop {
               // 1. Check for IPC messages
               self.process_ipc().await?;

               // 2. Reap completed loops
               self.reap_completed().await?;

               // 3. Find pending loops
               let pending = self.storage.find_pending()?;

               // 4. Schedule and start loops
               let slots = self.config.max_concurrent - self.running.len();
               let to_start = self.scheduler.select(pending, slots);
               for loop_record in to_start {
                   self.manager.start_loop(&loop_record.id).await?;
               }

               // 5. Sleep before next tick
               tokio::time::sleep(self.config.tick_interval).await;
           }
       }
   }
   ```

3. **Crash recovery:**
   ```rust
   impl Daemon {
       pub async fn recover(&mut self) -> Result<()> {
           let interrupted = self.storage.find_running()?;
           for loop_record in interrupted {
               if self.worktree_manager.exists(&loop_record.id) {
                   // Auto-commit and mark pending for resume
                   self.worktree_manager.auto_commit(&loop_record.id, "WIP: recovery").await?;
                   self.storage.update_status(&loop_record.id, LoopStatus::Pending)?;
               } else {
                   // Worktree lost, mark failed
                   self.storage.update_status(&loop_record.id, LoopStatus::Failed)?;
               }
           }
       }
   }
   ```

4. **Configuration:**
   - max_concurrent_loops
   - tick_interval
   - disk_quota_min_gb

**Validation:**
- Scheduler respects concurrency limits
- Tick loop processes pending loops
- Recovery handles interrupted loops
- Clean shutdown stops running loops

---

## Phase 14: IPC Layer

**Goal:** Unix socket server for TUI-daemon communication.

**Docs:**
- [ipc-protocol.md](ipc-protocol.md) - Message schemas
- [process-model.md](process-model.md) - TUI/Daemon relationship

**Dependencies:**
```bash
cargo add tokio-util --features codec
```

**Files to create:**
```
src/ipc/mod.rs
src/ipc/messages.rs
src/ipc/server.rs
src/ipc/client.rs
src/ipc/codec.rs
```

**Deliverables:**

1. **Message types (requests):**
   - `CreatePlan { task: String }`
   - `ApprovePlan { id: String }`
   - `RejectPlan { id: String, reason: Option<String> }`
   - `IteratePlan { id: String, feedback: String }`
   - `PauseLoop { id: String }`
   - `ResumeLoop { id: String }`
   - `StopLoop { id: String }`
   - `ListLoops`
   - `GetLoop { id: String }`
   - `Subscribe`

2. **Message types (events):**
   - `LoopCreated { loop: Loop }`
   - `LoopUpdated { loop: Loop }`
   - `IterationComplete { loop_id, iteration, passed }`
   - `PlanAwaitingApproval { id, content, specs }`
   - `Error { message: String }`

3. **IpcServer:**
   - Listen on Unix socket
   - Accept multiple clients
   - Route requests to LoopManager
   - Broadcast events to subscribers

4. **IpcClient:**
   - Connect to daemon socket
   - Send requests, receive responses
   - Subscribe to event stream

5. **Codec:**
   - Length-prefixed JSON framing
   - Newline-delimited alternative

**Validation:**
- Server accepts connections
- Requests route to correct handlers
- Events broadcast to all subscribers
- Client can send/receive messages

---

## Phase 15: TUI Client

**Goal:** Terminal UI with chat view, loops view, and plan approval.

**Docs:**
- [tui.md](tui.md) - UI specification
- [loop-architecture.md](loop-architecture.md) - Plan approval protocol

**Dependencies:**
```bash
cargo add ratatui
cargo add crossterm
```

**Files to create:**
```
src/tui/mod.rs
src/tui/app.rs
src/tui/views/mod.rs
src/tui/views/chat.rs
src/tui/views/loops.rs
src/tui/views/approval.rs
src/tui/input.rs
```

**Deliverables:**

1. **App struct:**
   - IpcClient connection
   - Current view (Chat, Loops, Approval)
   - Event loop with crossterm

2. **Chat view:**
   - Input line for user messages
   - Message history display
   - `/plan <task>` command to create plan

3. **Loops view:**
   - Tree display of loop hierarchy
   - Status indicators (pending, running, complete, failed)
   - Iteration count
   - Select loop for details

4. **Approval view:**
   - Display plan.md content
   - List specs to be created
   - [A]pprove / [R]eject / [I]terate buttons
   - Feedback input for iterate

5. **Keyboard navigation:**
   - Tab: switch views
   - Arrow keys: navigate
   - Enter: select/confirm
   - Esc: back/cancel
   - q: quit

6. **Event handling:**
   - Receive daemon events
   - Update UI state
   - Show notifications

**Validation:**
- App launches and renders
- Views switch correctly
- Commands send to daemon
- Events update UI

---

## Phase 16: CLI & Integration

**Goal:** CLI entry point with subcommands and end-to-end integration test.

**Docs:**
- [README.md](README.md) - Usage section

**Dependencies:**
```bash
cargo add clap --features derive
```

**Files to create/modify:**
```
src/main.rs
src/cli/mod.rs
src/cli/commands.rs
tests/integration/mod.rs
tests/integration/single_loop.rs
```

**Deliverables:**

1. **CLI structure:**
   ```
   loopr                    # Launch TUI (default)
   loopr daemon start       # Start daemon in background
   loopr daemon stop        # Stop daemon
   loopr daemon status      # Check daemon status
   loopr plan <task>        # Create plan (daemon must be running)
   loopr list               # List all loops
   loopr status <id>        # Get loop status
   loopr --version          # Print version
   loopr --help             # Print help
   ```

2. **Daemon management:**
   - Start: fork/daemonize, write PID file
   - Stop: send signal, wait for exit
   - Status: check PID file and process

3. **Integration test:**
   - Start daemon programmatically
   - Create a simple plan
   - Wait for completion (mock LLM)
   - Verify artifacts produced
   - Stop daemon

4. **End-to-end test (optional, requires API key):**
   - Skip if ANTHROPIC_API_KEY not set
   - Create real plan with real LLM
   - Verify iteration pattern works

**Validation:**
- `loopr --help` shows all commands
- `loopr daemon start` launches daemon
- `loopr daemon status` reports correctly
- Integration test passes with mock LLM
- Binary builds: `cargo build --release`

---

## Phase Completion Checklist

For each phase:

1. [ ] Read linked docs thoroughly
2. [ ] Create all specified files
3. [ ] Implement all deliverables
4. [ ] Write tests for new functionality
5. [ ] `cargo check` passes
6. [ ] `cargo clippy` passes (no warnings)
7. [ ] `cargo fmt --check` passes
8. [ ] `cargo test` passes
9. [ ] Commit with proper message format

**Commit message format:**
```
feat(scope): description

Phase N: <phase name>
- bullet points of what was done

Refs: docs/<relevant-doc>.md
```

---

## Quick Reference

| Phase | Key Files | Primary Docs |
|-------|-----------|--------------|
| 1 | error.rs, id.rs | domain-types.md |
| 2 | domain/*.rs | loop.md, domain-types.md |
| 3 | storage/*.rs | persistence.md |
| 4 | llm/*.rs | llm-client.md |
| 5 | tools/*.rs | tools.md, tool-catalog.md |
| 6 | prompt/*.rs | loop.md |
| 7 | validation/*.rs | loop-validation.md |
| 8 | runner/*.rs | loop.md, execution-model.md |
| 9 | artifact/*.rs | loop.md, artifact-tools.md |
| 10 | worktree/*.rs | execution-model.md |
| 11 | coordination/*.rs | loop-coordination.md |
| 12 | manager/*.rs | loop-architecture.md |
| 13 | daemon/*.rs | process-model.md, scheduler.md |
| 14 | ipc/*.rs | ipc-protocol.md |
| 15 | tui/*.rs | tui.md |
| 16 | cli/*.rs, main.rs | README.md |

---

## References

All documentation in `docs/`:

- **[loop.md](loop.md)** - Core Loop specification (essential)
- **[domain-types.md](domain-types.md)** - Type definitions
- **[persistence.md](persistence.md)** - Storage design
- **[llm-client.md](llm-client.md)** - Anthropic client
- **[tools.md](tools.md)** - Tool system
- **[tool-catalog.md](tool-catalog.md)** - Catalog format
- **[loop-validation.md](loop-validation.md)** - Validators
- **[execution-model.md](execution-model.md)** - Worktrees, recovery
- **[loop-architecture.md](loop-architecture.md)** - Hierarchy, spawning
- **[loop-coordination.md](loop-coordination.md)** - Signals
- **[worktree-coordination.md](worktree-coordination.md)** - Rebase protocol
- **[process-model.md](process-model.md)** - TUI/Daemon/Runner
- **[scheduler.md](scheduler.md)** - Priority model
- **[ipc-protocol.md](ipc-protocol.md)** - Message schemas
- **[tui.md](tui.md)** - User interface
- **[artifact-tools.md](artifact-tools.md)** - Structured output
- **[observability.md](observability.md)** - Events, logging
