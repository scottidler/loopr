use std::cmp::Ordering;
use std::collections::HashMap;

use eyre::{Result, eyre};
use log::warn;

use crate::daemon::context::Stores;
use crate::domain::learning::{Learning, LearningScope};
use crate::domain::role::Role;
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

/// Per-section token budget allocation.
/// Token estimation: ~1.3 tokens per word (~4 characters per token).
#[derive(Debug, Clone)]
pub struct TokenBudget {
    pub system_prompt: usize,
    pub work_target: usize,
    pub hierarchy: usize,
    pub learnings: usize,
    pub state_summary: usize,
    pub tools_or_actions: usize,
    pub previous_summary: usize,
}

impl TokenBudget {
    /// Default budget allocation per role (from MVP4 design doc).
    pub fn for_role(role: Role) -> Self {
        match role {
            Role::Coordinator => Self {
                system_prompt: 800,
                work_target: 500,
                hierarchy: 3000,
                learnings: 1500,
                state_summary: 3000,
                tools_or_actions: 500,
                previous_summary: 700,
            },
            Role::Researcher => Self {
                system_prompt: 500,
                work_target: 1000,
                hierarchy: 1000,
                learnings: 1000,
                state_summary: 0,
                tools_or_actions: 300,
                previous_summary: 700,
            },
            Role::Implementer => Self {
                system_prompt: 500,
                work_target: 1000,
                hierarchy: 2000,
                learnings: 2000,
                state_summary: 2000,
                tools_or_actions: 500,
                previous_summary: 4000,
            },
            Role::Reviewer => Self {
                system_prompt: 500,
                work_target: 1000,
                hierarchy: 2000,
                learnings: 2000,
                state_summary: 1000,
                tools_or_actions: 0,
                previous_summary: 0,
            },
            Role::Integrator => Self {
                system_prompt: 600,
                work_target: 500,
                hierarchy: 500,
                learnings: 1000,
                state_summary: 1500,
                tools_or_actions: 400,
                previous_summary: 500,
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
    work_item: Option<(String, String)>,
    // Learning scope chain
    scope_ids: Vec<(String, LearningScope)>,
    // IDs for sibling lookups
    work_item_id: Option<String>,
    phase_id: Option<String>,
    // Optional sections
    bundle_info: Option<(String, String, Vec<String>)>, // (id, claims, touched_paths)
    bundle_diff: Option<String>,
    tools: Vec<String>,
    previous_summary: Option<String>,
    staleness_note: Option<String>,
    state_summary: Option<String>,
    coordinator_goal: Option<String>,
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
        let budget = TokenBudget::for_role(role);
        Self {
            stores,
            role,
            budget,
            plan: None,
            spec: None,
            phase: None,
            work_item: None,
            scope_ids: Vec::new(),
            work_item_id: None,
            phase_id: None,
            bundle_info: None,
            bundle_diff: None,
            tools: Vec::new(),
            previous_summary: None,
            staleness_note: None,
            state_summary: None,
            coordinator_goal: None,
            iteration: None,
            footer: None,
        }
    }

    /// Load the full hierarchy from a work item ID: WorkItem -> Phase -> Spec -> Plan.
    pub fn load_work_item_hierarchy(mut self, work_item_id: &str) -> Result<Self> {
        let (wi_title, wi_desc, phase_id) = {
            let guard = self.stores.work_items.read().unwrap();
            let wi = guard
                .get(work_item_id)
                .ok_or_else(|| eyre!("work item not found: {}", work_item_id))?;
            (wi.title.clone(), wi.description.clone(), wi.phase_id.clone())
        };

        let (ph_title, ph_desc, spec_id, phase_id_owned) = {
            let guard = self.stores.phases.read().unwrap();
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
            let guard = self.stores.specs.read().unwrap();
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
            let guard = self.stores.plans.read().unwrap();
            let plan = guard
                .get(&plan_id)
                .ok_or_else(|| eyre!("plan not found: {}", plan_id))?;
            (plan.title.clone(), plan.description.clone(), plan.id.clone())
        };

        self.plan = Some((plan_title, plan_desc));
        self.spec = Some((spec_title, spec_desc));
        self.phase = Some((ph_title, ph_desc));
        self.work_item = Some((wi_title, wi_desc));
        self.work_item_id = Some(work_item_id.to_string());
        self.phase_id = Some(phase_id_owned.clone());
        self.scope_ids = vec![
            (work_item_id.to_string(), LearningScope::WorkItem),
            (phase_id_owned, LearningScope::Phase),
            (spec_id_owned, LearningScope::Spec),
            (plan_id_owned, LearningScope::Plan),
        ];

        Ok(self)
    }

    /// Load hierarchy from a bundle ID: Bundle -> WorkItem -> Phase -> Spec -> Plan.
    pub fn load_bundle_hierarchy(mut self, bundle_id: &str) -> Result<Self> {
        let (bid, claims, touched_paths, work_item_id) = {
            let guard = self.stores.bundles.read().unwrap();
            let bundle = guard
                .get(bundle_id)
                .ok_or_else(|| eyre!("bundle not found: {}", bundle_id))?;
            (
                bundle.id.clone(),
                bundle.claims.clone(),
                bundle.touched_paths.clone(),
                bundle.work_item_id.clone(),
            )
        };

        self.bundle_info = Some((bid, claims, touched_paths));

        // Load the git diff from the worktree branch so the reviewer can see actual code
        let diff = {
            let branch = format!("agent/{}", work_item_id);
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

        self.load_work_item_hierarchy(&work_item_id)
    }

    pub fn with_tools(mut self, tool_runner: &ToolRunner) -> Self {
        self.tools = tool_runner.available_tools().into_iter().map(String::from).collect();
        self
    }

    pub fn with_previous_summary(mut self, summary: Option<String>) -> Self {
        self.previous_summary = summary;
        self
    }

    pub fn with_staleness_note(mut self, note: Option<String>) -> Self {
        self.staleness_note = note;
        self
    }

    pub fn with_iteration(mut self, iteration: u32) -> Self {
        self.iteration = Some(iteration);
        self
    }

    pub fn with_state_summary(mut self, summary: String) -> Self {
        self.state_summary = Some(summary);
        self
    }

    pub fn with_footer(mut self, footer: String) -> Self {
        self.footer = Some(footer);
        self
    }

    /// Set the coordinator goal from stores (reads the active goal).
    pub fn with_coordinator_goal(mut self) -> Self {
        let goal = {
            let goals = self.stores.coordinator_goals.read().unwrap();
            goals.values().find(|g| g.active).map(|g| g.goal.clone())
        };
        self.coordinator_goal = goal;
        self
    }

    /// Access the loaded work item title (available after load_*_hierarchy).
    pub fn work_item_title(&self) -> Option<&str> {
        self.work_item.as_ref().map(|(t, _)| t.as_str())
    }

    /// Build the assembled context with per-section token budgeting.
    pub fn build(&self, system_prompt: &str) -> AssembledContext {
        let mut msg = String::with_capacity(4096);

        // --- Project Goal section (before hierarchy) ---
        if let Some(ref goal) = self.coordinator_goal {
            msg.push_str("## Project Goal\n\n");
            msg.push_str(goal);
            msg.push_str("\n\n");
        }

        // --- Hierarchy section ---
        if self.plan.is_some() || self.spec.is_some() || self.phase.is_some() || self.work_item.is_some() {
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
            if let Some((ref title, ref desc)) = self.work_item {
                hierarchy.push_str(&format!("**WorkItem:** {} — {}\n", title, desc));
            }
            hierarchy.push('\n');

            if estimate_tokens(&hierarchy) > self.budget.hierarchy {
                warn!("Hierarchy section exceeds token budget, truncating");
                msg.push_str(&truncate_prose(&hierarchy, self.budget.hierarchy));
            } else {
                msg.push_str(&hierarchy);
            }
        }

        // --- Sibling Work Items section (after hierarchy) ---
        if let (Some(phase_id), Some(current_wi_id)) = (&self.phase_id, &self.work_item_id) {
            let work_items = self.stores.work_items.read().unwrap();
            let siblings: Vec<String> = work_items
                .values()
                .filter(|wi| wi.phase_id == *phase_id && wi.id != *current_wi_id)
                .map(|wi| format!("- [{}] {}", wi.status, wi.title))
                .collect();
            if !siblings.is_empty() {
                msg.push_str("## Sibling Work Items\n\n");
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
            bundle_sec.push_str(&format!("**Claims:** {}\n", claims));
            if !paths.is_empty() {
                bundle_sec.push_str("**Touched Paths:**\n");
                for path in paths {
                    bundle_sec.push_str(&format!("- `{}`\n", path));
                }
            }
            bundle_sec.push('\n');

            // Include the actual code diff so the reviewer can see the changes
            if let Some(ref diff) = self.bundle_diff {
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
        {
            let learnings_map = self.stores.learnings.read().unwrap();
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
                warn!("Previous summary exceeds token budget, truncating");
                msg.push_str(&truncate_prose(summary, self.budget.previous_summary));
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

        // System prompt (truncated if needed)
        let final_system = if estimate_tokens(system_prompt) > self.budget.system_prompt {
            warn!("System prompt exceeds token budget, truncating");
            truncate_prose(system_prompt, self.budget.system_prompt)
        } else {
            system_prompt.to_string()
        };

        let token_estimate = estimate_tokens(&final_system) + estimate_tokens(&msg);

        AssembledContext {
            system_prompt: final_system,
            user_message: msg,
            token_estimate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ProjectConfig, PromotionPolicy, ToolEntry};
    use crate::domain::bundle::{Bundle, BundleStatus};
    use crate::domain::learning::Learning;
    use crate::domain::phase::Phase;
    use crate::domain::plan::Plan;
    use crate::domain::spec::Spec;
    use crate::domain::work_item::WorkItem;
    use crate::tools::ToolRunner;
    use std::sync::{Arc, Mutex as StdMutex};
    use taskstore::Store;

    fn make_learning(source_id: &str, scope: LearningScope, content: &str) -> Learning {
        Learning::new(source_id.to_string(), scope, content.to_string())
    }

    fn make_learning_with_role(source_id: &str, scope: LearningScope, content: &str, roles: Vec<Role>) -> Learning {
        let mut l = make_learning(source_id, scope, content);
        l.applicable_roles = Some(roles);
        l
    }

    fn make_learning_with_confidence(
        source_id: &str,
        scope: LearningScope,
        content: &str,
        confidence: f32,
    ) -> Learning {
        let mut l = make_learning(source_id, scope, content);
        l.confidence = confidence;
        l
    }

    fn to_map(learnings: Vec<Learning>) -> HashMap<String, Learning> {
        learnings.into_iter().map(|l| (l.id.clone(), l)).collect()
    }

    // --- select_learnings: Basic scope filtering ---

    #[test]
    fn test_select_by_scope_workitem() {
        let l = make_learning("wi-1", LearningScope::WorkItem, "insight");
        let map = to_map(vec![l]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "insight");
    }

    #[test]
    fn test_select_by_scope_chain() {
        let l1 = make_learning("wi-1", LearningScope::WorkItem, "wi insight");
        let l2 = make_learning("phase-1", LearningScope::Phase, "phase insight");
        let l3 = make_learning("spec-1", LearningScope::Spec, "spec insight");
        let l4 = make_learning("plan-1", LearningScope::Plan, "plan insight");
        let map = to_map(vec![l1, l2, l3, l4]);

        let scope_ids = [
            ("wi-1", LearningScope::WorkItem),
            ("phase-1", LearningScope::Phase),
            ("spec-1", LearningScope::Spec),
            ("plan-1", LearningScope::Plan),
        ];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
        assert_eq!(result.len(), 4);
    }

    #[test]
    fn test_select_global_always_included() {
        let l = make_learning("global", LearningScope::Global, "global insight");
        let map = to_map(vec![l]);

        // Empty scope chain — only Global should match
        let result = select_learnings(&map, &[], Role::Implementer, 0.0, 20);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "global insight");
    }

    #[test]
    fn test_select_excludes_unrelated_scope() {
        let l1 = make_learning("wi-1", LearningScope::WorkItem, "relevant");
        let l2 = make_learning("wi-999", LearningScope::WorkItem, "unrelated");
        let map = to_map(vec![l1, l2]);

        let scope_ids = [("wi-1", LearningScope::WorkItem)];
        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "relevant");
    }

    // --- select_learnings: Role filtering ---

    #[test]
    fn test_select_by_role_match() {
        let l = make_learning_with_role("wi-1", LearningScope::WorkItem, "impl insight", vec![Role::Implementer]);
        let map = to_map(vec![l]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_select_by_role_mismatch() {
        let l = make_learning_with_role("wi-1", LearningScope::WorkItem, "reviewer only", vec![Role::Reviewer]);
        let map = to_map(vec![l]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_select_none_roles_applies_to_all() {
        let l = make_learning("wi-1", LearningScope::WorkItem, "universal");
        let map = to_map(vec![l]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        for role in [
            Role::Implementer,
            Role::Reviewer,
            Role::Coordinator,
            Role::Researcher,
            Role::Integrator,
        ] {
            let result = select_learnings(&map, &scope_ids, role, 0.0, 20);
            assert_eq!(result.len(), 1, "should apply to role {role}");
        }
    }

    // --- select_learnings: Confidence filtering ---

    #[test]
    fn test_select_above_confidence_threshold() {
        let l = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "high conf", 0.8);
        let map = to_map(vec![l]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.3, 20);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_select_below_confidence_threshold() {
        let l = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "low conf", 0.1);
        let map = to_map(vec![l]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.3, 20);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_select_promoted_always_included_regardless_of_confidence() {
        let mut l = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "policy", 0.1);
        l.promoted = true;
        let map = to_map(vec![l]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.9, 20);
        assert_eq!(result.len(), 1);
        assert!(result[0].promoted);
    }

    // --- select_learnings: Sorting ---

    #[test]
    fn test_sort_promoted_first() {
        let mut l1 = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "normal", 0.9);
        l1.updated_at = 1000;
        let mut l2 = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "policy", 0.5);
        l2.promoted = true;
        l2.updated_at = 500;
        let map = to_map(vec![l1, l2]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
        assert_eq!(result.len(), 2);
        assert!(result[0].promoted, "promoted should come first");
        assert!(!result[1].promoted);
    }

    #[test]
    fn test_sort_by_confidence_desc() {
        let mut l1 = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "low", 0.3);
        l1.updated_at = 1000;
        let mut l2 = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "high", 0.9);
        l2.updated_at = 1000;
        let map = to_map(vec![l1, l2]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
        assert_eq!(result.len(), 2);
        assert!(result[0].confidence > result[1].confidence);
    }

    #[test]
    fn test_sort_by_recency_desc() {
        let mut l1 = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "older", 0.5);
        l1.updated_at = 1000;
        let mut l2 = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "newer", 0.5);
        l2.updated_at = 2000;
        let map = to_map(vec![l1, l2]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
        assert_eq!(result.len(), 2);
        assert!(result[0].updated_at > result[1].updated_at);
    }

    // --- select_learnings: Truncation ---

    #[test]
    fn test_max_count_truncation() {
        let learnings: Vec<Learning> = (0..30)
            .map(|i| make_learning("wi-1", LearningScope::WorkItem, &format!("insight {i}")))
            .collect();
        let map = to_map(learnings);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 10);
        assert_eq!(result.len(), 10);
    }

    #[test]
    fn test_fewer_than_max_count() {
        let l = make_learning("wi-1", LearningScope::WorkItem, "only one");
        let map = to_map(vec![l]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
        assert_eq!(result.len(), 1);
    }

    // --- select_learnings: Empty inputs ---

    #[test]
    fn test_empty_learnings() {
        let map = HashMap::new();
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
        assert!(result.is_empty());
    }

    #[test]
    fn test_empty_scope_ids_only_global() {
        let l1 = make_learning("wi-1", LearningScope::WorkItem, "scoped");
        let l2 = make_learning("global", LearningScope::Global, "global");
        let map = to_map(vec![l1, l2]);

        let result = select_learnings(&map, &[], Role::Implementer, 0.0, 20);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].scope, LearningScope::Global);
    }

    // --- select_learnings: Combined filtering ---

    #[test]
    fn test_combined_scope_role_confidence() {
        let l1 = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "good", 0.8);
        let mut l2 = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "wrong role", 0.8);
        l2.applicable_roles = Some(vec![Role::Reviewer]);
        let l3 = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "low conf", 0.1);
        let l4 = make_learning_with_confidence("wi-999", LearningScope::WorkItem, "wrong scope", 0.8);

        let map = to_map(vec![l1, l2, l3, l4]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.3, 20);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "good");
    }

    #[test]
    fn test_auto_promoted_learning_sorted_first() {
        let policy = PromotionPolicy {
            min_reinforcements: 2,
            max_age_days: 30,
            auto_promote: true,
        };
        let mut l1 = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "unpromoted", 0.9);
        l1.updated_at = 2000;
        let mut l2 = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "promoted", 0.7);
        l2.reinforce(&policy);
        l2.reinforce(&policy);
        l2.updated_at = 1000;
        assert!(l2.promoted, "should be auto-promoted after 2 reinforcements");

        let map = to_map(vec![l1, l2]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
        assert_eq!(result.len(), 2);
        assert!(result[0].promoted, "promoted should be first");
        assert_eq!(result[0].content, "promoted");
    }

    // --- select_learnings: Default confidence ---

    #[test]
    fn test_default_confidence_passes_standard_threshold() {
        let l = make_learning("wi-1", LearningScope::WorkItem, "new insight");
        assert!((l.confidence - 0.5).abs() < f32::EPSILON);
        let map = to_map(vec![l]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.3, 20);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_zero_max_count_returns_empty() {
        let l = make_learning("wi-1", LearningScope::WorkItem, "insight");
        let map = to_map(vec![l]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 0);
        assert!(result.is_empty());
    }

    // =====================================================
    // Token estimation tests
    // =====================================================

    #[test]
    fn test_estimate_tokens_empty() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn test_estimate_tokens_short() {
        // "hello" = 5 chars → (5+3)/4 = 2 tokens
        assert_eq!(estimate_tokens("hello"), 2);
    }

    #[test]
    fn test_estimate_tokens_exact_boundary() {
        // 8 chars → (8+3)/4 = 2 tokens
        assert_eq!(estimate_tokens("abcdefgh"), 2);
    }

    #[test]
    fn test_estimate_tokens_longer() {
        let text = "a".repeat(400);
        // 400 chars → (400+3)/4 = 100 tokens
        assert_eq!(estimate_tokens(&text), 100);
    }

    // =====================================================
    // Truncation tests
    // =====================================================

    #[test]
    fn test_truncate_prose_no_truncation() {
        let text = "Short text.";
        assert_eq!(truncate_prose(text, 100), "Short text.");
    }

    #[test]
    fn test_truncate_prose_at_sentence() {
        // 2 tokens = 8 chars max. "First. Second." = 15 chars, won't fit.
        let text = "First. Second sentence here.";
        let result = truncate_prose(text, 5);
        // 5 tokens = 20 chars. Text is 28 chars. Slice = first 20 = "First. Second senten"
        // rfind(". ") in "First. Second senten" → position 5 ("First. ")
        assert!(result.contains("First."));
        assert!(result.contains("[truncated]"));
    }

    #[test]
    fn test_truncate_prose_at_newline() {
        let text = "Line one\nLine two is much longer and will exceed the budget";
        let result = truncate_prose(text, 5);
        // 5 tokens = 20 chars. Slice = "Line one\nLine two is"
        // No ". " found, rfind('\n') at position 8
        assert!(result.contains("Line one\n"));
        assert!(result.contains("[truncated]"));
    }

    #[test]
    fn test_truncate_list_empty() {
        let items: Vec<String> = vec![];
        let result = truncate_list(&items, 100);
        assert!(result.is_empty());
    }

    #[test]
    fn test_truncate_list_all_fit() {
        let items = vec!["short".to_string(), "items".to_string()];
        let result = truncate_list(&items, 100);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_truncate_list_exceeds_budget() {
        let items: Vec<String> = (0..20)
            .map(|i| format!("This is learning item number {i} with some extra text"))
            .collect();
        // Each item is ~50 chars = ~13 tokens + 1 = 14 tokens per item
        // Budget of 30 tokens should fit about 2 items
        let result = truncate_list(&items, 30);
        assert!(result.len() < items.len());
        assert!(!result.is_empty());
    }

    // =====================================================
    // TokenBudget tests
    // =====================================================

    #[test]
    fn test_token_budget_for_implementer() {
        let budget = TokenBudget::for_role(Role::Implementer);
        assert_eq!(budget.system_prompt, 500);
        assert_eq!(budget.hierarchy, 2000);
        assert_eq!(budget.learnings, 2000);
        assert_eq!(budget.tools_or_actions, 500);
        assert_eq!(budget.previous_summary, 4000);
    }

    #[test]
    fn test_token_budget_for_reviewer() {
        let budget = TokenBudget::for_role(Role::Reviewer);
        assert_eq!(budget.system_prompt, 500);
        assert_eq!(budget.hierarchy, 2000);
        assert_eq!(budget.learnings, 2000);
        assert_eq!(budget.tools_or_actions, 0);
        assert_eq!(budget.previous_summary, 0);
    }

    #[test]
    fn test_token_budget_for_coordinator() {
        let budget = TokenBudget::for_role(Role::Coordinator);
        assert!(budget.state_summary > 0);
        assert!(budget.learnings > 0);
    }

    #[test]
    fn test_token_budget_for_researcher() {
        let budget = TokenBudget::for_role(Role::Researcher);
        assert!(budget.learnings > 0);
    }

    #[test]
    fn test_token_budget_for_integrator() {
        let budget = TokenBudget::for_role(Role::Integrator);
        assert!(budget.state_summary > 0);
        assert!(budget.system_prompt > 0);
    }

    // =====================================================
    // ContextBuilder tests
    // =====================================================

    fn setup_stores(dir: &std::path::Path) -> (Stores, String) {
        let config = Config {
            project: ProjectConfig {
                repo_path: dir.to_path_buf(),
                ..ProjectConfig::default()
            },
            ..Config::default()
        };
        let store = Store::open(dir).unwrap();
        let mut stores = Stores::new();
        stores.store = Some(Arc::new(StdMutex::new(store)));
        stores.config = config;

        let plan = Plan::new("Test Plan".into(), "A test plan".into(), "criteria".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan.id.clone(), plan);

        let spec = Spec::new(plan_id, "Test Spec".into(), "A test spec".into());
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec.id.clone(), spec);

        let phase = Phase::new(spec_id, "Test Phase".into(), "A test phase".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase.id.clone(), phase);

        let wi = WorkItem::new(phase_id.clone(), "Test WorkItem".into(), "Implement the feature".into());
        let wi_id = wi.id.clone();
        stores.work_items.write().unwrap().insert(wi.id.clone(), wi);

        let learning = Learning::new(
            phase_id,
            LearningScope::Phase,
            "Previous iteration found a bug in parsing".into(),
        );
        stores.learnings.write().unwrap().insert(learning.id.clone(), learning);

        (stores, wi_id)
    }

    fn setup_stores_with_bundle(dir: &std::path::Path) -> (Stores, String, String) {
        let (stores, wi_id) = setup_stores(dir);

        let mut bundle = Bundle::new(
            wi_id.clone(),
            Some("tick-001".into()),
            "feature/test".into(),
            "Added test module with basic functionality".into(),
        );
        bundle.status = BundleStatus::Triaged;
        bundle.touched_paths = vec!["src/test.rs".into(), "src/main.rs".into()];
        let bundle_id = bundle.id.clone();
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        (stores, wi_id, bundle_id)
    }

    #[test]
    fn test_context_builder_load_work_item_hierarchy() {
        let dir = std::env::temp_dir().join(format!("loopr-ctx-hier-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (stores, wi_id) = setup_stores(&dir);

        let builder = ContextBuilder::new(&stores, Role::Implementer)
            .load_work_item_hierarchy(&wi_id)
            .unwrap();

        assert_eq!(builder.work_item_title(), Some("Test WorkItem"));
        assert!(builder.plan.is_some());
        assert!(builder.spec.is_some());
        assert!(builder.phase.is_some());
        assert!(builder.work_item.is_some());
        assert_eq!(builder.scope_ids.len(), 4);
    }

    #[test]
    fn test_context_builder_load_missing_work_item() {
        let dir = std::env::temp_dir().join(format!("loopr-ctx-miss-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (stores, _) = setup_stores(&dir);

        let result = ContextBuilder::new(&stores, Role::Implementer).load_work_item_hierarchy("nonexistent");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("work item not found"));
    }

    #[test]
    fn test_context_builder_load_bundle_hierarchy() {
        let dir = std::env::temp_dir().join(format!("loopr-ctx-bundle-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (stores, _, bundle_id) = setup_stores_with_bundle(&dir);

        let builder = ContextBuilder::new(&stores, Role::Reviewer)
            .load_bundle_hierarchy(&bundle_id)
            .unwrap();

        assert_eq!(builder.work_item_title(), Some("Test WorkItem"));
        assert!(builder.bundle_info.is_some());
        let (bid, _, paths) = builder.bundle_info.as_ref().unwrap();
        assert_eq!(bid, &bundle_id);
        assert_eq!(paths.len(), 2);
    }

    #[test]
    fn test_context_builder_load_missing_bundle() {
        let dir = std::env::temp_dir().join(format!("loopr-ctx-missbdl-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (stores, _) = setup_stores(&dir);

        let result = ContextBuilder::new(&stores, Role::Reviewer).load_bundle_hierarchy("nonexistent");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("bundle not found"));
    }

    #[test]
    fn test_context_builder_build_implementer() {
        let dir = std::env::temp_dir().join(format!("loopr-ctx-build-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (stores, wi_id) = setup_stores(&dir);
        let tool_runner = ToolRunner::new(&[ToolEntry {
            name: "test".into(),
            command: "echo ok".into(),
            timeout_secs: 10,
            worktree: true,
        }]);

        let builder = ContextBuilder::new(&stores, Role::Implementer)
            .load_work_item_hierarchy(&wi_id)
            .unwrap()
            .with_tools(&tool_runner)
            .with_iteration(1)
            .with_footer("Implement the WorkItem described above.".to_string());

        let assembled = builder.build("You are an Implementer.");
        assert!(assembled.user_message.contains("Test Plan"));
        assert!(assembled.user_message.contains("Test Spec"));
        assert!(assembled.user_message.contains("Test Phase"));
        assert!(assembled.user_message.contains("Test WorkItem"));
        assert!(assembled.user_message.contains("`test`"));
        assert!(assembled.user_message.contains("Current Iteration: 1"));
        assert!(assembled.user_message.contains("Implement the WorkItem"));
        assert!(assembled.user_message.contains("Learnings"));
        assert_eq!(assembled.system_prompt, "You are an Implementer.");
        assert!(assembled.token_estimate > 0);
    }

    #[test]
    fn test_context_builder_build_reviewer() {
        let dir = std::env::temp_dir().join(format!("loopr-ctx-rev-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (stores, _, bundle_id) = setup_stores_with_bundle(&dir);

        let builder = ContextBuilder::new(&stores, Role::Reviewer)
            .load_bundle_hierarchy(&bundle_id)
            .unwrap()
            .with_footer("Review this Bundle.".to_string());

        let assembled = builder.build("You are a Reviewer.");
        assert!(assembled.user_message.contains("Test Plan"));
        assert!(assembled.user_message.contains("Test WorkItem"));
        assert!(assembled.user_message.contains("Bundle Under Review"));
        assert!(assembled.user_message.contains(&bundle_id));
        assert!(assembled.user_message.contains("`src/test.rs`"));
        assert!(assembled.user_message.contains("Review this Bundle."));
    }

    #[test]
    fn test_context_builder_with_previous_summary() {
        let dir = std::env::temp_dir().join(format!("loopr-ctx-prev-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (stores, wi_id) = setup_stores(&dir);

        let builder = ContextBuilder::new(&stores, Role::Implementer)
            .load_work_item_hierarchy(&wi_id)
            .unwrap()
            .with_previous_summary(Some("Last iteration added error types".into()))
            .with_iteration(3);

        let assembled = builder.build("system");
        assert!(assembled.user_message.contains("Previous Iteration Summary"));
        assert!(assembled.user_message.contains("Last iteration added error types"));
        assert!(assembled.user_message.contains("Current Iteration: 3"));
    }

    #[test]
    fn test_context_builder_with_staleness() {
        let dir = std::env::temp_dir().join(format!("loopr-ctx-stale-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (stores, wi_id) = setup_stores(&dir);

        let builder = ContextBuilder::new(&stores, Role::Implementer)
            .load_work_item_hierarchy(&wi_id)
            .unwrap()
            .with_staleness_note(Some("A new Tick 'tick-99' has been published.".into()))
            .with_iteration(2);

        let assembled = builder.build("system");
        assert!(assembled.user_message.contains("Staleness Warning"));
        assert!(assembled.user_message.contains("tick-99"));
    }

    #[test]
    fn test_context_builder_no_learnings_section_when_empty() {
        let dir = std::env::temp_dir().join(format!("loopr-ctx-nolearn-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (stores, wi_id) = setup_stores(&dir);

        // Clear all learnings
        stores.learnings.write().unwrap().clear();

        let builder = ContextBuilder::new(&stores, Role::Implementer)
            .load_work_item_hierarchy(&wi_id)
            .unwrap();

        let assembled = builder.build("system");
        assert!(!assembled.user_message.contains("Learnings"));
    }

    #[test]
    fn test_context_builder_no_tools_section_when_empty() {
        let dir = std::env::temp_dir().join(format!("loopr-ctx-notool-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (stores, wi_id) = setup_stores(&dir);

        let builder = ContextBuilder::new(&stores, Role::Implementer)
            .load_work_item_hierarchy(&wi_id)
            .unwrap();
        // No .with_tools() call

        let assembled = builder.build("system");
        assert!(!assembled.user_message.contains("Available Tools"));
    }

    #[test]
    fn test_context_builder_reviewer_no_previous_summary() {
        let dir = std::env::temp_dir().join(format!("loopr-ctx-revnp-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (stores, _, bundle_id) = setup_stores_with_bundle(&dir);

        // Reviewer budget has previous_summary = 0, so even if set it shouldn't appear
        let builder = ContextBuilder::new(&stores, Role::Reviewer)
            .load_bundle_hierarchy(&bundle_id)
            .unwrap()
            .with_previous_summary(Some("This should not appear".into()));

        let assembled = builder.build("system");
        assert!(!assembled.user_message.contains("Previous Iteration Summary"));
    }

    #[test]
    fn test_assembled_context_token_estimate() {
        let dir = std::env::temp_dir().join(format!("loopr-ctx-tokens-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (stores, wi_id) = setup_stores(&dir);

        let assembled = ContextBuilder::new(&stores, Role::Implementer)
            .load_work_item_hierarchy(&wi_id)
            .unwrap()
            .build("system prompt");

        assert!(assembled.token_estimate > 0);
        assert!(!assembled.user_message.is_empty());
    }
}
