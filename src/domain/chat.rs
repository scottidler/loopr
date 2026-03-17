use serde::{Deserialize, Serialize};

use crate::tools::types::Message;

// --- Funnel state ---

/// Funnel state for Chat sessions. Drives system prompt selection and TUI UX.
/// The TUI owns the state machine (transitions driven by /plan, /draft, /accept).
/// The daemon receives it with each chat.submit to select the right system prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FunnelState {
    /// Free conversation.
    Chat,
    /// Coordinator asking clarifying questions.
    Interview,
    /// Draft plan proposed, awaiting user review.
    PlanDraft,
    /// Plan accepted, automation running.
    Executing,
}

/// Build the system prompt for a Chat session based on funnel state.
/// Prompts are loaded from .pmt files via the PromptStore.
/// When in Executing state, `orchestration_status` provides a live summary
/// of the orchestration pipeline (built by the caller from Stores).
pub fn system_prompt_for_chat(
    funnel_state: FunnelState,
    is_draft_request: bool,
    orchestration_status: Option<&str>,
) -> String {
    let store = crate::prompts::store();
    match funnel_state {
        FunnelState::Chat => store.chat.clone(),
        FunnelState::Interview => {
            format!("{}\n\n{}", store.chat, store.chat_interview)
        }
        FunnelState::PlanDraft => {
            if is_draft_request {
                format!("{}\n\n{}", store.chat, store.chat_draft)
            } else {
                format!("{}\n\n{}", store.chat, store.chat_refine)
            }
        }
        FunnelState::Executing => {
            let status = orchestration_status.unwrap_or("(no status available)");
            format!("{}\n\n{}{}", store.chat, store.chat_executing, status)
        }
    }
}

// --- Chat history persistence ---

/// Persisted chat conversation. One record per chat session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatHistory {
    pub session_id: String,
    pub messages: Vec<Message>,
    pub funnel_state: FunnelState,
    /// The coordinator goal_id associated with this chat session's execution.
    /// Set when /accept transitions to Executing state.
    #[serde(default)]
    pub goal_id: Option<String>,
    pub updated_at: i64,
}

impl ChatHistory {
    pub fn new(session_id: String) -> Self {
        Self {
            session_id,
            messages: Vec::new(),
            funnel_state: FunnelState::Chat,
            goal_id: None,
            updated_at: chrono::Utc::now().timestamp_millis(),
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    fn ensure_prompts() {
        crate::prompts::init_defaults();
    }

    #[test]
    fn test_system_prompt_chat() {
        ensure_prompts();
        let prompt = system_prompt_for_chat(FunnelState::Chat, false, None);
        assert!(prompt.contains("AI assistant"));
        assert!(!prompt.contains("clarifying questions"));
        assert!(prompt.contains("MULTIPLE tools"));
        assert!(prompt.contains("3 tool iterations"));
        assert!(prompt.contains("delegate"));
    }

    #[test]
    fn test_system_prompt_interview() {
        ensure_prompts();
        let prompt = system_prompt_for_chat(FunnelState::Interview, false, None);
        assert!(prompt.contains("clarifying questions"));
    }

    #[test]
    fn test_system_prompt_plan_draft_request() {
        ensure_prompts();
        let prompt = system_prompt_for_chat(FunnelState::PlanDraft, true, None);
        assert!(prompt.contains("structured plan"));
    }

    #[test]
    fn test_system_prompt_plan_draft_refine() {
        ensure_prompts();
        let prompt = system_prompt_for_chat(FunnelState::PlanDraft, false, None);
        assert!(prompt.contains("refine the plan"));
    }

    #[test]
    fn test_system_prompt_executing_with_status() {
        ensure_prompts();
        let prompt = system_prompt_for_chat(FunnelState::Executing, false, Some("Works: 3 active"));
        assert!(prompt.contains("orchestration pipeline"));
        assert!(prompt.contains("/pause"));
        assert!(prompt.contains("/stop"));
        assert!(prompt.contains("Works: 3 active"));
    }

    #[test]
    fn test_system_prompt_executing_no_status() {
        ensure_prompts();
        let prompt = system_prompt_for_chat(FunnelState::Executing, false, None);
        assert!(prompt.contains("orchestration pipeline"));
        assert!(prompt.contains("no status available"));
    }

    #[test]
    fn test_funnel_state_serde_roundtrip() {
        let state = FunnelState::PlanDraft;
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(json, "\"plan_draft\"");
        let back: FunnelState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, state);
    }

    #[test]
    fn test_chat_history_new() {
        let history = ChatHistory::new("default-chat".to_string());
        assert_eq!(history.session_id, "default-chat");
        assert!(history.messages.is_empty());
        assert_eq!(history.funnel_state, FunnelState::Chat);
        assert!(history.goal_id.is_none());
    }

    #[test]
    fn test_chat_history_goal_id_serde_roundtrip() {
        let mut history = ChatHistory::new("test-session".to_string());
        history.goal_id = Some("cg-abc12".to_string());
        let json = serde_json::to_string(&history).unwrap();
        let back: ChatHistory = serde_json::from_str(&json).unwrap();
        assert_eq!(back.goal_id, Some("cg-abc12".to_string()));
    }

    #[test]
    fn test_chat_history_goal_id_default_on_missing() {
        // Backwards compatibility: old ChatHistory JSON without goal_id
        let json = r#"{"session_id":"s","messages":[],"funnel_state":"chat","updated_at":0}"#;
        let history: ChatHistory = serde_json::from_str(json).unwrap();
        assert!(history.goal_id.is_none());
    }
}
