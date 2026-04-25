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

use tracing::{info, instrument, warn};

use domain::{AcceptanceCriteria, Plan, Work, WorkId};
use llm::{LlmClient, ToolCall};

use crate::cycles::{detect_cycles, normalize, resolve_deps};
use crate::error::DecomposerError;
use crate::prompt::{assemble_system, assemble_user};
use crate::tool::{DecomposeChild, DecomposeResponse, submit_decomposition_schema};
use crate::tree::collect_workspace_tree;

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

    let system = assemble_system(&tree);
    let first_user = assemble_user(&plan.goal, None);

    let tool_call = match try_llm_once(llm, &system, &first_user).await {
        Ok(tc) => tc,
        Err(first_err) => {
            warn!(error = %first_err, "decompose: first LLM call failed, retrying once");
            let retry_user = assemble_user(&plan.goal, Some(&first_err.to_string()));
            match try_llm_once(llm, &system, &retry_user).await {
                Ok(tc) => tc,
                Err(retry_err) => {
                    span.record("outcome", "llm_failed");
                    return Err(DecomposerError::LlmFailed(retry_err));
                }
            }
        }
    };

    let response: DecomposeResponse = serde_json::from_value(tool_call.input).map_err(|e| {
        span.record("outcome", "malformed_children");
        DecomposerError::MalformedChildren(e.to_string())
    })?;

    if response.children.is_empty() {
        span.record("outcome", "zero_children");
        return Err(DecomposerError::ZeroChildren(plan.id.clone()));
    }

    // Empty-title check runs before normalization so we can name the
    // offending index. Any whitespace-only title also trips; after
    // `normalize` it would collapse into an indistinguishable empty
    // string.
    for (idx, child) in response.children.iter().enumerate() {
        if normalize(&child.title).is_empty() {
            span.record("outcome", "empty_title");
            return Err(DecomposerError::EmptyTitle(idx));
        }
    }

    let normalized_titles: Vec<String> = response.children.iter().map(|c| normalize(&c.title)).collect();
    let dupes = find_duplicates(&normalized_titles);
    if !dupes.is_empty() {
        span.record("outcome", "duplicate_titles");
        return Err(DecomposerError::DuplicateTitles(dupes));
    }

    // Pre-mint one WorkId per child, keyed by normalized title.
    let mut title_to_id: HashMap<String, WorkId> = HashMap::with_capacity(normalized_titles.len());
    for title in &normalized_titles {
        title_to_id.insert(title.clone(), WorkId::new());
    }

    // Build dependency graph for cycle detection; both node label and
    // dep targets use the normalized form so Kahn's sees the same
    // strings on both sides of each edge.
    let mut dep_graph: HashMap<String, Vec<String>> = HashMap::with_capacity(response.children.len());
    for (child, norm_title) in response.children.iter().zip(normalized_titles.iter()) {
        dep_graph.insert(
            norm_title.clone(),
            child.dependencies.iter().map(|d| normalize(d)).collect(),
        );
    }
    if let Err(cycle_desc) = detect_cycles(&dep_graph) {
        span.record("outcome", "cycle");
        warn!(cycle = %cycle_desc, "decompose: dependency cycle detected");
        return Err(DecomposerError::CycleDetected(cycle_desc));
    }

    // Resolve each child's deps to sibling WorkIds. All errors
    // collect into a single UnresolvedDeps.
    let resolved_deps = match resolve_deps(&response.children, &title_to_id) {
        Ok(r) => r,
        Err(e) => {
            span.record("outcome", "unresolved_deps");
            if let DecomposerError::UnresolvedDeps(msg) = &e {
                warn!(unresolved = %msg, "decompose: unresolved sibling deps");
            }
            return Err(e);
        }
    };

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
            return Err(DecomposerError::EmptyAcceptanceCriteria(child.title.clone()));
        }
        let mut work = Work::new(plan.id.clone(), child.title.clone());
        work.id = title_to_id[norm_title].clone();
        work.dependencies = deps;
        work.acceptance_criteria = AcceptanceCriteria(ac);
        works.push(work);
    }

    span.record("outcome", "ok");
    span.record("child_count", works.len());
    info!(child_count = works.len(), "decompose: produced works");
    Ok(works)
}

#[instrument(level = "debug", skip_all, fields(system_chars = system.len(), user_chars = user.len()), err)]
async fn try_llm_once<L: LlmClient>(llm: &L, system: &str, user: &str) -> Result<ToolCall, llm::LlmError> {
    llm.complete_with_tool(system, user, submit_decomposition_schema())
        .await
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
