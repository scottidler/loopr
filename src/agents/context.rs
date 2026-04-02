use std::cmp::Ordering;
use std::collections::HashMap;

use eyre::{Result, eyre};
use log::{debug, warn};

use crate::agents::error::AgentError;

use crate::daemon::context::Stores;
use crate::domain::learning::{Learning, LearningScope};
use crate::domain::role::Role;
use crate::guidance::AgentGuidance;
use crate::tools::ToolExecutor;
use crate::tools::ToolRunner;

// =====================================================
// Learning Selection
// =====================================================

/// Select learnings relevant to the given scope chain and role.
///
/// Filters by:
/// 1. **Scope**: matches any `(source_id, scope)` pair in `scope_ids`, or `LearningScope::Global`
/// 2. **Role**: `applicable_roles` contains `role`, or `None` (applies to all roles)
/// 3. **Confidence**: promoted learnings (policies) are always included; others must meet `min_confidence`
///
/// Results are sorted: promoted first, then by confidence DESC, then by recency DESC.
/// Truncated to `max_count` items.
///
/// MVP4 note: age-based decay is designed but deferred to MVP5.
pub fn select_learnings<'a>(
    learnings: &'a HashMap<String, Learning>,
    scope_ids: &[(&str, LearningScope)],
    role: Role,
    min_confidence: f32,
    max_count: usize,
) -> Vec<&'a Learning> {
    let mut candidates: Vec<&Learning> = learnings
        .values()
        .filter(|l| {
            // Scope match: this item or any ancestor, or Global
            scope_ids
                .iter()
                .any(|(id, scope)| l.source_id == *id && l.scope == *scope)
                || l.scope == LearningScope::Global
        })
        .filter(|l| {
            // Role match: applicable to this role, or applicable to all (None)
            l.applicable_roles
                .as_ref()
                .map(|roles| roles.contains(&role))
                .unwrap_or(true)
        })
        .filter(|l| {
            // Confidence match: promoted (policies) always included, others need min_confidence
            // MVP4: no age decay — effective_confidence == l.confidence
            l.promoted || l.confidence >= min_confidence
        })
        .collect();

    // Sort: promoted first, then confidence DESC, then recency DESC
    candidates.sort_by(|a, b| {
        b.promoted
            .cmp(&a.promoted)
            .then(b.confidence.partial_cmp(&a.confidence).unwrap_or(Ordering::Equal))
            .then(b.updated_at.cmp(&a.updated_at))
    });

    candidates.truncate(max_count);
    candidates
}

// =====================================================
// Token Budgeting
// =====================================================

/// Estimate token count from text. Approximation: ~4 characters per token.
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// Truncate text at the last sentence boundary within the token budget.
/// Keeps the **head** (oldest content) and drops the tail.
/// Appends `[truncated]` if truncation occurs.
fn truncate_prose(text: &str, max_tokens: usize) -> String {
    let max_chars = max_tokens * 4;
    if text.len() <= max_chars {
        return text.to_string();
    }
    let slice = &text[..max_chars];
    if let Some(pos) = slice.rfind(". ") {
        format!("{}. [truncated]", &slice[..pos])
    } else if let Some(pos) = slice.rfind('\n') {
        format!("{}\n[truncated]", &slice[..pos])
    } else {
        format!("{} [truncated]", slice)
    }
}

/// Truncate text from the **head** (oldest content), keeping the **tail** (newest).
/// Used for accumulated iteration history where recent context is most relevant.
/// Prepends `[earlier iterations truncated]` if truncation occurs.
fn truncate_from_head(text: &str, max_tokens: usize) -> String {
    let max_chars = max_tokens * 4;
    if text.len() <= max_chars {
        return text.to_string();
    }
    let start = text.len() - max_chars;
    // Find a clean break point (newline) after the cut point
    if let Some(pos) = text[start..].find('\n') {
        format!("[earlier iterations truncated]\n{}", &text[start + pos + 1..])
    } else {
        format!("[earlier iterations truncated] {}", &text[start..])
    }
}

/// Truncate a list by dropping items from the end until under token budget.
fn truncate_list(items: &[String], max_tokens: usize) -> Vec<String> {
    let mut result = Vec::new();
    let mut total_tokens = 0;
    for item in items {
        let item_tokens = estimate_tokens(item) + 1; // +1 for "- " prefix + newline overhead
        if total_tokens + item_tokens > max_tokens {
            break;
        }
        result.push(item.clone());
        total_tokens += item_tokens;
    }
    result
}

// =====================================================
// TokenBudget
// =====================================================

/// Per-section token budget for variable data in the user message.
///
/// The system prompt (.pmt template) is NEVER truncated - it is our source code
/// and must be delivered to the LLM verbatim. These budgets only apply to
/// variable-length data (diffs, learnings, state summaries, etc.).
///
/// Token estimation: ~4 characters per token.
#[derive(Debug, Clone)]
pub struct TokenBudget {
    pub work_target: usize,
    pub hierarchy: usize,
    pub learnings: usize,
    pub state_summary: usize,
    pub tools_or_actions: usize,
    pub previous_summary: usize,
    pub guidance: usize,
}

impl TokenBudget {
    /// Default budget allocation per role (from MVP4 design doc + guidance system).
    pub fn for_role(role: Role) -> Self {
        match role {
            Role::Coordinator => Self {
                work_target: 500,
                hierarchy: 3000,
                learnings: 1500,
                state_summary: 3000,
                tools_or_actions: 500,
                previous_summary: 700,
                guidance: 1500,
            },
            Role::Researcher => Self {
                work_target: 1000,
                hierarchy: 1000,
                learnings: 1000,
                state_summary: 0,
                tools_or_actions: 300,
                previous_summary: 700,
                guidance: 500,
            },
            Role::Implementer => Self {
                work_target: 1000,
                hierarchy: 2000,
                learnings: 2000,
                state_summary: 2000,
                tools_or_actions: 500,
                previous_summary: 4000,
                guidance: 800,
            },
            Role::Reviewer => Self {
                work_target: 1000,
                hierarchy: 2000,
                learnings: 2000,
                state_summary: 1000,
                tools_or_actions: 0,
                previous_summary: 0,
                guidance: 500,
            },
            Role::Integrator => Self {
                work_target: 500,
                hierarchy: 500,
                learnings: 1000,
                state_summary: 1500,
                tools_or_actions: 400,
                previous_summary: 500,
                guidance: 800,
            },
        }
    }
}

// =====================================================
// AssembledContext
// =====================================================

/// Assembled context ready for an LLM call.
#[derive(Debug, Clone)]
pub struct AssembledContext {
    pub system_prompt: String,
    pub user_message: String,
    pub token_estimate: usize,
}

// =====================================================
// ContextBuilder
// =====================================================

/// Resolved dependency metadata for hierarchy rendering.
struct DependencySummary {
    title: String,
    status: String,
    resource_tags: Vec<String>,
}

/// Role-agnostic context assembly with token budgeting.
///
/// Replaces per-agent `load_context()` + `build_user_message()` with a generic builder.
/// Uses lock-snapshot pattern: acquires each read lock briefly, clones data, releases.
pub struct ContextBuilder<'a> {
    stores: &'a Stores,
    role: Role,
    budget: TokenBudget,
    // Loaded hierarchy: (title, description)
    plan: Option<(String, String)>,
    spec: Option<(String, String)>,
    phase: Option<(String, String)>,
    work: Option<(String, String)>,
    // Work enrichment: acceptance criteria, file scope, dependency context
    work_acceptance_criteria: Vec<String>,
    work_resource_tags: Vec<String>,
    dependency_summaries: Vec<DependencySummary>,
    // Learning scope chain
    scope_ids: Vec<(String, LearningScope)>,
    // IDs for sibling lookups
    work_id: Option<String>,
    phase_id: Option<String>,
    // Optional sections
    bundle_info: Option<(String, Vec<String>, Vec<String>)>, // (id, claims, touched_paths)
    bundle_diff: Option<String>,
    bundle_noop_reason: Option<String>,
    /// For noop bundles: file contents read from repo for Reviewer verification.
    noop_file_contents: Option<Vec<(String, String)>>,
    tools: Vec<String>,
    previous_summary: Option<String>,
    staleness_note: Option<String>,
    state_summary: Option<String>,
    coordinator_goal: Option<String>,
    guidance_text: Option<String>,
    iteration: Option<u32>,
    footer: Option<String>,
}

impl std::fmt::Debug for ContextBuilder<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextBuilder")
            .field("role", &self.role)
            .field("budget", &self.budget)
            .finish_non_exhaustive()
    }
}

impl<'a> ContextBuilder<'a> {
    pub fn new(stores: &'a Stores, role: Role) -> Self {
        debug!("ContextBuilder::new(role={:?})", role);
        let budget = TokenBudget::for_role(role);
        Self {
            stores,
            role,
            budget,
            plan: None,
            spec: None,
            phase: None,
            work: None,
            work_acceptance_criteria: Vec::new(),
            work_resource_tags: Vec::new(),
            dependency_summaries: Vec::new(),
            scope_ids: Vec::new(),
            work_id: None,
            phase_id: None,
            bundle_info: None,
            bundle_diff: None,
            bundle_noop_reason: None,
            noop_file_contents: None,
            tools: Vec::new(),
            previous_summary: None,
            staleness_note: None,
            state_summary: None,
            coordinator_goal: None,
            guidance_text: None,
            iteration: None,
            footer: None,
        }
    }

    /// Load the full hierarchy from a work ID: Work -> Phase -> Spec -> Plan.
    pub fn load_work_hierarchy(mut self, work_id: &str) -> Result<Self> {
        debug!("ContextBuilder::load_work_hierarchy(work_id={})", work_id);
        let (wi_title, wi_desc, phase_id, wi_ac, wi_rt, dep_summaries) = {
            let guard = self.stores.read_works()?;
            let wi = guard.get(work_id).ok_or_else(|| eyre!("work not found: {}", work_id))?;
            let deps: Vec<DependencySummary> = wi
                .dependencies
                .iter()
                .filter_map(|dep_id| {
                    guard.get(dep_id).map(|dep| DependencySummary {
                        title: dep.title.clone(),
                        status: dep.status.to_string(),
                        resource_tags: dep.resource_tags.clone(),
                    })
                })
                .collect();
            (
                wi.title.clone(),
                wi.description.clone(),
                wi.phase_id.clone(),
                wi.acceptance_criteria.clone(),
                wi.resource_tags.clone(),
                deps,
            )
        };

        let (ph_title, ph_desc, spec_id, phase_id_owned) = {
            let guard = self.stores.read_phases()?;
            let phase = guard
                .get(&phase_id)
                .ok_or_else(|| eyre!("phase not found: {}", phase_id))?;
            (
                phase.title.clone(),
                phase.description.clone(),
                phase.spec_id.clone(),
                phase.id.clone(),
            )
        };

        let (spec_title, spec_desc, plan_id, spec_id_owned) = {
            let guard = self.stores.read_specs()?;
            let spec = guard
                .get(&spec_id)
                .ok_or_else(|| eyre!("spec not found: {}", spec_id))?;
            (
                spec.title.clone(),
                spec.description.clone(),
                spec.plan_id.clone(),
                spec.id.clone(),
            )
        };

        let (plan_title, plan_desc, plan_id_owned) = {
            let guard = self.stores.read_plans()?;
            let plan = guard
                .get(&plan_id)
                .ok_or_else(|| eyre!("plan not found: {}", plan_id))?;
            (plan.title.clone(), plan.description.clone(), plan.id.clone())
        };

        self.plan = Some((plan_title, plan_desc));
        self.spec = Some((spec_title, spec_desc));
        self.phase = Some((ph_title, ph_desc));
        self.work = Some((wi_title, wi_desc));
        self.work_acceptance_criteria = wi_ac;
        self.work_resource_tags = wi_rt;
        self.dependency_summaries = dep_summaries;
        self.work_id = Some(work_id.to_string());
        self.phase_id = Some(phase_id_owned.clone());
        self.scope_ids = vec![
            (work_id.to_string(), LearningScope::Work),
            (phase_id_owned, LearningScope::Phase),
            (spec_id_owned, LearningScope::Spec),
            (plan_id_owned, LearningScope::Plan),
        ];

        Ok(self)
    }

    /// Load hierarchy from a bundle ID: Bundle -> Work -> Phase -> Spec -> Plan.
    pub fn load_bundle_hierarchy(mut self, bundle_id: &str) -> Result<Self> {
        debug!("ContextBuilder::load_bundle_hierarchy(bundle_id={})", bundle_id);
        let (bid, claims, touched_paths, work_id, noop_reason) = {
            let guard = self.stores.read_bundles()?;
            let bundle = guard
                .get(bundle_id)
                .ok_or_else(|| eyre!("bundle not found: {}", bundle_id))?;
            (
                bundle.id.clone(),
                bundle.claims.clone(),
                bundle.touched_paths.clone(),
                bundle.work_id.clone(),
                bundle.noop_reason.clone(),
            )
        };

        self.bundle_info = Some((bid, claims, touched_paths.clone()));
        self.bundle_noop_reason = noop_reason.clone();

        if noop_reason.is_some() {
            // Noop bundle: skip git diff (no branch exists). Instead, read
            // relevant files from the repo so the Reviewer can verify the
            // codebase state against acceptance criteria.
            let repo_path = &self.stores.config.project.repo_path;
            let resource_tags = {
                let works = self.stores.read_works()?;
                works.get(&work_id).map(|w| w.resource_tags.clone()).unwrap_or_default()
            };
            // Prefer touched_paths (files the Implementer verified) over resource_tags
            let paths_to_read: Vec<String> = if touched_paths.is_empty() { resource_tags } else { touched_paths };
            let mut file_contents = Vec::new();
            for path in &paths_to_read {
                let full_path = repo_path.join(path);
                if let Ok(content) = std::fs::read_to_string(&full_path) {
                    file_contents.push((path.clone(), content));
                }
            }
            self.noop_file_contents = if file_contents.is_empty() { None } else { Some(file_contents) };
        } else {
            // Normal bundle: load the git diff from the worktree branch
            let diff = {
                let branch = format!("agent/{}", work_id);
                let repo_path = &self.stores.config.project.repo_path;
                let output = std::process::Command::new("git")
                    .args(["diff", "HEAD", &branch, "--stat", "-p"])
                    .current_dir(repo_path)
                    .output();
                match output {
                    Ok(o) if o.status.success() => {
                        let d = String::from_utf8_lossy(&o.stdout).to_string();
                        if d.trim().is_empty() { None } else { Some(d) }
                    }
                    _ => None,
                }
            };
            self.bundle_diff = diff;
        }

        self.load_work_hierarchy(&work_id)
    }

    /// Inject assembled guidance (schema + global + project LOOPR.md).
    pub fn with_guidance(mut self, guidance: &AgentGuidance) -> Self {
        debug!("ContextBuilder::with_guidance()");
        self.guidance_text = Some(crate::guidance::assemble_guidance(
            guidance,
            self.role,
            self.budget.guidance,
        ));
        self
    }

    pub fn with_tools(mut self, tool_runner: &ToolRunner) -> Self {
        debug!("ContextBuilder::with_tools()");
        self.tools = tool_runner.available_tools().into_iter().map(String::from).collect();
        self
    }

    pub fn with_tool_executor(mut self, executor: &ToolExecutor) -> Self {
        debug!("ContextBuilder::with_tool_executor()");
        self.tools = executor.available_tools().into_iter().map(String::from).collect();
        self
    }

    pub fn with_previous_summary(mut self, summary: Option<String>) -> Self {
        debug!(
            "ContextBuilder::with_previous_summary(has_summary={})",
            summary.is_some()
        );
        self.previous_summary = summary;
        self
    }

    pub fn with_staleness_note(mut self, note: Option<String>) -> Self {
        debug!("ContextBuilder::with_staleness_note(has_note={})", note.is_some());
        self.staleness_note = note;
        self
    }

    pub fn with_iteration(mut self, iteration: u32) -> Self {
        debug!("ContextBuilder::with_iteration(iteration={})", iteration);
        self.iteration = Some(iteration);
        self
    }

    pub fn with_state_summary(mut self, summary: String) -> Self {
        debug!("ContextBuilder::with_state_summary()");
        self.state_summary = Some(summary);
        self
    }

    pub fn with_footer(mut self, footer: String) -> Self {
        debug!("ContextBuilder::with_footer()");
        self.footer = Some(footer);
        self
    }

    /// Set the coordinator goal from stores (reads the active goal).
    pub fn with_coordinator_goal(mut self) -> Self {
        debug!("ContextBuilder::with_coordinator_goal()");
        let goal = {
            let Ok(goals) = self.stores.read_coordinator_goals() else {
                return self;
            };
            goals.values().find(|g| g.active).map(|g| g.goal.clone())
        };
        self.coordinator_goal = goal;
        self
    }

    /// Access the loaded work title (available after load_*_hierarchy).
    pub fn work_title(&self) -> Option<&str> {
        debug!("ContextBuilder::work_title()");
        self.work.as_ref().map(|(t, _)| t.as_str())
    }

    /// Build the assembled context with per-section token budgeting.
    pub fn build(&self, system_prompt: &str) -> Result<AssembledContext> {
        debug!("ContextBuilder::build(system_prompt_len={})", system_prompt.len());
        let mut msg = String::with_capacity(4096);

        // --- Project Goal section (before hierarchy) ---
        if let Some(ref goal) = self.coordinator_goal {
            msg.push_str("## Project Goal\n\n");
            msg.push_str(goal);
            msg.push_str("\n\n");
        }

        // --- Guidance section (schema + global + project LOOPR.md) ---
        if let Some(ref guidance) = self.guidance_text {
            msg.push_str(guidance);
        }

        // --- Hierarchy section ---
        if self.plan.is_some() || self.spec.is_some() || self.phase.is_some() || self.work.is_some() {
            let mut hierarchy = String::new();
            hierarchy.push_str("## Hierarchy\n\n");
            if let Some((ref title, ref desc)) = self.plan {
                hierarchy.push_str(&format!("**Plan:** {} — {}\n", title, desc));
            }
            if let Some((ref title, ref desc)) = self.spec {
                hierarchy.push_str(&format!("**Spec:** {} — {}\n", title, desc));
            }
            if let Some((ref title, ref desc)) = self.phase {
                hierarchy.push_str(&format!("**Phase:** {} — {}\n", title, desc));
            }
            if let Some((ref title, ref desc)) = self.work {
                hierarchy.push_str(&format!("**Work:** {} — {}\n", title, desc));
            }
            if !self.work_acceptance_criteria.is_empty() {
                hierarchy.push_str("\n**Acceptance Criteria:**\n");
                for ac in &self.work_acceptance_criteria {
                    hierarchy.push_str(&format!("- {}\n", ac));
                }
            }
            if !self.work_resource_tags.is_empty() {
                hierarchy.push_str("\n**Allowed Files:**\n");
                for tag in &self.work_resource_tags {
                    hierarchy.push_str(&format!("- {}\n", tag));
                }
            }
            if !self.dependency_summaries.is_empty() {
                hierarchy.push_str("\n**Dependencies:**\n");
                for dep in &self.dependency_summaries {
                    let files = dep.resource_tags.join(", ");
                    hierarchy.push_str(&format!("- [{}] {} - files: {}\n", dep.status, dep.title, files));
                }
            }
            hierarchy.push('\n');

            if estimate_tokens(&hierarchy) > self.budget.hierarchy {
                warn!("Hierarchy section exceeds token budget, truncating");
                msg.push_str(&truncate_prose(&hierarchy, self.budget.hierarchy));
            } else {
                msg.push_str(&hierarchy);
            }
        }

        // --- Sibling Works section (after hierarchy) ---
        if let (Some(phase_id), Some(current_wi_id)) = (&self.phase_id, &self.work_id)
            && let Ok(works) = self.stores.read_works()
        {
            let siblings: Vec<String> = works
                .values()
                .filter(|wi| wi.phase_id == *phase_id && wi.id != *current_wi_id)
                .map(|wi| format!("- [{}] {}", wi.status, wi.title))
                .collect();
            if !siblings.is_empty() {
                msg.push_str("## Sibling Works\n\n");
                for s in &siblings {
                    msg.push_str(s);
                    msg.push('\n');
                }
                msg.push('\n');
            }
        }

        // --- Bundle section (for reviewer) ---
        if let Some((ref id, ref claims, ref paths)) = self.bundle_info {
            let mut bundle_sec = String::new();
            bundle_sec.push_str("## Bundle Under Review\n\n");
            bundle_sec.push_str(&format!("**Bundle ID:** {}\n", id));
            bundle_sec.push_str(&format!("**Claims:** {}\n", claims.join(", ")));
            if !paths.is_empty() {
                bundle_sec.push_str("**Touched Paths:**\n");
                for path in paths {
                    bundle_sec.push_str(&format!("- `{}`\n", path));
                }
            }
            bundle_sec.push('\n');

            // Include either code diff (normal) or noop directive + file contents
            if let Some(ref reason) = self.bundle_noop_reason {
                bundle_sec.push_str("**NO-OP BUNDLE** - The Implementer made no code changes.\n\n");
                bundle_sec.push_str(&format!("**Implementer's claim:** {}\n\n", reason));
                bundle_sec.push_str(
                    "**Your task:** Do NOT look for a diff. Instead, use the file contents \
                     provided below and verify the codebase's CURRENT STATE against every \
                     acceptance criterion. If the criteria are already satisfied, approve. \
                     If not, reject with specifics about what is missing.\n\n",
                );
                if let Some(ref files) = self.noop_file_contents {
                    bundle_sec.push_str("**Current File Contents:**\n\n");
                    for (path, content) in files {
                        bundle_sec.push_str(&format!("### `{}`\n```\n", path));
                        if content.len() > 4000 {
                            bundle_sec.push_str(&content[..4000]);
                            bundle_sec.push_str("\n... [truncated]\n");
                        } else {
                            bundle_sec.push_str(content);
                        }
                        bundle_sec.push_str("```\n\n");
                    }
                }
            } else if let Some(ref diff) = self.bundle_diff {
                bundle_sec.push_str("**Code Changes:**\n```diff\n");
                // Truncate if too large
                if diff.len() > 8000 {
                    bundle_sec.push_str(&diff[..8000]);
                    bundle_sec.push_str("\n... [truncated]\n");
                } else {
                    bundle_sec.push_str(diff);
                }
                bundle_sec.push_str("```\n\n");
            }

            if estimate_tokens(&bundle_sec) > self.budget.work_target {
                warn!("Bundle section exceeds token budget, truncating");
                msg.push_str(&truncate_prose(&bundle_sec, self.budget.work_target));
            } else {
                msg.push_str(&bundle_sec);
            }
        }

        // --- Learnings section ---
        if let Ok(learnings_map) = self.stores.read_learnings() {
            let scope_refs: Vec<(&str, LearningScope)> =
                self.scope_ids.iter().map(|(id, scope)| (id.as_str(), *scope)).collect();
            let min_confidence = match self.role {
                Role::Coordinator => 0.6,
                _ => 0.3,
            };
            let selected = select_learnings(&learnings_map, &scope_refs, self.role, min_confidence, 20);

            if !selected.is_empty() {
                let learning_strings: Vec<String> = selected.iter().map(|l| l.content.clone()).collect();
                let truncated = truncate_list(&learning_strings, self.budget.learnings);

                if !truncated.is_empty() {
                    msg.push_str("## Learnings\n\n");
                    for l in &truncated {
                        msg.push_str(&format!("- {}\n", l));
                    }
                    msg.push('\n');
                }
            }
        }

        // --- Tools section ---
        if !self.tools.is_empty() && self.budget.tools_or_actions > 0 {
            let tool_strings: Vec<String> = self.tools.iter().map(|t| format!("`{}`", t)).collect();
            let truncated = truncate_list(&tool_strings, self.budget.tools_or_actions);

            if !truncated.is_empty() {
                msg.push_str("## Available Tools\n\n");
                for t in &truncated {
                    msg.push_str(&format!("- {}\n", t));
                }
                msg.push('\n');
            }
        }

        // --- State summary section ---
        if let Some(ref summary) = self.state_summary
            && self.budget.state_summary > 0
        {
            msg.push_str("## State Summary\n\n");
            if estimate_tokens(summary) > self.budget.state_summary {
                warn!("State summary exceeds token budget, truncating");
                msg.push_str(&truncate_prose(summary, self.budget.state_summary));
            } else {
                msg.push_str(summary);
            }
            msg.push_str("\n\n");
        }

        // --- Staleness warning ---
        if let Some(ref note) = self.staleness_note {
            msg.push_str("## Staleness Warning\n\n");
            msg.push_str(note);
            msg.push_str("\n\n");
        }

        // --- Previous iteration summary ---
        if let Some(ref summary) = self.previous_summary
            && self.budget.previous_summary > 0
        {
            msg.push_str("## Previous Iteration Summary\n\n");
            if estimate_tokens(summary) > self.budget.previous_summary {
                warn!("Previous summary exceeds token budget, truncating from head");
                msg.push_str(&truncate_from_head(summary, self.budget.previous_summary));
            } else {
                msg.push_str(summary);
            }
            msg.push_str("\n\n");
        }

        // --- Iteration and footer ---
        if let Some(iteration) = self.iteration {
            msg.push_str(&format!("## Current Iteration: {}\n\n", iteration));
        }
        if let Some(ref footer) = self.footer {
            msg.push_str(footer);
            msg.push('\n');
        }

        // System prompt is NEVER truncated. It is our .pmt template - our
        // source code for the agent's instructions and output schema. If it
        // doesn't fit, that's a hard error, not a silent degradation.
        let system_tokens = estimate_tokens(system_prompt);
        let msg_tokens = estimate_tokens(&msg);
        let token_estimate = system_tokens + msg_tokens;

        // Claude models have 200k token context windows. Reserve output tokens
        // (max_tokens from config, typically 4096-8192) plus a safety margin.
        // Input limit = 200k - 10k (conservative output+margin reserve).
        const MAX_INPUT_TOKENS: usize = 190_000;
        if token_estimate > MAX_INPUT_TOKENS {
            return Err(AgentError::ContextOverflow {
                tokens: token_estimate,
                limit: MAX_INPUT_TOKENS,
            }
            .into());
        }

        Ok(AssembledContext {
            system_prompt: system_prompt.to_string(),
            user_message: msg,
            token_estimate,
        })
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests;
