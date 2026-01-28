//! TUI Runner - main event loop.
//!
//! The `TuiRunner` owns the terminal, app, and event handler. It runs the
//! main loop: render → handle events → process actions → repeat.

use super::Tui;
use super::app::App;
use super::events::{Event, EventHandler};
use super::state::{ChatMessage, PendingAction};
use super::views::render;
use crate::store::{LoopRecord, LoopStatus, TaskStore};
use eyre::Result;
use log::info;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Main TUI runner that owns the event loop.
pub struct TuiRunner {
    /// The terminal instance
    terminal: Tui,
    /// Application state and input handling
    app: App,
    /// Event handler for keyboard and tick events
    event_handler: EventHandler,
    /// Task store for persistence (optional - can run without)
    store: Option<Arc<Mutex<TaskStore>>>,
}

impl TuiRunner {
    /// Create a new TUI runner.
    pub fn new(terminal: Tui) -> Self {
        Self {
            terminal,
            app: App::new(),
            event_handler: EventHandler::default(),
            store: None,
        }
    }

    /// Create a new TUI runner with a TaskStore.
    pub fn with_store(terminal: Tui, store: Arc<Mutex<TaskStore>>) -> Self {
        Self {
            terminal,
            app: App::new(),
            event_handler: EventHandler::default(),
            store: Some(store),
        }
    }

    /// Get a reference to the app.
    pub fn app(&self) -> &App {
        &self.app
    }

    /// Get a mutable reference to the app.
    pub fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }

    /// Run the main TUI loop.
    pub async fn run(&mut self) -> Result<()> {
        info!("Starting TUI main loop");

        loop {
            // 1. Render current state
            self.terminal.draw(|f| render(self.app.state(), f))?;

            // 2. Handle events (keyboard, tick)
            let event = self.event_handler.next().await?;
            match event {
                Event::Key(key) => {
                    if self.app.handle_key(key) {
                        break; // Quit requested
                    }
                }
                Event::Tick => {
                    // Refresh state from TaskStore
                    self.refresh_state().await?;
                }
                Event::Resize(_, _) => {
                    // Terminal will handle resize on next draw
                }
            }

            // 3. Process pending actions
            self.process_pending_actions().await?;

            // 4. Check for quit
            if self.app.state().should_quit {
                break;
            }
        }

        info!("TUI main loop ended");
        Ok(())
    }

    /// Refresh state from TaskStore.
    async fn refresh_state(&mut self) -> Result<()> {
        if let Some(store) = &self.store {
            let store = store.lock().await;
            // Query all loops
            let loops: Vec<LoopRecord> = store.list_all()?;

            // Update tree
            self.app.state_mut().loops_tree.build_from_records(loops.clone());

            // Update metrics
            let active = loops.iter().filter(|l| l.status == LoopStatus::Running).count();
            let draft = loops
                .iter()
                .filter(|l| l.status == LoopStatus::Pending && l.iteration == 0)
                .count();
            let complete = loops.iter().filter(|l| l.status == LoopStatus::Complete).count();

            self.app.state_mut().loops_active = active;
            self.app.state_mut().loops_draft = draft;
            self.app.state_mut().loops_complete = complete;
        }

        Ok(())
    }

    /// Process pending actions from user input.
    async fn process_pending_actions(&mut self) -> Result<()> {
        // Handle pending chat submit
        if let Some(message) = self.app.state_mut().pending_chat_submit.take() {
            self.handle_chat_submit(&message).await?;
        }

        // Handle pending loop actions
        if let Some(action) = self.app.state_mut().pending_action.take() {
            self.handle_loop_action(action).await?;
        }

        Ok(())
    }

    async fn handle_chat_submit(&mut self, message: &str) -> Result<()> {
        // Add user message to history
        self.app.state_mut().chat_history.push(ChatMessage::user(message));

        // For now, just add a placeholder response
        // In a full implementation, this would call the LLM client
        self.app.state_mut().chat_history.push(ChatMessage::assistant(
            "I received your message. LLM integration is coming in a future phase.",
        ));

        info!("Chat message processed: {}", message);
        Ok(())
    }

    async fn handle_loop_action(&mut self, action: PendingAction) -> Result<()> {
        match action {
            PendingAction::CreatePlan(description) => {
                info!("Creating plan: {}", description);
                // For now, just add a message
                self.app.state_mut().chat_history.push(ChatMessage::system(format!(
                    "Plan creation requested: {}\n\nThis will be implemented in Phase 5 (Loop Hierarchy).",
                    description
                )));

                // If we have a store, create a draft plan loop
                if let Some(store) = &self.store {
                    let mut store = store.lock().await;
                    let record = LoopRecord::new_plan(&description, 5);
                    store.save(&record)?;
                    info!("Created plan loop: {}", record.id);
                }
            }
            PendingAction::CancelLoop(id) => {
                info!("Canceling loop: {}", id);
                if let Some(store) = &self.store {
                    let mut store = store.lock().await;
                    if let Some(mut record) = store.get(&id)? {
                        record.status = LoopStatus::Invalidated;
                        record.touch();
                        store.update(&record)?;
                    }
                }
            }
            PendingAction::PauseLoop(id) => {
                info!("Pausing loop: {}", id);
                if let Some(store) = &self.store {
                    let mut store = store.lock().await;
                    if let Some(mut record) = store.get(&id)? {
                        record.status = LoopStatus::Paused;
                        record.touch();
                        store.update(&record)?;
                    }
                }
            }
            PendingAction::ResumeLoop(id) => {
                info!("Resuming loop: {}", id);
                if let Some(store) = &self.store {
                    let mut store = store.lock().await;
                    if let Some(mut record) = store.get(&id)? {
                        record.status = LoopStatus::Running;
                        record.touch();
                        store.update(&record)?;
                    }
                }
            }
            PendingAction::ActivateDraft(id) => {
                info!("Activating draft: {}", id);
                if let Some(store) = &self.store {
                    let mut store = store.lock().await;
                    if let Some(mut record) = store.get(&id)? {
                        record.status = LoopStatus::Pending;
                        record.touch();
                        store.update(&record)?;
                    }
                }
            }
            PendingAction::DeleteLoop(id) => {
                info!("Delete loop requested: {} (deletion not implemented yet)", id);
                // TaskStore doesn't have a delete method - records are soft-deleted via status
                if let Some(store) = &self.store {
                    let mut store = store.lock().await;
                    if let Some(mut record) = store.get(&id)? {
                        record.status = LoopStatus::Invalidated;
                        record.touch();
                        store.update(&record)?;
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: Full TUI tests require a terminal, which is difficult in CI.
    // These tests verify the structure compiles and basic logic works.

    #[test]
    fn test_runner_creation() {
        // We can't create a real terminal in tests, but we can verify
        // the App and EventHandler work standalone
        let app = App::new();
        assert!(!app.state().should_quit);

        let handler = EventHandler::default();
        let _ = handler; // Just verify it compiles
    }

    #[tokio::test]
    async fn test_pending_action_types() {
        // Verify action enum variants exist
        let actions = vec![
            PendingAction::CreatePlan("test".to_string()),
            PendingAction::CancelLoop("123".to_string()),
            PendingAction::PauseLoop("123".to_string()),
            PendingAction::ResumeLoop("123".to_string()),
            PendingAction::ActivateDraft("123".to_string()),
            PendingAction::DeleteLoop("123".to_string()),
        ];

        for action in actions {
            // Just verify the pattern matching compiles
            match action {
                PendingAction::CreatePlan(_) => {}
                PendingAction::CancelLoop(_) => {}
                PendingAction::PauseLoop(_) => {}
                PendingAction::ResumeLoop(_) => {}
                PendingAction::ActivateDraft(_) => {}
                PendingAction::DeleteLoop(_) => {}
            }
        }
    }
}
