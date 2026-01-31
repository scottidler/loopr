//! TUI Runner - main event loop.
//!
//! The `TuiRunner` owns the terminal, app, and event handler. It runs the
//! main loop: render → handle events → process actions → repeat.

use super::Tui;
use super::app::App;
use super::events::{Event, EventHandler};
use super::state::{ChatMessage, PendingAction, ToolCallDisplay};
use super::views::render;
use crate::config::load_config;
use crate::llm::ToolCall;
use crate::llm::anthropic::{AnthropicClient, AnthropicConfig};
use crate::llm::client::{
    CompletionRequest, ContentBlock, LlmClient, Message, MessageContent, Role, StopReason, StreamChunk,
};
use crate::llm::tools::{ToolContext, ToolExecutor};
use crate::store::{LoopRecord, LoopStatus, TaskStore};
use eyre::Result;
use log::{debug, error, info};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

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
    /// LLM client for chat (optional - can run without API key)
    llm_client: Option<Arc<dyn LlmClient>>,
    /// Receiver for streaming chunks from LLM
    stream_rx: Option<mpsc::Receiver<StreamChunk>>,
    /// Buffer for accumulating streaming response
    streaming_buffer: String,
    /// Conversation history for LLM context
    conversation_history: Vec<Message>,
    /// Tool executor for LLM tools
    tool_executor: ToolExecutor,
    /// Working directory for tool execution
    working_dir: PathBuf,
    /// Pending tool calls to execute
    pending_tool_calls: Vec<ToolCall>,
    /// Tool call displays for current message
    current_tool_displays: Vec<ToolCallDisplay>,
}

impl TuiRunner {
    /// Create a new TUI runner.
    pub fn new(terminal: Tui) -> Self {
        // Try to create LLM client from environment
        let llm_client = Self::try_create_llm_client();
        let working_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        Self {
            terminal,
            app: App::new(),
            event_handler: EventHandler::default(),
            store: None,
            llm_client,
            stream_rx: None,
            streaming_buffer: String::new(),
            conversation_history: Vec::new(),
            tool_executor: ToolExecutor::standard(),
            working_dir,
            pending_tool_calls: Vec::new(),
            current_tool_displays: Vec::new(),
        }
    }

    /// Create a new TUI runner with a TaskStore.
    pub fn with_store(terminal: Tui, store: Arc<Mutex<TaskStore>>) -> Self {
        // Try to create LLM client from environment
        let llm_client = Self::try_create_llm_client();
        let working_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        Self {
            terminal,
            app: App::new(),
            event_handler: EventHandler::default(),
            store: Some(store),
            llm_client,
            stream_rx: None,
            streaming_buffer: String::new(),
            conversation_history: Vec::new(),
            tool_executor: ToolExecutor::standard(),
            working_dir,
            pending_tool_calls: Vec::new(),
            current_tool_displays: Vec::new(),
        }
    }

    /// Try to create an LLM client from config file.
    fn try_create_llm_client() -> Option<Arc<dyn LlmClient>> {
        debug!("try_create_llm_client: called");

        // Load config from ~/.config/loopr/loopr.yml or defaults
        let config = match load_config(None) {
            Ok(c) => {
                debug!("try_create_llm_client: config loaded, llm.default={}", c.llm.default);
                c
            }
            Err(e) => {
                error!("try_create_llm_client: failed to load config: {}", e);
                return None;
            }
        };

        // Resolve LLM config - this validates the format and provider
        let resolved = match config.llm.resolve() {
            Ok(r) => {
                debug!(
                    "try_create_llm_client: resolved provider={}, model={}",
                    r.provider, r.model
                );
                r
            }
            Err(e) => {
                error!("try_create_llm_client: failed to resolve LLM config: {}", e);
                return None;
            }
        };

        let anthropic_config = AnthropicConfig {
            model: resolved.model.clone(),
            api_key_env: resolved.api_key_env.clone(),
            base_url: resolved.base_url.clone(),
            max_tokens: resolved.max_tokens,
            timeout_ms: resolved.timeout_ms,
        };

        debug!(
            "try_create_llm_client: creating client with model={}, base_url={}",
            anthropic_config.model, anthropic_config.base_url
        );

        match AnthropicClient::from_config(&anthropic_config) {
            Ok(client) => {
                info!(
                    "LLM client initialized: provider={}, model={}",
                    resolved.provider, resolved.model
                );
                Some(Arc::new(client))
            }
            Err(e) => {
                error!("try_create_llm_client: failed to create client: {}", e);
                None
            }
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
            // 1. Process any streaming chunks (updates state before render)
            self.process_stream_chunks();

            // 2. Render current state (including streaming buffer if active)
            let streaming_text = if self.app.state().chat_streaming {
                Some(self.streaming_buffer.as_str())
            } else {
                None
            };
            self.terminal.draw(|f| render(self.app.state(), f, streaming_text))?;

            // 3. Handle events (keyboard, tick)
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

            // 4. Process pending actions
            self.process_pending_actions().await?;

            // 5. Check for quit
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

        // Add to conversation history for LLM context
        self.conversation_history.push(Message {
            role: Role::User,
            content: MessageContent::Text(message.to_string()),
        });

        // Start LLM request
        self.start_llm_request().await
    }

    /// Start an LLM request with the current conversation history.
    async fn start_llm_request(&mut self) -> Result<()> {
        debug!("start_llm_request: messages={}", self.conversation_history.len());

        // Check if we have an LLM client
        if let Some(llm) = &self.llm_client {
            // Start streaming
            self.app.state_mut().chat_streaming = true;
            self.streaming_buffer.clear();
            self.pending_tool_calls.clear();
            self.current_tool_displays.clear();

            // Get tool definitions
            let tools = self.tool_executor.definitions();
            debug!(
                "start_llm_request: tools={}, working_dir={}",
                tools.len(),
                self.working_dir.display()
            );

            // Create the request
            let request = CompletionRequest {
                system_prompt: "You are a helpful AI assistant integrated into Loopr, a task management and automation tool. You have access to tools for reading files, running commands, and searching. Be concise and helpful.".to_string(),
                messages: self.conversation_history.clone(),
                tools,
                max_tokens: 4096,
            };

            // Create streaming channel
            let (tx, rx) = mpsc::channel(100);
            self.stream_rx = Some(rx);

            // Clone llm for the spawned task
            let llm = Arc::clone(llm);

            // Spawn the LLM task
            tokio::spawn(async move {
                if let Err(e) = llm.stream(request, tx).await {
                    error!("LLM streaming error: {}", e);
                }
            });

            info!("Started LLM streaming");
        } else {
            // No LLM client - show placeholder
            self.app.state_mut().chat_history.push(ChatMessage::assistant(
                "LLM not available. Set ANTHROPIC_API_KEY environment variable to enable chat.",
            ));
        }

        Ok(())
    }

    /// Process any pending streaming chunks from the LLM.
    fn process_stream_chunks(&mut self) {
        // Track if we need to execute tools after processing
        let mut should_execute_tools = false;
        let mut final_usage = None;

        if let Some(rx) = &mut self.stream_rx {
            // Process all available chunks without blocking
            while let Ok(chunk) = rx.try_recv() {
                match chunk {
                    StreamChunk::TextDelta(text) => {
                        self.streaming_buffer.push_str(&text);
                    }
                    StreamChunk::ToolUseStart { id, name } => {
                        debug!("Tool call started: {} ({})", name, id);
                        // Add to pending tool calls
                        self.pending_tool_calls.push(ToolCall {
                            id,
                            name: name.clone(),
                            input: serde_json::Value::Null,
                        });
                        // Add display entry
                        self.current_tool_displays.push(ToolCallDisplay {
                            name,
                            summary: "running...".to_string(),
                            expanded: false,
                            full_result: None,
                        });
                    }
                    StreamChunk::ToolUseDelta { id, json_delta } => {
                        // Accumulate JSON for the tool call
                        if let Some(call) = self.pending_tool_calls.iter_mut().find(|c| c.id == id) {
                            // Accumulate JSON delta (simple string concatenation for now)
                            if call.input.is_null() {
                                call.input = serde_json::Value::String(json_delta);
                            } else if let Some(s) = call.input.as_str() {
                                call.input = serde_json::Value::String(format!("{}{}", s, json_delta));
                            }
                        }
                    }
                    StreamChunk::ToolUseEnd { id } => {
                        // Parse the accumulated JSON
                        if let Some(call) = self.pending_tool_calls.iter_mut().find(|c| c.id == id) {
                            if let Some(json_str) = call.input.as_str()
                                && let Ok(parsed) = serde_json::from_str(json_str)
                            {
                                call.input = parsed;
                            }
                            debug!("Tool call complete: {} input={}", call.name, call.input);
                        }
                    }
                    StreamChunk::MessageDone { stop_reason, usage } => {
                        // Update token counts
                        self.app.state_mut().session_input_tokens += usage.input_tokens;
                        self.app.state_mut().session_output_tokens += usage.output_tokens;
                        self.app.state_mut().session_cost_usd += usage.cost_usd("claude-opus-4-5");
                        final_usage = Some(usage);

                        if stop_reason == StopReason::ToolUse && !self.pending_tool_calls.is_empty() {
                            // Need to execute tools and continue
                            should_execute_tools = true;
                        } else {
                            // Normal completion - finalize the response
                            self.finalize_assistant_response();
                        }

                        self.stream_rx = None;
                        break;
                    }
                    StreamChunk::Error(err) => {
                        self.app
                            .state_mut()
                            .chat_history
                            .push(ChatMessage::system(format!("Error: {}", err)));
                        self.app.state_mut().chat_streaming = false;
                        self.streaming_buffer.clear();
                        self.stream_rx = None;
                        break;
                    }
                }
            }
        }

        // If we need to execute tools, do it now
        if should_execute_tools {
            self.schedule_tool_execution();
        }

        let _ = final_usage; // Silence unused warning
    }

    /// Finalize the assistant response after streaming completes.
    fn finalize_assistant_response(&mut self) {
        if !self.streaming_buffer.is_empty() || !self.current_tool_displays.is_empty() {
            let mut msg = ChatMessage::assistant(&self.streaming_buffer);
            msg.tool_calls = std::mem::take(&mut self.current_tool_displays);
            self.app.state_mut().chat_history.push(msg);

            // Add to conversation history
            self.conversation_history.push(Message {
                role: Role::Assistant,
                content: MessageContent::Text(self.streaming_buffer.clone()),
            });
        }

        // Clear streaming state
        self.app.state_mut().chat_streaming = false;
        self.streaming_buffer.clear();
    }

    /// Schedule tool execution (called from sync context, spawns async task).
    fn schedule_tool_execution(&mut self) {
        let tool_calls = std::mem::take(&mut self.pending_tool_calls);
        if tool_calls.is_empty() {
            return;
        }

        // Build assistant message with tool uses for conversation history
        let mut content_blocks = Vec::new();
        if !self.streaming_buffer.is_empty() {
            content_blocks.push(ContentBlock::Text {
                text: self.streaming_buffer.clone(),
            });
        }
        for call in &tool_calls {
            content_blocks.push(ContentBlock::ToolUse {
                id: call.id.clone(),
                name: call.name.clone(),
                input: call.input.clone(),
            });
        }
        self.conversation_history.push(Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(content_blocks),
        });

        // Add to chat display
        let mut msg = ChatMessage::assistant(&self.streaming_buffer);
        msg.tool_calls = tool_calls
            .iter()
            .map(|c| ToolCallDisplay {
                name: c.name.clone(),
                summary: "executing...".to_string(),
                expanded: false,
                full_result: None,
            })
            .collect();
        self.app.state_mut().chat_history.push(msg);
        self.streaming_buffer.clear();

        // Create tool context
        let ctx = ToolContext::new(self.working_dir.clone(), "chat".to_string());

        // Execute tools synchronously in the main loop context (not ideal but works for now)
        let executor = &self.tool_executor;
        let mut tool_results = Vec::new();

        for call in &tool_calls {
            // We need to execute tools - for now, use a blocking approach
            // In a proper implementation, we'd spawn this and poll
            let result = futures::executor::block_on(executor.execute(call, &ctx));

            // Update the display
            if let Some(last_msg) = self.app.state_mut().chat_history.last_mut()
                && let Some(tc) = last_msg.tool_calls.iter_mut().find(|t| t.name == call.name)
            {
                tc.summary = if result.is_error {
                    format!("error: {}", truncate_result(&result.content, 50))
                } else {
                    truncate_result(&result.content, 50)
                };
                tc.full_result = Some(result.content.clone());
            }

            tool_results.push((call.id.clone(), result));
        }

        // Add tool results to conversation history
        let result_blocks: Vec<ContentBlock> = tool_results
            .iter()
            .map(|(id, result)| ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: result.content.clone(),
                is_error: result.is_error,
            })
            .collect();

        self.conversation_history.push(Message {
            role: Role::User,
            content: MessageContent::Blocks(result_blocks),
        });

        // Continue the conversation with tool results
        // We need to start a new LLM request, but we're in a sync context
        // Set a flag to do this in the next async iteration
        self.app.state_mut().chat_streaming = true;

        // Start new LLM request with tool results
        if let Some(llm) = &self.llm_client {
            let tools = self.tool_executor.definitions();
            let request = CompletionRequest {
                system_prompt: "You are a helpful AI assistant integrated into Loopr, a task management and automation tool. You have access to tools for reading files, running commands, and searching. Be concise and helpful.".to_string(),
                messages: self.conversation_history.clone(),
                tools,
                max_tokens: 4096,
            };

            let (tx, rx) = mpsc::channel(100);
            self.stream_rx = Some(rx);

            let llm = Arc::clone(llm);
            tokio::spawn(async move {
                if let Err(e) = llm.stream(request, tx).await {
                    error!("LLM streaming error after tool execution: {}", e);
                }
            });

            info!("Continued LLM streaming after tool execution");
        }
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

/// Truncate a result string for display.
fn truncate_result(s: &str, max_len: usize) -> String {
    let s = s.trim();
    // Take first line only
    let first_line = s.lines().next().unwrap_or("");
    if first_line.len() <= max_len {
        first_line.to_string()
    } else {
        format!("{}...", &first_line[..max_len])
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
