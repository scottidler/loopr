# Design Document: TUI Testing Strategy

**Author:** Scott Idler
**Date:** 2026-03-21
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

A four-layer testing strategy for Loopr's ratatui-based TUI that enables automated validation without requiring a human to watch the screen. Covers state logic unit tests, TestBackend content assertion tests, event replay integration tests, and a multimodal screenshot review protocol for interactive sessions.

## Problem Statement

### Background

Loopr's TUI (`src/tui/`) is built on ratatui 0.30 + crossterm 0.29. It has 8 views (Chat, Dashboard, Works, Bundles, Ticks, Learnings, Locks, Agents), a chat system with streaming, and an input system with 4 modes (Normal, GoalInput, ChatInput, ChatScroll). The non-TUI code validates easily via `otto ci` - compile, clippy, fmt, and tests provide a tight feedback loop. The TUI has no equivalent.

Currently, the TUI has:
- Good state logic tests in `app.rs` (view cycling, selection, role cycling, defaults)
- "Does not panic" render tests in every view using `TestBackend`
- Solid `input.rs` tests for key mapping and `apply_action` state mutations
- Zero tests that assert on what the user actually sees (buffer contents, layout, text)
- Zero tests that replay multi-step interaction sequences
- No way for an LLM agent (Claude) to get TUI feedback during iteration loops

### Problem

An LLM agent iterating on TUI code cannot validate its work. `otto ci` passes if the code compiles and existing "does not panic" tests pass, but those tests don't catch:
- Text rendered in the wrong position or not at all
- Layout broken at certain terminal sizes
- Missing or incorrect styling
- State transitions that produce visually wrong output (e.g., streaming indicator stuck)
- Regressions in multi-step interactions (type message, submit, see response, scroll)

The gap between "compiles and doesn't panic" and "looks right" is where TUI bugs live.

### Goals

- **G1**: LLM agent can validate TUI changes via `otto ci` without human intervention
- **G2**: Catch visual regressions (text content, layout, styling) automatically
- **G3**: Validate multi-step interaction sequences (key event -> state change -> correct render)
- **G4**: Human-in-the-loop review for subjective visual quality when needed
- **G5**: Existing test patterns remain compatible; no rewrites of working tests

### Non-Goals

- Full end-to-end testing through the daemon (that's integration/e2e territory)
- Pixel-perfect screenshot comparison (fragile, not worth it)
- Testing crossterm terminal escape sequences (crossterm's problem)
- Real IPC in TUI tests (mock the client)
- Testing async event loop (`run.rs`) - that requires IPC mocking, separate concern

## Proposed Solution

### Overview

Four testing layers, each addressing a different class of TUI bug:

| Layer | What it catches | Runs in CI | Agent-verifiable |
|-------|----------------|------------|-----------------|
| **L1: State Logic** | Wrong state after input | Yes | Yes |
| **L2: Content Assertions** | Wrong text rendered, missing elements | Yes | Yes |
| **L3: Event Replay** | Broken multi-step flows | Yes | Yes |
| **L4: Screenshot Review** | Subjective visual issues | No | With human |

### Architecture

#### Layer 1: State Logic Tests (existing, extend)

Already in `app.rs` and `input.rs`. The pattern is right - test `handle_key` -> `Action` mapping and `apply_action` -> state mutation independently.

**What to add** - coverage for currently untested state transitions:
- Chat slash command state machine (`/plan` -> `/draft` -> `/accept` flow)
- Funnel state transitions and their side effects
- Edge cases in cursor movement (UTF-8 boundaries, empty input)
- Goal input submit -> pending IPC generation
- Slash commands in wrong funnel state (e.g., `/draft` without `/plan` first)

#### Layer 2: Content Assertion Tests (new)

Use ratatui's `TestBackend` to render frames, then assert on the buffer contents. The chat view already has `TestBackend` tests that only check "does not panic". Upgrade them to check what's actually rendered.

```rust
#[test]
fn test_welcome_message_shown_when_empty() {
    let app = App::new();
    let mut terminal = test_terminal(80, 24);
    terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

    let buffer = terminal.backend().buffer();
    assert!(buffer_contains_text(buffer, "Welcome to Loopr Chat"));
    assert!(buffer_contains_text(buffer, "Type a message and press Enter"));
}

#[test]
fn test_user_message_displayed_with_prefix() {
    let mut app = App::new();
    app.chat_history.push(ChatMessage::user("hello world".into()));

    let mut terminal = test_terminal(80, 24);
    terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

    let buffer = terminal.backend().buffer();
    assert!(buffer_contains_text(buffer, "> hello world"));
}
```

**Key utility**: `buffer_contains_text(buffer: &Buffer, text: &str) -> bool` scans the `TestBackend` buffer row-by-row, concatenating cell symbols into strings, and checks for substring matches. This is the core primitive all content tests use.

**How it works under the hood**: `Buffer` has a public `area: Rect` field and a `cell((x, y))` method that returns `Option<&Cell>`. `Cell::symbol()` returns the grapheme as `&str`. We iterate rows within `area`, build a string per row, and search for the target text.

**Optional enhancement**: `insta` snapshot tests for full-frame captures. Use sparingly - only for complex views like Dashboard that have multiple layout regions.

#### Layer 3: Event Replay Tests (new)

Test multi-step sequences by feeding `KeyEvent`s through `handle_key` + `apply_action`, then rendering and asserting on intermediate states. These are synchronous - no tokio runtime needed since `handle_key` and `apply_action` are pure functions.

```rust
#[test]
fn test_chat_submit_flow() {
    let mut app = App::new();

    // Type "hello" and submit
    type_and_submit(&mut app, "hello");

    // apply_action(ChatSubmit) pushes ChatMessage::user to chat_history
    // and sets pending_chat_submit (consumed by the event loop)
    assert!(app.chat_input.is_empty());
    assert_eq!(app.chat_cursor_pos, 0);
    assert_eq!(app.chat_history.len(), 1);
    assert_eq!(app.chat_history[0].content, "hello");
    assert_eq!(app.chat_history[0].role, ChatRole::User);
    assert_eq!(app.pending_chat_submit, Some("hello".to_string()));
}

#[test]
fn test_plan_funnel_flow() {
    let mut app = App::new();

    // /plan transitions: Chat -> Interview, Chat -> Plan
    type_and_submit(&mut app, "/plan");
    assert_eq!(app.funnel_state, FunnelState::Interview);
    assert_eq!(app.chat_mode, ChatMode::Plan);
    // System message added
    assert!(app.chat_history.iter().any(|m|
        m.role == ChatRole::System && m.content.contains("Plan mode")));

    // Regular message in Interview mode -> goes to LLM
    type_and_submit(&mut app, "Build a widget");
    assert_eq!(app.pending_chat_submit, Some("Build a widget".to_string()));

    // /draft transitions: Interview -> PlanDraft
    type_and_submit(&mut app, "/draft");
    assert_eq!(app.funnel_state, FunnelState::PlanDraft);
    assert_eq!(app.pending_chat_submit, Some("/draft".to_string()));
}

#[test]
fn test_slash_command_wrong_state() {
    let mut app = App::new();

    // /draft without /plan first - should show error system message
    type_and_submit(&mut app, "/draft");
    assert_eq!(app.funnel_state, FunnelState::Chat); // unchanged
    assert!(app.chat_history.iter().any(|m|
        m.role == ChatRole::System && m.content.contains("/plan first")));
}
```

**Key utility**: `type_and_submit(app: &mut App, text: &str)` simulates typing each character via `handle_key(KeyCode::Char(c), ...)` + `apply_action`, then `handle_key(KeyCode::Enter, ...)` + `apply_action`. Makes flow tests readable and concise.

**Combined L2 + L3**: The most valuable tests combine both layers - replay an interaction, then render and assert on buffer content:

```rust
#[test]
fn test_submit_then_render_shows_message() {
    let mut app = App::new();
    type_and_submit(&mut app, "hello world");

    let mut terminal = test_terminal(80, 24);
    terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

    let buffer = terminal.backend().buffer();
    assert!(buffer_contains_text(buffer, "> hello world"));
    // Welcome message should be gone (replaced by chat history)
    assert!(!buffer_contains_text(buffer, "Welcome to Loopr Chat"));
}
```

#### Layer 4: Screenshot Review Protocol (human-assisted)

For interactive sessions where Claude is iterating on TUI code:

1. Claude makes changes and runs `otto ci` (L1-L3 pass)
2. User runs `loopr` in a terminal
3. User takes a screenshot and pastes it into the conversation
4. Claude reads the image (multimodal) and provides feedback

This is not automated but closes the loop for subjective quality (color choices, alignment aesthetics, overall feel). Document this as the expected workflow in CLAUDE.md so future sessions know the protocol.

### Test Utilities Module

Create `src/tui/test_utils.rs` with shared helpers. The module is `pub(crate)` and only compiled in test builds via `#[cfg(test)]` on the `mod` declaration in `src/tui/mod.rs`.

```rust
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use super::app::App;
use super::input::{handle_key, apply_action};

/// Create a test terminal with the given dimensions.
pub fn test_terminal(width: u16, height: u16) -> Terminal<TestBackend> {
    Terminal::new(TestBackend::new(width, height)).unwrap()
}

/// Check if a buffer contains the given text substring anywhere.
///
/// Scans row-by-row, concatenating cell symbols into a string per row,
/// then checks if any row contains `text` as a substring.
pub fn buffer_contains_text(buffer: &Buffer, text: &str) -> bool {
    let area = buffer.area;
    for y in area.top()..area.bottom() {
        let mut row = String::new();
        for x in area.left()..area.right() {
            row.push_str(buffer.cell((x, y)).map_or(" ", |c| c.symbol()));
        }
        if row.contains(text) {
            return true;
        }
    }
    false
}

/// Extract all text from a buffer region, one string per row.
/// Trailing whitespace is trimmed from each row.
pub fn buffer_text_rows(buffer: &Buffer, area: ratatui::layout::Rect) -> Vec<String> {
    (area.top()..area.bottom())
        .map(|y| {
            let mut row = String::new();
            for x in area.left()..area.right() {
                row.push_str(buffer.cell((x, y)).map_or(" ", |c| c.symbol()));
            }
            row.trim_end().to_string()
        })
        .collect()
}

/// Simulate typing a string and pressing Enter.
pub fn type_and_submit(app: &mut App, text: &str) {
    for c in text.chars() {
        let action = handle_key(
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            app.input_mode,
        );
        apply_action(app, action);
    }
    let action = handle_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        app.input_mode,
    );
    apply_action(app, action);
}

/// Simulate a single key press.
pub fn press_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    let action = handle_key(KeyEvent::new(code, modifiers), app.input_mode);
    apply_action(app, action);
}
```

**Note on `map_or`**: When `cell()` returns `None` (should not happen within `area` bounds), we fall back to `" "` (space) rather than empty string, matching ratatui's default empty cell behavior.

### Implementation Plan

**Phase 1: Test utilities + content assertions for chat view**
- Create `src/tui/test_utils.rs` with `buffer_contains_text`, `type_and_submit`, `test_terminal`, `buffer_text_rows`, `press_key`
- Add `#[cfg(test)] pub(crate) mod test_utils;` to `src/tui/mod.rs`
- Upgrade `views/chat.rs` "does not panic" tests to content assertions:
  - Welcome message present when empty
  - User messages display with `> ` prefix
  - Assistant messages display with `  ` prefix
  - Streaming indicator ("Thinking...") present when `chat_streaming = true`
  - Plan mode title shows " Plan " instead of " Chat "
  - Border color changes with funnel state (assert via cell style, not just text)
- Upgrade `views/dashboard.rs` tests to assert on status text ("Role:", "Connection:") and queue count format

**Phase 2: Event replay tests**
- Add replay tests to `input.rs` for full flows:
  - Chat submit (type + Enter -> chat_history populated, pending_chat_submit set)
  - `/plan` -> `/draft` -> `/accept` funnel progression
  - `/clear` resets state
  - Slash commands in wrong state produce system error messages
  - `/help` in each funnel state shows correct available commands
- Add cursor movement edge case tests (Home/End, UTF-8 multi-byte, empty input, cursor at bounds)
- Add view cycling + input mode sync tests as replay sequences

**Phase 3: Content assertions for remaining views**
- Works, Bundles, Ticks, Learnings, Locks, Agents views
- Test with empty state (should show empty list, no panic) - already covered, upgrade to content checks
- Test with populated state (items visible, correct format)
- Test selection highlighting renders at correct position (check for `> ` highlight symbol)

**Phase 4: Terminal size resilience + combined L2+L3 tests**
- Run key content assertions at multiple terminal sizes (80x24, 120x40, 40x10)
- Verify no panics and critical text still visible at small sizes
- Add combined replay-then-render tests for key flows (submit message -> see it rendered, /plan -> see border color change)

## Alternatives Considered

### Alternative 1: insta snapshot testing as primary approach
- **Description:** Capture full `TestBackend` buffer as text snapshots, use `insta` for diff-based regression detection
- **Pros:** Catches any visual change automatically; easy to write tests (just render and snapshot)
- **Cons:** Snapshots break on any intentional change (high churn); initial snapshots require human review; LLM agent can't approve snapshot updates without human; fragile across ratatui version bumps
- **Why not chosen:** As primary approach, too much friction. An LLM agent iterating on TUI code would break snapshots with every change and have no way to approve the new baseline. Content assertions ("does it contain this text?") are change-tolerant - they only break when something that should be visible isn't.

### Alternative 2: Headless terminal emulator (vt100/vte crate)
- **Description:** Run the full TUI through a virtual terminal emulator, capture the rendered output
- **Pros:** Tests the actual crossterm escape sequences; closest to real user experience
- **Cons:** Heavy dependency; crossterm quirks are not our bugs to catch; much slower than TestBackend; complex setup for marginal benefit
- **Why not chosen:** TestBackend gives us the ratatui rendering output which is the layer we control. Crossterm is well-tested upstream. The escape-sequence layer is not where Loopr's TUI bugs live.

### Alternative 3: Property-based testing with proptest
- **Description:** Generate random key sequences and verify invariants (no panics, state consistency)
- **Pros:** Finds edge cases humans miss; good for input handling robustness
- **Cons:** Hard to define meaningful properties beyond "doesn't panic"; slow; produces hard-to-reproduce failures
- **Why not chosen:** Good complement but not a replacement. Worth adding later for `input.rs` fuzz testing. Invariants to test: `chat_cursor_pos <= chat_input.len()`, `selected_index < current_list_len()` (or 0 if empty), `input_mode == ChatInput` iff `current_view == Chat`.

## Technical Considerations

### Dependencies

- **ratatui 0.30** (already present) - `TestBackend`, `Buffer`, `Cell` API
- **crossterm 0.29** (already present) - `KeyEvent`, `KeyCode`, `KeyModifiers` for replay tests
- No new dependencies required for Phases 1-4
- **insta** (optional, dev-dependency) - only if snapshot tests are added later

### Performance

- All L1-L3 tests are synchronous, in-memory, no I/O. Sub-millisecond per test.
- TestBackend buffer allocation: 80x24 = 1,920 cells, ~30KB. Negligible.
- Event replay tests are loops over `handle_key` + `apply_action` - pure function calls, very fast.
- No tokio runtime needed for any of these tests.

### Testing Strategy

Meta-validation for each phase:
- Phase 1: Intentionally break the welcome message text in `chat.rs` -> `buffer_contains_text` test fails
- Phase 2: Remove the ChatSubmit handler in `apply_action` -> replay test catches it
- Phase 3: Change a view's list format -> content assertion catches it
- Phase 4: Shrink a view's terminal size below minimum -> test catches panic or missing content

### Rollout Plan

Implement in-tree, gate behind `#[cfg(test)]`. No runtime cost. Ship incrementally per phase - each phase is independently valuable and mergeable.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| TestBackend buffer API changes across ratatui versions | Low | Medium | `buffer_contains_text` is the single touchpoint to buffer internals; update one function |
| Content assertions become brittle on cosmetic changes | Medium | Low | Assert on semantic content ("Welcome to Loopr Chat"), not exact positioning; use substring matching |
| Test utils module grows into a mini-framework | Low | Low | Cap at 5-6 functions. If it grows, the test design is wrong. |
| `buffer_contains_text` misses text split across rows by wrapping | Medium | Low | For wrap-sensitive assertions, use `buffer_text_rows` and check across adjacent rows. Most assertions target short strings that fit in one row at 80-width. |
| Tests pass but real terminal looks different (crossterm rendering quirk) | Low | Low | L4 (screenshot protocol) catches this class of bug. TestBackend tests the ratatui layer, which is where our code lives. |

## Open Questions

- [ ] Should `insta` be added as a dev-dependency for selective snapshot tests, or defer to a later phase?
- [ ] Should event replay tests cover mouse scroll events? (Mouse handling is in `run.rs` event loop, not `apply_action`, so it needs a different test pattern.)
- [ ] Is it worth testing the help overlay content, or is it static enough to not regress?

## References

- ratatui `TestBackend` - `ratatui::backend::TestBackend`
- ratatui `Buffer` API - `ratatui_core::buffer::Buffer` (fields: `area: Rect`, `content: Vec<Cell>`, methods: `cell()`, `get()`)
- ratatui `Cell` API - `cell.symbol() -> &str`
- Existing tests: `src/tui/app.rs:366`, `src/tui/views/chat.rs:199`, `src/tui/input.rs`
- ratatui testing recipes: https://ratatui.rs/recipes/testing/
