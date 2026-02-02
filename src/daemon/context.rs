//! Daemon context - shared state for request handlers
//!
//! DaemonContext owns all the components needed for daemon operations:
//! loop management, LLM client, tool execution, and event broadcasting.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::{RwLock, broadcast};

use crate::coordination::SignalManager;
use crate::error::Result;
use crate::ipc::messages::DaemonEvent;
use crate::llm::{AnthropicClient, AnthropicConfig, LlmClient, Message};
use crate::manager::{LoopManager, LoopManagerConfig};
use crate::storage::JsonlStorage;
use crate::tools::{LocalToolRouter, ToolCatalog};
use crate::validation::CompositeValidator;
use crate::worktree::WorktreeManager;

/// Chat session state for the chat view
#[derive(Debug, Default)]
pub struct ChatSession {
    /// Conversation history
    pub messages: Vec<Message>,
    /// Accumulated input tokens
    pub total_input_tokens: u64,
    /// Accumulated output tokens
    pub total_output_tokens: u64,
}

impl ChatSession {
    /// Create a new empty chat session
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a user message
    pub fn add_user_message(&mut self, content: &str) {
        self.messages.push(Message::user(content));
    }

    /// Add an assistant message
    pub fn add_assistant_message(&mut self, content: &str) {
        self.messages.push(Message::assistant(content));
    }

    /// Update token counts
    pub fn add_tokens(&mut self, input: u64, output: u64) {
        self.total_input_tokens += input;
        self.total_output_tokens += output;
    }

    /// Clear the session
    pub fn clear(&mut self) {
        self.messages.clear();
        self.total_input_tokens = 0;
        self.total_output_tokens = 0;
    }

    /// Get message count
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }
}

/// Type alias for the concrete LoopManager used by the daemon
pub type DaemonLoopManager = LoopManager<JsonlStorage, AnthropicClient, LocalToolRouter, CompositeValidator>;

/// Shared context for all daemon request handlers
pub struct DaemonContext {
    /// Loop lifecycle management
    pub loop_manager: Arc<RwLock<DaemonLoopManager>>,
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

impl DaemonContext {
    /// Create a new DaemonContext with all components initialized
    pub fn new(data_dir: &Path) -> Result<Self> {
        // Create storage
        let storage = Arc::new(JsonlStorage::new(data_dir.join(".taskstore"))?);

        // Create LLM client
        let llm_client = Arc::new(AnthropicClient::new(AnthropicConfig::default())?);

        // Create tool router with default catalog
        let catalog = ToolCatalog::default();
        let tool_router = Arc::new(LocalToolRouter::new(catalog));

        // Create validator
        let validator = Arc::new(CompositeValidator::default());

        // Create worktree manager
        let worktree_base = data_dir.join("worktrees");
        let repo_root = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let worktree_manager = Arc::new(WorktreeManager::new(worktree_base, repo_root));

        // Create signal manager
        let signal_manager = Arc::new(SignalManager::new(storage.clone()));

        // Create loop manager config
        let config = LoopManagerConfig {
            prompts_dir: data_dir.join("prompts"),
            repo_root: std::env::current_dir().unwrap_or_else(|_| ".".into()),
            ..Default::default()
        };

        // Create loop manager
        let loop_manager = Arc::new(RwLock::new(LoopManager::new(
            storage.clone(),
            llm_client.clone(),
            tool_router.clone(),
            validator,
            worktree_manager,
            signal_manager,
            config,
        )));

        // Create event broadcast channel
        let (event_tx, _) = broadcast::channel(256);

        Ok(Self {
            loop_manager,
            llm_client,
            tool_router,
            event_tx,
            chat_session: Arc::new(RwLock::new(ChatSession::new())),
            storage,
        })
    }

    /// Broadcast an event to all connected clients
    pub fn broadcast(&self, event: DaemonEvent) {
        // Ignore send errors (no subscribers is fine)
        let _ = self.event_tx.send(event);
    }

    /// Get a receiver for events
    pub fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.event_tx.subscribe()
    }

    /// Check if the LLM client is ready
    pub fn llm_ready(&self) -> bool {
        self.llm_client.is_ready()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_chat_session_new() {
        let session = ChatSession::new();
        assert!(session.messages.is_empty());
        assert_eq!(session.total_input_tokens, 0);
        assert_eq!(session.total_output_tokens, 0);
    }

    #[test]
    fn test_chat_session_add_messages() {
        let mut session = ChatSession::new();
        session.add_user_message("Hello");
        session.add_assistant_message("Hi there!");

        assert_eq!(session.message_count(), 2);
    }

    #[test]
    fn test_chat_session_add_tokens() {
        let mut session = ChatSession::new();
        session.add_tokens(100, 50);
        session.add_tokens(200, 100);

        assert_eq!(session.total_input_tokens, 300);
        assert_eq!(session.total_output_tokens, 150);
    }

    #[test]
    fn test_chat_session_clear() {
        let mut session = ChatSession::new();
        session.add_user_message("Hello");
        session.add_tokens(100, 50);
        session.clear();

        assert!(session.messages.is_empty());
        assert_eq!(session.total_input_tokens, 0);
        assert_eq!(session.total_output_tokens, 0);
    }

    #[test]
    fn test_daemon_context_new_without_api_key() {
        // This test will fail if ANTHROPIC_API_KEY is not set, which is expected
        let temp_dir = TempDir::new().unwrap();
        let result = DaemonContext::new(temp_dir.path());

        // The result depends on whether ANTHROPIC_API_KEY is set
        // In CI without the key, this should error
        // In local dev with the key, this should succeed
        if std::env::var("ANTHROPIC_API_KEY").is_ok() {
            assert!(result.is_ok());
        } else {
            assert!(result.is_err());
        }
    }
}
