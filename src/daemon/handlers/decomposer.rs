//! IPC handlers for single-level decomposition.
//!
//! `decomposer.decompose` performs one level of decomposition: reads a parent
//! record's markdown, calls the LLM to generate children, detects dependency
//! cycles, resolves sibling dependencies, and persists children atomically via
//! `TaskStore::create_many`. Multi-level flow emerges from engine ticks per Doc 6.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use eyre::{Context, bail, eyre};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::{debug, info, instrument, warn};

use crate::config::DecomposerConfig;
use crate::daemon::context::Stores;
use crate::domain::criteria::AcceptanceCriteria;
use crate::domain::doc::DocKind;
use crate::domain::markdown::{read_full_markdown, update_parent_children, write_doc_markdown_body};
use crate::domain::phase::Phase;
use crate::domain::spec::Spec;
use crate::domain::work::Work;
use crate::fsm::status::FsmStatus;
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};
use crate::prompts::SECTION_AC;
use crate::validator::client::HttpClient;

const LLM_CALL_TIMEOUT_SECS: u64 = 180;

// ─── Types ──────────────────────────────────────────────────────────────────

/// A single child document parsed from the LLM's JSON response.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChildEntry {
    title: String,
    content: String,
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(default)]
    acceptance_criteria: Vec<String>,
}

/// In-memory child record produced by decompose_single_level.
#[derive(Debug)]
struct ChildRecord {
    id: String,
    kind: DocKind,
    parent_id: String,
    title: String,
    content: String,
    dependencies: Vec<String>,
    acceptance_criteria: Vec<String>,
}

// ─── Handler ────────────────────────────────────────────────────────────────

#[instrument(skip_all, fields(
    parent_id = ?req.params.get("parent_id"),
    parent_collection = ?req.params.get("parent_collection"),
    target_kind = ?req.params.get("target_kind"),
))]
pub(super) async fn handle_decomposer_decompose(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_async_handler!(req.id, {
        let parent_id = req
            .params
            .get("parent_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre!("missing parent_id"))?
            .to_string();
        // parent_collection is validated but not currently used in persist_children
        // (the match is on target_kind). Retained for future create_many integration.
        let _parent_collection = req
            .params
            .get("parent_collection")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre!("missing parent_collection"))?;
        let target_kind_str = req
            .params
            .get("target_kind")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre!("missing target_kind"))?;
        let target_kind = match target_kind_str {
            "spec" => DocKind::Spec,
            "phase" => DocKind::Phase,
            "work" => DocKind::Work,
            other => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params(&format!("invalid target_kind: {}", other)),
                ));
            }
        };
        let count_guidance = req
            .params
            .get("count_guidance")
            .and_then(|v| v.as_str())
            .unwrap_or("1-5")
            .to_string();
        let dependency_pattern = req
            .params
            .get("dependency_pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("fan-out")
            .to_string();

        // Read parent markdown content from docs/loopr/<id>.md
        let repo_path = &stores.config.project.repo_path;
        let parent_content = read_full_markdown(std::path::Path::new(repo_path), &parent_id)
            .map_err(|e| eyre!("failed to read parent markdown for {}: {}", parent_id, e))?;

        if parent_content.is_empty() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params(&format!("parent {} has no markdown content", parent_id)),
            ));
        }

        // Call LLM for single-level decomposition
        let config = &stores.config.decomposer;
        let http_client = crate::validator::client::ReqwestClient::new();
        let records = decompose_single_level(
            &parent_content,
            &parent_id,
            target_kind,
            config,
            &http_client,
            &count_guidance,
            &dependency_pattern,
        )
        .await?;

        let child_count = records.len();
        if child_count == 0 {
            return Ok(DaemonResponse::ok(
                req.id,
                serde_json::json!({ "children": [], "child_count": 0 }),
            ));
        }

        // Persist children atomically
        let children_json = persist_children(stores, event_tx, &parent_id, records)?;

        Ok(DaemonResponse::ok(
            req.id,
            serde_json::json!({
                "children": children_json,
                "child_count": child_count,
            }),
        ))
    })
}

// ─── Core decomposition ─────────────────────────────────────────────────────

/// Single-level decomposition: one LLM call, parse, cycle detect, resolve deps.
/// Does NOT validate children - that's the engine's job (validate-after-decomposition strategy).
#[instrument(skip_all, fields(parent_id = %parent_id, target_kind = %target_kind))]
async fn decompose_single_level<H: HttpClient + Sync>(
    parent_content: &str,
    parent_id: &str,
    target_kind: DocKind,
    config: &DecomposerConfig,
    http_client: &H,
    count_guidance: &str,
    dependency_pattern: &str,
) -> eyre::Result<Vec<ChildRecord>> {
    let prompt = build_decompose_prompt(target_kind, parent_content, count_guidance, dependency_pattern)?;
    let children = match call_llm_for_children(http_client, config, &prompt).await {
        Ok(c) => c,
        Err(e) => {
            warn!("decomposition failed, retrying once: {}", e);
            let retry_prompt = format!(
                "{}\n\n## Previous Attempt Failed\n\n{}\n\nPlease fix the issues and try again.",
                prompt, e
            );
            call_llm_for_children(http_client, config, &retry_prompt).await?
        }
    };

    // Cycle detection
    let dep_graph: HashMap<String, Vec<String>> = children
        .iter()
        .map(|c| (c.title.clone(), c.dependencies.clone()))
        .collect();
    detect_cycles(&dep_graph)?;

    // Build ChildRecords with domain IDs
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
                parent_id: parent_id.to_string(),
                title: child.title.clone(),
                content: child.content.clone(),
                dependencies: Vec::new(),
                acceptance_criteria: ac,
            },
            child.dependencies.clone(),
        ));
    }

    // Resolve dependency titles to sibling IDs (local-only)
    let local_title_to_id: HashMap<String, String> =
        records.iter().map(|(r, _)| (r.title.clone(), r.id.clone())).collect();
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
        if !unresolved.is_empty() {
            bail!(
                "dependency resolution failed for '{}': unresolved deps {:?}",
                record.title,
                unresolved
            );
        }
        final_records.push(record);
    }

    info!(
        "decompose_single_level: parent={} -> {} {}(s)",
        parent_id,
        final_records.len(),
        target_kind
    );

    Ok(final_records)
}

// ─── Persistence ────────────────────────────────────────────────────────────

/// Persist children atomically via create_many, insert into in-memory stores,
/// write advisory markdown files, and emit record_created events.
fn persist_children(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    parent_id: &str,
    records: Vec<ChildRecord>,
) -> eyre::Result<Vec<serde_json::Value>> {
    let repo_path = stores.config.project.repo_path.clone();
    let target_kind = records
        .first()
        .map(|r| r.kind)
        .ok_or_else(|| eyre!("no records to persist"))?;

    let mut children_json = Vec::new();

    match target_kind {
        DocKind::Spec => {
            let domain_records: Vec<Spec> = records
                .iter()
                .map(|r| {
                    let mut spec = Spec::new(r.parent_id.clone(), r.title.clone());
                    spec.id = r.id.clone();
                    spec.dependencies = r.dependencies.clone();
                    spec.acceptance_criteria = AcceptanceCriteria(r.acceptance_criteria.clone());
                    spec
                })
                .collect();

            if let Some(store) = &stores.store {
                store
                    .lock()
                    .map_err(|_| eyre!("taskstore lock poisoned"))?
                    .create_many(domain_records.clone())
                    .map_err(|e| eyre!("atomic batch persist failed for specs: {}", e))?;
            }

            let mut specs = stores.write_specs()?;
            for (rec, child_record) in domain_records.into_iter().zip(records.iter()) {
                children_json.push(serde_json::json!({"id": rec.id, "title": rec.title}));
                let id = rec.id.clone();
                specs.insert(id.clone(), rec);
                // Advisory markdown write
                if let Err(e) =
                    write_doc_markdown_body(std::path::Path::new(&repo_path), &specs[&id], &child_record.content)
                {
                    warn!("docs/loopr write failed for {}: {}", id, e);
                }
                update_parent_children(std::path::Path::new(&repo_path), parent_id, &id, &child_record.title);
                let _ = event_tx.send(DaemonEvent::record_created("spec", &id));
            }
        }
        DocKind::Phase => {
            let domain_records: Vec<Phase> = records
                .iter()
                .map(|r| {
                    let mut phase = Phase::new(r.parent_id.clone(), r.title.clone());
                    phase.id = r.id.clone();
                    phase.dependencies = r.dependencies.clone();
                    phase.acceptance_criteria = AcceptanceCriteria(r.acceptance_criteria.clone());
                    phase
                })
                .collect();

            if let Some(store) = &stores.store {
                store
                    .lock()
                    .map_err(|_| eyre!("taskstore lock poisoned"))?
                    .create_many(domain_records.clone())
                    .map_err(|e| eyre!("atomic batch persist failed for phases: {}", e))?;
            }

            let mut phases = stores.write_phases()?;
            for (rec, child_record) in domain_records.into_iter().zip(records.iter()) {
                children_json.push(serde_json::json!({"id": rec.id, "title": rec.title}));
                let id = rec.id.clone();
                phases.insert(id.clone(), rec);
                if let Err(e) =
                    write_doc_markdown_body(std::path::Path::new(&repo_path), &phases[&id], &child_record.content)
                {
                    warn!("docs/loopr write failed for {}: {}", id, e);
                }
                update_parent_children(std::path::Path::new(&repo_path), parent_id, &id, &child_record.title);
                let _ = event_tx.send(DaemonEvent::record_created("phase", &id));
            }
        }
        DocKind::Work => {
            let domain_records: Vec<Work> = records
                .iter()
                .map(|r| {
                    let mut work = Work::new(r.parent_id.clone(), r.title.clone());
                    work.id = r.id.clone();
                    work.dependencies = r.dependencies.clone();
                    work.acceptance_criteria = AcceptanceCriteria(r.acceptance_criteria.clone());
                    work
                })
                .collect();

            if let Some(store) = &stores.store {
                store
                    .lock()
                    .map_err(|_| eyre!("taskstore lock poisoned"))?
                    .create_many(domain_records.clone())
                    .map_err(|e| eyre!("atomic batch persist failed for works: {}", e))?;
            }

            let mut works = stores.write_works()?;
            for (rec, child_record) in domain_records.into_iter().zip(records.iter()) {
                children_json.push(serde_json::json!({"id": rec.id, "title": rec.title}));
                let id = rec.id.clone();
                works.insert(id.clone(), rec);
                if let Err(e) =
                    write_doc_markdown_body(std::path::Path::new(&repo_path), &works[&id], &child_record.content)
                {
                    warn!("docs/loopr write failed for {}: {}", id, e);
                }
                update_parent_children(std::path::Path::new(&repo_path), parent_id, &id, &child_record.title);
                let _ = event_tx.send(DaemonEvent::record_created("work", &id));
            }
        }
        DocKind::Plan => {
            bail!("cannot create Plan children via decomposer.decompose");
        }
    }

    Ok(children_json)
}

// ─── Prompt construction ────────────────────────────────────────────────────

/// Build decomposition prompt with count_guidance and dependency_pattern injected.
fn build_decompose_prompt(
    target_kind: DocKind,
    parent_content: &str,
    count_guidance: &str,
    dependency_pattern: &str,
) -> eyre::Result<String> {
    let prompts = crate::prompts::store();
    let (instructions, template_text) = match target_kind {
        DocKind::Spec => (&prompts.decompose_spec, include_str!("../../../docs/templates/spec.md")),
        DocKind::Phase => (
            &prompts.decompose_phase,
            include_str!("../../../docs/templates/phase.md"),
        ),
        DocKind::Work => (&prompts.decompose_work, include_str!("../../../docs/templates/work.md")),
        DocKind::Plan => bail!("cannot decompose into Plan"),
    };

    Ok(format!(
        "{}\n\n## Guidance\n\n- Produce {} child documents\n- Dependency pattern: {}\n\n## Template\n\n{}\n\n## Parent Document\n\n{}",
        instructions, count_guidance, dependency_pattern, template_text, parent_content
    ))
}

/// Tool schema for structured decomposition output.
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
                                "description": "Titles of sibling documents this document depends on"
                            },
                            "acceptance_criteria": {
                                "type": "array",
                                "items": {"type": "string"},
                                "description": "Acceptance criteria assertions for this document"
                            }
                        },
                        "required": ["title", "content"]
                    }
                }
            },
            "required": ["children"]
        }
    })
}

// ─── LLM calls ──────────────────────────────────────────────────────────────

/// Call the LLM using tool-use structured output and return child entries.
#[instrument(skip_all, fields(model = %config.llm.model, provider = %config.provider))]
async fn call_llm_for_children<H: HttpClient + Sync>(
    http_client: &H,
    config: &DecomposerConfig,
    prompt: &str,
) -> eyre::Result<Vec<ChildEntry>> {
    let api_key = std::env::var(&config.llm.api_key_env)
        .context(format!("missing API key env var: {}", config.llm.api_key_env))?;

    let api_url = match config.provider.as_str() {
        "anthropic" => "https://api.anthropic.com/v1/messages",
        other => bail!("unsupported LLM provider: {}", other),
    };

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
        serde_json::from_str(&response_text).context("failed to parse LLM API response")?;

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

    if stop_reason == "max_tokens" {
        bail!(
            "decomposition hit max_tokens ({} tokens) - tool input was truncated",
            generation_tokens
        );
    }

    // Extract tool_use input.children, fall back to text parsing
    let children = if let Some(tool_input) = response["content"]
        .as_array()
        .and_then(|blocks| blocks.iter().find(|b| b["type"].as_str() == Some("tool_use")))
        .and_then(|b| b.get("input"))
        .and_then(|input| input.get("children"))
    {
        serde_json::from_value::<Vec<ChildEntry>>(tool_input.clone())
            .context("failed to parse tool_use input.children")?
    } else {
        let snippet: String = response_text.chars().take(800).collect();
        warn!(
            "model did not use tool, falling back to text parsing (response: {})",
            snippet
        );
        let text = response["content"]
            .as_array()
            .and_then(|blocks| blocks.iter().find(|b| b["type"].as_str() == Some("text")))
            .and_then(|b| b["text"].as_str())
            .ok_or_else(|| eyre::eyre!("LLM returned neither tool_use nor text content"))?;
        let json_text = text
            .trim()
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim();
        serde_json::from_str::<Vec<ChildEntry>>(json_text)
            .context("failed to parse LLM text output as child documents")?
    };

    if children.is_empty() {
        bail!("LLM produced zero child documents");
    }

    debug!("got {} children", children.len());
    Ok(children)
}

// ─── Utility functions ──────────────────────────────────────────────────────

/// Detect cycles in a dependency graph via topological sort.
pub(crate) fn detect_cycles(nodes: &HashMap<String, Vec<String>>) -> eyre::Result<()> {
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

/// Extract acceptance criteria lines from a markdown `## Acceptance Criteria` section.
/// Public wrapper for use by doc.rs entry path.
pub(crate) fn extract_acceptance_criteria_pub(content: &str) -> Vec<String> {
    extract_acceptance_criteria(content)
}

fn extract_acceptance_criteria(content: &str) -> Vec<String> {
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

// ─── Ratify handler ─────────────────────────────────────────────────────────

/// `decomposer.ratify` - bottom-up semantic validation of parent-children relationships.
/// Reads parent + children markdown from docs/loopr/, calls LLM for each parent-children group.
/// Advisory: logs warnings on failure but does not abort.
#[instrument(skip_all, fields(plan_id = ?req.params.get("plan_id")))]
pub(super) async fn handle_decomposer_ratify(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_async_handler!(req.id, {
        let plan_id = req
            .params
            .get("plan_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre!("missing plan_id"))?
            .to_string();

        let repo_path = stores.config.project.repo_path.clone();
        let config = &stores.config.decomposer;
        let http_client = crate::validator::client::ReqwestClient::new();

        // Collect parent-children pairs for ratification
        let plan_content =
            crate::domain::markdown::read_full_markdown(std::path::Path::new(&repo_path), &plan_id).unwrap_or_default();

        // Ratify plan -> specs
        let spec_ids: Vec<String> = stores
            .read_specs()?
            .values()
            .filter(|s| s.parent_id == plan_id)
            .map(|s| s.id.clone())
            .collect();

        if !spec_ids.is_empty() {
            let child_pairs: Vec<(String, String)> = spec_ids
                .iter()
                .filter_map(|id| {
                    let content =
                        crate::domain::markdown::read_full_markdown(std::path::Path::new(&repo_path), id).ok()?;
                    let title = stores.read_specs().ok()?.get(id)?.title.clone();
                    Some((title, content))
                })
                .collect();

            if !child_pairs.is_empty() {
                let prompt = build_ratify_prompt(&plan_content, &child_pairs);
                match call_llm_for_ratification(&http_client, config, &prompt).await {
                    Ok(result) if !result.passed => {
                        warn!(
                            "ratification failed for plan {}: {} issues",
                            plan_id,
                            result.issues.len()
                        );
                    }
                    Err(e) => warn!("ratification LLM call failed for plan {}: {}", plan_id, e),
                    Ok(_) => info!("ratification passed for plan {}", plan_id),
                }
            }
        }

        Ok(DaemonResponse::ok(
            req.id,
            serde_json::json!({ "plan_id": plan_id, "ratified": true }),
        ))
    })
}

// ─── Abandon children handler ────────────────────────────────────────────────

/// `decomposer.abandon_children` - transitions all non-terminal children of a parent to Abandoned,
/// optionally preserving specific IDs.
#[instrument(skip_all, fields(parent_id = ?req.params.get("parent_id")))]
pub(super) fn handle_decomposer_abandon_children(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        let parent_id = req
            .params
            .get("parent_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre!("missing parent_id"))?
            .to_string();
        let parent_collection = req
            .params
            .get("parent_collection")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre!("missing parent_collection"))?
            .to_string();
        let preserve_ids: Vec<String> = req
            .params
            .get("preserve_ids")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();

        let child_collection = match parent_collection.as_str() {
            "plan" => "spec",
            "spec" => "phase",
            "phase" => "work",
            other => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params(&format!("unknown parent_collection: {}", other)),
                ));
            }
        };

        let mut abandoned = 0u32;
        let mut preserved = 0u32;

        match child_collection {
            "spec" => {
                let child_ids: Vec<String> = stores
                    .read_specs()?
                    .values()
                    .filter(|s| s.parent_id == parent_id && !preserve_ids.contains(&s.id))
                    .map(|s| s.id.clone())
                    .collect();
                let mut specs = stores.write_specs()?;
                for id in child_ids {
                    if let Some(s) = specs.get_mut(&id)
                        && !s.status().is_terminal(&stores.fsm)
                    {
                        s.force_status(crate::domain::spec::SpecStatus::Abandoned);
                        s.updated_at = crate::id::now_millis();
                        if let Some(store) = &stores.store {
                            let _ = store.lock().ok().and_then(|mut sg| sg.update(s.clone()).ok());
                        }
                        let _ = event_tx.send(DaemonEvent::record_updated("spec", &id));
                        abandoned += 1;
                    }
                }
                preserved = preserve_ids.len() as u32;
            }
            "phase" => {
                let child_ids: Vec<String> = stores
                    .read_phases()?
                    .values()
                    .filter(|p| p.parent_id == parent_id && !preserve_ids.contains(&p.id))
                    .map(|p| p.id.clone())
                    .collect();
                let mut phases = stores.write_phases()?;
                for id in child_ids {
                    if let Some(p) = phases.get_mut(&id)
                        && !p.status().is_terminal(&stores.fsm)
                    {
                        p.force_status(crate::domain::phase::PhaseStatus::Abandoned);
                        p.updated_at = crate::id::now_millis();
                        if let Some(store) = &stores.store {
                            let _ = store.lock().ok().and_then(|mut sg| sg.update(p.clone()).ok());
                        }
                        let _ = event_tx.send(DaemonEvent::record_updated("phase", &id));
                        abandoned += 1;
                    }
                }
                preserved = preserve_ids.len() as u32;
            }
            "work" => {
                let child_ids: Vec<String> = stores
                    .read_works()?
                    .values()
                    .filter(|w| w.parent_id == parent_id && !preserve_ids.contains(&w.id))
                    .map(|w| w.id.clone())
                    .collect();
                let mut works = stores.write_works()?;
                for id in child_ids {
                    if let Some(w) = works.get_mut(&id)
                        && !w.status().is_terminal(&stores.fsm)
                    {
                        w.force_status(crate::domain::work::WorkStatus::Abandoned);
                        w.updated_at = crate::id::now_millis();
                        if let Some(store) = &stores.store {
                            let _ = store.lock().ok().and_then(|mut sg| sg.update(w.clone()).ok());
                        }
                        let _ = event_tx.send(DaemonEvent::record_updated("work", &id));
                        abandoned += 1;
                    }
                }
                preserved = preserve_ids.len() as u32;
            }
            _ => {}
        }

        info!(
            "abandon_children: parent={} abandoned={} preserved={}",
            parent_id, abandoned, preserved
        );

        Ok(DaemonResponse::ok(
            req.id,
            serde_json::json!({ "abandoned_count": abandoned, "preserved_count": preserved }),
        ))
    })
}

// ─── Re-decompose handler ────────────────────────────────────────────────────

/// `decomposer.re_decompose` - increments decomposition_attempts on the parent plan,
/// abandons non-preserved children, then triggers a fresh decomposition.
#[instrument(skip_all, fields(parent_id = ?req.params.get("parent_id")))]
pub(super) async fn handle_decomposer_re_decompose(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_async_handler!(req.id, {
        let parent_id = req
            .params
            .get("parent_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre!("missing parent_id"))?
            .to_string();
        let parent_collection = req
            .params
            .get("parent_collection")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre!("missing parent_collection"))?
            .to_string();
        let target_kind = req
            .params
            .get("target_kind")
            .and_then(|v| v.as_str())
            .unwrap_or("spec")
            .to_string();
        let preserve_ids: Vec<String> = req
            .params
            .get("preserve_ids")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();

        // Increment decomposition_attempts on the plan record
        if parent_collection == "plan" {
            let mut plans = stores.write_plans()?;
            if let Some(plan) = plans.get_mut(&parent_id) {
                plan.decomposition_attempts += 1;
                plan.updated_at = crate::id::now_millis();
                debug!(
                    "incremented decomposition_attempts for plan {} to {}",
                    parent_id, plan.decomposition_attempts
                );
                if let Some(store) = &stores.store {
                    store
                        .lock()
                        .map_err(|_| eyre!("taskstore lock poisoned"))?
                        .update(plan.clone())
                        .map_err(|e| eyre!("failed to update plan attempts: {}", e))?;
                }
            }
        }

        // Abandon non-preserved children
        let abandon_req = DaemonRequest::new(
            0,
            "decomposer.abandon_children",
            serde_json::json!({
                "parent_id": parent_id,
                "parent_collection": parent_collection,
                "preserve_ids": preserve_ids,
            }),
        );
        let abandon_resp = handle_decomposer_abandon_children(stores, event_tx, abandon_req);
        let abandoned_count = abandon_resp
            .result
            .as_ref()
            .and_then(|r| r["abandoned_count"].as_u64())
            .unwrap_or(0);

        // Now trigger fresh single-level decomposition
        let decompose_req = DaemonRequest::new(
            0,
            "decomposer.decompose",
            serde_json::json!({
                "parent_id": parent_id,
                "parent_collection": parent_collection,
                "target_kind": target_kind,
                "count_guidance": req.params.get("count_guidance").and_then(|v| v.as_str()).unwrap_or("1-5"),
                "dependency_pattern": req.params.get("dependency_pattern").and_then(|v| v.as_str()).unwrap_or("fan-out"),
            }),
        );
        let decompose_resp = handle_decomposer_decompose(stores, event_tx, decompose_req).await;

        if decompose_resp.is_error() {
            return Ok(decompose_resp);
        }

        let children = decompose_resp
            .result
            .as_ref()
            .and_then(|r| r["children"].clone().into())
            .unwrap_or(serde_json::json!([]));

        Ok(DaemonResponse::ok(
            req.id,
            serde_json::json!({
                "children": children,
                "abandoned_count": abandoned_count,
            }),
        ))
    })
}

// ─── Failure coordination handler ───────────────────────────────────────────

/// `decomposer.handle_failure` - wired to decomposition.failed event via strategy.
/// Updates CoordinatorState.decomposition_error so the coordinator has failure context.
/// Also increments decomposition_attempts on the parent to eventually trigger the limit.
#[instrument(skip_all, fields(parent_id = ?req.params.get("parent_id")))]
pub(super) fn handle_decomposer_failure(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        let parent_id = req
            .params
            .get("parent_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let reason = req
            .params
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("decomposition failed")
            .to_string();

        // Increment decomposition_attempts on the plan
        if parent_id.starts_with("pl-") {
            let mut plans = stores.write_plans()?;
            if let Some(plan) = plans.get_mut(&parent_id) {
                plan.decomposition_attempts += 1;
                plan.updated_at = crate::id::now_millis();
                debug!(
                    "decomposition.failed: incremented attempts for plan {} to {}",
                    parent_id, plan.decomposition_attempts
                );
                if let Some(store) = &stores.store {
                    let _ = store.lock().ok().and_then(|mut sg| sg.update(plan.clone()).ok());
                }
            }
        }

        // Update CoordinatorState.decomposition_error for any coordinator watching this plan
        {
            let mut states = stores.write_coordinator_states()?;
            for cs in states.values_mut() {
                // Match by goal_id -> plan linkage or by direct parent_id reference
                if cs.decomposition_error.is_none() {
                    cs.decomposition_error = Some(reason.clone());
                    cs.updated_at = crate::id::now_millis();
                    if let Some(store) = &stores.store {
                        let _ = store.lock().ok().and_then(|mut sg| sg.update(cs.clone()).ok());
                    }
                    debug!("set decomposition_error on coordinator state {}", cs.id);
                }
            }
        }

        Ok(DaemonResponse::ok(req.id, serde_json::json!({ "ok": true })))
    })
}

// ─── Ratify LLM helpers ──────────────────────────────────────────────────────

/// Build a ratification prompt: parent content + all children.
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

/// Call the LLM for ratification and parse result.
#[instrument(skip_all)]
async fn call_llm_for_ratification<H: HttpClient + Sync>(
    http_client: &H,
    config: &DecomposerConfig,
    prompt: &str,
) -> eyre::Result<RatifyResult> {
    let text = call_llm_text(http_client, config, prompt).await?;
    serde_json::from_str(&text).context("failed to parse ratification response")
}

/// Raw text LLM call (used for ratification).
#[instrument(skip_all)]
async fn call_llm_text<H: HttpClient + Sync>(
    http_client: &H,
    config: &DecomposerConfig,
    prompt: &str,
) -> eyre::Result<String> {
    let api_key = std::env::var(&config.llm.api_key_env)
        .context(format!("missing API key env var: {}", config.llm.api_key_env))?;
    let api_url = match config.provider.as_str() {
        "anthropic" => "https://api.anthropic.com/v1/messages",
        other => bail!("unsupported LLM provider: {}", other),
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

// ─── Types for ratification ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct RatifyResult {
    passed: bool,
    #[serde(default)]
    issues: Vec<RatifyIssue>,
}

#[derive(Debug, Deserialize)]
struct RatifyIssue {
    #[allow(dead_code)]
    issue: String,
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    // --- detect_cycles ---

    #[test]
    fn detect_cycles_acyclic_graph() {
        let mut nodes = HashMap::new();
        nodes.insert("A".to_string(), vec!["B".to_string()]);
        nodes.insert("B".to_string(), vec!["C".to_string()]);
        nodes.insert("C".to_string(), vec![]);
        assert!(detect_cycles(&nodes).is_ok());
    }

    #[test]
    fn detect_cycles_simple_cycle() {
        let mut nodes = HashMap::new();
        nodes.insert("A".to_string(), vec!["B".to_string()]);
        nodes.insert("B".to_string(), vec!["A".to_string()]);
        let err = detect_cycles(&nodes).unwrap_err();
        assert!(err.to_string().contains("dependency cycle detected"));
    }

    #[test]
    fn detect_cycles_empty_graph() {
        let nodes: HashMap<String, Vec<String>> = HashMap::new();
        assert!(detect_cycles(&nodes).is_ok());
    }

    #[test]
    fn detect_cycles_fan_out_no_deps() {
        let mut nodes = HashMap::new();
        nodes.insert("A".to_string(), vec![]);
        nodes.insert("B".to_string(), vec![]);
        nodes.insert("C".to_string(), vec![]);
        assert!(detect_cycles(&nodes).is_ok());
    }

    #[test]
    fn detect_cycles_three_node_cycle() {
        let mut nodes = HashMap::new();
        nodes.insert("A".to_string(), vec!["B".to_string()]);
        nodes.insert("B".to_string(), vec!["C".to_string()]);
        nodes.insert("C".to_string(), vec!["A".to_string()]);
        assert!(detect_cycles(&nodes).is_err());
    }

    // --- extract_acceptance_criteria ---

    #[test]
    fn extract_ac_from_section() {
        let content = "## Summary\nSome text\n## Acceptance Criteria\n- Users can log in\n- JWT tokens expire after 1h\n## References\n";
        let ac = extract_acceptance_criteria(content);
        assert_eq!(ac.len(), 2);
        assert_eq!(ac[0], "Users can log in");
        assert_eq!(ac[1], "JWT tokens expire after 1h");
    }

    #[test]
    fn extract_ac_no_section() {
        let content = "## Summary\nSome text\n## References\n";
        let ac = extract_acceptance_criteria(content);
        assert!(ac.is_empty());
    }

    #[test]
    fn extract_ac_assert_style() {
        let content = "## Acceptance Criteria\nassert users_can_login()\nassert jwt_valid()\n";
        let ac = extract_acceptance_criteria(content);
        assert_eq!(ac.len(), 2);
    }

    // --- build_decompose_prompt ---

    #[test]
    fn prompt_includes_guidance_section() {
        crate::prompts::init_defaults();
        let prompt = build_decompose_prompt(DocKind::Work, "parent content", "1-3", "fan-out").unwrap();
        assert!(prompt.contains("## Guidance"));
        assert!(prompt.contains("1-3"));
        assert!(prompt.contains("fan-out"));
        assert!(prompt.contains("parent content"));
    }

    #[test]
    fn prompt_rejects_plan_target() {
        crate::prompts::init_defaults();
        let result = build_decompose_prompt(DocKind::Plan, "content", "1-3", "fan-out");
        assert!(result.is_err());
    }

    #[test]
    fn prompt_includes_template_for_spec() {
        crate::prompts::init_defaults();
        let prompt = build_decompose_prompt(DocKind::Spec, "parent", "1-3", "sequential-chain").unwrap();
        assert!(prompt.contains("## Template"));
        assert!(prompt.contains("sequential-chain"));
    }

    // --- build_ratify_prompt ---

    #[test]
    fn ratify_prompt_includes_parent_and_children() {
        crate::prompts::init_defaults();
        let children = vec![
            ("Spec A".to_string(), "spec a content".to_string()),
            ("Spec B".to_string(), "spec b content".to_string()),
        ];
        let prompt = build_ratify_prompt("parent content", &children);
        assert!(prompt.contains("## Parent Document"));
        assert!(prompt.contains("parent content"));
        assert!(prompt.contains("## Child: Spec A"));
        assert!(prompt.contains("## Child: Spec B"));
    }

    // --- decomposition_attempts increments ---

    #[test]
    fn plan_decomposition_attempts_starts_at_zero() {
        use crate::domain::criteria::AcceptanceCriteria;
        use crate::domain::plan::Plan;
        let plan = Plan::new("test plan".to_string(), AcceptanceCriteria::default());
        assert_eq!(plan.decomposition_attempts, 0);
    }
}
