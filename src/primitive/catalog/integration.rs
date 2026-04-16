use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use serde_json::json;
use tracing::{debug, info, warn};

use crate::primitive::types::{
    Idempotency, InputField, OutputField, OutputType, Primitive, PrimitiveContext, PrimitiveOutput,
};

/// Executes the full integration cycle: collect accepted bundles, merge branches
/// into integration branch, create a Tick, validate, and publish.
///
/// Replaces the deleted IntegratorAgent::run_cycle() with direct execution.
pub struct IntegrateTick;

impl Primitive for IntegrateTick {
    fn name(&self) -> &'static str {
        "integrate-tick"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let plan_id = params["plan-id"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'plan-id'"))?;

            debug!("integrate-tick: plan_id={}", plan_id);

            // Step 1: Collect accepted bundles for this plan.
            // Walk: Work -> Phase (parent_id) -> Spec (parent_id) -> Plan (parent_id)
            let accepted_bundles: Vec<(String, String, String)> = {
                let bundles = ctx
                    .stores
                    .bundles
                    .read()
                    .map_err(|_| eyre::eyre!("bundles lock poisoned"))?;
                let works = ctx
                    .stores
                    .works
                    .read()
                    .map_err(|_| eyre::eyre!("works lock poisoned"))?;
                let phases = ctx
                    .stores
                    .phases
                    .read()
                    .map_err(|_| eyre::eyre!("phases lock poisoned"))?;
                let specs = ctx
                    .stores
                    .specs
                    .read()
                    .map_err(|_| eyre::eyre!("specs lock poisoned"))?;

                let work_belongs_to_plan = |work_id: &str| -> bool {
                    let work = match works.get(work_id) {
                        Some(w) => w,
                        None => return false,
                    };
                    let phase = match phases.get(&work.parent_id) {
                        Some(p) => p,
                        None => return false,
                    };
                    let spec = match specs.get(&phase.parent_id) {
                        Some(s) => s,
                        None => return false,
                    };
                    spec.parent_id == plan_id
                };

                bundles
                    .values()
                    .filter(|b| {
                        b.status() == crate::domain::bundle::BundleStatus::Accepted && work_belongs_to_plan(&b.work_id)
                    })
                    .map(|b| (b.id.clone(), b.work_id.clone(), b.branch_name.clone()))
                    .collect()
            };

            if accepted_bundles.is_empty() {
                debug!("integrate-tick: no accepted bundles for plan {}", plan_id);
                return Ok(PrimitiveOutput {
                    values: HashMap::new(),
                    summary: format!("no accepted bundles for plan '{}'", plan_id),
                });
            }

            info!(
                "integrate-tick: {} accepted bundles for plan {}",
                accepted_bundles.len(),
                plan_id
            );

            // Step 2: Determine integration branch
            let integration_branch = format!("integration/{}", plan_id);
            let repo_path = &ctx.stores.config.project.repo_path;

            // Step 3: Merge each bundle branch into the integration branch
            let mut merged_bundle_ids = Vec::new();
            let mut merge_failures = Vec::new();

            for (bundle_id, work_id, branch_name) in &accepted_bundles {
                debug!(
                    "integrate-tick: merging bundle {} (branch {}) into {}",
                    bundle_id, branch_name, integration_branch
                );

                // Checkout integration branch
                let checkout = std::process::Command::new("git")
                    .args(["checkout", &integration_branch])
                    .current_dir(repo_path)
                    .output();

                if checkout.as_ref().map(|o| !o.status.success()).unwrap_or(true) {
                    warn!(
                        "integrate-tick: failed to checkout {}, skipping bundle {}",
                        integration_branch, bundle_id
                    );
                    merge_failures.push(bundle_id.clone());
                    continue;
                }

                // Merge the bundle branch
                let merge = std::process::Command::new("git")
                    .args([
                        "merge",
                        "--no-ff",
                        "-m",
                        &format!("Merge bundle {}", bundle_id),
                        branch_name,
                    ])
                    .current_dir(repo_path)
                    .output();

                match merge {
                    Ok(output) if output.status.success() => {
                        info!("integrate-tick: merged bundle {} successfully", bundle_id);
                        merged_bundle_ids.push(bundle_id.clone());

                        // Transition bundle to Merged
                        let resp = ctx.bridge.request(
                            "bundle.transition",
                            json!({
                                "id": bundle_id,
                                "target_status": "Merged",
                                "role": "integrator",
                            }),
                        );
                        if resp.is_error() {
                            warn!("integrate-tick: failed to transition bundle {} to Merged", bundle_id);
                        }

                        // Transition work from InReview to Integrated
                        let resp = ctx.bridge.request(
                            "work.transition",
                            json!({
                                "id": work_id,
                                "target_status": "Integrated",
                                "role": "integrator",
                            }),
                        );
                        if resp.is_error() {
                            warn!("integrate-tick: failed to transition work {} to Integrated", work_id);
                        }

                        // Emit bundle.merged event for implementer rebase
                        let _ = ctx.event_tx.send(crate::ipc::protocol::DaemonEvent::new(
                            "bundle.merged",
                            json!({
                                "bundle_id": bundle_id,
                                "work_id": work_id,
                                "plan_id": plan_id,
                                "branch": branch_name,
                            }),
                        ));
                    }
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        warn!("integrate-tick: merge conflict for bundle {}: {}", bundle_id, stderr);
                        merge_failures.push(bundle_id.clone());

                        // Abort the failed merge
                        let _ = std::process::Command::new("git")
                            .args(["merge", "--abort"])
                            .current_dir(repo_path)
                            .output();

                        // Emit conflict-detected event for the conflict resolution strategy
                        let _ = ctx.event_tx.send(crate::ipc::protocol::DaemonEvent::new(
                            "integration.conflict-detected",
                            json!({
                                "bundle_id": bundle_id,
                                "work_id": work_id,
                                "plan_id": plan_id,
                            }),
                        ));
                    }
                    Err(e) => {
                        warn!(
                            "integrate-tick: git merge failed to execute for bundle {}: {}",
                            bundle_id, e
                        );
                        merge_failures.push(bundle_id.clone());
                    }
                }
            }

            if merged_bundle_ids.is_empty() {
                return Ok(PrimitiveOutput {
                    values: HashMap::new(),
                    summary: format!(
                        "integrate-tick: all {} merges failed for plan '{}'",
                        accepted_bundles.len(),
                        plan_id
                    ),
                });
            }

            // Step 4: Create a Tick record for the merged bundles
            let tick_resp = ctx.bridge.request(
                "tick.create",
                json!({
                    "plan_id": plan_id,
                    "bundle_ids": merged_bundle_ids,
                }),
            );

            let tick_id = tick_resp
                .result
                .as_ref()
                .and_then(|r| r["id"].as_str())
                .unwrap_or("")
                .to_string();

            if tick_id.is_empty() {
                eyre::bail!("integrate-tick: failed to create tick record");
            }

            // Step 5: Validate and publish via existing handler endpoints
            let publish_resp = ctx.bridge.request("integrator.publish", json!({ "tick_id": tick_id }));

            let outcome = if publish_resp.is_error() {
                warn!("integrate-tick: publish failed for tick {}", tick_id);
                "failed"
            } else {
                "published"
            };

            let head_sha = crate::daemon::handlers::integrator::get_git_head_sha(repo_path);

            // Step 6: Handle merge failures - reject bundles and reset works
            for failed_bundle_id in &merge_failures {
                let resp = ctx.bridge.request(
                    "bundle.transition",
                    json!({
                        "id": failed_bundle_id,
                        "target_status": "Rejected",
                        "role": "integrator",
                    }),
                );
                if resp.is_error() {
                    warn!("integrate-tick: failed to reject bundle {}", failed_bundle_id);
                }
            }

            let mut values = HashMap::new();
            values.insert("tick-id".to_string(), json!(tick_id));
            values.insert("outcome".to_string(), json!(outcome));
            values.insert("merged-count".to_string(), json!(merged_bundle_ids.len()));
            values.insert("failed-count".to_string(), json!(merge_failures.len()));
            if let Some(sha) = &head_sha {
                values.insert("head-sha".to_string(), json!(sha));
            }

            Ok(PrimitiveOutput {
                values,
                summary: format!(
                    "integrate-tick '{}': {} ({} merged, {} failed)",
                    tick_id,
                    outcome,
                    merged_bundle_ids.len(),
                    merge_failures.len()
                ),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![
            OutputField {
                name: "tick-id".to_string(),
                field_type: OutputType::String,
            },
            OutputField {
                name: "outcome".to_string(),
                field_type: OutputType::String,
            },
            OutputField {
                name: "head-sha".to_string(),
                field_type: OutputType::String,
            },
        ]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "plan-id".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "bundle-ids".to_string(),
                field_type: OutputType::StringArray,
                required: false, // Optional: discovers accepted bundles from plan-id at runtime
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::GuardRequired
    }

    fn requires_git_lock(&self) -> bool {
        true
    }
}

/// Merges one or more bundle branches into the integration branch.
pub struct MergeBranches;

impl Primitive for MergeBranches {
    fn name(&self) -> &'static str {
        "merge-branches"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let branch_names: Vec<String> = params["branch-names"]
                .as_array()
                .ok_or_else(|| eyre::eyre!("missing 'branch-names'"))?
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            let target = params["target-branch"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'target-branch'"))?;

            debug!("merge-branches: {} branches into {}", branch_names.len(), target);

            // Checkout target branch
            let checkout = std::process::Command::new("git")
                .args(["checkout", target])
                .current_dir(ctx.repo_path)
                .output()?;
            if !checkout.status.success() {
                eyre::bail!("merge-branches: checkout '{}' failed", target);
            }

            let mut conflicts = Vec::new();
            for branch in &branch_names {
                let merge = std::process::Command::new("git")
                    .args(["merge", "--no-ff", branch])
                    .current_dir(ctx.repo_path)
                    .output()?;
                if !merge.status.success() {
                    let _ = std::process::Command::new("git")
                        .args(["merge", "--abort"])
                        .current_dir(ctx.repo_path)
                        .output();
                    conflicts.push(serde_json::json!({"branch": branch}));
                }
            }

            let head_output = std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(ctx.repo_path)
                .output()?;
            let head_sha = String::from_utf8_lossy(&head_output.stdout).trim().to_string();

            let mut values = HashMap::new();
            values.insert("head-sha".to_string(), serde_json::json!(head_sha));
            values.insert("conflicts".to_string(), serde_json::Value::Array(conflicts.clone()));

            Ok(PrimitiveOutput {
                values,
                summary: format!("merged {} branches, {} conflicts", branch_names.len(), conflicts.len()),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![
            OutputField {
                name: "head-sha".to_string(),
                field_type: OutputType::String,
            },
            OutputField {
                name: "conflicts".to_string(),
                field_type: OutputType::Json,
            },
        ]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "branch-names".to_string(),
                field_type: OutputType::StringArray,
                required: true,
            },
            InputField {
                name: "target-branch".to_string(),
                field_type: OutputType::String,
                required: true,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::NonIdempotent
    }

    fn requires_git_lock(&self) -> bool {
        true
    }
}

/// Executes validation commands against the current state.
pub struct RunValidation;

impl Primitive for RunValidation {
    fn name(&self) -> &'static str {
        "run-validation"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let commands: Vec<String> = params["commands"]
                .as_array()
                .ok_or_else(|| eyre::eyre!("missing 'commands'"))?
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();

            debug!("run-validation: {} commands", commands.len());

            let mut all_passed = true;
            let mut log = String::new();

            for cmd in &commands {
                let output = std::process::Command::new("sh")
                    .args(["-c", cmd])
                    .current_dir(ctx.repo_path)
                    .output()?;
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                log.push_str(&format!("$ {}\n{}{}\n", cmd, stdout, stderr));
                if !output.status.success() {
                    all_passed = false;
                }
            }

            let mut values = HashMap::new();
            values.insert("passed".to_string(), serde_json::json!(all_passed));
            values.insert("log".to_string(), serde_json::json!(log));

            Ok(PrimitiveOutput {
                values,
                summary: format!("validation: {} commands, passed={}", commands.len(), all_passed),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![
            OutputField {
                name: "passed".to_string(),
                field_type: OutputType::Bool,
            },
            OutputField {
                name: "log".to_string(),
                field_type: OutputType::String,
            },
        ]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "commands".to_string(),
                field_type: OutputType::StringArray,
                required: true,
            },
            InputField {
                name: "timeout-secs".to_string(),
                field_type: OutputType::U32,
                required: false,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }

    fn requires_git_lock(&self) -> bool {
        true
    }
}

/// Creates a per-plan integration branch from main.
pub struct CreateIntegrationBranch;

impl Primitive for CreateIntegrationBranch {
    fn name(&self) -> &'static str {
        "create-integration-branch"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let plan_id = params["plan-id"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'plan-id'"))?;

            let branch_name = format!("integration/{}", plan_id);
            debug!("create-integration-branch: {}", branch_name);

            let output = std::process::Command::new("git")
                .args(["branch", &branch_name, "main"])
                .current_dir(ctx.repo_path)
                .output()?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                eyre::bail!("create-integration-branch failed: {}", stderr);
            }

            let mut values = HashMap::new();
            values.insert("branch-name".to_string(), serde_json::json!(branch_name));

            Ok(PrimitiveOutput {
                values,
                summary: format!("created branch '{}'", branch_name),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![OutputField {
            name: "branch-name".to_string(),
            field_type: OutputType::String,
        }]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![InputField {
            name: "plan-id".to_string(),
            field_type: OutputType::String,
            required: true,
        }]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::GuardRequired
    }

    fn requires_git_lock(&self) -> bool {
        true
    }
}

/// Merges the integration branch to main and cleans up.
pub struct MergeIntegrationToMain;

impl Primitive for MergeIntegrationToMain {
    fn name(&self) -> &'static str {
        "merge-integration-to-main"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let plan_id = params["plan-id"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'plan-id'"))?;
            let message = params["message"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'message'"))?;

            let branch_name = format!("integration/{}", plan_id);
            debug!("merge-integration-to-main: {} -> main", branch_name);

            // Checkout main
            let checkout = std::process::Command::new("git")
                .args(["checkout", "main"])
                .current_dir(ctx.repo_path)
                .output()?;
            if !checkout.status.success() {
                eyre::bail!("merge-integration-to-main: checkout main failed");
            }

            // Merge --no-ff
            let merge = std::process::Command::new("git")
                .args(["merge", "--no-ff", "-m", message, &branch_name])
                .current_dir(ctx.repo_path)
                .output()?;
            if !merge.status.success() {
                eyre::bail!("merge-integration-to-main: merge failed");
            }

            // Delete integration branch
            let _ = std::process::Command::new("git")
                .args(["branch", "-D", &branch_name])
                .current_dir(ctx.repo_path)
                .output();

            Ok(PrimitiveOutput {
                values: HashMap::new(),
                summary: format!("merged '{}' to main", branch_name),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![
            InputField {
                name: "plan-id".to_string(),
                field_type: OutputType::String,
                required: true,
            },
            InputField {
                name: "message".to_string(),
                field_type: OutputType::String,
                required: true,
            },
        ]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::NonIdempotent
    }

    fn requires_git_lock(&self) -> bool {
        true
    }
}

/// Deletes an integration branch without merging (plan failure/abandonment).
pub struct DeleteIntegrationBranch;

impl Primitive for DeleteIntegrationBranch {
    fn name(&self) -> &'static str {
        "delete-integration-branch"
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            let plan_id = params["plan-id"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("missing 'plan-id'"))?;

            let branch_name = format!("integration/{}", plan_id);
            debug!("delete-integration-branch: {}", branch_name);

            // Checkout main first
            let _ = std::process::Command::new("git")
                .args(["checkout", "main"])
                .current_dir(ctx.repo_path)
                .output();

            let output = std::process::Command::new("git")
                .args(["branch", "-D", &branch_name])
                .current_dir(ctx.repo_path)
                .output()?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stderr.contains("not found") {
                    eyre::bail!("delete-integration-branch failed: {}", stderr);
                }
            }

            Ok(PrimitiveOutput {
                values: HashMap::new(),
                summary: format!("deleted branch '{}'", branch_name),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![InputField {
            name: "plan-id".to_string(),
            field_type: OutputType::String,
            required: true,
        }]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }

    fn requires_git_lock(&self) -> bool {
        true
    }
}
