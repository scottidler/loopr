# Observability

**Author:** Scott A. Idler
**Date:** 2026-01-31
**Status:** Implementation Spec

---

## Summary

Loopr uses a multi-layer event system for observability:

1. **DaemonEvent** - Real-time events pushed to TUIs via IPC
2. **EventRecord** - Persistent event log stored in TaskStore
3. **Internal Events** - In-memory broadcast for component coordination

This document consolidates all event types and their usage.

---

## Event Layers

```
┌─────────────────────────────────────────────────────────────────┐
│                        TUI                                       │
│  Receives DaemonEvent via IPC socket                            │
└─────────────────────────────────────────────────────────────────┘
                              ↑
                    IPC (Unix socket)
                              │
┌─────────────────────────────────────────────────────────────────┐
│                       Daemon                                     │
│                                                                  │
│  ┌─────────────┐    ┌──────────────┐    ┌─────────────────────┐ │
│  │  EventBus   │───>│ DaemonEvent  │───>│   IPC to TUI        │ │
│  │ (broadcast) │    │   (push)     │    │                     │ │
│  └─────────────┘    └──────────────┘    └─────────────────────┘ │
│        │                                                         │
│        │            ┌──────────────┐    ┌─────────────────────┐ │
│        └───────────>│ EventRecord  │───>│ events.jsonl        │ │
│                     │ (persist)    │    │ (TaskStore)         │ │
│                     └──────────────┘    └─────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
```

---

## DaemonEvent (IPC Events)

Events pushed from daemon to connected TUIs in real-time. These are transient - not persisted.

### Schema

```rust
/// Daemon pushes events (no request ID)
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DaemonEvent {
    event: String,
    data: Value,
}
```

### Event Types

| Event | Data | Description |
|-------|------|-------------|
| `chat.chunk` | `{ text, done }` | Streaming LLM response chunk |
| `chat.tool_call` | `{ tool, input }` | Tool invocation started |
| `chat.tool_result` | `{ tool, output }` | Tool completed |
| `loop.created` | `{ loop: Loop }` | New loop created |
| `loop.updated` | `{ loop: Loop }` | Loop state changed |
| `loop.iteration` | `{ id, iteration, status }` | Iteration completed |
| `loop.artifact` | `{ id, path }` | Artifact produced |
| `loop.recovered` | `{ id, from_status, to_status }` | Loop recovered after restart |
| `plan.awaiting_approval` | `{ id, content, specs }` | Plan ready for user review |
| `plan.approved` | `{ id, specs_spawned }` | Plan was approved |
| `plan.rejected` | `{ id, reason }` | Plan was rejected |
| `metrics.update` | `{ ... }` | Metrics changed |
| `merge.started` | `{ loop_id, branch }` | Merge to main started |
| `merge.completed` | `{ loop_id, new_head }` | Merge completed |
| `merge.conflict` | `{ loop_id, files }` | Merge conflict detected |
| `error` | `{ message, code, context }` | System error |

### Usage

```rust
impl Daemon {
    fn notify_tuis(&self, event: DaemonEvent) {
        for conn in &self.tui_connections {
            let _ = conn.send_event(event.clone());
        }
    }

    async fn on_loop_updated(&self, record: &Loop) {
        // Persist to TaskStore
        self.store.update(record)?;

        // Notify connected TUIs
        self.notify_tuis(DaemonEvent {
            event: "loop.updated".to_string(),
            data: serde_json::to_value(record)?,
        });
    }
}
```

---

## EventRecord (Persistent Events)

General-purpose event log stored in TaskStore for debugging and replay.

### Schema

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    pub id: String,
    pub event_type: String,
    pub loop_id: Option<String>,
    pub payload: Value,
    pub created_at: i64,
}
```

### Storage

Location: `~/.loopr/<project>/store/events.jsonl`

Indexed fields: `event_type`, `loop_id`

### Event Types

| Event Type | Loop ID | Description |
|------------|---------|-------------|
| `loop.created` | Yes | Loop record created |
| `loop.started` | Yes | Loop began executing |
| `loop.status_change` | Yes | Status transition |
| `loop.iteration_started` | Yes | Iteration began |
| `loop.iteration_complete` | Yes | Iteration finished |
| `loop.complete` | Yes | Loop completed successfully |
| `loop.failed` | Yes | Loop exhausted iterations |
| `loop.invalidated` | Yes | Parent re-iterated |
| `tool.started` | Yes | Tool execution began |
| `tool.completed` | Yes | Tool execution finished |
| `validation.started` | Yes | Validation command began |
| `validation.completed` | Yes | Validation finished |
| `merge.started` | Yes | Merge to main began |
| `merge.completed` | Yes | Merge successful |
| `signal.sent` | Optional | Signal sent to loop(s) |
| `signal.acknowledged` | Optional | Signal received |
| `daemon.started` | No | Daemon process started |
| `daemon.shutdown` | No | Daemon shutting down |
| `runner.connected` | No | Runner connected to daemon |
| `runner.disconnected` | No | Runner disconnected |

### Usage

```rust
impl StateManager {
    async fn update_loop_status(&self, loop_id: &str, status: LoopStatus) {
        let mut record: Loop = self.store.get(loop_id)?.unwrap();
        let old_status = record.status;
        record.status = status;
        self.store.update(&record)?;

        // Log event
        self.store.create(&EventRecord {
            id: generate_event_id(),
            event_type: "loop.status_change".to_string(),
            loop_id: Some(loop_id.to_string()),
            payload: json!({
                "from": old_status,
                "to": status,
            }),
            created_at: now_ms(),
        })?;
    }
}
```

---

## Internal Event Bus

In-memory broadcast for component coordination within the daemon.

### Schema

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

    // Git operations
    MergeFailed { loop_id: String, reason: String },
    MergeConflict { loop_id: String, files: Vec<String> },
    BranchDiverged { loop_id: String, commits_behind: usize },
    LoopsPausedDivergence { count: usize },

    // Plan approval
    PlanAwaitingApproval { plan_id: String, content_summary: String },
    PlanApproved { plan_id: String },
    PlanRejected { plan_id: String, reason: Option<String> },
}
```

### EventBus Pattern

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

### Usage

Components subscribe to events they care about:

```rust
// In TUI event handler
let mut rx = event_bus.subscribe();
loop {
    match rx.recv().await {
        Ok(Event::LoopCompleted { execution_id, success, .. }) => {
            // Update UI
        }
        Ok(Event::TokenReceived { token, .. }) => {
            // Stream to chat view
        }
        _ => {}
    }
}
```

---

## Event Flow Examples

### Chat Message Flow

```
User types message
        │
        ▼
TUI ────────────────────────────────────────────────────────────> Daemon
        │   {"method":"chat.send","params":{"message":"..."}}
        │
        │<──────────────── DaemonEvent: chat.chunk ──────────────
        │<──────────────── DaemonEvent: chat.chunk ──────────────
        │<──────────────── DaemonEvent: chat.tool_call ──────────
        │<──────────────── DaemonEvent: chat.tool_result ────────
        │<──────────────── DaemonEvent: chat.chunk (done=true) ──
        ▼
```

### Loop Status Change Flow

```
Loop validation passes
        │
        ▼
LoopManager updates status
        │
        ├──> EventRecord created in events.jsonl
        │
        ├──> Internal Event::LoopCompleted broadcast
        │
        └──> DaemonEvent: loop.updated pushed to TUIs
```

### Plan Approval Flow

```
PlanLoop completes validation
        │
        ▼
LoopManager sets status = AwaitingApproval
        │
        ├──> EventRecord: loop.status_change
        │
        └──> DaemonEvent: plan.awaiting_approval
                    │
                    ▼
              TUI displays plan for review
                    │
        User approves ───────────────────────────────────> Daemon
                    │   {"method":"plan.approve"}
                    │
                    ▼
              LoopManager spawns SpecLoops
                    │
                    ├──> EventRecord: loop.created (per spec)
                    │
                    └──> DaemonEvent: plan.approved
```

---

## Querying Events

### By Loop ID

```rust
let events = store.query::<EventRecord>(&[
    Filter::eq("loop_id", loop_id),
])?;

// Returns all events for a specific loop, in chronological order
```

### By Event Type

```rust
let failures = store.query::<EventRecord>(&[
    Filter::eq("event_type", "loop.failed"),
])?;
```

### Time Range

```rust
let recent = store.query::<EventRecord>(&[
    Filter::gte("created_at", one_hour_ago_ms),
])?;
```

---

## Debugging with Events

### Replay a Loop's History

```bash
# Extract all events for a loop
jq 'select(.loop_id == "001-002-003")' ~/.loopr/myproject/store/events.jsonl
```

### Find All Failures

```bash
# Find all failed loops
jq 'select(.event_type == "loop.failed")' ~/.loopr/myproject/store/events.jsonl
```

### Track Iteration Progress

```bash
# Watch iterations for a loop
jq 'select(.loop_id == "001" and .event_type | startswith("loop.iteration"))' \
    ~/.loopr/myproject/store/events.jsonl
```

---

## Metrics

Metrics are derived from events and exposed via the `metrics.get` IPC method:

```rust
struct Metrics {
    // Loop stats
    loops_total: u64,
    loops_running: u64,
    loops_complete: u64,
    loops_failed: u64,

    // Iteration stats
    iterations_total: u64,
    iterations_per_loop_avg: f64,

    // LLM stats
    api_calls_total: u64,
    tokens_input_total: u64,
    tokens_output_total: u64,
    estimated_cost_usd: f64,

    // Tool stats
    tool_calls_total: u64,
    tool_calls_by_name: HashMap<String, u64>,
    tool_avg_duration_ms: HashMap<String, u64>,

    // Timing
    uptime_secs: u64,
    last_activity: i64,
}
```

---

## References

- [domain-types.md](domain-types.md) - EventRecord definition
- [ipc-protocol.md](ipc-protocol.md) - DaemonEvent protocol
- [implementation-patterns.md](implementation-patterns.md) - EventBus pattern
- [persistence.md](persistence.md) - TaskStore details
- [tui.md](tui.md) - TUI event handling
