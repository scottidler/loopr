# Design Document: Fix Quit Behavior and Unified Chat UI

**Author:** Scott Idler
**Date:** 2026-02-01
**Status:** In Review
**Review Passes Completed:** 5/5

## Summary

This document proposes two UX improvements to the Loopr TUI: (1) changing quit behavior to only respond to Ctrl+C and Ctrl+D instead of the 'q' key, and (2) unifying the chat interface from a two-box layout into a single bordered container with an inline input prompt.

## Problem Statement

### Background

Loopr is a recursive loop-based AI task orchestration system with a ratatui-based TUI client. The TUI provides three views: Chat, Loops, and Approval. Users interact primarily through the Chat view, typing messages to communicate with the Loopr daemon.

The current TUI implementation has two usability issues that impact the chat experience:

1. **Quit key conflict**: The 'q' key is mapped to quit the application, preventing users from typing the letter 'q' in chat messages without accidentally exiting.

2. **Redundant visual chrome**: The chat interface uses two separate bordered boxes (one for messages, one for input), consuming vertical space and creating visual clutter that doesn't add value.

### Problem

**Issue 1: 'q' key quits the application**

When users are in the Chat view composing a message, pressing 'q' immediately exits the application instead of inserting the character. This is unexpected behavior for a text input field and forces users to avoid words containing 'q' or carefully manage focus.

Current implementation in `src/tui/input.rs`:
```rust
pub fn is_quit(&self) -> bool {
    self.code == KeyCode::Char('q')
        || (self.code == KeyCode::Char('c') && self.modifiers.contains(KeyModifiers::CONTROL))
}
```

**Issue 2: Two-box chat layout wastes space**

The current chat layout splits the view into two bordered boxes:
- Top: "Chat Messages" box with full border
- Bottom: "Input" box with full border (fixed 3-line height)

This design:
- Wastes 4 lines of vertical space on redundant borders
- Creates unnecessary visual separation between messages and input
- Doesn't match modern chat application conventions

### Goals

- Allow users to type any character, including 'q', in the chat input
- Maintain clear, standard quit mechanisms (Ctrl+C, Ctrl+D)
- Create a cleaner, more space-efficient chat interface
- Follow modern chat UI conventions with inline input prompts
- Improve the overall user experience without breaking existing functionality

### Non-Goals

- Changing the three-tab view structure (Chat, Loops, Approval)
- Adding new chat features (emoji, formatting, etc.)
- Modifying the Loops or Approval views
- Changing the underlying message/daemon communication
- Adding configurable key bindings (future work)

## Proposed Solution

### Overview

Implement two targeted changes to the TUI:

1. **Quit behavior**: Remove 'q' from quit triggers, keep Ctrl+C, add Ctrl+D
2. **Chat layout**: Merge two boxes into single container with `> ` input prompt

### Architecture

No architectural changes required. Both modifications are localized UI changes:

```
Before:                          After:
┌─ Chat Messages ──────────┐    ┌─ Chat ────────────────────┐
│ You: Hello               │    │ You: Hello                │
│ Loopr: Hi there!         │    │ Loopr: Hi there!          │
│                          │    │                           │
│                          │    │                           │
└──────────────────────────┘    │                           │
┌─ Input ──────────────────┐    │ > _                       │
│ _                        │    └───────────────────────────┘
└──────────────────────────┘
```

### Data Model

No data model changes. The existing `AppState` structure remains unchanged:

```rust
pub struct AppState {
    pub active_view: ActiveView,
    pub should_quit: bool,
    pub chat_input: String,
    pub chat_messages: Vec<ChatMessage>,
    // ... other fields unchanged
}
```

### API Design

No API changes. Internal method signature update only:

**`KeyEvent::is_quit()` behavior change:**

| Key | Before | After |
|-----|--------|-------|
| `q` | Quit | Insert 'q' |
| `Ctrl+C` | Quit | Quit |
| `Ctrl+D` | (no action) | Quit |

### Implementation Plan

#### Phase 1: Update Quit Behavior

**File: `src/tui/input.rs`**

Modify `is_quit()` method:

```rust
/// Check if this is a quit key (Ctrl+C or Ctrl+D)
pub fn is_quit(&self) -> bool {
    (self.code == KeyCode::Char('c') && self.modifiers.contains(KeyModifiers::CONTROL))
        || (self.code == KeyCode::Char('d') && self.modifiers.contains(KeyModifiers::CONTROL))
}
```

**File: `src/main.rs`**

Update user-facing messages:

- Line ~104 (welcome message): Change "q to quit" to "Ctrl+C to quit"
- Line ~165 (status bar): Change "q to quit" to "Ctrl+C to quit"

#### Phase 2: Unify Chat Layout

**File: `src/tui/views.rs`**

Replace `ChatView::render()` implementation:

```rust
impl View for ChatView {
    fn render(&self, frame: &mut Frame, area: Rect, state: &AppState) {
        // Single outer block with border
        let outer_block = Block::default()
            .borders(Borders::ALL)
            .title(" Chat ");

        let inner_area = outer_block.inner(area);
        frame.render_widget(outer_block, area);

        // Split inner area: messages at top, input line at bottom
        let inner_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner_area);

        // Messages area (no borders)
        let items: Vec<ListItem> = state
            .chat_messages
            .iter()
            .map(Self::format_message)
            .collect();
        let messages = List::new(items);
        frame.render_widget(messages, inner_chunks[0]);

        // Input line with prompt (no borders)
        let input_line = Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Cyan)),
            Span::raw(state.chat_input.as_str()),
            Span::styled("_", Style::default().fg(Color::DarkGray)),
        ]);
        let input = Paragraph::new(input_line);
        frame.render_widget(input, inner_chunks[1]);
    }

    fn title(&self) -> &'static str {
        "Chat"
    }
}
```

#### Phase 3: Update Tests

**File: `src/tui/input.rs`**

Update existing test `test_key_event_is_quit` (lines 301-310):

```rust
#[test]
fn test_key_event_is_quit() {
    // 'q' should NOT quit (changed from previous behavior)
    let q_key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
    assert!(!q_key.is_quit());

    // Ctrl+C should quit (unchanged)
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(ctrl_c.is_quit());

    // Ctrl+D should quit (new)
    let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert!(ctrl_d.is_quit());

    // Other chars should NOT quit (unchanged)
    let a_key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
    assert!(!a_key.is_quit());
}
```

No UI rendering snapshot tests exist, so no updates needed there.

### Summary: Files to Modify

| File | Lines | Change |
|------|-------|--------|
| `src/tui/input.rs` | 23-27 | Update `is_quit()` to remove 'q', add Ctrl+D |
| `src/tui/input.rs` | 301-310 | Update `test_key_event_is_quit` test |
| `src/main.rs` | 104 | Update welcome message text |
| `src/main.rs` | 165 | Update status bar default text |
| `src/tui/views.rs` | 57-79 | Replace entire `ChatView::render()` method |

## Alternatives Considered

### Alternative 1: Modal Input Mode

- **Description:** Add an "insert mode" like vim where 'q' only quits when not in input mode
- **Pros:** Keeps 'q' as quick quit when navigating; familiar to vim users
- **Cons:** Adds complexity; requires mode indicator UI; learning curve for non-vim users
- **Why not chosen:** Over-engineered for the problem; most users expect text fields to accept all characters

### Alternative 2: Escape-then-Q to Quit

- **Description:** Require pressing Escape first to "exit" the input field, then 'q' to quit
- **Pros:** Maintains 'q' quit; prevents accidental exits
- **Cons:** Two-key sequence is slower; Escape already used for view switching
- **Why not chosen:** Conflicts with existing Escape behavior (returns to Chat view from other tabs)

### Alternative 3: Keep Two Boxes, Remove Borders

- **Description:** Keep the two-box layout but remove inner borders
- **Pros:** Minimal code change; maintains logical separation
- **Cons:** Still wastes vertical space; input area height still fixed at 3 lines
- **Why not chosen:** Doesn't address the core space efficiency issue

### Alternative 4: Floating Input Overlay

- **Description:** Input appears as a floating bar at the bottom of the terminal
- **Pros:** Modern look; clear visual separation
- **Cons:** More complex rendering; potential z-ordering issues with ratatui
- **Why not chosen:** Significantly more complex implementation for marginal benefit

## Technical Considerations

### Dependencies

No new dependencies required. Uses existing:
- `ratatui 0.29` - Layout, Block, Paragraph, List, Span, Style
- `crossterm 0.28` - KeyCode, KeyModifiers

### Performance

No performance impact. Changes are:
- One fewer conditional check in `is_quit()` (removes 'q' check, adds Ctrl+D check)
- Similar rendering complexity (single block vs two blocks)

### Security

No security implications. Changes are purely cosmetic/UX.

### Testing Strategy

1. **Unit tests:** Update existing `test_key_event_is_quit` in `src/tui/input.rs` as described in Phase 3.

2. **Manual testing:**
   - Launch TUI with `cargo run`
   - Type message containing 'q' - should appear in input
   - Press Ctrl+C - should quit
   - Press Ctrl+D - should quit
   - Verify single-box chat layout renders correctly
   - Verify `> _` prompt appears at bottom of chat area

### Rollout Plan

1. Implement changes on `v2` branch
2. Run `cargo build` to verify compilation
3. Run `cargo test` to verify tests pass
4. Manual QA testing of TUI
5. Commit with descriptive message
6. Merge to main when ready

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Users accustomed to 'q' quit | Low | Low | Clear documentation; Ctrl+C is universal |
| Ctrl+D conflicts with shell EOF | Low | Medium | Only triggers when TUI has focus; consistent with other TUIs |
| Layout breaks on small terminals | Low | Medium | Test with minimum terminal size (80x24) |
| Cursor visibility in input | Low | Low | Use visible `_` character as cursor indicator |
| Long input text overflow | Low | Medium | Paragraph widget handles horizontal overflow; future enhancement for scrolling |
| Message list scrolling | Low | Low | List widget handles overflow; auto-scroll to bottom not in scope |

### Edge Cases to Verify

1. **Empty chat input:** The `> _` prompt should display correctly with no text
2. **Very long input:** Single-line input should truncate gracefully (existing Paragraph behavior)
3. **Unicode input:** Cursor indicator should work with multi-byte characters (existing TextInput handles this)
4. **Terminal resize:** Layout should adapt (ratatui handles this automatically)
5. **Many chat messages:** List should scroll (existing List widget behavior)
6. **Ctrl+D on empty input:** Should quit, not send empty message (quit check happens before input handling)

## Open Questions

All questions resolved:

- [x] Should Ctrl+D be added as a quit key? **Decision: Yes, standard terminal convention**
- [x] Should the input prompt be `> ` or `>>> `? **Decision: `> ` for simplicity**
- [x] Should cursor blink be implemented? **Decision: Deferred to future enhancement**

## References

- `src/tui/input.rs` - Input handling and `is_quit()` implementation
- `src/tui/views.rs` - View rendering including `ChatView`
- `src/main.rs` - TUI initialization and event loop
- [Ratatui Documentation](https://docs.rs/ratatui/0.29.0/ratatui/) - UI framework docs
- [Crossterm Documentation](https://docs.rs/crossterm/0.28.0/crossterm/) - Terminal event handling
