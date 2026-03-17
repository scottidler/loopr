use serde::{Deserialize, Serialize};

use crate::tools::types::Message;

// --- System prompt constants ---

pub const CHAT_SYSTEM_PROMPT: &str = "\
You are an AI assistant embedded in the Loopr development orchestrator. \
You help the user explore ideas, discuss architecture, and plan changes to their codebase. \
When the user is ready to formalize a plan, they will type /plan.\n\n\
You have tools available: read, write, edit, grep, glob, find, list, tree, shell, search, fetch, and delegate.\n\n\
TOOL STRATEGY — READ CAREFULLY:\n\
- You can call MULTIPLE tools in a SINGLE response. If you need to read 5 files, \
  emit 5 read tool_use blocks in ONE response. They execute in parallel.\n\
- You have a MAXIMUM of 3 tool iterations. Each time you call tools counts as one. \
  Maximize every turn — batch ALL independent tool calls together.\n\
- Use `delegate` for tasks requiring more than 5 tool calls or deep multi-step research. \
  Delegate spawns a subagent with its own context window and 20 iterations.\n\
- Do NOT step through files one at a time. Do NOT retry failed searches sequentially.\n\
- Use `shell` for system commands when no built-in tool fits.\n\n\
Be concise and direct. Act on user requests immediately using tools — don't ask for permission.";

pub const INTERVIEW_PROMPT: &str = "You are helping the user coalesce around a concrete, actionable plan. \
Your job is to ask clarifying questions until the goal, scope, and acceptance criteria are clear. \
Do not propose a plan until the user signals they are ready by typing /draft. \
Focus on understanding the problem, constraints, and desired outcome.";

pub const DRAFT_PROMPT: &str = "The user is ready for a plan draft. Based on the conversation so far, \
produce a structured plan with: Title, Goal, Acceptance Criteria (numbered list), and Phases \
(if applicable). Output plain text, not markdown. Be concise.";

pub const PLAN_REFINE_PROMPT: &str = "The user wants to refine the plan draft. Apply their feedback and \
output the revised plan in the same format. Only change what they asked for.";

pub const EXECUTING_PROMPT: &str = "\
You are monitoring an active orchestration pipeline in Loopr. \
The Coordinator agent is decomposing a Plan into Specs, Phases, and Works, \
then assigning Implementer agents to write code in isolated worktrees.\n\n\
You can help the user understand progress, answer questions about the pipeline, \
and relay intervention commands. The orchestration status is included below.\n\n\
Available intervention commands the user can type:\n\
- /pause - Pause the Coordinator (implementations in progress will complete)\n\
- /stop - Stop all orchestration (cancel Coordinator and active agents)\n\
- /status - Show detailed orchestration status\n\n\
ORCHESTRATION STATUS:\n";

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
/// When in Executing state, `orchestration_status` provides a live summary
/// of the orchestration pipeline (built by the caller from Stores).
pub fn system_prompt_for_chat(
    funnel_state: FunnelState,
    is_draft_request: bool,
    orchestration_status: Option<&str>,
) -> String {
    match funnel_state {
        FunnelState::Chat => CHAT_SYSTEM_PROMPT.to_string(),
        FunnelState::Interview => {
            format!("{CHAT_SYSTEM_PROMPT}\n\n{INTERVIEW_PROMPT}")
        }
        FunnelState::PlanDraft => {
            if is_draft_request {
                format!("{CHAT_SYSTEM_PROMPT}\n\n{DRAFT_PROMPT}")
            } else {
                format!("{CHAT_SYSTEM_PROMPT}\n\n{PLAN_REFINE_PROMPT}")
            }
        }
        FunnelState::Executing => {
            let status = orchestration_status.unwrap_or("(no status available)");
            format!("{CHAT_SYSTEM_PROMPT}\n\n{EXECUTING_PROMPT}{status}")
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

    #[test]
    fn test_system_prompt_chat() {
        let prompt = system_prompt_for_chat(FunnelState::Chat, false, None);
        assert!(prompt.contains("AI assistant"));
        assert!(!prompt.contains("clarifying questions"));
        assert!(prompt.contains("MULTIPLE tools"));
        assert!(prompt.contains("3 tool iterations"));
        assert!(prompt.contains("delegate"));
    }

    #[test]
    fn test_system_prompt_interview() {
        let prompt = system_prompt_for_chat(FunnelState::Interview, false, None);
        assert!(prompt.contains("clarifying questions"));
    }

    #[test]
    fn test_system_prompt_plan_draft_request() {
        let prompt = system_prompt_for_chat(FunnelState::PlanDraft, true, None);
        assert!(prompt.contains("structured plan"));
    }

    #[test]
    fn test_system_prompt_plan_draft_refine() {
        let prompt = system_prompt_for_chat(FunnelState::PlanDraft, false, None);
        assert!(prompt.contains("refine the plan"));
    }

    #[test]
    fn test_system_prompt_executing_with_status() {
        let prompt = system_prompt_for_chat(FunnelState::Executing, false, Some("Works: 3 active"));
        assert!(prompt.contains("orchestration pipeline"));
        assert!(prompt.contains("/pause"));
        assert!(prompt.contains("/stop"));
        assert!(prompt.contains("Works: 3 active"));
    }

    #[test]
    fn test_system_prompt_executing_no_status() {
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
