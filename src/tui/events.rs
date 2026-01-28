//! Event handling for the TUI.
//!
//! This module provides:
//! - `Event`: The unified event type (keyboard, tick)
//! - `EventHandler`: Async event stream from keyboard and tick timer

use crossterm::event::{self, Event as CrosstermEvent, KeyEvent, KeyEventKind};
use eyre::Result;
use std::time::Duration;

/// Unified event type for the TUI.
#[derive(Debug, Clone)]
pub enum Event {
    /// Keyboard input event
    Key(KeyEvent),
    /// Periodic tick for state refresh
    Tick,
    /// Terminal resize
    Resize(u16, u16),
}

/// Handles keyboard and tick events.
///
/// Polls for crossterm events with a tick interval for periodic state refresh.
pub struct EventHandler {
    /// Tick rate in milliseconds
    tick_rate: Duration,
}

impl EventHandler {
    /// Create a new event handler with the given tick rate.
    pub fn new(tick_rate_ms: u64) -> Self {
        Self {
            tick_rate: Duration::from_millis(tick_rate_ms),
        }
    }

    /// Get the next event.
    ///
    /// Returns `Some(Event)` if an event occurred, `None` on timeout (tick).
    /// The tick is generated when the poll timeout expires without an event.
    pub async fn next(&self) -> Result<Event> {
        // Use tokio's blocking spawn to avoid blocking the async runtime
        let tick_rate = self.tick_rate;

        let event = tokio::task::spawn_blocking(move || -> Result<Event> {
            if event::poll(tick_rate)? {
                match event::read()? {
                    CrosstermEvent::Key(key) => {
                        // Only handle key press events, not release
                        if key.kind == KeyEventKind::Press {
                            Ok(Event::Key(key))
                        } else {
                            Ok(Event::Tick)
                        }
                    }
                    CrosstermEvent::Resize(w, h) => Ok(Event::Resize(w, h)),
                    _ => Ok(Event::Tick),
                }
            } else {
                // Timeout - generate tick
                Ok(Event::Tick)
            }
        })
        .await??;

        Ok(event)
    }
}

impl Default for EventHandler {
    fn default() -> Self {
        Self::new(250) // 250ms tick rate by default
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_handler_creation() {
        let handler = EventHandler::new(100);
        assert_eq!(handler.tick_rate, Duration::from_millis(100));
    }

    #[test]
    fn test_event_handler_default() {
        let handler = EventHandler::default();
        assert_eq!(handler.tick_rate, Duration::from_millis(250));
    }

    #[test]
    fn test_event_debug() {
        let tick = Event::Tick;
        let debug_str = format!("{:?}", tick);
        assert!(debug_str.contains("Tick"));
    }
}
