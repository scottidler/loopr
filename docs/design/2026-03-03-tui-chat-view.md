# Design Document: TUI Chat View — Free Chat + Plan Mode

**Author:** Scott Idler + Claude
**Date:** 2026-03-03
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

The loopr TUI currently displays dashboard/list views but has no conversational interface. This design adds a Chat view as the default screen with two modes: **Chat mode** — a full Claude Code-like LLM conversation where the user explores ideas freely using a TUI-side LLM client, and **Plan mode** — activated via `/plan`, which hands the accumulated chat context to the Coordinator agent for a targeted interview, plan drafting via the Rule of Five, and eventual plan activation that starts the Loopr automation pipeline. Chat input patterns are stolen from taskdaemon's REPL interface.

## Problem Statement

### Background

Loopr's TUI launches and shows seven views (Dashboard, Works, Bundles, Ticks, Learnings, Locks, Agents) with vim-style navigation. The only input is a single-line goal popup (`g` key). The Coordinator agent has an interview flow (`Interviewing` FSM state) with `coordinator.interview_question` and `coordinator.interview_respond` IPC, but the TUI ignores these events.

The user needs to explore ideas, discuss architecture, and build context *before* engaging the Coordinator. The Coordinator should receive a rich context dump, not a cold one-liner.

### Problem

1. **No chat interface.** The TUI has no way to converse with an LLM.
2. **No context building.** Users can't explore ideas before committing to a plan.
3. **Interview flow is disconnected.** The Coordinator interview exists backend-side but has no frontend.
4. **Cold start.** The Coordinator receives a bare goal string with no surrounding context.

### Goals

- Chat view as the default screen with a Claude Code-like experience (always-input, streaming responses)
- Free-form LLM conversation using a TUI-side `AgentLlmClient` (no daemon involvement)
- `/plan` command transitions to Plan mode, serializing chat context to the Coordinator
- Coordinator interview flow displayed inline in the same chat history
- Rule of Five applied to plan drafts before presenting to user
- Plan approval (Draft → Active) starts Loopr automation
- UTF-8 safe text input with cursor movement (stolen from taskdaemon)

### Non-Goals

- Markdown rendering in responses (future)
- Tool use in free chat (Coordinator has tools; free chat is text-only for now)
- Multi-line input (Enter submits; Shift+Enter for newlines is future)
- Persisting free chat history across TUI restarts (future)

## Proposed Solution

### Overview

#### Two Modes, One Chat History

The Chat view has a single scrollable message history. Messages from both modes appear in the same timeline:

```
[Chat mode]    > Tell me about the existing bundle validation flow
[Chat mode]      The bundle validation in loopr works as follows...
[Chat mode]    > What if we added parallel validation?
[Chat mode]      That's an interesting idea. You could...
[Chat mode]    > /plan
[System]         Entering Plan mode. Chat context sent to Coordinator.
[Plan mode]      Based on your conversation, I have some clarifying questions:
[Plan mode]      1. Should parallel validation be opt-in or the default?
[Plan mode]    > It should be opt-in via config
[Plan mode]      Draft plan created. Refining (pass 2/5)...
[Plan mode]      === Proposed Plan ===
[Plan mode]      Title: Parallel Bundle Validation
[Plan mode]      ...
[System]         Plan ready. Press Ctrl+a to approve and activate.
```

#### Chat Mode (default)

- TUI creates a standalone `AgentLlmClient` with a local `broadcast::channel`
- User messages go directly to the LLM via `call_with_history()`
- Streaming chunks arrive via local broadcast receiver → rendered as in-progress assistant message
- Full conversation history maintained in `Vec<ChatMessage>` for multi-turn context
- No daemon involvement

#### Plan Mode (entered via `/plan`)

- Chat transcript serialized and sent to Coordinator as goal context
- `coordinator.set_goal` IPC with goal = chat transcript summary
- `agent.start` IPC to start Coordinator
- Coordinator enters `Interviewing` FSM state
- Interview questions arrive via `coordinator.interview_question` events → rendered as assistant messages
- User responses sent via `coordinator.interview_respond` IPC
- When Coordinator has enough context → generates Plan via Rule of Five (5 refinement passes)
- Plan shown to user as formatted system message
- User approves → `coordinator.approve_plan` IPC → Plan transitions Draft → Active → automation starts

### Architecture

#### State Model

```rust
/// Chat operating mode
pub enum ChatMode {
    /// Free-form LLM conversation (TUI-side)
    Chat,
    /// Coordinator interview + plan drafting (daemon-side)
    Plan,
}

pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
}

pub enum ChatRole {
    User,
    Assistant,
    System,
}

// New fields on App
pub chat_mode: ChatMode,
pub chat_history: Vec<ChatMessage>,
pub chat_input: String,
pub chat_cursor_pos: usize,         // byte offset, UTF-8 safe
pub chat_streaming: bool,           // true while LLM is generating
pub chat_response_buffer: String,   // accumulates streaming chunks (Chat mode)
pub chat_scroll: Option<usize>,     // None = auto-scroll to bottom
pub pending_chat_submit: Option<String>,
pub pending_plan_id: Option<String>, // set when coordinator proposes a plan
```

#### LLM Client (Chat Mode)

Follows the taskdaemon pattern (`td/src/tui/runner.rs: start_repl_request`):

```rust
// Created once at TUI startup
let config = Config::load(None)?;
let chat_config = config.agents.coordinator.clone(); // reuse coordinator model config
let (llm_event_tx, _) = broadcast::channel::<DaemonEvent>(256);
let llm_client = Arc::new(AgentLlmClient::new(
    chat_config, "tui-chat".to_string(), llm_event_tx.clone(),
)?);

// Per-request: spawn background task, receive chunks via broadcast
let mut llm_event_rx = llm_event_tx.subscribe();
```

**Streaming pattern** (stolen from taskdaemon's `process_stream_chunks`):

```rust
// At START of every event loop iteration (before select!):
// Non-blocking drain of all available streaming chunks
while let Ok(event) = llm_event_rx.try_recv() {
    if let Some(chunk) = extract_llm_chunk(&event) {
        app.chat_response_buffer.push_str(&chunk.text);
        if chunk.is_final {
            // Finalize: move buffer → ChatMessage::Assistant
            let content = std::mem::take(&mut app.chat_response_buffer);
            app.chat_history.push(ChatMessage::assistant(content));
            app.chat_streaming = false;
        }
    }
}
```

**LLM call** (background tokio task):
```rust
// On user submit in Chat mode:
let client = llm_client.clone();
let messages = app.chat_history_as_llm_messages(); // Convert to Vec<ChatMessage>
let system_prompt = CHAT_SYSTEM_PROMPT.to_string();
app.chat_streaming = true;

llm_task = Some(tokio::spawn(async move {
    client.call_with_history(&system_prompt, &messages).await
}));
```

**Conversation history** is maintained as `Vec<ChatMessage>` in App. On each submit, convert to `Vec<agents::implementer::ChatMessage>` for `call_with_history()`.

**System prompt** for free chat:
```
You are an AI assistant embedded in the Loopr development orchestrator.
You help the user explore ideas, discuss architecture, and plan changes
to their codebase. When the user is ready to formalize a plan, they
will type /plan.
```

#### Coordinator Integration (Plan Mode)

When user types `/plan`:

1. Serialize `chat_history` to a transcript string
2. Call `coordinator.set_goal` with `{ "goal": "<transcript>" }`
3. Call `agent.start` with `{ "agent_type": "Coordinator" }`
4. Set `chat_mode = ChatMode::Plan`
5. Add `ChatMessage::System("Entering Plan mode. Chat context sent to Coordinator.")`

In Plan mode, the event loop handles daemon events:
- `coordinator.interview_question` → `ChatMessage::Assistant` with questions
- `agent.llm_output` (Coordinator) → set `chat_streaming` indicator
- User messages → `coordinator.interview_respond` IPC

When Coordinator proposes a plan:
- Plan ID stored in `pending_plan_id`
- Plan details shown as system message
- User presses `Ctrl+a` to approve

#### Event Loop (Modified `tokio::select!`)

```rust
tokio::select! {
    // 1. Keyboard events (existing)
    crossterm_event = events.next() => { ... }

    // 2. LLM streaming chunks (NEW — Chat mode only)
    Ok(event) = llm_event_rx.recv(), if chat_mode == ChatMode::Chat => {
        // Extract chunk from DaemonEvent, append to response_buffer
        // On is_final: finalize buffer as ChatMessage::Assistant
    }

    // 3. LLM task completion (NEW — Chat mode only)
    result = &mut llm_task, if llm_task_active => {
        // Finalize response, clear streaming state
    }

    // 4. IPC messages from daemon (existing + Plan mode additions)
    ipc_msg = client.recv(), if client.is_some() => {
        // BEFORE event_collection(): check for coordinator.* events
        // Handle coordinator.interview_question, agent.llm_output
        // Fall through to existing collection refresh
    }

    // 5. Reconnection timer (existing)
    _ = reconnect_timer.tick(), if client.is_none() => { ... }
}
```

### Input Handling

#### Always-Input Design (Claude Code pattern)

**In Chat view, the user is ALWAYS in input mode.** No bare letter hotkeys. All printable characters go to the input buffer. This is fundamentally different from the other views which use vim-style navigation.

**Chat view key bindings:**

| Key | Action |
|-----|--------|
| Any printable char | Insert at cursor position |
| `Backspace` | Delete character before cursor (UTF-8 safe) |
| `Delete` | Delete character after cursor |
| `Left` / `Right` | Move cursor (char boundary aware) |
| `Home` / `End` | Jump to start/end of input |
| `Enter` | Submit input (to LLM in Chat mode, to Coordinator in Plan mode) |
| `Esc` | Enter scroll mode (input preserved, cursor hidden) |
| `Tab` | Switch to next view (Dashboard, etc.) |
| `Shift+Tab` | Switch to previous view |
| `PgUp` / `PgDn` | Scroll history |
| `Ctrl+c` | Quit |
| `Ctrl+a` | Approve plan (Plan mode only, when `pending_plan_id` is set) |

**Scroll mode** (entered via `Esc`):

| Key | Action |
|-----|--------|
| `j` / `Down` | Scroll down |
| `k` / `Up` | Scroll up |
| `g` | Scroll to top |
| `G` | Scroll to bottom (auto-scroll) |
| `PgUp` / `PgDn` | Page scroll |
| Any printable char | Exit scroll mode, re-enter input, insert char |
| `Esc` | Stay in scroll mode |

This means the `InputMode` enum becomes:

```rust
pub enum InputMode {
    Normal,       // existing — used by non-Chat views
    GoalInput,    // existing — goal popup (may be deprecated)
    ChatInput,    // NEW — always-on text input in Chat view
    ChatScroll,   // NEW — scroll mode in Chat view (Esc to enter, any char to exit)
}
```

#### Slash Commands

Input starting with `/` is intercepted before sending to LLM:

| Command | Action |
|---------|--------|
| `/plan` | Enter Plan mode (serialize chat context → Coordinator) |
| `/chat` | Return to Chat mode (leave Plan mode without discarding) |
| `/clear` | Clear chat history |
| `/help` | Show available commands |

#### UTF-8 Cursor Helpers (stolen from taskdaemon)

```rust
fn prev_char_boundary(input: &str, pos: usize) -> usize {
    let mut new_pos = pos.saturating_sub(1);
    while new_pos > 0 && !input.is_char_boundary(new_pos) {
        new_pos -= 1;
    }
    new_pos
}

fn next_char_boundary(input: &str, pos: usize) -> usize {
    let mut new_pos = pos + 1;
    while new_pos < input.len() && !input.is_char_boundary(new_pos) {
        new_pos += 1;
    }
    new_pos.min(input.len())
}
```

### Rendering

Exact 3-zone layout stolen from taskdaemon (`td/src/tui/views.rs`):

#### Global Layout (3 zones)

```rust
let chunks = Layout::default()
    .direction(Direction::Vertical)
    .constraints([
        Constraint::Length(3), // Header: tabs + connection
        Constraint::Min(0),    // Content: chat area (or other views)
        Constraint::Length(3), // Footer: keybinding hints
    ])
    .split(frame.area());
```

#### Target Appearance

```
┌──────────────────────────────────────────────────────────┐
│ ● Loopr │ Chat|Plan · Dashboard · Works · Bundles · More │
├──── Chat ────────────────────────────────────────────────┤
│ Welcome to Loopr Chat                                    │
│                                                          │
│ Type a message and press Enter to explore ideas.         │
│ Type /plan when ready to formalize a plan.               │
│                                                          │
│                                                          │
│                                                          │
│                                                          │
│                                                          │
│                                                          │
│ > _                                                      │
├──────────────────────────────────────────────────────────┤
│ [Enter] Send  /plan Plan  /clear Clear   [Tab] Views [?] │
└──────────────────────────────────────────────────────────┘
```

#### Header (stolen from taskdaemon `render_header`)

Single bordered block with left-aligned tabs and right-aligned metrics:

```
Left:  ● Loopr │ Chat|Plan · Dashboard · Works · Bundles · More
Right: ↑1.2K ↓2.5K │ $0.04  (shown when chat has activity)
```

- `●` connection indicator: green=connected, red=disconnected
- `Loopr` in cyan bold (HEADER color)
- `Chat|Plan` — active mode bold cyan, inactive dim (like taskdaemon's `Chat|Plan`)
- Other tabs: `Dashboard · Works · Bundles · More` — active bold cyan, inactive dim, separated by ` · `
- `More` expands to remaining views: Ticks, Learnings, Locks, Agents (accessible via Tab)
- Right side: token counts + cost (shown only after first LLM call)

#### Content Area — Chat View

Bordered block with mode-specific title:

```rust
let title = match app.chat_mode {
    ChatMode::Chat => " Chat ",
    ChatMode::Plan => " Plan ",
};
let block = Block::default()
    .borders(Borders::ALL)
    .title(title)
    .border_style(Style::default().fg(colors::HEADER));
```

Inner area split: history fills remaining, input at bottom:

```rust
let chunks = Layout::default()
    .direction(Direction::Vertical)
    .constraints([
        Constraint::Min(0),                // History (scrollable)
        Constraint::Length(input_height),   // Input (dynamic, 1-10 lines)
    ])
    .split(inner);
```

#### History Rendering

`Paragraph` with `Wrap { trim: false }` and scroll offset:

- **User messages:** `"> "` prefix, green (`Color::Rgb(0, 255, 127)`), bold
- **Assistant messages:** `"  "` prefix, rendered line-by-line (future: markdown via `tui_markdown`)
- **System messages:** `"  "` prefix, dim gray
- **Streaming:** in-progress assistant text from `chat_response_buffer` + status line:
  `* Thinking... (ctrl+c to interrupt · 12s · ↑1.2K ↓450)`
- **Welcome message** (when history empty): bold cyan title + dim description
- Blank line after each message for readability
- Auto-scroll to bottom unless manual scroll set

#### Input Rendering (stolen from taskdaemon `render_repl_input`)

```rust
// Split input at cursor position
let (before_cursor, after_cursor) = input.split_at(cursor_pos);
// "> " prefix (green bold) + before + cursor + after
// Cursor at end: blinking underscore "_"
// Cursor in middle: inverted char (black on white, blinking)
// During streaming: dimmed, no cursor
```

#### Footer (stolen from taskdaemon `render_footer`)

Bordered block with left-aligned view keybindings and right-aligned global keybindings:

```
Left:  [Enter] Send  /plan Plan  /clear Clear
Right: [Tab] Views  [?] Help  [Ctrl+c] Quit
```

- Keys rendered in cyan bold, descriptions in normal white
- Context-sensitive: changes based on current view (Chat vs Works vs etc.)
- In scroll mode: `[Esc] Back to input  [j/k] Scroll  [G] Bottom`

#### Color Palette (stolen from taskdaemon)

```rust
mod colors {
    pub const HEADER: Color = Color::Rgb(0, 255, 255);       // Cyan
    pub const KEYBIND: Color = Color::Rgb(0, 255, 255);      // Cyan
    pub const DIM: Color = Color::DarkGray;
    pub const REPL_USER: Color = Color::Rgb(0, 255, 127);    // Spring green
    pub const REPL_ASSISTANT: Color = Color::Rgb(100, 149, 237); // Cornflower blue
    pub const REPL_ERROR: Color = Color::Rgb(220, 20, 60);   // Crimson
    pub const RUNNING: Color = Color::Rgb(0, 255, 127);      // Spring green
    pub const COMPLETE: Color = Color::Rgb(50, 205, 50);     // Lime green
    pub const FAILED: Color = Color::Rgb(220, 20, 60);       // Crimson
}
```

### Implementation Plan

#### Phase 1: 3-Zone Layout Refactor + Chat View Shell

Refactor the entire TUI rendering from the current nested-box layout to taskdaemon's clean 3-zone layout. This is the biggest phase — it replaces the `draw()` function in `run.rs` and adds the Chat view.

**Files:**
- `src/tui/app.rs` — Add `Chat` to `View` enum (first in `ALL`), add `ChatMode`, `ChatMessage`, `ChatRole`, add `ChatInput`/`ChatScroll` to `InputMode`, add all chat fields, add `colors` module
- `src/tui/run.rs` — Replace `draw()` with 3-zone layout: new `render_header()` (taskdaemon-style tab bar with `● Loopr │ Chat|Plan · Dashboard · Works · Bundles · More`), delegate content to view renderers, new `render_footer()` (context-sensitive keybinds, left=view actions, right=global). Remove current tab bar, role display, and action bar code.
- `src/tui/views/mod.rs` — Add `pub mod chat;`
- `src/tui/views/chat.rs` — New file: `render()` with bordered block, welcome message, history area + input area split. Steal rendering from taskdaemon's `render_repl_view`, `render_repl_history`, `render_repl_input`.
- `src/tui/input.rs` — Add `ChatInput` mode (always-input: all printable → insert, Enter submits, Esc → scroll mode, Tab → next view, PgUp/PgDn scroll), `ChatScroll` mode (j/k/g/G/PgUp/PgDn, any printable → back to ChatInput), UTF-8 cursor helpers

**Deliverable:** Loopr TUI matches taskdaemon screenshot — clean header/content/footer, Chat as default view with `> _` input, welcome message, existing views still accessible via Tab.

#### Phase 2: TUI-Side LLM Client (Chat Mode)

**Files:**
- `src/tui/run.rs` — Create `AgentLlmClient` at startup with local broadcast channel. On `pending_chat_submit`: spawn tokio task calling `call_with_history()`. Add `llm_event_rx` branch to `tokio::select!` for streaming chunks. Finalize `chat_response_buffer` → `ChatMessage::Assistant` on completion.
- `src/tui/app.rs` — Add `chat_response_buffer` finalization helpers

**Deliverable:** Full free chat — user types, LLM responds with streaming, multi-turn conversation works.

#### Phase 3: `/plan` Command & Coordinator Integration

**Files:**
- `src/tui/input.rs` — Detect `/plan` and `/chat` slash commands, route to app state
- `src/tui/run.rs` — On `/plan`: serialize `chat_history` to transcript, call `coordinator.set_goal` + `agent.start` via IPC, switch `chat_mode`. Handle `coordinator.interview_question` events → `ChatMessage::Assistant`. Route user messages to `coordinator.interview_respond`.
- `src/tui/app.rs` — Add `SetGoalAndStart(String)`, `InterviewRespond(String)` to `IpcAction`

**Deliverable:** `/plan` enters Plan mode, coordinator asks questions, user responds, conversation flows.

#### Phase 4: Plan Display & Approval

**Files:**
- `src/tui/run.rs` — Handle plan proposal events from coordinator. Show plan as formatted system message. Handle `Ctrl+a` → `coordinator.approve_plan` IPC.
- `src/tui/app.rs` — Add `ApprovePlan(String)` to `IpcAction`, `pending_plan_id` tracking

**Deliverable:** Plan shown to user, `Ctrl+a` approves, Draft → Active, automation starts.

#### Phase 5: Streaming Polish & Scroll

**Files:**
- `src/tui/views/chat.rs` — Spinner animation, auto-scroll, scroll mode rendering
- `src/tui/run.rs` — Frame counter for spinner tick

**Deliverable:** Smooth streaming, proper scroll behavior, polished UX.

## Alternatives Considered

### Alternative 1: Daemon-Side Chat LLM

- **Description:** Route free chat through daemon via new IPC method.
- **Pros:** Consistent architecture, daemon can persist chat, event streaming reuses existing infra.
- **Cons:** Requires new daemon handlers, daemon shouldn't own freeform chat state.
- **Why not chosen:** Free chat is a TUI-local concern. Only Plan mode needs daemon.

### Alternative 2: No Free Chat — Direct to Coordinator

- **Description:** Skip free chat, go straight to coordinator interview.
- **Pros:** Simpler, less code.
- **Cons:** Cold start — no context building. User can't explore before committing.
- **Why not chosen:** The whole point is to build rich context before planning.

## Technical Considerations

### Dependencies

- No new crates. Uses existing `ratatui`, `crossterm`, `tokio`, `reqwest` (via `AgentLlmClient`).
- `AgentLlmClient` is created TUI-side with a local `broadcast::channel`. Requires `ANTHROPIC_API_KEY` env var.

### Performance

- LLM calls run in background tokio task — non-blocking to UI
- Streaming chunks processed via `broadcast::Receiver::recv()` in select! — low latency
- Chat history is `Vec<ChatMessage>` — O(n) scroll calc is fine for <1000 messages

### Testing Strategy

- **Unit tests:** `ChatMessage` construction, cursor helpers, input key handling, slash command parsing, chat mode transitions
- **Render tests:** `render()` with TestBackend — empty, with messages, streaming, scroll mode, small terminal
- **LLM integration:** Mock `LlmClient` trait impl for testing chat flow without API calls

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `ANTHROPIC_API_KEY` not set | Medium | High | Check at TUI startup, show clear error in chat: "Set ANTHROPIC_API_KEY to enable chat" |
| Chat transcript too long for coordinator goal | Medium | Medium | Summarize transcript (take last N messages or ask LLM to summarize) before passing to coordinator |
| Coordinator interview questions are terse/unclear | Low | Medium | System prompt instructs coordinator to be conversational, reference chat context |
| LLM task panics in background | Low | High | Catch with JoinHandle, show error as system message, allow retry |

## Open Questions

- [x] TUI-side vs daemon-side LLM for free chat? → TUI-side with local broadcast channel
- [x] Bare hotkeys in Chat view? → No. Always-input mode. Scroll mode via Esc.
- [ ] How to summarize chat transcript for coordinator goal? → Recommend: pass last 20 messages as-is, or LLM-summarize if >20
- [ ] Rule of Five integration in coordinator? → Coordinator's Planning state runs 5 LLM iterations to refine plan draft. This is a coordinator agent change, not a TUI change.

## References

- Taskdaemon TUI REPL: `~/repos/taskdaemon/taskdaemon/td/src/tui/{state,views,app,runner}.rs`
- Neuraphage REPL: `~/repos/neuraphage/neuraphage/src/repl/{mod,display}.rs`
- Loopr AgentLlmClient: `src/agents/llm_client.rs`
- Loopr Coordinator interview: `src/agents/executor.rs:1069-1138`, `src/daemon/handlers.rs:3988-4151`
- Loopr CoordinatorState FSM: `src/domain/coordinator_state.rs`
- Jeffrey Emanuel Rule of Five: `~/repos/scottidler/obsidian/🤖 Tech/research/2026-01-20/jeffrey-emanuel-rule-of-five-agentic-llm.md`
- Plan statuses: `src/domain/plan.rs` — HierarchyStatus: Draft → Active → Complete | Abandoned
