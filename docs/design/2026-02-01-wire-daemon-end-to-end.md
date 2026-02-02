# Design Document: Wire Daemon End-to-End

**Author:** Scott A. Idler
**Date:** 2026-02-01
**Status:** Ready for Implementation
**Review Passes Completed:** 5/5

## Summary

The Loopr codebase has all building blocks implemented in isolation, but they're not wired together. The daemon returns "Method not yet implemented" for all meaningful operations because `handle_request()` only handles `ping` and `status`. This design wires the existing components into a functional end-to-end system.

## Problem Statement

### Background

Loopr v2 was built iteratively across 96 iterations following `implementation-phases.md`. Each phase created working, tested components:
- LLM client with real Anthropic API calls
- Tool system with bash, read_file, write_file
- Storage layer with JSONL persistence
- Loop::run() implementing the Ralph Wiggum pattern
- LoopManager with full lifecycle methods
- IPC framework with server, client, messages
- TUI with views, tabs, input handling

However, these components were never integrated.

### Problem

1. **Daemon request handlers are stubs**: `handle_request()` in `src/daemon/mod.rs:235-270` only handles `ping` and `status`. All other methods return "Method 'X' not yet implemented".

2. **No event broadcasting**: `IpcServer::broadcast()` exists but is never called in production code. Zero events reach the TUI.

3. **CLI commands are stubs**: All `handle_*_command()` functions in `src/main.rs` print "not yet implemented".

4. **Components exist but aren't connected**: LoopManager has `start_loop()`, `pause_loop()`, etc. but daemon never calls them.

### Current State

**Daemon Request Handlers:**

| Method | Current Behavior | Should Do |
|--------|------------------|-----------|
| `ping` | Returns `{"pong": true}` | Working |
| `status` | Returns version info | Working |
| `loop.list` | Returns `{"loops": []}` (TODO) | Query LoopManager |
| `loop.get` | Returns `{"loop": null}` (TODO) | Query LoopManager |
| `loop.create_plan` | Returns hardcoded ID (TODO) | Create via LoopManager |
| `chat.send` | "not yet implemented" | Call LLM, return response |
| `loop.start` | "not yet implemented" | LoopManager.start_loop() |
| `loop.pause` | "not yet implemented" | LoopManager.pause_loop() |
| `loop.resume` | "not yet implemented" | LoopManager.resume_loop() |
| `loop.cancel` | "not yet implemented" | LoopManager.stop_loop() |
| `plan.approve` | "not yet implemented" | Parse plan, spawn specs |
| `plan.reject` | "not yet implemented" | Mark plan failed |
| All others | "not yet implemented" | Wire to implementations |

### Goals

- Wire all daemon request handlers to actual implementations
- Enable event broadcasting so TUI receives updates
- Implement chat functionality with LLM integration
- Wire CLI commands to daemon via IPC client
- Enable plan approval flow (user gate)

### Non-Goals

- Runner subprocesses (sandboxing) - defer to future work
- LLM streaming - nice-to-have, not critical path
- New features - only wire existing code

## Proposed Solution

### Overview

Refactor the `Daemon` struct to own the components it needs (LoopManager, LlmClient, ToolRouter, event channel), then wire `handle_request()` to call actual implementations instead of returning stubs.

### Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                              Daemon                                   │
│                                                                      │
│  ┌──────────────────────────────────────────────────────────────┐   │
│  │                     DaemonContext                             │   │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐   │   │
│  │  │LoopManager  │  │AnthropicCli │  │  LocalToolRouter    │   │   │
│  │  │             │  │             │  │                     │   │   │
│  │  │ create_loop │  │ complete()  │  │ execute()           │   │   │
│  │  │ start_loop  │  │             │  │                     │   │   │
│  │  │ pause_loop  │  │             │  │                     │   │   │
│  │  │ stop_loop   │  │             │  │                     │   │   │
│  │  └─────────────┘  └─────────────┘  └─────────────────────┘   │   │
│  │                                                               │   │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐   │   │
│  │  │ ChatSession │  │ event_tx    │  │    JsonlStorage     │   │   │
│  │  │             │  │ (broadcast) │  │                     │   │   │
│  │  │ messages[]  │  │             │  │ loops.jsonl         │   │   │
│  │  │             │  │             │  │ signals.jsonl       │   │   │
│  │  └─────────────┘  └─────────────┘  └─────────────────────┘   │   │
│  └──────────────────────────────────────────────────────────────┘   │
│                                │                                     │
│                                ▼                                     │
│  ┌──────────────────────────────────────────────────────────────┐   │
│  │                    handle_request()                           │   │
│  │                                                               │   │
│  │  match method {                                               │   │
│  │    "chat.send"    => handle_chat_send(ctx, params)           │   │
│  │    "loop.list"    => ctx.loop_manager.list_loops()           │   │
│  │    "loop.get"     => ctx.loop_manager.get_loop(id)           │   │
│  │    "loop.start"   => ctx.loop_manager.start_loop(id)         │   │
│  │    "plan.approve" => handle_plan_approve(ctx, params)        │   │
│  │    ...                                                        │   │
│  │  }                                                            │   │
│  └──────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────┘
```

### Data Model

**DaemonContext** - Shared state passed to request handlers:

```rust
pub struct DaemonContext {
    /// Loop lifecycle management
    pub loop_manager: Arc<RwLock<LoopManager<JsonlStorage, AnthropicClient, LocalToolRouter>>>,

    /// LLM client for chat
    pub llm_client: Arc<AnthropicClient>,

    /// Tool execution
    pub tool_router: Arc<LocalToolRouter>,

    /// Event broadcasting to TUI clients
    pub event_tx: broadcast::Sender<DaemonEvent>,

    /// Chat session state (conversation history)
    pub chat_session: Arc<RwLock<ChatSession>>,

    /// Persistent storage
    pub storage: Arc<JsonlStorage>,
}
```

**ChatSession** - Conversation state for chat view:

```rust
pub struct ChatSession {
    /// Conversation history
    pub messages: Vec<Message>,

    /// Accumulated token usage
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
}
```

### API Design

Request handlers become async functions that take context:

```rust
async fn handle_request(
    request: DaemonRequest,
    ctx: &DaemonContext,
) -> DaemonResponse {
    match request.method.as_str() {
        // Connection
        "ping" => handle_ping(request.id),
        "status" => handle_status(request.id),

        // Chat
        "chat.send" => handle_chat_send(request.id, &request.params, ctx).await,
        "chat.cancel" => handle_chat_cancel(request.id, ctx).await,
        "chat.clear" => handle_chat_clear(request.id, ctx).await,

        // Loops
        "loop.list" => handle_loop_list(request.id, ctx).await,
        "loop.get" => handle_loop_get(request.id, &request.params, ctx).await,
        "loop.create_plan" => handle_loop_create_plan(request.id, &request.params, ctx).await,
        "loop.start" => handle_loop_start(request.id, &request.params, ctx).await,
        "loop.pause" => handle_loop_pause(request.id, &request.params, ctx).await,
        "loop.resume" => handle_loop_resume(request.id, &request.params, ctx).await,
        "loop.cancel" => handle_loop_cancel(request.id, &request.params, ctx).await,
        "loop.delete" => handle_loop_delete(request.id, &request.params, ctx).await,

        // Plan approval
        "plan.approve" => handle_plan_approve(request.id, &request.params, ctx).await,
        "plan.reject" => handle_plan_reject(request.id, &request.params, ctx).await,
        "plan.iterate" => handle_plan_iterate(request.id, &request.params, ctx).await,
        "plan.get_preview" => handle_plan_get_preview(request.id, &request.params, ctx).await,

        // Metrics
        "metrics.get" => handle_metrics_get(request.id, ctx).await,

        _ => DaemonResponse::error(
            request.id,
            DaemonError::new(ErrorCode::METHOD_NOT_FOUND, format!("Unknown method: {}", request.method)),
        ),
    }
}
```

### Implementation Plan

#### Phase 1: Core Daemon Wiring (Critical Path)

**1.1 Create DaemonContext struct**

File: `src/daemon/context.rs` (new)

```rust
pub struct DaemonContext {
    pub loop_manager: Arc<RwLock<LoopManager<JsonlStorage, AnthropicClient, LocalToolRouter>>>,
    pub llm_client: Arc<AnthropicClient>,
    pub tool_router: Arc<LocalToolRouter>,
    pub event_tx: broadcast::Sender<DaemonEvent>,
    pub chat_session: Arc<RwLock<ChatSession>>,
    pub storage: Arc<JsonlStorage>,
}

impl DaemonContext {
    pub fn new(data_dir: &Path) -> Result<Self> {
        let storage = Arc::new(JsonlStorage::new(data_dir.join(".taskstore"))?);
        let llm_client = Arc::new(AnthropicClient::from_env()?);
        let tool_router = Arc::new(LocalToolRouter::new());
        let (event_tx, _) = broadcast::channel(256);

        let loop_manager = Arc::new(RwLock::new(LoopManager::new(
            storage.clone(),
            llm_client.clone(),
            tool_router.clone(),
        )));

        Ok(Self {
            loop_manager,
            llm_client,
            tool_router,
            event_tx,
            chat_session: Arc::new(RwLock::new(ChatSession::new())),
            storage,
        })
    }
}
```

**1.2 Refactor Daemon::run() to use context**

File: `src/daemon/mod.rs`

```rust
impl Daemon {
    pub async fn run(&mut self) -> Result<()> {
        // ... existing PID file logic ...

        // Create context with all components
        let ctx = Arc::new(DaemonContext::new(&self.config.data_dir)?);

        // Create async request handler
        let ctx_clone = ctx.clone();
        let handler = Arc::new(AsyncHandler::new(move |request| {
            let ctx = ctx_clone.clone();
            async move { handle_request(request, &ctx).await }
        }));

        // Run server
        server.run(handler).await
    }
}
```

**1.3 Implement loop.* handlers**

File: `src/daemon/handlers/loops.rs` (new)

```rust
pub async fn handle_loop_list(id: u64, ctx: &DaemonContext) -> DaemonResponse {
    let manager = ctx.loop_manager.read().await;
    match manager.list_loops() {
        Ok(loops) => {
            let loops_json: Vec<_> = loops.iter().map(|l| serde_json::to_value(l).unwrap()).collect();
            DaemonResponse::success(id, json!({"loops": loops_json}))
        }
        Err(e) => DaemonResponse::error(id, DaemonError::internal(e.to_string())),
    }
}

pub async fn handle_loop_start(id: u64, params: &Value, ctx: &DaemonContext) -> DaemonResponse {
    let loop_id = match params["id"].as_str() {
        Some(id) => id,
        None => return DaemonResponse::error(id, DaemonError::invalid_params("Missing 'id'")),
    };

    let mut manager = ctx.loop_manager.write().await;
    match manager.start_loop(loop_id).await {
        Ok(()) => {
            // Broadcast event
            if let Ok(loop_record) = manager.get_loop(loop_id) {
                let _ = ctx.event_tx.send(DaemonEvent::loop_updated(&loop_record));
            }
            DaemonResponse::success(id, json!({}))
        }
        Err(e) => DaemonResponse::error(id, DaemonError::internal(e.to_string())),
    }
}

// Similar for pause, resume, cancel, delete...
```

**1.4 Implement chat.send handler**

File: `src/daemon/handlers/chat.rs` (new)

```rust
pub async fn handle_chat_send(id: u64, params: &Value, ctx: &DaemonContext) -> DaemonResponse {
    let message = match params["message"].as_str() {
        Some(m) => m,
        None => return DaemonResponse::error(id, DaemonError::invalid_params("Missing 'message'")),
    };

    // Add user message to session
    {
        let mut session = ctx.chat_session.write().await;
        session.messages.push(Message::user(message));
    }

    // Build completion request
    let session = ctx.chat_session.read().await;
    let request = CompletionRequest {
        system: CHAT_SYSTEM_PROMPT.to_string(),
        messages: session.messages.clone(),
        tools: ctx.tool_router.definitions(),
        max_tokens: 4096,
        ..Default::default()
    };
    drop(session);

    // Call LLM
    let response = match ctx.llm_client.complete(request).await {
        Ok(r) => r,
        Err(e) => return DaemonResponse::error(id, DaemonError::internal(e.to_string())),
    };

    // Broadcast response text
    let _ = ctx.event_tx.send(DaemonEvent::chat_chunk(&response.text, false));

    // Execute tool calls
    for call in &response.tool_calls {
        let _ = ctx.event_tx.send(DaemonEvent::chat_tool_call(&call.name, &call.input));

        let result = ctx.tool_router.execute(call.clone(), &PathBuf::from(".")).await;
        match result {
            Ok(r) => {
                let _ = ctx.event_tx.send(DaemonEvent::chat_tool_result(&call.name, &r.output));
            }
            Err(e) => {
                let _ = ctx.event_tx.send(DaemonEvent::chat_tool_result(&call.name, &e.to_string()));
            }
        }
    }

    // Mark done
    let _ = ctx.event_tx.send(DaemonEvent::chat_chunk("", true));

    // Add assistant response to session
    {
        let mut session = ctx.chat_session.write().await;
        session.messages.push(Message::assistant(&response.text));
        session.total_input_tokens += response.usage.input_tokens;
        session.total_output_tokens += response.usage.output_tokens;
    }

    DaemonResponse::success(id, json!({"message_id": generate_id()}))
}
```

#### Phase 2: Event Broadcasting

**2.1 Wire event channel to IpcServer**

The `IpcServer` already has `broadcast()`. Need to ensure events from `ctx.event_tx` reach it.

Option A: Pass event_tx to IpcServer and have it subscribe internally.
Option B: Daemon polls event_rx and calls server.broadcast().

Recommend Option A for cleaner separation.

**2.2 Emit events from LoopManager**

Add event_tx to LoopManager constructor or use a callback pattern.

#### Phase 3: Plan Approval Flow

**3.1 Implement approval handlers**

```rust
pub async fn handle_plan_approve(id: u64, params: &Value, ctx: &DaemonContext) -> DaemonResponse {
    let plan_id = params["id"].as_str()?;

    let manager = ctx.loop_manager.read().await;
    let plan = manager.get_loop(plan_id)?;

    // Parse specs from plan artifact
    let specs = parse_plan_specs(&plan.output_artifacts[0])?;

    // Spawn spec loops
    drop(manager);
    let mut manager = ctx.loop_manager.write().await;
    for spec in &specs {
        manager.create_child_loop(&plan, LoopType::Spec, spec)?;
    }

    // Broadcast approval event
    let _ = ctx.event_tx.send(DaemonEvent::plan_approved(plan_id, specs.len()));

    DaemonResponse::success(id, json!({"specs_spawned": specs.len()}))
}
```

#### Phase 4: CLI Commands

Wire `src/main.rs` handlers to use IpcClient:

```rust
async fn handle_list_command(status: Option<&str>, loop_type: Option<&str>, config: &Config) -> Result<()> {
    let client = IpcClient::connect().await?;
    let response = client.list_loops().await?;

    if let Some(loops) = response.result.and_then(|r| r["loops"].as_array()) {
        for loop_record in loops {
            let id = loop_record["id"].as_str().unwrap_or("?");
            let ltype = loop_record["loop_type"].as_str().unwrap_or("?");
            let lstatus = loop_record["status"].as_str().unwrap_or("?");
            println!("{} - {} ({})", id.cyan(), ltype, lstatus.yellow());
        }
    }

    Ok(())
}
```

## Alternatives Considered

### Alternative 1: Keep Components Separate, Use Message Passing

- **Description:** Keep Daemon minimal, use channels to communicate with separate LoopManager process
- **Pros:** Better isolation, easier testing
- **Cons:** More complexity, more IPC, harder to debug
- **Why not chosen:** Over-engineering for current needs. Single-process is simpler.

### Alternative 2: Lazy Initialization of Components

- **Description:** Only create LlmClient/LoopManager when first needed
- **Pros:** Faster startup when not using all features
- **Cons:** More complex initialization, error handling scattered
- **Why not chosen:** Startup time isn't a concern; simpler to initialize upfront

### Alternative 3: Global State via lazy_static

- **Description:** Use global singletons for LlmClient, LoopManager
- **Pros:** Easy access from anywhere
- **Cons:** Hard to test, hidden dependencies, no dependency injection
- **Why not chosen:** Anti-pattern; explicit context passing is cleaner

## Technical Considerations

### Dependencies

**Existing (no changes needed):**
- `tokio` - Async runtime
- `serde_json` - JSON handling
- All component crates already in Cargo.toml

### Performance

- Context creation: ~10ms (LLM client init, storage load)
- Request handling: Dominated by LLM API latency (~1-5s)
- Event broadcasting: Negligible (<1ms)
- No performance concerns for this wiring work

### Security

- API key read from environment (existing pattern)
- No new attack surface introduced
- Same Unix socket permissions as before

### Testing Strategy

**Unit Tests:**
- Each handler function tested with mock context
- Use MockLlmClient for chat tests
- Use in-memory storage for loop tests

**Integration Tests:**
```rust
#[tokio::test]
async fn test_chat_send_integration() {
    let ctx = DaemonContext::new_test().await;

    let params = json!({"message": "Hello"});
    let response = handle_chat_send(1, &params, &ctx).await;

    assert!(response.is_success());
    assert!(response.result.unwrap()["message_id"].is_string());
}

#[tokio::test]
async fn test_loop_lifecycle_integration() {
    let ctx = DaemonContext::new_test().await;

    // Create
    let create_resp = handle_loop_create_plan(1, &json!({"description": "Test"}), &ctx).await;
    let loop_id = create_resp.result.unwrap()["id"].as_str().unwrap();

    // List
    let list_resp = handle_loop_list(2, &ctx).await;
    let loops = list_resp.result.unwrap()["loops"].as_array().unwrap();
    assert_eq!(loops.len(), 1);

    // Start
    let start_resp = handle_loop_start(3, &json!({"id": loop_id}), &ctx).await;
    assert!(start_resp.is_success());
}
```

**Manual Verification:**
```bash
# Start daemon
cargo run -q -- daemon start

# Test chat
cargo run -q
# Type: "What is 2+2?"
# Should get LLM response

# Test loops
cargo run -q -- plan "Create hello.txt"
cargo run -q -- list
cargo run -q -- status <id>
```

### Rollout Plan

1. Implement Phase 1 (daemon wiring)
2. Run `cargo test` - verify no regressions
3. Manual test: daemon start, chat.send
4. Implement Phase 2 (events)
5. Manual test: TUI shows events
6. Implement Phase 3 (approval)
7. Manual test: plan approval flow
8. Implement Phase 4 (CLI)
9. Full integration test
10. Commit with reference to this design doc

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Deadlock from nested locks | Medium | High | Use RwLock carefully, document lock ordering |
| Memory leak from chat history | Low | Medium | Implement session cleanup, max history size |
| Event channel overflow | Low | Low | Use bounded channel, drop old events |
| API key not set | Medium | Medium | Clear error message, graceful degradation |
| Async handler complexity | Medium | Medium | Keep handlers simple, extract business logic |

## Open Questions

- [x] Where should DaemonContext live? → `src/daemon/context.rs`
- [x] Should handlers be in mod.rs or separate files? → Separate files in `src/daemon/handlers/`
- [ ] Should chat session persist across daemon restarts? (Defer - session-only for now)
- [ ] Max chat history size? (Defer - implement if memory becomes issue)

## Files Changed Summary

| File | Type | Description |
|------|------|-------------|
| `src/daemon/mod.rs` | Modify | Use DaemonContext, refactor run() |
| `src/daemon/context.rs` | Create | DaemonContext struct |
| `src/daemon/handlers/mod.rs` | Create | Handler module |
| `src/daemon/handlers/loops.rs` | Create | Loop handlers |
| `src/daemon/handlers/chat.rs` | Create | Chat handlers |
| `src/daemon/handlers/plan.rs` | Create | Plan approval handlers |
| `src/main.rs` | Modify | Wire CLI commands to IpcClient |
| `src/lib.rs` | Modify | Export new modules |

---

## Implementation Phases: Complete Laundry List

### Phase 1: Core Daemon Wiring (CRITICAL)

**Priority:** P0 - Unblocks everything
**Estimated Effort:** Large

#### 1.1 Refactor Daemon Struct

**File:** `src/daemon/mod.rs`

- [ ] Add `DaemonContext` struct with:
  - `loop_manager: Arc<RwLock<LoopManager<...>>>`
  - `llm_client: Arc<AnthropicClient>`
  - `tool_router: Arc<LocalToolRouter>`
  - `storage: Arc<JsonlStorage>`
  - `event_tx: broadcast::Sender<DaemonEvent>`
- [ ] Add `context: Option<Arc<DaemonContext>>` field to `Daemon`
- [ ] Initialize components in `Daemon::run()`:
  - Create `JsonlStorage` with data_dir
  - Create `AnthropicClient::from_env()`
  - Create `LocalToolRouter::new()`
  - Create `LoopManager` with dependencies
  - Create broadcast channel for events

#### 1.2 Create Async Request Handler

**File:** `src/daemon/handler.rs` (NEW)

- [ ] Create `AsyncRequestHandler` struct
- [ ] Implement `RequestHandler` trait with async handle method
- [ ] Route requests to appropriate handlers:

| Method | Handler Function | Status |
|--------|-----------------|--------|
| `ping` | inline | Already works |
| `status` | inline | Already works |
| `chat.send` | `handle_chat_send()` | TODO |
| `chat.cancel` | `handle_chat_cancel()` | TODO |
| `chat.clear` | `handle_chat_clear()` | TODO |
| `loop.list` | `loop_manager.list_loops()` | TODO |
| `loop.get` | `loop_manager.get_loop(id)` | TODO |
| `loop.create_plan` | `loop_manager.create_loop()` | TODO |
| `loop.start` | `loop_manager.start_loop(id)` | TODO |
| `loop.pause` | `loop_manager.pause_loop(id)` | TODO |
| `loop.resume` | `loop_manager.resume_loop(id)` | TODO |
| `loop.cancel` | `loop_manager.stop_loop(id)` | TODO |
| `loop.delete` | `storage.delete()` | TODO |
| `plan.approve` | `handle_plan_approve()` | TODO |
| `plan.reject` | `handle_plan_reject()` | TODO |
| `plan.iterate` | `handle_plan_iterate()` | TODO |
| `plan.get_preview` | `handle_plan_preview()` | TODO |
| `metrics.get` | `handle_metrics_get()` | TODO |
| `connect` | `handle_connect()` | TODO |
| `disconnect` | `handle_disconnect()` | TODO |

#### 1.3 Implement Chat Handler

**File:** `src/daemon/chat.rs` (NEW)

- [ ] Create `ChatSession` struct:
  ```rust
  pub struct ChatSession {
      pub messages: Vec<Message>,
      pub created_at: i64,
  }
  ```
- [ ] Implement `handle_chat_send()`:
  - Add user message to session
  - Build CompletionRequest with tools
  - Call `llm_client.complete()`
  - Execute any tool calls
  - Broadcast events (chat.chunk, chat.tool_call, chat.tool_result)
  - Add assistant response to session
  - Return message_id
- [ ] Implement `handle_chat_clear()`:
  - Clear session messages
- [ ] Define CHAT_SYSTEM_PROMPT constant

#### 1.4 Update IPC Server for Async

**File:** `src/ipc/server.rs`

- [ ] Make `RequestHandler::handle()` async:
  ```rust
  fn handle(&self, request: DaemonRequest) -> impl Future<Output = DaemonResponse> + Send;
  ```
- [ ] Update `handle_client()` to await handler

#### 1.5 Wire Server to Daemon

**File:** `src/daemon/mod.rs`

- [ ] Replace `CallbackHandler` with `AsyncRequestHandler`
- [ ] Pass `DaemonContext` to handler
- [ ] Ensure event_tx is same channel as IpcServer uses

#### 1.6 Verification

```bash
cargo run -q -- daemon start
cargo run -q -- daemon status
# Chat test (requires ANTHROPIC_API_KEY)
cargo run -q   # Enter TUI, type message
# Loop test
cargo run -q -- plan "test task"
```

---

### Phase 2: Event Broadcasting

**Priority:** P1 - Needed for TUI updates
**Estimated Effort:** Medium

#### 2.1 Emit Events from LoopManager

**File:** `src/manager/loop_manager.rs`

- [ ] Add `event_tx: Option<broadcast::Sender<DaemonEvent>>` field
- [ ] Add `with_events()` constructor or setter
- [ ] Emit `loop.created` in `create_loop()`
- [ ] Emit `loop.updated` in:
  - `start_loop()`
  - `pause_loop()`
  - `resume_loop()`
  - `stop_loop()`
  - `update_status()`

#### 2.2 Emit Events from Loop::run()

**File:** `src/domain/loop_record.rs`

- [ ] Pass event sender to `run()` method
- [ ] Emit `loop.iteration` after each iteration
- [ ] Emit `loop.updated` on status changes

#### 2.3 Emit Chat Events

**File:** `src/daemon/chat.rs`

- [ ] `chat.chunk` emitted during response
- [ ] `chat.tool_call` emitted before tool execution
- [ ] `chat.tool_result` emitted after tool execution
- [ ] Final `chat.chunk` with `done: true`

#### 2.4 Emit Plan Events

**File:** `src/daemon/handlers/plan.rs`

- [ ] Emit `plan.awaiting_approval` when plan completes
- [ ] Emit `plan.approved` in `handle_plan_approve()`
- [ ] Emit `plan.rejected` in `handle_plan_reject()`

#### 2.5 Verification

```bash
cargo run -q
# Create via /plan, see loop appear in Loops view
# See status updates in real-time
```

---

### Phase 3: Plan Approval Flow (User Gate)

**Priority:** P1 - Needed for full loop execution
**Estimated Effort:** Medium

#### 3.1 Track Plans Awaiting Approval

**File:** `src/domain/loop_record.rs`

- [ ] Consider adding `AwaitingApproval` to `LoopStatus` enum
- [ ] Or use `Complete` with a flag in context

#### 3.2 Implement Plan Approval Handler

**File:** `src/daemon/handlers/plan.rs`

- [ ] Implement `handle_plan_approve()`:
  - Get plan loop
  - Verify it's a completed plan
  - Parse plan artifact for specs
  - Spawn spec loops
  - Emit event
  - Return specs_spawned count

#### 3.3 Implement Plan Rejection Handler

- [ ] Implement `handle_plan_reject()`:
  - Update plan status to `Failed`
  - Store rejection reason
  - Emit `plan.rejected` event

#### 3.4 Implement Plan Iteration Handler

- [ ] Implement `handle_plan_iterate()`:
  - Add feedback to plan's `progress` field
  - Reset status to `Running`
  - Trigger another iteration

**File:** `src/manager/loop_manager.rs`

- [ ] Add `force_iterate(id: &str, feedback: &str)` method

#### 3.5 Implement Plan Preview Handler

- [ ] Implement `handle_plan_preview()`:
  - Read plan artifact content
  - Parse specs list
  - Return both

#### 3.6 Wire TUI Approval View

**File:** `src/tui/app.rs` / `src/main.rs`

- [ ] On approve: call `client.approve_plan(id)`
- [ ] On reject: call `client.reject_plan(id, reason)`
- [ ] On iterate: call `client.iterate_plan(id, feedback)`

#### 3.7 Verification

```bash
cargo run -q -- plan "Create hello.txt"
# Wait for completion
cargo run -q -- approve <plan-id>
# Should spawn spec loops
```

---

### Phase 4: CLI Commands

**Priority:** P2 - Enables testing without TUI
**Estimated Effort:** Small

#### 4.1 Wire Plan Command

**File:** `src/main.rs`

- [ ] Update `handle_plan_command()` to use IpcClient

#### 4.2 Wire List Command

- [ ] Update `handle_list_command()`:
  - Connect to daemon
  - Call `client.list_loops()`
  - Filter by status/type
  - Print table

#### 4.3 Wire Status Command

- [ ] Update `handle_status_command()`:
  - Call `client.get_loop(id)`
  - Print detailed info

#### 4.4 Wire Approve/Reject Commands

- [ ] Update `handle_approve_command()`
- [ ] Update `handle_reject_command()`

#### 4.5 Wire Pause/Resume/Cancel Commands

- [ ] Update `handle_pause_command()`
- [ ] Update `handle_resume_command()`
- [ ] Update `handle_cancel_command()`

#### 4.6 Verification

```bash
cargo run -q -- daemon start
cargo run -q -- plan "test"
cargo run -q -- list
cargo run -q -- status <id>
cargo run -q -- approve <id>
cargo run -q -- pause <id>
cargo run -q -- resume <id>
cargo run -q -- cancel <id>
```

---

### Phase 5: LLM Streaming Integration

**Priority:** P3 - Nice to have
**Estimated Effort:** Medium

#### 5.1 Add Streaming to AnthropicClient

**File:** `src/llm/anthropic.rs`

- [ ] Add `complete_stream()` method
- [ ] Set `stream: true` in request
- [ ] Return channel of `StreamEvent`s

#### 5.2 Add SSE Parser

**File:** `src/llm/streaming.rs`

- [ ] Parse SSE lines: `event:`, `data:`
- [ ] Handle Anthropic event types:
  - `message_start`
  - `content_block_delta` → text chunks
  - `message_stop`

#### 5.3 Wire Streaming to Chat Handler

**File:** `src/daemon/chat.rs`

- [ ] Use `complete_stream()` instead of `complete()`
- [ ] Broadcast `chat.chunk` for each delta

#### 5.4 Verification

```bash
# In TUI, response appears token-by-token
```

---

### Phase 6: Runner Subprocesses (FUTURE)

**Priority:** P4 - Defer
**Estimated Effort:** Large

#### 6.1 Create Runner Binary

**File:** `src/bin/runner.rs` (NEW)

- [ ] Runner process: connect, handshake, receive jobs, execute, return results

#### 6.2 Network Sandboxing

**File:** `src/runner/sandbox.rs` (NEW)

- [ ] `runner-no-net`: block network via namespace or seccomp

#### 6.3 Daemon Runner Management

**File:** `src/daemon/runner_manager.rs` (NEW)

- [ ] Spawn runners on startup
- [ ] Handle handshakes
- [ ] Route jobs by lane
- [ ] Handle crashes

#### 6.4 Update ToolRouter

- [ ] Add `RemoteToolRouter` for runner IPC

**DEFERRED** - `LocalToolRouter` sufficient for MVP.

---

## Complete Files Checklist

### New Files
- [ ] `src/daemon/context.rs` - DaemonContext
- [ ] `src/daemon/handlers/mod.rs` - Handler module
- [ ] `src/daemon/handlers/chat.rs` - Chat handlers
- [ ] `src/daemon/handlers/loops.rs` - Loop handlers
- [ ] `src/daemon/handlers/plan.rs` - Plan approval handlers

### Modified Files
- [ ] `src/daemon/mod.rs` - Daemon refactor
- [ ] `src/ipc/server.rs` - Async handler trait
- [ ] `src/manager/loop_manager.rs` - Event emission
- [ ] `src/domain/loop_record.rs` - Event emission
- [ ] `src/main.rs` - Wire CLI commands
- [ ] `src/lib.rs` - Export modules

### Future Files (Phase 5-6)
- [ ] `src/llm/anthropic.rs` - Streaming
- [ ] `src/bin/runner.rs`
- [ ] `src/runner/sandbox.rs`
- [ ] `src/daemon/runner_manager.rs`

---

## References

- `docs/ipc-protocol.md` - Message schemas and method definitions
- `docs/process-model.md` - Daemon lifecycle specification
- `docs/architecture.md` - System architecture overview
- `docs/loop.md` - Loop execution model
- `docs/tui.md` - TUI event handling specification
