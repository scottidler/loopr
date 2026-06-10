//! The main `decompose<L: LlmClient>` function. Orchestrates:
//!
//! 1. Workspace file-tree collection (`tree::collect_workspace_tree`).
//! 2. Prompt assembly (`prompt::assemble_system` / `assemble_user`).
//! 3. LLM call via `LlmClient::complete_with_tool` with the
//!    `submit_decomposition` schema, plus one retry with the error
//!    interpolated into the user message on any `LlmError`.
//! 4. Tool-call input deserialization to `DecomposeResponse`.
//! 5. Validation: non-empty children, no empty titles, no duplicates
//!    after normalization.
//! 6. Title-to-`WorkId` mint, dep-graph cycle detection, dep
//!    resolution.
//! 7. Build `Vec<Work>` with pre-minted ids, resolved deps, and non-
//!    empty acceptance criteria (falling back to extraction from
//!    `content` when the LLM didn't populate the array).
//!
//! The function is pure with respect to the store: no persistence.
//! The caller (`loopr`'s `plan.create` handler) is responsible for
//! `store.works().create_many(works)`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use tracing::{info, instrument, warn};

use context::PromptLoader;
use domain::{AcceptanceCriteria, GraphError, Plan, Work, WorkGraph, WorkId};
use llm::{LlmClient, ToolCall};
use telemetry::transcript::{TranscriptIteration, append_iteration, decomposer_path};

use crate::config::DecomposerConfig;
use crate::error::DecomposerError;
use crate::prompt::{assemble_system, assemble_user};
use crate::resolve::{normalize, resolve_deps};
use crate::tool::{DecomposeChild, DecomposeResponse, submit_decomposition_schema};
use crate::tree::collect_workspace_tree;

/// Render one `DecomposeResponse` summary line per child, the way the
/// transcript's "Parsed Actions" section expects: `<title> (deps: [...]) ac=<n>`.
/// On the failure paths (no parsed response yet) this returns an empty
/// vec; the caller passes whatever it has.
fn render_decompose_response(response: &DecomposeResponse) -> Vec<String> {
    response
        .children
        .iter()
        .map(|c| {
            let deps_joined = c.dependencies.join(", ");
            let ac_count = c.acceptance_criteria.len();
            format!("{} (deps: [{}]) ac={}", c.title, deps_joined, ac_count)
        })
        .collect()
}

/// Append one iteration's transcript block. Best-effort: failures emit
/// `warn!` and continue. Mirrors `agents::implementer::write_implementer_transcript`
/// but writes to `<target>/.loopr/records/plans/<plan-id>/decomposition.md`.
#[allow(clippy::too_many_arguments)]
fn write_decomposer_transcript(
    target: &Path,
    plan_id: &str,
    iteration: u32,
    system_prompt: &str,
    user_prompt: &str,
    raw_response: &str,
    parsed_actions: Vec<String>,
    outcome: &str,
    started_at: &str,
    latency_ms: u64,
) {
    let mut iter = TranscriptIteration::new_single_turn(String::new(), started_at);
    iter.iteration = iteration;
    iter.latency_ms = latency_ms;
    iter.system_prompt = system_prompt.to_string();
    iter.user_prompt = user_prompt.to_string();
    iter.response = raw_response.to_string();
    iter.parsed_actions = parsed_actions;
    iter.dispatcher_outcomes = vec![outcome.to_string()];
    let path = decomposer_path(target, plan_id);
    if let Err(e) = append_iteration(&path, &iter) {
        warn!(error = %e, path = %path.display(), "decomposer transcript append failed");
    }
}

/// Format an instant as a coarse ISO-8601-ish timestamp without pulling
/// `chrono` into the decomposer just for the transcript's `started_at`
/// field. Telemetry already owns the precise timestamp via the spans;
/// this is the human-readable convenience field on the rendered block.
fn now_iso8601() -> String {
    let now = std::time::SystemTime::now();
    let unix = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Coarse format: seconds-since-epoch is enough for the "Raw" link in
    // the rendered iteration. Tests do not assert on the exact string.
    format!("{unix}")
}

/// Decompose a `Plan` into a batch of child `Work`s.
///
/// Contract: `plan` is already validated by the caller (its
/// `PlanStatus` is `Active`), `target` exists as a directory, `llm`
/// is a live client. On success, every returned `Work` has:
/// - `parent_id == plan.id`
/// - `status == WorkStatus::Pending`
/// - `dependencies` resolved to concrete `WorkId`s pointing at
///   siblings in the same batch
/// - `acceptance_criteria` non-empty (LLM-provided or extracted from
///   content's `## Acceptance Criteria` section)
///
/// On any failure, returns without side effects. A single retry covers
/// BOTH a transient LLM error and a post-parse validation error: the
/// first failure (LLM or validation) re-prompts with the error text
/// interpolated into the user message, then a second failure bails.
/// `DecomposerError` surfaces the final failure.
#[instrument(level = "info", skip_all, fields(
    plan_id = %plan.id,
    goal_len = plan.goal.len(),
    child_count = tracing::field::Empty,
    outcome = tracing::field::Empty,
))]
pub async fn decompose<L: LlmClient>(
    plan: &Plan,
    target: &Path,
    llm: &L,
    config: &DecomposerConfig,
) -> Result<Vec<Work>, DecomposerError> {
    let span = tracing::Span::current();

    let tree = match collect_workspace_tree(target) {
        Ok(t) => t,
        Err(e) => {
            span.record("outcome", "workspace_scan_failed");
            return Err(e);
        }
    };

    let loader = PromptLoader::for_target(target)?;
    let system = assemble_system(&loader, &tree)?;
    let first_user = assemble_user(&loader, &plan.goal, None)?;

    let plan_id_str = plan.id.as_ref().to_string();

    // Unified attempt loop. A single retry covers BOTH a transient LLM
    // error and a post-parse validation error (bullet 12): the first
    // failure re-prompts with the error text interpolated into the user
    // message via `assemble_user(goal, Some(err))`, then the second
    // failure bails. Pre-fix the seven validation errors bailed
    // immediately, never triggering the retry the error machinery,
    // comments, and design doc all promised.
    let mut user: String = first_user;
    let mut started_at = now_iso8601();
    let mut attempt: u32 = 1;

    // Labeled block (not a loop — it runs once and either returns on
    // success/terminal-bail or yields the error text to re-prompt with).
    let retry_err: String = 'first_attempt: {
        let t0 = Instant::now();
        let llm_result = try_llm_once(llm, &system, &user).await;
        let latency_ms = t0.elapsed().as_millis() as u64;

        match llm_result {
            Ok(tool_call) => {
                let raw_response =
                    serde_json::to_string_pretty(&tool_call.input).unwrap_or_else(|_| tool_call.input.to_string());
                match parse_and_validate(&tool_call.input, plan, config.max_children) {
                    Ok((works, response)) => {
                        span.record("outcome", "ok");
                        span.record("child_count", works.len());
                        info!(child_count = works.len(), attempt, "decompose: produced works");
                        write_decomposer_transcript(
                            target,
                            &plan_id_str,
                            attempt,
                            &system,
                            &user,
                            &raw_response,
                            render_decompose_response(&response),
                            "ok",
                            &started_at,
                            latency_ms,
                        );
                        return Ok(works);
                    }
                    Err(failure) => {
                        span.record("outcome", failure.outcome);
                        let parsed = failure.rendered.clone();
                        if attempt >= MAX_DECOMPOSE_ATTEMPTS {
                            warn!(outcome = failure.outcome, error = %failure.error, "decompose: validation failed after retry; bailing");
                            write_decomposer_transcript(
                                target,
                                &plan_id_str,
                                attempt,
                                &system,
                                &user,
                                &raw_response,
                                parsed,
                                failure.outcome,
                                &started_at,
                                latency_ms,
                            );
                            return Err(failure.error);
                        }
                        warn!(outcome = failure.outcome, error = %failure.error, "decompose: validation failed; retrying with error in prompt");
                        write_decomposer_transcript(
                            target,
                            &plan_id_str,
                            attempt,
                            &system,
                            &user,
                            &raw_response,
                            parsed,
                            failure.outcome,
                            &started_at,
                            latency_ms,
                        );
                        break 'first_attempt failure.error.to_string();
                    }
                }
            }
            Err(llm_err) => {
                if attempt >= MAX_DECOMPOSE_ATTEMPTS {
                    span.record("outcome", "llm_failed");
                    write_decomposer_transcript(
                        target,
                        &plan_id_str,
                        attempt,
                        &system,
                        &user,
                        &llm_err.to_string(),
                        Vec::new(),
                        "llm_failed",
                        &started_at,
                        latency_ms,
                    );
                    return Err(DecomposerError::LlmFailed(Box::new(llm_err)));
                }
                warn!(error = %llm_err, "decompose: LLM call failed, retrying once");
                write_decomposer_transcript(
                    target,
                    &plan_id_str,
                    attempt,
                    &system,
                    &user,
                    &llm_err.to_string(),
                    Vec::new(),
                    "llm_failed_retrying",
                    &started_at,
                    latency_ms,
                );
                break 'first_attempt llm_err.to_string();
            }
        }
    };

    // Prepare the single retry: re-prompt with the failure interpolated.
    attempt += 1;
    user = assemble_user(&loader, &plan.goal, Some(&retry_err))?;
    started_at = now_iso8601();

    let t0 = Instant::now();
    let llm_result = try_llm_once(llm, &system, &user).await;
    let latency_ms = t0.elapsed().as_millis() as u64;
    match llm_result {
        Ok(tool_call) => {
            let raw_response =
                serde_json::to_string_pretty(&tool_call.input).unwrap_or_else(|_| tool_call.input.to_string());
            match parse_and_validate(&tool_call.input, plan, config.max_children) {
                Ok((works, response)) => {
                    span.record("outcome", "ok");
                    span.record("child_count", works.len());
                    info!(child_count = works.len(), attempt, "decompose: produced works");
                    write_decomposer_transcript(
                        target,
                        &plan_id_str,
                        attempt,
                        &system,
                        &user,
                        &raw_response,
                        render_decompose_response(&response),
                        "ok",
                        &started_at,
                        latency_ms,
                    );
                    Ok(works)
                }
                Err(failure) => {
                    span.record("outcome", failure.outcome);
                    let parsed = failure.rendered.clone();
                    warn!(outcome = failure.outcome, error = %failure.error, "decompose: validation failed after retry; bailing");
                    write_decomposer_transcript(
                        target,
                        &plan_id_str,
                        attempt,
                        &system,
                        &user,
                        &raw_response,
                        parsed,
                        failure.outcome,
                        &started_at,
                        latency_ms,
                    );
                    Err(failure.error)
                }
            }
        }
        Err(llm_err) => {
            span.record("outcome", "llm_failed");
            write_decomposer_transcript(
                target,
                &plan_id_str,
                attempt,
                &system,
                &user,
                &llm_err.to_string(),
                Vec::new(),
                "llm_failed",
                &started_at,
                latency_ms,
            );
            Err(DecomposerError::LlmFailed(Box::new(llm_err)))
        }
    }
}

/// Maximum total `decompose` attempts (initial + one retry). The retry
/// budget is shared across LLM errors and validation errors.
const MAX_DECOMPOSE_ATTEMPTS: u32 = 2;

/// A validation failure carrying its typed error, the stable transcript
/// outcome label, and the parsed response (when it deserialized) for
/// transcript rendering.
struct ValidationFailure {
    error: DecomposerError,
    outcome: &'static str,
    /// Pre-rendered child lines for the transcript (empty when the
    /// response did not even deserialize). Stored rendered rather than
    /// as the full `DecomposeResponse` so the `Err` variant stays small.
    rendered: Vec<String>,
}

impl ValidationFailure {
    /// Box the failure. `DecomposerError` embeds `context::PromptError`,
    /// which is large, so the unboxed `Result<_, ValidationFailure>`
    /// trips clippy's `result_large_err`. Validation failures are the
    /// rare path, so the heap allocation is free in the common case.
    fn boxed(error: DecomposerError, outcome: &'static str, rendered: Vec<String>) -> Box<Self> {
        Box::new(Self {
            error,
            outcome,
            rendered,
        })
    }
}

/// Pure parse + validation of one LLM tool-call input. No I/O, no
/// transcripts, no LLM — the caller owns retry + transcript writing.
/// Runs the seven post-parse checks (plus the max-children bound) in the
/// order that yields the clearest operator-facing error, and on success
/// returns the built `Vec<Work>` alongside the parsed response.
/// Reject a child `files` scope entry that cannot be a repo-relative
/// forward-slash path. Returns a static reason on rejection.
fn invalid_scope_path(p: &str) -> Option<&'static str> {
    if p.contains('\\') {
        return Some("backslash path separator (use forward slashes)");
    }
    let path = std::path::Path::new(p);
    if path.is_absolute() {
        return Some("absolute path (must be repo-relative)");
    }
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Some("parent-directory traversal (`..`)");
    }
    None
}

fn parse_and_validate(
    input: &serde_json::Value,
    plan: &Plan,
    max_children: usize,
) -> Result<(Vec<Work>, DecomposeResponse), Box<ValidationFailure>> {
    let response: DecomposeResponse = match serde_json::from_value::<DecomposeResponse>(input.clone()) {
        Ok(r) => r,
        Err(e) => {
            return Err(ValidationFailure::boxed(
                DecomposerError::MalformedChildren(e.to_string()),
                "malformed_children",
                Vec::new(),
            ));
        }
    };

    if response.children.is_empty() {
        return Err(ValidationFailure::boxed(
            DecomposerError::ZeroChildren(plan.id.clone()),
            "zero_children",
            render_decompose_response(&response),
        ));
    }

    // Max-children bound (bullet 14): the handler spawns an Implementer
    // per unblocked Work with no pool cap, so an oversized decomposition
    // would fan out too many concurrent agents. Checked beside the
    // zero-children floor.
    if response.children.len() > max_children {
        return Err(ValidationFailure::boxed(
            DecomposerError::TooManyChildren {
                count: response.children.len(),
                max: max_children,
            },
            "too_many_children",
            render_decompose_response(&response),
        ));
    }

    // Empty-title check runs before normalization so we can name the
    // offending index. Any whitespace-only title also trips; after
    // `normalize` it would collapse into an indistinguishable empty
    // string.
    for (idx, child) in response.children.iter().enumerate() {
        if normalize(&child.title).is_empty() {
            return Err(ValidationFailure::boxed(
                DecomposerError::EmptyTitle(idx),
                "empty_title",
                render_decompose_response(&response),
            ));
        }
    }

    let normalized_titles: Vec<String> = response.children.iter().map(|c| normalize(&c.title)).collect();
    let dupes = find_duplicates(&normalized_titles);
    if !dupes.is_empty() {
        return Err(ValidationFailure::boxed(
            DecomposerError::DuplicateTitles(dupes),
            "duplicate_titles",
            render_decompose_response(&response),
        ));
    }

    // Scope-path validation (finding 10): each child's `files` must be a
    // repo-relative forward-slash path. An absolute path, a `..` traversal,
    // or a backslash separator silently voids the scope the agents are told
    // to respect, so reject it at produce time and let the retry path
    // re-emit a clean scope.
    for child in &response.children {
        for f in &child.files {
            if let Some(why) = invalid_scope_path(f) {
                return Err(ValidationFailure::boxed(
                    DecomposerError::InvalidFiles {
                        child: child.title.clone(),
                        path: f.clone(),
                        why,
                    },
                    "invalid_files",
                    render_decompose_response(&response),
                ));
            }
        }
    }

    // Pre-mint one WorkId per child, keyed by normalized title.
    let mut title_to_id: HashMap<String, WorkId> = HashMap::with_capacity(normalized_titles.len());
    for title in &normalized_titles {
        title_to_id.insert(title.clone(), WorkId::new());
    }

    // Resolve each child's deps to sibling WorkIds. Resolution runs
    // BEFORE cycle detection so unknown-sibling refs surface as
    // UnresolvedDeps (the clearer message) and the graph sees real edges.
    let resolved_deps = match resolve_deps(&response.children, &title_to_id) {
        Ok(r) => r,
        Err(e) => {
            if let DecomposerError::UnresolvedDeps(msg) = &e {
                warn!(unresolved = %msg, "decompose: unresolved sibling deps");
            }
            return Err(ValidationFailure::boxed(
                e,
                "unresolved_deps",
                render_decompose_response(&response),
            ));
        }
    };

    // Cycle detection over the resolved WorkId edges via domain::WorkGraph.
    let edges: Vec<(WorkId, Vec<WorkId>)> = normalized_titles
        .iter()
        .zip(resolved_deps.iter())
        .map(|(norm_title, deps)| (title_to_id[norm_title].clone(), deps.clone()))
        .collect();
    if let Err(GraphError::Cycle(cycle_ids)) = WorkGraph::from_edges(edges) {
        let id_to_title: HashMap<&WorkId, &String> = title_to_id.iter().map(|(title, id)| (id, title)).collect();
        let cycle_desc = cycle_ids
            .iter()
            .map(|id| {
                id_to_title
                    .get(id)
                    .map(|t| t.as_str())
                    .unwrap_or("<unknown>")
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join(", ");
        warn!(cycle = %cycle_desc, "decompose: dependency cycle detected");
        return Err(ValidationFailure::boxed(
            DecomposerError::CycleDetected(cycle_desc),
            "cycle",
            render_decompose_response(&response),
        ));
    }

    // Build Works. AC falls back to markdown extraction if the LLM
    // didn't populate `acceptance_criteria`. The empty-AC failing title
    // is captured and the loop broken so the borrow of `response.children`
    // ends before `response` is moved into the error below.
    let mut works: Vec<Work> = Vec::with_capacity(response.children.len());
    let mut empty_ac_title: Option<String> = None;
    for ((child, norm_title), deps) in response
        .children
        .iter()
        .zip(normalized_titles.iter())
        .zip(resolved_deps)
    {
        let ac = non_empty_ac_for(child);
        if ac.is_empty() {
            empty_ac_title = Some(child.title.clone());
            break;
        }
        let mut work = Work::new(plan.id.clone(), child.title.clone());
        work.id = title_to_id[norm_title].clone();
        work.dependencies = deps;
        work.acceptance_criteria = AcceptanceCriteria(ac);
        work.files = child.files.clone();
        works.push(work);
    }
    if let Some(title) = empty_ac_title {
        return Err(ValidationFailure::boxed(
            DecomposerError::EmptyAcceptanceCriteria(title),
            "empty_ac",
            render_decompose_response(&response),
        ));
    }

    Ok((works, response))
}

#[instrument(level = "debug", skip_all, fields(system_chars = system.len(), user_chars = user.len()), err)]
async fn try_llm_once<L: LlmClient>(llm: &L, system: &str, user: &str) -> Result<ToolCall, llm::LlmError> {
    // Phase 4 widened the trait to return `(ToolCall, Usage)`; the
    // decomposer doesn't consume `Usage` directly (the metering wrapper
    // owns counter accumulation), so discard it at this call site.
    let (tool_call, _usage) = llm
        .complete_with_tool(system, user, submit_decomposition_schema(), None)
        .await?;
    tracing::debug!(
        system_chars = system.len(),
        user_chars = user.len(),
        tool_name = %tool_call.tool_name,
        "decomposer: try_llm_once ok"
    );
    Ok(tool_call)
}

/// Return the set of duplicate normalized titles. A title appears in
/// the output if and only if it occurs at least twice.
fn find_duplicates(titles: &[String]) -> Vec<String> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut dupes: Vec<String> = Vec::new();
    let mut emitted: HashSet<String> = HashSet::new();
    for t in titles {
        if !seen.insert(t.as_str()) && emitted.insert(t.clone()) {
            dupes.push(t.clone());
        }
    }
    dupes
}

/// Return a non-empty `Vec<String>` of acceptance criteria if
/// possible, falling back to parsing the child's markdown `content`
/// for a `## Acceptance Criteria` section. Returns empty only when
/// both sources are empty.
fn non_empty_ac_for(child: &DecomposeChild) -> Vec<String> {
    if !child.acceptance_criteria.is_empty() {
        return child.acceptance_criteria.clone();
    }
    extract_acceptance_criteria(&child.content)
}

const SECTION_AC: &str = "Acceptance Criteria";

/// Extract acceptance criteria from a `## Acceptance Criteria` section.
/// Ported from v3's `decomposer.rs:169-192` verbatim.
fn extract_acceptance_criteria(content: &str) -> Vec<String> {
    let mut in_section = false;
    let mut criteria = Vec::new();

    for line in content.lines() {
        if line.starts_with(&format!("## {SECTION_AC}")) {
            in_section = true;
            continue;
        }
        if in_section && line.starts_with("## ") {
            break;
        }
        if in_section {
            let trimmed = line.trim();
            if trimmed.starts_with("assert") || trimmed.starts_with("- ") {
                let clean = trimmed.trim_start_matches("- ").to_string();
                if !clean.is_empty() {
                    criteria.push(clean);
                }
            }
        }
    }
    criteria
}

#[cfg(test)]
mod tests;
