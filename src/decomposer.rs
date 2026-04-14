//! Standalone plan decomposer: receives plan markdown as a string, calls an LLM,
//! and builds the full Plan/Spec/Phase/Work hierarchy in memory.
//!
//! This is a system call (function), NOT an agent. It has no session, FSM, or
//! iteration loop. The Coordinator invokes it before execution begins, and can
//! re-invoke it for targeted re-decomposition during execution.

use std::collections::HashMap;
use std::time::Duration;

use eyre::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, instrument, trace, warn};

use futures::future::{join_all, try_join_all};

use crate::config::DecomposerConfig;
use crate::domain::criteria::AcceptanceCriteria;
use crate::domain::doc::DocKind;
use crate::domain::phase::{Phase, PhaseStatus};
use crate::domain::plan::{Plan, PlanStatus};
use crate::domain::spec::{Spec, SpecStatus};
use crate::domain::work::{Work, WorkStatus};
use crate::prompts::SECTION_AC;
use crate::validator::client::HttpClient;

const LLM_CALL_TIMEOUT_SECS: u64 = 180;

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

/// In-memory child record produced by decompose_into.
///
/// Replaces the old `Doc` intermediary + file round-trip. Content lives here
/// in memory until `persist_hierarchy` writes it to JSONL and docs/loopr/.
#[derive(Debug)]
struct ChildRecord {
    id: String,
    kind: DocKind,
    parent_id: Option<String>,
    title: String,
    content: String,
    dependencies: Vec<String>,
    unresolved_dep_titles: Vec<String>,
    acceptance_criteria: Vec<String>,
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

/// Build the decomposition prompt: template instructions + template content + parent content.
/// `target_kind` is the kind of document to produce (not the parent kind).
fn build_decompose_prompt(target_kind: DocKind, parent_content: &str) -> Result<String> {
    let prompts = crate::prompts::store();
    let (instructions, template_text) = match target_kind {
        DocKind::Spec => (&prompts.decompose_spec, include_str!("../docs/templates/spec.md")),
        DocKind::Phase => (&prompts.decompose_phase, include_str!("../docs/templates/phase.md")),
        DocKind::Work => (&prompts.decompose_work, include_str!("../docs/templates/work.md")),
        DocKind::Plan => bail!("cannot decompose into Plan"),
    };
    Ok(format!(
        "{}\n\n## Template\n\n{}\n\n## Parent Document\n\n{}",
        instructions, template_text, parent_content
    ))
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
        if line.starts_with(&format!("## {}", SECTION_AC)) {
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

/// Tool schema for structured decomposition output (Fix 8).
/// Forces the LLM to emit child documents as structured JSON via tool-use,
/// eliminating manual JSON parsing and markdown fence stripping.
fn decomposition_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "submit_decomposition",
        "description": "Submit the decomposed child documents. Call this exactly once with all children.",
        "input_schema": {
            "type": "object",
            "properties": {
                "children": {
                    "type": "array",
                    "description": "The decomposed child documents",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": {
                                "type": "string",
                                "description": "Title of the child document"
                            },
                            "content": {
                                "type": "string",
                                "description": "Full markdown content of the child document"
                            },
                            "dependencies": {
                                "type": "array",
                                "items": {"type": "string"},
                                "description": "IDs of sibling documents this document depends on"
                            },
                            "acceptance_criteria": {
                                "type": "array",
                                "items": {"type": "string"},
                                "description": "Acceptance criteria assertions for this document"
                            },
                        },
                        "required": ["title", "content"]
                    }
                }
            },
            "required": ["children"]
        }
    })
}

/// Call the LLM using tool-use structured output and return child entries.
///
/// Uses Claude's tool-use API so the model fills in a schema natively, eliminating
/// manual JSON parsing and markdown fence stripping that caused parse failures.
#[instrument(skip_all, fields(model = %config.llm.model, provider = %config.provider))]
async fn call_llm_for_children<H: HttpClient + Sync>(
    http_client: &H,
    config: &DecomposerConfig,
    prompt: &str,
) -> Result<Vec<ChildEntry>> {
    let api_key = std::env::var(&config.llm.api_key_env)
        .context(format!("Missing API key env var: {}", config.llm.api_key_env))?;

    let api_url = match config.provider.as_str() {
        "anthropic" => "https://api.anthropic.com/v1/messages",
        other => bail!("Unsupported LLM provider: {}", other),
    };

    // Tool-use generation needs more tokens than validation: use at least 8192 so the
    // structured `children` array isn't truncated before all entries are written.
    const MIN_GENERATION_TOKENS: u32 = 8192;
    let generation_tokens = config.llm.max_tokens.max(MIN_GENERATION_TOKENS);

    let request = serde_json::json!({
        "model": config.llm.model,
        "max_tokens": generation_tokens,
        "temperature": config.llm.temperature,
        "tools": [decomposition_tool_schema()],
        "tool_choice": {"type": "tool", "name": "submit_decomposition"},
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

    // Surface API-level errors before trying to parse content
    if let Some(err) = response.get("error") {
        let snippet: String = response_text.chars().take(500).collect();
        bail!("Anthropic API error: {} (raw: {})", err, snippet);
    }

    let stop_reason = response
        .get("stop_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    debug!(
        "response stop_reason={} content_len={}",
        stop_reason,
        response["content"].as_array().map(|a| a.len()).unwrap_or(0)
    );

    // Fail fast: if the model hit max_tokens, the tool input was truncated and input:{} is empty.
    if stop_reason == "max_tokens" {
        bail!(
            "decomposition hit max_tokens ({} tokens) - tool input was truncated; \
             increase decomposer.max_tokens in config",
            generation_tokens
        );
    }

    // Tool-use response: extract the `input.children` from the tool_use block.
    // Fall back to text parsing if the model didn't use the tool (e.g., provider quirk).
    let children = if let Some(tool_input) = response["content"]
        .as_array()
        .and_then(|blocks| blocks.iter().find(|b| b["type"].as_str() == Some("tool_use")))
        .and_then(|b| b.get("input"))
        .and_then(|input| input.get("children"))
    {
        serde_json::from_value::<Vec<ChildEntry>>(tool_input.clone())
            .context("Failed to parse tool_use input.children as child documents")?
    } else {
        // Fallback: the model returned text instead of a tool call.
        // Strip markdown fences and parse as JSON array.
        let snippet: String = response_text.chars().take(800).collect();
        warn!(
            "model did not use tool, falling back to text parsing (response: {})",
            snippet
        );
        let text = response["content"]
            .as_array()
            .and_then(|blocks| blocks.iter().find(|b| b["type"].as_str() == Some("text")))
            .and_then(|b| b["text"].as_str())
            .ok_or_else(|| eyre::eyre!("LLM returned neither tool_use nor text content (response: {})", snippet))?;

        let json_text = text
            .trim()
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim();

        serde_json::from_str::<Vec<ChildEntry>>(json_text)
            .context("Failed to parse LLM text output as JSON array of child documents")?
    };

    if children.is_empty() {
        bail!("LLM produced zero child documents");
    }

    debug!("got {} children", children.len());
    Ok(children)
}

/// Call the LLM for validation and parse result.
#[instrument(skip_all, fields(model = %config.validation_model, provider = %config.provider))]
async fn call_llm_for_validation<H: HttpClient + Sync>(
    http_client: &H,
    config: &DecomposerConfig,
    prompt: &str,
) -> Result<ValidationResult> {
    // Use the validation model (Haiku) for structural checks
    let mut validation_config = config.clone();
    validation_config.llm.model = config.validation_model.clone();

    let api_key = std::env::var(&validation_config.llm.api_key_env).context(format!(
        "Missing API key env var: {}",
        validation_config.llm.api_key_env
    ))?;

    let api_url = match validation_config.provider.as_str() {
        "anthropic" => "https://api.anthropic.com/v1/messages",
        other => bail!("Unsupported LLM provider: {}", other),
    };

    let request = serde_json::json!({
        "model": validation_config.llm.model,
        "max_tokens": validation_config.llm.max_tokens,
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
#[instrument(skip_all, fields(model = %config.llm.model, provider = %config.provider))]
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
#[instrument(skip_all, fields(model = %config.llm.model, provider = %config.provider))]
async fn call_llm_for_children_raw<H: HttpClient + Sync>(
    http_client: &H,
    config: &DecomposerConfig,
    prompt: &str,
) -> Result<String> {
    let api_key = std::env::var(&config.llm.api_key_env)
        .context(format!("Missing API key env var: {}", config.llm.api_key_env))?;

    let api_url = match config.provider.as_str() {
        "anthropic" => "https://api.anthropic.com/v1/messages",
        other => bail!("Unsupported LLM provider: {}", other),
    };

    let request = serde_json::json!({
        "model": config.llm.model,
        "max_tokens": config.llm.max_tokens,
        "temperature": config.llm.temperature,
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

/// Internal decomposition core: produce `target_kind` children from a parent document.
///
/// All I/O is in-memory. The parent content is passed as a `&str`; child content lives
/// in the returned `Vec<ChildRecord>`. No files are written.
#[instrument(skip_all, fields(parent_id = %parent_id, target_kind = %target_kind))]
async fn decompose_into<H: HttpClient + Sync>(
    parent_content: &str,
    parent_id: &str,
    target_kind: DocKind,
    config: &DecomposerConfig,
    http_client: &H,
) -> Result<Vec<ChildRecord>> {
    // Build prompt and call LLM
    let prompt = build_decompose_prompt(target_kind, parent_content)?;
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
        trace!("validating child title={:?} kind={}", child.title, target_kind);
        let validate_prompt = build_validate_prompt(target_kind, &child.content);
        match call_llm_for_validation(http_client, config, &validate_prompt).await {
            Ok(result) if !result.valid => {
                warn!("Validation failed for '{}': {:?}", child.title, result.issues);
            }
            Err(e) => {
                warn!("Validation call failed for '{}': {}", child.title, e);
            }
            Ok(_) => {
                trace!("validation passed for {:?}", child.title);
            }
        }
    }

    // Cycle detection
    let dep_graph: HashMap<String, Vec<String>> = children
        .iter()
        .map(|c| (c.title.clone(), c.dependencies.clone()))
        .collect();
    detect_cycles(&dep_graph)?;

    // Build in-memory ChildRecords, assigning domain IDs upfront.
    let mut records: Vec<(ChildRecord, Vec<String>)> = Vec::new();

    for child in &children {
        let id = crate::id::generate_id(target_kind.id_prefix());
        let ac = if child.acceptance_criteria.is_empty() && target_kind == DocKind::Work {
            extract_acceptance_criteria(&child.content)
        } else {
            child.acceptance_criteria.clone()
        };

        records.push((
            ChildRecord {
                id,
                kind: target_kind,
                parent_id: Some(parent_id.to_string()),
                title: child.title.clone(),
                content: child.content.clone(),
                dependencies: Vec::new(),
                unresolved_dep_titles: Vec::new(),
                acceptance_criteria: ac,
            },
            child.dependencies.clone(),
        ));
    }

    // Build the local sibling title-to-id map (same-batch, same-parent only).
    let local_title_to_id: HashMap<String, String> =
        records.iter().map(|(r, _)| (r.title.clone(), r.id.clone())).collect();
    // Case-insensitive + whitespace-trimmed fallback: catches the most common LLM errors.
    let local_lower: HashMap<String, String> = local_title_to_id
        .iter()
        .map(|(k, v)| (k.trim().to_lowercase(), v.clone()))
        .collect();

    let mut final_records: Vec<ChildRecord> = Vec::new();
    for (mut record, dep_titles) in records {
        let mut resolved = Vec::new();
        let mut unresolved = Vec::new();
        for title in &dep_titles {
            if let Some(id) = local_title_to_id
                .get(title)
                .or_else(|| local_lower.get(&title.trim().to_lowercase()))
            {
                resolved.push(id.clone());
            } else {
                unresolved.push(title.clone());
            }
        }
        record.dependencies = resolved;
        record.unresolved_dep_titles = unresolved;
        final_records.push(record);
    }

    // Strict failure: unresolved dep titles mean the LLM referenced a sibling that
    // doesn't exist in this batch. The retry loop will re-prompt with the error.
    let unresolved_errors: Vec<String> = final_records
        .iter()
        .filter(|r| !r.unresolved_dep_titles.is_empty())
        .map(|r| format!("'{}' has unresolved deps: {:?}", r.title, r.unresolved_dep_titles))
        .collect();
    if !unresolved_errors.is_empty() {
        bail!("Dependency resolution failed:\n{}", unresolved_errors.join("\n"));
    }

    info!(
        "done parent={} -> {} {}(s) produced",
        parent_id,
        final_records.len(),
        target_kind
    );

    Ok(final_records)
}

/// Decompose a full hierarchy: Plan -> Specs -> Phases -> Works, entirely in memory.
///
/// This is the entry point for plan activation. Receives plan markdown as a string.
/// Specs are decomposed concurrently (join_all), each spec's phases are also concurrent
/// within the spec branch. After all branches complete, a post-merge pass resolves
/// cross-spec/cross-phase dependencies.
///
/// In Brief mode (plan has no contracts), skips Spec and Phase levels and decomposes
/// Plan directly into Works.
///
/// Returns `(hierarchy, partial_err)`.
///
/// `hierarchy` contains typed Plan/Spec/Phase/Work domain records ready for persistence.
/// `partial_err`: when `Some`, one or more spec branches failed but successful branches
/// are still returned. Ratification is skipped on partial failure.
#[instrument(skip_all, fields(brief = %brief))]
pub async fn decompose_hierarchy<H: HttpClient + Sync>(
    plan_markdown: &str,
    config: &DecomposerConfig,
    http_client: &H,
    brief: bool,
) -> Result<(DecomposedHierarchy, Option<String>)> {
    let plan_title = extract_title_from_markdown(plan_markdown);
    let plan_ac = AcceptanceCriteria(extract_acceptance_criteria(plan_markdown));
    let plan_id = crate::id::generate_id("pl");

    let mut all_records: Vec<ChildRecord> = Vec::new();

    if brief {
        // Brief mode: Plan -> Works directly (skip Spec/Phase levels)
        let works = decompose_into(plan_markdown, &plan_id, DocKind::Work, config, http_client).await?;
        all_records.extend(works);
    } else {
        // Full mode: Plan -> Specs (sequential) -> Phases + Works (parallel per spec)
        let specs = decompose_into(plan_markdown, &plan_id, DocKind::Spec, config, http_client).await?;
        info!("{} specs produced, starting parallel spec branches", specs.len());

        let spec_futures: Vec<_> = specs
            .iter()
            .map(|spec| decompose_spec_branch(&spec.content, &spec.id, config, http_client))
            .collect();
        let branch_results_raw = join_all(spec_futures).await;

        let mut branch_results: Vec<Vec<ChildRecord>> = Vec::new();
        let mut branch_error: Option<String> = None;

        for (spec, result) in specs.iter().zip(branch_results_raw) {
            match result {
                Ok(branch) => branch_results.push(branch),
                Err(e) => {
                    warn!("spec branch {} '{}' failed: {}", spec.id, spec.title, e);
                    branch_error = Some(format!("spec '{}': {}", spec.title, e));
                }
            }
        }

        if branch_error.is_none() {
            info!("all {} spec branches complete", specs.len());
        } else {
            info!(
                "{}/{} spec branches succeeded, {} failed",
                branch_results.len(),
                specs.len(),
                specs.len() - branch_results.len()
            );
        }

        for branch_records in branch_results {
            all_records.extend(branch_records);
        }
        all_records.extend(specs);
        // Cross-branch dep resolution removed: deps are same-level, same-parent only.
        // Spec deps resolve within the Plan->Spec pass. Phase deps resolve within
        // each Spec->Phase branch. Work deps resolve within each Phase->Work branch.

        if let Some(err) = branch_error {
            info!("partial failure plan={} total_records={}", plan_id, all_records.len());
            let mut hierarchy = records_to_hierarchy(&plan_id, &plan_title, plan_markdown, plan_ac, &all_records)?;
            if brief {
                hierarchy.plan.tier = crate::domain::plan::Tier::Brief;
            }
            return Ok((hierarchy, Some(err)));
        }
    }

    info!(
        "hierarchy complete plan={} total_records={}",
        plan_id,
        all_records.len()
    );
    ratify_hierarchy(&plan_id, plan_markdown, &all_records, config, http_client).await?;

    let mut hierarchy = records_to_hierarchy(&plan_id, &plan_title, plan_markdown, plan_ac, &all_records)?;
    if brief {
        hierarchy.plan.tier = crate::domain::plan::Tier::Brief;
    }
    Ok((hierarchy, None))
}

/// Decompose one spec into phases + works concurrently.
#[instrument(skip_all, fields(spec_id = %spec_id))]
async fn decompose_spec_branch<H: HttpClient + Sync>(
    spec_content: &str,
    spec_id: &str,
    config: &DecomposerConfig,
    http_client: &H,
) -> Result<Vec<ChildRecord>> {
    let phases = decompose_into(spec_content, spec_id, DocKind::Phase, config, http_client).await?;
    debug!(
        "spec={} got {} phases, starting parallel phase branches",
        spec_id,
        phases.len()
    );

    let phase_futures: Vec<_> = phases
        .iter()
        .map(|phase| decompose_phase_branch(&phase.content, &phase.id, config, http_client))
        .collect();
    let phase_results = try_join_all(phase_futures).await?;
    debug!("spec={} all phase branches complete", spec_id);

    let mut all_records = phases;
    for works in phase_results {
        all_records.extend(works);
    }

    debug!("spec={} branch total records={}", spec_id, all_records.len());
    Ok(all_records)
}

/// Decompose one phase into works.
#[instrument(skip_all, fields(phase_id = %phase_id))]
async fn decompose_phase_branch<H: HttpClient + Sync>(
    phase_content: &str,
    phase_id: &str,
    config: &DecomposerConfig,
    http_client: &H,
) -> Result<Vec<ChildRecord>> {
    let works = decompose_into(phase_content, phase_id, DocKind::Work, config, http_client).await?;
    debug!("phase={} got {} works", phase_id, works.len());
    Ok(works)
}

/// Bottom-up ratification of the decomposition hierarchy (in-memory).
#[instrument(skip_all, fields(plan_id = %plan_id, record_count = %all_records.len()))]
async fn ratify_hierarchy<H: HttpClient + Sync>(
    plan_id: &str,
    plan_content: &str,
    all_records: &[ChildRecord],
    config: &DecomposerConfig,
    http_client: &H,
) -> Result<()> {
    // Build a content map: id -> content (includes the plan itself)
    let mut content_map: HashMap<&str, &str> = HashMap::new();
    content_map.insert(plan_id, plan_content);
    for r in all_records {
        content_map.insert(r.id.as_str(), r.content.as_str());
    }

    // Group records by parent_id
    let mut children_of: HashMap<&str, Vec<&ChildRecord>> = HashMap::new();
    for record in all_records {
        if let Some(ref pid) = record.parent_id {
            children_of.entry(pid.as_str()).or_default().push(record);
        }
    }

    // Ratify each parent-children group
    for (parent_id, children) in &children_of {
        let Some(parent_content) = content_map.get(parent_id) else {
            continue;
        };

        let child_pairs: Vec<(String, String)> =
            children.iter().map(|c| (c.title.clone(), c.content.clone())).collect();

        if child_pairs.is_empty() {
            continue;
        }

        trace!("ratifying parent={} with {} children", parent_id, child_pairs.len());
        let prompt = build_ratify_prompt(parent_content, &child_pairs);
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
                debug!("parent={} passed", parent_id);
            }
        }
    }

    Ok(())
}

// =====================================================
// DecomposedHierarchy + docs_to_hierarchy
// =====================================================

/// The fully typed output of a decomposition run.
///
/// Returned by `decompose_hierarchy` so callers receive strongly-typed records
/// directly instead of `Vec<Doc>` that must be converted in a separate pass.
///
/// `content` maps domain record ID → LLM markdown body. `persist_hierarchy` uses
/// this to write `docs/loopr/<id>.md` with the full prose content.
pub struct DecomposedHierarchy {
    pub plan: Plan,
    pub specs: Vec<Spec>,
    pub phases: Vec<Phase>,
    pub works: Vec<Work>,
    pub content: std::collections::HashMap<String, String>,
}

/// Returns the expected ID prefix for a dep of the given kind.
/// Spec deps must point to other Specs (sp-), Phase deps to Phases (ph-),
/// Work deps to Works (wk-). Used for defense-in-depth prefix validation.
fn expected_dep_prefix(kind: DocKind) -> &'static str {
    match kind {
        DocKind::Spec => "sp-",
        DocKind::Phase => "ph-",
        DocKind::Work => "wk-",
        DocKind::Plan => "pl-",
    }
}

/// Convert in-memory `Vec<ChildRecord>` into typed domain records.
///
/// Builds Plan/Spec/Phase/Work using pre-assigned IDs from ChildRecord.
/// The Plan is assigned the caller-generated plan_id. All records start Active/Ready
/// immediately (coordinator begins executing after decomposition).
fn records_to_hierarchy(
    plan_id: &str,
    plan_title: &str,
    plan_markdown: &str,
    plan_ac: AcceptanceCriteria,
    all_records: &[ChildRecord],
) -> Result<DecomposedHierarchy> {
    // Map ChildRecord.id -> domain record id (they are the same in the new design).
    // We still need a set of known IDs for Work dependency resolution.
    let known_ids: std::collections::HashSet<&str> = all_records.iter().map(|r| r.id.as_str()).collect();

    let mut plan = Plan::new(plan_title.to_string(), plan_ac);
    plan.id = plan_id.to_string();
    plan.force_status(PlanStatus::Active);

    let mut specs: Vec<Spec> = Vec::new();
    let mut phases: Vec<Phase> = Vec::new();
    let mut works: Vec<Work> = Vec::new();

    let spec_records: Vec<&ChildRecord> = all_records.iter().filter(|r| r.kind == DocKind::Spec).collect();
    let phase_records: Vec<&ChildRecord> = all_records.iter().filter(|r| r.kind == DocKind::Phase).collect();
    let work_records: Vec<&ChildRecord> = all_records.iter().filter(|r| r.kind == DocKind::Work).collect();

    for child in spec_records
        .iter()
        .chain(phase_records.iter())
        .chain(work_records.iter())
    {
        let parent_id = match &child.parent_id {
            Some(pid) => pid.clone(),
            None => continue,
        };

        let prefix = expected_dep_prefix(child.kind);
        match child.kind {
            DocKind::Spec => {
                let mut spec = Spec::new(parent_id, child.title.clone());
                spec.id = child.id.clone();
                spec.force_status(SpecStatus::Pending);
                spec.acceptance_criteria = AcceptanceCriteria(child.acceptance_criteria.clone());
                spec.dependencies = child
                    .dependencies
                    .iter()
                    .filter(|dep_id| {
                        let ok = known_ids.contains(dep_id.as_str()) && dep_id.starts_with(prefix);
                        if !ok {
                            error!(
                                "records_to_hierarchy: rejecting dep '{}' for spec '{}' (expected prefix '{}')",
                                dep_id, child.id, prefix
                            );
                        }
                        ok
                    })
                    .cloned()
                    .collect();
                specs.push(spec);
            }
            DocKind::Phase => {
                let mut phase = Phase::new(parent_id, child.title.clone());
                phase.id = child.id.clone();
                phase.force_status(PhaseStatus::Pending);
                phase.acceptance_criteria = AcceptanceCriteria(child.acceptance_criteria.clone());
                phase.dependencies = child
                    .dependencies
                    .iter()
                    .filter(|dep_id| {
                        let ok = known_ids.contains(dep_id.as_str()) && dep_id.starts_with(prefix);
                        if !ok {
                            error!(
                                "records_to_hierarchy: rejecting dep '{}' for phase '{}' (expected prefix '{}')",
                                dep_id, child.id, prefix
                            );
                        }
                        ok
                    })
                    .cloned()
                    .collect();
                phases.push(phase);
            }
            DocKind::Work => {
                let mut work = Work::new(parent_id, child.title.clone());
                work.id = child.id.clone();
                work.force_status(WorkStatus::Pending);
                work.acceptance_criteria = AcceptanceCriteria(child.acceptance_criteria.clone());
                work.dependencies = child
                    .dependencies
                    .iter()
                    .filter(|dep_id| {
                        let ok = known_ids.contains(dep_id.as_str()) && dep_id.starts_with(prefix);
                        if !ok {
                            error!(
                                "records_to_hierarchy: rejecting dep '{}' for work '{}' (expected prefix '{}')",
                                dep_id, child.id, prefix
                            );
                        }
                        ok
                    })
                    .cloned()
                    .collect();
                works.push(work);
            }
            DocKind::Plan => continue,
        }
    }

    // Build content map: id -> LLM markdown body (for persist_hierarchy to write docs/loopr/)
    let mut content = std::collections::HashMap::new();
    content.insert(plan_id.to_string(), plan_markdown.to_string());
    for record in all_records {
        content.insert(record.id.clone(), record.content.clone());
    }

    Ok(DecomposedHierarchy {
        plan,
        specs,
        phases,
        works,
        content,
    })
}

/// Extract the plan title from the first `# ` heading line in the markdown.
fn extract_title_from_markdown(markdown: &str) -> String {
    for line in markdown.lines() {
        let trimmed = line.trim();
        if let Some(stripped) = trimmed.strip_prefix("# ") {
            let title = stripped.trim();
            if !title.is_empty() {
                return title.to_string();
            }
        }
    }
    "Untitled Plan".to_string()
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn test_child_entry_backward_compat_ignores_files() {
        // Old LLM responses that include "files" must still deserialize (serde ignores unknown fields).
        let json = r#"{"title":"T","content":"C","files":["main.py"]}"#;
        let entry: ChildEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.title, "T");
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
            llm: crate::config::LlmConfig {
                model: "test-model".to_string(),
                api_key_env: "TEST_DECOMPOSER_KEY".to_string(),
                max_tokens: 4096,
                temperature: 0.3,
            },
            provider: "anthropic".to_string(),
            validation_model: "test-haiku".to_string(),
            prompts: crate::config::DecomposerPrompts::default(),
        }
    }

    #[tokio::test]
    async fn test_decompose_into_plan_to_specs() {
        crate::prompts::init_defaults();
        let env_key = format!("TEST_DECOMPOSER_KEY_{}", crate::id::generate_id("xx"));
        unsafe { std::env::set_var(&env_key, "test-key") };

        let parent_content = "# Plan\n\n## Problem Statement\n\nTest problem.\n\n## Goals\n\nTest goal.";
        let parent_id = crate::id::generate_id("pl");

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
        config.llm.api_key_env = env_key.clone();

        let result = decompose_into(parent_content, &parent_id, DocKind::Spec, &config, &mock)
            .await
            .unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].kind, DocKind::Spec);
        assert_eq!(result[1].kind, DocKind::Spec);
        assert!(result[0].id.starts_with("sp-"));
        assert_eq!(result[0].parent_id.as_deref(), Some(parent_id.as_str()));
        // Second spec should depend on first
        assert_eq!(result[1].dependencies, vec![result[0].id.clone()]);
        // No files written
        assert_eq!(result[0].content, "# Spec\n\n## Overview\n\nThe core implementation.");

        unsafe { std::env::remove_var(&env_key) };
    }

    #[tokio::test]
    async fn test_decompose_into_unresolved_dep_title_fails() {
        // A dep title that doesn't match any sibling in the local batch must cause
        // decompose_into to return an error (strict failure, not silent drop).
        crate::prompts::init_defaults();
        let env_key = format!("TEST_DECOMPOSER_KEY_{}", crate::id::generate_id("xx"));
        unsafe { std::env::set_var(&env_key, "test-key") };

        let parent_content = "# Plan\n\nTest";
        let parent_id = crate::id::generate_id("pl");

        let children_json = serde_json::json!([
            {
                "title": "Spec A",
                "content": "# Spec A",
                "dependencies": ["NonExistentSpec"]
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
        config.llm.api_key_env = env_key.clone();

        let result = decompose_into(parent_content, &parent_id, DocKind::Spec, &config, &mock).await;
        assert!(result.is_err(), "expected error for unresolved dep title");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Dependency resolution failed"),
            "error must mention dep resolution: {err_msg}"
        );
        assert!(
            err_msg.contains("NonExistentSpec"),
            "error must name the unresolved dep: {err_msg}"
        );

        unsafe { std::env::remove_var(&env_key) };
    }

    #[tokio::test]
    async fn test_decompose_into_case_insensitive_dep_resolves() {
        // Dep title with different casing from the sibling title should resolve
        // via the case-insensitive fallback map.
        crate::prompts::init_defaults();
        let env_key = format!("TEST_DECOMPOSER_KEY_{}", crate::id::generate_id("xx"));
        unsafe { std::env::set_var(&env_key, "test-key") };

        let parent_content = "# Plan\n\nTest";
        let parent_id = crate::id::generate_id("pl");

        let children_json = serde_json::json!([
            {
                "title": "Core Module",
                "content": "# Core Module",
                "dependencies": []
            },
            {
                "title": "API Layer",
                "content": "# API Layer",
                "dependencies": ["core module"]
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
        config.llm.api_key_env = env_key.clone();

        let result = decompose_into(parent_content, &parent_id, DocKind::Spec, &config, &mock)
            .await
            .unwrap();
        assert_eq!(result.len(), 2);
        // API Layer should have resolved "core module" (lowercase) to Core Module's ID
        assert_eq!(
            result[1].dependencies,
            vec![result[0].id.clone()],
            "case-insensitive dep should resolve to Core Module's ID"
        );

        unsafe { std::env::remove_var(&env_key) };
    }

    #[tokio::test]
    async fn test_decompose_cycle_detection_fails() {
        crate::prompts::init_defaults();
        let env_key = format!("TEST_DECOMPOSER_KEY_{}", crate::id::generate_id("xx"));
        unsafe { std::env::set_var(&env_key, "test-key") };

        let parent_content = "# Plan\n\nTest";
        let parent_id = crate::id::generate_id("pl");

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
        config.llm.api_key_env = env_key.clone();

        let result = decompose_into(parent_content, &parent_id, DocKind::Spec, &config, &mock).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cycle"));

        unsafe { std::env::remove_var(&env_key) };
    }

    #[tokio::test]
    async fn test_decompose_into_work_ac_extraction() {
        crate::prompts::init_defaults();
        let env_key = format!("TEST_DECOMPOSER_KEY_{}", crate::id::generate_id("xx"));
        unsafe { std::env::set_var(&env_key, "test-key") };

        let parent_content = "# Phase\n\nTest phase";
        let parent_id = crate::id::generate_id("ph");

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
        config.llm.api_key_env = env_key.clone();

        let result = decompose_into(parent_content, &parent_id, DocKind::Work, &config, &mock)
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].kind, DocKind::Work);
        assert_eq!(
            result[0].acceptance_criteria,
            vec!["assert schema_exists()", "assert table_count() == 3"]
        );

        unsafe { std::env::remove_var(&env_key) };
    }

    #[tokio::test]
    async fn test_decompose_into_brief_plan_to_works() {
        // Brief mode: decompose_into with DocKind::Work from a Plan parent
        crate::prompts::init_defaults();
        let env_key = format!("TEST_DECOMPOSER_KEY_{}", crate::id::generate_id("xx"));
        unsafe { std::env::set_var(&env_key, "test-key") };

        let parent_content = "# Brief Plan\n\n## Problem Statement\n\nSmall task.\n\n## Goals\n\nDo it quickly.";
        let parent_id = crate::id::generate_id("pl");

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
        config.llm.api_key_env = env_key.clone();

        let result = decompose_into(parent_content, &parent_id, DocKind::Work, &config, &mock)
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].kind, DocKind::Work);
        assert!(result[0].id.starts_with("wk-"));
        assert_eq!(result[0].parent_id.as_deref(), Some(parent_id.as_str()));

        unsafe { std::env::remove_var(&env_key) };
    }

    #[tokio::test]
    async fn test_decompose_hierarchy_cross_scope_dependency_fails_branch() {
        // Cross-spec deps are not supported (reactive execution model, local-only resolution).
        // Work Beta (in Spec B) references "Work Alpha" (in Spec A) by title.
        // Since deps resolve from local sibling batch only, "Work Alpha" is not in Phase B's
        // local map - the dep fails strict validation, causing Spec B's branch to produce
        // a partial_err. Spec A's branch succeeds normally.
        crate::prompts::init_defaults();
        let env_key = format!("TEST_DECOMPOSER_KEY_{}", crate::id::generate_id("xx"));
        unsafe { std::env::set_var(&env_key, "test-key") };

        let plan_markdown = "# Cross-Scope Plan\n\n## Problem Statement\n\nTest cross-spec deps.\n\n## Goals\n\nVerify local-only resolution.";

        let valid_json = r#"{"valid": true, "issues": []}"#;

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
        // Call 10: decompose Phase B -> [Work Beta] (unresolvable dep "Work Alpha")
        // Call 11: validate Work Beta (validation runs before dep resolution)
        // Spec B branch fails with dep resolution error - no ratification calls.
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
                // Call 11: validate Work Beta (consumed before dep resolution fails)
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
        config.llm.api_key_env = env_key.clone();

        let (hierarchy, partial_err) = decompose_hierarchy(plan_markdown, &config, &mock, false).await.unwrap();
        assert!(
            partial_err.is_some(),
            "expected partial failure for unresolvable cross-spec dep, got None"
        );
        let err_msg = partial_err.unwrap();
        assert!(
            err_msg.contains("Dependency resolution failed") || err_msg.contains("Work Beta"),
            "partial_err should mention dep resolution: {err_msg}"
        );

        // Spec A's branch succeeded - Work Alpha must exist
        hierarchy
            .works
            .iter()
            .find(|w| w.title == "Work Alpha")
            .expect("Work Alpha not found in partial hierarchy");

        // Spec B's branch failed - Work Beta must NOT exist
        assert!(
            hierarchy.works.iter().all(|w| w.title != "Work Beta"),
            "Work Beta should not appear - its branch failed"
        );

        unsafe { std::env::remove_var(&env_key) };
    }

    #[tokio::test]
    async fn test_decompose_hierarchy_parallel_specs_complete() {
        // Verify that decompose_hierarchy produces docs for ALL specs when specs are
        // decomposed in parallel. Two specs, each with one phase and one work.
        crate::prompts::init_defaults();
        let env_key = format!("TEST_DECOMPOSER_KEY_{}", crate::id::generate_id("xx"));
        unsafe { std::env::set_var(&env_key, "test-key") };

        let plan_markdown =
            "# Parallel Plan\n\n## Problem Statement\n\nTwo independent tracks.\n\n## Goals\n\nBoth must complete.";

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
        config.llm.api_key_env = env_key.clone();

        let (hierarchy, partial_err) = decompose_hierarchy(plan_markdown, &config, &mock, false).await.unwrap();
        assert!(
            partial_err.is_none(),
            "expected no partial failure, got: {:?}",
            partial_err
        );

        assert_eq!(
            hierarchy.specs.len(),
            2,
            "expected 2 specs, got {}",
            hierarchy.specs.len()
        );
        assert_eq!(
            hierarchy.phases.len(),
            2,
            "expected 2 phases, got {}",
            hierarchy.phases.len()
        );
        assert_eq!(
            hierarchy.works.len(),
            2,
            "expected 2 works, got {}",
            hierarchy.works.len()
        );

        unsafe { std::env::remove_var(&env_key) };
    }

    #[tokio::test]
    async fn test_decompose_hierarchy_cross_spec_deps_not_resolved() {
        // Cross-spec deps cause a branch failure under local-only resolution.
        // Work Beta (in Spec Beta) references "Work Alpha" (in Spec Alpha) by title.
        // "Work Alpha" is not in Phase Beta's local sibling batch, so dep resolution
        // fails, producing a partial_err. Spec Alpha's branch succeeds normally.
        crate::prompts::init_defaults();
        let env_key = format!("TEST_DECOMPOSER_KEY_{}", crate::id::generate_id("xx"));
        unsafe { std::env::set_var(&env_key, "test-key") };

        let plan_markdown =
            "# Cross-Spec Plan\n\n## Problem Statement\n\nCross-spec dep test.\n\n## Goals\n\nBeta depends on Alpha.";

        let valid_json = r#"{"valid": true, "issues": []}"#;

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

        // Responses consumed LIFO (last element first). Spec Beta's branch fails after
        // dep resolution; no ratification calls are made.
        let mock = SequenceMockHttp {
            responses: std::sync::Mutex::new(vec![
                mk(valid_json),    // validate Work Beta (consumed before dep resolution fails)
                mk(&works_beta),   // decompose Phase Beta -> works
                mk(valid_json),    // validate Phase Beta
                mk(&phases_beta),  // decompose Spec Beta -> phases
                mk(valid_json),    // validate Work Alpha
                mk(&works_alpha),  // decompose Phase Alpha -> works
                mk(valid_json),    // validate Phase Alpha
                mk(&phases_alpha), // decompose Spec Alpha -> phases
                mk(valid_json),    // validate Spec Beta
                mk(valid_json),    // validate Spec Alpha
                mk(&specs_json),   // decompose plan -> specs
            ]),
        };

        let mut config = test_config();
        config.llm.api_key_env = env_key.clone();

        let (hierarchy, partial_err) = decompose_hierarchy(plan_markdown, &config, &mock, false).await.unwrap();
        assert!(
            partial_err.is_some(),
            "expected partial failure for unresolvable cross-spec dep"
        );

        // Spec Alpha's branch succeeded - Work Alpha must exist
        hierarchy
            .works
            .iter()
            .find(|w| w.title == "Work Alpha")
            .expect("Work Alpha not found in partial hierarchy");

        // Spec Beta's branch failed - Work Beta must NOT exist
        assert!(
            hierarchy.works.iter().all(|w| w.title != "Work Beta"),
            "Work Beta should not appear - its branch failed"
        );

        unsafe { std::env::remove_var(&env_key) };
    }

    #[tokio::test(start_paused = true)]
    async fn test_decompose_into_times_out_slow_llm() {
        // With start_paused=true, tokio's clock is virtual. The mock sleeps for 200s of
        // virtual time; the 60s timeout fires first. The test runs in real milliseconds.
        crate::prompts::init_defaults();
        let env_key = format!("TEST_DECOMPOSER_KEY_{}", crate::id::generate_id("xx"));
        unsafe { std::env::set_var(&env_key, "test-key") };

        let parent_content = "# Timeout Plan\n\n## Problem Statement\n\nTest.\n\n## Goals\n\nHang.";
        let parent_id = crate::id::generate_id("pl");

        struct SlowMockHttp;
        impl HttpClient for SlowMockHttp {
            async fn post(&self, _url: &str, _headers: &[(&str, &str)], _body: &str) -> Result<String> {
                // Virtual sleep; tokio advances time, so the 60s timeout fires first
                tokio::time::sleep(Duration::from_secs(200)).await;
                Ok("never".to_string())
            }
        }

        let mut config = test_config();
        config.llm.api_key_env = env_key.clone();

        let result = decompose_into(parent_content, &parent_id, DocKind::Spec, &config, &SlowMockHttp).await;

        assert!(result.is_err(), "expected timeout error, got: {:?}", result);
        let msg = format!("{:?}", result.unwrap_err());
        assert!(
            msg.contains("timed out") || msg.contains("Decomposition failed"),
            "error should mention timeout: {msg}"
        );

        unsafe { std::env::remove_var(&env_key) };
    }

    #[tokio::test]
    async fn test_decompose_hierarchy_partial_failure_persists_successful_branches() {
        // When one spec branch fails and another succeeds, decompose_hierarchy must return
        // the successful branch's docs and a Some(error) partial failure message.
        // The caller is then responsible for persisting the partial error.
        crate::prompts::init_defaults();
        let env_key = format!("TEST_DECOMPOSER_KEY_{}", crate::id::generate_id("xx"));
        unsafe { std::env::set_var(&env_key, "test-key") };

        let plan_markdown = "# Partial Plan\n\n## Problem Statement\n\nTwo tracks, one fails.\n\n## Goals\n\nSurvive.";

        let valid_json = r#"{"valid": true, "issues": []}"#;

        let specs_json = serde_json::json!([
            {"title": "Good Track", "content": "# Good Track\n\n## Overview\n\nWill succeed.", "dependencies": []},
            {"title": "Bad Track", "content": "# Bad Track\n\n## Overview\n\nWill fail.", "dependencies": []}
        ])
        .to_string();
        let phases_one = serde_json::json!([
            {"title": "Good Phase", "content": "# Good Phase\n\n## Overview\n\nOne phase.", "dependencies": []}
        ])
        .to_string();
        let works_one = serde_json::json!([
            {"title": "Good Work", "content": "# Work\n\n## Description\n\nDo good.\n\n## Acceptance Criteria\n\nassert good()\n\n## Dependencies\n\nNone", "dependencies": []}
        ])
        .to_string();

        let mk = |text: &str| serde_json::json!({"content": [{"type": "text", "text": text}]}).to_string();

        // Responses are popped LIFO (last = first consumed). Execution order:
        // 1. decompose plan -> specs
        // 2. validate Good Track (from plan decompose)
        // 3. validate Bad Track (from plan decompose)
        // 4. decompose Good Track -> phases (spec 1 branch runs first)
        // 5. validate Good Phase
        // 6. decompose Good Phase -> works
        // 7. validate Good Work
        // 8. Bad Track branch calls decompose -> mock empty -> error
        let mock = SequenceMockHttp {
            responses: std::sync::Mutex::new(vec![
                mk(valid_json),  // #7 validate Good Work (consumed last)
                mk(&works_one),  // #6 decompose Good Phase -> works
                mk(valid_json),  // #5 validate Good Phase
                mk(&phases_one), // #4 decompose Good Track -> phases
                mk(valid_json),  // #3 validate Bad Track (from plan decompose)
                mk(valid_json),  // #2 validate Good Track (from plan decompose)
                mk(&specs_json), // #1 decompose plan -> specs (consumed first)
            ]),
        };

        let mut config = test_config();
        config.llm.api_key_env = env_key.clone();

        let result = decompose_hierarchy(plan_markdown, &config, &mock, false).await;
        assert!(result.is_ok(), "decompose_hierarchy should succeed with partial docs");

        let (hierarchy, partial_err) = result.unwrap();

        // partial_err must be set - Bad Track branch failed
        assert!(partial_err.is_some(), "expected partial_err to be set, got None");
        let err_msg = partial_err.unwrap();
        assert!(
            err_msg.contains("Bad Track"),
            "partial_err should name the failed spec, got: {err_msg}"
        );

        // Good Track's domain records must be present
        assert_eq!(
            hierarchy.phases.len(),
            1,
            "expected 1 phase from Good Track, got {}",
            hierarchy.phases.len()
        );
        assert_eq!(
            hierarchy.works.len(),
            1,
            "expected 1 work from Good Track, got {}",
            hierarchy.works.len()
        );

        // Both specs are present (Good Track + failed Bad Track which was produced by plan decompose)
        assert_eq!(
            hierarchy.specs.len(),
            2,
            "both specs should be present (even failed one from plan decompose), got {}",
            hierarchy.specs.len()
        );

        unsafe { std::env::remove_var(&env_key) };
    }

    // --- records_to_hierarchy ---

    fn build_test_records() -> (String, String, String, AcceptanceCriteria, Vec<ChildRecord>) {
        let plan_id = crate::id::generate_id("pl");
        let plan_title = "Test Plan".to_string();
        let plan_markdown = "# Test Plan\n\nA test plan.\n\n## Acceptance Criteria\n\n- plan passes";

        let spec_id = crate::id::generate_id("sp");
        let phase_id = crate::id::generate_id("ph");
        let work_id = crate::id::generate_id("wk");

        let spec = ChildRecord {
            id: spec_id.clone(),
            kind: DocKind::Spec,
            parent_id: Some(plan_id.clone()),
            title: "Core Spec".to_string(),
            content: "Spec content".to_string(),
            dependencies: vec![],
            unresolved_dep_titles: vec![],
            acceptance_criteria: vec!["spec passes".to_string()],
        };
        let phase = ChildRecord {
            id: phase_id.clone(),
            kind: DocKind::Phase,
            parent_id: Some(spec_id),
            title: "Phase One".to_string(),
            content: "Phase content".to_string(),
            dependencies: vec![],
            unresolved_dep_titles: vec![],
            acceptance_criteria: vec!["phase passes".to_string()],
        };
        let work = ChildRecord {
            id: work_id,
            kind: DocKind::Work,
            parent_id: Some(phase_id),
            title: "Write tests".to_string(),
            content: "Work content".to_string(),
            dependencies: vec![],
            unresolved_dep_titles: vec![],
            acceptance_criteria: vec!["assert tests pass".to_string()],
        };

        let ac = AcceptanceCriteria(vec!["plan passes".to_string()]);
        (
            plan_id,
            plan_title,
            plan_markdown.to_string(),
            ac,
            vec![spec, phase, work],
        )
    }

    #[test]
    fn test_records_to_hierarchy_basic() {
        let (plan_id, plan_title, plan_markdown, plan_ac, records) = build_test_records();
        let h = records_to_hierarchy(&plan_id, &plan_title, &plan_markdown, plan_ac, &records).unwrap();

        assert_eq!(h.plan.title, "Test Plan");
        // description removed from struct; content lives in hierarchy.content map
        assert_eq!(
            h.content.get(&plan_id).map(String::as_str),
            Some(plan_markdown.as_str())
        );
        assert_eq!(h.specs.len(), 1);
        assert_eq!(h.phases.len(), 1);
        assert_eq!(h.works.len(), 1);
    }

    #[test]
    fn test_records_to_hierarchy_plan_status_active() {
        let (plan_id, plan_title, plan_markdown, plan_ac, records) = build_test_records();
        let h = records_to_hierarchy(&plan_id, &plan_title, &plan_markdown, plan_ac, &records).unwrap();

        use crate::domain::plan::HierarchyStatus;
        assert_eq!(h.plan.status(), HierarchyStatus::Active);
        assert_eq!(h.specs[0].status(), HierarchyStatus::Pending);
        assert_eq!(h.phases[0].status(), HierarchyStatus::Pending);
        use crate::domain::work::WorkStatus;
        assert_eq!(h.works[0].status(), WorkStatus::Pending);
    }

    #[test]
    fn test_records_to_hierarchy_ac_propagated() {
        let (plan_id, plan_title, plan_markdown, plan_ac, records) = build_test_records();
        let h = records_to_hierarchy(&plan_id, &plan_title, &plan_markdown, plan_ac, &records).unwrap();

        assert_eq!(h.plan.acceptance_criteria.0, vec!["plan passes"]);
        assert_eq!(h.specs[0].acceptance_criteria.0, vec!["spec passes"]);
        assert_eq!(h.phases[0].acceptance_criteria.0, vec!["phase passes"]);
        assert_eq!(h.works[0].acceptance_criteria.0, vec!["assert tests pass"]);
    }

    #[test]
    fn test_records_to_hierarchy_parent_ids_resolved() {
        let (plan_id, plan_title, plan_markdown, plan_ac, records) = build_test_records();
        let h = records_to_hierarchy(&plan_id, &plan_title, &plan_markdown, plan_ac, &records).unwrap();

        assert_eq!(h.specs[0].parent_id, h.plan.id);
        assert_eq!(h.phases[0].parent_id, h.specs[0].id);
        assert_eq!(h.works[0].parent_id, h.phases[0].id);
    }

    #[test]
    fn test_records_to_hierarchy_work_dependency_resolved() {
        let plan_id = crate::id::generate_id("pl");
        let spec_id = crate::id::generate_id("sp");
        let phase_id = crate::id::generate_id("ph");
        let wa_id = crate::id::generate_id("wk");
        let wb_id = crate::id::generate_id("wk");

        let records = vec![
            ChildRecord {
                id: spec_id.clone(),
                kind: DocKind::Spec,
                parent_id: Some(plan_id.clone()),
                title: "Spec".to_string(),
                content: "spec".to_string(),
                dependencies: vec![],
                unresolved_dep_titles: vec![],
                acceptance_criteria: vec![],
            },
            ChildRecord {
                id: phase_id.clone(),
                kind: DocKind::Phase,
                parent_id: Some(spec_id),
                title: "Phase".to_string(),
                content: "phase".to_string(),
                dependencies: vec![],
                unresolved_dep_titles: vec![],
                acceptance_criteria: vec![],
            },
            ChildRecord {
                id: wa_id.clone(),
                kind: DocKind::Work,
                parent_id: Some(phase_id.clone()),
                title: "Work A".to_string(),
                content: "wa content".to_string(),
                dependencies: vec![],
                unresolved_dep_titles: vec![],
                acceptance_criteria: vec!["assert a()".to_string()],
            },
            ChildRecord {
                id: wb_id,
                kind: DocKind::Work,
                parent_id: Some(phase_id),
                title: "Work B".to_string(),
                content: "wb content".to_string(),
                dependencies: vec![wa_id.clone()],
                unresolved_dep_titles: vec![],
                acceptance_criteria: vec!["assert b()".to_string()],
            },
        ];

        let ac = AcceptanceCriteria(vec![]);
        let h = records_to_hierarchy(&plan_id, "Plan", "# Plan\n\nA plan.", ac, &records).unwrap();

        assert_eq!(h.works.len(), 2);
        let wa = h.works.iter().find(|w| w.title == "Work A").unwrap();
        let wb = h.works.iter().find(|w| w.title == "Work B").unwrap();
        assert_eq!(wb.dependencies, vec![wa.id.clone()]);
    }

    #[test]
    fn test_records_to_hierarchy_work_produced() {
        let (plan_id, plan_title, plan_markdown, plan_ac, records) = build_test_records();
        let h = records_to_hierarchy(&plan_id, &plan_title, &plan_markdown, plan_ac, &records).unwrap();
        assert!(!h.works.is_empty());
    }

    #[test]
    fn test_records_to_hierarchy_files_propagated() {
        let plan_id = crate::id::generate_id("pl");
        let spec_id = crate::id::generate_id("sp");
        let phase_id = crate::id::generate_id("ph");
        let work_id = crate::id::generate_id("wk");

        let records = vec![
            ChildRecord {
                id: spec_id.clone(),
                kind: DocKind::Spec,
                parent_id: Some(plan_id.clone()),
                title: "Spec".to_string(),
                content: "spec".to_string(),
                dependencies: vec![],
                unresolved_dep_titles: vec![],
                acceptance_criteria: vec![],
            },
            ChildRecord {
                id: phase_id.clone(),
                kind: DocKind::Phase,
                parent_id: Some(spec_id),
                title: "Phase".to_string(),
                content: "phase".to_string(),
                dependencies: vec![],
                unresolved_dep_titles: vec![],
                acceptance_criteria: vec![],
            },
            ChildRecord {
                id: work_id,
                kind: DocKind::Work,
                parent_id: Some(phase_id),
                title: "Implement module".to_string(),
                content: "work content".to_string(),
                dependencies: vec![],
                unresolved_dep_titles: vec![],
                acceptance_criteria: vec!["assert module imports".to_string()],
            },
        ];

        let ac = AcceptanceCriteria(vec![]);
        let h = records_to_hierarchy(&plan_id, "Plan", "# Plan\n\nA plan.", ac, &records).unwrap();

        assert!(!h.works.is_empty());
    }

    #[test]
    fn test_records_to_hierarchy_cross_type_dep_rejected() {
        // A Spec with a dep ID starting with "ph-" (a Phase ID) must be filtered out.
        // After Fix 2, this is structurally unreachable via decompose_into, but
        // records_to_hierarchy is defense-in-depth.
        let plan_id = crate::id::generate_id("pl");
        let spec_a_id = crate::id::generate_id("sp");
        let phase_id = crate::id::generate_id("ph"); // cross-type dep

        let records = vec![ChildRecord {
            id: spec_a_id.clone(),
            kind: DocKind::Spec,
            parent_id: Some(plan_id.clone()),
            title: "Spec A".to_string(),
            content: "# Spec A".to_string(),
            // dep is a Phase ID - wrong prefix for a Spec dep
            dependencies: vec![phase_id.clone()],
            unresolved_dep_titles: vec![],
            acceptance_criteria: vec![],
        }];

        let ac = AcceptanceCriteria(vec![]);
        let h = records_to_hierarchy(&plan_id, "Plan", "# Plan\n\nA plan.", ac, &records).unwrap();

        assert!(
            h.specs[0].dependencies.is_empty(),
            "cross-type dep should be filtered out, got: {:?}",
            h.specs[0].dependencies
        );
    }

    #[test]
    fn test_records_to_hierarchy_same_type_dep_kept() {
        // A Spec dep pointing to another Spec ID (sp- prefix) must be kept.
        let plan_id = crate::id::generate_id("pl");
        let spec_a_id = crate::id::generate_id("sp");
        let spec_b_id = crate::id::generate_id("sp");

        let records = vec![
            ChildRecord {
                id: spec_a_id.clone(),
                kind: DocKind::Spec,
                parent_id: Some(plan_id.clone()),
                title: "Spec A".to_string(),
                content: "# Spec A".to_string(),
                dependencies: vec![],
                unresolved_dep_titles: vec![],
                acceptance_criteria: vec![],
            },
            ChildRecord {
                id: spec_b_id.clone(),
                kind: DocKind::Spec,
                parent_id: Some(plan_id.clone()),
                title: "Spec B".to_string(),
                content: "# Spec B".to_string(),
                dependencies: vec![spec_a_id.clone()],
                unresolved_dep_titles: vec![],
                acceptance_criteria: vec![],
            },
        ];

        let ac = AcceptanceCriteria(vec![]);
        let h = records_to_hierarchy(&plan_id, "Plan", "# Plan\n\nA plan.", ac, &records).unwrap();

        assert_eq!(
            h.specs[1].dependencies,
            vec![spec_a_id.clone()],
            "same-type dep should be preserved"
        );
    }

    // --- build_decompose_prompt ---

    #[test]
    fn test_decompose_prompt_contains_template_section() {
        crate::prompts::init_defaults();
        let result = build_decompose_prompt(DocKind::Spec, "parent content").unwrap();
        assert!(
            result.contains("## Template"),
            "decompose prompt missing ## Template section"
        );
    }

    #[test]
    fn test_decompose_prompt_spec_contains_template_heading() {
        crate::prompts::init_defaults();
        let result = build_decompose_prompt(DocKind::Spec, "parent content").unwrap();
        assert!(
            result.contains("## Overview"),
            "decompose prompt missing spec template ## Overview heading"
        );
    }

    #[test]
    fn test_decompose_prompt_contains_parent_document_section() {
        crate::prompts::init_defaults();
        let sentinel = "SENTINEL_PARENT_CONTENT_ae92f1";
        let result = build_decompose_prompt(DocKind::Spec, sentinel).unwrap();
        assert!(
            result.contains("## Parent Document"),
            "decompose prompt missing ## Parent Document section"
        );
        assert!(
            result.contains(sentinel),
            "parent content not present in decompose prompt"
        );
    }

    #[test]
    fn test_decompose_and_validate_prompts_share_spec_template() {
        crate::prompts::init_defaults();
        let decompose = build_decompose_prompt(DocKind::Spec, "parent").unwrap();
        let validate = build_validate_prompt(DocKind::Spec, "child");
        assert!(
            decompose.contains("# Spec Template"),
            "decompose prompt missing spec template content"
        );
        assert!(
            validate.contains("# Spec Template"),
            "validate prompt missing spec template content"
        );
    }

    #[test]
    fn test_decompose_prompt_plan_returns_err() {
        crate::prompts::init_defaults();
        let result = build_decompose_prompt(DocKind::Plan, "parent");
        assert!(result.is_err(), "expected error for DocKind::Plan");
    }
}
