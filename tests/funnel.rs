//! Integration tests: Chat Funnel Persona Matrix
//!
//! Drive scripted persona conversations through the chat funnel via
//! `chat.submit` IPC with driver-controlled `funnel_state` transitions.
//!
//! All LLM-driven tests are `#[ignore]` - they require a live ANTHROPIC_API_KEY
//! and are slow (3-10 LLM calls each). Run them manually:
//!
//! ```sh
//! cargo test --test funnel -- --ignored
//! ```
//!
//! Timeout is configurable via `LOOPR_TEST_TIMEOUT_SECS` (default: 120).
//!
//! # IPC Flow (new chat funnel path)
//!
//! 1. `chat.submit(funnel_state="interview")` - LLM asks clarifying questions
//! 2. Repeat with persona responses via `chat.submit(funnel_state="interview")`
//! 3. `chat.submit(funnel_state="plan_draft", is_draft_request=true)` - LLM drafts plan
//! 4. `coordinator.accept_plan(plan=plan_text)` - bridge creates Plan + Goal + starts Coordinator

#![allow(clippy::unwrap_used)]

mod common;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

use eyre::{Result, eyre};
use loopr::ipc::client::IpcClient;
use loopr::ipc::protocol::{DaemonEvent, IpcMessage};
use reqwest::Client;
use serde_json::json;

use common::{DaemonHandle, TempTestDir, test_id};

// ---------------------------------------------------------------------------
// Persona fixture
// ---------------------------------------------------------------------------

/// A scripted interview persona with hardcoded turn-indexed responses.
///
/// The `run_chat_persona` driver feeds `responses[turn]` as the answer to each
/// interview turn via `chat.submit`, falling back to `fallback` once
/// the scripted responses are exhausted.
pub struct PersonaFixture {
    /// Human-readable name used in diagnostics and failure messages.
    pub name: &'static str,
    /// Initial goal text sent as the first `chat.submit` message.
    pub initial_goal: &'static str,
    /// Turn-indexed answers: `responses[0]` answers the first LLM question, etc.
    pub responses: &'static [&'static str],
    /// Answer used for every question once `responses` is exhausted.
    pub fallback: &'static str,
    /// Tier 1 structural assertion: all keywords must appear in plan text (case-insensitive).
    pub required_keywords: &'static [&'static str],
    /// Tier 2 LLM evaluator rubric (nightly only, opt-in via `LOOPR_TIER2_EVAL=1`).
    ///
    /// A short description of what the resulting plan must demonstrate to pass.
    /// If `None`, Tier 2 is skipped for this persona.
    pub rubric: Option<&'static str>,
}

/// Golden Path: a clear, well-scoped request that the Coordinator should
/// convert to a coherent Draft Plan with minimal interview friction.
pub const GOLDEN_PATH: PersonaFixture = PersonaFixture {
    name: "Golden Path",
    initial_goal: "Build a CLI todo app in Rust with SQLite persistence",
    responses: &[
        "Yes, I want add, list, and done subcommands.",
        "A single local SQLite file is fine. No sync needed.",
        "That plan looks good to me.",
    ],
    fallback: "That sounds fine, go ahead.",
    required_keywords: &["rust", "sqlite"],
    rubric: Some(
        "The plan must scope a Rust CLI todo app with SQLite persistence, \
         covering add, list, and done subcommands. It should not propose a \
         web app, database server, or sync mechanism.",
    ),
};

/// Vague User: an underspecified goal that requires the Coordinator to ask
/// clarifying questions and extract a scoped, actionable plan.
pub const VAGUE_USER: PersonaFixture = PersonaFixture {
    name: "Vague User",
    initial_goal: "Build something to manage my stuff.",
    responses: &[
        "I mean like tasks - things I need to do.",
        "CLI would be fine, nothing fancy.",
        "Rust would be great if that's easy.",
    ],
    fallback: "Just keep it simple, whatever you think is best.",
    required_keywords: &["cli", "task"],
    rubric: Some(
        "The plan must scope a CLI task manager derived from the user's \
         clarifications. It must not be a web app or a vague 'stuff manager'. \
         The scope should be specific and actionable.",
    ),
};

/// Scope Creeper: starts with a simple goal then redirects to a more complex architecture.
///
/// The Coordinator must track the user's evolving requirements (Postgres, Redis) and
/// produce a plan that reflects the redirected architecture, not the original CSV-only request.
pub const SCOPE_CREEPER: PersonaFixture = PersonaFixture {
    name: "Scope Creeper",
    initial_goal: "Build a simple Python CLI to read a CSV.",
    responses: &[
        "Actually, make it output to a Postgres database instead of CSV/JSON.",
        "I'm not sure about the DB schema, just figure it out. Also add Redis caching.",
        "That plan looks right to me.",
    ],
    fallback: "Just do whatever you think is best.",
    required_keywords: &["postgres", "redis", "python"],
    rubric: Some(
        "The plan must reflect the user's evolved requirements toward a \
         Postgres/Redis architecture. The original CSV-only request must not \
         dominate the plan - the user explicitly redirected toward a database backend.",
    ),
};

/// Pushback User: rejects the Coordinator's initial direction and redirects to a different stack.
///
/// The Coordinator must handle rejection gracefully, incorporate the new direction,
/// and still produce a well-scoped plan within a reasonable number of turns.
pub const PUSHBACK_USER: PersonaFixture = PersonaFixture {
    name: "Pushback User",
    initial_goal: "Build a web app for tracking expenses.",
    responses: &[
        "No, I don't want a web app. I'd rather have a CLI tool.",
        "Right - just a terminal CLI. No browser, no server.",
        "Yes, that looks good.",
    ],
    fallback: "Keep it simple, whatever you think is best.",
    required_keywords: &["cli", "expense"],
    rubric: Some(
        "The plan must reflect the user's redirect from a web app to a CLI tool \
         for expense tracking. A web app, browser, or server must not appear \
         as the primary deliverable.",
    ),
};

/// Silent User: provides no scripted responses - every answer falls back to the generic fallback.
///
/// The Coordinator must not deadlock or loop indefinitely when the user is unresponsive.
/// It must reach ProposePlan within the timeout using only the initial goal and its own judgment.
pub const SILENT_USER: PersonaFixture = PersonaFixture {
    name: "Silent User",
    initial_goal: "Build a tool.",
    responses: &[],
    fallback: "I don't know, you decide.",
    required_keywords: &["tool"],
    rubric: Some(
        "The plan must be a coherent, self-contained scope derived entirely \
         from the initial goal. It must not deadlock or produce an empty plan. \
         The Coordinator should make reasonable assumptions without user input.",
    ),
};

// ---------------------------------------------------------------------------
// Chat persona driver
// ---------------------------------------------------------------------------

/// Wait for the `agent.llm_output` event with `is_final: true` from the daemon,
/// draining any buffered events first.
async fn wait_for_final(client: &mut IpcClient, buffered: &mut VecDeque<DaemonEvent>, timeout: Duration) -> Result<()> {
    tokio::time::timeout(timeout, async {
        loop {
            let event = if let Some(e) = buffered.pop_front() {
                e
            } else {
                match client.recv().await? {
                    Some(IpcMessage::Event(e)) => e,
                    Some(IpcMessage::Response(_)) => continue,
                    None => return Err(eyre!("daemon disconnected while waiting for llm_output")),
                }
            };

            if event.event == "agent.llm_output" && event.data["is_final"].as_bool() == Some(true) {
                return Ok(());
            }
        }
    })
    .await
    .map_err(|_| eyre!("timed out waiting for agent.llm_output(is_final=true)"))?
}

/// Extract the last assistant message text from chat.history response.
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

/// Drive a `PersonaFixture` through the chat funnel via IPC.
///
/// Uses `chat.submit` with driver-controlled `funnel_state` transitions:
/// 1. Submit initial goal with `funnel_state: "interview"`
/// 2. For each scripted response, submit with `funnel_state: "interview"`
/// 3. Submit draft request with `funnel_state: "plan_draft", is_draft_request: true`
/// 4. Extract plan text from last assistant message
/// 5. Call `coordinator.accept_plan` with the plan text
///
/// Returns the plan ID from the accept_plan response.
async fn run_chat_persona(socket_path: &PathBuf, fixture: &PersonaFixture) -> Result<String> {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        return Err(eyre!("ANTHROPIC_API_KEY must be set to run persona tests"));
    }

    let session_id = format!("persona-{}", test_id());
    let mut client = IpcClient::connect(socket_path).await?;
    let mut buffered: VecDeque<DaemonEvent> = VecDeque::new();

    let timeout_secs = std::env::var("LOOPR_TEST_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(120);
    let timeout = Duration::from_secs(timeout_secs);

    // Step 1: Submit initial goal in interview state
    let (resp, events) = client
        .request(
            "chat.submit",
            json!({
                "session_id": session_id,
                "message": fixture.initial_goal,
                "funnel_state": "interview",
            }),
        )
        .await
        .map_err(|e| eyre!("chat.submit (initial) failed: {e}"))?;
    if resp.is_error() {
        return Err(eyre!("chat.submit (initial) returned error: {:?}", resp.error));
    }
    buffered.extend(events);
    wait_for_final(&mut client, &mut buffered, timeout).await?;

    // Step 2: Submit each scripted response in interview state
    for (i, response) in fixture.responses.iter().enumerate() {
        let (resp, events) = client
            .request(
                "chat.submit",
                json!({
                    "session_id": session_id,
                    "message": *response,
                    "funnel_state": "interview",
                }),
            )
            .await
            .map_err(|e| eyre!("chat.submit (response {i}) failed: {e}"))?;
        if resp.is_error() {
            return Err(eyre!("chat.submit (response {i}) returned error: {:?}", resp.error));
        }
        buffered.extend(events);
        wait_for_final(&mut client, &mut buffered, timeout).await?;
    }

    // Step 3: Request plan draft
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
        .map_err(|e| eyre!("chat.submit (draft) failed: {e}"))?;
    if resp.is_error() {
        return Err(eyre!("chat.submit (draft) returned error: {:?}", resp.error));
    }
    buffered.extend(events);
    wait_for_final(&mut client, &mut buffered, timeout).await?;

    // Step 4: Extract plan text from chat history
    let (history_resp, _) = client
        .request("chat.history", json!({ "session_id": session_id }))
        .await
        .map_err(|e| eyre!("chat.history failed: {e}"))?;
    if history_resp.is_error() {
        return Err(eyre!("chat.history returned error: {:?}", history_resp.error));
    }

    let result = history_resp
        .result
        .ok_or_else(|| eyre!("persona '{}': chat.history returned null result", fixture.name))?;
    let plan_text = extract_last_assistant_text(&result["messages"])
        .ok_or_else(|| eyre!("persona '{}': no assistant message found in chat history", fixture.name))?;

    // Step 5: Accept the plan
    let (accept_resp, _) = client
        .request("coordinator.accept_plan", json!({ "plan": plan_text }))
        .await
        .map_err(|e| eyre!("coordinator.accept_plan failed: {e}"))?;
    if accept_resp.is_error() {
        return Err(eyre!(
            "persona '{}': coordinator.accept_plan returned error: {:?}",
            fixture.name,
            accept_resp.error,
        ));
    }

    let plan_id = accept_resp
        .result
        .as_ref()
        .and_then(|r| r["plan_id"].as_str())
        .ok_or_else(|| eyre!("persona '{}': accept_plan response missing plan_id", fixture.name))?
        .to_string();

    Ok(plan_id)
}

// ---------------------------------------------------------------------------
// Tier 1 structural assertion runner
// ---------------------------------------------------------------------------

/// Fetch the plan via `plan.get` and run Tier 1 structural assertions:
///
/// - Plan record exists and is retrievable
/// - `title` is non-empty
/// - `description` is non-empty
/// - All `fixture.required_keywords` appear in plan text (case-insensitive)
///
/// Returns the concatenated plan text (title + description + acceptance_criteria)
/// for optional use by the Tier 2 evaluator.
async fn assert_plan_tier1(socket_path: &PathBuf, plan_id: &str, fixture: &PersonaFixture) -> Result<String> {
    let mut client = IpcClient::connect(socket_path).await?;

    let (resp, _) = client
        .request("plan.get", json!({"id": plan_id}))
        .await
        .map_err(|e| eyre!("plan.get failed: {e}"))?;

    if resp.is_error() {
        return Err(eyre!(
            "persona '{}': plan.get returned error for id={}: {:?}",
            fixture.name,
            plan_id,
            resp.error,
        ));
    }

    let result = resp
        .result
        .ok_or_else(|| eyre!("persona '{}': plan.get returned null result", fixture.name))?;

    let title = result["title"].as_str().unwrap_or("");
    let description = result["description"].as_str().unwrap_or("");
    let acceptance_criteria = result["acceptance_criteria"].as_str().unwrap_or("");

    if title.is_empty() {
        return Err(eyre!("persona '{}': plan title is empty", fixture.name));
    }

    if description.is_empty() {
        return Err(eyre!("persona '{}': plan description is empty", fixture.name));
    }

    let plan_text = format!("{title} {description} {acceptance_criteria}");
    for keyword in fixture.required_keywords {
        if !plan_text.to_lowercase().contains(&keyword.to_lowercase()) {
            return Err(eyre!(
                "persona '{}': required keyword '{}' not found in plan (title={:?})",
                fixture.name,
                keyword,
                title,
            ));
        }
    }

    Ok(plan_text)
}

// ---------------------------------------------------------------------------
// Tier 2 LLM evaluator (nightly only - opt-in via LOOPR_TIER2_EVAL=1)
// ---------------------------------------------------------------------------

/// Structured verdict returned by the Tier 2 evaluator LLM.
#[derive(serde::Deserialize, Debug)]
struct EvalResult {
    passed: bool,
    reason: String,
}

/// Tier 2: single-shot LLM evaluator using `claude-haiku-4-5-20251001`.
///
/// Gated by `LOOPR_TIER2_EVAL=1`. If the env var is not set (or is not "1"),
/// returns `Ok(())` immediately so that the existing `#[ignore]` persona tests
/// remain Tier-1-only unless the caller explicitly opts in. The `otto
/// e2e-nightly` task sets this variable.
///
/// Uses a cheaper, independent model (haiku) rather than the Coordinator model
/// to reduce grading bias and cost. The model is given the plan text and the
/// persona rubric and asked for a binary JSON verdict.
async fn assert_plan_tier2(plan_text: &str, fixture: &PersonaFixture) -> Result<()> {
    // Opt-in gate: skip unless LOOPR_TIER2_EVAL=1
    if std::env::var("LOOPR_TIER2_EVAL").ok().as_deref() != Some("1") {
        return Ok(());
    }

    let rubric = match fixture.rubric {
        Some(r) => r,
        None => return Ok(()), // persona has no rubric - nothing to evaluate
    };

    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .map_err(|_| eyre!("ANTHROPIC_API_KEY not set (required for Tier 2 evaluation)"))?;

    let system_prompt = "You are a plan quality evaluator. \
        Given a draft plan and a rubric, decide whether the plan satisfies the rubric. \
        Respond with ONLY valid JSON in this exact format, no other text: \
        {\"passed\": true, \"reason\": \"one sentence\"} \
        or {\"passed\": false, \"reason\": \"one sentence\"}.";

    let user_message =
        format!("Rubric:\n{rubric}\n\nPlan:\n{plan_text}\n\nDoes this plan satisfy the rubric? JSON only.");

    let body = serde_json::json!({
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 256,
        "system": system_prompt,
        "messages": [{"role": "user", "content": user_message}],
    });

    let client = Client::new();
    let response = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", &api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| eyre!("Tier 2 HTTP request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let err_body = response.text().await.unwrap_or_default();
        return Err(eyre!("Tier 2 API error {status}: {err_body}"));
    }

    let resp_json: serde_json::Value = response
        .json()
        .await
        .map_err(|e| eyre!("Tier 2 response parse failed: {e}"))?;

    let text = resp_json["content"][0]["text"]
        .as_str()
        .ok_or_else(|| eyre!("Tier 2: no text in response: {:?}", resp_json))?;

    let eval: EvalResult = serde_json::from_str(text.trim())
        .map_err(|e| eyre!("Tier 2: failed to parse evaluator JSON ({e}): {text:?}"))?;

    if !eval.passed {
        return Err(eyre!(
            "persona '{}': Tier 2 evaluator FAILED - {}",
            fixture.name,
            eval.reason,
        ));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Persona tests (ignored - require ANTHROPIC_API_KEY, 3-10 LLM calls each)
// ---------------------------------------------------------------------------

/// Golden Path: a well-scoped goal produces a coherent Draft Plan.
#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY and live LLM; run with: cargo test --test funnel -- --ignored"]
async fn test_persona_golden_path() {
    let temp_dir = TempTestDir::new("loopr-funnel");
    let daemon = DaemonHandle::spawn(temp_dir).await.unwrap();
    let plan_id = run_chat_persona(&daemon.socket_path, &GOLDEN_PATH).await.unwrap();
    assert!(
        !plan_id.is_empty(),
        "expected non-empty plan_id from Golden Path interview"
    );
    let plan_text = assert_plan_tier1(&daemon.socket_path, &plan_id, &GOLDEN_PATH)
        .await
        .unwrap();
    assert_plan_tier2(&plan_text, &GOLDEN_PATH).await.unwrap();
}

/// Vague User: an underspecified goal; LLM must extract a scoped plan.
#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY and live LLM; run with: cargo test --test funnel -- --ignored"]
async fn test_persona_vague_user() {
    let temp_dir = TempTestDir::new("loopr-funnel");
    let daemon = DaemonHandle::spawn(temp_dir).await.unwrap();
    let plan_id = run_chat_persona(&daemon.socket_path, &VAGUE_USER).await.unwrap();
    assert!(
        !plan_id.is_empty(),
        "expected non-empty plan_id from Vague User interview"
    );
    let plan_text = assert_plan_tier1(&daemon.socket_path, &plan_id, &VAGUE_USER)
        .await
        .unwrap();
    assert_plan_tier2(&plan_text, &VAGUE_USER).await.unwrap();
}

/// Scope Creeper: an evolving goal ending at Postgres + Redis architecture.
#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY and live LLM; run with: cargo test --test funnel -- --ignored"]
async fn test_persona_scope_creeper() {
    let temp_dir = TempTestDir::new("loopr-funnel");
    let daemon = DaemonHandle::spawn(temp_dir).await.unwrap();
    let plan_id = run_chat_persona(&daemon.socket_path, &SCOPE_CREEPER).await.unwrap();
    assert!(
        !plan_id.is_empty(),
        "expected non-empty plan_id from Scope Creeper interview"
    );
    let plan_text = assert_plan_tier1(&daemon.socket_path, &plan_id, &SCOPE_CREEPER)
        .await
        .unwrap();
    assert_plan_tier2(&plan_text, &SCOPE_CREEPER).await.unwrap();
}

/// Pushback User: initial web app request redirected to a CLI tool.
#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY and live LLM; run with: cargo test --test funnel -- --ignored"]
async fn test_persona_pushback_user() {
    let temp_dir = TempTestDir::new("loopr-funnel");
    let daemon = DaemonHandle::spawn(temp_dir).await.unwrap();
    let plan_id = run_chat_persona(&daemon.socket_path, &PUSHBACK_USER).await.unwrap();
    assert!(
        !plan_id.is_empty(),
        "expected non-empty plan_id from Pushback User interview"
    );
    let plan_text = assert_plan_tier1(&daemon.socket_path, &plan_id, &PUSHBACK_USER)
        .await
        .unwrap();
    assert_plan_tier2(&plan_text, &PUSHBACK_USER).await.unwrap();
}

/// Silent User: no scripted responses - every answer is the fallback string.
#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY and live LLM; run with: cargo test --test funnel -- --ignored"]
async fn test_persona_silent_user() {
    let temp_dir = TempTestDir::new("loopr-funnel");
    let daemon = DaemonHandle::spawn(temp_dir).await.unwrap();
    let plan_id = run_chat_persona(&daemon.socket_path, &SILENT_USER).await.unwrap();
    assert!(
        !plan_id.is_empty(),
        "expected non-empty plan_id from Silent User interview"
    );
    let plan_text = assert_plan_tier1(&daemon.socket_path, &plan_id, &SILENT_USER)
        .await
        .unwrap();
    assert_plan_tier2(&plan_text, &SILENT_USER).await.unwrap();
}

// ---------------------------------------------------------------------------
// Fast structural tests (always run - no LLM, no daemon required)
// ---------------------------------------------------------------------------

#[test]
fn test_persona_fixture_fields() {
    // SILENT_USER intentionally has zero scripted responses (all-fallback code path).
    // The invariant is: every persona must have a non-empty name, initial_goal, and fallback.
    for fixture in [&GOLDEN_PATH, &VAGUE_USER, &SCOPE_CREEPER, &PUSHBACK_USER, &SILENT_USER] {
        assert!(!fixture.name.is_empty(), "{}: name must not be empty", fixture.name);
        assert!(
            !fixture.initial_goal.is_empty(),
            "{}: initial_goal must not be empty",
            fixture.name
        );
        assert!(
            !fixture.fallback.is_empty(),
            "{}: fallback must not be empty",
            fixture.name
        );
    }
}

#[test]
fn test_persona_fixture_required_keywords() {
    for fixture in [&GOLDEN_PATH, &VAGUE_USER, &SCOPE_CREEPER, &PUSHBACK_USER, &SILENT_USER] {
        assert!(
            !fixture.required_keywords.is_empty(),
            "{}: required_keywords must not be empty - every persona needs at least one Tier 1 assertion",
            fixture.name,
        );
        for keyword in fixture.required_keywords {
            assert!(
                !keyword.is_empty(),
                "{}: required_keywords must not contain empty strings",
                fixture.name
            );
        }
    }
}

#[test]
fn test_tier1_keyword_matching() {
    let plan_text = "A Rust CLI tool with SQLite persistence for managing todos";
    for keyword in GOLDEN_PATH.required_keywords {
        assert!(
            plan_text.to_lowercase().contains(&keyword.to_lowercase()),
            "keyword '{}' should match in plan_text",
            keyword
        );
    }
}

#[test]
fn test_temp_test_dir_lifecycle() {
    let dir = TempTestDir::new("loopr-funnel-fixture");
    let path = dir.path().clone();
    assert!(path.exists(), "temp dir should exist after creation");
    drop(dir);
    assert!(!path.exists(), "temp dir should be removed on drop");
}
