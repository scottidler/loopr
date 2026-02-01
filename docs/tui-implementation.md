# TUI Implementation Details

**Author:** Scott A. Idler
**Date:** 2026-01-31
**Status:** Implementation Spec
**Source:** Extracted from taskdaemon/td/src/tui/

---

## Summary

This document captures concrete implementation patterns for the Loopr TUI, extracted from the working taskdaemon implementation. These patterns have been proven in practice and should be reused in the Loopr v2 rewrite.

---

## 1. Input Buffer & Cursor Management

### Cursor Position Tracking

**Critical:** Use **byte offsets**, not character indices, for UTF-8 correctness.

```rust
pub struct TuiState {
    pub chat_input: String,
    pub chat_cursor_pos: usize,  // BYTE OFFSET, not char index!
}
```

### UTF-8 Safe Cursor Movement

```rust
impl App {
    fn prev_char_boundary(&self, pos: usize) -> usize {
        let input = &self.state.chat_input;
        let mut new_pos = pos.saturating_sub(1);
        while new_pos > 0 && !input.is_char_boundary(new_pos) {
            new_pos -= 1;
        }
        new_pos
    }

    fn next_char_boundary(&self, pos: usize) -> usize {
        let input = &self.state.chat_input;
        let mut new_pos = pos + 1;
        while new_pos < input.len() && !input.is_char_boundary(new_pos) {
            new_pos += 1;
        }
        new_pos.min(input.len())
    }
}
```

### Arrow Key Handling

```rust
KeyCode::Left => {
    if self.state.chat_cursor_pos > 0 {
        self.state.chat_cursor_pos = self.prev_char_boundary(self.state.chat_cursor_pos);
    }
}
KeyCode::Right => {
    if self.state.chat_cursor_pos < self.state.chat_input.len() {
        self.state.chat_cursor_pos = self.next_char_boundary(self.state.chat_cursor_pos);
    }
}
KeyCode::Home => {
    self.state.chat_cursor_pos = 0;
}
KeyCode::End => {
    self.state.chat_cursor_pos = self.state.chat_input.len();
}
```

### Character Insertion & Deletion

```rust
KeyCode::Backspace => {
    if self.state.chat_cursor_pos > 0 {
        let new_pos = self.prev_char_boundary(self.state.chat_cursor_pos);
        self.state.chat_input.drain(new_pos..self.state.chat_cursor_pos);
        self.state.chat_cursor_pos = new_pos;
    }
    // Auto-exit input mode if empty
    if self.state.chat_input.is_empty() {
        self.state.interaction_mode = InteractionMode::Normal;
    }
}
KeyCode::Delete => {
    if self.state.chat_cursor_pos < self.state.chat_input.len() {
        let end_pos = self.next_char_boundary(self.state.chat_cursor_pos);
        self.state.chat_input.drain(self.state.chat_cursor_pos..end_pos);
    }
}
KeyCode::Char(c) => {
    self.state.chat_input.insert(self.state.chat_cursor_pos, c);
    self.state.chat_cursor_pos += c.len_utf8();  // UTF-8 aware!
}
```

---

## 2. Cursor Rendering

### Visual Cursor Display

```rust
fn render_chat_input(state: &TuiState, frame: &mut Frame, area: Rect) {
    let cursor_pos = state.chat_cursor_pos.min(state.chat_input.len());
    let (before_cursor, after_cursor) = state.chat_input.split_at(cursor_pos);

    let mut spans = vec![Span::styled(
        "> ",
        Style::default().fg(colors::CHAT_USER).add_modifier(Modifier::BOLD),
    )];

    let input_style = Style::default().fg(Color::White);

    // Text before cursor
    if !before_cursor.is_empty() {
        spans.push(Span::styled(before_cursor, input_style));
    }

    // Cursor rendering (2 cases)
    if !state.chat_streaming {
        if after_cursor.is_empty() {
            // Cursor at end: blinking underscore
            spans.push(Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK)));
        } else {
            // Cursor in middle: inverted character with blink
            let mut chars = after_cursor.chars();
            if let Some(c) = chars.next() {
                spans.push(Span::styled(
                    c.to_string(),
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::White)
                        .add_modifier(Modifier::SLOW_BLINK),
                ));
                let remaining: String = chars.collect();
                if !remaining.is_empty() {
                    spans.push(Span::styled(remaining, input_style));
                }
            }
        }
    } else {
        // Streaming: no cursor, just show rest
        if !after_cursor.is_empty() {
            spans.push(Span::styled(after_cursor, input_style));
        }
    }

    let input_content = Line::from(spans);
    let input = Paragraph::new(input_content).wrap(Wrap { trim: false });
    frame.render_widget(input, area);
}
```

---

## 3. Layout Structure

### Three-Level Layout (All Views)

```rust
// Header (3 lines) | Content (variable) | Footer (3 lines)
let chunks = Layout::default()
    .direction(Direction::Vertical)
    .constraints([
        Constraint::Length(3),  // Header: status, tabs, metrics
        Constraint::Min(0),     // Main content area
        Constraint::Length(3),  // Footer: keybinds or input mode
    ])
    .split(frame.area());

render_header(state, frame, chunks[0]);
// ... render view specific content ...
render_footer(state, frame, chunks[2]);
```

### Chat View Layout (Dynamic Input Height)

```rust
fn render_chat_view(state: &mut TuiState, frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Chat ")
        .border_style(Style::default().fg(colors::HEADER));

    let inner = block.inner(area);

    // Dynamic input height based on wrapped text
    let input_height = calculate_input_height(&state.chat_input, inner.width);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),               // History
            Constraint::Length(input_height), // Input (dynamic)
        ])
        .split(inner);

    frame.render_widget(block, area);
    render_chat_history(state, frame, chunks[0]);
    render_chat_input(state, frame, chunks[1]);
}

fn calculate_input_height(input: &str, width: u16) -> u16 {
    if input.is_empty() {
        return 1; // Minimum 1 line
    }

    // Account for "> " prefix (2 chars) and cursor "_" (1 char)
    let effective_width = width.saturating_sub(3) as usize;
    if effective_width == 0 {
        return 1;
    }

    // Count lines needed for wrapped content
    let lines = input.len().div_ceil(effective_width);
    lines.clamp(1, 10) as u16 // Cap at 10 lines max
}
```

### Header Layout

```
┌─────────────────────────────────────────────────────────────────────┐
│ ● Loopr │ Chat · Loops                        ↑1.2K ↓0.3K │ $0.15  │
└─────────────────────────────────────────────────────────────────────┘
  ^         ^                                    ^
  Status    Tabs (current highlighted)           Metrics
```

---

## 4. Color Scheme

### Status Colors (k9s-inspired)

```rust
pub mod colors {
    use ratatui::style::Color;

    // Status indicators
    pub const RUNNING: Color = Color::Rgb(0, 255, 127);      // Spring green
    pub const PENDING: Color = Color::Rgb(255, 215, 0);      // Gold
    pub const COMPLETE: Color = Color::Rgb(50, 205, 50);     // Lime green
    pub const FAILED: Color = Color::Rgb(220, 20, 60);       // Crimson
    pub const DRAFT: Color = Color::Rgb(255, 255, 0);        // Yellow
    pub const PAUSED: Color = Color::Rgb(255, 165, 0);       // Orange
    pub const BLOCKED: Color = Color::Rgb(255, 69, 0);       // Red-orange

    // UI elements
    pub const HEADER: Color = Color::Rgb(100, 149, 237);     // Cornflower blue
    pub const CHAT_USER: Color = Color::Rgb(135, 206, 250);  // Light sky blue
    pub const CHAT_ASSISTANT: Color = Color::Rgb(144, 238, 144); // Light green
    pub const TOOL_CALL: Color = Color::Rgb(255, 182, 193);  // Light pink
}
```

### Status Icons

```rust
pub mod icons {
    pub const RUNNING: &str = "●";    // Filled circle
    pub const PENDING: &str = "○";    // Empty circle
    pub const COMPLETE: &str = "✓";   // Check mark
    pub const FAILED: &str = "✗";     // X mark
    pub const DRAFT: &str = "◌";      // Dotted circle
    pub const PAUSED: &str = "⊘";     // Circle with slash
}
```

---

## 5. Interaction Model (Claude Code Style)

### No Vim Modes

Unlike vim-style TUIs, Loopr uses a **Claude Code-style interaction model**:
- Chat view is **always ready for input** - no mode switching
- All printable characters go directly to the input buffer
- Commands use `/` prefix (e.g., `/clear`, `/plan`)
- Navigation uses modifier keys or dedicated keys that don't conflict with typing
- Quit requires **double-tap** safety pattern

### Quit Pattern (Double-Tap Safety)

```rust
pub struct QuitDetector {
    last_ctrl_c: Option<Instant>,
    last_ctrl_d: Option<Instant>,
    threshold: Duration,  // 500ms default
}

impl QuitDetector {
    pub fn new() -> Self {
        Self {
            last_ctrl_c: None,
            last_ctrl_d: None,
            threshold: Duration::from_millis(500),
        }
    }

    pub fn check_ctrl_c(&mut self) -> bool {
        let now = Instant::now();
        if let Some(last) = self.last_ctrl_c {
            if now.duration_since(last) < self.threshold {
                return true;  // Quit!
            }
        }
        self.last_ctrl_c = Some(now);
        false  // First press, show hint
    }

    pub fn check_ctrl_d(&mut self) -> bool {
        let now = Instant::now();
        if let Some(last) = self.last_ctrl_d {
            if now.duration_since(last) < self.threshold {
                return true;  // Quit!
            }
        }
        self.last_ctrl_d = Some(now);
        false  // First press, show hint
    }
}
```

### Key Handling (View-Based, Not Mode-Based)

```rust
fn handle_key(&mut self, key: KeyEvent) -> bool {
    // Global keys first (work in any view)
    match (key.code, key.modifiers) {
        // Quit: double-tap Ctrl+C or Ctrl+D
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
            if self.quit_detector.check_ctrl_c() {
                return true;  // Quit
            }
            self.show_hint("Press Ctrl+C again to quit");
            return false;
        }
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
            if self.quit_detector.check_ctrl_d() {
                return true;  // Quit
            }
            self.show_hint("Press Ctrl+D again to quit");
            return false;
        }

        // View switching
        (KeyCode::Tab, KeyModifiers::NONE) => {
            self.state.cycle_view();
            return false;
        }
        (KeyCode::BackTab, KeyModifiers::SHIFT) => {
            self.state.cycle_view_reverse();
            return false;
        }

        // Help overlay
        (KeyCode::F(1), _) => {
            self.state.toggle_help();
            return false;
        }

        _ => {}
    }

    // View-specific handling
    match &self.state.current_view {
        View::Chat => self.handle_chat_key(key),
        View::Loops => self.handle_loops_key(key),
    }
}
```

### Chat View Key Handling

```rust
fn handle_chat_key(&mut self, key: KeyEvent) -> bool {
    match (key.code, key.modifiers) {
        // Send message
        (KeyCode::Enter, KeyModifiers::NONE) => {
            if !self.state.chat_input.is_empty() {
                self.send_message();
            }
        }

        // Clear input
        (KeyCode::Esc, _) => {
            self.state.chat_input.clear();
            self.state.chat_cursor_pos = 0;
        }

        // Cursor movement (no modifier needed - these don't produce chars)
        (KeyCode::Left, KeyModifiers::NONE) => {
            self.cursor_left();
        }
        (KeyCode::Right, KeyModifiers::NONE) => {
            self.cursor_right();
        }
        (KeyCode::Home, _) => {
            self.state.chat_cursor_pos = 0;
        }
        (KeyCode::End, _) => {
            self.state.chat_cursor_pos = self.state.chat_input.len();
        }

        // History scroll (with modifier to not conflict with cursor)
        (KeyCode::Up, KeyModifiers::ALT) | (KeyCode::PageUp, _) => {
            self.state.chat_scroll_up(10);
        }
        (KeyCode::Down, KeyModifiers::ALT) | (KeyCode::PageDown, _) => {
            self.state.chat_scroll_down(10);
        }

        // Delete
        (KeyCode::Backspace, _) => {
            self.backspace();
        }
        (KeyCode::Delete, _) => {
            self.delete();
        }

        // Any printable character -> input buffer
        (KeyCode::Char(c), modifiers) if !modifiers.contains(KeyModifiers::CONTROL) => {
            self.state.chat_input.insert(self.state.chat_cursor_pos, c);
            self.state.chat_cursor_pos += c.len_utf8();
        }

        _ => {}
    }
    false
}
```

### Loops View Key Handling

```rust
fn handle_loops_key(&mut self, key: KeyEvent) -> bool {
    match (key.code, key.modifiers) {
        // Navigation (no conflict - Loops view has no text input)
        (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
            self.state.loops_tree.select_prev();
        }
        (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
            self.state.loops_tree.select_next();
        }
        (KeyCode::Left, _) | (KeyCode::Char('h'), KeyModifiers::NONE) => {
            self.state.loops_tree.collapse_or_parent();
        }
        (KeyCode::Right, _) | (KeyCode::Char('l'), KeyModifiers::NONE) => {
            self.state.loops_tree.expand();
        }

        // Actions
        (KeyCode::Enter, _) => {
            self.describe_selected_loop();
        }
        (KeyCode::Char('x'), KeyModifiers::NONE) => {
            self.cancel_selected_loop();
        }
        (KeyCode::Char('o'), KeyModifiers::NONE) => {
            self.view_loop_output();
        }

        _ => {}
    }
    false
}
```

---

## 6. Scroll Management

### Chat Scroll (Auto-scroll + Manual)

```rust
pub struct TuiState {
    pub chat_scroll: Option<usize>,  // None = auto-scroll to bottom
    pub chat_max_scroll: usize,      // Cached during render
}

impl TuiState {
    pub fn chat_scroll_up(&mut self, lines: usize) {
        let current = self.chat_scroll.unwrap_or(self.chat_max_scroll);
        let clamped = current.min(self.chat_max_scroll);
        self.chat_scroll = Some(clamped.saturating_sub(lines));
    }

    pub fn chat_scroll_down(&mut self, lines: usize) {
        let current = self.chat_scroll.unwrap_or(self.chat_max_scroll);
        let new_scroll = current.saturating_add(lines).min(self.chat_max_scroll);
        if new_scroll >= self.chat_max_scroll {
            self.chat_scroll = None;  // Back to auto-scroll
        } else {
            self.chat_scroll = Some(new_scroll);
        }
    }

    pub fn chat_scroll_to_bottom(&mut self) {
        self.chat_scroll = None;  // Auto-scroll mode
    }
}
```

### Scrollable Content Rendering

```rust
let scroll_offset = state.chat_scroll.unwrap_or(state.chat_max_scroll);

let paragraph = Paragraph::new(lines)
    .wrap(Wrap { trim: false })
    .scroll((scroll_offset as u16, 0));  // (vertical, horizontal)

frame.render_widget(paragraph, area);

// Update max_scroll for next frame
state.chat_max_scroll = calculate_max_scroll(total_lines, visible_height);
```

---

## 7. Tree View (Loops)

### Tree Data Structure

```rust
pub struct LoopTree {
    nodes: HashMap<String, TreeNode>,
    roots: Vec<String>,
    expand_state: HashMap<String, bool>,  // Persists across rebuilds
    selected_id: Option<String>,
    visible_nodes: Vec<String>,  // Flattened for rendering
}

pub struct TreeNode {
    pub item: LoopItem,
    pub depth: usize,
    pub children: Vec<String>,
    pub expanded: bool,
}
```

### Build Tree From Flat Items

```rust
impl LoopTree {
    pub fn build_from_items(&mut self, items: Vec<LoopItem>) {
        // Preserve expand state across rebuilds
        let prev_expand_state = std::mem::take(&mut self.expand_state);
        let prev_selected = self.selected_id.clone();

        self.nodes.clear();
        self.roots.clear();

        // Index children by parent
        let mut children_by_parent: HashMap<Option<String>, Vec<&LoopItem>> = HashMap::new();
        for item in &items {
            children_by_parent.entry(item.parent_id.clone()).or_default().push(item);
        }

        // Recursively build tree
        if let Some(root_items) = children_by_parent.get(&None) {
            for item in root_items {
                self.build_subtree(item, 0, &children_by_parent, &prev_expand_state);
                self.roots.push(item.id.clone());
            }
        }

        // Restore previous selection if valid
        self.selected_id = if let Some(ref id) = prev_selected {
            if self.nodes.contains_key(id) { prev_selected } else { None }
        } else {
            None
        };

        // Rebuild visible nodes list
        self.rebuild_visible_nodes();
    }

    fn rebuild_visible_nodes(&mut self) {
        self.visible_nodes.clear();
        for root_id in &self.roots.clone() {
            self.add_visible_nodes_recursive(root_id);
        }
    }

    fn add_visible_nodes_recursive(&mut self, id: &str) {
        self.visible_nodes.push(id.to_string());
        if let Some(node) = self.nodes.get(id) {
            if node.expanded {
                for child_id in node.children.clone() {
                    self.add_visible_nodes_recursive(&child_id);
                }
            }
        }
    }
}
```

### Tree Prefix Rendering (Unicode Box Drawing)

```rust
fn build_tree_prefix(tree: &LoopTree, id: &str, depth: usize) -> String {
    if depth == 0 {
        return String::new();
    }

    let node = tree.get(id).expect("node should exist");
    let mut ancestors: Vec<(String, bool)> = Vec::new();

    // Walk up to get ancestor chain
    let mut current_id = id.to_string();
    let mut current_node = node;

    while let Some(ref parent_id) = current_node.item.parent_id {
        let is_last = tree.is_last_child(&current_id);
        ancestors.push((current_id.clone(), is_last));

        if let Some(parent) = tree.get(parent_id) {
            current_id = parent_id.clone();
            current_node = parent;
        } else {
            break;
        }
    }

    ancestors.reverse();

    // Build prefix with box-drawing chars
    let mut prefix = String::new();
    for (i, (_ancestor_id, is_last)) in ancestors.iter().enumerate() {
        if i == ancestors.len() - 1 {
            // Connection for this node
            prefix.push_str(if *is_last { "└─" } else { "├─" });
        } else {
            // Ancestor's vertical line
            prefix.push_str(if *is_last { "  " } else { "│ " });
        }
    }

    prefix
}
```

### Tree Navigation

```rust
// Expand/collapse
KeyCode::Right | KeyCode::Char('l') => {
    if let Some(id) = &self.state.loops_tree.selected_id {
        self.state.loops_tree.expand(id);
    }
}
KeyCode::Left | KeyCode::Char('h') => {
    if let Some(id) = &self.state.loops_tree.selected_id {
        if !self.state.loops_tree.collapse(id) {
            // Already collapsed - select parent
            self.state.loops_tree.select_parent();
        }
    }
}

// Up/down navigation
KeyCode::Up | KeyCode::Char('k') => {
    self.state.loops_tree.select_prev();
}
KeyCode::Down | KeyCode::Char('j') => {
    self.state.loops_tree.select_next();
}
```

---

## 8. Live Output Streaming

### Output Buffer with Size Limit

```rust
pub struct LiveOutputBuffer {
    pub iteration: u32,
    pub content: String,
    pub max_size: usize,  // 100KB default
}

impl LiveOutputBuffer {
    pub fn append(&mut self, text: &str) {
        self.content.push_str(text);

        // Truncate from beginning if over max_size
        if self.content.len() > self.max_size {
            let excess = self.content.len() - self.max_size;
            let cut_at = self.content
                .char_indices()
                .find(|(i, _)| *i >= excess)
                .map(|(i, _)| i)
                .unwrap_or(excess);
            self.content = self.content[cut_at..].to_string();
        }
    }
}
```

### Stored per Loop

```rust
pub struct TuiState {
    // ... other fields ...
    pub live_output: HashMap<String, LiveOutputBuffer>,
}

impl TuiState {
    pub fn get_live_output(&self, loop_id: &str) -> Option<&LiveOutputBuffer> {
        self.live_output.get(loop_id)
    }
}
```

---

## 9. Overlay Rendering

### Help/Confirm Overlays

```rust
fn render_help_overlay(frame: &mut Frame, area: Rect) {
    let popup_area = centered_rect(60, 70, area);  // 60% wide, 70% tall
    frame.render_widget(Clear, popup_area);  // Clear background

    let help = Paragraph::new(help_text)
        .block(Block::default()
            .borders(Borders::ALL)
            .title(" Help (? to close) "));

    frame.render_widget(help, popup_area);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
```

---

## 10. Event Loop Pattern

### Main Loop Structure

```rust
pub struct TuiRunner {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    app: App,
    state: TuiState,
    daemon: DaemonConnection,
}

impl TuiRunner {
    pub async fn run(&mut self) -> Result<()> {
        loop {
            // 1. Render frame
            self.terminal.draw(|f| views::render(&self.state, f))?;

            // 2. Handle events with select
            tokio::select! {
                // Keyboard input (prioritized)
                key = self.app.next_key() => {
                    if let Some(key) = key {
                        if self.handle_key(key).await {
                            break; // Quit requested
                        }
                    }
                }

                // Daemon events
                event = self.daemon.event_rx.recv() => {
                    if let Some(event) = event {
                        self.handle_daemon_event(event).await?;
                    } else {
                        // Daemon disconnected
                        self.state.connected = false;
                    }
                }

                // Tick for animations, reconnect attempts
                _ = tokio::time::sleep(Duration::from_millis(250)) => {
                    self.tick().await?;
                }
            }
        }

        Ok(())
    }
}
```

---

## 11. Selection State Pattern

### For Flat Lists

```rust
pub struct SelectionState {
    pub selected_index: usize,
    pub scroll_offset: usize,
}

impl SelectionState {
    pub fn select_next(&mut self, max_items: usize) {
        if max_items > 0 && self.selected_index < max_items - 1 {
            self.selected_index += 1;
        }
    }

    pub fn select_prev(&mut self) {
        if self.selected_index > 0 {
            self.selected_index -= 1;
        }
    }

    pub fn ensure_visible(&mut self, visible_height: usize) {
        // Scroll to keep selection visible
        if self.selected_index < self.scroll_offset {
            self.scroll_offset = self.selected_index;
        } else if self.selected_index >= self.scroll_offset + visible_height {
            self.scroll_offset = self.selected_index - visible_height + 1;
        }
    }
}
```

---

## 12. Key Bindings (Claude Code Style)

### Global (Any View)

| Key | Action |
|-----|--------|
| `Ctrl+C` (x2) | Quit (double-tap within 500ms) |
| `Ctrl+D` (x2) | Quit (double-tap within 500ms) |
| `Tab` | Cycle views (Chat → Loops → Chat) |
| `Shift+Tab` | Cycle views reverse |
| `F1` | Toggle help overlay |

### Chat View

**Always in input mode** - all printable characters go to input buffer.

| Key | Action |
|-----|--------|
| `[any char]` | Insert at cursor |
| `Enter` | Send message (or execute `/command`) |
| `Esc` | Clear input |
| `Left` / `Right` | Move cursor |
| `Home` / `End` | Start/end of line |
| `Backspace` | Delete char before cursor |
| `Delete` | Delete char at cursor |
| `Alt+Up` / `PgUp` | Scroll history up |
| `Alt+Down` / `PgDn` | Scroll history down |
| `Ctrl+Home` | Scroll to top |
| `Ctrl+End` | Scroll to bottom (auto-scroll) |

### Chat Commands (/-prefixed)

| Command | Action |
|---------|--------|
| `/clear` | Clear conversation history |
| `/plan <desc>` | Create a new plan |
| `/help` | Show help |
| `/quit` | Quit (alternative to Ctrl+C x2) |

### Loops View

**Navigation mode** - single keys work since there's no text input.

| Key | Action |
|-----|--------|
| `Up` / `k` | Select previous |
| `Down` / `j` | Select next |
| `Left` / `h` | Collapse node (or select parent) |
| `Right` / `l` | Expand node |
| `Enter` | View details (describe) |
| `o` | View output |
| `x` | Cancel loop |
| `s` | Toggle state (pause/resume) |
| `g` | Go to top |
| `G` | Go to bottom |
| `PgUp` / `PgDn` | Page up/down |
| `/` | Filter loops (enter filter input) |

---

## References

- [tui.md](tui.md) - High-level TUI design
- [architecture.md](architecture.md) - System overview
- Source: `taskdaemon/td/src/tui/` (proven implementation)
