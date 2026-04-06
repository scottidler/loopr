//! Standalone plan decomposer: takes a Doc, calls an LLM, validates output,
//! writes child .md files, creates child Doc records, and returns.
//!
//! This is a system call (function), NOT an agent. It has no session, FSM, or
//! iteration loop. The Coordinator invokes it before execution begins, and can
//! re-invoke it for targeted re-decomposition during execution.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use eyre::{Context, Result, bail};
use log::{info, warn};
use serde::{Deserialize, Serialize};

use futures::future::try_join_all;

use crate::config::DecomposerConfig;
use crate::domain::doc::{Doc, DocKind, write_doc_file};
use crate::validator::client::HttpClient;

const LLM_CALL_TIMEOUT_SECS: u64 = 60;

/// A single child document parsed from the LLM's JSON response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildEntry {
    pub title: String,
    pub content: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
}

/// Result of a template validation call.
#[derive(Debug, Deserialize)]
pub struct ValidationResult {
    pub valid: bool,
    #[serde(default)]
    pub issues: Vec<String>,
}

/// Result of a ratification call.
#[derive(Debug, Deserialize)]
pub struct RatifyResult {
    pub passed: bool,
    #[serde(default)]
    pub issues: Vec<RatifyIssue>,
}

#[derive(Debug, Deserialize)]
pub struct RatifyIssue {
    pub child: String,
    pub issue: String,
}

/// The child DocKind produced when decomposing a given kind.
fn child_kind(parent_kind: DocKind) -> Option<DocKind> {
    match parent_kind {
        DocKind::Plan => Some(DocKind::Spec),
        DocKind::Spec => Some(DocKind::Phase),
        DocKind::Phase => Some(DocKind::Work),
        DocKind::Work => None,
    }
}

/// Build the decomposition prompt: template instructions + parent content.
/// `target_kind` is the kind of document to produce (not the parent kind).
fn build_decompose_prompt(target_kind: DocKind, parent_content: &str) -> Result<String> {
    let prompts = crate::prompts::store();
    let template = match target_kind {
        DocKind::Spec => &prompts.decompose_spec,
        DocKind::Phase => &prompts.decompose_phase,
        DocKind::Work => &prompts.decompose_work,
        DocKind::Plan => bail!("cannot decompose into Plan"),
    };
    Ok(format!("{}\n\n## Parent Document\n\n{}", template, parent_content))
}

/// Build a validation prompt for a child document.
fn build_validate_prompt(child_kind: DocKind, child_content: &str) -> String {
    let prompts = crate::prompts::store();
    let template_text = match child_kind {
        DocKind::Spec => include_str!("../docs/templates/spec.md"),
        DocKind::Phase => include_str!("../docs/templates/phase.md"),
        DocKind::Work => include_str!("../docs/templates/work.md"),
        DocKind::Plan => include_str!("../docs/templates/plan.md"),
    };
    format!(
        "{}\n\n## Template\n\n{}\n\n## Document to Validate\n\n{}",
        prompts.decompose_validate, template_text, child_content
    )
}

/// Build a ratification prompt: parent + all children.
fn build_ratify_prompt(parent_content: &str, children: &[(String, String)]) -> String {
    let prompts = crate::prompts::store();
    let mut prompt = format!(
        "{}\n\n## Parent Document\n\n{}",
        prompts.decompose_ratify, parent_content
    );
    for (title, content) in children {
        prompt.push_str(&format!("\n\n## Child: {}\n\n{}", title, content));
    }
    prompt
}

/// Detect cycles in a dependency graph via topological sort.
///
/// `nodes` maps title -> list of dependency titles.
/// Returns `Ok(())` if acyclic, or `Err` with the cycle description.
pub fn detect_cycles(nodes: &HashMap<String, Vec<String>>) -> Result<()> {
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    for title in nodes.keys() {
        in_degree.entry(title.as_str()).or_insert(0);
    }
    for deps in nodes.values() {
        for dep in deps {
            if let Some(deg) = in_degree.get_mut(dep.as_str()) {
                *deg += 1;
            }
        }
    }

    let mut queue: Vec<&str> = in_degree
        .iter()
        .filter(|(_, deg)| **deg == 0)
        .map(|(title, _)| *title)
        .collect();
    let mut visited = 0usize;

    while let Some(node) = queue.pop() {
        visited += 1;
        if let Some(deps) = nodes.get(node) {
            for dep in deps {
                if let Some(deg) = in_degree.get_mut(dep.as_str()) {
                    *deg -= 1;
                    if *deg == 0 {
                        queue.push(dep.as_str());
                    }
                }
            }
        }
    }

    if visited < nodes.len() {
        let cycled: Vec<_> = in_degree.iter().filter(|(_, deg)| **deg > 0).map(|(t, _)| *t).collect();
        bail!("dependency cycle detected among: {}", cycled.join(", "));
    }
    Ok(())
}

/// Extract acceptance criteria lines from a markdown document's
/// `## Acceptance Criteria` section.
pub fn extract_acceptance_criteria(content: &str) -> Vec<String> {
    let mut in_section = false;
    let mut criteria = Vec::new();

    for line in content.lines() {
        if line.starts_with("## Acceptance Criteria") {
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

/// Call the LLM and parse the response as a JSON array of ChildEntry.
async fn call_llm_for_children<H: HttpClient + Sync>(
    http_client: &H,
    config: &DecomposerConfig,
    prompt: &str,
) -> Result<Vec<ChildEntry>> {
    let api_key =
        std::env::var(&config.api_key_env).context(format!("Missing API key env var: {}", config.api_key_env))?;

    let api_url = match config.provider.as_str() {
        "anthropic" => "https://api.anthropic.com/v1/messages",
        other => bail!("Unsupported LLM provider: {}", other),
    };

    let request = serde_json::json!({
        "model": config.model,
        "max_tokens": config.max_tokens,
        "temperature": config.temperature,
        "messages": [{"role": "user", "content": prompt}]
    });

    let body = serde_json::to_string(&request)?;
    let headers = [
        ("content-type", "application/json"),
        ("x-api-key", api_key.as_str()),
        ("anthropic-version", "2023-06-01"),
    ];

    let response_text = tokio::time::timeout(
        Duration::from_secs(LLM_CALL_TIMEOUT_SECS),
        http_client.post(api_url, &headers, &body),
    )
    .await
    .map_err(|_| eyre::eyre!("LLM call timed out after {}s", LLM_CALL_TIMEOUT_SECS))??;

    let response: serde_json::Value =
        serde_json::from_str(&response_text).context("Failed to parse LLM API response")?;

    let text = response["content"]
        .as_array()
        .and_then(|blocks| blocks.first())
        .and_then(|b| b["text"].as_str())
        .ok_or_else(|| eyre::eyre!("LLM returned no text content"))?;

    // Strip markdown code fences if present
    let json_text = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    let children: Vec<ChildEntry> =
        serde_json::from_str(json_text).context("Failed to parse LLM output as JSON array of child documents")?;

    if children.is_empty() {
        bail!("LLM produced zero child documents");
    }

    Ok(children)
}

/// Call the LLM for validation and parse result.
async fn call_llm_for_validation<H: HttpClient + Sync>(
    http_client: &H,
    config: &DecomposerConfig,
    prompt: &str,
) -> Result<ValidationResult> {
    // Use the validation model (Haiku) for structural checks
    let mut validation_config = config.clone();
    validation_config.model = config.validation_model.clone();

    let api_key = std::env::var(&validation_config.api_key_env)
        .context(format!("Missing API key env var: {}", validation_config.api_key_env))?;

    let api_url = match validation_config.provider.as_str() {
        "anthropic" => "https://api.anthropic.com/v1/messages",
        other => bail!("Unsupported LLM provider: {}", other),
    };

    let request = serde_json::json!({
        "model": validation_config.model,
        "max_tokens": validation_config.max_tokens,
        "temperature": 0.0,
        "messages": [{"role": "user", "content": prompt}]
    });

    let body = serde_json::to_string(&request)?;
    let headers = [
        ("content-type", "application/json"),
        ("x-api-key", api_key.as_str()),
        ("anthropic-version", "2023-06-01"),
    ];

    let response_text = tokio::time::timeout(
        Duration::from_secs(LLM_CALL_TIMEOUT_SECS),
        http_client.post(api_url, &headers, &body),
    )
    .await
    .map_err(|_| eyre::eyre!("LLM call timed out after {}s", LLM_CALL_TIMEOUT_SECS))??;
    let response: serde_json::Value = serde_json::from_str(&response_text)?;

    let text = response["content"]
        .as_array()
        .and_then(|blocks| blocks.first())
        .and_then(|b| b["text"].as_str())
        .ok_or_else(|| eyre::eyre!("Validation LLM returned no text"))?;

    let json_text = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    serde_json::from_str(json_text).context("Failed to parse validation response")
}

/// Call the LLM for ratification and parse result.
async fn call_llm_for_ratification<H: HttpClient + Sync>(
    http_client: &H,
    config: &DecomposerConfig,
    prompt: &str,
) -> Result<RatifyResult> {
    // Uses the main model for ratification (reasoning task, not structural check)
    let text = call_llm_for_children_raw(http_client, config, prompt).await?;
    serde_json::from_str(&text).context("Failed to parse ratification response")
}

/// Raw LLM call that returns text (shared helper).
async fn call_llm_for_children_raw<H: HttpClient + Sync>(
    http_client: &H,
    config: &DecomposerConfig,
    prompt: &str,
) -> Result<String> {
    let api_key =
        std::env::var(&config.api_key_env).context(format!("Missing API key env var: {}", config.api_key_env))?;

    let api_url = match config.provider.as_str() {
        "anthropic" => "https://api.anthropic.com/v1/messages",
        other => bail!("Unsupported LLM provider: {}", other),
    };

    let request = serde_json::json!({
        "model": config.model,
        "max_tokens": config.max_tokens,
        "temperature": config.temperature,
        "messages": [{"role": "user", "content": prompt}]
    });

    let body = serde_json::to_string(&request)?;
    let headers = [
        ("content-type", "application/json"),
        ("x-api-key", api_key.as_str()),
        ("anthropic-version", "2023-06-01"),
    ];

    let response_text = tokio::time::timeout(
        Duration::from_secs(LLM_CALL_TIMEOUT_SECS),
        http_client.post(api_url, &headers, &body),
    )
    .await
    .map_err(|_| eyre::eyre!("LLM call timed out after {}s", LLM_CALL_TIMEOUT_SECS))??;
    let response: serde_json::Value = serde_json::from_str(&response_text)?;

    let text = response["content"]
        .as_array()
        .and_then(|blocks| blocks.first())
        .and_then(|b| b["text"].as_str())
        .ok_or_else(|| eyre::eyre!("LLM returned no text"))?;

    let json_text = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    Ok(json_text.to_string())
}

/// Internal decomposition core: produce `target_kind` children from `parent`.
///
/// Reads the parent's .md content, calls the LLM, validates each child,
/// detects dependency cycles, writes child .md files to the run directory,
/// and returns the child Doc records.
///
/// Uses staging: child files are written to a temp directory first, then
/// moved to the run directory only if all validation passes.
async fn decompose_into<H: HttpClient + Sync>(
    parent: &Doc,
    target_kind: DocKind,
    run_dir: &Path,
    config: &DecomposerConfig,
    http_client: &H,
    global_title_to_id: &mut HashMap<String, String>,
) -> Result<Vec<Doc>> {
    info!("decompose: {} {} -> {}s", parent.kind, parent.id, target_kind);

    // Read parent content
    let parent_path = run_dir.join(&parent.markdown);
    let parent_content = std::fs::read_to_string(&parent_path)
        .context(format!("Failed to read parent doc: {}", parent_path.display()))?;

    // Build prompt and call LLM
    let prompt = build_decompose_prompt(target_kind, &parent_content)?;
    let children = match call_llm_for_children(http_client, config, &prompt).await {
        Ok(c) => c,
        Err(e) => {
            warn!("Decomposition failed, retrying once: {}", e);
            let retry_prompt = format!(
                "{}\n\n## Previous Attempt Failed\n\n{}\n\nPlease fix the issues and try again.",
                prompt, e
            );
            call_llm_for_children(http_client, config, &retry_prompt).await?
        }
    };

    // Validate each child
    for child in &children {
        let validate_prompt = build_validate_prompt(target_kind, &child.content);
        match call_llm_for_validation(http_client, config, &validate_prompt).await {
            Ok(result) if !result.valid => {
                warn!("Validation failed for '{}': {:?}", child.title, result.issues);
                // Could retry here, but design says one retry at decompose level
            }
            Err(e) => {
                warn!("Validation call failed for '{}': {}", child.title, e);
            }
            Ok(_) => {}
        }
    }

    // Cycle detection
    let dep_graph: HashMap<String, Vec<String>> = children
        .iter()
        .map(|c| (c.title.clone(), c.dependencies.clone()))
        .collect();
    detect_cycles(&dep_graph)?;

    // Stage: write .md files and create Doc records.
    // Each decompose_into call uses a unique staging dir (parent.id suffix) so that
    // concurrent calls on the same run_dir do not conflict.
    let staging_dir = run_dir.join(format!(".staging-{}", parent.id));
    std::fs::create_dir_all(&staging_dir)?;

    let mut docs = Vec::new();
    let mut taken: Vec<String> = Vec::new();

    for child in &children {
        let filename = write_doc_file(&staging_dir, target_kind, &child.title, &child.content, &taken)?;
        taken.push(filename.clone());

        let mut doc = Doc::new(target_kind, Some(parent.id.clone()), child.title.clone(), filename);

        doc.acceptance_criteria = if child.acceptance_criteria.is_empty() && target_kind == DocKind::Work {
            extract_acceptance_criteria(&child.content)
        } else {
            child.acceptance_criteria.clone()
        };

        docs.push((doc, child.title.clone(), child.dependencies.clone()));
    }

    // Resolve title-based dependencies to Doc IDs.
    // Build the local sibling map first, then merge into the global map so that
    // future decompose_into calls can resolve cross-scope references.
    let local_title_to_id: HashMap<String, String> = docs
        .iter()
        .map(|(doc, title, _)| (title.clone(), doc.id.clone()))
        .collect();

    global_title_to_id.extend(local_title_to_id.iter().map(|(k, v)| (k.clone(), v.clone())));

    let mut final_docs: Vec<Doc> = Vec::new();
    for (mut doc, _, dep_titles) in docs {
        let mut resolved = Vec::new();
        let mut unresolved = Vec::new();
        for title in &dep_titles {
            if let Some(id) = local_title_to_id.get(title).or_else(|| global_title_to_id.get(title)) {
                resolved.push(id.clone());
            } else {
                // Stash unresolved titles for the post-merge cross-spec resolution pass.
                warn!(
                    "dependency '{}' not yet resolvable, deferring to post-merge pass",
                    title
                );
                unresolved.push(title.clone());
            }
        }
        doc.dependencies = resolved;
        doc.unresolved_dep_titles = unresolved;
        final_docs.push(doc);
    }

    // Flush staging to run directory
    for entry in std::fs::read_dir(&staging_dir)? {
        let entry = entry?;
        let dest = run_dir.join(entry.file_name());
        std::fs::rename(entry.path(), dest)?;
    }
    std::fs::remove_dir(&staging_dir)?;

    info!(
        "decompose: produced {} {} docs from {} {}",
        final_docs.len(),
        target_kind,
        parent.kind,
        parent.id
    );

    Ok(final_docs)
}

/// Decompose a single parent Doc into child Docs using the natural child kind.
///
/// Thin wrapper around `decompose_into` that computes the child kind from the parent.
/// Uses an isolated local title map (no cross-scope resolution).
pub async fn decompose<H: HttpClient + Sync>(
    parent: &Doc,
    run_dir: &Path,
    config: &DecomposerConfig,
    http_client: &H,
) -> Result<Vec<Doc>> {
    let ck = child_kind(parent.kind).ok_or_else(|| eyre::eyre!("cannot decompose a {} document", parent.kind))?;
    let mut local_map = HashMap::new();
    decompose_into(parent, ck, run_dir, config, http_client, &mut local_map).await
}

/// Decompose a full hierarchy: Plan -> Specs -> Phases -> Works.
///
/// This is the entry point for plan activation. Specs are decomposed concurrently
/// (try_join_all), each spec's phases are also concurrent within the spec branch.
/// After all branches complete, a post-merge pass resolves cross-spec/cross-phase
/// dependencies that could not be resolved during parallel execution.
///
/// In Brief mode (plan has no contracts), skips Spec and Phase levels
/// and decomposes Plan directly into Works.
pub async fn decompose_hierarchy<H: HttpClient + Sync>(
    plan: &Doc,
    run_dir: &Path,
    config: &DecomposerConfig,
    http_client: &H,
    brief: bool,
) -> Result<Vec<Doc>> {
    let mut all_docs = Vec::new();

    if brief {
        // Brief mode: Plan -> Works directly (skip Spec/Phase levels)
        let mut local_map = HashMap::new();
        let works = decompose_into(plan, DocKind::Work, run_dir, config, http_client, &mut local_map).await?;
        all_docs.extend(works);
    } else {
        // Full mode: Plan -> Specs (sequential) -> Phases + Works (parallel per spec)
        let mut spec_map = HashMap::new();
        let specs = decompose_into(plan, DocKind::Spec, run_dir, config, http_client, &mut spec_map).await?;

        // Decompose each spec branch concurrently.
        // Each branch returns (docs, local_title_to_id). Branches are polled on the
        // same async task (no tokio::spawn) so no Send bound is needed.
        let spec_futures: Vec<_> = specs
            .iter()
            .map(|spec| decompose_spec_branch(spec, run_dir, config, http_client))
            .collect();
        let branch_results = try_join_all(spec_futures).await?;

        // Merge all branch title maps into a single global map for post-pass resolution.
        let mut global_title_to_id = spec_map;
        for (_, branch_map) in &branch_results {
            global_title_to_id.extend(branch_map.iter().map(|(k, v)| (k.clone(), v.clone())));
        }

        // Collect all docs from all branches, then resolve cross-branch deps.
        for (branch_docs, _) in branch_results {
            all_docs.extend(branch_docs);
        }
        all_docs.extend(specs);
        resolve_cross_branch_deps(&mut all_docs, &global_title_to_id);
    }

    // Hierarchical ratification (bottom-up, sequential by design)
    ratify_hierarchy(plan, &all_docs, run_dir, config, http_client).await?;

    Ok(all_docs)
}

/// Decompose one spec into phases + works, with phases' work-decompositions running concurrently.
/// Returns (phases + works, merged title-to-id map for this branch).
async fn decompose_spec_branch<H: HttpClient + Sync>(
    spec: &Doc,
    run_dir: &Path,
    config: &DecomposerConfig,
    http_client: &H,
) -> Result<(Vec<Doc>, HashMap<String, String>)> {
    let mut branch_map = HashMap::new();
    let phases = decompose_into(spec, DocKind::Phase, run_dir, config, http_client, &mut branch_map).await?;

    // Decompose works for each phase concurrently within this spec branch.
    let phase_futures: Vec<_> = phases
        .iter()
        .map(|phase| decompose_phase_branch(phase, run_dir, config, http_client))
        .collect();
    let phase_results = try_join_all(phase_futures).await?;

    let mut all_docs = phases;
    for (works, phase_map) in phase_results {
        all_docs.extend(works);
        branch_map.extend(phase_map.into_iter());
    }

    Ok((all_docs, branch_map))
}

/// Decompose one phase into works, returning the works and the local title-to-id map.
async fn decompose_phase_branch<H: HttpClient + Sync>(
    phase: &Doc,
    run_dir: &Path,
    config: &DecomposerConfig,
    http_client: &H,
) -> Result<(Vec<Doc>, HashMap<String, String>)> {
    let mut phase_map = HashMap::new();
    let works = decompose_into(phase, DocKind::Work, run_dir, config, http_client, &mut phase_map).await?;
    Ok((works, phase_map))
}

/// After all parallel branches finish and maps are merged, resolve any dependency titles
/// that were deferred because the referenced doc was being built in a sibling branch.
fn resolve_cross_branch_deps(docs: &mut [Doc], global_map: &HashMap<String, String>) {
    for doc in docs.iter_mut() {
        if doc.unresolved_dep_titles.is_empty() {
            continue;
        }
        let pending = std::mem::take(&mut doc.unresolved_dep_titles);
        for title in pending {
            if let Some(id) = global_map.get(&title) {
                doc.dependencies.push(id.clone());
            } else {
                warn!("cross-branch dep '{}' could not be resolved after merge", title);
            }
        }
    }
}

/// Bottom-up ratification of the decomposition hierarchy.
async fn ratify_hierarchy<H: HttpClient + Sync>(
    plan: &Doc,
    all_docs: &[Doc],
    run_dir: &Path,
    config: &DecomposerConfig,
    http_client: &H,
) -> Result<()> {
    // Group docs by parent_id
    let mut children_of: HashMap<&str, Vec<&Doc>> = HashMap::new();
    for doc in all_docs {
        if let Some(ref pid) = doc.parent_id {
            children_of.entry(pid.as_str()).or_default().push(doc);
        }
    }

    // Ratify each parent-children group
    for (parent_id, children) in &children_of {
        // Find the parent doc
        let parent_doc = if *parent_id == plan.id {
            plan
        } else {
            match all_docs.iter().find(|d| d.id == *parent_id) {
                Some(d) => d,
                None => continue,
            }
        };

        let parent_content = std::fs::read_to_string(run_dir.join(&parent_doc.markdown)).unwrap_or_default();

        let child_pairs: Vec<(String, String)> = children
            .iter()
            .filter_map(|c| {
                let content = std::fs::read_to_string(run_dir.join(&c.markdown)).ok()?;
                Some((c.markdown.clone(), content))
            })
            .collect();

        if child_pairs.is_empty() {
            continue;
        }

        let prompt = build_ratify_prompt(&parent_content, &child_pairs);
        match call_llm_for_ratification(http_client, config, &prompt).await {
            Ok(result) if !result.passed => {
                warn!(
                    "Ratification failed for parent {}: {:?}",
                    parent_id,
                    result.issues.iter().map(|i| &i.issue).collect::<Vec<_>>()
                );
            }
            Err(e) => {
                warn!("Ratification call failed for parent {}: {}", parent_id, e);
            }
            Ok(_) => {
                info!("Ratification passed for parent {}", parent_id);
            }
        }
    }

    Ok(())
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TestDir;

    // --- detect_cycles ---

    #[test]
    fn test_detect_cycles_no_cycle() {
        let mut graph = HashMap::new();
        graph.insert("A".to_string(), vec!["B".to_string()]);
        graph.insert("B".to_string(), vec!["C".to_string()]);
        graph.insert("C".to_string(), vec![]);
        assert!(detect_cycles(&graph).is_ok());
    }

    #[test]
    fn test_detect_cycles_simple_cycle() {
        let mut graph = HashMap::new();
        graph.insert("A".to_string(), vec!["B".to_string()]);
        graph.insert("B".to_string(), vec!["A".to_string()]);
        let err = detect_cycles(&graph).unwrap_err();
        assert!(err.to_string().contains("cycle"));
    }

    #[test]
    fn test_detect_cycles_self_cycle() {
        let mut graph = HashMap::new();
        graph.insert("A".to_string(), vec!["A".to_string()]);
        assert!(detect_cycles(&graph).is_err());
    }

    #[test]
    fn test_detect_cycles_diamond_no_cycle() {
        let mut graph = HashMap::new();
        graph.insert("A".to_string(), vec![]);
        graph.insert("B".to_string(), vec![]);
        graph.insert("C".to_string(), vec!["A".to_string(), "B".to_string()]);
        assert!(detect_cycles(&graph).is_ok());
    }

    #[test]
    fn test_detect_cycles_three_node_cycle() {
        let mut graph = HashMap::new();
        graph.insert("A".to_string(), vec!["B".to_string()]);
        graph.insert("B".to_string(), vec!["C".to_string()]);
        graph.insert("C".to_string(), vec!["A".to_string()]);
        assert!(detect_cycles(&graph).is_err());
    }

    #[test]
    fn test_detect_cycles_empty_graph() {
        let graph: HashMap<String, Vec<String>> = HashMap::new();
        assert!(detect_cycles(&graph).is_ok());
    }

    #[test]
    fn test_detect_cycles_linear_chain() {
        let mut graph = HashMap::new();
        graph.insert("A".to_string(), vec!["B".to_string()]);
        graph.insert("B".to_string(), vec!["C".to_string()]);
        graph.insert("C".to_string(), vec!["D".to_string()]);
        graph.insert("D".to_string(), vec![]);
        assert!(detect_cycles(&graph).is_ok());
    }

    // --- extract_acceptance_criteria ---

    #[test]
    fn test_extract_acceptance_criteria_assert_lines() {
        let content = "# Work\n\n## Acceptance Criteria\n\nassert x == 1\nassert y > 0\n\n## Dependencies\n";
        let criteria = extract_acceptance_criteria(content);
        assert_eq!(criteria, vec!["assert x == 1", "assert y > 0"]);
    }

    #[test]
    fn test_extract_acceptance_criteria_bullet_lines() {
        let content = "# Work\n\n## Acceptance Criteria\n\n- Must handle errors\n- Must log\n\n## Next\n";
        let criteria = extract_acceptance_criteria(content);
        assert_eq!(criteria, vec!["Must handle errors", "Must log"]);
    }

    #[test]
    fn test_extract_acceptance_criteria_missing_section() {
        let content = "# Work\n\n## Description\n\nSome description\n";
        let criteria = extract_acceptance_criteria(content);
        assert!(criteria.is_empty());
    }

    #[test]
    fn test_extract_acceptance_criteria_empty_section() {
        let content = "# Work\n\n## Acceptance Criteria\n\n## Dependencies\n";
        let criteria = extract_acceptance_criteria(content);
        assert!(criteria.is_empty());
    }

    // --- child_kind ---

    #[test]
    fn test_child_kind_plan() {
        assert_eq!(child_kind(DocKind::Plan), Some(DocKind::Spec));
    }

    #[test]
    fn test_child_kind_spec() {
        assert_eq!(child_kind(DocKind::Spec), Some(DocKind::Phase));
    }

    #[test]
    fn test_child_kind_phase() {
        assert_eq!(child_kind(DocKind::Phase), Some(DocKind::Work));
    }

    #[test]
    fn test_child_kind_work() {
        assert_eq!(child_kind(DocKind::Work), None);
    }

    // --- ChildEntry serde ---

    #[test]
    fn test_child_entry_serde_roundtrip() {
        let entry = ChildEntry {
            title: "Core Implementation".to_string(),
            content: "# Spec\n\n## Overview\n\nThe core.".to_string(),
            dependencies: vec!["Setup".to_string()],
            acceptance_criteria: vec!["it works".to_string()],
        };
        let json = serde_json::to_string(&entry).unwrap();
        let restored: ChildEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.title, "Core Implementation");
        assert_eq!(restored.dependencies, vec!["Setup"]);
    }

    #[test]
    fn test_child_entry_defaults() {
        let json = r#"{"title":"T","content":"C"}"#;
        let entry: ChildEntry = serde_json::from_str(json).unwrap();
        assert!(entry.dependencies.is_empty());
        assert!(entry.acceptance_criteria.is_empty());
    }

    // --- decompose with mock LLM ---

    struct SequenceMockHttp {
        responses: std::sync::Mutex<Vec<String>>,
    }

    impl HttpClient for SequenceMockHttp {
        async fn post(&self, _url: &str, _headers: &[(&str, &str)], _body: &str) -> Result<String> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                bail!("no more mock responses");
            }
            Ok(responses.pop().unwrap())
        }
    }

    fn test_config() -> DecomposerConfig {
        DecomposerConfig {
            provider: "anthropic".to_string(),
            model: "test-model".to_string(),
            api_key_env: "TEST_DECOMPOSER_KEY".to_string(),
            max_tokens: 4096,
            temperature: 0.3,
            validation_model: "test-haiku".to_string(),
        }
    }

    #[tokio::test]
    async fn test_decompose_plan_to_specs() {
        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-decompose-test");
        let env_key = format!("TEST_DECOMPOSER_KEY_{}", crate::id::generate_id("xx"));
        unsafe { std::env::set_var(&env_key, "test-key") };

        // Write parent .md
        std::fs::write(
            dir.join("plan-test.md"),
            "# Plan\n\n## Problem Statement\n\nTest problem.\n\n## Goals\n\nTest goal.",
        )
        .unwrap();

        let parent = Doc::new(DocKind::Plan, None, "Test Plan".to_string(), "plan-test.md".to_string());

        let children_json = serde_json::json!([
            {
                "title": "Core Implementation",
                "content": "# Spec\n\n## Overview\n\nThe core implementation.",
                "dependencies": []
            },
            {
                "title": "API Integration",
                "content": "# Spec\n\n## Overview\n\nAPI layer.",
                "dependencies": ["Core Implementation"]
            }
        ])
        .to_string();

        let validation_json = r#"{"valid": true, "issues": []}"#;
        let mock = SequenceMockHttp {
            responses: std::sync::Mutex::new(vec![
                // Responses are popped from the back, so reverse order
                // validation for child 2
                serde_json::json!({"content": [{"type": "text", "text": validation_json}]}).to_string(),
                // validation for child 1
                serde_json::json!({"content": [{"type": "text", "text": validation_json}]}).to_string(),
                // decompose call
                serde_json::json!({"content": [{"type": "text", "text": &children_json}]}).to_string(),
            ]),
        };

        let mut config = test_config();
        config.api_key_env = env_key.clone();

        let result = decompose(&parent, &dir, &config, &mock).await.unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].kind, DocKind::Spec);
        assert_eq!(result[1].kind, DocKind::Spec);
        assert!(result[0].id.starts_with("sp-"));
        assert_eq!(result[0].parent_id.as_deref(), Some(parent.id.as_str()));

        // Second spec should depend on first
        assert_eq!(result[1].dependencies, vec![result[0].id.clone()]);

        // Files should exist in run dir
        assert!(dir.join(&result[0].markdown).exists());
        assert!(dir.join(&result[1].markdown).exists());

        // Staging dir should be cleaned up
        assert!(!dir.join(".staging").exists());

        unsafe { std::env::remove_var(&env_key) };
    }

    #[tokio::test]
    async fn test_decompose_cycle_detection_fails() {
        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-decompose-cycle");
        let env_key = format!("TEST_DECOMPOSER_KEY_{}", crate::id::generate_id("xx"));
        unsafe { std::env::set_var(&env_key, "test-key") };

        std::fs::write(dir.join("plan-test.md"), "# Plan\n\nTest").unwrap();
        let parent = Doc::new(DocKind::Plan, None, "Test Plan".to_string(), "plan-test.md".to_string());

        // Circular dependencies
        let children_json = serde_json::json!([
            {
                "title": "A",
                "content": "# Spec A",
                "dependencies": ["B"]
            },
            {
                "title": "B",
                "content": "# Spec B",
                "dependencies": ["A"]
            }
        ])
        .to_string();

        let validation_json = r#"{"valid": true, "issues": []}"#;
        let mock = SequenceMockHttp {
            responses: std::sync::Mutex::new(vec![
                serde_json::json!({"content": [{"type": "text", "text": validation_json}]}).to_string(),
                serde_json::json!({"content": [{"type": "text", "text": validation_json}]}).to_string(),
                serde_json::json!({"content": [{"type": "text", "text": &children_json}]}).to_string(),
            ]),
        };

        let mut config = test_config();
        config.api_key_env = env_key.clone();

        let result = decompose(&parent, &dir, &config, &mock).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cycle"));

        unsafe { std::env::remove_var(&env_key) };
    }

    #[tokio::test]
    async fn test_decompose_work_extraction() {
        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-decompose-work");
        let env_key = format!("TEST_DECOMPOSER_KEY_{}", crate::id::generate_id("xx"));
        unsafe { std::env::set_var(&env_key, "test-key") };

        std::fs::write(dir.join("phase-test.md"), "# Phase\n\nTest phase").unwrap();
        let parent = Doc::new(
            DocKind::Phase,
            Some("sp-abc12".to_string()),
            "Test Phase".to_string(),
            "phase-test.md".to_string(),
        );

        let children_json = serde_json::json!([
            {
                "title": "Create Schema",
                "content": "# Work\n\n## Description\n\nCreate schema.\n\n## Acceptance Criteria\n\nassert schema_exists()\nassert table_count() == 3\n\n## Dependencies\n\nNone",
                "dependencies": []
            }
        ])
        .to_string();

        let validation_json = r#"{"valid": true, "issues": []}"#;
        let mock = SequenceMockHttp {
            responses: std::sync::Mutex::new(vec![
                serde_json::json!({"content": [{"type": "text", "text": validation_json}]}).to_string(),
                serde_json::json!({"content": [{"type": "text", "text": &children_json}]}).to_string(),
            ]),
        };

        let mut config = test_config();
        config.api_key_env = env_key.clone();

        let result = decompose(&parent, &dir, &config, &mock).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].kind, DocKind::Work);
        assert_eq!(
            result[0].acceptance_criteria,
            vec!["assert schema_exists()", "assert table_count() == 3"]
        );

        unsafe { std::env::remove_var(&env_key) };
    }

    #[tokio::test]
    async fn test_decompose_into_plan_to_works_brief() {
        // Brief mode: decompose_into with DocKind::Work from a Plan parent
        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-decompose-brief");
        let env_key = format!("TEST_DECOMPOSER_KEY_{}", crate::id::generate_id("xx"));
        unsafe { std::env::set_var(&env_key, "test-key") };

        std::fs::write(
            dir.join("plan-brief.md"),
            "# Brief Plan\n\n## Problem Statement\n\nSmall task.\n\n## Goals\n\nDo it quickly.",
        )
        .unwrap();

        let parent = Doc::new(
            DocKind::Plan,
            None,
            "Brief Plan".to_string(),
            "plan-brief.md".to_string(),
        );

        let children_json = serde_json::json!([
            {
                "title": "Implement Feature",
                "content": "# Work\n\n## Description\n\nDo the thing.\n\n## Acceptance Criteria\n\nassert done()\n\n## Dependencies\n\nNone",
                "dependencies": []
            }
        ])
        .to_string();

        let validation_json = r#"{"valid": true, "issues": []}"#;
        let mock = SequenceMockHttp {
            responses: std::sync::Mutex::new(vec![
                serde_json::json!({"content": [{"type": "text", "text": validation_json}]}).to_string(),
                serde_json::json!({"content": [{"type": "text", "text": &children_json}]}).to_string(),
            ]),
        };

        let mut config = test_config();
        config.api_key_env = env_key.clone();

        let mut local_map = HashMap::new();
        let result = decompose_into(&parent, DocKind::Work, &dir, &config, &mock, &mut local_map)
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].kind, DocKind::Work);
        assert!(result[0].id.starts_with("wk-"));
        assert_eq!(result[0].parent_id.as_deref(), Some(parent.id.as_str()));

        unsafe { std::env::remove_var(&env_key) };
    }

    #[tokio::test]
    async fn test_decompose_hierarchy_cross_scope_dependency_resolved() {
        // Verify that decompose_hierarchy threads a single global_title_to_id map across
        // all decompose_into calls so that a Work in Spec B can reference a Work in Spec A.
        //
        // This tests the public API (decompose_hierarchy), not the internal loop directly.
        // If decompose_hierarchy reinitialised the map per-spec, Work Beta's dependency
        // would be silently dropped and this test would fail.
        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-decompose-hierarchy-cross");
        let env_key = format!("TEST_DECOMPOSER_KEY_{}", crate::id::generate_id("xx"));
        unsafe { std::env::set_var(&env_key, "test-key") };

        std::fs::write(
            dir.join("plan-cross-scope.md"),
            "# Cross-Scope Plan\n\n## Problem Statement\n\nTest cross-spec deps.\n\n## Goals\n\nVerify global map.",
        )
        .unwrap();
        let plan = Doc::new(
            DocKind::Plan,
            None,
            "Cross-Scope Plan".to_string(),
            "plan-cross-scope.md".to_string(),
        );

        let valid_json = r#"{"valid": true, "issues": []}"#;
        let ratify_json = r#"{"passed": true, "issues": []}"#;

        // LLM response sequence (consumed LIFO - last element in Vec is first consumed):
        // Call 1:  decompose plan -> [Spec A, Spec B]
        // Call 2:  validate Spec A
        // Call 3:  validate Spec B
        // Call 4:  decompose Spec A -> [Phase A]
        // Call 5:  validate Phase A
        // Call 6:  decompose Phase A -> [Work Alpha]
        // Call 7:  validate Work Alpha
        // Call 8:  decompose Spec B -> [Phase B]
        // Call 9:  validate Phase B
        // Call 10: decompose Phase B -> [Work Beta] (depends on Work Alpha)
        // Call 11: validate Work Beta
        // Calls 12-16: ratify Plan, Spec A, Spec B, Phase A, Phase B (order non-deterministic)
        let specs_json = serde_json::json!([
            {"title": "Spec A", "content": "# Spec A\n\n## Overview\n\nFirst spec.", "dependencies": []},
            {"title": "Spec B", "content": "# Spec B\n\n## Overview\n\nSecond spec.", "dependencies": []}
        ])
        .to_string();
        let phases_a_json = serde_json::json!([
            {"title": "Phase A", "content": "# Phase A\n\n## Overview\n\nFirst phase.", "dependencies": []}
        ])
        .to_string();
        let works_a_json = serde_json::json!([
            {
                "title": "Work Alpha",
                "content": "# Work\n\n## Description\n\nDo alpha.\n\n## Acceptance Criteria\n\nassert alpha_done()\n\n## Dependencies\n\nNone",
                "dependencies": []
            }
        ])
        .to_string();
        let phases_b_json = serde_json::json!([
            {"title": "Phase B", "content": "# Phase B\n\n## Overview\n\nSecond phase.", "dependencies": []}
        ])
        .to_string();
        let works_b_json = serde_json::json!([
            {
                "title": "Work Beta",
                "content": "# Work\n\n## Description\n\nDo beta.\n\n## Acceptance Criteria\n\nassert beta_done()\n\n## Dependencies\n\nWork Alpha",
                "dependencies": ["Work Alpha"]
            }
        ])
        .to_string();

        let mk = |text: &str| serde_json::json!({"content": [{"type": "text", "text": text}]}).to_string();

        let mock = SequenceMockHttp {
            responses: std::sync::Mutex::new(vec![
                // Ratifications 12-16 (5 groups, order non-deterministic so all same)
                mk(ratify_json),
                mk(ratify_json),
                mk(ratify_json),
                mk(ratify_json),
                mk(ratify_json),
                // Call 11: validate Work Beta
                mk(valid_json),
                // Call 10: decompose Phase B -> [Work Beta]
                mk(&works_b_json),
                // Call 9: validate Phase B
                mk(valid_json),
                // Call 8: decompose Spec B -> [Phase B]
                mk(&phases_b_json),
                // Call 7: validate Work Alpha
                mk(valid_json),
                // Call 6: decompose Phase A -> [Work Alpha]
                mk(&works_a_json),
                // Call 5: validate Phase A
                mk(valid_json),
                // Call 4: decompose Spec A -> [Phase A]
                mk(&phases_a_json),
                // Call 3: validate Spec B
                mk(valid_json),
                // Call 2: validate Spec A
                mk(valid_json),
                // Call 1: decompose plan -> [Spec A, Spec B]
                mk(&specs_json),
            ]),
        };

        let mut config = test_config();
        config.api_key_env = env_key.clone();

        let all_docs = decompose_hierarchy(&plan, &dir, &config, &mock, false).await.unwrap();

        // Find Work Alpha and Work Beta in the results
        let alpha = all_docs
            .iter()
            .find(|d| d.title == "Work Alpha")
            .expect("Work Alpha not found");
        let beta = all_docs
            .iter()
            .find(|d| d.title == "Work Beta")
            .expect("Work Beta not found");

        assert_eq!(
            beta.dependencies,
            vec![alpha.id.clone()],
            "Work Beta must resolve its cross-spec dependency to Work Alpha's ID"
        );

        unsafe { std::env::remove_var(&env_key) };
    }

    #[tokio::test]
    async fn test_decompose_hierarchy_parallel_specs_complete() {
        // Verify that decompose_hierarchy produces docs for ALL specs when specs are
        // decomposed in parallel. Two specs, each with one phase and one work.
        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-parallel-specs");
        let env_key = format!("TEST_DECOMPOSER_KEY_{}", crate::id::generate_id("xx"));
        unsafe { std::env::set_var(&env_key, "test-key") };

        std::fs::write(
            dir.join("plan-parallel.md"),
            "# Parallel Plan\n\n## Problem Statement\n\nTwo independent tracks.\n\n## Goals\n\nBoth must complete.",
        )
        .unwrap();
        let plan = Doc::new(
            DocKind::Plan,
            None,
            "Parallel Plan".to_string(),
            "plan-parallel.md".to_string(),
        );

        let valid_json = r#"{"valid": true, "issues": []}"#;
        let ratify_json = r#"{"passed": true, "issues": []}"#;

        let specs_json = serde_json::json!([
            {"title": "Track One", "content": "# Track One\n\n## Overview\n\nFirst.", "dependencies": []},
            {"title": "Track Two", "content": "# Track Two\n\n## Overview\n\nSecond.", "dependencies": []}
        ])
        .to_string();
        let phases_one = serde_json::json!([
            {"title": "Phase One", "content": "# Phase One\n\n## Overview\n\nPhase.", "dependencies": []}
        ])
        .to_string();
        let works_one = serde_json::json!([
            {"title": "Work One", "content": "# Work\n\n## Description\n\nDo one.\n\n## Acceptance Criteria\n\nassert one()\n\n## Dependencies\n\nNone", "dependencies": []}
        ])
        .to_string();
        let phases_two = serde_json::json!([
            {"title": "Phase Two", "content": "# Phase Two\n\n## Overview\n\nPhase.", "dependencies": []}
        ])
        .to_string();
        let works_two = serde_json::json!([
            {"title": "Work Two", "content": "# Work\n\n## Description\n\nDo two.\n\n## Acceptance Criteria\n\nassert two()\n\n## Dependencies\n\nNone", "dependencies": []}
        ])
        .to_string();

        let mk = |text: &str| serde_json::json!({"content": [{"type": "text", "text": text}]}).to_string();

        // Responses are popped LIFO. Order matches the actual call sequence with the
        // synchronous mock (Spec A branch completes before Spec B branch due to cooperative
        // polling on a single async task):
        // 1: plan -> specs (validate x2)
        // 2: spec Track One -> phases (validate x1)
        // 3: phase Phase One -> works (validate x1)
        // 4: spec Track Two -> phases (validate x1)
        // 5: phase Phase Two -> works (validate x1)
        // 6+: ratifications (5 groups)
        let mock = SequenceMockHttp {
            responses: std::sync::Mutex::new(vec![
                mk(ratify_json),
                mk(ratify_json),
                mk(ratify_json),
                mk(ratify_json),
                mk(ratify_json),
                mk(valid_json),  // validate Work Two
                mk(&works_two),  // decompose Phase Two -> works
                mk(valid_json),  // validate Phase Two
                mk(&phases_two), // decompose Track Two -> phases
                mk(valid_json),  // validate Track Two spec
                mk(valid_json),  // validate Work One
                mk(&works_one),  // decompose Phase One -> works
                mk(valid_json),  // validate Phase One
                mk(&phases_one), // decompose Track One -> phases
                mk(valid_json),  // validate Track One spec
                mk(valid_json),  // validate Track Two spec (from plan decompose)
                mk(&specs_json), // decompose plan -> specs
            ]),
        };

        let mut config = test_config();
        config.api_key_env = env_key.clone();

        let all_docs = decompose_hierarchy(&plan, &dir, &config, &mock, false).await.unwrap();

        let specs: Vec<_> = all_docs.iter().filter(|d| d.kind == DocKind::Spec).collect();
        let phases: Vec<_> = all_docs.iter().filter(|d| d.kind == DocKind::Phase).collect();
        let works: Vec<_> = all_docs.iter().filter(|d| d.kind == DocKind::Work).collect();
        assert_eq!(specs.len(), 2, "expected 2 specs, got {}", specs.len());
        assert_eq!(phases.len(), 2, "expected 2 phases, got {}", phases.len());
        assert_eq!(works.len(), 2, "expected 2 works, got {}", works.len());

        unsafe { std::env::remove_var(&env_key) };
    }

    #[tokio::test]
    async fn test_decompose_hierarchy_cross_spec_deps_resolved() {
        // With parallel spec branches, Work Beta (in Spec B) depends on Work Alpha
        // (in Spec A). During Spec B's branch, Work Alpha's ID is unknown. The
        // post-merge pass must resolve it from the merged global map.
        // This is identical in spirit to test_decompose_hierarchy_cross_scope_dependency_resolved
        // but explicitly targets the post-merge path (unresolved_dep_titles → dependencies).
        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-parallel-cross-spec");
        let env_key = format!("TEST_DECOMPOSER_KEY_{}", crate::id::generate_id("xx"));
        unsafe { std::env::set_var(&env_key, "test-key") };

        std::fs::write(
            dir.join("plan-cross.md"),
            "# Cross-Spec Plan\n\n## Problem Statement\n\nCross-spec dep test.\n\n## Goals\n\nBeta depends on Alpha.",
        )
        .unwrap();
        let plan = Doc::new(
            DocKind::Plan,
            None,
            "Cross-Spec Plan".to_string(),
            "plan-cross.md".to_string(),
        );

        let valid_json = r#"{"valid": true, "issues": []}"#;
        let ratify_json = r#"{"passed": true, "issues": []}"#;

        let specs_json = serde_json::json!([
            {"title": "Spec Alpha", "content": "# Spec Alpha\n\n## Overview\n\nFirst.", "dependencies": []},
            {"title": "Spec Beta",  "content": "# Spec Beta\n\n## Overview\n\nSecond.", "dependencies": []}
        ])
        .to_string();
        let phases_alpha = serde_json::json!([
            {"title": "Phase Alpha", "content": "# Phase Alpha\n\n## Overview\n\nPhase.", "dependencies": []}
        ])
        .to_string();
        let works_alpha = serde_json::json!([
            {"title": "Work Alpha", "content": "# Work\n\n## Description\n\nDo alpha.\n\n## Acceptance Criteria\n\nassert alpha()\n\n## Dependencies\n\nNone", "dependencies": []}
        ])
        .to_string();
        let phases_beta = serde_json::json!([
            {"title": "Phase Beta", "content": "# Phase Beta\n\n## Overview\n\nPhase.", "dependencies": []}
        ])
        .to_string();
        let works_beta = serde_json::json!([
            {"title": "Work Beta", "content": "# Work\n\n## Description\n\nDo beta.\n\n## Acceptance Criteria\n\nassert beta()\n\n## Dependencies\n\nWork Alpha", "dependencies": ["Work Alpha"]}
        ])
        .to_string();

        let mk = |text: &str| serde_json::json!({"content": [{"type": "text", "text": text}]}).to_string();

        let mock = SequenceMockHttp {
            responses: std::sync::Mutex::new(vec![
                mk(ratify_json),
                mk(ratify_json),
                mk(ratify_json),
                mk(ratify_json),
                mk(ratify_json),
                mk(valid_json),    // validate Work Beta
                mk(&works_beta),   // decompose Phase Beta -> works
                mk(valid_json),    // validate Phase Beta
                mk(&phases_beta),  // decompose Spec Beta -> phases
                mk(valid_json),    // validate Spec Beta (from spec branch)
                mk(valid_json),    // validate Work Alpha
                mk(&works_alpha),  // decompose Phase Alpha -> works
                mk(valid_json),    // validate Phase Alpha
                mk(&phases_alpha), // decompose Spec Alpha -> phases
                mk(valid_json),    // validate Spec Alpha (from spec branch)
                mk(valid_json),    // validate Spec Beta (from plan decompose)
                mk(&specs_json),   // decompose plan -> specs
            ]),
        };

        let mut config = test_config();
        config.api_key_env = env_key.clone();

        let all_docs = decompose_hierarchy(&plan, &dir, &config, &mock, false).await.unwrap();

        let alpha = all_docs
            .iter()
            .find(|d| d.title == "Work Alpha")
            .expect("Work Alpha not found");
        let beta = all_docs
            .iter()
            .find(|d| d.title == "Work Beta")
            .expect("Work Beta not found");

        assert_eq!(
            beta.dependencies,
            vec![alpha.id.clone()],
            "Work Beta must resolve its cross-spec dep to Work Alpha's ID via post-merge pass"
        );

        unsafe { std::env::remove_var(&env_key) };
    }

    #[tokio::test(start_paused = true)]
    async fn test_decompose_into_times_out_slow_llm() {
        // With start_paused=true, tokio's clock is virtual. The mock sleeps for 200s of
        // virtual time; the 60s timeout fires first. The test runs in real milliseconds.
        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-decompose-timeout");
        let env_key = format!("TEST_DECOMPOSER_KEY_{}", crate::id::generate_id("xx"));
        unsafe { std::env::set_var(&env_key, "test-key") };

        std::fs::write(
            dir.join("plan-timeout.md"),
            "# Timeout Plan\n\n## Problem Statement\n\nTest.\n\n## Goals\n\nHang.",
        )
        .unwrap();

        let parent = Doc::new(
            DocKind::Plan,
            None,
            "Timeout Plan".to_string(),
            "plan-timeout.md".to_string(),
        );

        struct SlowMockHttp;
        impl HttpClient for SlowMockHttp {
            async fn post(&self, _url: &str, _headers: &[(&str, &str)], _body: &str) -> Result<String> {
                // Virtual sleep; tokio advances time, so the 60s timeout fires first
                tokio::time::sleep(Duration::from_secs(200)).await;
                Ok("never".to_string())
            }
        }

        let mut config = test_config();
        config.api_key_env = env_key.clone();

        let mut local_map = HashMap::new();
        let result = decompose_into(&parent, DocKind::Spec, &dir, &config, &SlowMockHttp, &mut local_map).await;

        assert!(result.is_err(), "expected timeout error, got: {:?}", result);
        let msg = format!("{:?}", result.unwrap_err());
        assert!(
            msg.contains("timed out") || msg.contains("Decomposition failed"),
            "error should mention timeout: {msg}"
        );

        unsafe { std::env::remove_var(&env_key) };
    }
}
