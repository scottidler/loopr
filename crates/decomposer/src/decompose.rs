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
/// On any failure, returns without side effects. The single retry
/// has already fired if the first call failed; `DecomposerError`
/// surfaces the final failure.
#[instrument(level = "info", skip_all, fields(
    plan_id = %plan.id,
    goal_len = plan.goal.len(),
    child_count = tracing::field::Empty,
    outcome = tracing::field::Empty,
))]
pub async fn decompose<L: LlmClient>(plan: &Plan, target: &Path, llm: &L) -> Result<Vec<Work>, DecomposerError> {
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

    // First LLM call. The success path runs through validation below
    // before we have a parsed response to put in the transcript; the
    // failure path writes its own transcript entry and returns. The
    // retry path overwrites `iteration_used` / `user_used` so the
    // success-path transcript carries iteration 2's prompts when the
    // first call failed.
    let mut iteration_used: u32 = 1;
    let mut user_used: String = first_user.clone();
    let started_at_initial = now_iso8601();
    let t0_first = Instant::now();
    let tool_call = match try_llm_once(llm, &system, &first_user).await {
        Ok(tc) => {
            tracing::debug!(
                latency_ms = t0_first.elapsed().as_millis() as u64,
                "decomposer: first LLM call ok"
            );
            tc
        }
        Err(first_err) => {
            let first_latency = t0_first.elapsed().as_millis() as u64;
            warn!(error = %first_err, "decompose: first LLM call failed, retrying once");
            // Best-effort: log iteration 1's failed call before the retry.
            write_decomposer_transcript(
                target,
                &plan_id_str,
                1,
                &system,
                &first_user,
                &first_err.to_string(),
                Vec::new(),
                "llm_failed_retrying",
                &started_at_initial,
                first_latency,
            );
            let retry_user = assemble_user(&loader, &plan.goal, Some(&first_err.to_string()))?;
            iteration_used = 2;
            user_used = retry_user.clone();
            let started_at_retry = now_iso8601();
            let t0_retry = Instant::now();
            match try_llm_once(llm, &system, &retry_user).await {
                Ok(tc) => {
                    tracing::debug!(
                        latency_ms = t0_retry.elapsed().as_millis() as u64,
                        "decomposer: retry LLM call ok"
                    );
                    tc
                }
                Err(retry_err) => {
                    let retry_latency = t0_retry.elapsed().as_millis() as u64;
                    span.record("outcome", "llm_failed");
                    write_decomposer_transcript(
                        target,
                        &plan_id_str,
                        2,
                        &system,
                        &retry_user,
                        &retry_err.to_string(),
                        Vec::new(),
                        "llm_failed",
                        &started_at_retry,
                        retry_latency,
                    );
                    return Err(DecomposerError::LlmFailed(retry_err));
                }
            }
        }
    };

    // The successful tool_call's input is re-serialized for the
    // transcript regardless of whether the response parses cleanly
    // below; raw text in the transcript is the whole point.
    let raw_response = serde_json::to_string_pretty(&tool_call.input).unwrap_or_else(|_| tool_call.input.to_string());
    let total_latency_ms = t0_first.elapsed().as_millis() as u64;

    let response: DecomposeResponse = match serde_json::from_value::<DecomposeResponse>(tool_call.input.clone()) {
        Ok(r) => r,
        Err(e) => {
            span.record("outcome", "malformed_children");
            write_decomposer_transcript(
                target,
                &plan_id_str,
                iteration_used,
                &system,
                &user_used,
                &raw_response,
                Vec::new(),
                "malformed_children",
                &started_at_initial,
                total_latency_ms,
            );
            return Err(DecomposerError::MalformedChildren(e.to_string()));
        }
    };

    if response.children.is_empty() {
        span.record("outcome", "zero_children");
        write_decomposer_transcript(
            target,
            &plan_id_str,
            iteration_used,
            &system,
            &user_used,
            &raw_response,
            Vec::new(),
            "zero_children",
            &started_at_initial,
            total_latency_ms,
        );
        return Err(DecomposerError::ZeroChildren(plan.id.clone()));
    }

    // Empty-title check runs before normalization so we can name the
    // offending index. Any whitespace-only title also trips; after
    // `normalize` it would collapse into an indistinguishable empty
    // string.
    for (idx, child) in response.children.iter().enumerate() {
        if normalize(&child.title).is_empty() {
            span.record("outcome", "empty_title");
            write_decomposer_transcript(
                target,
                &plan_id_str,
                iteration_used,
                &system,
                &user_used,
                &raw_response,
                render_decompose_response(&response),
                "empty_title",
                &started_at_initial,
                total_latency_ms,
            );
            return Err(DecomposerError::EmptyTitle(idx));
        }
    }

    let normalized_titles: Vec<String> = response.children.iter().map(|c| normalize(&c.title)).collect();
    let dupes = find_duplicates(&normalized_titles);
    if !dupes.is_empty() {
        span.record("outcome", "duplicate_titles");
        write_decomposer_transcript(
            target,
            &plan_id_str,
            iteration_used,
            &system,
            &user_used,
            &raw_response,
            render_decompose_response(&response),
            "duplicate_titles",
            &started_at_initial,
            total_latency_ms,
        );
        return Err(DecomposerError::DuplicateTitles(dupes));
    }

    // Pre-mint one WorkId per child, keyed by normalized title.
    let mut title_to_id: HashMap<String, WorkId> = HashMap::with_capacity(normalized_titles.len());
    for title in &normalized_titles {
        title_to_id.insert(title.clone(), WorkId::new());
    }

    // Resolve each child's deps to sibling WorkIds. All errors collect
    // into a single UnresolvedDeps. Resolution runs BEFORE cycle
    // detection so unknown-sibling refs surface as UnresolvedDeps (the
    // clearer message) and the graph below sees only real edges.
    let resolved_deps = match resolve_deps(&response.children, &title_to_id) {
        Ok(r) => r,
        Err(e) => {
            span.record("outcome", "unresolved_deps");
            if let DecomposerError::UnresolvedDeps(msg) = &e {
                warn!(unresolved = %msg, "decompose: unresolved sibling deps");
            }
            write_decomposer_transcript(
                target,
                &plan_id_str,
                iteration_used,
                &system,
                &user_used,
                &raw_response,
                render_decompose_response(&response),
                "unresolved_deps",
                &started_at_initial,
                total_latency_ms,
            );
            return Err(e);
        }
    };

    // Cycle detection over the resolved WorkId edges via domain::WorkGraph.
    // On a cycle, map the offending ids back to titles (title_to_id is a
    // strict bijection - duplicate normalized titles are rejected above)
    // so the operator-facing message names titles, not opaque ids.
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
        span.record("outcome", "cycle");
        warn!(cycle = %cycle_desc, "decompose: dependency cycle detected");
        write_decomposer_transcript(
            target,
            &plan_id_str,
            iteration_used,
            &system,
            &user_used,
            &raw_response,
            render_decompose_response(&response),
            "cycle",
            &started_at_initial,
            total_latency_ms,
        );
        return Err(DecomposerError::CycleDetected(cycle_desc));
    }

    // Build Works, pairing each child with its pre-minted id and
    // resolved deps. AC falls back to markdown extraction if the LLM
    // didn't populate `acceptance_criteria`.
    let mut works: Vec<Work> = Vec::with_capacity(response.children.len());
    for ((child, norm_title), deps) in response
        .children
        .iter()
        .zip(normalized_titles.iter())
        .zip(resolved_deps.into_iter())
    {
        let ac = non_empty_ac_for(child);
        if ac.is_empty() {
            span.record("outcome", "empty_ac");
            write_decomposer_transcript(
                target,
                &plan_id_str,
                iteration_used,
                &system,
                &user_used,
                &raw_response,
                render_decompose_response(&response),
                "empty_ac",
                &started_at_initial,
                total_latency_ms,
            );
            return Err(DecomposerError::EmptyAcceptanceCriteria(child.title.clone()));
        }
        let mut work = Work::new(plan.id.clone(), child.title.clone());
        work.id = title_to_id[norm_title].clone();
        work.dependencies = deps;
        work.acceptance_criteria = AcceptanceCriteria(ac);
        work.files = child.files.clone();
        works.push(work);
    }

    span.record("outcome", "ok");
    span.record("child_count", works.len());
    info!(child_count = works.len(), "decompose: produced works");
    write_decomposer_transcript(
        target,
        &plan_id_str,
        iteration_used,
        &system,
        &user_used,
        &raw_response,
        render_decompose_response(&response),
        "ok",
        &started_at_initial,
        total_latency_ms,
    );
    Ok(works)
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
