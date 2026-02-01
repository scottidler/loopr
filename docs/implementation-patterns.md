# Implementation Patterns

**Author:** Scott A. Idler
**Date:** 2026-01-31
**Status:** Implementation Spec
**Source:** Extracted from taskdaemon/td/src/

---

## Summary

This document captures proven implementation patterns from the taskdaemon codebase that should be reused in Loopr v2. These patterns cover coordination, state management, tool execution, LLM integration, and more.

---

## 1. Actor-Based Coordination

### Coordinator Pattern

The Coordinator acts as a central message broker for inter-loop communication using Tokio async channels.

```rust
pub struct Coordinator {
    config: CoordinatorConfig,
    tx: mpsc::Sender<CoordRequest>,
    rx: mpsc::Receiver<CoordRequest>,
    event_store: Option<EventStore>,
}

pub struct CoordinatorHandle {
    tx: mpsc::Sender<CoordRequest>,
    exec_id: String,
}
```

**Key Features:**
- Channel-based message passing (`mpsc::channel`)
- Registration pattern: loops register to get a `CoordinatorHandle`
- Rate limiting per execution with sliding window
- Optional event persistence for crash recovery

### Rate Limiter Implementation

```rust
struct RateLimiter {
    counters: HashMap<String, VecDeque<Instant>>,
    limit: usize,
    window: Duration,
}

impl RateLimiter {
    fn check(&mut self, exec_id: &str) -> bool {
        let now = Instant::now();
        let times = self.counters.entry(exec_id.to_string()).or_default();

        // Prune old entries outside window
        while let Some(front) = times.front() {
            if now.duration_since(*front) > self.window {
                times.pop_front();
            } else {
                break;
            }
        }

        if times.len() < self.limit {
            times.push_back(now);
            true
        } else {
            false
        }
    }
}
```

---

## 2. State Management

### Actor-Based State Manager

```rust
pub struct StateManager {
    tx: mpsc::Sender<StateCommand>,
    event_tx: tokio::sync::broadcast::Sender<StateEvent>,
}

pub enum StateCommand {
    CreateLoop(Loop, oneshot::Sender<Result<()>>),
    UpdateLoop(String, LoopUpdate, oneshot::Sender<Result<()>>),
    GetLoop(String, oneshot::Sender<Result<Option<Loop>>>),
    ListLoops(LoopFilter, oneshot::Sender<Result<Vec<Loop>>>),
    // ... more commands
}
```

### State Events (Broadcast)

```rust
pub enum StateEvent {
    ExecutionCreated { id: String, loop_type: String },
    ExecutionUpdated { id: String },
    ExecutionPending { id: String },
    IterationLogCreated { execution_id: String, iteration: u32, exit_code: i32 },
    // Live streaming events
    TokenReceived { execution_id: String, token: String },
    ToolCallCompleted { execution_id: String, tool_name: String, success: bool },
}
```

### State Version Notification (External Polling)

```rust
fn state_notify_path() -> PathBuf {
    dirs::data_local_dir().unwrap().join("loopr/.state_version")
}

fn notify_state_change() {
    let path = state_notify_path();
    let version = read_state_version().wrapping_add(1);
    std::fs::write(&path, version.to_string()).ok();
}

pub fn read_state_version() -> u64 {
    let path = state_notify_path();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}
```

**Pattern:** External processes (TUI) can poll `.state_version` file to detect daemon changes without tight coupling.

---

## 3. Domain Types

> **Note:** TaskDaemon uses separate `Loop` (definition) and `LoopExecution` (runtime instance) types.
> **Loopr v2 simplifies this** to a single `Loop` struct with `LoopConfig` for behavior.
> See [domain-types.md](domain-types.md) for Loopr v2's unified approach.

### TaskDaemon Loop Record (Reference)

```rust
// TaskDaemon's definition type (what to do)
pub struct Loop {
    pub id: String,
    pub r#type: String,           // matches YAML loop type
    pub title: String,
    pub status: LoopStatus,       // Pending, Running, Ready, InProgress, Complete, Failed
    pub parent: Option<String>,   // parent Loop ID (for cascade)
    pub deps: Vec<String>,        // dependency IDs that must complete first
    pub file: Option<String>,     // path to markdown artifact
    pub phases: Vec<Phase>,       // unit-of-work phases
    pub priority: Priority,       // scheduler priority
    pub context: serde_json::Value, // type-specific template context
    pub created_at: i64,
    pub updated_at: i64,
}

pub struct Phase {
    pub name: String,
    pub description: String,
    pub status: PhaseStatus,  // Pending, Running, Complete, Failed
}
```

### TaskDaemon LoopExecution (Reference)

```rust
// TaskDaemon's execution type (how it's going)
pub struct LoopExecution {
    pub id: String,
    pub loop_type: String,
    pub title: Option<String>,
    pub parent: Option<String>,
    pub deps: Vec<String>,
    pub status: ExecutionStatus,  // Draft, Pending, Running, Paused, Rebasing, Blocked, Complete, Failed, Stopped
    pub worktree: Option<String>,
    pub iteration: u32,
    pub progress: String,       // accumulated progress from all iterations
    pub context: Value,         // template variables for prompts
    pub artifact_path: Option<String>,
    pub artifact_status: Option<String>, // "draft" | "complete" | "failed"
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_duration_ms: u64,
}
```

### Iteration Log (Audit Trail)

```rust
pub struct IterationLog {
    pub id: String,  // "{execution_id}-iter-{N}"
    pub execution_id: String,
    pub iteration: u32,
    pub validation_command: String,
    pub exit_code: i32,
    pub stdout: String,      // NO truncation for storage
    pub stderr: String,      // NO truncation for storage
    pub duration_ms: u64,
    pub files_changed: Vec<String>,
    pub llm_input_tokens: Option<u64>,
    pub llm_output_tokens: Option<u64>,
    pub tool_calls: Vec<ToolCallSummary>,
    pub created_at: i64,
    pub updated_at: i64,
}

pub struct ToolCallSummary {
    pub tool_name: String,
    pub arguments_summary: String,  // truncated to 200 chars for display
    pub result_summary: String,     // truncated to 200 chars for display
    pub is_error: bool,
}
```

### Status Transitions

```
Draft → Pending → Running → Complete/Failed/Stopped
                    ↓
                 Rebasing (when main branch updated)
                    ↓
                 Blocked (waiting for deps or rebase conflicts)
                    ↓
                 Paused (user-initiated, resumable)
```

---

## 4. TaskStore Record Trait

```rust
pub trait Record: Serialize + DeserializeOwned + Send + Sync {
    fn id(&self) -> &str;
    fn updated_at(&self) -> i64;
    fn collection_name() -> &'static str;
    fn indexed_fields(&self) -> HashMap<String, IndexValue>;
}

pub enum IndexValue {
    String(String),
    Int(i64),
    Bool(bool),
}
```

**TaskStore Features:**
- JSONL-based append-only log with indexing
- Auto-rebuild indexes on startup
- Git-friendly (merge-friendly format)

---

## 5. Tool System

### Tool Trait

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn input_schema(&self) -> Value;
    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult;
}

pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}
```

### Tool Profiles (Access Control)

```rust
pub enum ToolProfile {
    Full,        // read/write/bash (default for Code)
    ReadOnly,    // read-only, no write/edit/dangerous bash
}
```

### Tool Executor Registry

```rust
pub struct ToolExecutor {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolExecutor {
    pub fn with_profile(profile: ToolProfile) -> Self {
        let mut tools = HashMap::new();

        // Always available
        tools.insert("read", Box::new(ReadTool));
        tools.insert("glob", Box::new(GlobTool));
        tools.insert("grep", Box::new(GrepTool));
        tools.insert("list", Box::new(ListTool));
        tools.insert("tree", Box::new(TreeTool));

        if matches!(profile, ToolProfile::Full) {
            tools.insert("write", Box::new(WriteTool));
            tools.insert("edit", Box::new(EditTool));
            tools.insert("bash", Box::new(BashTool));
        }

        Self { tools }
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values()
            .map(|t| ToolDefinition {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
            })
            .collect()
    }

    pub async fn execute(&self, tool_call: &ToolCall, ctx: &ToolContext) -> ToolResult {
        match self.tools.get(&tool_call.name) {
            Some(tool) => tool.execute(tool_call.input.clone(), ctx).await,
            None => ToolResult {
                content: format!("Unknown tool: {}", tool_call.name),
                is_error: true,
            },
        }
    }
}
```

### Tool Context

```rust
pub struct ToolContext {
    pub worktree_path: PathBuf,
    pub coordinator_handle: Option<CoordinatorHandle>,
    pub explore_spawner: Option<ExploreSpawner>,
}
```

---

## 6. LLM Client

### Client Trait

```rust
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError>;
    async fn stream(&self, request: CompletionRequest, chunk_tx: mpsc::Sender<StreamChunk>)
        -> Result<CompletionResponse, LlmError>;
}
```

### Request/Response Types

```rust
pub struct CompletionRequest {
    pub system_prompt: String,      // Rendered from Handlebars template
    pub messages: Vec<Message>,     // Typically 1 user message
    pub tools: Vec<ToolDefinition>, // Available tools for this loop
    pub max_tokens: u32,
}

pub struct CompletionResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: StopReason,  // EndTurn, ToolUse, MaxTokens
    pub usage: TokenUsage,
}

pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}
```

### Streaming Chunks

```rust
pub enum StreamChunk {
    ContentStart,
    ContentDelta { delta: String },
    ToolUseStart { name: String, id: String },
    ToolUseDelta { input_delta: String },
    ToolUseEnd,
    Done { usage: TokenUsage, stop_reason: StopReason },
}
```

### Anthropic Client Retry Logic

```rust
const RETRYABLE_STATUS_CODES: &[u16] = &[408, 429, 500, 502, 503, 504, 529];
const MAX_RETRIES: u32 = 3;
const INITIAL_BACKOFF_MS: u64 = 1000;

async fn complete_with_retry(&self, request: &CompletionRequest) -> Result<CompletionResponse> {
    let mut attempt = 0;
    let mut backoff = Duration::from_millis(INITIAL_BACKOFF_MS);

    loop {
        match self.complete_once(request).await {
            Ok(response) => return Ok(response),
            Err(e) if attempt < MAX_RETRIES && e.is_retryable() => {
                attempt += 1;
                tokio::time::sleep(backoff).await;
                backoff *= 2;  // Exponential backoff
            }
            Err(e) => return Err(e),
        }
    }
}
```

---

## 7. Configuration System

**See [configuration-reference.md](configuration-reference.md) for the complete configuration reference.**

### Config Hierarchy

```rust
impl Config {
    pub fn load(config_path: Option<&PathBuf>) -> Result<Self> {
        // 1. Try explicit config_path
        // 2. Try .loopr.yml (local project)
        // 3. Try ~/.config/loopr/loopr.yml (user global)
        // 4. Use defaults
    }
}
```

### Config Structure

```rust
pub struct Config {
    pub log_level: Option<String>,
    pub llm: LlmConfig,
    pub concurrency: ConcurrencyConfig,
    pub validation: ValidationConfig,
    pub progress: ProgressConfig,
    pub git: GitConfig,
    pub storage: StorageConfig,
    pub loops: LoopsConfig,
    pub debug: DebugConfig,
}

pub struct ConcurrencyConfig {
    pub max_loops: u32,              // Default: 50
    pub max_api_calls: u32,          // Default: 10
    pub max_worktrees: u32,          // Default: 50
}

pub struct ValidationConfig {
    pub command: String,             // Default: "otto ci"
    pub iteration_timeout_ms: u64,   // Default: 300_000
    pub max_iterations: u32,         // Default: 100
}

pub struct StorageConfig {
    pub taskstore_dir: String,       // ~/.local/share/loopr
    pub jsonl_warn_mb: u32,          // Default: 100
    pub jsonl_error_mb: u32,         // Default: 500
}
```

### LLM Config

```yaml
llm:
  default: "anthropic/claude-sonnet"  # Provider/model format
  timeout-ms: 300000
  providers:
    anthropic:
      api-key-env: ANTHROPIC_API_KEY
      api-key-file: optional_path
      base-url: https://api.anthropic.com
      models:
        claude-sonnet-4-20250514:
          max-tokens: 8192
```

---

## 8. Loop Type System

### LoopType YAML Schema

```rust
pub struct LoopType {
    pub extends: Option<String>,              // inheritance
    pub parent: Option<String>,               // cascade parent type
    pub description: String,
    pub prompt_template: String,              // Handlebars
    pub validation_command: String,           // e.g., "otto ci"
    pub success_exit_code: i32,               // Default: 0
    pub max_iterations: u32,
    pub iteration_timeout_ms: u64,
    pub inputs: Vec<String>,                  // template variables
    pub outputs: Vec<String>,                 // artifact paths
    pub tools: Vec<String>,                   // tool names available
}
```

### Type Inheritance

```rust
impl LoopType {
    pub fn merge_parent(&mut self, parent: &LoopType) {
        // Child values override parent
        if self.prompt_template.is_empty() {
            self.prompt_template = parent.prompt_template.clone();
        }
        if self.validation_command.is_empty() {
            self.validation_command = parent.validation_command.clone();
        }

        // Vectors are merged (child adds to parent)
        let mut combined_tools = parent.tools.clone();
        combined_tools.extend(self.tools.clone());
        self.tools = combined_tools;
    }
}
```

### Loading Priority

1. `builtin` (embedded in binary)
2. `~/.config/loopr/loops/*.yml` (user global)
3. `.loopr/loops/*.yml` (project-specific)

Later definitions override earlier ones.

---

## 9. Scheduler

### Priority Queue Scheduler

```rust
pub struct Scheduler {
    config: SchedulerConfig,
    inner: Mutex<SchedulerInner>,
    notify: Notify,
}

struct SchedulerInner {
    queue: BinaryHeap<ScheduledRequest>,      // Priority ordered
    running: HashMap<String, ScheduledRequest>,
    request_times: VecDeque<Instant>,         // For rate limiting
    stats: SchedulerStats,
}

pub enum ScheduleResult {
    Running { exec_id: String },
    Queued { exec_id: String, position: usize },
    RateLimited { retry_after: Duration },
    Rejected { reason: String },
}
```

### Scheduling Logic

```rust
impl Scheduler {
    pub async fn schedule(&self, request: ScheduledRequest) -> ScheduleResult {
        let mut inner = self.inner.lock().await;

        // Check rate limit
        inner.prune_old_requests();
        if inner.request_times.len() >= self.config.max_requests_per_window {
            let oldest = inner.request_times.front().unwrap();
            let retry_after = self.config.window - oldest.elapsed();
            return ScheduleResult::RateLimited { retry_after };
        }

        // Check concurrency limit
        if inner.running.len() >= self.config.max_concurrent {
            let position = inner.queue.len() + 1;
            inner.queue.push(request);
            return ScheduleResult::Queued { exec_id: request.exec_id, position };
        }

        // Can run immediately
        inner.running.insert(request.exec_id.clone(), request.clone());
        inner.request_times.push_back(Instant::now());
        ScheduleResult::Running { exec_id: request.exec_id }
    }
}
```

---

## 10. Loop Engine

### Engine Structure

```rust
pub struct LoopEngine {
    pub exec_id: String,
    config: LoopConfig,
    llm: Arc<dyn LlmClient>,
    tool_executor: ToolExecutor,
    progress: Box<dyn ProgressStrategy>,
    worktree: PathBuf,
    iteration: u32,
    status: LoopStatus,
    coord_handle: Option<CoordinatorHandle>,
    scheduler: Option<Arc<Scheduler>>,
    execution_context: serde_json::Value,
    state: Option<StateManager>,
    tool_call_buffer: Vec<ToolCallSummary>,
    iteration_token_usage: TokenUsage,
    event_emitter: Option<EventEmitter>,
}
```

### Iteration Result

```rust
pub enum IterationResult {
    Complete { iterations: u32 },
    Continue { validation_output: String, exit_code: i32 },
    RateLimited { retry_after: Duration },
    Interrupted { reason: String },
    Error { message: String, recoverable: bool },
}
```

### Main Loop Pattern

```rust
impl LoopEngine {
    pub async fn run(&mut self) -> Result<LoopOutcome> {
        while self.iteration < self.config.max_iterations {
            // Check for stop signal
            if self.should_stop().await {
                return Ok(LoopOutcome::Stopped);
            }

            // Run single iteration
            match self.run_iteration().await? {
                IterationResult::Complete { iterations } => {
                    return Ok(LoopOutcome::Complete { iterations });
                }
                IterationResult::Continue { validation_output, exit_code } => {
                    self.progress.record(validation_output, exit_code);
                    self.iteration += 1;
                }
                IterationResult::RateLimited { retry_after } => {
                    tokio::time::sleep(retry_after).await;
                }
                IterationResult::Interrupted { reason } => {
                    return Ok(LoopOutcome::Interrupted { reason });
                }
                IterationResult::Error { message, recoverable } => {
                    if !recoverable {
                        return Err(eyre!(message));
                    }
                    self.iteration += 1;
                }
            }
        }

        Ok(LoopOutcome::MaxIterations { iterations: self.iteration })
    }
}
```

---

## 11. Crash Recovery

### Detection

```rust
pub async fn scan_for_recovery(state: &StateManager) -> RecoveryStats {
    let incomplete_loops = state.list_loops(LoopFilter {
        status: Some(vec![LoopStatus::InProgress]),
        ..Default::default()
    }).await?;

    let incomplete_execs = state.list_executions(ExecutionFilter {
        status: Some(vec![ExecutionStatus::Running, ExecutionStatus::Rebasing]),
        ..Default::default()
    }).await?;

    RecoveryStats {
        loops_to_recover: incomplete_loops.len(),
        executions_to_recover: incomplete_execs.len(),
    }
}
```

### Recovery Flow

1. On daemon startup, scan for incomplete loops/executions
2. Mark as interrupted (not failed - preserves resumability)
3. Offer user choice: resume, retry, or cancel
4. If resumed, restart from last checkpoint

---

## 12. Event System

### Event Taxonomy

```rust
pub enum Event {
    // Loop lifecycle
    LoopStarted { execution_id: String, loop_type: String, task_description: String },
    PhaseStarted { execution_id: String, phase_index: usize, phase_name: String, total_phases: usize },
    IterationStarted { execution_id: String, iteration: u32 },
    IterationCompleted { execution_id: String, iteration: u32, outcome: String },
    LoopCompleted { execution_id: String, success: bool, total_iterations: u32 },

    // LLM interactions
    PromptSent { execution_id: String, iteration: u32, prompt_summary: String, token_count: u64 },
    TokenReceived { execution_id: String, iteration: u32, token: String },
    ResponseCompleted { execution_id: String, iteration: u32, response_summary: String, input_tokens: u64, output_tokens: u64, has_tool_calls: bool },

    // Tool execution
    ToolCallStarted { execution_id: String, iteration: u32, tool_name: String, tool_args_summary: String },
    ToolCallCompleted { execution_id: String, iteration: u32, tool_name: String, success: bool, result_summary: String, duration_ms: u64 },

    // Validation
    ValidationStarted { execution_id: String, iteration: u32, command: String },
    ValidationOutput { execution_id: String, iteration: u32, line: String, is_stderr: bool },
    ValidationCompleted { execution_id: String, iteration: u32, exit_code: i32 },
}
```

### Event Bus Pattern

```rust
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    pub fn emit(&self, event: Event) {
        let _ = self.tx.send(event);  // Ignore if no receivers
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}
```

---

## 13. Cascade Handling (Loop Hierarchy)

### Parent-Child Relationships

```rust
pub struct CascadeHandler {
    state: Arc<StateManager>,
    type_loader: Arc<RwLock<LoopLoader>>,
}

impl CascadeHandler {
    pub async fn on_loop_ready(&self, record: &Loop, parent_exec_id: &str)
        -> Result<Vec<LoopExecution>> {
        // When a Loop becomes Ready, spawn child loops
        // Child types defined via `parent` field in loop type
    }

    pub async fn on_decomposition_complete(&self, parent_id: &str)
        -> Result<Vec<LoopExecution>> {
        // After decomposition completes, update parent to InProgress
        // Find ready child Loops and spawn executions
    }
}
```

### Cascade Flow

```
1. Plan created with status Pending
2. User approves plan → status Ready
3. CascadeHandler.on_loop_ready() detects child type "spec"
4. Creates LoopExecution for each spec with parent context
5. Each spec's phases spawn phase Loops
6. Each phase spawns code LoopExecutions
```

---

## Key Design Principles

1. **Actor Model**: Coordinator and StateManager are async actors communicating via channels
2. **Stateless LLM Client**: Fresh context per call (Ralph Wiggum pattern)
3. **Trait-based Extensibility**: Tools, LLM clients, progress strategies all pluggable
4. **JSONL Append-Only Persistence**: Immutable audit trail via taskstore
5. **Event-driven + Polling Hybrid**: Event-driven for responsiveness, polling fallback for reliability
6. **Early State Binding**: Execution context captured upfront, not mutated during run
7. **Graceful Degradation**: Crashes detected, incomplete work recovered
8. **Type-driven Configuration**: Loop types load from YAML with inheritance
9. **Multi-provider LLM Support**: Pluggable providers (Anthropic, etc.)
10. **Tool Profiles for Safety**: Full vs ReadOnly tool access based on loop type

---

## References

- [architecture.md](architecture.md) - System overview
- [loop-architecture.md](loop-architecture.md) - Loop hierarchy
- [tools.md](tools.md) - Tool system
- [llm-client.md](llm-client.md) - LLM integration
- [persistence.md](persistence.md) - TaskStore design
- [scheduler.md](scheduler.md) - Scheduling details
- Source: `taskdaemon/td/src/` (proven implementation)
