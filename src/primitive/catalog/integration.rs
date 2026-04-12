use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use tracing::debug;

use crate::primitive::types::{
    Idempotency, InputField, OutputField, OutputType, Primitive, PrimitiveContext, PrimitiveOutput,
};

/// Executes the full integration cycle atomically.
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
            debug!("integrate-tick: plan-id={}", params["plan-id"]);

            let resp = ctx.bridge.request("integrator.tick", params);
            if let Some(err) = &resp.error {
                eyre::bail!("integrate-tick failed: {}", err.message);
            }

            let tick_id = resp
                .result
                .as_ref()
                .and_then(|r| r["tick_id"].as_str())
                .unwrap_or("")
                .to_string();
            let outcome = resp
                .result
                .as_ref()
                .and_then(|r| r["outcome"].as_str())
                .unwrap_or("unknown")
                .to_string();
            let head_sha = resp
                .result
                .as_ref()
                .and_then(|r| r["head_sha"].as_str())
                .map(|s| s.to_string());

            let mut values = HashMap::new();
            values.insert("tick-id".to_string(), serde_json::json!(tick_id));
            values.insert("outcome".to_string(), serde_json::json!(outcome));
            if let Some(sha) = &head_sha {
                values.insert("head-sha".to_string(), serde_json::json!(sha));
            }

            Ok(PrimitiveOutput {
                values,
                summary: format!("integrate-tick '{}': {}", tick_id, outcome),
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
                required: true,
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
