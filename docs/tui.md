# TUI Design: Terminal User Interface

**Author:** Scott A. Idler
**Date:** 2026-01-25
**Status:** Implementation Spec
**Parent Doc:** [loop-architecture.md](loop-architecture.md)

---

## Summary

Loopr provides a k9s-style terminal interface with two views: **Chat** and **Loops**. The `loopr` binary runs everything in a single process—no daemon, no fork. Tokio manages async operations (LLM streaming, loop execution). Users interact via Chat to describe tasks, use `/plan` to create plans, and monitor execution in the Loops tree view.

---

## Architecture: Direct Execution (No Daemon)

Loopr runs as a single process. The `loopr` binary launches the TUI directly and manages loops in-process using tokio async tasks.

```
loopr (binary)
├── TuiRunner              # Main loop: render + events + tick
│   ├── App                # Keyboard handling, state transitions
│   ├── EventHandler       # Keyboard + tick events
│   └── views::render()    # ratatui rendering
├── LoopManager            # Spawns/monitors loops (tokio tasks)
│   ├── Scheduler          # Prioritizes pending loops
│   └── running_loops[]    # Active tokio::spawn handles
├── TaskStore              # Persistence (JSONL + SQLite)
└── LlmClient              # API calls to Claude (streaming)
```

**Why no daemon?**
- Simpler deployment (single binary)
- No IPC complexity (everything in-process)
- Direct tokio task management
- State persists in TaskStore for resume on restart

**Startup:**
1. `loopr` parses args, initializes TaskStore
2. Creates LoopManager and LlmClient
3. Initializes terminal (raw mode, alternate screen)
4. Runs TuiRunner main loop

**Shutdown:**
1. User presses `q` (or Ctrl+C)
2. LoopManager gracefully stops running loops
3. Terminal restored to normal mode
4. Process exits

**Crash recovery:** On restart, `loopr` reads TaskStore, finds loops with `status=running`, marks them as `interrupted`, and allows user to resume or cancel.

---

## Two Views

### View 1: Chat

The primary interaction view. Users describe tasks, the LLM responds, and plans are created.

```
┌─────────────────────────────────────────────────────────────────────┐
│ ● Loopr │ Chat · Loops                        ↑1.2K ↓0.3K │ $0.15  │
├─────────────────────────────────────────────────────────────────────┤
│ ─ Chat ─────────────────────────────────────────────────────────────│
│ Welcome to Loopr Chat                                               │
│                                                                     │
│ Type a message and press Enter to chat with the AI assistant.       │
│                                                                     │
│ > Build a REST API for user management                              │
│                                                                     │
│   I'll help you build a REST API. Let me create a plan...           │
│                                                                     │
│ ● read_file(src/main.rs)                                            │
│ └ Found 45 lines                                                    │
│                                                                     │
│                                                                     │
│                                                                     │
│                                                                     │
│                                                                     │
│ > _                                                                 │
├─────────────────────────────────────────────────────────────────────┤
│ [Enter] Send /clear Clear                   [Tab] Views [?] Help [q]│
└─────────────────────────────────────────────────────────────────────┘
```

**Chat Features:**
- Message history with scroll (j/k or arrows)
- Streaming LLM responses with progress indicator
- Tool call display (collapsible with Ctrl+o)
- `/clear` to reset conversation
- `/plan <description>` to create a plan (triggers Rule of Five)

### View 2: Loops

Hierarchical tree view of all loops (Plan → Spec → Phase → Ralph).

```
┌─────────────────────────────────────────────────────────────────────┐
│ ● Loopr │ Chat · Loops                   2 active │ 1 draft │ 3 done│
├─────────────────────────────────────────────────────────────────────┤
│ ─ Loops (6) ────────────────────────────────────────────────────────│
│ ▼ ● Plan: Build REST API [1/5]              → plan.md ✓             │
│   ├─▼ ● Spec: User endpoints [2/3]          → spec.md ✓             │
│   │   ├── ● Phase: Create models (iter 3/10)                        │
│   │   ├── ○ Phase: Add validation                                   │
│   │   └── ○ Phase: Write tests                                      │
│   └── ○ Spec: Auth endpoints                                        │
│ ◌ Plan: Add logging [draft]                                         │
│                                                                     │
│                                                                     │
│                                                                     │
│                                                                     │
│                                                                     │
│                                                                     │
├─────────────────────────────────────────────────────────────────────┤
│ [Enter] Describe [s] State [o] Output [L] Logs [x] Cancel   [Tab] [?]│
└─────────────────────────────────────────────────────────────────────┘
```

**Loops Features:**
- Tree navigation with expand/collapse (arrows or h/l)
- Status icons: ● running, ○ pending, ◌ draft, ✓ complete, ✗ failed
- Progress indicators: `[2/3]` for child count, `(iter 3/10)` for iterations
- Artifact status shown inline
- Actions: describe, view output, view logs, cancel, toggle state

---

## Layout Structure

### Header (3 lines height)

```rust
Layout::default()
    .direction(Direction::Vertical)
    .constraints([
        Constraint::Length(3),  // Header
        Constraint::Min(0),     // Content
        Constraint::Length(3),  // Footer
    ])
```

**Header content:**
- Left: Status indicator (●/○) + "Loopr" + view tabs
- Right: Metrics (tokens, cost, loop counts)

```
│ ● Loopr │ Chat · Loops                   ↑1.2K ↓0.3K │ $0.15 │ 2 active │
```

### Footer (3 lines height)

**Footer content:**
- Left: Context-sensitive keybindings
- Right: Global actions (Tab, ?, q)

```
│ [Enter] Send /clear Clear                   [Tab] Views [?] Help [q] Quit │
```

### Status Colors (k9s-inspired)

```rust
mod colors {
    pub const RUNNING: Color = Color::Rgb(0, 255, 127);   // Spring green
    pub const PENDING: Color = Color::Rgb(255, 215, 0);   // Gold
    pub const COMPLETE: Color = Color::Rgb(50, 205, 50);  // Lime green
    pub const FAILED: Color = Color::Rgb(220, 20, 60);    // Crimson
    pub const DRAFT: Color = Color::Rgb(255, 255, 0);     // Yellow
    pub const HEADER: Color = Color::Rgb(0, 255, 255);    // Cyan
    pub const KEYBIND: Color = Color::Rgb(0, 255, 255);   // Cyan
    pub const DIM: Color = Color::DarkGray;
}
```

---

## Keyboard Navigation

### Global Keys

| Key | Action |
|-----|--------|
| `Tab` | Cycle views (Chat → Loops → Chat) |
| `?` | Toggle help overlay |
| `q` | Quit (confirm if loops running) |
| `Ctrl+C` | Force quit |
| `Esc` | Back / clear filter / cancel |

### Chat View Keys

| Key | Action |
|-----|--------|
| `Enter` | Send message |
| `j/k` or `↑/↓` | Scroll history |
| `g/G` | Top/bottom of history |
| `Ctrl+o` | Toggle tool output expand/collapse |
| `/clear` | Clear conversation |
| `/plan <desc>` | Create a plan from description |

### Loops View Keys

| Key | Action |
|-----|--------|
| `j/k` or `↑/↓` | Navigate tree |
| `h/l` or `←/→` | Collapse/expand node |
| `Enter` | Describe selected loop |
| `s` | Toggle state (draft→running, running→paused, paused→running) |
| `o` | View output/progress |
| `L` | View logs |
| `x` | Cancel loop |
| `D` | Delete loop |
| `g/G` | Top/bottom of tree |

---

## State Management

### AppState

```rust
pub struct AppState {
    // View state
    pub current_view: View,
    pub interaction_mode: InteractionMode,

    // Chat state
    pub chat_history: Vec<ChatMessage>,
    pub chat_input: String,
    pub chat_streaming: bool,

    // Loops state
    pub loops_tree: LoopTree,
    pub loops_scroll: usize,

    // Metrics (from TaskStore polling)
    pub loops_active: usize,
    pub loops_draft: usize,
    pub loops_complete: usize,
    pub session_input_tokens: u64,
    pub session_output_tokens: u64,
    pub session_cost_usd: f64,

    // Pending actions (processed by runner)
    pub pending_chat_submit: Option<String>,
    pub pending_action: Option<PendingAction>,
}

pub enum View {
    Chat,   // Conversation with LLM
    Loops,  // Hierarchical tree of running loops
}

pub enum InteractionMode {
    Normal,
    ChatInput,
    Help,
    Confirm(ConfirmDialog),
}

pub enum PendingAction {
    CancelLoop(String),
    PauseLoop(String),
    ResumeLoop(String),
    ActivateDraft(String),
    DeleteLoop(String),
}
```

### LoopTree

Hierarchical representation of loops for tree view:

```rust
pub struct LoopTree {
    nodes: HashMap<String, TreeNode>,
    root_ids: Vec<String>,
    visible_ids: Vec<String>,  // Flattened for rendering
    selected_id: Option<String>,
}

pub struct TreeNode {
    pub item: LoopItem,
    pub depth: usize,
    pub expanded: bool,
    pub children: Vec<String>,
}

pub struct LoopItem {
    pub id: String,
    pub name: String,
    pub loop_type: String,      // plan, spec, phase, ralph
    pub status: String,
    pub iteration: String,      // "3/10"
    pub parent_id: Option<String>,
    pub artifact_file: Option<String>,
    pub artifact_status: Option<String>,
}
```

---

## Data Flow

### TUI Runner Main Loop

The TuiRunner owns both the TUI and the LoopManager. Everything runs in-process using tokio.

```rust
pub struct TuiRunner {
    terminal: Tui,
    app: App,
    event_handler: EventHandler,
    store: TaskStore,
    loop_manager: LoopManager,       // Manages loop execution (tokio tasks)
    llm_client: Arc<dyn LlmClient>,
}

impl TuiRunner {
    pub async fn run(&mut self) -> Result<()> {
        loop {
            // 1. Render current state
            self.terminal.draw(|f| views::render(self.app.state_mut(), f))?;

            // 2. Handle events (keyboard, tick)
            if let Some(event) = self.event_handler.next().await? {
                match event {
                    Event::Key(key) => {
                        if self.app.handle_key(key) {
                            break; // Quit requested
                        }
                    }
                    Event::Tick => {
                        // Poll TaskStore and update TUI state
                        self.refresh_state().await?;
                        // Let LoopManager check for runnable loops
                        self.loop_manager.tick().await?;
                    }
                }
            }

            // 3. Process pending actions (user commands)
            self.process_pending_actions().await?;

            // 4. Check for quit
            if self.app.state().should_quit {
                break;
            }
        }

        // Graceful shutdown - wait for running loops to complete or cancel
        self.loop_manager.shutdown().await?;
        Ok(())
    }

    async fn refresh_state(&mut self) -> Result<()> {
        // Poll TaskStore for loop updates
        let loops = self.store.query::<LoopRecord>(&[])?;
        self.app.state_mut().loops_tree.build_from_records(loops);

        // Update metrics
        self.app.state_mut().loops_active =
            self.store.count_by_status("running")?;
        self.app.state_mut().loops_draft =
            self.store.count_by_status("draft")?;
        self.app.state_mut().loops_complete =
            self.store.count_by_status("complete")?;

        Ok(())
    }
}
```

### Chat Submission Flow

```
User types message → Enter
    ↓
pending_chat_submit = Some(message)
    ↓
TuiRunner.process_pending_actions()
    ↓
Add user message to history
Start streaming LLM response
    ↓
Stream chunks update chat_response_buffer
Tool calls execute, results display
    ↓
On complete: add assistant message to history
Clear response buffer
```

### Plan Creation Flow

```
User types "/plan Build a REST API for users"
    ↓
TuiRunner detects /plan command
    ↓
TuiRunner starts Rule of Five loop with description
    ↓
Plan draft created, review passes 1-5
    ↓
On pass 5 complete:
    - Create LoopRecord (type=plan, status=draft)
    - Show in Loops tree
    ↓
User switches to Loops view (Tab)
User presses 's' to activate → status=pending
    ↓
LoopManager picks up, creates worktree, runs
```

---

## File Structure

```
loopr/src/tui/
├── mod.rs              # Public exports, init/restore
├── app.rs              # App struct, keyboard handling
├── state.rs            # AppState, View, InteractionMode
├── views.rs            # All rendering (render_chat, render_loops)
├── tree.rs             # LoopTree for hierarchical display
├── runner.rs           # TuiRunner, main loop, action processing
└── events.rs           # Event enum, EventHandler (keyboard + tick)
```

---

## Configuration

From [loop-config.md](loop-config.md):

```yaml
tui:
  tick_rate_ms: 250      # Refresh rate for state polling
  scroll_page_size: 10   # Lines per PageUp/PageDown

  # Colors (optional overrides)
  colors:
    running: "#00FF7F"
    pending: "#FFD700"
    complete: "#32CD32"
    failed: "#DC143C"
```

---

## Testing

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_view_cycle() {
        let mut app = App::new();
        assert!(matches!(app.state().current_view, View::Chat));

        app.handle_key(KeyEvent::from(KeyCode::Tab));
        assert!(matches!(app.state().current_view, View::Loops));

        app.handle_key(KeyEvent::from(KeyCode::Tab));
        assert!(matches!(app.state().current_view, View::Chat));
    }

    #[test]
    fn test_loops_tree_navigation() {
        let mut app = App::new();
        app.state_mut().current_view = View::Loops;

        let items = vec![
            LoopItem { id: "1".into(), name: "Plan A".into(), .. },
            LoopItem { id: "2".into(), name: "Spec A1".into(), parent_id: Some("1".into()), .. },
        ];
        app.state_mut().loops_tree.build_from_items(items);

        app.handle_key(KeyEvent::from(KeyCode::Char('j')));
        assert_eq!(app.state().loops_tree.selected_id(), Some(&"2".to_string()));
    }

    #[test]
    fn test_plan_command() {
        let mut app = App::new();
        app.state_mut().chat_input = "/plan Build auth system".to_string();

        // Submit triggers plan creation
        app.handle_key(KeyEvent::from(KeyCode::Enter));

        // Plan creation is queued
        assert!(app.state().pending_plan_create.is_some());
    }
}
```

---

## References

- [loop-architecture.md](loop-architecture.md) - Loop hierarchy
- [loop-coordination.md](loop-coordination.md) - State polling (no IPC)
- [domain-types.md](domain-types.md) - LoopRecord schema
- [ratatui docs](https://docs.rs/ratatui) - TUI framework
- [k9s](https://k9scli.io/) - UI/UX inspiration
