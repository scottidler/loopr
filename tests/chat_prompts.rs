//! Integration tests: Chat system prompt selection and LLM smoke tests
//!
//! Unit tests verify `system_prompt_for_chat` produces correct prompt
//! augmentation for each `FunnelState`. LLM smoke tests (single-shot,
//! `#[ignore]`) verify prompts steer the LLM correctly via `chat.submit`.

#![allow(clippy::unwrap_used)]

mod common;

use std::collections::VecDeque;
use std::time::Duration;

use eyre::{Result, eyre};
use loopr::domain::chat::{FunnelState, system_prompt_for_chat};
use loopr::ipc::client::IpcClient;
use loopr::ipc::protocol::{DaemonEvent, IpcMessage};
use serde_json::json;

use common::{DaemonHandle, TempTestDir, test_id};

// ---------------------------------------------------------------------------
// Prompt unit tests (deterministic, no LLM, no daemon)
// ---------------------------------------------------------------------------

fn ensure_prompts() {
    loopr::prompts::init_defaults();
}

/// Chat state returns a non-empty base prompt.
#[test]
fn test_prompt_chat_base() {
    ensure_prompts();
    let prompt = system_prompt_for_chat(FunnelState::Chat, false, None);
    assert!(!prompt.is_empty());
    assert!(
        prompt.contains("AI assistant"),
        "base chat prompt should mention AI assistant role"
    );
}

/// Interview state augments with interview-specific guidance.
#[test]
fn test_prompt_interview_augmented() {
    ensure_prompts();
    let prompt = system_prompt_for_chat(FunnelState::Interview, false, None);
    assert!(
        prompt.contains("clarifying questions"),
        "interview prompt should contain 'clarifying questions'"
    );
}

/// PlanDraft with is_draft_request=true includes draft-specific text.
#[test]
fn test_prompt_draft_augmented() {
    ensure_prompts();
    let prompt = system_prompt_for_chat(FunnelState::PlanDraft, true, None);
    assert!(
        prompt.contains("structured plan"),
        "draft prompt should contain 'structured plan'"
    );
}

/// PlanDraft with is_draft_request=false includes refine-specific text.
#[test]
fn test_prompt_refine_augmented() {
    ensure_prompts();
    let prompt = system_prompt_for_chat(FunnelState::PlanDraft, false, None);
    assert!(
        prompt.contains("refine the plan"),
        "refine prompt should contain 'refine the plan'"
    );
}

/// Executing state includes the orchestration status text.
#[test]
fn test_prompt_executing_includes_status() {
    ensure_prompts();
    let status = "Plan: active | Specs: 3 | Works: 5 running";
    let prompt = system_prompt_for_chat(FunnelState::Executing, false, Some(status));
    assert!(
        prompt.contains(status),
        "executing prompt should include the status text verbatim"
    );
}

// ---------------------------------------------------------------------------
// LLM smoke test helpers
// ---------------------------------------------------------------------------

/// Wait for the `agent.llm-output` event with `is-final: true`.
async fn wait_for_final(client: &mut IpcClient, buffered: &mut VecDeque<DaemonEvent>, timeout: Duration) -> Result<()> {
    tokio::time::timeout(timeout, async {
        loop {
            let event = if let Some(e) = buffered.pop_front() {
                e
            } else {
                match client.recv().await? {
                    Some(IpcMessage::Event(e)) => e,
                    Some(IpcMessage::Response(_)) => continue,
                    None => return Err(eyre!("daemon disconnected while waiting for llm-output")),
                }
            };

            if event.event == "agent.llm-output" && event.data["is-final"].as_bool() == Some(true) {
                return Ok(());
            }
        }
    })
    .await
    .map_err(|_| eyre!("timed out waiting for agent.llm-output(is-final=true)"))?
}

/// Extract the last assistant message text from chat.history messages array.
fn extract_last_assistant_text(messages: &serde_json::Value) -> Option<String> {
    messages
        .as_array()?
        .iter()
        .rev()
        .find(|m| m["role"].as_str() == Some("assistant"))
        .and_then(|m| {
            m["content"].as_array().and_then(|blocks| {
                blocks
                    .iter()
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .first()
                    .map(|s| s.to_string())
            })
        })
}

// ---------------------------------------------------------------------------
// LLM smoke tests (single-shot, require ANTHROPIC_API_KEY)
// ---------------------------------------------------------------------------

/// Interview mode: LLM should ask questions, not produce code.
///
/// Sends a single `chat.submit` with funnel_state="interview" and verifies:
/// - Response does NOT contain code fences (``` ``` ```)
/// - Response contains at least one question mark
#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY; run with: cargo test --test chat_prompts -- --ignored"]
async fn test_chat_interview_asks_questions() {
    let temp = TempTestDir::new("loopr-chat-prompts");
    let daemon = DaemonHandle::spawn(temp).await.unwrap();
    let mut client = IpcClient::connect(&daemon.socket_path).await.unwrap();
    let mut buffered: VecDeque<DaemonEvent> = VecDeque::new();

    let session_id = format!("smoke-interview-{}", test_id());
    let timeout = Duration::from_secs(60);

    let (resp, events) = client
        .request(
            "chat.submit",
            json!({
                "session_id": session_id,
                "message": "Build a web app",
                "funnel_state": "interview",
            }),
        )
        .await
        .unwrap();
    assert!(!resp.is_error(), "chat.submit failed: {:?}", resp.error);
    buffered.extend(events);
    wait_for_final(&mut client, &mut buffered, timeout).await.unwrap();

    let (history_resp, _) = client
        .request("chat.history", json!({ "session_id": session_id }))
        .await
        .unwrap();
    let result = history_resp.result.unwrap();
    let text = extract_last_assistant_text(&result["messages"]).expect("no assistant message found in chat history");

    assert!(
        !text.contains("```"),
        "interview response should not contain code fences; got: {text}"
    );
    assert!(
        text.contains('?'),
        "interview response should ask at least one question; got: {text}"
    );
}

/// Draft mode: LLM should produce a structured plan.
///
/// Sends a `chat.submit` with interview-like history followed by a draft
/// request, and verifies the response has structure.
#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY; run with: cargo test --test chat_prompts -- --ignored"]
async fn test_chat_draft_produces_structure() {
    let temp = TempTestDir::new("loopr-chat-prompts");
    let daemon = DaemonHandle::spawn(temp).await.unwrap();
    let mut client = IpcClient::connect(&daemon.socket_path).await.unwrap();
    let mut buffered: VecDeque<DaemonEvent> = VecDeque::new();

    let session_id = format!("smoke-draft-{}", test_id());
    let timeout = Duration::from_secs(60);

    // Simulate interview context: goal + one clarification
    let (resp, events) = client
        .request(
            "chat.submit",
            json!({
                "session_id": session_id,
                "message": "Build a Rust CLI todo app with SQLite persistence",
                "funnel_state": "interview",
            }),
        )
        .await
        .unwrap();
    assert!(!resp.is_error(), "chat.submit (interview) failed: {:?}", resp.error);
    buffered.extend(events);
    wait_for_final(&mut client, &mut buffered, timeout).await.unwrap();

    let (resp, events) = client
        .request(
            "chat.submit",
            json!({
                "session_id": session_id,
                "message": "I want add, list, and done subcommands. A single local SQLite file is fine.",
                "funnel_state": "interview",
            }),
        )
        .await
        .unwrap();
    assert!(!resp.is_error(), "chat.submit (clarification) failed: {:?}", resp.error);
    buffered.extend(events);
    wait_for_final(&mut client, &mut buffered, timeout).await.unwrap();

    // Request draft plan
    let (resp, events) = client
        .request(
            "chat.submit",
            json!({
                "session_id": session_id,
                "message": "Please draft a plan based on our discussion.",
                "funnel_state": "plan_draft",
                "is_draft_request": true,
            }),
        )
        .await
        .unwrap();
    assert!(!resp.is_error(), "chat.submit (draft) failed: {:?}", resp.error);
    buffered.extend(events);
    wait_for_final(&mut client, &mut buffered, timeout).await.unwrap();

    let (history_resp, _) = client
        .request("chat.history", json!({ "session_id": session_id }))
        .await
        .unwrap();
    let result = history_resp.result.unwrap();
    let text = extract_last_assistant_text(&result["messages"]).expect("no assistant message found in chat history");

    let lower = text.to_lowercase();

    // Should have structure: heading or numbered list
    let has_heading = text.contains('#');
    let has_numbered_list = text.contains("1.") || text.contains("1)");
    assert!(
        has_heading || has_numbered_list,
        "draft response should contain a heading (#) or numbered list; got: {text}"
    );

    // Should mention acceptance criteria or deliverables
    let has_criteria = lower.contains("acceptance criteria") || lower.contains("deliverables");
    assert!(
        has_criteria,
        "draft response should mention 'acceptance criteria' or 'deliverables'; got: {text}"
    );
}
