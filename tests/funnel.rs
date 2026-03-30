//! Integration tests: Coordinator Interviewing FSM (Constrained Persona Matrix)
//!
//! Drive the Coordinator through its `Interviewing` FSM state using scripted,
//! turn-indexed persona responses fed over Unix socket JSON-RPC IPC.
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
//! # IPC Flow
//!
//! 1. `coordinator.set_goal`   - persists the goal (does NOT start the agent)
//! 2. `agent.start`            - spawns the Coordinator agent task
//! 3. `coordinator.interview_question` (event) - Coordinator asks a question
//! 4. `coordinator.interview_respond`  - driver sends turn-indexed answer
//! 5. Repeat 3-4 until `record.created` (collection: "plan") is received

#![allow(clippy::unwrap_used)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use eyre::{Result, eyre};
use loopr::config::{Config, DaemonConfig, ProjectConfig};
use loopr::daemon::context::DaemonContext;
use loopr::daemon::daemon_main;
use loopr::ipc::client::IpcClient;
use loopr::ipc::protocol::{DaemonEvent, IpcMessage};
use reqwest::Client;
use serde_json::json;
use tokio::task::JoinHandle;

// ---------------------------------------------------------------------------
// Unique ID helper (no external deps)
// ---------------------------------------------------------------------------

fn test_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{}", std::process::id(), n)
}

// ---------------------------------------------------------------------------
// Persona fixture
// ---------------------------------------------------------------------------

/// A scripted interview persona with hardcoded turn-indexed responses.
///
/// The `PersonaDriver` feeds `responses[turn]` as the answer to each
/// `coordinator.interview_question` event, falling back to `fallback` once
/// the scripted responses are exhausted.
pub struct PersonaFixture {
    /// Human-readable name used in diagnostics and failure messages.
    pub name: &'static str,
    /// Initial goal text sent via `coordinator.set_goal`.
    pub initial_goal: &'static str,
    /// Turn-indexed answers: `responses[0]` answers the first question, etc.
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
// Temp dir RAII
// ---------------------------------------------------------------------------

/// A temporary directory that is removed (with all contents) when dropped.
struct TempTestDir(PathBuf);

impl TempTestDir {
    fn new(prefix: &str) -> Self {
        let path = std::env::temp_dir().join(format!("{}-{}", prefix, test_id()));
        std::fs::create_dir_all(&path).expect("failed to create test temp dir");
        Self(path)
    }

    fn path(&self) -> &PathBuf {
        &self.0
    }
}

impl Drop for TempTestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// Test daemon handle
// ---------------------------------------------------------------------------

/// An in-process daemon spawned for integration testing.
///
/// Uses an isolated temp directory and socket path per test instance.
/// The daemon Tokio task is aborted when this handle is dropped.
///
/// **Note:** The supervisor task spawned inside `daemon_main` is not
/// automatically aborted. It will exit once its `Stores` Arc and broadcast
/// channel sender are released after the `DaemonContext` is dropped.
struct DaemonHandle {
    pub socket_path: PathBuf,
    task: JoinHandle<eyre::Result<()>>,
    _temp_dir: TempTestDir,
}

impl DaemonHandle {
    /// Spawn a fresh daemon in `temp_dir` and wait for its socket to appear.
    ///
    /// The config overrides socket_path, pid_path, and repo_path to the temp
    /// directory. `auto_start_coordinator` and pull-based workers remain off
    /// (Config::default) so the test drives startup explicitly via `agent.start`.
    async fn spawn(temp_dir: TempTestDir) -> Result<Self> {
        // Initialize prompts before starting daemon tasks
        loopr::prompts::init_defaults();

        let socket_path = temp_dir.path().join("daemon.sock");
        let pid_path = temp_dir.path().join("daemon.pid");
        let repo_path = temp_dir.path().join("repo");
        let session_dir = temp_dir.path().join("session");
        std::fs::create_dir_all(&repo_path)?;
        std::fs::create_dir_all(&session_dir)?;

        let config = Config {
            daemon: DaemonConfig {
                socket_path: socket_path.clone(),
                pid_path,
            },
            project: ProjectConfig {
                repo_path,
                ..ProjectConfig::default()
            },
            ..Config::default()
        };

        let session_id = format!("funnel-{}", test_id());
        let (ctx, _) = DaemonContext::shared(config, session_id, session_dir)?;
        let task = tokio::spawn(async move { daemon_main(ctx).await });

        // Wait for socket to appear (up to 5 seconds).
        for _ in 0..50 {
            if socket_path.exists() {
                return Ok(Self {
                    socket_path,
                    task,
                    _temp_dir: temp_dir,
                });
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        task.abort();
        Err(eyre!("daemon socket never appeared at {}", socket_path.display()))
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// ---------------------------------------------------------------------------
// Persona driver
// ---------------------------------------------------------------------------

/// Drive a `PersonaFixture` through the Coordinator interview via IPC.
///
/// Sends `coordinator.set_goal` + `agent.start`, then responds to each
/// `coordinator.interview_question` event with the turn-indexed answer from
/// `fixture.responses` (or `fixture.fallback` once exhausted).
///
/// Events collected as side-effects of `request()` calls are buffered and
/// processed before reading further from the socket, preventing dropped events
/// under race conditions where the Coordinator responds before the client's
/// `recv()` loop is entered.
///
/// Returns the plan ID from the `record.created` (collection: "plan") event.
async fn run_persona(socket_path: &PathBuf, fixture: &PersonaFixture) -> Result<String> {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        return Err(eyre!("ANTHROPIC_API_KEY must be set to run persona tests"));
    }

    let mut client = IpcClient::connect(socket_path).await?;
    let mut turn = 0usize;
    let mut buffered: VecDeque<DaemonEvent> = VecDeque::new();

    // Set goal. Does NOT start the agent - that requires a separate agent.start call.
    let (goal_resp, goal_events) = client
        .request("coordinator.set_goal", json!({"goal": fixture.initial_goal}))
        .await
        .map_err(|e| eyre!("coordinator.set_goal failed: {e}"))?;
    if goal_resp.is_error() {
        return Err(eyre!("coordinator.set_goal returned error: {:?}", goal_resp.error));
    }
    buffered.extend(goal_events);

    // Start the Coordinator agent. In sync test contexts auto-start is skipped
    // (handlers.rs:4218), so agent.start must always be called explicitly.
    let (start_resp, start_events) = client
        .request("agent.start", json!({"agent_type": "coordinator"}))
        .await
        .map_err(|e| eyre!("agent.start failed: {e}"))?;
    if start_resp.is_error() {
        return Err(eyre!("agent.start returned error: {:?}", start_resp.error));
    }
    buffered.extend(start_events);

    let timeout_secs = std::env::var("LOOPR_TEST_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(120);

    let plan_id = tokio::time::timeout(Duration::from_secs(timeout_secs), async {
        loop {
            // Drain buffered events (side-effects of request() calls) first.
            let event = if let Some(e) = buffered.pop_front() {
                e
            } else {
                match client.recv().await? {
                    Some(IpcMessage::Event(e)) => e,
                    Some(IpcMessage::Response(_)) => continue, // stray response - skip
                    None => return Err(eyre!("daemon disconnected before plan was created")),
                }
            };

            match event.event.as_str() {
                "coordinator.interview_question" => {
                    let answer = fixture.responses.get(turn).copied().unwrap_or(fixture.fallback);
                    let (_, resp_events) = client
                        .request("coordinator.interview_respond", json!({"answer": answer}))
                        .await?;
                    buffered.extend(resp_events);
                    turn += 1;
                }
                "record.created" if event.data["collection"].as_str() == Some("plan") => {
                    let id = event.data["id"].as_str().unwrap_or_default().to_string();
                    return Ok::<String, eyre::Error>(id);
                }
                _ => {} // ignore agent.status_changed and other events
            }
        }
    })
    .await
    .map_err(|_| {
        eyre!(
            "persona '{}': timed out after {}s waiting for record.created (collection: plan)",
            fixture.name,
            timeout_secs,
        )
    })??;

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
///
/// Tier 1: `record.created` received; non-empty title/description; keywords
/// "rust" and "sqlite" present in plan text.
/// Tier 2 (nightly, opt-in via `LOOPR_TIER2_EVAL=1`): LLM evaluator checks
/// the plan scopes a Rust CLI todo app with SQLite, not a web app.
#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY and live Coordinator LLM; run with: cargo test --test funnel -- --ignored"]
async fn test_persona_golden_path() {
    let temp_dir = TempTestDir::new("loopr-funnel");
    let daemon = DaemonHandle::spawn(temp_dir).await.unwrap();
    let plan_id = run_persona(&daemon.socket_path, &GOLDEN_PATH).await.unwrap();
    assert!(
        !plan_id.is_empty(),
        "expected non-empty plan_id from Golden Path interview"
    );
    let plan_text = assert_plan_tier1(&daemon.socket_path, &plan_id, &GOLDEN_PATH)
        .await
        .unwrap();
    assert_plan_tier2(&plan_text, &GOLDEN_PATH).await.unwrap();
}

/// Vague User: an underspecified goal; Coordinator must extract a scoped plan.
///
/// Tier 1: non-empty title/description; keywords "cli" + "task" present -
/// confirming the Coordinator absorbed the user's clarifications.
/// Tier 2 (nightly): evaluator checks the plan is a scoped CLI task manager,
/// not a vague "stuff manager".
#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY and live Coordinator LLM; run with: cargo test --test funnel -- --ignored"]
async fn test_persona_vague_user() {
    loopr::prompts::init_defaults();
    let temp_dir = TempTestDir::new("loopr-funnel");
    let daemon = DaemonHandle::spawn(temp_dir).await.unwrap();
    let plan_id = run_persona(&daemon.socket_path, &VAGUE_USER).await.unwrap();
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
///
/// Tier 1: keywords "postgres", "redis", "python" present - confirming the
/// Coordinator tracked the redirected requirements, not the original CSV goal.
/// Tier 2 (nightly): evaluator checks the plan reflects the Postgres/Redis
/// architecture, not the original CSV-only request.
#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY and live Coordinator LLM; run with: cargo test --test funnel -- --ignored"]
async fn test_persona_scope_creeper() {
    loopr::prompts::init_defaults();
    let temp_dir = TempTestDir::new("loopr-funnel");
    let daemon = DaemonHandle::spawn(temp_dir).await.unwrap();
    let plan_id = run_persona(&daemon.socket_path, &SCOPE_CREEPER).await.unwrap();
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
///
/// Tier 1: keywords "cli" and "expense" present - confirming the Coordinator
/// incorporated the user's pushback and produced a CLI-focused plan.
/// Tier 2 (nightly): evaluator checks the plan reflects CLI expense tracking,
/// not the original web app.
#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY and live Coordinator LLM; run with: cargo test --test funnel -- --ignored"]
async fn test_persona_pushback_user() {
    loopr::prompts::init_defaults();
    let temp_dir = TempTestDir::new("loopr-funnel");
    let daemon = DaemonHandle::spawn(temp_dir).await.unwrap();
    let plan_id = run_persona(&daemon.socket_path, &PUSHBACK_USER).await.unwrap();
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
///
/// Tier 1: non-empty title/description; keyword "tool" present - confirms
/// the Coordinator does not deadlock and reaches ProposePlan within timeout.
/// Tier 2 (nightly): evaluator checks the plan is coherent and self-contained
/// given only the initial goal and the Coordinator's own judgment.
#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY and live Coordinator LLM; run with: cargo test --test funnel -- --ignored"]
async fn test_persona_silent_user() {
    loopr::prompts::init_defaults();
    let temp_dir = TempTestDir::new("loopr-funnel");
    let daemon = DaemonHandle::spawn(temp_dir).await.unwrap();
    let plan_id = run_persona(&daemon.socket_path, &SILENT_USER).await.unwrap();
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
