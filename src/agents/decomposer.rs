use eyre::{Result, eyre};
use tracing::{debug, info, warn};

use crate::agents::AgentContext;
use crate::agents::bridge::AgentIpcBridge;

/// Single-level decomposer agent.
///
/// Reads one Active parent (Plan, Spec, or Phase), loads the decomposer role
/// config, and calls the `decomposer.decompose` IPC handler to produce Pending
/// children. Does not loop or orchestrate multiple levels - the multi-level
/// flow emerges from the FSM and reconciliation strategies.
pub struct DecomposerAgent<'a> {
    pub ctx: AgentContext,
    bridge: &'a AgentIpcBridge,
    target_id: String,
}

impl<'a> DecomposerAgent<'a> {
    pub fn new(ctx: AgentContext, bridge: &'a AgentIpcBridge, target_id: String) -> Self {
        Self { ctx, bridge, target_id }
    }

    pub async fn run(&mut self) -> Result<()> {
        debug!(
            "decomposer: target_id={} session={}",
            self.target_id, self.ctx.session.id
        );
        self.ctx.session.iteration = 1;

        // Step 1: Determine the parent's collection by probing stores.
        let (parent_collection, parent_record) = self.resolve_parent()?;
        info!(
            "decomposer: resolved target {} as {} record",
            self.target_id, parent_collection
        );

        // Step 2: Walk up to the plan to read decomposer-config.
        let decomposer_config = self.resolve_decomposer_config(&parent_collection, &parent_record);
        debug!("decomposer: config={}", decomposer_config);

        // Step 3: Load role config and find the rule for this parent kind.
        let rule = self.load_rule(&parent_collection, &decomposer_config)?;

        // Step 4: Call decomposer.decompose via the IPC bridge.
        let params = serde_json::json!({
            "parent_id": self.target_id,
            "parent_collection": parent_collection,
            "target_kind": rule.target_kind,
            "count_guidance": rule.count_guidance,
            "dependency_pattern": rule.dependency_pattern,
        });

        let resp = self.bridge.request("decomposer.decompose", params);
        if let Some(err) = &resp.error {
            self.emit_failed(&format!("decompose call failed: {}", err.message));
            return Err(eyre!("decompose failed: {}", err.message));
        }

        let children = resp
            .result
            .as_ref()
            .and_then(|r| r["children"].as_array())
            .map(|a| a.len())
            .unwrap_or(0);

        // Step 5: Handle zero-children case.
        if children == 0 {
            info!(
                "decomposer: zero children returned for {} {}; transitioning to Complete",
                parent_collection, self.target_id
            );
            let transition_resp = self.bridge.request(
                &format!("{}.transition", parent_collection),
                serde_json::json!({
                    "id": self.target_id,
                    "target_status": "Complete",
                    "role": "decomposer",
                    "reason": "no-children-generated",
                }),
            );
            if let Some(err) = &transition_resp.error {
                warn!(
                    "decomposer: failed to transition {} {} to Complete: {}",
                    parent_collection, self.target_id, err.message
                );
            }
            self.emit_completed(0);
            return Ok(());
        }

        // Step 6: Emit success.
        info!(
            "decomposer: created {} children for {} {}",
            children, parent_collection, self.target_id
        );
        self.emit_completed(children);
        Ok(())
    }

    /// Probe stores to determine what collection the target_id belongs to.
    fn resolve_parent(&self) -> Result<(String, serde_json::Value)> {
        for collection in &["plan", "spec", "phase"] {
            let resp = self.bridge.request(
                &format!("{}.get", collection),
                serde_json::json!({ "id": self.target_id }),
            );
            if let (None, Some(result)) = (&resp.error, resp.result) {
                return Ok((collection.to_string(), result));
            }
        }
        Err(eyre!(
            "target_id '{}' not found in plan/spec/phase stores",
            self.target_id
        ))
    }

    /// Walk up from the current record to the plan and read its `tier` field.
    /// The Plan struct has a native `tier` field (Tier::Full / Tier::Brief) set
    /// by the classify-tier primitive. Kebab-case in YAML/JSON -> snake_case in
    /// Rust struct -> serialized as lowercase string ("full" or "brief").
    fn resolve_decomposer_config(&self, collection: &str, record: &serde_json::Value) -> String {
        // If the record IS a plan, read tier directly.
        if collection == "plan" {
            return record
                .get("tier")
                .and_then(|v| v.as_str())
                .unwrap_or("full")
                .to_lowercase();
        }

        // Walk up via parent_id chain.
        let parent_id = record.get("parent_id").and_then(|v| v.as_str()).unwrap_or("");
        let parent_collection = match collection {
            "phase" => "spec",
            "spec" => "plan",
            _ => return "full".to_string(),
        };

        let resp = self.bridge.request(
            &format!("{}.get", parent_collection),
            serde_json::json!({ "id": parent_id }),
        );
        if let Some(result) = resp.result {
            return self.resolve_decomposer_config(parent_collection, &result);
        }

        "full".to_string()
    }

    /// Load the role config YAML and find the rule for this parent kind.
    fn load_rule(&self, parent_collection: &str, config_name: &str) -> Result<DecomposerRule> {
        let config_path = format!(
            "strategies/roles/decomposer{}.yml",
            if config_name == "full" { String::new() } else { format!("-{}", config_name) }
        );

        let repo_path = &self.ctx.stores.config.project.repo_path;
        let full_path = std::path::Path::new(repo_path).join(&config_path);

        let content = std::fs::read_to_string(&full_path)
            .map_err(|e| eyre!("failed to read role config '{}': {}", full_path.display(), e))?;

        let parsed: serde_json::Value =
            serde_yaml::from_str(&content).map_err(|e| eyre!("failed to parse role config: {}", e))?;

        let rules = parsed
            .get("decomposer")
            .and_then(|d| d.get("rules"))
            .ok_or_else(|| eyre!("role config missing decomposer.rules"))?;

        let rule_value = rules.get(parent_collection).ok_or_else(|| {
            eyre!(
                "no decomposition rule for parent kind '{}' in config '{}'",
                parent_collection,
                config_name
            )
        })?;

        Ok(DecomposerRule {
            target_kind: rule_value["target-kind"].as_str().unwrap_or("work").to_string(),
            count_guidance: rule_value["count-guidance"].as_str().unwrap_or("1-5").to_string(),
            dependency_pattern: rule_value["dependency-pattern"]
                .as_str()
                .unwrap_or("fan-out")
                .to_string(),
        })
    }

    fn emit_completed(&self, child_count: usize) {
        let _ = self.bridge.request(
            "event.emit",
            serde_json::json!({
                "event": "decomposition.completed",
                "data": {
                    "parent_id": self.target_id,
                    "child_count": child_count,
                    "session_id": self.ctx.session.id,
                }
            }),
        );
    }

    fn emit_failed(&self, reason: &str) {
        // Update CoordinatorState.decomposition_error and increment attempts counter
        // before emitting the event, so the coordinator has context when it wakes.
        let _ = self.bridge.request(
            "decomposer.handle_failure",
            serde_json::json!({
                "parent_id": self.target_id,
                "reason": reason,
            }),
        );
        let _ = self.bridge.request(
            "event.emit",
            serde_json::json!({
                "event": "decomposition.failed",
                "data": {
                    "parent_id": self.target_id,
                    "reason": reason,
                    "session_id": self.ctx.session.id,
                }
            }),
        );
    }
}

/// A decomposition rule extracted from the role config YAML.
struct DecomposerRule {
    target_kind: String,
    count_guidance: String,
    dependency_pattern: String,
}
